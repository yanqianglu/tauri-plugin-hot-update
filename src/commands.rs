//! The IPC command surface — thin wrappers over [`HotUpdate`] whose JSON
//! shapes are the cross-language contract with the TypeScript package
//! (`guest-js/`, npm `tauri-plugin-hot-update-api`). Every serialized shape
//! here is pinned by a golden test in `commands/tests.rs`; a Rust-side
//! rename must fail a test before it can break the TS types.
//!
//! Disabled-by-config semantics (`plugins.hot-update.enabled: false`):
//! commands that *report or ack* state stay total — `current_bundle` answers
//! truthfully (embedded, app version), `notify_app_ready` and `reset` are
//! safe no-ops, so app boot code needs no dark-ship special-casing. Commands
//! that *perform updates* (`check`, `download`) refuse with
//! [`Error::Disabled`].

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Runtime, State};

use crate::machine::AckOutcome;
use crate::runtime::{BundleSource, CurrentBundle, HotUpdate};
use crate::update::{UpdateConfig, UpdateOutcome};
use crate::{Error, Result};

/// Event emitted while [`download`] streams the archive, carrying a
/// [`DownloadProgress`] payload. Emission is throttled to at most one event
/// per 100 ms so the IPC bridge is never flooded by per-chunk callbacks; the
/// final chunk (`downloaded == total`) is always emitted.
pub const PROGRESS_EVENT: &str = "hot-update://progress";

const PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(100);

/// Payload of [`PROGRESS_EVENT`]: archive bytes received so far out of the
/// signed manifest's total.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadProgress {
    pub downloaded: u64,
    pub total: u64,
}

/// The validated update source, managed alongside [`HotUpdate`] by the
/// plugin setup hook. `None` means disabled by config.
pub(crate) struct CommandConfig {
    pub(crate) update: Option<UpdateConfig>,
}

impl CommandConfig {
    fn update_config(&self) -> Result<&UpdateConfig> {
        self.update.as_ref().ok_or(Error::Disabled)
    }

    fn is_disabled(&self) -> bool {
        self.update.is_none()
    }
}

/// Wire shape of `notify_app_ready`, mirroring [`AckOutcome`]. A separate
/// type because the machine's enum carries bare tuple payloads, which
/// internally-tagged serde cannot represent — and because wire shapes belong
/// to the IPC layer, not the pure state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub(crate) enum AckResult {
    Committed { seq: u64 },
    AlreadyCommitted { seq: u64 },
    EmbeddedNoop,
    Stale { seq: u64 },
}

impl From<AckOutcome> for AckResult {
    fn from(outcome: AckOutcome) -> Self {
        match outcome {
            AckOutcome::Committed(seq) => Self::Committed { seq },
            AckOutcome::AlreadyCommitted(seq) => Self::AlreadyCommitted { seq },
            AckOutcome::EmbeddedNoop => Self::EmbeddedNoop,
            AckOutcome::Stale(seq) => Self::Stale { seq },
        }
    }
}

/// Rate limiter for progress emission: the first callback and anything at
/// least `min_interval` after the previous emission pass; the final chunk
/// always passes so the UI is guaranteed to see 100%.
pub(crate) struct ProgressThrottle {
    min_interval: Duration,
    last_emit: Option<Instant>,
}

impl ProgressThrottle {
    pub(crate) fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            last_emit: None,
        }
    }

    pub(crate) fn should_emit(&mut self, downloaded: u64, total: u64) -> bool {
        let is_final = downloaded >= total;
        let due = self
            .last_emit
            .map_or(true, |at| at.elapsed() >= self.min_interval);
        if is_final || due {
            self.last_emit = Some(Instant::now());
            return true;
        }
        false
    }
}

/// Fetch and verify the manifest, then report whether an update applies.
/// Never downloads the archive. Gate refusals (`upToDate`, `blacklisted`,
/// `shellTooOld`, `alreadyStaged`) are `Ok` outcomes; errors are transport,
/// verification, or config failures.
#[tauri::command]
pub(crate) async fn check(
    hot_update: State<'_, HotUpdate>,
    config: State<'_, CommandConfig>,
) -> Result<UpdateOutcome> {
    hot_update.check(config.update_config()?).await
}

/// The full pipeline: check, and if an update applies — download, verify,
/// extract, and stage it for the next cold launch, emitting throttled
/// [`PROGRESS_EVENT`]s along the way. Concurrent calls are serialized; the
/// loser reports `alreadyStaged`/`upToDate` instead of downloading twice.
#[tauri::command]
pub(crate) async fn download<R: Runtime>(
    app: AppHandle<R>,
    hot_update: State<'_, HotUpdate>,
    config: State<'_, CommandConfig>,
) -> Result<UpdateOutcome> {
    let mut throttle = ProgressThrottle::new(PROGRESS_MIN_INTERVAL);
    hot_update
        .check_and_download(config.update_config()?, |downloaded, total| {
            if throttle.should_emit(downloaded, total) {
                if let Err(e) = app.emit(PROGRESS_EVENT, DownloadProgress { downloaded, total }) {
                    log::warn!("hot-update: failed to emit {PROGRESS_EVENT}: {e}");
                }
            }
        })
        .await
}

/// Commit the bundle this process booted (`notifyAppReady`). Idempotent, and
/// deliberately safe to call unconditionally on every launch — including
/// when serving embedded assets or when the plugin is disabled.
#[tauri::command]
pub(crate) fn notify_app_ready(
    hot_update: State<'_, HotUpdate>,
    config: State<'_, CommandConfig>,
) -> Result<AckResult> {
    if config.is_disabled() {
        return Ok(AckResult::EmbeddedNoop);
    }
    Ok(hot_update.notify_app_ready()?.into())
}

/// What is being served right now. When the plugin is disabled this is, by
/// definition, the embedded bundle at the app's own version.
#[tauri::command]
pub(crate) fn current_bundle<R: Runtime>(
    app: AppHandle<R>,
    hot_update: State<'_, HotUpdate>,
    config: State<'_, CommandConfig>,
) -> Result<CurrentBundle> {
    if config.is_disabled() {
        return Ok(CurrentBundle {
            source: BundleSource::Embedded,
            seq: None,
            version: app.package_info().version.clone(),
        });
    }
    hot_update.current_bundle()
}

/// Debug/support escape hatch: wipe all OTA state and bundles and revert to
/// embedded serving on the next launch. A no-op while disabled (a disabled
/// plugin owns no live state; leftovers from a previously enabled build are
/// handled by boot resolution when re-enabled).
#[tauri::command]
pub(crate) fn reset(
    hot_update: State<'_, HotUpdate>,
    config: State<'_, CommandConfig>,
) -> Result<()> {
    if config.is_disabled() {
        return Ok(());
    }
    hot_update.reset()
}

#[cfg(test)]
mod tests;
