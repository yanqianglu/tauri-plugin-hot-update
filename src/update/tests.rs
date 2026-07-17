//! End-to-end pipeline tests: releases signed with the real CLI code
//! ([`crate::sign`]), served by a real local HTTP server, acquired through
//! the real pipeline into a real on-disk store. No mocks anywhere.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use minisign::KeyPair;
use semver::Version;

use super::*;
use crate::runtime::{self, HotUpdate, Shared};
use crate::testutil::{Fixture, Route};

fn ver(s: &str) -> Version {
    Version::parse(s).unwrap()
}

/// One cold launch, as in the runtime tests: fresh Shared over a persistent
/// store root.
fn boot(root: &Path, embedded: &str) -> HotUpdate {
    let shared = Arc::new(Shared::default());
    runtime::initialize(&shared, root.to_path_buf(), ver(embedded));
    HotUpdate { shared }
}

fn no_progress(_: u64, _: u64) {}

// ------------------------------------------------------------ happy path

#[tokio::test]
async fn end_to_end_signed_release_is_verified_extracted_and_staged() {
    let fx = Fixture::new();
    let (manifest, _) = fx.publish("1.1.0", "1.0.0");
    let hot_update = boot(&fx.root, "1.0.0");

    // check(): offered and applicable, but nothing downloaded.
    let outcome = hot_update.check(&fx.config()).await.unwrap();
    assert_eq!(
        outcome,
        UpdateOutcome::Available {
            manifest: manifest.clone()
        }
    );
    assert_eq!(fx.server.request_count("/bundle-1.1.0.tar.gz"), 0);

    // download: staged on disk, exactly as the state machine expects.
    let outcome = hot_update
        .check_and_download(&fx.config(), no_progress)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        UpdateOutcome::Staged {
            seq: 1,
            version: ver("1.1.0")
        }
    );
    let state = fx.state();
    assert_eq!(state["staged"], 1);
    assert_eq!(state["versions"]["1"]["version"], "1.1.0");
    assert_eq!(
        state["versions"]["1"]["archiveSha256"],
        manifest.archive.sha256
    );
    assert_eq!(
        fs::read(fx.bundle_dir(1).join("index.html")).unwrap(),
        b"<html>ota v-next</html>"
    );
    assert_eq!(
        fs::read(fx.bundle_dir(1).join("assets/app.js")).unwrap(),
        b"console.log('hot')"
    );
    assert!(
        fx.debris().is_empty(),
        "no temp files may leak: {:?}",
        fx.debris()
    );

    // Next cold launch arms and serves it; the ack commits it.
    let hot_update = boot(&fx.root, "1.0.0");
    assert_eq!(
        hot_update.notify_app_ready().unwrap(),
        crate::AckOutcome::Committed(1)
    );
    let current = hot_update.current_bundle().unwrap();
    assert_eq!(current.source, crate::BundleSource::Ota);
    assert_eq!(current.version, ver("1.1.0"));
}

#[tokio::test]
async fn progress_is_reported_per_chunk_up_to_the_exact_total() {
    let fx = Fixture::new();
    let (manifest, _) = fx.publish("1.1.0", "1.0.0");
    let hot_update = boot(&fx.root, "1.0.0");

    let mut seen: Vec<(u64, u64)> = Vec::new();
    hot_update
        .check_and_download(&fx.config(), |downloaded, total| {
            seen.push((downloaded, total))
        })
        .await
        .unwrap();
    assert!(!seen.is_empty());
    assert!(
        seen.windows(2).all(|w| w[0].0 < w[1].0),
        "monotonic: {seen:?}"
    );
    assert!(seen.iter().all(|(_, t)| *t == manifest.archive.size));
    assert_eq!(seen.last().unwrap().0, manifest.archive.size);
}

#[tokio::test]
async fn second_download_of_the_same_release_is_idempotent() {
    let fx = Fixture::new();
    fx.publish("1.1.0", "1.0.0");
    let hot_update = boot(&fx.root, "1.0.0");

    let first = hot_update
        .check_and_download(&fx.config(), no_progress)
        .await
        .unwrap();
    assert!(matches!(first, UpdateOutcome::Staged { seq: 1, .. }));
    let second = hot_update
        .check_and_download(&fx.config(), no_progress)
        .await
        .unwrap();
    assert_eq!(
        second,
        UpdateOutcome::AlreadyStaged {
            seq: 1,
            version: ver("1.1.0")
        }
    );
    assert_eq!(fx.server.request_count("/bundle-1.1.0.tar.gz"), 1);
}

// ------------------------------------------------------------------ gates

#[tokio::test]
async fn old_validly_signed_manifest_is_refused_downgrade_replay() {
    let fx = Fixture::new();
    // A perfectly valid, correctly signed release — but the shell has
    // already seen 2.0.0 (its embedded version). A MITM replaying this
    // manifest must not roll the fleet back.
    fx.publish("1.5.0", "1.0.0");
    let hot_update = boot(&fx.root, "2.0.0");

    let outcome = hot_update
        .check_and_download(&fx.config(), no_progress)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        UpdateOutcome::UpToDate {
            offered: ver("1.5.0"),
            watermark: ver("2.0.0")
        }
    );
    assert_eq!(fx.server.request_count("/bundle-1.5.0.tar.gz"), 0);
    assert_eq!(fx.state()["staged"], serde_json::Value::Null);
}

#[tokio::test]
async fn committed_version_reports_up_to_date_on_the_next_check() {
    let fx = Fixture::new();
    fx.publish("1.1.0", "1.0.0");
    let hot_update = boot(&fx.root, "1.0.0");
    hot_update
        .check_and_download(&fx.config(), no_progress)
        .await
        .unwrap();
    boot(&fx.root, "1.0.0").notify_app_ready().unwrap(); // trial + ack

    let outcome = boot(&fx.root, "1.0.0").check(&fx.config()).await.unwrap();
    assert_eq!(
        outcome,
        UpdateOutcome::UpToDate {
            offered: ver("1.1.0"),
            watermark: ver("1.1.0")
        }
    );
}

#[tokio::test]
async fn blacklisted_archive_hash_is_refused_without_downloading() {
    let fx = Fixture::new();
    fx.publish("1.1.0", "1.0.0");
    let hot_update = boot(&fx.root, "1.0.0");
    hot_update
        .check_and_download(&fx.config(), no_progress)
        .await
        .unwrap();
    boot(&fx.root, "1.0.0"); // trial boot… crash: no ack
    boot(&fx.root, "1.0.0"); // first unacked relaunch: re-armed (strike 1)
    boot(&fx.root, "1.0.0"); // second unacked relaunch: hash blacklisted (strike 2)

    let downloads_before = fx.server.request_count("/bundle-1.1.0.tar.gz");
    let outcome = boot(&fx.root, "1.0.0")
        .check_and_download(&fx.config(), no_progress)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        UpdateOutcome::Blacklisted {
            version: ver("1.1.0")
        }
    );
    assert_eq!(
        fx.server.request_count("/bundle-1.1.0.tar.gz"),
        downloads_before
    );
}

#[tokio::test]
async fn manifest_requiring_a_newer_shell_is_refused() {
    let fx = Fixture::new();
    fx.publish("1.1.0", "2.0.0");
    let hot_update = boot(&fx.root, "1.0.0");

    let outcome = hot_update
        .check_and_download(&fx.config(), no_progress)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        UpdateOutcome::ShellTooOld {
            required: ver("2.0.0"),
            shell: ver("1.0.0")
        }
    );
    assert_eq!(fx.server.request_count("/bundle-1.1.0.tar.gz"), 0);
}

// ----------------------------------------------------------- verification

#[tokio::test]
async fn tampered_manifest_is_rejected_and_nothing_is_downloaded() {
    let fx = Fixture::new();
    fx.publish("1.1.0", "1.0.0");
    // Rewrite the served manifest (version bump) keeping the signature.
    let tampered = fs::read_to_string(fx.tmp.path().join("release-1.1.0/manifest.json"))
        .unwrap()
        .replace("1.1.0", "9.9.9");
    fx.server.set("/manifest.json", tampered.into_bytes());

    let hot_update = boot(&fx.root, "1.0.0");
    let result = hot_update
        .check_and_download(&fx.config(), no_progress)
        .await;
    assert!(
        matches!(result, Err(Error::ManifestSignature(_))),
        "{result:?}"
    );
    assert_eq!(fx.server.request_count("/bundle-1.1.0.tar.gz"), 0);
    assert!(!fx.root.join("state.json").exists() || fx.state()["staged"].is_null());
}

#[tokio::test]
async fn manifest_signed_by_an_untrusted_key_is_rejected() {
    let fx = Fixture::new();
    fx.publish("1.1.0", "1.0.0");
    let hot_update = boot(&fx.root, "1.0.0");
    let attacker = KeyPair::generate_unencrypted_keypair().unwrap();
    let config = UpdateConfig {
        pubkeys: vec![attacker.pk.to_base64()],
        ..fx.config()
    };
    let result = hot_update.check(&config).await;
    assert!(
        matches!(result, Err(Error::ManifestSignature(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn rotated_second_trusted_key_verifies_over_the_wire() {
    let fx = Fixture::new();
    let (manifest, _) = fx.publish("1.1.0", "1.0.0");
    let hot_update = boot(&fx.root, "1.0.0");
    let old_key = KeyPair::generate_unencrypted_keypair().unwrap();
    let config = UpdateConfig {
        pubkeys: vec![old_key.pk.to_base64(), fx.keypair.pk.to_base64()],
        ..fx.config()
    };
    assert_eq!(
        hot_update.check(&config).await.unwrap(),
        UpdateOutcome::Available { manifest }
    );
}

#[tokio::test]
async fn tampered_archive_is_rejected_by_sha256_and_leaves_no_trace() {
    let fx = Fixture::new();
    let (_, archive_path) = fx.publish("1.1.0", "1.0.0");
    let mut bytes = fs::read(fx.tmp.path().join("release-1.1.0/bundle-1.1.0.tar.gz")).unwrap();
    bytes[42] ^= 0xff; // same size, different content
    fx.server.set(&archive_path, bytes);

    let hot_update = boot(&fx.root, "1.0.0");
    let result = hot_update
        .check_and_download(&fx.config(), no_progress)
        .await;
    assert!(
        matches!(result, Err(Error::ArchiveSha256 { .. })),
        "{result:?}"
    );
    assert_eq!(fx.state()["staged"], serde_json::Value::Null);
    assert!(!fx.bundle_dir(1).exists());
    assert!(
        fx.debris().is_empty(),
        "partial download must be cleaned up"
    );
}

#[tokio::test]
async fn short_download_fails_cleanly_and_a_retry_restarts_from_scratch() {
    let fx = Fixture::new();
    let (_, archive_path) = fx.publish("1.1.0", "1.0.0");
    let full = fs::read(fx.tmp.path().join("release-1.1.0/bundle-1.1.0.tar.gz")).unwrap();
    // Server declares the full length but closes halfway through.
    fx.server.set_route(
        &archive_path,
        Route {
            declared_len: Some(full.len() as u64),
            body: full[..full.len() / 2].to_vec(),
        },
    );

    let hot_update = boot(&fx.root, "1.0.0");
    let result = hot_update
        .check_and_download(&fx.config(), no_progress)
        .await;
    // Premature close surfaces as a transport error or a size mismatch,
    // depending on where the stream breaks — both are hard stops.
    assert!(
        matches!(result, Err(Error::Http(_)) | Err(Error::ArchiveSize { .. })),
        "{result:?}"
    );
    assert!(
        fx.debris().is_empty(),
        "no .part file may survive the failure"
    );

    // Restart from scratch succeeds once the server serves the full body.
    fx.server.set(&archive_path, full);
    let outcome = hot_update
        .check_and_download(&fx.config(), no_progress)
        .await
        .unwrap();
    assert!(
        matches!(outcome, UpdateOutcome::Staged { seq: 1, .. }),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn archive_stream_longer_than_the_signed_size_is_aborted() {
    let fx = Fixture::new();
    let (manifest, archive_path) = fx.publish("1.1.0", "1.0.0");
    let mut bytes = fs::read(fx.tmp.path().join("release-1.1.0/bundle-1.1.0.tar.gz")).unwrap();
    bytes.extend_from_slice(&[0u8; 4096]); // padded past the signed size
    fx.server.set(&archive_path, bytes);

    let hot_update = boot(&fx.root, "1.0.0");
    let result = hot_update
        .check_and_download(&fx.config(), no_progress)
        .await;
    match result {
        Err(Error::ArchiveSize { declared, actual }) => {
            assert_eq!(declared, manifest.archive.size);
            assert!(actual > declared);
        }
        other => panic!("expected ArchiveSize, got {other:?}"),
    }
}

// ------------------------------------------------------------- transport

#[tokio::test]
async fn missing_manifest_is_a_typed_http_status_error() {
    let fx = Fixture::new(); // nothing published
    let hot_update = boot(&fx.root, "1.0.0");
    let result = hot_update.check(&fx.config()).await;
    assert!(
        matches!(result, Err(Error::HttpStatus { status: 404, .. })),
        "{result:?}"
    );
}

#[tokio::test]
async fn oversized_manifest_body_is_refused() {
    let fx = Fixture::new();
    fx.server.set(
        "/manifest.json",
        vec![b'x'; (crate::download::MANIFEST_MAX_BYTES + 1) as usize],
    );
    let hot_update = boot(&fx.root, "1.0.0");
    let result = hot_update.check(&fx.config()).await;
    assert!(
        matches!(result, Err(Error::ResponseTooLarge { .. })),
        "{result:?}"
    );
}

#[tokio::test]
async fn update_api_errors_cleanly_before_initialization() {
    let hot_update = HotUpdate {
        shared: Arc::new(Shared::default()),
    };
    let fx_config = UpdateConfig {
        manifest_url: "http://127.0.0.1:1/manifest.json".into(),
        pubkeys: vec![],
    };
    assert!(matches!(
        hot_update.check(&fx_config).await,
        Err(Error::NotActive)
    ));
    assert!(matches!(
        hot_update.check_and_download(&fx_config, no_progress).await,
        Err(Error::NotActive)
    ));
}
