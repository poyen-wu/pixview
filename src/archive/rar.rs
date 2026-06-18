use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Result};

use super::http_server::{spawn_http_stream, StreamSource};
use super::ArchiveEntry;

/// Maximum entry size we're willing to fully decode just to validate a
/// candidate password. Entries larger than this are skipped when picking a
/// probe target.
const PROBE_SIZE_CAP: u64 = 32 * 1024 * 1024;

pub(crate) fn read_rar_entry(archive: &Path, name: &str, password: Option<&str>) -> Result<Vec<u8>> {
    let mut open = match password {
        Some(p) => unrar::Archive::with_password(archive, p).open_for_processing()?,
        None => unrar::Archive::new(archive).open_for_processing()?,
    };
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

pub(crate) fn list_rar(
    archive: &Path,
    prefix: &str,
    password: Option<&str>,
) -> Result<Vec<ArchiveEntry>> {
    let open = match password {
        Some(p) => unrar::Archive::with_password(archive, p).open_for_listing()?,
        None => unrar::Archive::new(archive).open_for_listing()?,
    };
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

/// True iff the RAR encrypts its file headers (so listing is impossible without
/// a password). Opening for listing is enough to read the header-encryption
/// flag; if even that fails, assume header encryption.
pub(crate) fn rar_needs_password_to_list(archive: &Path) -> Result<bool> {
    match unrar::Archive::new(archive).open_for_listing() {
        Ok(open) => Ok(open.has_encrypted_headers()),
        Err(_) => Ok(true),
    }
}

/// True iff any entry has encrypted content (with non-encrypted headers, i.e.
/// filenames are visible but file bytes need a password).
pub(crate) fn rar_content_encrypted(archive: &Path) -> Result<bool> {
    let open = unrar::Archive::new(archive).open_for_listing()?;
    for entry in open {
        if entry?.is_encrypted() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Validate `password` by reading the smallest encrypted file entry with it.
pub(crate) fn rar_validate_password(archive: &Path, password: &str) -> Result<()> {
    let mut best: Option<(String, u64)> = None;
    let open = unrar::Archive::new(archive).open_for_listing()?;
    for entry in open {
        let e = entry?;
        if e.is_directory() || !e.is_encrypted() {
            continue;
        }
        let size = e.unpacked_size;
        if size > PROBE_SIZE_CAP {
            continue;
        }
        match &best {
            Some((_, bs)) if *bs <= size => {}
            _ => best = Some((e.filename.to_string_lossy().replace('\\', "/"), size)),
        }
    }

    let Some((name, _)) = best else {
        // No small encrypted entry to probe; trust the password.
        return Ok(());
    };

    let mut open = unrar::Archive::with_password(archive, password).open_for_processing()?;
    loop {
        match open.read_header()? {
            Some(open_at_file) => {
                let en = open_at_file
                    .entry()
                    .filename
                    .to_string_lossy()
                    .replace('\\', "/");
                if en == name {
                    let (_data, _rest) = open_at_file.read()?;
                    return Ok(());
                }
                open = open_at_file.skip()?;
            }
            None => bail!("probe entry not found: {}", name),
        }
    }
}

pub(crate) fn stream_rar_video(
    archive: PathBuf,
    name: String,
    password: Option<String>,
) -> (String, Arc<std::sync::atomic::AtomicBool>) {
    // RAR has no random-access API in the unrar crate, so we always cache
    // the entry fully into RAM before serving.
    let mut in_memory_cache: Option<Arc<Vec<u8>>> = None;

    let opened = match password.as_deref() {
        Some(p) => unrar::Archive::with_password(&archive, p).open_for_processing(),
        None => unrar::Archive::new(&archive).open_for_processing(),
    };
    if let Ok(mut open) = opened {
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

    let source = match in_memory_cache {
        Some(buf) => StreamSource::Memory(buf),
        None => StreamSource::Memory(Arc::new(Vec::new())),
    };

    spawn_http_stream(source)
}
