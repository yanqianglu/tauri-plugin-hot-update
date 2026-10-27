//! Boot recovery against the real pipeline: signed releases served by a local
//! HTTP server into a real on-disk store, as in the update tests.

use std::path::Path;
use std::sync::Arc;

use semver::Version;

use super::*;
use crate::runtime::{self, Shared};
use crate::testutil::Fixture;

fn boot(root: &Path, embedded: &str) -> HotUpdate {
    let shared = Arc::new(Shared::default());
    runtime::initialize(
        &shared,
        root.to_path_buf(),
        Version::parse(embedded).unwrap(),
    );
    HotUpdate { shared }
}

#[tokio::test]
async fn a_page_that_never_acks_gets_the_fix_without_any_page_code() {
    let fx = Fixture::new();
    fx.publish("1.1.1", "1.0.0");
    let hot_update = boot(&fx.root, "1.0.0");

    // The embedded page crashed at start: notifyAppReady was never called.
    let recovery = recover_once(&hot_update, &fx.config()).await;

    assert_eq!(recovery, Recovery::Switched);
    let current = hot_update.current_bundle().unwrap();
    assert_eq!(current.version, Version::parse("1.1.1").unwrap());
    assert!(fx.bundle_dir(1).join("index.html").exists());
}

#[tokio::test]
async fn a_page_that_acked_is_left_alone_and_nothing_is_fetched() {
    let fx = Fixture::new();
    fx.publish("1.1.1", "1.0.0");
    let hot_update = boot(&fx.root, "1.0.0");
    hot_update.notify_app_ready().unwrap();

    let recovery = recover_once(&hot_update, &fx.config()).await;

    assert_eq!(recovery, Recovery::PageIsFine);
    assert_eq!(fx.server.request_count("/manifest.json"), 0);
}

#[tokio::test]
async fn no_newer_bundle_means_no_change() {
    let fx = Fixture::new();
    fx.publish("1.0.0", "1.0.0");
    let hot_update = boot(&fx.root, "1.0.0");

    let recovery = recover_once(&hot_update, &fx.config()).await;

    assert_eq!(recovery, Recovery::NoUpdate);
}

#[tokio::test]
async fn an_unreachable_server_is_a_logged_failure_not_a_panic() {
    let fx = Fixture::new();
    let hot_update = boot(&fx.root, "1.0.0");
    let mut config = fx.config();
    config.manifest_url = "http://127.0.0.1:1/manifest.json".into();

    let recovery = recover_once(&hot_update, &config).await;

    assert!(matches!(recovery, Recovery::Failed(_)), "{recovery:?}");
}

#[tokio::test]
async fn a_bundle_still_on_trial_is_staged_but_not_stacked() {
    let fx = Fixture::new();
    fx.publish("1.1.1", "1.0.0");
    let first = boot(&fx.root, "1.0.0");
    // Stage 1.1.1 and arm it by a cold launch: it is now booting, unacked.
    first
        .check_and_download(&fx.config(), |_, _| {})
        .await
        .unwrap();
    let second = boot(&fx.root, "1.0.0");
    assert_eq!(
        second.current_bundle().unwrap().version,
        Version::parse("1.1.1").unwrap()
    );
    fx.publish("1.1.2", "1.0.0");

    let recovery = recover_once(&second, &fx.config()).await;

    // Two unproven bundles must never stack.
    assert_eq!(recovery, Recovery::StagedOnly);
    assert_eq!(
        second.current_bundle().unwrap().version,
        Version::parse("1.1.1").unwrap()
    );
}
