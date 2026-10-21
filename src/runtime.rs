//! Boot-time wiring between the pure state machine and the I/O shell, plus
//! the in-process runtime API ([`HotUpdate`]).
//!
//! Ordering guarantee (the anti-brick invariant): [`initialize`] runs inside
//! the plugin's setup hook, which tauri executes during `Builder::build`
//! (tauri 2.10.2 `app.rs:2289`; re-verified on 2.11.5 `app.rs:2440`) —
//! strictly before any window or webview is created, and therefore before
//! the assets provider serves a single byte. The staged→booting promotion
//! is persisted inside [`initialize`], so by the time serving starts the rollback marker is
//! already on disk. If persisting fails, the newly armed bundle is *not*
//! served (a trial boot without a persisted marker could evade rollback).

use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

use semver::Version;
use serde::Serialize;

use crate::machine::{self, AckOutcome, Active, ApplyDecision};
use crate::store::Store;
use crate::{Error, Result};

/// Cell shared between the assets provider (created at `install()` time,
/// before the app exists) and the plugin (which resolves paths and runs boot
/// resolution once the app is being built). Written exactly once.
#[derive(Default)]
pub(crate) struct Shared {
    activation: OnceLock<Activation>,
}

/// What the provider serves right now. Set at boot resolution and replaced
/// only by [`HotUpdate::apply_staged`] (the in-process swap that spares the
/// user a restart); a download finishing later still only *stages*.
#[derive(Clone)]
struct Current {
    active: Active,
    /// Bundle dir for [`Active::Ota`], resolved once per swap.
    active_dir: Option<PathBuf>,
    /// Version of the active bundle (OTA) or of the embedded assets.
    active_version: Version,
}

/// Everything fixed for the lifetime of the process, plus the swappable
/// [`Current`] source.
pub(crate) struct Activation {
    pub(crate) contract: Option<crate::ReleaseContract>,
    _process_lock: Option<std::fs::File>,
    store: Store,
    current: RwLock<Current>,
    embedded_version: Version,
    /// Serializes update pipeline runs ([`crate::update`]): concurrent
    /// check/download calls must not race seq allocation or double-download.
    update_lock: tokio::sync::Mutex<()>,
}

impl Activation {
    pub(crate) fn store(&self) -> &Store {
        &self.store
    }

    /// The shell version the compatibility gate checks `minShellVersion`
    /// against. `version:bump` keeps the embedded frontend in lockstep with
    /// the app version, so this is also the embedded bundle version.
    pub(crate) fn embedded_version(&self) -> &Version {
        &self.embedded_version
    }

    pub(crate) fn update_lock(&self) -> &tokio::sync::Mutex<()> {
        &self.update_lock
    }

    fn current(&self) -> Current {
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Shared {
    /// Directory the provider should serve from, or `None` for embedded.
    /// `None` before activation: the fail-safe default is embedded.
    pub(crate) fn active_dir(&self) -> Option<PathBuf> {
        self.activation.get()?.current().active_dir
    }

    pub(crate) fn is_activated(&self) -> bool {
        self.activation.get().is_some()
    }

    pub(crate) fn activation(&self) -> Result<&Activation> {
        self.activation.get().ok_or(Error::NotActive)
    }
}

/// Run boot resolution against `root` (the `hot-update/` dir) and activate
/// serving. Called from the plugin setup hook; must never panic or fail the
/// app — every failure path degrades to serving embedded assets.
#[cfg(test)]
pub(crate) fn initialize(shared: &Shared, root: PathBuf, embedded_version: Version) {
    initialize_scoped(shared, root, embedded_version, None);
}

pub(crate) fn initialize_scoped(
    shared: &Shared,
    root: PathBuf,
    embedded_version: Version,
    contract: Option<crate::ReleaseContract>,
) {
    let process_lock = if contract.is_some() {
        let acquire = || -> std::io::Result<std::fs::File> {
            std::fs::create_dir_all(&root)?;
            let file = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .open(root.join("process.lock"))?;
            // Tauri starts the replacement before the old process exits.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            loop {
                match file.try_lock() {
                    Ok(()) => break,
                    Err(std::fs::TryLockError::WouldBlock)
                        if std::time::Instant::now() < deadline =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(error) => return Err(std::io::Error::other(error)),
                }
            }
            Ok(file)
        };
        match acquire() {
            Ok(lock) => Some(lock),
            Err(error) => {
                log::error!("hot-update: store cannot be locked: {error}; using embedded assets");
                return;
            }
        }
    } else {
        None
    };
    let store = if contract.is_some() {
        Store::scoped(root)
    } else {
        Store::new(root)
    };
    let mut state = store.load_state();
    let blocked = state.ota_disabled;
    if let Some(expected) = &contract {
        state.contract = Some(expected.clone());
        let incompatible: std::collections::BTreeSet<u64> = state
            .versions
            .iter()
            .filter(|(_, meta)| {
                meta.contract.as_ref() != Some(expected)
                    || meta.base_version.as_ref()
                        != Some(&crate::BaseVersion::Valid(embedded_version.clone()))
            })
            .map(|(seq, _)| *seq)
            .collect();
        for pointer in [
            &mut state.booting,
            &mut state.committed,
            &mut state.last_good,
            &mut state.staged,
        ] {
            if pointer.is_some_and(|seq| incompatible.contains(&seq)) {
                *pointer = None;
            }
        }
    }
    let present = store.present_seqs();
    let outcome = machine::resolve_boot(state, &embedded_version, &present);

    if let Some(refusal) = &outcome.base_refused {
        log::info!("hot-update: {refusal}");
    }

    if let Some(seq) = outcome.rolled_back {
        log::warn!(
            "hot-update: bundle seq {seq} was armed last boot but never acked; \
             rolled back and blacklisted its archive hash"
        );
    }

    // Persist BEFORE serving. On failure, fall back to what the previous
    // persisted state already supports: the committed bundle survived boot
    // validation and its pointer is already durable, but a *newly armed*
    // bundle must not run without its rollback marker on disk.
    let active = if blocked {
        Active::Embedded
    } else {
        match store.save_state(&outcome.state) {
            Ok(()) => {
                store.apply_effects(&outcome.effects);
                store.sweep_foreign_entries();
                outcome.active
            }
            Err(e) => {
                log::error!(
                    "hot-update: failed to persist state.json ({e}); \
                 serving committed/embedded without arming the staged bundle"
                );
                match outcome.state.committed {
                    Some(seq) => Active::Ota(seq),
                    None => Active::Embedded,
                }
            }
        }
    };

    let (active_dir, active_version) = match active {
        Active::Ota(seq) => {
            // resolve_boot only arms/keeps seqs it has metadata for.
            let version = outcome
                .state
                .versions
                .get(&seq)
                .map(|meta| meta.version.clone())
                .unwrap_or_else(|| embedded_version.clone());
            (Some(store.bundle_dir(seq)), version)
        }
        Active::Embedded => (None, embedded_version.clone()),
    };

    match active {
        Active::Ota(seq) => log::info!(
            "hot-update: serving OTA bundle seq {seq} (v{active_version}) from {}",
            store.bundle_dir(seq).display()
        ),
        Active::Embedded => {
            log::info!("hot-update: serving embedded assets (v{embedded_version})")
        }
    }

    let activation = Activation {
        contract,
        _process_lock: process_lock,
        store,
        current: RwLock::new(Current {
            active,
            active_dir,
            active_version,
        }),
        embedded_version,
        update_lock: tokio::sync::Mutex::new(()),
    };
    if shared.activation.set(activation).is_err() {
        log::warn!("hot-update: initialize called twice; keeping the first activation");
    }
}

/// Where the currently served frontend comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BundleSource {
    Embedded,
    Ota,
}

/// Snapshot of what this process is serving.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentBundle {
    pub source: BundleSource,
    /// Bundle seq when `source == Ota`.
    pub seq: Option<u64>,
    pub version: Version,
}

/// Result of [`HotUpdate::apply_staged`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ApplyOutcome {
    /// The staged bundle is now what this process serves; reload the webview.
    Applied { seq: u64, version: Version },
    /// No update is waiting.
    NothingStaged,
    /// The bundle this process booted has not acked yet; try again after
    /// `notifyAppReady()`.
    TrialPending,
    /// The staged bundle could no longer be armed (blacklisted, superseded by
    /// the embedded frontend, refused by the base gate, or missing on disk)
    /// and was discarded.
    NotApplicable,
}

/// Runtime API, managed in the app state by [`crate::init`]. IPC commands
/// (WP4) will be thin wrappers over these methods.
pub struct HotUpdate {
    pub(crate) shared: Arc<Shared>,
}

impl HotUpdate {
    pub fn staged_version(&self) -> Result<Option<Version>> {
        let activation = self.shared.activation()?;
        let state = activation.store.load_state();
        Ok(state
            .staged
            .and_then(|seq| state.versions.get(&seq).map(|meta| meta.version.clone())))
    }

    pub fn state_disabled(&self) -> Result<bool> {
        Ok(self.shared.activation()?.store.load_state().ota_disabled)
    }
    /// Commit the bundle this process booted (`notifyAppReady`).
    ///
    /// Commits the in-memory booted seq captured at boot resolution — never
    /// a value re-read from disk — so a download that finished mid-session
    /// stays staged for its own trial boot. Idempotent.
    pub fn notify_app_ready(&self) -> Result<AckOutcome> {
        let activation = self.shared.activation()?;
        let booted = activation.current().active;
        let (outcome, effects) = activation.store.update(|state| {
            let (state, outcome, effects) = machine::ack(state, booted);
            (state, (outcome, effects))
        })?;
        activation.store.apply_effects(&effects);
        match outcome {
            AckOutcome::Committed(seq) => {
                log::info!("hot-update: bundle seq {seq} committed as last-good")
            }
            AckOutcome::Stale(seq) => {
                log::warn!("hot-update: ack for seq {seq} no longer matches on-disk state; ignored")
            }
            AckOutcome::AlreadyCommitted(_) | AckOutcome::EmbeddedNoop => {}
        }
        Ok(outcome)
    }

    /// What is being served right now.
    pub fn current_bundle(&self) -> Result<CurrentBundle> {
        let activation = self.shared.activation()?;
        let current = activation.current();
        Ok(match current.active {
            Active::Ota(seq) => CurrentBundle {
                source: BundleSource::Ota,
                seq: Some(seq),
                version: current.active_version,
            },
            Active::Embedded => CurrentBundle {
                source: BundleSource::Embedded,
                seq: None,
                version: activation.embedded_version.clone(),
            },
        })
    }

    /// Swap the served frontend to the staged bundle *now*, in this process,
    /// without a cold launch. The caller reloads the webview right after, so
    /// the next page load is served from the new bundle.
    ///
    /// The staged→booting promotion is persisted BEFORE the provider is
    /// switched (the same anti-brick ordering as boot arming), so a crash or
    /// kill after the swap but before the new page's `notifyAppReady` falls
    /// into the ordinary two-strike rollback path. Refuses while the current
    /// bundle has not been acked yet ([`ApplyOutcome::TrialPending`]): two
    /// unproven bundles must never stack.
    pub fn apply_staged(&self) -> Result<ApplyOutcome> {
        let activation = self.shared.activation()?;
        if activation.contract.is_some() {
            return Ok(ApplyOutcome::NotApplicable);
        }
        let present = activation.store.present_seqs();
        let (decision, effects) = activation.store.update(|state| {
            let (state, decision, effects) =
                machine::promote_staged(state, &activation.embedded_version, &present);
            (state, (decision, effects))
        })?;
        activation.store.apply_effects(&effects);
        Ok(match decision {
            ApplyDecision::Applied(seq) => {
                let version = activation
                    .store
                    .load_state()
                    .versions
                    .get(&seq)
                    .map(|meta| meta.version.clone())
                    .unwrap_or_else(|| activation.embedded_version.clone());
                let mut current = activation
                    .current
                    .write()
                    .unwrap_or_else(|e| e.into_inner());
                *current = Current {
                    active: Active::Ota(seq),
                    active_dir: Some(activation.store.bundle_dir(seq)),
                    active_version: version.clone(),
                };
                log::info!("hot-update: applied staged bundle seq {seq} (v{version}) in-process");
                ApplyOutcome::Applied { seq, version }
            }
            ApplyDecision::NothingStaged => ApplyOutcome::NothingStaged,
            ApplyDecision::TrialPending => ApplyOutcome::TrialPending,
            ApplyDecision::Discarded => ApplyOutcome::NotApplicable,
            ApplyDecision::BaseRefused(refusal) => {
                log::info!("hot-update: {refusal}");
                ApplyOutcome::NotApplicable
            }
        })
    }

    /// Debug/support escape hatch: wipe all OTA state and bundles, restart
    /// the watermark from the embedded version. The currently served bundle
    /// dir (if any) is left in place until the next boot's GC so the running
    /// webview is not torn; serving reverts to embedded on the next launch.
    pub fn reset(&self) -> Result<()> {
        let activation = self.shared.activation()?;
        if activation.contract.is_some() {
            return Err(Error::Config(
                "scoped OTA state requires offline repair".into(),
            ));
        }
        let active = activation.current().active;
        let effects = activation
            .store
            .update(|state| machine::reset(state, &activation.embedded_version, active))?;
        activation.store.apply_effects(&effects);
        activation.store.sweep_foreign_entries();
        log::info!("hot-update: state reset; embedded serving resumes next launch");
        Ok(())
    }
}

#[cfg(test)]
mod tests;
