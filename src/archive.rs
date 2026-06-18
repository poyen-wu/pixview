use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};

#[derive(Clone, Debug, PartialEq)]
pub enum EntryPath {
    Native(PathBuf),
    InZip(PathBuf, String),
    InRar(PathBuf, String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArchiveType {
    Zip,
    Rar,
}

/// Detect archive type by file extension. Returns `None` for non-archives.
pub fn archive_type(name: &str) -> Option<ArchiveType> {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "zip" => Some(ArchiveType::Zip),
        "rar" => Some(ArchiveType::Rar),
        _ => None,
    }
}

/// A single entry discovered while listing an archive directory.
pub struct ArchiveEntry {
    /// Full path of the entry inside the archive (e.g. "subdir/image.jpg").
    pub internal_path: String,
    /// Leaf name to display in the browser.
    pub display_name: String,
    /// Whether this entry is a directory (or a virtual dir derived from a prefix).
    pub is_dir: bool,
}

/// Read the full bytes of a single archive entry. Used for loading images.
pub fn read_entry(path: &EntryPath) -> Result<Vec<u8>> {
    match path {
        EntryPath::Native(_) => bail!("read_entry is only valid for archive paths"),
        EntryPath::InZip(arc, name) => read_zip_entry(arc, name),
        EntryPath::InRar(arc, name) => read_rar_entry(arc, name),
    }
}

/// List entries under the prefix of an `InZip` / `InRar` cwd.
pub fn list_archive(cwd: &EntryPath) -> Result<Vec<ArchiveEntry>> {
    match cwd {
        EntryPath::InZip(arc, prefix) => list_zip(arc, prefix),
        EntryPath::InRar(arc, prefix) => list_rar(arc, prefix),
        EntryPath::Native(_) => bail!("list_archive is only valid for archive paths"),
    }
}

/// Spawn a local HTTP server that streams an archive entry to ffmpeg.
/// Returns `None` for native filesystem paths (caller handles those directly).
pub fn stream_video(path: &EntryPath) -> Option<(String, Arc<AtomicBool>)> {
    match path {
        EntryPath::Native(_) => None,
        EntryPath::InZip(arc, name) => Some(stream_zip_video(arc.clone(), name.clone())),
        EntryPath::InRar(arc, name) => Some(stream_rar_video(arc.clone(), name.clone())),
    }
}

// ----------------------------------------------------------------------------
// ZIP backend
// ----------------------------------------------------------------------------

fn read_zip_entry(archive: &Path, name: &str) -> Result<Vec<u8>> {
    let file = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file)?;
    let mut zfile = zip.by_name(name)?;
    let mut buf = Vec::new();
    zfile.read_to_end(&mut buf)?;
    Ok(buf)
}

fn list_zip(archive: &Path, prefix: &str) -> Result<Vec<ArchiveEntry>> {
    let file = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file)?;

    let mut out = Vec::new();
    let mut seen_dirs = HashSet::new();
    for i in 0..zip.len() {
        let zfile = zip.by_index(i)?;
        let name = zfile.name().to_string();
        if !name.starts_with(prefix) || name == prefix {
            continue;
        }
        let remainder = &name[prefix.len()..];
        if let Some(slash_idx) = remainder.find('/') {
            let dir_name = &remainder[..slash_idx];
            if seen_dirs.insert(dir_name.to_string()) {
                out.push(ArchiveEntry {
                    internal_path: format!("{}{}/", prefix, dir_name),
                    display_name: dir_name.to_string(),
                    is_dir: true,
                });
            }
        } else {
            let display_name = remainder.to_string();
            out.push(ArchiveEntry {
                internal_path: name,
                display_name,
                is_dir: false,
            });
        }
    }
    Ok(out)
}

fn stream_zip_video(archive: PathBuf, name: String) -> (String, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop_signal = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop_signal);
    listener.set_nonblocking(true).unwrap();

    let mut total_size = 0;
    let mut physical_data_start: Option<u64> = None;
    let mut in_memory_cache: Option<Arc<Vec<u8>>> = None;

    if let Ok(file) = std::fs::File::open(&archive) {
        if let Ok(mut zip) = zip::ZipArchive::new(file) {
            if let Ok(mut zfile) = zip.by_name(&name) {
                total_size = zfile.size();
                let is_stored = zfile.compression() == zip::CompressionMethod::Stored;

                if is_stored {
                    if let Some(ds) = zfile.data_start() {
                        physical_data_start = Some(ds);
                        // HARD CAP to prevent EOF crash if ZIP header size is padded
                        if let Ok(meta) = std::fs::metadata(&archive) {
                            let max_available = meta.len().saturating_sub(ds);
                            total_size = total_size.min(max_available);
                        }
                    }
                }

                // Cache compressed files to RAM
                if physical_data_start.is_none() {
                    let mut buf = Vec::with_capacity(total_size as usize);
                    let _ = std::io::copy(&mut zfile, &mut buf);
                    total_size = buf.len() as u64;
                    in_memory_cache = Some(Arc::new(buf));
                }
            }
        }
    }

    std::thread::spawn(move || {
        while !stop_clone.load(Ordering::Relaxed) {
            if let Ok((mut stream, _)) = listener.accept() {
                // CRITICAL FIX: The accepted stream inherits the listener's non-blocking flag on Unix.
                // We MUST make it blocking, otherwise it drops instantly and causes FFmpeg to
                // enter an infinite, 100% CPU reconnect loop.
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

                let archive = archive.clone();
                let cache = in_memory_cache.clone();

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

                    let mut start_byte = 0;
                    let mut end_byte_opt = None;
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
                        if let Some(mem_buf) = &cache {
                            let start_idx = start_byte as usize;
                            let end_idx = (start_byte + content_length) as usize;
                            if start_idx <= mem_buf.len() {
                                let safe_end = end_idx.min(mem_buf.len());
                                let _ = stream.write_all(&mem_buf[start_idx..safe_end]);
                            }
                        } else if let Some(ds) = physical_data_start {
                            if let Ok(mut raw_file) = std::fs::File::open(&archive) {
                                if raw_file.seek(SeekFrom::Start(ds + start_byte)).is_ok() {
                                    let mut chunk = raw_file.take(content_length);
                                    let _ = std::io::copy(&mut chunk, &mut stream);
                                }
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

// ----------------------------------------------------------------------------
// RAR backend
// ----------------------------------------------------------------------------

fn read_rar_entry(archive: &Path, name: &str) -> Result<Vec<u8>> {
    let mut open = unrar::Archive::new(archive).open_for_processing()?;
    loop {
        match open.read_header()? {
            Some(open_at_file) => {
                let entry_name = open_at_file
                    .entry()
                    .filename
                    .to_string_lossy()
                    .replace('\\', "/");
                if entry_name == name {
                    let (data, _) = open_at_file.read()?;
                    return Ok(data);
                }
                open = open_at_file.skip()?;
            }
            None => break,
        }
    }
    bail!("entry not found in rar archive: {}", name);
}

fn list_rar(archive: &Path, prefix: &str) -> Result<Vec<ArchiveEntry>> {
    let open = unrar::Archive::new(archive).open_for_listing()?;
    let mut out = Vec::new();
    let mut seen_dirs = HashSet::new();

    for entry in open {
        let entry = entry?;
        let name = entry.filename.to_string_lossy().replace('\\', "/");
        if !name.starts_with(prefix) || name == prefix {
            continue;
        }
        let remainder = &name[prefix.len()..];
        if let Some(slash_idx) = remainder.find('/') {
            let dir_name = &remainder[..slash_idx];
            if seen_dirs.insert(dir_name.to_string()) {
                out.push(ArchiveEntry {
                    internal_path: format!("{}{}/", prefix, dir_name),
                    display_name: dir_name.to_string(),
                    is_dir: true,
                });
            }
        } else {
            let display_name = remainder.to_string();
            out.push(ArchiveEntry {
                internal_path: name,
                display_name,
                is_dir: false,
            });
        }
    }
    Ok(out)
}

fn stream_rar_video(archive: PathBuf, name: String) -> (String, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop_signal = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop_signal);
    listener.set_nonblocking(true).unwrap();

    // RAR has no random-access API in the unrar crate, so we always cache
    // the entry fully into RAM before serving. This matches the compressed-zip
    // path; only difference is we cannot use a physical_data_start shortcut.
    let mut total_size: u64 = 0;
    let mut in_memory_cache: Option<Arc<Vec<u8>>> = None;

    if let Ok(mut open) = unrar::Archive::new(&archive).open_for_processing() {
        loop {
            match open.read_header() {
                Ok(Some(open_at_file)) => {
                    let entry_name = open_at_file
                        .entry()
                        .filename
                        .to_string_lossy()
                        .replace('\\', "/");
                    if entry_name == name {
                        if let Ok((data, _rest)) = open_at_file.read() {
                            total_size = data.len() as u64;
                            in_memory_cache = Some(Arc::new(data));
                        }
                        break;
                    }
                    match open_at_file.skip() {
                        Ok(next) => open = next,
                        Err(_) => break,
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
    }

    std::thread::spawn(move || {
        while !stop_clone.load(Ordering::Relaxed) {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

                let cache = in_memory_cache.clone();

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
                        if let Some(mem_buf) = &cache {
                            let start_idx = start_byte as usize;
                            let end_idx = (start_byte + content_length) as usize;
                            if start_idx <= mem_buf.len() {
                                let safe_end = end_idx.min(mem_buf.len());
                                let _ = stream.write_all(&mem_buf[start_idx..safe_end]);
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
