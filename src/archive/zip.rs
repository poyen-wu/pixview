use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use super::http_server::{spawn_http_stream, StreamSource};
use super::ArchiveEntry;

pub(crate) fn read_zip_entry(archive: &Path, name: &str) -> Result<Vec<u8>> {
    let file = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file)?;
    let mut zfile = zip.by_name(name)?;
    let mut buf = Vec::new();
    zfile.read_to_end(&mut buf)?;
    Ok(buf)
}

pub(crate) fn list_zip(archive: &Path, prefix: &str) -> Result<Vec<ArchiveEntry>> {
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

pub(crate) fn stream_zip_video(archive: PathBuf, name: String) -> (String, Arc<std::sync::atomic::AtomicBool>) {
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

    let source = if let Some(ds) = physical_data_start {
        StreamSource::FileRange {
            path: archive,
            data_start: ds,
            total: total_size,
        }
    } else if let Some(buf) = in_memory_cache {
        StreamSource::Memory(buf)
    } else {
        // Nothing to serve; fall back to an empty memory buffer so the server
        // still binds and ffmpeg gets a clean empty response rather than a panic.
        StreamSource::Memory(Arc::new(Vec::new()))
    };

    spawn_http_stream(source)
}
