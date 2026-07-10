//! Round-trips the real signing code against the real verify path with
//! fresh keys — the same code the `hot-update-sign` CLI runs.

use std::fs;
use std::path::Path;

use minisign::KeyPair;
use semver::Version;
use sha2::Digest;
use tempfile::TempDir;

use super::*;
use crate::manifest::verify_and_parse;

fn ver(s: &str) -> Version {
    Version::parse(s).unwrap()
}

fn make_dist(dir: &Path) {
    fs::create_dir_all(dir.join("assets/deep")).unwrap();
    fs::write(dir.join("index.html"), b"<html>watermoon</html>").unwrap();
    fs::write(dir.join("assets/app.js"), b"console.log('ota')").unwrap();
    fs::write(dir.join("assets/deep/style.css"), b"body{}").unwrap();
}

fn sign_fixture(tmp: &Path, out: &str) -> (KeyPair, SignedRelease) {
    let dist = tmp.join("dist");
    if !dist.exists() {
        make_dist(&dist);
    }
    let keypair = KeyPair::generate_unencrypted_keypair().unwrap();
    let release = sign_release(
        &SignOptions {
            dist_dir: &dist,
            version: ver("1.2.0"),
            min_shell_version: ver("1.1.0"),
            base_url: "https://cdn.example.com/ota/",
            out_dir: &tmp.join(out),
        },
        &keypair.sk,
    )
    .expect("sign_release");
    (keypair, release)
}

#[test]
fn signed_release_verifies_under_the_plugin_verify_path() {
    let tmp = TempDir::new().unwrap();
    let (keypair, release) = sign_fixture(tmp.path(), "out");

    let manifest_bytes = fs::read(&release.manifest_path).unwrap();
    let signature = fs::read_to_string(&release.signature_path).unwrap();
    let manifest = verify_and_parse(&manifest_bytes, &signature, &[keypair.pk.to_base64()])
        .expect("the release must verify under its own public key");

    assert_eq!(manifest, release.manifest);
    assert_eq!(manifest.version, ver("1.2.0"));
    assert_eq!(manifest.min_shell_version, ver("1.1.0"));
    // Trailing slash on base_url must not double up.
    assert_eq!(
        manifest.archive.url,
        "https://cdn.example.com/ota/bundle-1.2.0.tar.gz"
    );

    let archive_bytes = fs::read(&release.archive_path).unwrap();
    assert_eq!(manifest.archive.size, archive_bytes.len() as u64);
    assert_eq!(
        manifest.archive.sha256,
        crate::download::to_hex(&sha2::Sha256::digest(&archive_bytes))
    );
    // createdAt is real RFC 3339.
    time::OffsetDateTime::parse(
        &manifest.created_at,
        &time::format_description::well_known::Rfc3339,
    )
    .expect("createdAt must be RFC 3339");
}

#[test]
fn a_release_signed_by_a_different_key_does_not_verify() {
    let tmp = TempDir::new().unwrap();
    let (_signer, release) = sign_fixture(tmp.path(), "out");
    let other = KeyPair::generate_unencrypted_keypair().unwrap();

    let manifest_bytes = fs::read(&release.manifest_path).unwrap();
    let signature = fs::read_to_string(&release.signature_path).unwrap();
    let result = verify_and_parse(&manifest_bytes, &signature, &[other.pk.to_base64()]);
    assert!(matches!(result, Err(crate::Error::ManifestSignature(_))), "{result:?}");
}

#[test]
fn archive_build_is_deterministic() {
    let tmp = TempDir::new().unwrap();
    let (_k1, first) = sign_fixture(tmp.path(), "out1");
    let (_k2, second) = sign_fixture(tmp.path(), "out2");
    assert_eq!(
        fs::read(&first.archive_path).unwrap(),
        fs::read(&second.archive_path).unwrap(),
        "same dist must produce byte-identical archives (and thus one sha256)"
    );
}

#[test]
fn produced_archive_round_trips_through_the_hardened_extractor() {
    let tmp = TempDir::new().unwrap();
    let (_keypair, release) = sign_fixture(tmp.path(), "out");
    let target = tmp.path().join("extracted");
    fs::create_dir(&target).unwrap();

    crate::extract::extract_tar_gz(&release.archive_path, &target)
        .expect("our own archives must pass our own hardening");
    assert_eq!(
        fs::read(target.join("index.html")).unwrap(),
        b"<html>watermoon</html>"
    );
    assert_eq!(
        fs::read(target.join("assets/deep/style.css")).unwrap(),
        b"body{}"
    );
}

#[test]
fn empty_dist_dir_is_refused() {
    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("empty");
    fs::create_dir(&dist).unwrap();
    let keypair = KeyPair::generate_unencrypted_keypair().unwrap();
    let result = sign_release(
        &SignOptions {
            dist_dir: &dist,
            version: ver("1.0.0"),
            min_shell_version: ver("1.0.0"),
            base_url: "https://x",
            out_dir: &tmp.path().join("out"),
        },
        &keypair.sk,
    );
    assert!(matches!(result, Err(SignError::EmptyDist(_))), "{result:?}");
}
