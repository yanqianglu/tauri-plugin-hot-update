use serde::{Serialize, Serializer};

/// Errors surfaced by the hot-update plugin.
///
/// Every verification gate in the update pipeline is a hard stop with its own
/// variant — a failed signature, hash, or size check aborts the update; there
/// is no warn-and-continue path anywhere in this module.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// The plugin was not initialized: either `install()` was not called on
    /// the context, or `.plugin(init(handle))` was not registered, or
    /// initialization failed at boot (in which case the app serves the
    /// embedded bundle — the fail-safe floor).
    #[error("hot-update is not active (plugin not initialized); serving embedded assets")]
    NotActive,

    /// A staging request was refused by the state machine gates.
    #[error("stage refused: {0}")]
    StageRefused(#[from] crate::machine::StageError),

    /// Invalid `plugins.hot-update` configuration. Raised from the plugin's
    /// setup hook, so it aborts app startup — config ships inside the store
    /// binary and must be caught on the developer's first run.
    #[error("hot-update config invalid: {0}")]
    Config(String),

    /// `check`/`download` was invoked while the plugin is disabled by
    /// config (`plugins.hot-update.enabled` is false).
    #[error("hot-update is disabled by config (`plugins.hot-update.enabled` is false)")]
    Disabled,

    /// A configured trusted public key is not valid minisign key material.
    /// This is a hard stop even when other keys in the list would verify:
    /// silently skipping a malformed trust anchor would weaken the key list
    /// without anyone noticing.
    #[error("invalid minisign public key in the trusted key list")]
    InvalidPublicKey,

    /// The manifest signature failed: undecodable `.minisig` data, or no key
    /// in the trusted list verified the manifest bytes.
    #[error("manifest signature rejected: {0}")]
    ManifestSignature(String),

    /// The signed manifest bytes are not valid JSON for the manifest schema.
    #[error("manifest is not valid manifest JSON: {0}")]
    ManifestParse(#[from] serde_json::Error),

    /// The manifest parsed but declares nonsensical values (malformed
    /// sha256, zero or over-cap archive size).
    #[error("manifest invalid: {0}")]
    ManifestInvalid(String),

    /// Transport-level HTTP failure (connect, TLS, read).
    #[error("update request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// The update server answered with a non-success status.
    #[error("update server returned HTTP {status} for {url}")]
    HttpStatus { status: u16, url: String },

    /// A response body exceeded its sanity cap (manifest or signature).
    #[error("response for {url} exceeds the {limit}-byte cap")]
    ResponseTooLarge { url: String, limit: u64 },

    /// The downloaded archive's byte count diverged from the signed
    /// manifest's `archive.size` (short body, or a stream that kept going).
    #[error("archive size mismatch: manifest declares {declared} bytes, got {actual}")]
    ArchiveSize { declared: u64, actual: u64 },

    /// The downloaded archive's sha256 diverged from the signed manifest.
    #[error("archive sha256 mismatch: manifest declares {declared}, got {actual}")]
    ArchiveSha256 { declared: String, actual: String },

    /// Archive extraction was refused (hostile entry) or failed.
    #[error(transparent)]
    Extract(#[from] crate::extract::ExtractError),
}

impl Serialize for Error {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.to_string().as_ref())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
