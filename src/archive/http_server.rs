use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::multi::FileSegment;

/// Source of bytes for a streaming response. Each backend produces one of these
/// up front; `spawn_http_stream` then handles the HTTP serving for all cases.
pub(crate) enum StreamSource {
    /// Entry fully buffered in RAM (compressed RAR/7z entries, or compressed ZIP entries).
    Memory(Arc<Vec<u8>>),
    /// Entry can be served by seeking into a single archive file on disk
    /// (stored ZIP entries), starting at byte `data_start` and running for `total` bytes.
    FileRange {
        path: std::path::PathBuf,
        data_start: u64,
        total: u64,
    },
    /// Entry spans multiple physical files (split ZIP / split 7z stored entries).
    /// `segments` is an ordered list of contiguous byte ranges, one per part file
    /// the entry touches; `total` is the sum of all `len_in_file` values.
    MultiFileRange {
        segments: Vec<FileSegment>,
        total: u64,
    },
}

impl StreamSource {
    fn total_size(&self) -> u64 {
        match self {
            StreamSource::Memory(buf) => buf.len() as u64,
            StreamSource::FileRange { total, .. } => *total,
            StreamSource::MultiFileRange { total, .. } => *total,
        }
    }
}

/// Spawn a local HTTP server that streams the given source to ffmpeg.
/// Returns the URL ffmpeg should open and a stop signal that tears down the
/// accept loop when dropped/tripped by the caller.
pub(crate) fn spawn_http_stream(source: StreamSource) -> (String, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop_signal = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop_signal);
    listener.set_nonblocking(true).unwrap();

    let total_size = source.total_size();

    std::thread::spawn(move || {
        while !stop_clone.load(Ordering::Relaxed) {
            if let Ok((mut stream, _)) = listener.accept() {
                // CRITICAL FIX: The accepted stream inherits the listener's non-blocking flag
                // on Unix. We MUST make it blocking, otherwise it drops instantly and causes
                // FFmpeg to enter an infinite, 100% CPU reconnect loop.
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

                let source = source_share(&source);

                std::thread::spawn(move || {
                    let mut req_buf = Vec::new();
                    let mut buf = [0; 1024];
                    while let Ok(n) = stream.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        req_buf.extend_from_slice(&buf[..n]);
                        if req_buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }

                    let req_str = String::from_utf8_lossy(&req_buf);
                    if req_str.is_empty() || total_size == 0 {
                        return;
                    }

                    let mut start_byte: u64 = 0;
                    let mut end_byte_opt: Option<u64> = None;
                    let mut has_range = false;

                    for line in req_str.lines() {
                        let line_clean = line.to_lowercase().replace(" ", "");
                        if line_clean.starts_with("range:bytes=") {
                            has_range = true;
                            if let Some(bytes_str) = line_clean.split("bytes=").nth(1) {
                                let range_str = bytes_str.trim();
                                if range_str.starts_with('-') {
                                    if let Ok(suffix_len) = range_str[1..].parse::<u64>() {
                                        start_byte = total_size.saturating_sub(suffix_len);
                                        end_byte_opt = Some(total_size.saturating_sub(1));
                                    }
                                } else {
                                    let parts: Vec<&str> = range_str.split('-').collect();
                                    if let Ok(b) = parts[0].parse::<u64>() {
                                        start_byte = b;
                                    }
                                    if parts.len() > 1 && !parts[1].is_empty() {
                                        if let Ok(b) = parts[1].parse::<u64>() {
                                            end_byte_opt = Some(b);
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if start_byte >= total_size {
                        let headers = format!(
                            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nConnection: close\r\n\r\n",
                            total_size
                        );
                        let _ = stream.write_all(headers.as_bytes());
                        return;
                    }

                    let end_byte = end_byte_opt
                        .unwrap_or(total_size.saturating_sub(1))
                        .min(total_size.saturating_sub(1));
                    let content_length = end_byte.saturating_sub(start_byte) + 1;

                    let headers = if has_range {
                        format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Type: video/mp4\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                            start_byte, end_byte, total_size, content_length
                        )
                    } else {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                            total_size
                        )
                    };

                    if stream.write_all(headers.as_bytes()).is_ok() {
                        match &source {
                            StreamSource::Memory(mem_buf) => {
                                let start_idx = start_byte as usize;
                                let end_idx = (start_byte + content_length) as usize;
                                if start_idx <= mem_buf.len() {
                                    let safe_end = end_idx.min(mem_buf.len());
                                    let _ = stream.write_all(&mem_buf[start_idx..safe_end]);
                                }
                            }
                            StreamSource::FileRange { path, data_start, .. } => {
                                if let Ok(mut raw_file) = std::fs::File::open(path) {
                                    if raw_file.seek(SeekFrom::Start(data_start + start_byte)).is_ok() {
                                        let mut chunk = raw_file.take(content_length);
                                        let _ = std::io::copy(&mut chunk, &mut stream);
                                    }
                                }
                            }
                            StreamSource::MultiFileRange { segments, .. } => {
                                stream_multi_range(segments, &mut stream, start_byte, content_length);
                            }
                        }
                    }
                });
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    });

    (format!("http://127.0.0.1:{}/vid.mp4", port), stop_signal)
}

/// Cheaply clone the source so each accepted connection gets its own handle.
fn source_share(source: &StreamSource) -> StreamSource {
    match source {
        StreamSource::Memory(buf) => StreamSource::Memory(Arc::clone(buf)),
        StreamSource::FileRange { path, data_start, total } => StreamSource::FileRange {
            path: path.clone(),
            data_start: *data_start,
            total: *total,
        },
        StreamSource::MultiFileRange { segments, total } => StreamSource::MultiFileRange {
            segments: segments.clone(),
            total: *total,
        },
    }
}

/// Write `[start_byte, start_byte + content_length)` from the concatenated
/// segment stream into `stream`. Walks segments in order, opening each part
/// file, seeking to the in-file offset, and copying the relevant slice.
fn stream_multi_range<W: Write>(
    segments: &[FileSegment],
    stream: &mut W,
    start_byte: u64,
    content_length: u64,
) {
    let mut remaining = content_length;
    let mut logical_cur = 0u64;
    for seg in segments {
        if remaining == 0 {
            break;
        }
        let seg_end = logical_cur + seg.len_in_file;
        // Skip segments entirely before the requested window.
        if seg_end <= start_byte {
            logical_cur = seg_end;
            continue;
        }
        // Compute the overlap of [start_byte, start_byte+content_length) with
        // [logical_cur, seg_end) within this segment.
        let window_start = logical_cur.max(start_byte);
        let window_end = seg_end.min(start_byte + content_length);
        if window_end <= window_start {
            logical_cur = seg_end;
            continue;
        }
        let offset_in_file = seg.start_in_file + (window_start - logical_cur);
        let take = window_end - window_start;

        if let Ok(mut f) = std::fs::File::open(&seg.path) {
            if f.seek(SeekFrom::Start(offset_in_file)).is_ok() {
                let mut chunk = f.take(take);
                let _ = std::io::copy(&mut chunk, stream);
            }
        }
        remaining = remaining.saturating_sub(take);
        logical_cur = seg_end;
    }
}
