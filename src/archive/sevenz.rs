use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use super::http_server::{spawn_http_stream, StreamSource};
use super::multi::MultiFileReader;
use super::{split_set, ArchiveEntry};

/// Maximum entry size we're willing to fully decode just to validate a
/// candidate password. Entries larger than this are skipped when picking a
/// probe target.
const PROBE_SIZE_CAP: u64 = 32 * 1024 * 1024;

fn make_password(p: Option<&str>) -> sevenz_rust2::Password {
    match p {
        Some(s) => sevenz_rust2::Password::new(s),
        None => sevenz_rust2::Password::empty(),
    }
}

/// Open a parsed `Archive` (headers only) using an empty password. Works for
/// non-encrypted and content-encrypted archives; fails with `PasswordRequired`
/// when filenames themselves are encrypted.
fn open_7z_info(archive: &Path) -> Result<sevenz_rust2::Archive> {
    if let Some(parts) = split_set(archive) {
        let mut reader = MultiFileReader::open(&parts)?;
        Ok(sevenz_rust2::Archive::read(
            &mut reader,
            &sevenz_rust2::Password::empty(),
        )?)
    } else {
        Ok(sevenz_rust2::Archive::open(archive)?)
    }
}

pub(crate) fn read_7z_entry(archive: &Path, name: &str, password: Option<&str>) -> Result<Vec<u8>> {
    let pwd = make_password(password);
    if let Some(parts) = split_set(archive) {
        let mut reader = MultiFileReader::open(&parts)?;
        let mut ar = sevenz_rust2::ArchiveReader::new(&mut reader, pwd)?;
        return Ok(ar.read_file(name)?);
    }
    let mut reader = sevenz_rust2::ArchiveReader::open(archive, pwd)?;
    Ok(reader.read_file(name)?)
}

pub(crate) fn list_7z(
    archive: &Path,
    prefix: &str,
    _password: Option<&str>,
) -> Result<Vec<ArchiveEntry>> {
    // Listing parses headers with an empty password; for content-encrypted
    // archives filenames remain readable, so no password is needed here.
    let archive_info = open_7z_info(archive)?;

    let mut out = Vec::new();
    let mut seen_dirs = HashSet::new();

    for entry in &archive_info.files {
        if entry.is_directory() {
            continue;
        }
        let name = entry.name.replace('\\', "/");
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

/// True iff the 7z encrypts its header (filenames), i.e. listing is impossible
/// without a password.
pub(crate) fn sevenz_needs_password_to_list(archive: &Path) -> Result<bool> {
    let err = if let Some(parts) = split_set(archive) {
        let mut reader = MultiFileReader::open(&parts)?;
        sevenz_rust2::Archive::read(&mut reader, &sevenz_rust2::Password::empty())
    } else {
        let mut reader = std::fs::File::open(archive)?;
        sevenz_rust2::Archive::read(&mut reader, &sevenz_rust2::Password::empty())
    };
    Ok(matches!(err, Err(sevenz_rust2::Error::PasswordRequired)))
}

/// True iff the archive's content is encrypted while filenames remain
/// readable. Detected by scanning the compression blocks for an AES coder.
pub(crate) fn sevenz_content_encrypted(archive: &Path) -> Result<bool> {
    let archive_info = open_7z_info(archive)?;
    for block in &archive_info.blocks {
        for coder in &block.coders {
            if coder.encoder_method_id() == sevenz_rust2::EncoderMethod::ID_AES256_SHA256 {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Validate `password` by reading the smallest file entry with it.
pub(crate) fn sevenz_validate_password(archive: &Path, password: &str) -> Result<()> {
    let archive_info = open_7z_info(archive)?;
    let mut best: Option<&str> = None;
    let mut best_size = u64::MAX;
    for e in &archive_info.files {
        if e.is_directory() || !e.has_stream {
            continue;
        }
        if e.size > PROBE_SIZE_CAP || e.size >= best_size {
            continue;
        }
        best_size = e.size;
        best = Some(e.name.as_str());
    }

    let Some(name) = best else {
        // No small entry to probe; trust the password.
        return Ok(());
    };

    let pwd = sevenz_rust2::Password::new(password);
    if let Some(parts) = split_set(archive) {
        let mut reader = MultiFileReader::open(&parts)?;
        let mut ar = sevenz_rust2::ArchiveReader::new(&mut reader, pwd)?;
        ar.read_file(name)?;
    } else {
        let mut ar = sevenz_rust2::ArchiveReader::open(archive, pwd)?;
        ar.read_file(name)?;
    }
    Ok(())
}

pub(crate) fn stream_7z_video(
    archive: PathBuf,
    name: String,
    password: Option<String>,
) -> (String, Arc<std::sync::atomic::AtomicBool>) {
    // 7z commonly uses solid compression (multiple files sharing one compressed
    // stream), so there is no reliable random-access API. Like the RAR/compressed-zip
    // path, we cache the entry fully into RAM before serving. Split sets work
    // transparently because MultiFileReader exposes the concatenated stream.
    let mut in_memory_cache: Option<Arc<Vec<u8>>> = None;

    let pwd = make_password(password.as_deref());
    let read_ok = if let Some(parts) = split_set(&archive) {
        MultiFileReader::open(&parts)
            .and_then(|mut reader| {
                let mut ar = sevenz_rust2::ArchiveReader::new(&mut reader, pwd)?;
                ar.read_file(&name).map(Some).map_err(Into::into)
            })
            .ok()
            .flatten()
    } else {
        sevenz_rust2::ArchiveReader::open(&archive, pwd)
            .ok()
            .and_then(|mut reader| reader.read_file(&name).ok())
    };

    if let Some(data) = read_ok {
        in_memory_cache = Some(Arc::new(data));
    }

    let source = match in_memory_cache {
        Some(buf) => StreamSource::Memory(buf),
        None => StreamSource::Memory(Arc::new(Vec::new())),
    };

    spawn_http_stream(source)
}
