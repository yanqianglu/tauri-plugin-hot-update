//! Hostile-archive tests: every archive is real tar.gz bytes, built either
//! with the `tar` crate (honest archives) or from hand-crafted 512-byte
//! headers (hostile entries the crate's own builder refuses to produce).

use std::fs;
use std::io::Write;
use std::path::Path;

use flate2::write::GzEncoder;
use flate2::Compression;
use tempfile::TempDir;

use super::*;

fn small_limits(max_entries: usize, max_total_bytes: u64) -> Limits {
    Limits {
        max_entries,
        max_total_bytes,
    }
}

/// gzip raw tar bytes (entries + the two 512-byte zero end blocks).
fn gz(tar_bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(tar_bytes).unwrap();
    encoder.write_all(&[0u8; 1024]).unwrap();
    encoder.finish().unwrap()
}

/// An honest archive built with the tar crate.
fn honest_archive(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for (path, data) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o755); // deliberately exotic; must not be preserved
        tar.append_data(&mut header, path, *data).unwrap();
    }
    gz(&tar.into_inner().unwrap())
}

/// A raw tar entry with full control over the name/type/size fields —
/// bypasses the builder's validation to model attacker-crafted archives.
fn raw_entry(name: &[u8], entry_type: tar::EntryType, size: u64, data: &[u8]) -> Vec<u8> {
    let mut header = tar::Header::new_gnu();
    header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name);
    if entry_type == tar::EntryType::GNUSparse {
        // A well-formed sparse header (valid octal realsize), so it survives
        // the tar crate's own parsing and reaches OUR type check.
        header.as_gnu_mut().unwrap().realsize = *b"00000000000\0";
    }
    header.set_entry_type(entry_type);
    header.set_size(size);
    header.set_mode(0o644);
    header.set_cksum();
    let mut out = header.as_bytes().to_vec();
    out.extend_from_slice(data);
    out.extend(std::iter::repeat(0).take(data.len().div_ceil(512) * 512 - data.len()));
    out
}

fn write_archive(dir: &Path, bytes: &[u8]) -> std::path::PathBuf {
    let path = dir.join("bundle.tar.gz");
    fs::write(&path, bytes).unwrap();
    path
}

fn setup(archive: &[u8]) -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let archive_path = write_archive(tmp.path(), archive);
    let target = tmp.path().join("out");
    fs::create_dir(&target).unwrap();
    (tmp, archive_path, target)
}

// ------------------------------------------------------------------ happy

#[test]
fn plain_files_extract_with_content_parents_and_default_permissions() {
    let files: &[(&str, &[u8])] = &[
        ("index.html", b"<html>hi</html>"),
        ("assets/deep/app.js", b"console.log(1)"),
    ];
    let (_tmp, archive, target) = setup(&honest_archive(files));
    extract_tar_gz(&archive, &target).expect("extract");

    assert_eq!(fs::read(target.join("index.html")).unwrap(), files[0].1);
    assert_eq!(
        fs::read(target.join("assets/deep/app.js")).unwrap(),
        files[1].1
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(target.join("index.html")).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0, "0755 header mode must not be preserved");
    }
}

#[test]
fn directory_entries_are_created() {
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Directory);
    header.set_size(0);
    header.set_mode(0o755);
    tar.append_data(&mut header, "static/fonts", &[][..]).unwrap();
    let (_tmp, archive, target) = setup(&gz(&tar.into_inner().unwrap()));

    extract_tar_gz(&archive, &target).expect("extract");
    assert!(target.join("static/fonts").is_dir());
}

// ------------------------------------------------------------ path safety

#[test]
fn traversing_absolute_and_non_normal_paths_are_rejected() {
    // Note "./index.html": intentionally strict — only plain relative-normal
    // paths are accepted, which is exactly what the signing CLI produces.
    // (Interior "." components like "a/./b" are normalized away by
    // Path::components and are therefore harmless, not tested here.)
    for hostile in [
        &b"../evil.txt"[..],
        b"a/../../evil.txt",
        b"/etc/evil.txt",
        b"./index.html",
        b"..",
        b"C:evil.txt",
        b"a\\evil.txt",
    ] {
        let bytes = gz(&raw_entry(hostile, tar::EntryType::Regular, 1, b"x"));
        let (_tmp, archive, target) = setup(&bytes);
        let result = extract_tar_gz(&archive, &target);
        assert!(
            matches!(result, Err(ExtractError::UnsafePath { .. })),
            "{} → {result:?}",
            String::from_utf8_lossy(hostile)
        );
        // Nothing may have landed outside (or inside) the target.
        assert!(fs::read_dir(&target).unwrap().next().is_none());
        assert!(!_tmp.path().join("evil.txt").exists());
    }
}

// -------------------------------------------------------- link exfiltration

#[test]
fn symlink_entries_are_rejected() {
    // A symlink to the app container would let the assets provider's
    // fs::read exfiltrate auth data into the webview (WP2 handoff).
    let mut entry = tar::Header::new_gnu();
    entry.as_gnu_mut().unwrap().name[..8].copy_from_slice(b"auth.txt");
    entry.set_entry_type(tar::EntryType::Symlink);
    entry.set_link_name("/data/secrets/auth.json").unwrap();
    entry.set_size(0);
    entry.set_cksum();
    let (_tmp, archive, target) = setup(&gz(entry.as_bytes().as_ref()));

    let result = extract_tar_gz(&archive, &target);
    assert!(
        matches!(
            result,
            Err(ExtractError::ForbiddenEntryType { entry_type: "symlink", .. })
        ),
        "{result:?}"
    );
    assert!(!target.join("auth.txt").exists());
}

#[test]
fn hardlink_entries_are_rejected() {
    let bytes = gz(&raw_entry(b"clone.txt", tar::EntryType::Link, 0, b""));
    let (_tmp, archive, target) = setup(&bytes);
    let result = extract_tar_gz(&archive, &target);
    assert!(
        matches!(
            result,
            Err(ExtractError::ForbiddenEntryType { entry_type: "hardlink", .. })
        ),
        "{result:?}"
    );
}

#[test]
fn device_fifo_and_sparse_entries_are_rejected() {
    for (entry_type, expected) in [
        (tar::EntryType::Char, "char device"),
        (tar::EntryType::Block, "block device"),
        (tar::EntryType::Fifo, "fifo"),
        (tar::EntryType::GNUSparse, "sparse file"),
    ] {
        let bytes = gz(&raw_entry(b"dev", entry_type, 0, b""));
        let (_tmp, archive, target) = setup(&bytes);
        let result = extract_tar_gz(&archive, &target);
        assert!(
            matches!(
                &result,
                Err(ExtractError::ForbiddenEntryType { entry_type, .. }) if *entry_type == expected
            ),
            "{expected} → {result:?}"
        );
    }
}

// ------------------------------------------------------------- bomb caps

#[test]
fn entry_count_cap_is_enforced() {
    let mut raw = Vec::new();
    for i in 0..4 {
        raw.extend(raw_entry(format!("f{i}").as_bytes(), tar::EntryType::Regular, 0, b""));
    }
    let bytes = gz(&raw);
    let (_tmp, archive, target) = setup(&bytes);

    let ok = extract_with_limits(&archive, &target, small_limits(4, 1024));
    assert!(ok.is_ok(), "{ok:?}");
    let result = extract_with_limits(&archive, &target, small_limits(3, 1024));
    assert!(
        matches!(result, Err(ExtractError::TooManyEntries { limit: 3 })),
        "{result:?}"
    );
}

#[test]
fn production_size_cap_refuses_a_lying_giant_header_before_reading_data() {
    // Header declares 600 MB; the cap must trip on the declared size,
    // before any data is pulled through the decoder.
    let bytes = gz(&raw_entry(
        b"huge.bin",
        tar::EntryType::Regular,
        MAX_UNCOMPRESSED_BYTES + 1,
        b"",
    ));
    let (_tmp, archive, target) = setup(&bytes);
    let result = extract_tar_gz(&archive, &target);
    assert!(
        matches!(result, Err(ExtractError::TooLarge { limit: MAX_UNCOMPRESSED_BYTES })),
        "{result:?}"
    );
}

#[test]
fn total_size_cap_accumulates_across_entries() {
    let mut raw = Vec::new();
    raw.extend(raw_entry(b"a.bin", tar::EntryType::Regular, 600, &[0u8; 600]));
    raw.extend(raw_entry(b"b.bin", tar::EntryType::Regular, 600, &[0u8; 600]));
    let bytes = gz(&raw);
    let (_tmp, archive, target) = setup(&bytes);
    let result = extract_with_limits(&archive, &target, small_limits(10, 1000));
    assert!(
        matches!(result, Err(ExtractError::TooLarge { limit: 1000 })),
        "{result:?}"
    );
}

#[test]
fn stream_cap_stops_decompression_bombs_hidden_in_tar_metadata() {
    // A GNU long-name entry whose data the tar crate buffers internally —
    // per-entry size accounting never sees it, the raw stream cap does.
    // (Stream cap = max_total_bytes + 32 MB slack, so 40 MB of metadata
    // with max_total_bytes = 0 crosses it.)
    let name_blob = vec![b'a'; 40 * 1024 * 1024];
    let raw = raw_entry(
        b"././@LongLink",
        tar::EntryType::GNULongName,
        name_blob.len() as u64,
        &name_blob,
    );
    let bytes = gz(&raw);
    let (_tmp, archive, target) = setup(&bytes);
    let result = extract_with_limits(&archive, &target, small_limits(10, 0));
    assert!(
        matches!(result, Err(ExtractError::TooLarge { limit: 0 })),
        "{result:?}"
    );
}

#[test]
fn truncated_archive_is_an_io_error_not_a_panic() {
    let full = honest_archive(&[("index.html", b"data")]);
    let (_tmp, archive, target) = setup(&full[..full.len() / 2]);
    let result = extract_tar_gz(&archive, &target);
    assert!(matches!(result, Err(ExtractError::Io(_))), "{result:?}");
}
