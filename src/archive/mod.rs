mod http_server;
mod multi;
mod rar;
mod sevenz;
mod zip;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{bail, Result};

/// Process-lifetime cache of passwords keyed by archive path. Populated on
/// demand when the user enters an encrypted archive; never persisted to disk.
static PASSWORDS: LazyLock<Mutex<HashMap<PathBuf, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Store the password for `path`, replacing any previous value.
pub fn set_password(path: &Path, password: &str) {
    PASSWORDS
        .lock()
        .expect("PASSWORDS mutex poisoned")
        .insert(path.to_path_buf(), password.to_string());
}

/// Returns the cached password for `path`, if any.
pub fn get_password(path: &Path) -> Option<String> {
    PASSWORDS
        .lock()
        .expect("PASSWORDS mutex poisoned")
        .get(path)
        .cloned()
}

/// Drop the cached password for `path` (e.g. when it proved wrong).
pub fn clear_password(path: &Path) {
    PASSWORDS
        .lock()
        .expect("PASSWORDS mutex poisoned")
        .remove(path);
}

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

/// Detect archive type by file extension. Returns `None` for non-archives
/// or for split-archive naming schemes (use [`archive_type_ext`] for those).
pub fn archive_type(name: &str) -> Option<ArchiveType> {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "zip" => Some(ArchiveType::Zip),
        "rar" => Some(ArchiveType::Rar),
        "7z" => Some(ArchiveType::SevenZ),
        _ => None,
    }
}

/// Result of classifying a filesystem entry's archive status, taking
/// split-archive naming into account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryClassification {
    /// Not an archive (regular file or directory).
    NonArchive,
    /// A standalone (non-split) archive file, or a lone `.001`-style part
    /// with no siblings (which is just a normal archive with a numeric suffix).
    Standalone(ArchiveType),
    /// A member of a multi-file split archive set.
    SplitMember {
        kind: ArchiveType,
        /// True iff this is the lowest-numbered part (the clickable primary).
        is_primary: bool,
        /// All parts in this set, sorted by part number.
        parts: Vec<PathBuf>,
    },
}

impl EntryClassification {
    pub fn archive_type(&self) -> Option<ArchiveType> {
        match self {
            Self::NonArchive => None,
            Self::Standalone(t) | Self::SplitMember { kind: t, .. } => Some(*t),
        }
    }

    pub fn is_archive(&self) -> bool {
        !matches!(self, Self::NonArchive)
    }
}

/// Detect archive type by file extension or split-archive naming, sniffing
/// magic bytes for ambiguous bare-numeric splits (`.001` / `.002` / ...).
/// Use [`archive_type`] for the cheap, I/O-free extension check.
pub fn archive_type_ext(path: &Path) -> Option<ArchiveType> {
    classify_entry(path).archive_type()
}

/// Classify a single filesystem entry. Less efficient than
/// [`classify_directory`] when iterating a whole folder (does its own
/// `read_dir` per call), but fine for one-off use.
pub fn classify_entry(path: &Path) -> EntryClassification {
    let Some(parent) = path.parent() else {
        return EntryClassification::NonArchive;
    };
    match classify_directory(parent) {
        Ok(list) => list
            .into_iter()
            .find(|(p, _)| p == path)
            .map(|(_, c)| c)
            .unwrap_or(EntryClassification::NonArchive),
        Err(_) => EntryClassification::NonArchive,
    }
}

/// Returns the ordered list of physical files for a split archive, or `None`
/// if `primary` is not part of a multi-file split set (e.g. a standalone
/// archive or a non-archive). Backends use this to decide whether to open via
/// a [`MultiFileReader`] or the single-file API.
pub fn split_set(primary: &Path) -> Option<Vec<PathBuf>> {
    match classify_entry(primary) {
        EntryClassification::SplitMember { parts, .. } => Some(parts),
        _ => None,
    }
}

/// Classify every entry directly under `dir` in a single `read_dir` pass.
/// More efficient than calling [`classify_entry`] per file when iterating a
/// whole folder (which would otherwise re-scan the directory each time).
pub fn classify_directory(dir: &Path) -> std::io::Result<Vec<(PathBuf, EntryClassification)>> {
    // Pass 1: read the directory.
    let mut entries: Vec<(PathBuf, String)> = Vec::new();
    for entry in fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        entries.push((path, name));
    }

    // Pass 2: parse split membership for each entry; collect sibling indices
    // keyed by (scheme, lowercased stem).
    let mut parsed: Vec<Option<ParsedSplit>> = Vec::with_capacity(entries.len());
    let mut groups: HashMap<(SplitScheme, String), Vec<usize>> = HashMap::new();
    for (i, (_, name)) in entries.iter().enumerate() {
        let p = parse_split_member(name);
        if let Some(ref pp) = p {
            let key = (pp.scheme, pp.stem.to_ascii_lowercase());
            groups.entry(key).or_default().push(i);
        }
        parsed.push(p);
    }

    // Pass 2b: legacy RAR volumes. The primary `stem.rar` does not match the
    // `stem.rNN` pattern, so we look for an unparsed `stem.rar` whose stem
    // matches an existing RarLegacy group and synthesise an entry at index 0.
    for (i, (_, name)) in entries.iter().enumerate() {
        if parsed[i].is_some() {
            continue;
        }
        let lower = name.to_ascii_lowercase();
        if let Some(candidate_stem) = lower.strip_suffix(".rar") {
            let key = (SplitScheme::RarLegacy, candidate_stem.to_string());
            if let Some(group) = groups.get_mut(&key) {
                group.push(i);
                parsed[i] = Some(ParsedSplit {
                    scheme: SplitScheme::RarLegacy,
                    kind: ArchiveType::Rar,
                    stem: candidate_stem.to_string(),
                    index: 0,
                    digits: 0,
                });
            }
        }
    }

    // Pass 3: build classifications.
    let mut out: Vec<(PathBuf, EntryClassification)> = Vec::with_capacity(entries.len());
    for (i, (path, name)) in entries.iter().enumerate() {
        let class = match parsed[i].clone() {
            Some(p) => {
                let key = (p.scheme, p.stem.to_ascii_lowercase());
                let group_indices: Vec<usize> = groups.get(&key).cloned().unwrap_or_default();

                // For BareNumbered, sniff the primary's magic bytes once to
                // settle ZIP vs 7z. If unrecognised, the whole set is non-archive.
                let kind = if p.scheme == SplitScheme::BareNumbered {
                    let primary_idx = group_indices
                        .iter()
                        .copied()
                        .min_by_key(|&gi| {
                            parsed[gi].as_ref().map(|q| q.index).unwrap_or(usize::MAX)
                        })
                        .unwrap_or(i);
                    match sniff_archive_type(&entries[primary_idx].0) {
                        Some(k) => k,
                        None => {
                            out.push((path.clone(), EntryClassification::NonArchive));
                            continue;
                        }
                    }
                } else {
                    p.kind
                };

                if group_indices.len() <= 1 {
                    // Lone `.001`-style file with no siblings: treat as a normal
                    // standalone archive (the file IS the complete archive).
                    EntryClassification::Standalone(kind)
                } else {
                    let mut sorted: Vec<usize> = group_indices.clone();
                    sorted.sort_by_key(|&gi| {
                        parsed[gi].as_ref().map(|q| q.index).unwrap_or(usize::MAX)
                    });
                    let parts: Vec<PathBuf> =
                        sorted.iter().map(|&gi| entries[gi].0.clone()).collect();
                    let is_primary = sorted.first().copied() == Some(i);
                    EntryClassification::SplitMember { kind, is_primary, parts }
                }
            }
            None => match archive_type(name) {
                Some(t) => EntryClassification::Standalone(t),
                None => EntryClassification::NonArchive,
            },
        };
        out.push((path.clone(), class));
    }

    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SplitScheme {
    /// `stem.partNN.rar` — modern RAR multipart.
    RarMultipart,
    /// `stem.rNN` — legacy RAR volumes; primary is `stem.rar`.
    RarLegacy,
    /// `stem.zip.NNN` — explicit ZIP split.
    ZipNumbered,
    /// `stem.7z.NNN` — explicit 7z split.
    SevenZNumbered,
    /// `stem.NNN` — bare numeric split (3+ digits), sniffed from magic bytes.
    BareNumbered,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedSplit {
    scheme: SplitScheme,
    /// Preliminary archive kind; overridden by sniffing for `BareNumbered`.
    kind: ArchiveType,
    stem: String,
    /// Numeric portion parsed from the filename. For `RarLegacy`, `.rNN` parses
    /// to `NN + 1` so the synthetic `.rar` primary at index 0 sorts first.
    index: usize,
    digits: usize,
}

fn strip_trailing_ascii_digits(s: &str) -> (&str, &str) {
    let bytes = s.as_bytes();
    let mut idx = bytes.len();
    while idx > 0 && bytes[idx - 1].is_ascii_digit() {
        idx -= 1;
    }
    (&s[..idx], &s[idx..])
}

/// Try to parse `name` (a leaf filename) as a split-archive member. Performs
/// no I/O. Returns the parsed components if the name matches any of the
/// supported split naming conventions, else `None`.
fn parse_split_member(name: &str) -> Option<ParsedSplit> {
    let lower = name.to_ascii_lowercase();

    // *.partNN.rar — modern RAR multipart.
    if let Some(before_rar) = lower.strip_suffix(".rar") {
        let (head, digits) = strip_trailing_ascii_digits(before_rar);
        if !digits.is_empty() {
            if let Some(stem) = head.strip_suffix(".part") {
                return Some(ParsedSplit {
                    scheme: SplitScheme::RarMultipart,
                    kind: ArchiveType::Rar,
                    stem: stem.to_string(),
                    index: digits.parse().unwrap_or(0),
                    digits: digits.len(),
                });
            }
        }
    }

    // *.zip.NNN
    if let Some((stem, idx, dlen)) = parse_typed_numbered(&lower, ".zip") {
        return Some(ParsedSplit {
            scheme: SplitScheme::ZipNumbered,
            kind: ArchiveType::Zip,
            stem,
            index: idx,
            digits: dlen,
        });
    }

    // *.7z.NNN
    if let Some((stem, idx, dlen)) = parse_typed_numbered(&lower, ".7z") {
        return Some(ParsedSplit {
            scheme: SplitScheme::SevenZNumbered,
            kind: ArchiveType::SevenZ,
            stem,
            index: idx,
            digits: dlen,
        });
    }

    // *.rNN — legacy RAR volumes. Index is `NN + 1` so the synthetic primary
    // `stem.rar` (added in `classify_directory`) sorts at position 0.
    if let Some(dot) = lower.rfind('.') {
        let ext = &lower[dot + 1..];
        let stem = &lower[..dot];
        if ext.len() >= 2
            && ext.starts_with('r')
            && ext[1..].bytes().all(|b| b.is_ascii_digit())
        {
            let num: usize = ext[1..].parse().unwrap_or(0);
            return Some(ParsedSplit {
                scheme: SplitScheme::RarLegacy,
                kind: ArchiveType::Rar,
                stem: stem.to_string(),
                index: num + 1,
                digits: ext.len() - 1,
            });
        }
    }

    // *.NNN — bare numeric split (3+ digits to reduce false positives on
    // extensions like `.v1`, `.mp3`, etc.). Kind is sniffed by the caller.
    if let Some(dot) = lower.rfind('.') {
        let ext = &lower[dot + 1..];
        let stem = &lower[..dot];
        if ext.len() >= 3 && ext.bytes().all(|b| b.is_ascii_digit()) {
            return Some(ParsedSplit {
                scheme: SplitScheme::BareNumbered,
                kind: ArchiveType::Zip, // placeholder; sniffed later
                stem: stem.to_string(),
                index: ext.parse().unwrap_or(0),
                digits: ext.len(),
            });
        }
    }

    None
}

/// Match `*.<archive_ext>.NNN` (e.g. `name.zip.001`). `archive_ext` must
/// include its leading dot (`.zip`, `.7z`). Returns (stem, parsed_index, digits_count).
fn parse_typed_numbered(lower: &str, archive_ext: &str) -> Option<(String, usize, usize)> {
    debug_assert!(archive_ext.starts_with('.'));
    let (before_digits, digits) = strip_trailing_ascii_digits(lower);
    if digits.is_empty() {
        return None;
    }
    // Strip the separator dot between `<archive_ext>` and the digits.
    let before_dot = before_digits.strip_suffix('.')?;
    let stem = before_dot.strip_suffix(archive_ext)?;
    Some((stem.to_string(), digits.parse().unwrap_or(0), digits.len()))
}

/// Sniff magic bytes from the first few bytes of `path` to decide ZIP vs 7z.
fn sniff_archive_type(path: &Path) -> Option<ArchiveType> {
    use std::io::Read;
    let mut f = fs::File::open(path).ok()?;
    let mut buf = [0u8; 6];
    let n = f.read(&mut buf).ok()?;
    if n >= 4
        && buf[..2] == *b"PK"
        && matches!((buf[2], buf[3]), (0x03, 0x04) | (0x07, 0x08))
    {
        return Some(ArchiveType::Zip);
    }
    if n >= 6 && buf[..6] == *b"7z\xbc\xaf\x27\x1c" {
        return Some(ArchiveType::SevenZ);
    }
    None
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

/// True iff the archive's filenames/headers are encrypted, i.e. listing is
/// impossible without a password. Always `false` for non-archives and for
/// formats that never encrypt filenames (ZIP).
pub fn requires_password_for_listing(path: &Path) -> Result<bool> {
    match archive_type_ext(path) {
        Some(ArchiveType::Zip) => zip::zip_needs_password_to_list(path),
        Some(ArchiveType::Rar) => rar::rar_needs_password_to_list(path),
        Some(ArchiveType::SevenZ) => sevenz::sevenz_needs_password_to_list(path),
        None => Ok(false),
    }
}

/// True iff the archive's *content* is encrypted while filenames remain
/// readable (so listing works but reading entries needs a password). Always
/// `false` for non-archives.
pub fn requires_password_for_content(path: &Path) -> Result<bool> {
    match archive_type_ext(path) {
        Some(ArchiveType::Zip) => zip::zip_content_encrypted(path),
        Some(ArchiveType::Rar) => rar::rar_content_encrypted(path),
        Some(ArchiveType::SevenZ) => sevenz::sevenz_content_encrypted(path),
        None => Ok(false),
    }
}

/// Validate the currently-cached password for `archive` by decrypting a small
/// probe entry. Used by the UI's wrong-password retry loop. Returns `Ok(())`
/// if the password decrypts, `Err` otherwise.
pub fn validate_password_by_probe(archive: &Path) -> Result<()> {
    let pwd = match get_password(archive) {
        Some(p) => p,
        None => bail!("no password cached to validate"),
    };
    match archive_type_ext(archive) {
        Some(ArchiveType::Zip) => zip::zip_validate_password(archive, &pwd),
        Some(ArchiveType::Rar) => rar::rar_validate_password(archive, &pwd),
        Some(ArchiveType::SevenZ) => sevenz::sevenz_validate_password(archive, &pwd),
        None => bail!("not an archive: {}", archive.display()),
    }
}

/// Read the full bytes of a single archive entry. Used for loading images.
pub fn read_entry(path: &EntryPath) -> Result<Vec<u8>> {
    match path {
        EntryPath::Native(_) => bail!("read_entry is only valid for archive paths"),
        EntryPath::InZip(arc, name) => zip::read_zip_entry(arc, name, get_password(arc).as_deref()),
        EntryPath::InRar(arc, name) => rar::read_rar_entry(arc, name, get_password(arc).as_deref()),
        EntryPath::InSevenZ(arc, name) => {
            sevenz::read_7z_entry(arc, name, get_password(arc).as_deref())
        }
    }
}

/// List entries under the prefix of an `InZip` / `InRar` / `InSevenZ` cwd.
pub fn list_archive(cwd: &EntryPath) -> Result<Vec<ArchiveEntry>> {
    match cwd {
        EntryPath::InZip(arc, prefix) => zip::list_zip(arc, prefix, get_password(arc).as_deref()),
        EntryPath::InRar(arc, prefix) => rar::list_rar(arc, prefix, get_password(arc).as_deref()),
        EntryPath::InSevenZ(arc, prefix) => {
            sevenz::list_7z(arc, prefix, get_password(arc).as_deref())
        }
        EntryPath::Native(_) => bail!("list_archive is only valid for archive paths"),
    }
}

/// Spawn a local HTTP server that streams an archive entry to ffmpeg.
/// Returns `None` for native filesystem paths (caller handles those directly).
pub fn stream_video(path: &EntryPath) -> Option<(String, Arc<AtomicBool>)> {
    match path {
        EntryPath::Native(_) => None,
        EntryPath::InZip(arc, name) => {
            Some(zip::stream_zip_video(arc.clone(), name.clone(), get_password(arc)))
        }
        EntryPath::InRar(arc, name) => {
            Some(rar::stream_rar_video(arc.clone(), name.clone(), get_password(arc)))
        }
        EntryPath::InSevenZ(arc, name) => Some(sevenz::stream_7z_video(
            arc.clone(),
            name.clone(),
            get_password(arc),
        )),
    }
}

#[cfg(test)]
mod split_tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn unique_dir(label: &str) -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("pixview-test-{label}-{nanos:x}"));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn touch(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, contents).unwrap();
        p
    }

    fn classify_by_name(dir: &Path, name: &str) -> EntryClassification {
        let list = classify_directory(dir).unwrap();
        list.iter()
            .find(|(p, _)| p.file_name().map(|n| n == name).unwrap_or(false))
            .map(|(_, c)| c.clone())
            .unwrap_or(EntryClassification::NonArchive)
    }

    #[test]
    fn parse_modern_rar_multipart() {
        let p = parse_split_member("archive.part01.rar").unwrap();
        assert_eq!(p.scheme, SplitScheme::RarMultipart);
        assert_eq!(p.stem, "archive");
        assert_eq!(p.index, 1);
        assert_eq!(p.digits, 2);
        assert!(parse_split_member("archive.part1.rar").is_some());
        assert!(parse_split_member("archive.part001.rar").is_some());
        // The plain `.rar` (primary of a multipart set) does NOT match — it's
        // synthesised separately in classify_directory.
        assert!(parse_split_member("archive.rar").is_none());
    }

    #[test]
    fn parse_legacy_rar_volumes() {
        let p = parse_split_member("archive.r00").unwrap();
        assert_eq!(p.scheme, SplitScheme::RarLegacy);
        assert_eq!(p.index, 1); // r00 → ordinal 1; .rar primary is ordinal 0
        assert_eq!(p.digits, 2);
        assert!(parse_split_member("archive.r99").is_some());
        // Single-letter `.r0` is rejected (need >= 2 chars in ext).
        assert!(parse_split_member("archive.r0").is_some()); // r0 → ext "r0", len=2
    }

    #[test]
    fn parse_zip_split() {
        let p = parse_split_member("archive.zip.001").unwrap();
        assert_eq!(p.scheme, SplitScheme::ZipNumbered);
        assert_eq!(p.stem, "archive");
        assert_eq!(p.index, 1);
        assert_eq!(p.digits, 3);
    }

    #[test]
    fn parse_sevenz_split() {
        let p = parse_split_member("archive.7z.042").unwrap();
        assert_eq!(p.scheme, SplitScheme::SevenZNumbered);
        assert_eq!(p.index, 42);
    }

    #[test]
    fn parse_bare_numeric_requires_three_digits() {
        assert!(parse_split_member("archive.001").is_some());
        assert!(parse_split_member("archive.0001").is_some());
        // 1-2 digit suffixes are NOT bare-numeric splits (avoids `.v1`, `.mp3` etc.)
        assert!(parse_split_member("archive.01").is_none());
        assert!(parse_split_member("archive.1").is_none());
    }

    #[test]
    fn parse_rejects_non_splits() {
        assert!(parse_split_member("archive.zip").is_none());
        assert!(parse_split_member("archive.rar").is_none());
        assert!(parse_split_member("archive.7z").is_none());
        assert!(parse_split_member("photo.jpg").is_none());
        assert!(parse_split_member("video.mp4").is_none());
        assert!(parse_split_member("readme").is_none());
    }

    #[test]
    fn classify_modern_rar_set() {
        let dir = unique_dir("rar-new");
        touch(&dir, "archive.part01.rar", b"x");
        touch(&dir, "archive.part02.rar", b"x");
        touch(&dir, "archive.part03.rar", b"x");

        let primary = classify_by_name(&dir, "archive.part01.rar");
        match primary {
            EntryClassification::SplitMember { kind, is_primary, parts } => {
                assert_eq!(kind, ArchiveType::Rar);
                assert!(is_primary);
                assert_eq!(parts.len(), 3);
                assert_eq!(parts[0].file_name().unwrap(), "archive.part01.rar");
            }
            other => panic!("expected SplitMember, got {other:?}"),
        }

        let non_primary = classify_by_name(&dir, "archive.part02.rar");
        match non_primary {
            EntryClassification::SplitMember { is_primary, parts, .. } => {
                assert!(!is_primary);
                assert_eq!(parts.len(), 3);
            }
            other => panic!("expected SplitMember, got {other:?}"),
        }
    }

    #[test]
    fn classify_legacy_rar_set() {
        let dir = unique_dir("rar-old");
        touch(&dir, "archive.rar", b"x");
        touch(&dir, "archive.r00", b"x");
        touch(&dir, "archive.r01", b"x");

        let primary = classify_by_name(&dir, "archive.rar");
        match primary {
            EntryClassification::SplitMember { kind, is_primary, parts } => {
                assert_eq!(kind, ArchiveType::Rar);
                assert!(is_primary);
                assert_eq!(parts.len(), 3);
                assert_eq!(parts[0].file_name().unwrap(), "archive.rar");
                assert_eq!(parts[1].file_name().unwrap(), "archive.r00");
            }
            other => panic!("expected SplitMember for .rar, got {other:?}"),
        }

        let vol = classify_by_name(&dir, "archive.r00");
        match vol {
            EntryClassification::SplitMember { is_primary, .. } => assert!(!is_primary),
            other => panic!("expected SplitMember for .r00, got {other:?}"),
        }
    }

    #[test]
    fn classify_zip_split_set() {
        let dir = unique_dir("zip");
        touch(&dir, "archive.zip.001", b"PK\x03\x04");
        touch(&dir, "archive.zip.002", b"x");

        let primary = classify_by_name(&dir, "archive.zip.001");
        match primary {
            EntryClassification::SplitMember { kind, is_primary, parts } => {
                assert_eq!(kind, ArchiveType::Zip);
                assert!(is_primary);
                assert_eq!(parts.len(), 2);
            }
            other => panic!("expected SplitMember, got {other:?}"),
        }
    }

    #[test]
    fn classify_bare_numeric_sniffs_zip() {
        let dir = unique_dir("bare-zip");
        touch(&dir, "archive.001", b"PK\x03\x04extra bytes here");
        touch(&dir, "archive.002", b"more bytes");

        let primary = classify_by_name(&dir, "archive.001");
        assert_eq!(primary.archive_type(), Some(ArchiveType::Zip));
        assert!(matches!(primary, EntryClassification::SplitMember { is_primary: true, .. }));
    }

    #[test]
    fn classify_bare_numeric_sniffs_sevenz() {
        let dir = unique_dir("bare-7z");
        touch(&dir, "archive.001", b"7z\xbc\xaf\x27\x1c rest");
        touch(&dir, "archive.002", b"x");

        let primary = classify_by_name(&dir, "archive.001");
        assert_eq!(primary.archive_type(), Some(ArchiveType::SevenZ));
    }

    #[test]
    fn classify_bare_numeric_unknown_magic_is_nonarchive() {
        let dir = unique_dir("bare-unknown");
        touch(&dir, "archive.001", b"unknown magic bytes");
        touch(&dir, "archive.002", b"x");

        let primary = classify_by_name(&dir, "archive.001");
        assert_eq!(primary, EntryClassification::NonArchive);
    }

    #[test]
    fn lone_numeric_part_is_standalone() {
        let dir = unique_dir("lone");
        touch(&dir, "archive.zip.001", b"PK\x03\x04 standalone zip body");

        let class = classify_by_name(&dir, "archive.zip.001");
        // No .002 sibling → treated as a standalone archive, not a split set.
        assert!(matches!(class, EntryClassification::Standalone(ArchiveType::Zip)));
    }

    #[test]
    fn split_set_returns_parts_for_primary() {
        let dir = unique_dir("splitset");
        touch(&dir, "a.7z.001", b"7z\xbc\xaf\x27\x1c");
        touch(&dir, "a.7z.002", b"x");
        touch(&dir, "a.7z.003", b"x");

        let primary = dir.join("a.7z.001");
        let parts = split_set(&primary).unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], primary);

        // Non-archive returns None.
        assert!(split_set(&dir.join("nonexistent.zip")).is_none());
    }

    #[test]
    fn archive_type_ext_handles_split_names() {
        let dir = unique_dir("ext");
        touch(&dir, "arc.part01.rar", b"x");
        touch(&dir, "arc.part02.rar", b"x");
        touch(&dir, "arc.zip.001", b"PK\x03\x04");
        touch(&dir, "arc.zip.002", b"x");
        touch(&dir, "plain.zip", b"PK\x03\x04");

        assert_eq!(archive_type_ext(&dir.join("arc.part01.rar")), Some(ArchiveType::Rar));
        assert_eq!(archive_type_ext(&dir.join("arc.zip.001")), Some(ArchiveType::Zip));
        assert_eq!(archive_type_ext(&dir.join("plain.zip")), Some(ArchiveType::Zip));
        assert_eq!(archive_type_ext(&dir.join("photo.jpg")), None);
    }
}
