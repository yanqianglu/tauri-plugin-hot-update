//! HTTP fetch helpers: capped small-body fetches (manifest, signature) and
//! the streaming archive download with sha256-during-download verification.
//!
//! Integrity comes from the signature chain, not the transport: the archive
//! must hash to the sha256 pinned inside the *signed* manifest, and its byte
//! count must match exactly — a stream that runs long is aborted mid-flight,
//! one that runs short is refused at the end. There is no resume: a partial
//! or mismatched download is discarded and restarted from scratch (v1
//! simplicity; archives are a few MB).

use std::fmt::Write as _;
use std::fs::File;
use std::io::Write as _;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::manifest::ArchiveInfo;
use crate::{Error, Result};

/// Sanity cap for the manifest body. Real manifests are a few hundred bytes.
pub(crate) const MANIFEST_MAX_BYTES: u64 = 64 * 1024;

/// Sanity cap for the `.minisig` body. Real signatures are ~300 bytes.
pub(crate) const SIGNATURE_MAX_BYTES: u64 = 8 * 1024;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Progress callback: `(downloaded_bytes, total_bytes)`. Invoked once per
/// received chunk; WP4 forwards it as `download` progress events.
pub type OnProgress<'a> = &'a mut (dyn FnMut(u64, u64) + Send);

pub(crate) fn http_client() -> Result<reqwest::Client> {
    // reqwest's `rustls-no-provider` requires a process-level crypto
    // provider before a client can be built. Install ring exactly like
    // tauri-plugin-updater does, unless the app already chose one.
    #[cfg(feature = "rustls-tls")]
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        // Can only fail if a default was installed concurrently — fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    Ok(reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()?)
}

/// GET `url` into memory, refusing bodies over `cap` bytes.
pub(crate) async fn fetch_capped(
    client: &reqwest::Client,
    url: &str,
    cap: u64,
) -> Result<Vec<u8>> {
    let mut response = checked_get(client, url).await?;
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() as u64 + chunk.len() as u64 > cap {
            return Err(Error::ResponseTooLarge {
                url: url.to_string(),
                limit: cap,
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Stream the archive into `dest`, hashing as it downloads. Enforces the
/// signed manifest's exact `size` (aborting as soon as the stream exceeds
/// it) and its `sha256`. `dest` holds partial data on failure — the caller
/// owns its cleanup (a tempfile, deleted on drop).
pub(crate) async fn download_archive(
    client: &reqwest::Client,
    archive: &ArchiveInfo,
    dest: &mut File,
    on_progress: OnProgress<'_>,
) -> Result<()> {
    let mut response = checked_get(client, &archive.url).await?;
    let mut hasher = Sha256::new();
    let mut downloaded: u64 = 0;
    while let Some(chunk) = response.chunk().await? {
        downloaded += chunk.len() as u64;
        if downloaded > archive.size {
            return Err(Error::ArchiveSize {
                declared: archive.size,
                actual: downloaded,
            });
        }
        hasher.update(&chunk);
        dest.write_all(&chunk)?;
        on_progress(downloaded, archive.size);
    }
    if downloaded != archive.size {
        return Err(Error::ArchiveSize {
            declared: archive.size,
            actual: downloaded,
        });
    }
    let actual = to_hex(&hasher.finalize());
    if actual != archive.sha256 {
        return Err(Error::ArchiveSha256 {
            declared: archive.sha256.clone(),
            actual,
        });
    }
    dest.flush()?;
    Ok(())
}

async fn checked_get(client: &reqwest::Client, url: &str) -> Result<reqwest::Response> {
    let response = client.get(url).send().await?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::HttpStatus {
            status: status.as_u16(),
            url: url.to_string(),
        });
    }
    Ok(response)
}

/// Lowercase hex — the canonical sha256 form used in manifests and the
/// failure blacklist.
pub(crate) fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}
