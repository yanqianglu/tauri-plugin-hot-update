//! Signature-chain tests: real minisign keys generated in-test, the real
//! `minisign` signer against our `minisign-verify` verify path — no mocks.

use std::io::Cursor;

use minisign::KeyPair;
use semver::Version;

use super::*;
use crate::Error;

fn keypair() -> KeyPair {
    KeyPair::generate_unencrypted_keypair().expect("generate keypair")
}

fn sign(kp: &KeyPair, bytes: &[u8]) -> String {
    minisign::sign(None, &kp.sk, Cursor::new(bytes), None, None)
        .expect("sign")
        .into_string()
}

/// The design doc's manifest example, verbatim — the wire-format lock.
const DESIGN_DOC_MANIFEST: &str = r#"{
  "version": "1.2.0",
  "createdAt": "2026-07-09T00:00:00Z",
  "minShellVersion": "1.1.1",
  "archive": {
    "url": "https://example.com/bundle-1.2.0.tar.gz",
    "sha256": "b5bb9d8014a0f9b1d61e21e796d78dccdf1352f23cd32812f4850b878ae4944c",
    "size": 4194304
  }
}"#;

#[test]
fn valid_manifest_verifies_and_parses_the_designed_wire_format() {
    let kp = keypair();
    let bytes = DESIGN_DOC_MANIFEST.as_bytes();
    let manifest =
        verify_and_parse(bytes, &sign(&kp, bytes), &[kp.pk.to_base64()]).expect("verify");

    assert_eq!(manifest.version, Version::parse("1.2.0").unwrap());
    assert_eq!(manifest.created_at, "2026-07-09T00:00:00Z");
    assert_eq!(manifest.min_shell_version, Version::parse("1.1.1").unwrap());
    assert_eq!(
        manifest.archive.url,
        "https://example.com/bundle-1.2.0.tar.gz"
    );
    assert_eq!(
        manifest.archive.sha256,
        "b5bb9d8014a0f9b1d61e21e796d78dccdf1352f23cd32812f4850b878ae4944c"
    );
    assert_eq!(manifest.archive.size, 4194304);
}

#[test]
fn tampered_manifest_bytes_are_rejected() {
    let kp = keypair();
    let signature = sign(&kp, DESIGN_DOC_MANIFEST.as_bytes());
    // The attacker rewrites the version but keeps the valid signature.
    let tampered = DESIGN_DOC_MANIFEST.replace("1.2.0", "9.9.9");
    let result = verify_and_parse(tampered.as_bytes(), &signature, &[kp.pk.to_base64()]);
    assert!(
        matches!(result, Err(Error::ManifestSignature(_))),
        "{result:?}"
    );
}

#[test]
fn signature_by_an_untrusted_key_is_rejected() {
    let trusted = keypair();
    let attacker = keypair();
    let bytes = DESIGN_DOC_MANIFEST.as_bytes();
    let result = verify_and_parse(bytes, &sign(&attacker, bytes), &[trusted.pk.to_base64()]);
    assert!(
        matches!(result, Err(Error::ManifestSignature(_))),
        "{result:?}"
    );
}

#[test]
fn rotation_any_key_in_the_trusted_list_may_verify() {
    let old_key = keypair();
    let new_key = keypair();
    let bytes = DESIGN_DOC_MANIFEST.as_bytes();
    let signature = sign(&new_key, bytes);
    // Transition period: shell trusts [old, new]; manifest signed with new.
    let keys = [old_key.pk.to_base64(), new_key.pk.to_base64()];
    assert!(verify_and_parse(bytes, &signature, &keys).is_ok());
}

#[test]
fn garbage_signature_data_is_rejected() {
    let kp = keypair();
    let bytes = DESIGN_DOC_MANIFEST.as_bytes();
    for garbage in ["", "not a minisig", "untrusted comment: x\nAAAA"] {
        let result = verify_and_parse(bytes, garbage, &[kp.pk.to_base64()]);
        assert!(
            matches!(result, Err(Error::ManifestSignature(_))),
            "{result:?}"
        );
    }
}

#[test]
fn malformed_trust_anchor_is_a_hard_error_even_when_another_key_verifies() {
    let kp = keypair();
    let bytes = DESIGN_DOC_MANIFEST.as_bytes();
    let signature = sign(&kp, bytes);
    // Good key first: lazy per-key parsing would mask the broken anchor.
    let keys = [kp.pk.to_base64(), "AAAA not a key".to_string()];
    let result = verify_and_parse(bytes, &signature, &keys);
    assert!(matches!(result, Err(Error::InvalidPublicKey)), "{result:?}");
}

#[test]
fn empty_trusted_key_list_is_refused() {
    let kp = keypair();
    let bytes = DESIGN_DOC_MANIFEST.as_bytes();
    let result = verify_and_parse(bytes, &sign(&kp, bytes), &[]);
    assert!(matches!(result, Err(Error::InvalidPublicKey)), "{result:?}");
}

#[test]
fn full_minisign_pub_file_contents_are_accepted_as_a_trust_anchor() {
    let kp = keypair();
    let pub_file = kp.pk.to_box().expect("box").into_string();
    assert!(pub_file.lines().count() > 1, "expected the two-line format");
    let bytes = DESIGN_DOC_MANIFEST.as_bytes();
    assert!(verify_and_parse(bytes, &sign(&kp, bytes), &[pub_file]).is_ok());
}

#[test]
fn validly_signed_but_malformed_json_fails_parse() {
    let kp = keypair();
    for bad in [
        &b"not json"[..],
        br#"{"version": "1.2.0"}"#, // missing fields
        br#"{"version": "not-semver", "createdAt": "x", "minShellVersion": "1.0.0", "archive": {"url": "u", "sha256": "s", "size": 1}}"#,
    ] {
        let result = verify_and_parse(bad, &sign(&kp, bad), &[kp.pk.to_base64()]);
        assert!(matches!(result, Err(Error::ManifestParse(_))), "{result:?}");
    }
}

#[test]
fn unknown_manifest_fields_are_tolerated() {
    let kp = keypair();
    let bytes = DESIGN_DOC_MANIFEST.replace(
        "{\n  \"version\"",
        "{\n  \"futureField\": {\"a\": 1},\n  \"version\"",
    );
    let result = verify_and_parse(
        bytes.as_bytes(),
        &sign(&kp, bytes.as_bytes()),
        &[kp.pk.to_base64()],
    );
    assert!(result.is_ok(), "{result:?}");
}

#[test]
fn malformed_sha256_is_refused() {
    let kp = keypair();
    for bad_sha in ["deadbeef", "zz".repeat(32).as_str()] {
        let json = DESIGN_DOC_MANIFEST.replace(
            "b5bb9d8014a0f9b1d61e21e796d78dccdf1352f23cd32812f4850b878ae4944c",
            bad_sha,
        );
        let result = verify_and_parse(
            json.as_bytes(),
            &sign(&kp, json.as_bytes()),
            &[kp.pk.to_base64()],
        );
        assert!(
            matches!(result, Err(Error::ManifestInvalid(_))),
            "{result:?}"
        );
    }
}

#[test]
fn sha256_is_canonicalized_to_lowercase() {
    let kp = keypair();
    let json = DESIGN_DOC_MANIFEST.replace(
        "b5bb9d8014a0f9b1d61e21e796d78dccdf1352f23cd32812f4850b878ae4944c",
        "B5BB9D8014A0F9B1D61E21E796D78DCCDF1352F23CD32812F4850B878AE4944C",
    );
    let manifest = verify_and_parse(
        json.as_bytes(),
        &sign(&kp, json.as_bytes()),
        &[kp.pk.to_base64()],
    )
    .expect("verify");
    assert_eq!(
        manifest.archive.sha256,
        "b5bb9d8014a0f9b1d61e21e796d78dccdf1352f23cd32812f4850b878ae4944c"
    );
}

#[test]
fn nonsensical_archive_sizes_are_refused() {
    let kp = keypair();
    for bad_size in [
        "0",
        &(crate::extract::MAX_UNCOMPRESSED_BYTES + 1).to_string(),
    ] {
        let json = DESIGN_DOC_MANIFEST.replace("4194304", bad_size);
        let result = verify_and_parse(
            json.as_bytes(),
            &sign(&kp, json.as_bytes()),
            &[kp.pk.to_base64()],
        );
        assert!(
            matches!(result, Err(Error::ManifestInvalid(_))),
            "{result:?}"
        );
    }
}
