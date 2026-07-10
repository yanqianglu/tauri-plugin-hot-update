//! The `Assets` provider: active-OTA-bundle-dir first, embedded fallback.
//!
//! This is the single call site behind the `tauri://` scheme handler on every
//! platform, so the origin never changes and web storage (auth/session state)
//! is preserved by construction. SPA-route fallback (`/foo` →
//! `/foo/index.html` → `/index.html`) lives in tauri's `get_asset`, which
//! retries `get()` per candidate — the provider answers exact keys only.
//!
//! CSP rule (design doc, settled 2026-07-09): `csp_hashes()` is keyed to the
//! active source *per key*. When a key is served from the OTA bundle the
//! compile-time hashes (computed from the embedded HTML) would be stale, so
//! an empty iterator is returned — leaving a configured `'unsafe-inline'`
//! active, exactly like a plain web deploy. When a key falls back to the
//! embedded assets, the embedded hashes are delegated to. Never stale hashes
//! for fresh content.

use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use tauri::utils::assets::{AssetKey, AssetsIter, CspHash};
use tauri::{App, Assets, Runtime};

use crate::runtime::Shared;

/// Assets provider wrapping the embedded `EmbeddedAssets` taken from the
/// generated `Context` (via the official `Context::set_assets`).
pub struct HotUpdateAssets<R: Runtime> {
    embedded: Box<dyn Assets<R>>,
    shared: Arc<Shared>,
}

impl<R: Runtime> HotUpdateAssets<R> {
    pub(crate) fn new(embedded: Box<dyn Assets<R>>, shared: Arc<Shared>) -> Self {
        Self { embedded, shared }
    }

    /// The on-disk file this key resolves to, iff an OTA bundle is active
    /// and the bundle contains the file.
    fn ota_file(&self, key: &AssetKey) -> Option<PathBuf> {
        let dir = self.shared.active_dir()?;
        let path = safe_join(dir, key.as_ref())?;
        path.is_file().then_some(path)
    }
}

impl<R: Runtime> Assets<R> for HotUpdateAssets<R> {
    fn setup(&self, app: &App<R>) {
        // Tauri calls this after the config windows are created — too late to
        // resolve anything, but a good place to detect a mis-wired app.
        if !self.shared.is_activated() {
            log::error!(
                "hot-update: assets provider installed but the plugin never initialized — \
                 did you forget .plugin(tauri_plugin_hot_update::init(handle))? \
                 Serving embedded assets only"
            );
        }
        self.embedded.setup(app);
    }

    fn get(&self, key: &AssetKey) -> Option<Cow<'_, [u8]>> {
        if let Some(path) = self.ota_file(key) {
            match std::fs::read(&path) {
                Ok(bytes) => {
                    log::debug!("hot-update: {} served from OTA bundle", key.as_ref());
                    return Some(Cow::Owned(bytes));
                }
                Err(e) => {
                    log::warn!(
                        "hot-update: reading {} failed ({e}); falling back to embedded",
                        path.display()
                    );
                }
            }
        }
        self.embedded.get(key)
    }

    fn iter(&self) -> Box<AssetsIter<'_>> {
        // Only used by `AssetResolver::iter` / dev tooling, never by the
        // serving path; delegating to embedded keeps it deterministic.
        self.embedded.iter()
    }

    fn csp_hashes(&self, html_path: &AssetKey) -> Box<dyn Iterator<Item = CspHash<'_>> + '_> {
        if self.ota_file(html_path).is_some() {
            // OTA-served HTML: compile-time hashes would be stale — empty.
            Box::new(std::iter::empty())
        } else {
            // Embedded-served HTML: the compile-time hashes match exactly.
            self.embedded.csp_hashes(html_path)
        }
    }
}

/// Never served: occupies `Context::assets` for the instant between taking
/// the embedded assets out and installing the wrapper (the wrapper must own
/// the original box before it can be constructed).
pub(crate) struct PlaceholderAssets;

impl<R: Runtime> Assets<R> for PlaceholderAssets {
    fn get(&self, _key: &AssetKey) -> Option<Cow<'_, [u8]>> {
        None
    }
    fn iter(&self) -> Box<AssetsIter<'_>> {
        Box::new(std::iter::empty())
    }
    fn csp_hashes(&self, _html_path: &AssetKey) -> Box<dyn Iterator<Item = CspHash<'_>> + '_> {
        Box::new(std::iter::empty())
    }
}

/// Join a normalized asset key onto the bundle dir, refusing anything that
/// could escape it. `AssetKey` is already normalized by tauri (rooted, unix
/// separators), but this is the serving layer — be strict regardless: every
/// component must be a plain name (no `..`, no `.`, no root/prefix
/// components such as Windows drive letters).
fn safe_join(dir: &Path, key: &str) -> Option<PathBuf> {
    let rel = key.trim_start_matches('/');
    if rel.is_empty() {
        return None;
    }
    let mut out = dir.to_path_buf();
    for component in Path::new(rel).components() {
        match component {
            Component::Normal(part) => out.push(part),
            _ => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests;
