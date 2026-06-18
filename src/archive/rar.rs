use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Result};

use super::http_server::{spawn_http_stream, StreamSource};
use super::ArchiveEntry;

pub(crate) fn read_rar_entry(archive: &Path, name: &str) -> Result<Vec<u8>> {
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

pub(crate) fn list_rar(archive: &Path, prefix: &str) -> Result<Vec<ArchiveEntry>> {
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

pub(crate) fn stream_rar_video(
    archive: PathBuf,
    name: String,
) -> (String, Arc<std::sync::atomic::AtomicBool>) {
    // RAR has no random-access API in the unrar crate, so we always cache
    // the entry fully into RAM before serving.
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
