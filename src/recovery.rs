//! Boot recovery: an update path that does not need the page.
//!
//! The normal path starts in the frontend (`download()` after the app
//! mounts). A bundle that crashes before that point can never fetch its own
//! fix, and the rollback pointer cannot help when the crashing bundle is the
//! one compiled into the app. Boot recovery closes that gap from the native
//! side: if the page has not acked (`notifyAppReady()`) within the configured
//! time, the plugin runs the same check/download pipeline itself, switches to
//! the staged bundle and asks the app to reload the webview.
//!
//! It tries once per process. The switched-to bundle goes through the
//! ordinary `staged → booting → committed` path, so a bad fix is rolled back
//! like any other bundle. A page that acks while the download is still
//! running is left alone: the bundle stays staged for its own trial boot.

use std::time::Duration;

use tauri::{AppHandle, Manager, Runtime};

use crate::runtime::{ApplyOutcome, HotUpdate};
use crate::update::{UpdateConfig, UpdateOutcome};

/// What one recovery pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Recovery {
    /// The page acked: nothing to recover.
    PageIsFine,
    /// A newer bundle is now served; the webview must reload.
    Switched,
    /// A bundle is staged but was not switched to (the page acked meanwhile,
    /// or the running bundle is still on trial). It boots next launch.
    StagedOnly,
    /// No applicable update. The page stays as it is.
    NoUpdate,
    /// The check or switch failed (offline, bad signature, ...). Logged.
    Failed(String),
}

/// One recovery pass, without any waiting or reloading.
pub(crate) async fn recover_once(hot_update: &HotUpdate, config: &UpdateConfig) -> Recovery {
    if hot_update.shared.page_acked() {
        return Recovery::PageIsFine;
    }
    let outcome = match hot_update.check_and_download(config, |_, _| {}).await {
        Ok(outcome) => outcome,
        Err(e) => return Recovery::Failed(e.to_string()),
    };
    if !matches!(
        outcome,
        UpdateOutcome::Staged { .. } | UpdateOutcome::AlreadyStaged { .. }
    ) {
        return Recovery::NoUpdate;
    }
    // The download can take a while; a page that came up meanwhile is not
    // reloaded out from under the reader.
    if hot_update.shared.page_acked() {
        return Recovery::StagedOnly;
    }
    match hot_update.apply_staged() {
        Ok(ApplyOutcome::Applied { .. }) => Recovery::Switched,
        Ok(ApplyOutcome::TrialPending) => Recovery::StagedOnly,
        Ok(ApplyOutcome::NothingStaged | ApplyOutcome::NotApplicable) => Recovery::NoUpdate,
        Err(e) => Recovery::Failed(e.to_string()),
    }
}

/// Wait `after`, then run one recovery pass and reload the webviews if it
/// switched bundles. Spawned once by the plugin setup hook.
pub(crate) async fn run<R: Runtime>(
    app: AppHandle<R>,
    hot_update: HotUpdate,
    config: UpdateConfig,
    after: Duration,
) {
    tokio::time::sleep(after).await;
    match recover_once(&hot_update, &config).await {
        Recovery::PageIsFine => {}
        Recovery::Switched => {
            log::warn!("hot-update: page never acked; switched to a newer bundle, reloading");
            for (label, window) in app.webview_windows() {
                if let Err(e) = window.eval("window.location.reload()") {
                    log::warn!("hot-update: reload of window {label} failed: {e}");
                }
            }
        }
        Recovery::StagedOnly => {
            log::info!("hot-update: boot recovery staged a bundle; it boots next launch")
        }
        Recovery::NoUpdate => log::info!("hot-update: boot recovery found no applicable update"),
        Recovery::Failed(e) => log::warn!("hot-update: boot recovery failed: {e}"),
    }
}

#[cfg(test)]
mod tests;
