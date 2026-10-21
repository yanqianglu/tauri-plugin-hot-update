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
        contract: None,
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
        contract: None,
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
        contract: None,
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

// ------------------------------------------------------------ base gate

#[tokio::test]
async fn bundle_built_from_an_older_base_than_the_shell_is_refused() {
    let fx = Fixture::new();
    fx.publish_with_base("1.1.6-ota.1", "1.1.0", Some("1.1.4"));
    let outcome = boot(&fx.root, "1.1.5")
        .check_and_download(&fx.config(), no_progress)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        UpdateOutcome::ShellNewerThanBase {
            base: ver("1.1.4"),
            shell: ver("1.1.5")
        }
    );
    assert_eq!(fx.server.request_count("/bundle-1.1.6-ota.1.tar.gz"), 0);
}

#[tokio::test]
async fn bundle_built_from_the_same_base_as_the_shell_is_staged() {
    let fx = Fixture::new();
    fx.publish_with_base("1.1.6-ota.1", "1.1.0", Some("1.1.5"));
    let outcome = boot(&fx.root, "1.1.5")
        .check_and_download(&fx.config(), no_progress)
        .await
        .unwrap();
    assert!(
        matches!(outcome, UpdateOutcome::Staged { .. }),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn manifest_without_base_version_skips_the_base_gate() {
    let fx = Fixture::new();
    fx.publish_with_base("1.1.6-ota.1", "1.1.0", None);
    let outcome = boot(&fx.root, "1.1.5").check(&fx.config()).await.unwrap();
    assert!(
        matches!(outcome, UpdateOutcome::Available { .. }),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn base_version_newer_than_an_older_shell_is_accepted() {
    // An older installed shell (1.1.5 < base 1.1.6) takes the bundle: it is
    // newer code than the shell's embedded frontend, which is the point of OTA.
    let fx = Fixture::new();
    fx.publish_with_base("1.1.7-ota.1", "1.1.0", Some("1.1.6"));
    let outcome = boot(&fx.root, "1.1.5")
        .check_and_download(&fx.config(), no_progress)
        .await
        .unwrap();
    assert!(
        matches!(outcome, UpdateOutcome::Staged { .. }),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn base_version_prerelease_orders_below_its_release() {
    // Plain semver precedence: 1.1.23-beta.1 < 1.1.23. A bundle built from a
    // prerelease base is refused by the 1.1.23 release shell (the release is
    // newer), and accepted by the shell of the same prerelease.
    let fx = Fixture::new();
    fx.publish_with_base("1.1.24-ota.1", "1.1.0", Some("1.1.23-beta.1"));
    let outcome = boot(&fx.root, "1.1.23").check(&fx.config()).await.unwrap();
    assert_eq!(
        outcome,
        UpdateOutcome::ShellNewerThanBase {
            base: ver("1.1.23-beta.1"),
            shell: ver("1.1.23")
        }
    );

    let fx = Fixture::new();
    fx.publish_with_base("1.1.24-ota.1", "1.1.0", Some("1.1.23-beta.1"));
    let outcome = boot(&fx.root, "1.1.23-beta.1")
        .check(&fx.config())
        .await
        .unwrap();
    assert!(
        matches!(outcome, UpdateOutcome::Available { .. }),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn base_version_refusal_changes_no_state() {
    let fx = Fixture::new();
    fx.publish_with_base("1.1.6-ota.1", "1.1.0", Some("1.1.4"));
    let hot_update = boot(&fx.root, "1.1.5");
    let state_file = fx.root.join("state.json");
    let read_state = || fs::read(&state_file).ok();
    let list_bundles = || {
        let mut names: Vec<_> = fs::read_dir(fx.root.join("bundles"))
            .map(|dir| dir.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        names.sort();
        names
    };
    let state_before = read_state();
    let bundles_before = list_bundles();

    for _ in 0..2 {
        let outcome = hot_update
            .check_and_download(&fx.config(), no_progress)
            .await
            .unwrap();
        assert!(
            matches!(outcome, UpdateOutcome::ShellNewerThanBase { .. }),
            "{outcome:?}"
        );
    }

    // No watermark bump, no stage, no failed/last-good change: state.json is
    // byte-identical. No download and nothing new under bundles/.
    assert_eq!(read_state(), state_before);
    assert_eq!(list_bundles(), bundles_before);
    assert_eq!(fx.server.request_count("/bundle-1.1.6-ota.1.tar.gz"), 0);
    if state_before.is_some() {
        let state = fx.state();
        assert!(state["staged"].is_null());
        assert_eq!(state["maxVersionSeen"], "1.1.5");
    }
}

#[tokio::test]
async fn unparseable_base_version_refuses_only_that_bundle() {
    let fx = Fixture::new();
    fx.publish_with_raw_base("1.1.6-ota.1", "1.1.0", serde_json::json!("1.x"));
    let hot_update = boot(&fx.root, "1.1.5");
    let state_before = fs::read(fx.root.join("state.json")).ok();

    let expected = UpdateOutcome::InvalidBaseVersion {
        base: "1.x".into(),
        shell: ver("1.1.5"),
    };
    assert_eq!(hot_update.check(&fx.config()).await.unwrap(), expected);
    assert_eq!(
        hot_update
            .check_and_download(&fx.config(), no_progress)
            .await
            .unwrap(),
        expected
    );
    // A refusal, not a failure: no download, no state change.
    assert_eq!(fx.server.request_count("/bundle-1.1.6-ota.1.tar.gz"), 0);
    assert_eq!(fs::read(fx.root.join("state.json")).ok(), state_before);

    // The channel is not broken: the next release with a good base stages.
    fx.publish_with_base("1.1.6-ota.2", "1.1.0", Some("1.1.5"));
    let outcome = hot_update
        .check_and_download(&fx.config(), no_progress)
        .await
        .unwrap();
    assert!(
        matches!(outcome, UpdateOutcome::Staged { .. }),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn non_string_base_version_is_refused_as_invalid() {
    let fx = Fixture::new();
    fx.publish_with_raw_base("1.1.6-ota.1", "1.1.0", serde_json::json!(115));
    let outcome = boot(&fx.root, "1.1.5").check(&fx.config()).await.unwrap();
    assert_eq!(
        outcome,
        UpdateOutcome::InvalidBaseVersion {
            base: "115".into(),
            shell: ver("1.1.5"),
        }
    );
}

#[tokio::test]
async fn downloaded_bundle_is_rechecked_against_a_newer_shell_at_launch() {
    let fx = Fixture::new();
    fx.publish_with_base("1.1.7-ota.1", "1.1.0", Some("1.1.5"));
    let outcome = boot(&fx.root, "1.1.5")
        .check_and_download(&fx.config(), no_progress)
        .await
        .unwrap();
    let UpdateOutcome::Staged { seq, .. } = outcome else {
        panic!("{outcome:?}");
    };
    // The base is recorded with the staged bundle.
    assert_eq!(
        fx.state()["versions"][seq.to_string()]["baseVersion"],
        "1.1.5"
    );

    // Store update to 1.1.6 before the next launch: not applied.
    let hot_update = boot(&fx.root, "1.1.6");
    assert_eq!(
        hot_update.current_bundle().unwrap().source,
        runtime::BundleSource::Embedded
    );
    let state = fx.state();
    assert!(state["staged"].is_null());
    assert!(state["booting"].is_null());
    assert_eq!(state["failed"], serde_json::json!([]));
    assert!(!fx.bundle_dir(seq).exists());
}
