//! On-disk lifecycle tests: every "boot" is a fresh [`Shared`] (a new
//! process) against a persistent store root — real files, real state.json.

use std::fs;
use std::path::Path;

use semver::Version;
use tempfile::TempDir;

use super::*;
use crate::machine::{stage, BundleMeta};

fn ver(s: &str) -> Version {
    Version::parse(s).unwrap()
}

/// One cold launch: new process (fresh Shared), boot resolution, runtime API.
fn boot(root: &Path, embedded: &str) -> (Arc<Shared>, HotUpdate) {
    let shared = Arc::new(Shared::default());
    initialize(&shared, root.to_path_buf(), ver(embedded));
    let hot_update = HotUpdate {
        shared: Arc::clone(&shared),
    };
    (shared, hot_update)
}

/// Simulate the WP3 downloader: extract a bundle dir, then stage it.
fn stage_bundle(root: &Path, seq: u64, version: &str, sha: &str) {
    let store = Store::new(root.to_path_buf());
    let dir = store.bundle_dir(seq);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("index.html"), format!("bundle {seq}")).unwrap();
    store
        .update(|state| {
            stage(
                state,
                seq,
                BundleMeta {
                    version: ver(version),
                    archive_sha256: sha.to_string(),
                },
            )
        })
        .unwrap()
        .expect("stage gates should pass");
}

fn raw_state(root: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(root.join("state.json")).unwrap()).unwrap()
}

fn bundle_dir(root: &Path, seq: u64) -> std::path::PathBuf {
    Store::new(root.to_path_buf()).bundle_dir(seq)
}

// ---------------------------------------------------------------- fresh boot

#[test]
fn fresh_install_serves_embedded_and_persists_initial_state() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("hot-update");
    let (shared, hot_update) = boot(&root, "1.0.0");

    assert_eq!(shared.active_dir(), None);
    let current = hot_update.current_bundle().unwrap();
    assert_eq!(current.source, BundleSource::Embedded);
    assert_eq!(current.seq, None);
    assert_eq!(current.version, ver("1.0.0"));
    assert_eq!(raw_state(&root)["maxVersionSeen"], "1.0.0");
    assert_eq!(
        hot_update.notify_app_ready().unwrap(),
        AckOutcome::EmbeddedNoop
    );
}

// ------------------------------------------------- staged → booting → commit

#[test]
fn full_trial_cycle_arms_persists_then_commits() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("hot-update");
    boot(&root, "1.0.0");
    stage_bundle(&root, 1, "1.1.0", "sha-1");

    // Cold launch: the arm must already be on disk when initialize returns —
    // i.e. before the provider could serve a single byte.
    let (shared, hot_update) = boot(&root, "1.0.0");
    assert_eq!(shared.active_dir(), Some(&bundle_dir(&root, 1)));
    let raw = raw_state(&root);
    assert_eq!(raw["booting"], 1, "arm persisted before serving");
    assert_eq!(raw["staged"], serde_json::Value::Null);

    assert_eq!(hot_update.notify_app_ready().unwrap(), AckOutcome::Committed(1));
    let raw = raw_state(&root);
    assert_eq!(raw["committed"], 1);
    assert_eq!(raw["lastGood"], 1);
    assert_eq!(raw["booting"], serde_json::Value::Null);
    assert_eq!(raw["maxVersionSeen"], "1.1.0");

    let current = hot_update.current_bundle().unwrap();
    assert_eq!(current.source, BundleSource::Ota);
    assert_eq!(current.seq, Some(1));
    assert_eq!(current.version, ver("1.1.0"));

    // Steady state on the next launch: committed serving, idempotent ack.
    let (shared, hot_update) = boot(&root, "1.0.0");
    assert_eq!(shared.active_dir(), Some(&bundle_dir(&root, 1)));
    assert_eq!(
        hot_update.notify_app_ready().unwrap(),
        AckOutcome::AlreadyCommitted(1)
    );
}

// ------------------------------------------------------------------ rollback

#[test]
fn unacked_trial_rolls_back_blacklists_and_deletes_the_bundle() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("hot-update");
    boot(&root, "1.0.0");
    stage_bundle(&root, 1, "1.1.0", "sha-1");

    let (shared, _) = boot(&root, "1.0.0"); // trial boot, crash: no ack
    assert_eq!(shared.active_dir(), Some(&bundle_dir(&root, 1)));

    let (shared, hot_update) = boot(&root, "1.0.0");
    assert_eq!(shared.active_dir(), None, "rolled back to embedded");
    assert_eq!(raw_state(&root)["failed"][0], "sha-1");
    assert!(!bundle_dir(&root, 1).exists(), "failed bundle GC'd");
    assert_eq!(
        hot_update.current_bundle().unwrap().source,
        BundleSource::Embedded
    );
}

#[test]
fn crash_loop_converges_to_the_committed_bundle_on_disk() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("hot-update");
    boot(&root, "1.0.0");
    stage_bundle(&root, 1, "1.1.0", "sha-1");
    let (_, hot_update) = boot(&root, "1.0.0");
    hot_update.notify_app_ready().unwrap(); // seq 1 is last-good

    for (seq, version, sha) in [(2u64, "1.2.0", "sha-2"), (3, "1.3.0", "sha-3")] {
        stage_bundle(&root, seq, version, sha);
        let (shared, _) = boot(&root, "1.0.0"); // trial boot, no ack
        assert_eq!(shared.active_dir(), Some(&bundle_dir(&root, seq)));
        let (shared, _) = boot(&root, "1.0.0"); // rollback
        assert_eq!(shared.active_dir(), Some(&bundle_dir(&root, 1)));
    }

    let raw = raw_state(&root);
    assert_eq!(raw["committed"], 1);
    assert!(!bundle_dir(&root, 2).exists());
    assert!(!bundle_dir(&root, 3).exists());
    assert!(bundle_dir(&root, 1).exists());
}

// --------------------------------- download finishing during a trial session

#[test]
fn download_landing_mid_trial_stays_staged_while_the_trial_commits() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("hot-update");
    boot(&root, "1.0.0");
    stage_bundle(&root, 1, "1.1.0", "sha-1");

    let (shared, hot_update) = boot(&root, "1.0.0"); // trial boot of seq 1
    assert_eq!(shared.active_dir(), Some(&bundle_dir(&root, 1)));
    stage_bundle(&root, 2, "1.2.0", "sha-2"); // download finishes mid-session

    assert_eq!(hot_update.notify_app_ready().unwrap(), AckOutcome::Committed(1));
    let raw = raw_state(&root);
    assert_eq!(raw["committed"], 1, "the booted seq committed");
    assert_eq!(raw["staged"], 2, "fresh download untouched by the ack");

    // Next launch: seq 2 gets its own trial; committing it replaces seq 1.
    let (shared, hot_update) = boot(&root, "1.0.0");
    assert_eq!(shared.active_dir(), Some(&bundle_dir(&root, 2)));
    assert_eq!(hot_update.notify_app_ready().unwrap(), AckOutcome::Committed(2));
    assert!(!bundle_dir(&root, 1).exists(), "replaced last-good deleted");
}

// ----------------------------------------------------------------------- GC

#[test]
fn orphan_dirs_and_foreign_debris_are_swept_at_boot() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("hot-update");
    // A kill between extraction and the state write: dir exists, no pointer.
    let orphan = bundle_dir(&root, 9);
    fs::create_dir_all(&orphan).unwrap();
    fs::write(orphan.join("index.html"), b"never staged").unwrap();
    let debris_dir = root.join("bundles").join("tmp-extract");
    fs::create_dir_all(&debris_dir).unwrap();
    let debris_file = root.join("bundles").join("stray.tar.gz");
    fs::write(&debris_file, b"partial download").unwrap();

    let (shared, _) = boot(&root, "1.0.0");
    assert_eq!(shared.active_dir(), None, "orphan is never served");
    assert!(!orphan.exists());
    assert!(!debris_dir.exists());
    assert!(!debris_file.exists());
}

// ----------------------------------------------------- corrupt state recovery

#[test]
fn corrupt_state_json_recovers_to_embedded_and_a_fresh_file() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("hot-update");
    boot(&root, "1.0.0");
    stage_bundle(&root, 1, "1.1.0", "sha-1");
    fs::write(root.join("state.json"), b"}{ definitely not json").unwrap();

    let (shared, hot_update) = boot(&root, "1.0.0");
    assert_eq!(shared.active_dir(), None, "fresh state serves embedded");
    assert!(!bundle_dir(&root, 1).exists(), "now-unreferenced bundle GC'd");
    let raw = raw_state(&root); // parses again: file was rewritten valid
    assert_eq!(raw["maxVersionSeen"], "1.0.0");
    assert_eq!(
        hot_update.current_bundle().unwrap().source,
        BundleSource::Embedded
    );
}

// ------------------------------------------------- embedded-newer-than-OTA

#[test]
fn store_update_shipping_newer_assets_discards_the_ota_bundle() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("hot-update");
    boot(&root, "1.0.0");
    stage_bundle(&root, 1, "1.1.0", "sha-1");
    let (_, hot_update) = boot(&root, "1.0.0");
    hot_update.notify_app_ready().unwrap();

    // The user installs a store release whose embedded frontend is 1.2.0.
    let (shared, hot_update) = boot(&root, "1.2.0");
    assert_eq!(shared.active_dir(), None, "store release must not be shadowed");
    assert!(!bundle_dir(&root, 1).exists());
    let current = hot_update.current_bundle().unwrap();
    assert_eq!(current.source, BundleSource::Embedded);
    assert_eq!(current.version, ver("1.2.0"));
    assert_eq!(raw_state(&root)["maxVersionSeen"], "1.2.0");
}

// ------------------------------------------------------- persistence failure

#[cfg(unix)]
#[test]
fn unpersistable_arm_serves_last_good_instead_of_the_staged_bundle() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("hot-update");
    boot(&root, "1.0.0");
    stage_bundle(&root, 1, "1.1.0", "sha-1");
    let (_, hot_update) = boot(&root, "1.0.0");
    hot_update.notify_app_ready().unwrap(); // seq 1 committed
    stage_bundle(&root, 2, "1.2.0", "sha-2");

    // Make state.json unwritable: the arm of seq 2 cannot be persisted.
    fs::set_permissions(&root, fs::Permissions::from_mode(0o555)).unwrap();
    let (shared, _) = boot(&root, "1.0.0");
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();

    // A trial boot without a persisted rollback marker could evade rollback
    // detection — so seq 2 must NOT be served; last-good (seq 1) is.
    assert_eq!(shared.active_dir(), Some(&bundle_dir(&root, 1)));
    let raw = raw_state(&root);
    assert_eq!(raw["staged"], 2, "on-disk state untouched; retried next boot");
    assert_eq!(raw["booting"], serde_json::Value::Null);
}

// --------------------------------------------------------------------- reset

#[test]
fn reset_returns_to_factory_and_spares_the_live_bundle_until_next_boot() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("hot-update");
    boot(&root, "1.0.0");
    stage_bundle(&root, 1, "1.1.0", "sha-1");
    let (_, hot_update) = boot(&root, "1.0.0");
    hot_update.notify_app_ready().unwrap();

    let (shared, hot_update) = boot(&root, "1.0.0");
    assert_eq!(shared.active_dir(), Some(&bundle_dir(&root, 1)));
    hot_update.reset().unwrap();

    let raw = raw_state(&root);
    assert_eq!(raw["committed"], serde_json::Value::Null);
    assert_eq!(raw["maxVersionSeen"], "1.0.0", "watermark restarts from embedded");
    assert!(
        bundle_dir(&root, 1).exists(),
        "live bundle not yanked from under the webview"
    );

    let (shared, _) = boot(&root, "1.0.0");
    assert_eq!(shared.active_dir(), None);
    assert!(!bundle_dir(&root, 1).exists(), "swept on the next boot");
}

// ------------------------------------------------------------- runtime guards

#[test]
fn runtime_api_errors_cleanly_before_initialization() {
    let hot_update = HotUpdate {
        shared: Arc::new(Shared::default()),
    };
    assert!(matches!(hot_update.notify_app_ready(), Err(crate::Error::NotActive)));
    assert!(matches!(hot_update.current_bundle(), Err(crate::Error::NotActive)));
    assert!(matches!(hot_update.reset(), Err(crate::Error::NotActive)));
}

#[test]
fn double_initialization_keeps_the_first_activation() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("hot-update");
    boot(&root, "1.0.0");
    stage_bundle(&root, 1, "1.1.0", "sha-1");

    let shared = Arc::new(Shared::default());
    initialize(&shared, root.clone(), ver("1.0.0"));
    let first_active = shared.active_dir().cloned();
    initialize(&shared, root.clone(), ver("9.9.9"));
    assert_eq!(shared.active_dir().cloned(), first_active);
}

#[test]
fn current_bundle_serializes_camel_case_for_the_wp4_api() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("hot-update");
    boot(&root, "1.0.0");
    stage_bundle(&root, 1, "1.1.0", "sha-1");
    let (_, hot_update) = boot(&root, "1.0.0");

    let json = serde_json::to_value(hot_update.current_bundle().unwrap()).unwrap();
    assert_eq!(
        json,
        serde_json::json!({ "source": "ota", "seq": 1, "version": "1.1.0" })
    );
}
