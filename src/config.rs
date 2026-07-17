//! Plugin configuration: the `plugins.hot-update` object in
//! `tauri.conf.json`, validated at plugin init so a bad config is a startup
//! error, never a runtime surprise. The update source is deliberately
//! config-only — JS cannot pass a manifest URL or keys, so compromised
//! webview content can never redirect updates.
//!
//! ```json
//! {
//!   "plugins": {
//!     "hot-update": {
//!       "manifestUrl": "https://updates.example.com/manifest.json",
//!       "pubkeys": ["RWT..."],
//!       "enabled": true
//!     }
//!   }
//! }
//! ```

use serde::Deserialize;

use crate::update::UpdateConfig;
use crate::{Error, Result};

/// `plugins.hot-update` in `tauri.conf.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    /// URL of `manifest.json`. The detached signature is fetched from
    /// `{manifestUrl}.minisig`, so this must be a plain file URL with no
    /// query string. Required when `enabled` is true.
    #[serde(default)]
    pub manifest_url: Option<String>,
    /// Trusted minisign public keys — raw base64 (`RW…`) or full
    /// `minisign.pub` file contents. A manifest verifying under ANY key is
    /// trusted (ship old + new keys during a rotation). At least one is
    /// required when `enabled` is true.
    #[serde(default)]
    pub pubkeys: Vec<String>,
    /// Dark-ship switch: register the plugin but keep it inert. When false,
    /// no update is ever checked or served — the app runs on its embedded
    /// assets exactly as if the plugin were absent.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Config {
    /// Validate into the pipeline's [`UpdateConfig`], or `None` when
    /// disabled. Every failure here aborts app startup by design: the config
    /// ships inside the store binary, so a malformed trust anchor or URL is
    /// a build/config bug the developer must see on first run.
    ///
    /// A disabled config is not validated at all — dark-shipping with
    /// placeholder values must never brick the app.
    pub(crate) fn validate(&self) -> Result<Option<UpdateConfig>> {
        if !self.enabled {
            return Ok(None);
        }
        let url = self
            .manifest_url
            .as_deref()
            .map(str::trim)
            .unwrap_or_default();
        if url.is_empty() {
            return Err(Error::Config(
                "`manifestUrl` is required when the plugin is enabled".into(),
            ));
        }
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return Err(Error::Config(format!(
                "`manifestUrl` must be an http(s) URL, got {url:?}"
            )));
        }
        if url.contains('?') {
            return Err(Error::Config(
                "`manifestUrl` must be a plain file URL without a query string \
                 (the detached signature is fetched from `<manifestUrl>.minisig`)"
                    .into(),
            ));
        }
        if self.pubkeys.is_empty() {
            return Err(Error::Config(
                "at least one trusted minisign public key is required in `pubkeys`".into(),
            ));
        }
        crate::manifest::validate_pubkeys(&self.pubkeys)?;
        Ok(Some(UpdateConfig {
            manifest_url: url.to_string(),
            pubkeys: self.pubkeys.clone(),
        }))
    }
}

#[cfg(test)]
mod tests;
