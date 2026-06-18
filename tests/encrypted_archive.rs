//! End-to-end tests for password-protected archive support.
//!
//! Creates real encrypted archives via the system `zip` / `7z` tools (skipped
//! when a tool is absent) and exercises the detection, listing, reading and
//! password-validation paths of the `archive` module.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use pixview::archive::{
    self, clear_password, get_password, list_archive, read_entry, requires_password_for_content,
    requires_password_for_listing, set_password, validate_password_by_probe, EntryPath,
};

/// Small PNG payload used as the archive's only file.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, // signature
    0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x49, 0x44, 0x41, 0x54, 0x78,
    0x9c, 0x62, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

fn unique_dir(label: &str) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("pixview-enc-test-{label}-{nanos:x}"));
    fs::create_dir_all(&p).unwrap();
    p
}

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .map(|o| o.status.success() || !o.stderr.is_empty())
        .unwrap_or_else(|_| {
            Command::new(tool)
                .output()
                .is_ok()
        })
}

/// Create a classic (ZipCrypto) encrypted zip containing `png.png`.
fn make_encrypted_zip(dir: &Path, password: &str) -> Option<PathBuf> {
    if !have("zip") {
        return None;
    }
    let src = dir.join("png.png");
    fs::write(&src, PNG).unwrap();
    let out = dir.join("enc.zip");
    // Use relative names so Info-ZIP stores the leaf name "png.png".
    let status = Command::new("zip")
        .arg("-q")
        .arg("-j") // junk paths: store leaf name only
        .arg("-P")
        .arg(password)
        .arg(&out)
        .arg(&src)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    Some(out)
}

#[test]
fn encrypted_zip_content_only() {
    let dir = unique_dir("zip");
    let Some(zip) = make_encrypted_zip(&dir, "secret") else {
        eprintln!("skipping: zip tool not available");
        return;
    };

    // ZIP never encrypts filenames: listing doesn't need a password...
    assert!(!requires_password_for_listing(&zip).unwrap());
    // ...but the content is encrypted.
    assert!(requires_password_for_content(&zip).unwrap());

    // Filenames are visible without a password.
    let entries = list_archive(&EntryPath::InZip(zip.clone(), String::new())).unwrap();
    assert!(entries.iter().any(|e| e.display_name.contains("png")));

    // The internal entry name depends on how `zip` stored it; find it.
    let name = entries
        .iter()
        .find(|e| !e.is_dir)
        .map(|e| e.internal_path.clone())
        .unwrap();

    // Reading without a password fails.
    clear_password(&zip);
    let read = read_entry(&EntryPath::InZip(zip.clone(), name.clone()));
    assert!(read.is_err(), "read without password should fail");

    // Correct password round-trips the bytes.
    set_password(&zip, "secret");
    let data = read_entry(&EntryPath::InZip(zip.clone(), name.clone())).unwrap();
    assert_eq!(data, PNG);
    assert_eq!(get_password(&zip).as_deref(), Some("secret"));

    // Probe validation accepts the right password...
    assert!(validate_password_by_probe(&zip).is_ok());

    // ...and rejects a wrong one.
    set_password(&zip, "wrong");
    assert!(validate_password_by_probe(&zip).is_err());

    clear_password(&zip);
}

/// Create a 7z archive. With `encrypt_headers` the filenames are encrypted.
fn make_encrypted_7z(dir: &Path, password: &str, encrypt_headers: bool) -> Option<PathBuf> {
    if !have("7z") {
        return None;
    }
    let src = dir.join("png.png");
    fs::write(&src, PNG).unwrap();
    let out = dir.join(if encrypt_headers { "hdr.7z" } else { "content.7z" });
    let mut cmd = Command::new("7z");
    cmd.arg("a").arg("-bd").arg("-y").arg("-p").arg(password);
    if encrypt_headers {
        cmd.arg("-mhe=on");
    }
    let status = cmd.arg(&out).arg(&src).current_dir(dir).status().ok()?;
    if !status.success() {
        return None;
    }
    Some(out)
}

#[test]
fn encrypted_7z_content_only() {
    let dir = unique_dir("7z-content");
    let Some(arc) = make_encrypted_7z(&dir, "secret", false) else {
        eprintln!("skipping: 7z tool not available");
        return;
    };

    // Content-encrypted 7z: headers readable, content encrypted.
    assert!(!requires_password_for_listing(&arc).unwrap());
    assert!(requires_password_for_content(&arc).unwrap());

    // Listing works without a password.
    let entries = list_archive(&EntryPath::InSevenZ(arc.clone(), String::new())).unwrap();
    let name = entries
        .iter()
        .find(|e| !e.is_dir)
        .map(|e| e.internal_path.clone())
        .unwrap();

    // Reading needs the password.
    clear_password(&arc);
    assert!(read_entry(&EntryPath::InSevenZ(arc.clone(), name.clone())).is_err());
    set_password(&arc, "secret");
    let data = read_entry(&EntryPath::InSevenZ(arc.clone(), name.clone())).unwrap();
    assert_eq!(data, PNG);

    // Probe validation.
    assert!(validate_password_by_probe(&arc).is_ok());
    set_password(&arc, "nope");
    assert!(validate_password_by_probe(&arc).is_err());

    clear_password(&arc);
}

#[test]
fn encrypted_7z_headers() {
    let dir = unique_dir("7z-hdr");
    let Some(arc) = make_encrypted_7z(&dir, "secret", true) else {
        eprintln!("skipping: 7z tool not available");
        return;
    };

    // Header-encrypted 7z: listing is impossible without the password.
    assert!(requires_password_for_listing(&arc).unwrap());

    // Listing without a password fails.
    clear_password(&arc);
    assert!(list_archive(&EntryPath::InSevenZ(arc.clone(), String::new())).is_err());

    // With the password, listing + reading both work.
    set_password(&arc, "secret");
    let entries = list_archive(&EntryPath::InSevenZ(arc.clone(), String::new())).unwrap();
    let name = entries
        .iter()
        .find(|e| !e.is_dir)
        .map(|e| e.internal_path.clone())
        .unwrap();
    let data = read_entry(&EntryPath::InSevenZ(arc.clone(), name)).unwrap();
    assert_eq!(data, PNG);

    clear_password(&arc);
}

#[test]
fn non_encrypted_archives_need_no_password() {
    let dir = unique_dir("plain");
    let Some(zip) = (|| {
        let src = dir.join("png.png");
        fs::write(&src, PNG).unwrap();
        let out = dir.join("plain.zip");
        let status = Command::new("zip")
            .arg("-q")
            .arg(&out)
            .arg(&src)
            .current_dir(&dir)
            .status()
            .ok()?;
        if status.success() {
            Some(out)
        } else {
            None
        }
    })() else {
        eprintln!("skipping: zip tool not available");
        return;
    };

    assert!(!requires_password_for_listing(&zip).unwrap());
    assert!(!requires_password_for_content(&zip).unwrap());
    // No password cached, yet reading/listing should work.
    assert!(list_archive(&EntryPath::InZip(zip.clone(), String::new())).is_ok());
}

// `archive` is used only for its module path here; silence unused-import
// warning if all tool-dependent tests are skipped.
#[allow(unused_imports)]
use archive as _;
