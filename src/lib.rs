//! Hot update / OTA live updates for Tauri v2 mobile and desktop apps —
//! CodePush-style, self-hosted.
//!
//! Serves the frontend from a downloaded OTA bundle when one is active,
//! falling back to the embedded assets compiled from `frontendDist`. Updates
//! apply on the next cold launch only, guarded by a three-state rollback
//! pointer (`staged → booting → committed`): a bundle that never acks via
//! [`HotUpdate::notify_app_ready`] is blacklisted by archive hash on the next
//! boot and serving falls back to the last-known-good bundle or the embedded
//! assets. One bad OTA push can never brick an installed app.
//!
//! # Integration (two steps)
//!
//! The assets swap must happen on the `Context` before `Builder::build`
//! consumes it, while path resolution (the app data dir) is only available
//! once the app is being built — hence two steps sharing one handle:
//!
//! ```rust,ignore
//! fn main() {
//!     let mut context = tauri::generate_context!();
//!     // 1. Swap the embedded assets for the hot-update provider.
//!     let hot_update = tauri_plugin_hot_update::install(&mut context);
//!     tauri::Builder::default()
//!         // 2. Register the plugin; its setup hook resolves the bundle
//!         //    store and arms/rolls back BEFORE any webview exists.
//!         .plugin(tauri_plugin_hot_update::init(hot_update))
//!         .run(context)
//!         .expect("error while running tauri application");
//! }
//! ```
//!
//! Register this plugin before other plugins so nothing observes assets
//! earlier than the boot resolution. The frontend must call
//! `notifyAppReady()` (WP4; today [`HotUpdate::notify_app_ready`] from Rust)
//! once the app shell has mounted and rendered — deliberately independent of
//! network reachability or auth, so a backend outage can never condemn a
//! good bundle fleet-wide.
//!
//! # Ordering guarantee
//!
//! Plugin setup hooks run inside `Builder::build` (tauri 2.10.2
//! `app.rs:2289`), strictly before config windows and webviews are created
//! (`app.rs:2373`). The staged→booting promotion is therefore persisted
//! before the provider serves a single byte, on every platform — including
//! Android, where the app data dir cannot be resolved before the app exists.

use std::sync::Arc;

use tauri::plugin::{Builder as PluginBuilder, TauriPlugin};
use tauri::{Manager, Runtime};

mod assets;
mod error;
mod machine;
mod runtime;
mod store;

pub use assets::HotUpdateAssets;
pub use error::{Error, Result};
pub use machine::{AckOutcome, StageError};
pub use runtime::{BundleSource, CurrentBundle, HotUpdate};

use runtime::Shared;

/// Opaque link between [`install`] (which creates the assets provider) and
/// [`init`] (which activates it once paths are resolvable).
pub struct HotUpdateHandle {
    shared: Arc<Shared>,
}

/// Step 1: swap the generated context's embedded assets for the hot-update
/// provider. Must be called before `tauri::Builder::build`/`run` consumes
/// the context. Pass the returned handle to [`init`].
///
/// Until [`init`]'s setup hook runs, the provider serves the embedded assets
/// — the fail-safe floor.
pub fn install<R: Runtime>(context: &mut tauri::Context<R>) -> HotUpdateHandle {
    let shared = Arc::new(Shared::default());
    // Two-step swap through a placeholder: the wrapper must own the original
    // embedded box before it can be constructed.
    let embedded = context.set_assets(Box::new(assets::PlaceholderAssets));
    context.set_assets(Box::new(HotUpdateAssets::new(embedded, Arc::clone(&shared))));
    HotUpdateHandle { shared }
}

/// Step 2: the plugin. Its setup hook (which tauri runs before any webview
/// is created) resolves `{app_data_dir}/hot-update`, loads `state.json`,
/// performs rollback/arming, persists, and activates serving. Failures
/// degrade to serving embedded assets; they never abort the app.
pub fn init<R: Runtime>(handle: HotUpdateHandle) -> TauriPlugin<R> {
    PluginBuilder::<R, ()>::new("hot-update")
        .setup(move |app, _api| {
            let embedded_version = app.package_info().version.clone();
            match app.path().app_data_dir() {
                Ok(base) => {
                    runtime::initialize(&handle.shared, base.join("hot-update"), embedded_version);
                }
                Err(e) => {
                    log::error!(
                        "hot-update: app_data_dir() failed ({e}); serving embedded assets only"
                    );
                }
            }
            app.manage(HotUpdate {
                shared: Arc::clone(&handle.shared),
            });
            Ok(())
        })
        .build()
}

/// Access the [`HotUpdate`] runtime API from any `Manager` (app handle,
/// window, webview).
///
/// Panics if the plugin was not registered via [`init`].
pub trait HotUpdateExt<R: Runtime> {
    fn hot_update(&self) -> &HotUpdate;
}

impl<R: Runtime, T: Manager<R>> HotUpdateExt<R> for T {
    fn hot_update(&self) -> &HotUpdate {
        self.state::<HotUpdate>().inner()
    }
}
