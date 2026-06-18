use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use zip::ZipArchive;

use super::http_server::{spawn_http_stream, StreamSource};
use super::multi::MultiFileReader;
use super::{split_set, ArchiveEntry};

/// Either a single archive file or a stitched-together split set, exposed as a
/// single `Read + Seek` source for `ZipArchive`. Avoids trait objects (which
/// can't unify two non-auto traits).
enum ZipSrc {
    Single(std::fs::File),
    Multi(MultiFileReader),
}

impl Read for ZipSrc {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            ZipSrc::Single(f) => f.read(buf),
            ZipSrc::Multi(m) => m.read(buf),
        }
    }
}

impl Seek for ZipSrc {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        match self {
            ZipSrc::Single(f) => f.seek(pos),
            ZipSrc::Multi(m) => m.seek(pos),
        }
    }
}

fn open_zip_src(archive: &Path) -> Result<ZipSrc> {
    if let Some(parts) = split_set(archive) {
        Ok(ZipSrc::Multi(MultiFileReader::open(&parts)?))
    } else {
        Ok(ZipSrc::Single(std::fs::File::open(archive)?))
    }
}

pub(crate) fn read_zip_entry(archive: &Path, name: &str, password: Option<&str>) -> Result<Vec<u8>> {
    let mut za = ZipArchive::new(open_zip_src(archive)?)?;
    read_zip_impl(&mut za, name, password)
}

fn read_zip_impl<R: Read + Seek>(
    za: &mut ZipArchive<R>,
    name: &str,
    password: Option<&str>,
) -> Result<Vec<u8>> {
    let mut zfile = match password {
        Some(p) => za.by_name_decrypt(name, p.as_bytes())?,
        None => za.by_name(name)?,
    };
    let mut buf = Vec::new();
    zfile.read_to_end(&mut buf)?;
    Ok(buf)
}

pub(crate) fn list_zip(
    archive: &Path,
    prefix: &str,
    _password: Option<&str>,
) -> Result<Vec<ArchiveEntry>> {
    // ZIP never encrypts filenames, so listing never needs a password.
    let mut za = ZipArchive::new(open_zip_src(archive)?)?;
    list_zip_impl(&mut za, prefix)
}

fn list_zip_impl<R: Read + Seek>(za: &mut ZipArchive<R>, prefix: &str) -> Result<Vec<ArchiveEntry>> {
    // Read names straight from the central directory (which is never encrypted
    // in the ZIP format) via `file_names()`, so listing works even for
    // content-encrypted archives without a password.
    let mut out = Vec::new();
    let mut seen_dirs = HashSet::new();
    for raw in za.file_names() {
        let name = raw.to_string();
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

/// ZIP never encrypts filenames, so listing is always possible without a
/// password.
pub(crate) fn zip_needs_password_to_list(_archive: &Path) -> Result<bool> {
    Ok(false)
}

/// True iff any entry in the archive has encrypted content. Detected by
/// probing `by_index`, which raises `PASSWORD_REQUIRED` for encrypted entries
/// when no password is supplied.
pub(crate) fn zip_content_encrypted(archive: &Path) -> Result<bool> {
    use zip::result::ZipError;
    let mut za = ZipArchive::new(open_zip_src(archive)?)?;
    for i in 0..za.len() {
        let encrypted = match za.by_index(i) {
            Ok(zf) => zf.encrypted(),
            Err(ZipError::UnsupportedArchive(ZipError::PASSWORD_REQUIRED)) => true,
            Err(_) => false,
        };
        if encrypted {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Validate `password` by decrypting a small chunk of the first encrypted
/// entry. Returns `Ok(())` if the password is accepted, `Err` otherwise.
pub(crate) fn zip_validate_password(archive: &Path, password: &str) -> Result<()> {
    use zip::result::ZipError;
    let mut za = ZipArchive::new(open_zip_src(archive)?)?;
    for i in 0..za.len() {
        let encrypted = match za.by_index(i) {
            Ok(zf) => zf.encrypted(),
            Err(ZipError::UnsupportedArchive(ZipError::PASSWORD_REQUIRED)) => true,
            Err(_) => false,
        };
        if encrypted {
            let mut zf = za.by_index_decrypt(i, password.as_bytes())?;
            // Reading at least one byte exercises the ZipCrypto check byte /
            // WinZip AES password verifier, so a wrong password surfaces here.
            let mut buf = [0u8; 1];
            match zf.read(&mut buf) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
                Err(e) => return Err(e.into()),
            }
            return Ok(());
        }
    }
    // No encrypted entry found; nothing to validate.
    Ok(())
}

pub(crate) fn stream_zip_video(
    archive: PathBuf,
    name: String,
    password: Option<String>,
) -> (String, Arc<std::sync::atomic::AtomicBool>) {
    if let Some(parts) = split_set(&archive) {
        return stream_split_zip(parts, name, password);
    }
    stream_single_zip(archive, name, password)
}

fn stream_single_zip(
    archive: PathBuf,
    name: String,
    password: Option<String>,
) -> (String, Arc<std::sync::atomic::AtomicBool>) {
    let mut total_size = 0u64;
    let mut physical_data_start: Option<u64> = None;
    let mut in_memory_cache: Option<Arc<Vec<u8>>> = None;

    if let Ok(file) = std::fs::File::open(&archive) {
        if let Ok(mut zip) = ZipArchive::new(file) {
            let zfile_result = match &password {
                Some(p) => zip.by_name_decrypt(&name, p.as_bytes()),
                None => zip.by_name(&name),
            };
            if let Ok(mut zfile) = zfile_result {
                total_size = zfile.size();
                let is_stored = zfile.compression() == zip::CompressionMethod::Stored;
                let is_encrypted = zfile.encrypted();

                // Raw file-range serving only works for unencrypted stored
                // entries; encrypted bytes are ciphertext and must be
                // decrypted into memory regardless of compression.
                if is_stored && !is_encrypted {
                    if let Some(ds) = zfile.data_start() {
                        physical_data_start = Some(ds);
                        // HARD CAP to prevent EOF crash if ZIP header size is padded
                        if let Ok(meta) = std::fs::metadata(&archive) {
                            let max_available = meta.len().saturating_sub(ds);
                            total_size = total_size.min(max_available);
                        }
                    }
                }

                // Cache compressed (or encrypted) files to RAM.
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
    password: Option<String>,
) -> (String, Arc<std::sync::atomic::AtomicBool>) {
    let mut stored_range: Option<(u64, u64)> = None; // (data_start, size) in logical stream
    let mut in_memory_cache: Option<Arc<Vec<u8>>> = None;

    if let Ok(mut reader) = MultiFileReader::open(&parts) {
        let logical_total = reader.total_size();
        if let Ok(mut za) = ZipArchive::new(&mut reader) {
            let zfile_result = match &password {
                Some(p) => za.by_name_decrypt(&name, p.as_bytes()),
                None => za.by_name(&name),
            };
            if let Ok(mut zfile) = zfile_result {
                let size = zfile.size();
                let is_stored = zfile.compression() == zip::CompressionMethod::Stored;
                let is_encrypted = zfile.encrypted();

                if is_stored && !is_encrypted {
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
