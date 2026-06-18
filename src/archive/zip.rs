use std::collections::HashSet;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use zip::ZipArchive;

use super::http_server::{spawn_http_stream, StreamSource};
use super::multi::MultiFileReader;
use super::{split_set, ArchiveEntry};

pub(crate) fn read_zip_entry(archive: &Path, name: &str) -> Result<Vec<u8>> {
    if let Some(parts) = split_set(archive) {
        let reader = MultiFileReader::open(&parts)?;
        let mut za = ZipArchive::new(reader)?;
        read_zip_impl(&mut za, name)
    } else {
        let file = std::fs::File::open(archive)?;
        let mut za = ZipArchive::new(file)?;
        read_zip_impl(&mut za, name)
    }
}

fn read_zip_impl<R: Read + Seek>(za: &mut ZipArchive<R>, name: &str) -> Result<Vec<u8>> {
    let mut zfile = za.by_name(name)?;
    let mut buf = Vec::new();
    zfile.read_to_end(&mut buf)?;
    Ok(buf)
}

pub(crate) fn list_zip(archive: &Path, prefix: &str) -> Result<Vec<ArchiveEntry>> {
    if let Some(parts) = split_set(archive) {
        let reader = MultiFileReader::open(&parts)?;
        let mut za = ZipArchive::new(reader)?;
        list_zip_impl(&mut za, prefix)
    } else {
        let file = std::fs::File::open(archive)?;
        let mut za = ZipArchive::new(file)?;
        list_zip_impl(&mut za, prefix)
    }
}

fn list_zip_impl<R: Read + Seek>(za: &mut ZipArchive<R>, prefix: &str) -> Result<Vec<ArchiveEntry>> {
    let mut out = Vec::new();
    let mut seen_dirs = HashSet::new();
    for i in 0..za.len() {
        let zfile = za.by_index(i)?;
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
    if let Some(parts) = split_set(&archive) {
        return stream_split_zip(parts, name);
    }
    stream_single_zip(archive, name)
}

fn stream_single_zip(archive: PathBuf, name: String) -> (String, Arc<std::sync::atomic::AtomicBool>) {
    let mut total_size = 0u64;
    let mut physical_data_start: Option<u64> = None;
    let mut in_memory_cache: Option<Arc<Vec<u8>>> = None;

    if let Ok(file) = std::fs::File::open(&archive) {
        if let Ok(mut zip) = ZipArchive::new(file) {
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
        StreamSource::Memory(Arc::new(Vec::new()))
    };

    spawn_http_stream(source)
}

fn stream_split_zip(
    parts: Vec<PathBuf>,
    name: String,
) -> (String, Arc<std::sync::atomic::AtomicBool>) {
    let mut stored_range: Option<(u64, u64)> = None; // (data_start, size) in logical stream
    let mut in_memory_cache: Option<Arc<Vec<u8>>> = None;

    if let Ok(mut reader) = MultiFileReader::open(&parts) {
        let logical_total = reader.total_size();
        if let Ok(mut za) = ZipArchive::new(&mut reader) {
            if let Ok(mut zfile) = za.by_name(&name) {
                let size = zfile.size();
                let is_stored = zfile.compression() == zip::CompressionMethod::Stored;

                if is_stored {
                    if let Some(ds) = zfile.data_start() {
                        // Cap to the logical stream length so MultiFileReader
                        // doesn't try to map past EOF (padded ZIP trailers).
                        let capped = size.min(logical_total.saturating_sub(ds));
                        stored_range = Some((ds, capped));
                    }
                }

                if stored_range.is_none() {
                    let mut buf = Vec::with_capacity(size as usize);
                    let _ = std::io::copy(&mut zfile, &mut buf);
                    in_memory_cache = Some(Arc::new(buf));
                }
            }
        }
    }

    let source = if let Some((ds, size)) = stored_range {
        // Build per-file segments spanning the split parts. Re-open the reader
        // to compute bounds without consuming the one used for ZipArchive above.
        let segments = MultiFileReader::open(&parts)
            .ok()
            .map(|r| r.map_logical_range(ds, size))
            .unwrap_or_default();
        if segments.is_empty() {
            StreamSource::Memory(Arc::new(Vec::new()))
        } else {
            let total: u64 = segments.iter().map(|s| s.len_in_file).sum();
            StreamSource::MultiFileRange { segments, total }
        }
    } else if let Some(buf) = in_memory_cache {
        StreamSource::Memory(buf)
    } else {
        StreamSource::Memory(Arc::new(Vec::new()))
    };

    spawn_http_stream(source)
}
