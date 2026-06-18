mod http_server;
mod rar;
mod sevenz;
mod zip;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::{bail, Result};

#[derive(Clone, Debug, PartialEq)]
pub enum EntryPath {
    Native(PathBuf),
    InZip(PathBuf, String),
    InRar(PathBuf, String),
    InSevenZ(PathBuf, String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArchiveType {
    Zip,
    Rar,
    SevenZ,
}

/// Detect archive type by file extension. Returns `None` for non-archives.
pub fn archive_type(name: &str) -> Option<ArchiveType> {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "zip" => Some(ArchiveType::Zip),
        "rar" => Some(ArchiveType::Rar),
        "7z" => Some(ArchiveType::SevenZ),
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
        EntryPath::InZip(arc, name) => zip::read_zip_entry(arc, name),
        EntryPath::InRar(arc, name) => rar::read_rar_entry(arc, name),
        EntryPath::InSevenZ(arc, name) => sevenz::read_7z_entry(arc, name),
    }
}

/// List entries under the prefix of an `InZip` / `InRar` / `InSevenZ` cwd.
pub fn list_archive(cwd: &EntryPath) -> Result<Vec<ArchiveEntry>> {
    match cwd {
        EntryPath::InZip(arc, prefix) => zip::list_zip(arc, prefix),
        EntryPath::InRar(arc, prefix) => rar::list_rar(arc, prefix),
        EntryPath::InSevenZ(arc, prefix) => sevenz::list_7z(arc, prefix),
        EntryPath::Native(_) => bail!("list_archive is only valid for archive paths"),
    }
}

/// Spawn a local HTTP server that streams an archive entry to ffmpeg.
/// Returns `None` for native filesystem paths (caller handles those directly).
pub fn stream_video(path: &EntryPath) -> Option<(String, Arc<AtomicBool>)> {
    match path {
        EntryPath::Native(_) => None,
        EntryPath::InZip(arc, name) => Some(zip::stream_zip_video(arc.clone(), name.clone())),
        EntryPath::InRar(arc, name) => Some(rar::stream_rar_video(arc.clone(), name.clone())),
        EntryPath::InSevenZ(arc, name) => Some(sevenz::stream_7z_video(arc.clone(), name.clone())),
    }
}
