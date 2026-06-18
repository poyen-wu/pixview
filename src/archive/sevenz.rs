use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use super::http_server::{spawn_http_stream, StreamSource};
use super::multi::MultiFileReader;
use super::{split_set, ArchiveEntry};

pub(crate) fn read_7z_entry(archive: &Path, name: &str) -> Result<Vec<u8>> {
    if let Some(parts) = split_set(archive) {
        let mut reader = MultiFileReader::open(&parts)?;
        let mut ar = sevenz_rust2::ArchiveReader::new(&mut reader, sevenz_rust2::Password::empty())?;
        let data = ar.read_file(name)?;
        return Ok(data);
    }
    let mut reader = sevenz_rust2::ArchiveReader::open(archive, sevenz_rust2::Password::empty())?;
    let data = reader.read_file(name)?;
    Ok(data)
}

pub(crate) fn list_7z(archive: &Path, prefix: &str) -> Result<Vec<ArchiveEntry>> {
    let archive_info = if let Some(parts) = split_set(archive) {
        let mut reader = MultiFileReader::open(&parts)?;
        sevenz_rust2::Archive::read(&mut reader, &sevenz_rust2::Password::empty())?
    } else {
        sevenz_rust2::Archive::open(archive)?
    };

    let mut out = Vec::new();
    let mut seen_dirs = HashSet::new();

    for entry in &archive_info.files {
        if entry.is_directory() {
            continue;
        }
        let name = entry.name().replace('\\', "/");
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

pub(crate) fn stream_7z_video(
    archive: PathBuf,
    name: String,
) -> (String, Arc<std::sync::atomic::AtomicBool>) {
    // 7z commonly uses solid compression (multiple files sharing one compressed
    // stream), so there is no reliable random-access API. Like the RAR/compressed-zip
    // path, we cache the entry fully into RAM before serving. Split sets work
    // transparently because MultiFileReader exposes the concatenated stream.
    let mut in_memory_cache: Option<Arc<Vec<u8>>> = None;

    let read_ok = if let Some(parts) = split_set(&archive) {
        MultiFileReader::open(&parts)
            .and_then(|mut reader| {
                let mut ar = sevenz_rust2::ArchiveReader::new(
                    &mut reader,
                    sevenz_rust2::Password::empty(),
                )?;
                ar.read_file(&name).map(Some).map_err(Into::into)
            })
            .ok()
            .flatten()
    } else {
        sevenz_rust2::ArchiveReader::open(&archive, sevenz_rust2::Password::empty())
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
