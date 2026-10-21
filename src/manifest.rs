//! The signed update manifest: schema, signature verification, validation.
//!
//! Verification order is fixed and security-relevant (design doc, "Manifest
//! and signing"): the minisign signature is verified over the **raw manifest
//! bytes first**, against the embedded trusted-key LIST (any key may verify —
//! that is how key rotation works: a store release adds the new key, both
//! sign during the transition, the old key is dropped later). Only then are
//! the bytes parsed as JSON and sanity-validated. Nothing downstream ever
//! touches unverified data.

use minisign_verify::{PublicKey, Signature};
use semver::Version;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{Error, Result};

/// The update manifest, exactly as published next to its detached
/// `.minisig` signature.
///
/// Unknown fields are deliberately tolerated (no `deny_unknown_fields`):
/// newer manifest schema additions must not break older installed shells —
/// the signature already guarantees the whole document is trusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    /// Bundle version; must be strictly newer than the shell's watermark.
    pub version: Version,
    /// Informational publish timestamp (RFC 3339). Not used in any gate —
    /// version ordering is what matters — so it stays an opaque string.
    pub created_at: String,
    /// Minimum shell (app) version able to run this bundle.
    pub min_shell_version: Version,
    /// Native app version the bundle was built from (the repo version at
    /// release time). Optional: manifests without it skip the base gate, and
    /// older shells ignore it. A shell newer than this base is refused, so an
    /// OTA never replaces a newer native build's embedded frontend.
    ///
    /// Ordering is plain semver precedence, build metadata ignored: a
    /// prerelease base sorts below its release (`1.1.23-beta.1 < 1.1.23`), so
    /// the `1.1.23` shell refuses a bundle built from `1.1.23-beta.1`. The
    /// gate compares versions only, never build numbers: every native release
    /// must bump the app version.
    ///
    /// An unparseable value does not reject the manifest: it parses as
    /// [`BaseVersion::Invalid`] and the gate refuses only this bundle
    /// (`invalidBaseVersion`), so a bad field can never break the channel for
    /// shells that would otherwise just skip it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_version: Option<BaseVersion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<ReleaseContract>,
    pub archive: ArchiveInfo,
}

/// A manifest `baseVersion`: a semver, or the raw value when it does not
/// parse. Serialized as a plain string either way, so the wire shape is the
/// same as a bare `Version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseVersion {
    Valid(Version),
    /// Not a semver. Holds the raw value (a non-string JSON value is kept as
    /// its JSON text) for the refusal status and logs.
    Invalid(String),
}

impl BaseVersion {
    /// Parse a raw value; never fails.
    pub fn parse(raw: &str) -> Self {
        match Version::parse(raw) {
            Ok(version) => Self::Valid(version),
            Err(_) => Self::Invalid(raw.to_string()),
        }
    }
}

impl From<Version> for BaseVersion {
    fn from(version: Version) -> Self {
        Self::Valid(version)
    }
}

impl std::fmt::Display for BaseVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Valid(version) => version.fmt(f),
            Self::Invalid(raw) => f.write_str(raw),
        }
    }
}

impl Serialize for BaseVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for BaseVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Ok(match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::String(raw) => Self::parse(&raw),
            other => Self::Invalid(other.to_string()),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReleaseContract {
    pub application_id: String,
    pub channel: String,
    pub state_format: String,
    pub native_revision: String,
}

impl ReleaseContract {
    pub fn validate(&self) -> Result<()> {
        if self.application_id.is_empty()
            || self.channel.is_empty()
            || self.state_format.is_empty()
            || self.native_revision.len() != 40
            || !self
                .native_revision
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(Error::Config("OTA contract needs application, channel, state format and a 40-digit native revision".into()));
        }
        Ok(())
    }
}

/// Where the bundle archive lives and what it must hash to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveInfo {
    pub url: String,
    /// Hex sha256 of the archive bytes. Canonicalized to lowercase at parse
    /// time so it can serve as the blacklist identity in `state.json`.
    pub sha256: String,
    /// Exact archive byte count; the download aborts past it.
    pub size: u64,
}

/// Verify `signature` (the `.minisig` file contents) over the raw
/// `manifest_bytes` against the trusted key list, then parse and validate.
///
/// Any single trusted key verifying is sufficient (rotation). A malformed
/// key anywhere in the list is a hard [`Error::InvalidPublicKey`] — a broken
/// trust anchor is a config bug that must surface, not be skipped.
pub fn verify_and_parse(
    manifest_bytes: &[u8],
    signature: &str,
    trusted_pubkeys: &[String],
) -> Result<Manifest> {
    if trusted_pubkeys.is_empty() {
        return Err(Error::InvalidPublicKey);
    }
    // Parse every trust anchor before verifying anything: a malformed key
    // must surface even when an earlier key in the list would verify.
    let keys = trusted_pubkeys
        .iter()
        .map(|key| parse_public_key(key))
        .collect::<Result<Vec<_>>>()?;
    let signature = Signature::decode(signature)
        .map_err(|e| Error::ManifestSignature(format!("undecodable .minisig data: {e}")))?;

    let verified = keys
        .iter()
        .any(|key| key.verify(manifest_bytes, &signature, false).is_ok());
    if !verified {
        return Err(Error::ManifestSignature(
            "no trusted public key verified the manifest".into(),
        ));
    }

    let mut manifest: Manifest = serde_json::from_slice(manifest_bytes)?;
    validate(&mut manifest)?;
    Ok(manifest)
}

/// Init-time check that every configured trust anchor parses as minisign
/// key material ([`verify_and_parse`] re-parses at use). Same hard-stop rule
/// as verification: one malformed key fails the whole list.
pub(crate) fn validate_pubkeys(keys: &[String]) -> Result<()> {
    keys.iter()
        .try_for_each(|key| parse_public_key(key).map(drop))
}

/// Accept either the raw base64 key (`RW…`) or the full two-line
/// `minisign.pub` file contents.
fn parse_public_key(key: &str) -> Result<PublicKey> {
    let key = key.trim();
    let result = if key.lines().count() > 1 {
        PublicKey::decode(key)
    } else {
        PublicKey::from_base64(key)
    };
    result.map_err(|_| Error::InvalidPublicKey)
}

/// Sanity checks on the (already signature-verified) manifest, plus sha256
/// canonicalization to lowercase.
fn validate(manifest: &mut Manifest) -> Result<()> {
    let sha = &manifest.archive.sha256;
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::ManifestInvalid(format!(
            "archive.sha256 {sha:?} is not 64 hex characters"
        )));
    }
    manifest.archive.sha256 = manifest.archive.sha256.to_ascii_lowercase();

    let size = manifest.archive.size;
    if size == 0 || size > crate::extract::MAX_UNCOMPRESSED_BYTES {
        return Err(Error::ManifestInvalid(format!(
            "archive.size {size} is outside 1..={}",
            crate::extract::MAX_UNCOMPRESSED_BYTES
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
