//! Integration tests for split / multi-part archive support.
//!
//! These tests generate real split ZIP and 7z fixtures on the fly using the
//! system `zip` / `7z` tools, then exercise the `pixview::archive` module
//! against them. RAR multipart can't be generated here (the `rar` encoder is
//! proprietary), but its detection logic is covered by unit tests in
//! `src/archive/mod.rs`.
//!
//! Tests are skipped automatically when the required tool isn't on `$PATH`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use pixview::archive::{
    archive_type_ext, classify_directory, list_archive, read_entry, split_set, ArchiveType,
    EntryClassification, EntryPath,
};

// --------------------------------------------------------------------------------
// Helpers
// --------------------------------------------------------------------------------

fn unique_dir(label: &str) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("pixview-it-{label}-{nanos:x}"));
    fs::create_dir_all(&p).unwrap();
    p
}

fn have(tool: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {tool} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Simple xorshift64* PRNG so tests are deterministic but produce
/// hard-to-compress content (otherwise a few-KB text payload compresses to
/// near-nothing and the resulting zip/7z fits in a single volume).
fn random_bytes(seed: u64, n: usize) -> Vec<u8> {
    let mut state = seed.wrapping_add(0x9E3779B97F4A7C15);
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let bytes = state.to_le_bytes();
        let take = bytes.len().min(n - out.len());
        out.extend_from_slice(&bytes[..take]);
    }
    out
}

/// Build a payload file body: a short identifying header followed by
/// `body_len` bytes of pseudo-random data so the compressed archive is
/// large enough to span multiple parts.
fn sample_payload(name: &str, seed: u64, body_len: usize) -> (String, Vec<u8>) {
    let mut body = Vec::with_capacity(64 + body_len);
    body.extend_from_slice(b"PIXVIEW-TEST-");
    body.extend_from_slice(name.as_bytes());
    body.extend_from_slice(b"-");
    body.extend_from_slice(&random_bytes(seed, body_len));
    (name.to_string(), body)
}

fn write_payload(dir: &Path, name: &str, body: &[u8]) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, body).unwrap();
    p
}

/// Chunk `whole` into `prefix.001`, `prefix.002`, ... files under `dir`,
/// each `chunk_size` bytes (the last may be shorter). Removes `whole`.
/// Returns the ordered part paths. Numbering starts at 1 to match the
/// conventional split-archive naming (the system `split -d` tool starts at 0,
/// which is why we don't use it here).
fn chunk_into_prefix(dir: &Path, whole: &Path, prefix: &str, chunk_size: usize) -> Vec<PathBuf> {
    let data = fs::read(whole).unwrap();
    let mut parts = Vec::new();
    for (i, chunk) in data.chunks(chunk_size).enumerate() {
        let name = format!("{}.{:0>3}", prefix, i + 1);
        let p = dir.join(name);
        fs::write(&p, chunk).unwrap();
        parts.push(p);
    }
    fs::remove_file(whole).unwrap();
    assert!(
        parts.len() >= 2,
        "chunking produced only {} part(s); increase payload size or shrink chunk_size",
        parts.len()
    );
    parts
}

fn first_classified(dir: &Path, name: &str) -> EntryClassification {
    classify_directory(dir)
        .unwrap()
        .into_iter()
        .find(|(p, _)| p.file_name().map(|n| n == name).unwrap_or(false))
        .map(|(_, c)| c)
        .unwrap_or(EntryClassification::NonArchive)
}

// --------------------------------------------------------------------------------
// ZIP split (.zip.001 / .zip.002 / ...) via `zip` + in-process chunker
// --------------------------------------------------------------------------------

fn build_split_zip(dir: &Path, chunk_size: usize) -> (PathBuf, Vec<(String, Vec<u8>)>) {
    let work = dir.join("zip-work");
    fs::create_dir_all(&work).unwrap();

    // Two payloads, 6 KiB of incompressible body each → ~12 KiB zip → many parts.
    let payloads = vec![
        sample_payload("alpha.bin", 1, 6 * 1024),
        sample_payload("beta.bin", 2, 6 * 1024),
    ];
    for (name, body) in &payloads {
        write_payload(&work, name, body);
    }

    let whole = dir.join("bundle.zip");
    let status = Command::new("zip")
        .arg("-q")
        .arg("-X") // strip extra attributes for reproducibility
        .arg(&whole)
        .arg("alpha.bin")
        .arg("beta.bin")
        .current_dir(&work)
        .status()
        .expect("zip failed to start");
    assert!(status.success(), "zip step failed");

    let parts = chunk_into_prefix(dir, &whole, "bundle.zip", chunk_size);
    (parts.into_iter().next().unwrap(), payloads)
}

#[test]
fn split_zip_classification() {
    if !have("zip") {
        eprintln!("skipping: zip not available");
        return;
    }
    let dir = unique_dir("zip-class");
    let (primary, _) = build_split_zip(&dir, 1024);

    // Primary (.001) should classify as SplitMember + Zip.
    let class = first_classified(&dir, "bundle.zip.001");
    let expected_parts = match &class {
        EntryClassification::SplitMember { kind, is_primary, parts } => {
            assert_eq!(*kind, ArchiveType::Zip);
            assert!(*is_primary);
            assert!(parts.len() >= 2, "expected at least 2 parts, got {}", parts.len());
            assert_eq!(parts[0].file_name().unwrap(), "bundle.zip.001");
            parts.len()
        }
        other => panic!("expected SplitMember for primary, got {other:?}"),
    };

    // Secondary parts are non-primary members of the same set.
    let next = first_classified(&dir, "bundle.zip.002");
    match next {
        EntryClassification::SplitMember { is_primary, parts, .. } => {
            assert!(!is_primary);
            assert_eq!(parts.len(), expected_parts);
        }
        other => panic!("expected SplitMember for secondary, got {other:?}"),
    }

    assert_eq!(archive_type_ext(&primary), Some(ArchiveType::Zip));
}

#[test]
fn split_zip_split_set_helper() {
    if !have("zip") {
        eprintln!("skipping: zip not available");
        return;
    }
    let dir = unique_dir("zip-set");
    let (primary, _) = build_split_zip(&dir, 1024);

    let parts = split_set(&primary).expect("primary should resolve to a split set");
    assert!(parts.len() >= 2);
    assert_eq!(parts[0], primary);
}

#[test]
fn split_zip_list_and_read() {
    if !have("zip") {
        eprintln!("skipping: zip not available");
        return;
    }
    let dir = unique_dir("zip-rw");
    let (primary, payloads) = build_split_zip(&dir, 1024);

    let cwd = EntryPath::InZip(primary.clone(), String::new());
    let entries = list_archive(&cwd).expect("list_archive on split zip");
    assert!(entries.len() >= payloads.len(), "expected entries from split zip");

    for (name, body) in &payloads {
        let entry = entries
            .iter()
            .find(|e| e.display_name == *name)
            .unwrap_or_else(|| panic!("entry {name} not listed"));
        let read = read_entry(&EntryPath::InZip(primary.clone(), entry.internal_path.clone()))
            .expect("read_entry on split zip");
        assert_eq!(read.len(), body.len(), "size mismatch for {name}");
        assert_eq!(&read[..], &body[..], "byte mismatch for {name}");
    }
}

// --------------------------------------------------------------------------------
// 7z split (.7z.001 / .7z.002 / ...) via `7z a -v`
// --------------------------------------------------------------------------------

fn build_split_7z(dir: &Path, volume_bytes: usize) -> (PathBuf, Vec<(String, Vec<u8>)>) {
    let work = dir.join("7z-work");
    fs::create_dir_all(&work).unwrap();

    let payloads = vec![sample_payload("gamma.bin", 3, 8 * 1024)];
    for (name, body) in &payloads {
        write_payload(&work, name, body);
    }

    let vol = format!("{}b", volume_bytes);
    let status = Command::new("7z")
        .arg("a")
        .arg(format!("-v{}", vol))
        .arg("-bso0") // suppress stdout
        .arg("-bsp0") // suppress progress
        .arg("../bundle.7z")
        .arg("gamma.bin")
        .current_dir(&work)
        .status()
        .expect("7z failed to start");
    assert!(status.success(), "7z step failed");

    // Verify multiple volumes were produced.
    let primary = dir.join("bundle.7z.001");
    assert!(
        primary.exists() && dir.join("bundle.7z.002").exists(),
        "7z did not produce multiple volumes"
    );
    (primary, payloads)
}

#[test]
fn split_7z_classification() {
    if !have("7z") {
        eprintln!("skipping: 7z not available");
        return;
    }
    let dir = unique_dir("7z-class");
    let (primary, _) = build_split_7z(&dir, 1024);

    let class = first_classified(&dir, "bundle.7z.001");
    match &class {
        EntryClassification::SplitMember { kind, is_primary, parts } => {
            assert_eq!(*kind, ArchiveType::SevenZ);
            assert!(*is_primary);
            assert!(parts.len() >= 2);
        }
        other => panic!("expected SplitMember for primary 7z, got {other:?}"),
    }

    assert_eq!(archive_type_ext(&primary), Some(ArchiveType::SevenZ));
}

#[test]
fn split_7z_list_and_read() {
    if !have("7z") {
        eprintln!("skipping: 7z not available");
        return;
    }
    let dir = unique_dir("7z-rw");
    let (primary, payloads) = build_split_7z(&dir, 1024);

    let cwd = EntryPath::InSevenZ(primary.clone(), String::new());
    let entries = list_archive(&cwd).expect("list_archive on split 7z");
    assert!(entries.len() >= payloads.len());

    for (name, body) in &payloads {
        let entry = entries
            .iter()
            .find(|e| e.display_name == *name)
            .unwrap_or_else(|| panic!("entry {name} not listed in 7z"));
        let read = read_entry(&EntryPath::InSevenZ(
            primary.clone(),
            entry.internal_path.clone(),
        ))
        .expect("read_entry on split 7z");
        assert_eq!(read.len(), body.len(), "size mismatch for {name}");
        assert_eq!(&read[..], &body[..], "byte mismatch for {name}");
    }
}

// --------------------------------------------------------------------------------
// Bare numeric split (.001 / .002 with sniffing)
// --------------------------------------------------------------------------------

#[test]
fn bare_numeric_zip_split_is_sniffed() {
    if !have("zip") {
        eprintln!("skipping: zip not available");
        return;
    }
    let dir = unique_dir("bare-zip");
    let work = dir.join("w");
    fs::create_dir_all(&work).unwrap();

    let p = sample_payload("only.bin", 4, 8 * 1024);
    write_payload(&work, &p.0, &p.1);

    let whole = dir.join("archive.zip");
    let status = Command::new("zip")
        .arg("-q")
        .arg("-X")
        .arg(&whole)
        .arg("only.bin")
        .current_dir(&work)
        .status()
        .unwrap();
    assert!(status.success());

    // Chunk to bare `archive.001`, `archive.002` (no `.zip` in name → forces
    // the BareNumbered + magic-sniffing path).
    chunk_into_prefix(&dir, &whole, "archive", 512);

    let primary = dir.join("archive.001");
    let class = first_classified(&dir, "archive.001");
    match &class {
        EntryClassification::SplitMember { kind, is_primary, .. } => {
            assert_eq!(*kind, ArchiveType::Zip);
            assert!(*is_primary);
        }
        other => panic!("expected SplitMember for bare zip split, got {other:?}"),
    }
    assert_eq!(archive_type_ext(&primary), Some(ArchiveType::Zip));
}
