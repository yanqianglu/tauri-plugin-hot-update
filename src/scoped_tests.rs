use std::fs;
use std::sync::Arc;

use semver::Version;

use crate::machine::AckOutcome;
use crate::runtime::{self, ApplyOutcome, BundleSource, HotUpdate, Shared};
use crate::sign::{sign_release, SignOptions};
use crate::testutil::Fixture;
use crate::update::{UpdateConfig, UpdateOutcome};
use crate::{Error, ReleaseContract};

const NATIVE: &str = "9.0.0";

fn contract() -> ReleaseContract {
    ReleaseContract {
        application_id: "com.example.scoped".into(),
        channel: "stable".into(),
        state_format: "persisted-v1".into(),
        native_revision: "a".repeat(40),
    }
}

fn boot(fixture: &Fixture, native: &str, identity: ReleaseContract) -> HotUpdate {
    let shared = Arc::new(Shared::default());
    runtime::initialize_scoped(
        &shared,
        fixture.root.clone(),
        Version::parse(native).unwrap(),
        Some(identity),
    );
    HotUpdate { shared }
}

fn config(fixture: &Fixture) -> UpdateConfig {
    UpdateConfig {
        contract: Some(contract()),
        ..fixture.config()
    }
}

fn publish(
    fixture: &Fixture,
    version: &str,
    min_shell: &str,
    base: Option<&str>,
    identity: Option<ReleaseContract>,
) -> String {
    let release = sign_release(
        &SignOptions {
            dist_dir: &fixture.tmp.path().join("dist"),
            version: Version::parse(version).unwrap(),
            min_shell_version: Version::parse(min_shell).unwrap(),
            base_version: base.map(|value| Version::parse(value).unwrap()),
            contract: identity,
            base_url: &fixture.server.url(""),
            out_dir: &fixture.tmp.path().join(format!("scoped-release-{version}")),
        },
        &fixture.keypair.sk,
    )
    .unwrap();
    let path = format!("/bundle-{version}.tar.gz");
    fixture
        .server
        .set("/manifest.json", fs::read(release.manifest_path).unwrap());
    fixture.server.set(
        "/manifest.json.minisig",
        fs::read(release.signature_path).unwrap(),
    );
    fixture
        .server
        .set(&path, fs::read(release.archive_path).unwrap());
    path
}

async fn stage(fixture: &Fixture, update: &HotUpdate, version: &str) -> u64 {
    publish(fixture, version, NATIVE, Some(NATIVE), Some(contract()));
    match update
        .check_and_download(&config(fixture), |_, _| {})
        .await
        .unwrap()
    {
        UpdateOutcome::Staged {
            seq,
            version: offered,
        } => {
            assert_eq!(offered, Version::parse(version).unwrap());
            seq
        }
        outcome => panic!("expected staged release, got {outcome:?}"),
    }
}

#[tokio::test]
async fn signed_release_contract_mismatches_stop_before_archive_download() {
    let fixture = Fixture::new();
    let update = boot(&fixture, NATIVE, contract());
    let mut identities = vec![None];
    for field in ["application", "channel", "state", "revision"] {
        let mut identity = contract();
        match field {
            "application" => identity.application_id = "com.other.app".into(),
            "channel" => identity.channel = "preview".into(),
            "state" => identity.state_format = "persisted-v2".into(),
            "revision" => identity.native_revision = "b".repeat(40),
            _ => unreachable!(),
        }
        identities.push(Some(identity));
    }
    for identity in identities {
        let archive = publish(&fixture, "0.1.0", NATIVE, Some(NATIVE), identity);
        assert!(matches!(
            update.check(&config(&fixture)).await,
            Err(Error::ManifestInvalid(_))
        ));
        assert!(matches!(
            update
                .check_and_download(&config(&fixture), |_, _| {})
                .await,
            Err(Error::ManifestInvalid(_))
        ));
        assert_eq!(fixture.server.request_count(&archive), 0);
        assert_eq!(update.staged_version().unwrap(), None);
    }
}

#[tokio::test]
async fn exact_native_version_is_required_in_both_manifest_base_fields() {
    let fixture = Fixture::new();
    let update = boot(&fixture, NATIVE, contract());
    for (base, minimum) in [
        (None, NATIVE),
        (Some("8.0.0"), NATIVE),
        (Some("9.0.1"), NATIVE),
        (Some(NATIVE), "8.0.0"),
        (Some(NATIVE), "9.0.1"),
    ] {
        let archive = publish(&fixture, "0.1.0", minimum, base, Some(contract()));
        assert!(matches!(
            update
                .check_and_download(&config(&fixture), |_, _| {})
                .await,
            Err(Error::ManifestInvalid(_))
        ));
        assert_eq!(fixture.server.request_count(&archive), 0);
    }
}

#[tokio::test]
async fn frontend_versions_advance_independently_of_the_native_version() {
    let fixture = Fixture::new();
    let embedded = boot(&fixture, NATIVE, contract());
    let seq = stage(&fixture, &embedded, "0.1.0").await;
    assert_eq!(
        embedded.current_bundle().unwrap().source,
        BundleSource::Embedded
    );
    drop(embedded);

    let trial = boot(&fixture, NATIVE, contract());
    let running = trial.current_bundle().unwrap();
    assert_eq!(running.source, BundleSource::Ota);
    assert_eq!(running.version, Version::parse("0.1.0").unwrap());
    assert_eq!(
        trial.notify_app_ready().unwrap(),
        AckOutcome::Committed(seq)
    );
    drop(trial);

    let confirmed = boot(&fixture, NATIVE, contract());
    assert_eq!(confirmed.current_bundle().unwrap(), running);
    assert_eq!(
        confirmed.notify_app_ready().unwrap(),
        AckOutcome::AlreadyCommitted(seq)
    );
    let old_archive = publish(&fixture, "0.0.9", NATIVE, Some(NATIVE), Some(contract()));
    assert!(matches!(
        confirmed.check_and_download(&config(&fixture), |_, _| {}).await.unwrap(),
        UpdateOutcome::UpToDate { offered, watermark }
            if offered == Version::parse("0.0.9").unwrap()
                && watermark == Version::parse("0.1.0").unwrap()
    ));
    assert_eq!(fixture.server.request_count(&old_archive), 0);
    let next = stage(&fixture, &confirmed, "0.2.0").await;
    assert_ne!(seq, next);
    assert_eq!(confirmed.current_bundle().unwrap(), running);
}

#[tokio::test]
async fn staged_frontend_never_changes_the_process_source_or_receives_its_ready_ack() {
    let fixture = Fixture::new();
    let embedded = boot(&fixture, NATIVE, contract());
    let first = stage(&fixture, &embedded, "0.1.0").await;
    assert_eq!(
        embedded.notify_app_ready().unwrap(),
        AckOutcome::EmbeddedNoop
    );
    assert_eq!(
        embedded.apply_staged().unwrap(),
        ApplyOutcome::NotApplicable
    );
    assert_eq!(
        embedded.staged_version().unwrap(),
        Some(Version::parse("0.1.0").unwrap())
    );
    drop(embedded);

    let trial = boot(&fixture, NATIVE, contract());
    let running = trial.current_bundle().unwrap();
    let second = stage(&fixture, &trial, "0.2.0").await;
    assert_eq!(trial.current_bundle().unwrap(), running);
    assert_eq!(trial.apply_staged().unwrap(), ApplyOutcome::NotApplicable);
    assert_eq!(
        trial.notify_app_ready().unwrap(),
        AckOutcome::Committed(first)
    );
    let state = fixture.state();
    assert_eq!(state["committed"], first);
    assert_eq!(state["staged"], second);
    assert!(state["booting"].is_null());
    assert_eq!(
        trial.staged_version().unwrap(),
        Some(Version::parse("0.2.0").unwrap())
    );
}

#[tokio::test]
async fn two_unacknowledged_trials_restore_the_confirmed_frontend_and_blacklist_bad_bytes() {
    let fixture = Fixture::new();
    let embedded = boot(&fixture, NATIVE, contract());
    let first = stage(&fixture, &embedded, "0.1.0").await;
    drop(embedded);
    let good = boot(&fixture, NATIVE, contract());
    assert_eq!(
        good.notify_app_ready().unwrap(),
        AckOutcome::Committed(first)
    );
    fs::write(
        fixture.tmp.path().join("dist/index.html"),
        b"<html>broken frontend</html>",
    )
    .unwrap();
    let second = stage(&fixture, &good, "0.2.0").await;
    let bad_hash = fixture.state()["versions"][second.to_string()]["archiveSha256"].clone();
    drop(good);

    for _ in 0..2 {
        let trial = boot(&fixture, NATIVE, contract());
        assert_eq!(trial.current_bundle().unwrap().seq, Some(second));
        drop(trial);
    }
    let recovered = boot(&fixture, NATIVE, contract());
    assert_eq!(recovered.current_bundle().unwrap().seq, Some(first));
    assert_eq!(
        recovered.current_bundle().unwrap().version,
        Version::parse("0.1.0").unwrap()
    );
    assert!(fixture.state()["failed"]
        .as_array()
        .unwrap()
        .contains(&bad_hash));
    assert!(matches!(
        recovered.check(&config(&fixture)).await.unwrap(),
        UpdateOutcome::Blacklisted { .. }
    ));
    assert!(!fixture.bundle_dir(second).exists());
}

#[tokio::test]
async fn corrupt_state_is_preserved_and_disables_updates_while_serving_embedded() {
    let fixture = Fixture::new();
    let embedded = boot(&fixture, NATIVE, contract());
    let seq = stage(&fixture, &embedded, "0.1.0").await;
    drop(embedded);
    let corrupt = b"{ interrupted state write";
    fs::write(fixture.root.join("state.json"), corrupt).unwrap();
    let recovered = boot(&fixture, NATIVE, contract());
    assert_eq!(
        recovered.current_bundle().unwrap().source,
        BundleSource::Embedded
    );
    assert!(recovered.state_disabled().unwrap());
    assert_eq!(fs::read(fixture.root.join("state.json")).unwrap(), corrupt);
    assert!(recovered
        .check_and_download(&config(&fixture), |_, _| {})
        .await
        .is_err());
    assert!(fixture.bundle_dir(seq).exists());
    drop(recovered);
    let next = boot(&fixture, NATIVE, contract());
    assert!(next.state_disabled().unwrap());
    assert_eq!(fs::read(fixture.root.join("state.json")).unwrap(), corrupt);
}

#[tokio::test]
async fn missing_state_after_initialization_disables_updates_instead_of_resetting_history() {
    let fixture = Fixture::new();
    let embedded = boot(&fixture, NATIVE, contract());
    stage(&fixture, &embedded, "0.1.0").await;
    drop(embedded);
    fs::remove_file(fixture.root.join("state.json")).unwrap();
    let recovered = boot(&fixture, NATIVE, contract());
    assert!(recovered.state_disabled().unwrap());
    assert_eq!(
        recovered.current_bundle().unwrap().source,
        BundleSource::Embedded
    );
    assert!(!fixture.root.join("state.json").exists());
    assert!(recovered.check(&config(&fixture)).await.is_err());
}

#[test]
fn a_second_process_cannot_share_a_store_but_can_acquire_it_after_exit() {
    if let Some(root) = std::env::var_os("HOT_UPDATE_LOCK_TEST_ROOT") {
        let shared = Arc::new(Shared::default());
        runtime::initialize_scoped(
            &shared,
            root.into(),
            Version::parse(NATIVE).unwrap(),
            Some(contract()),
        );
        let expected = std::env::var("HOT_UPDATE_LOCK_TEST_ACQUIRED").unwrap() == "true";
        assert_eq!(shared.is_activated(), expected);
        return;
    }
    let fixture = Fixture::new();
    let first = boot(&fixture, NATIVE, contract());
    let check_child = |expected: bool| {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "scoped_tests::a_second_process_cannot_share_a_store_but_can_acquire_it_after_exit",
                "--nocapture",
            ])
            .env("HOT_UPDATE_LOCK_TEST_ROOT", &fixture.root)
            .env("HOT_UPDATE_LOCK_TEST_ACQUIRED", expected.to_string())
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "{}{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
    };
    check_child(false);
    drop(first);
    check_child(true);
}

#[tokio::test]
async fn a_changed_native_executable_cannot_boot_staged_trial_or_confirmed_assets() {
    for lifecycle in ["staged", "trial", "confirmed"] {
        let fixture = Fixture::new();
        let update = boot(&fixture, NATIVE, contract());
        stage(&fixture, &update, "0.1.0").await;
        drop(update);
        if lifecycle != "staged" {
            let trial = boot(&fixture, NATIVE, contract());
            if lifecycle == "confirmed" {
                trial.notify_app_ready().unwrap();
            }
            drop(trial);
        }
        let changed = boot(&fixture, "9.0.1", contract());
        assert_eq!(
            changed.current_bundle().unwrap().source,
            BundleSource::Embedded,
            "{lifecycle}"
        );
        assert_eq!(changed.staged_version().unwrap(), None);
        assert!(changed.shared.active_dir().is_none());
    }
}

#[tokio::test]
async fn a_changed_contract_cannot_boot_previously_confirmed_assets() {
    for field in ["application", "channel", "state", "revision"] {
        let fixture = Fixture::new();
        let update = boot(&fixture, NATIVE, contract());
        stage(&fixture, &update, "0.1.0").await;
        drop(update);
        let trial = boot(&fixture, NATIVE, contract());
        trial.notify_app_ready().unwrap();
        drop(trial);
        let mut identity = contract();
        match field {
            "application" => identity.application_id = "com.other.app".into(),
            "channel" => identity.channel = "preview".into(),
            "state" => identity.state_format = "persisted-v2".into(),
            "revision" => identity.native_revision = "b".repeat(40),
            _ => unreachable!(),
        }
        let changed = boot(&fixture, NATIVE, identity);
        assert_eq!(
            changed.current_bundle().unwrap().source,
            BundleSource::Embedded,
            "{field}"
        );
        assert!(changed.shared.active_dir().is_none());
    }
}

#[cfg(unix)]
#[test]
fn signing_rejects_symlinks_that_point_outside_the_frontend() {
    let fixture = Fixture::new();
    let secret = fixture.tmp.path().join("outside.txt");
    fs::write(&secret, "not a frontend asset").unwrap();
    std::os::unix::fs::symlink(secret, fixture.tmp.path().join("dist/linked.txt")).unwrap();
    let result = sign_release(
        &SignOptions {
            dist_dir: &fixture.tmp.path().join("dist"),
            version: Version::parse("0.1.0").unwrap(),
            min_shell_version: Version::parse(NATIVE).unwrap(),
            base_version: Some(Version::parse(NATIVE).unwrap()),
            contract: Some(contract()),
            base_url: &fixture.server.url(""),
            out_dir: &fixture.tmp.path().join("rejected-release"),
        },
        &fixture.keypair.sk,
    );
    assert!(result.is_err());
}

#[tokio::test]
async fn callers_cannot_omit_or_replace_the_native_release_identity() {
    let fixture = Fixture::new();
    let update = boot(&fixture, NATIVE, contract());
    let mut changed = contract();
    changed.channel = "preview".into();
    for identity in [None, Some(changed)] {
        let request = UpdateConfig {
            contract: identity,
            ..fixture.config()
        };
        assert!(matches!(
            update.check(&request).await,
            Err(Error::Config(_))
        ));
        assert!(matches!(
            update.check_and_download(&request, |_, _| {}).await,
            Err(Error::Config(_))
        ));
    }
    assert!(fixture.server.requests().is_empty());
}

#[tokio::test]
async fn resetting_a_scoped_runtime_cannot_erase_its_release_history_or_enable_live_apply() {
    let fixture = Fixture::new();
    let update = boot(&fixture, NATIVE, contract());
    stage(&fixture, &update, "0.1.0").await;
    let before = fs::read(fixture.root.join("state.json")).unwrap();
    assert!(matches!(update.reset(), Err(Error::Config(_))));
    assert_eq!(fs::read(fixture.root.join("state.json")).unwrap(), before);
    assert_eq!(update.apply_staged().unwrap(), ApplyOutcome::NotApplicable);
    assert_eq!(
        update.current_bundle().unwrap().source,
        BundleSource::Embedded
    );
    assert_eq!(
        update.staged_version().unwrap(),
        Some(Version::parse("0.1.0").unwrap())
    );
}
