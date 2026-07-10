//! The update-acquisition pipeline: manifest fetch → signature verification
//! → gates → archive download (sha256 streamed) → hardened extraction →
//! atomic staging. This is what WP4's `check`/`download` commands call.
//!
//! Pipeline order is the verification chain from the design doc — each step
//! consumes only the previous step's *verified* output, and every gate is a
//! hard stop (typed refusal or error), never a warn-and-continue:
//!
//! 1. fetch manifest bytes + detached `.minisig` (capped bodies)
//! 2. minisign verify against the embedded trusted-key list (rotation-ready)
//! 3. parse + validate JSON
//! 4. gates against persisted state: `version > maxVersionSeen` (downgrade
//!    replay), archive sha256 not blacklisted, `minShellVersion <= shell`
//! 5. stream archive to a temp file under `bundles/`, hashing en route;
//!    exact size + sha256 enforced before anything touches the bytes
//! 6. extract (hardened) into a temp dir under `bundles/`
//! 7. `allocate_seq` → atomic rename to `bundles/seq-N/` → `machine::stage`
//!    via `Store::update` (which re-checks the gates — belt and braces)
//!
//! A crash anywhere leaves at worst a temp file/dir under `bundles/`, which
//! the next boot's GC sweeps. Idempotent: an offered version that is already
//! staged or committed is reported as such, not re-downloaded.

use std::fs;

use semver::Version;
use serde::Serialize;

use crate::download::{
    download_archive, fetch_capped, http_client, MANIFEST_MAX_BYTES, SIGNATURE_MAX_BYTES,
};
use crate::machine::{self, BundleMeta, State};
use crate::manifest::{self, Manifest};
use crate::runtime::HotUpdate;
use crate::{Error, Result};

/// Where updates come from and who is trusted to sign them. WP4 reads this
/// from the plugin config; WP3 callers construct it directly.
#[derive(Debug, Clone)]
pub struct UpdateConfig {
    /// URL of `manifest.json`. The detached signature is fetched from
    /// `{manifest_url}.minisig`, so the URL must be a plain file URL
    /// (no query strings).
    pub manifest_url: String,
    /// Trusted minisign public keys — raw base64 or full `minisign.pub`
    /// contents. A manifest verifying under ANY key is trusted (rotation:
    /// ship old + new during a transition).
    pub pubkeys: Vec<String>,
}

/// Result of a check or download pass. Refusals are first-class outcomes,
/// not errors: they are the pipeline saying "this manifest is not for us",
/// with the reason preserved for the app/UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum UpdateOutcome {
    /// `check()` only: a verified, applicable update is offered.
    Available { manifest: Manifest },
    /// `check_and_download()` only: downloaded, verified, extracted, staged
    /// for the next cold launch.
    Staged { seq: u64, version: Version },
    /// The offered version is not strictly newer than the watermark. This is
    /// both the everyday "no update" answer and the downgrade-replay
    /// rejection — an old validly-signed manifest lands here.
    UpToDate { offered: Version, watermark: Version },
    /// The offered archive hash previously failed a trial boot and is
    /// permanently blacklisted; a fixed release ships under a new hash.
    Blacklisted { version: Version },
    /// The bundle requires a newer shell; this install stays put until a
    /// store update arrives.
    ShellTooOld { required: Version, shell: Version },
    /// Exactly this archive is already staged and waiting for its trial
    /// boot; nothing to do.
    AlreadyStaged { seq: u64, version: Version },
}

impl HotUpdate {
    /// Fetch and verify the manifest, then report whether it is applicable.
    /// Never downloads the archive. Errors are verification/transport
    /// failures; gate refusals are `Ok` outcomes.
    pub async fn check(&self, config: &UpdateConfig) -> Result<UpdateOutcome> {
        let activation = self.shared.activation()?;
        let _serialized = activation.update_lock().lock().await;
        let client = http_client()?;
        let manifest = fetch_verified_manifest(&client, config).await?;
        let state = activation.store().load_state();
        Ok(match gate(&manifest, &state, activation.embedded_version()) {
            Some(refusal) => refusal,
            None => UpdateOutcome::Available { manifest },
        })
    }

    /// The full pipeline: check, and if an update is applicable, download,
    /// verify, extract, and stage it for the next cold launch.
    ///
    /// `on_progress(downloaded, total)` fires per received archive chunk.
    /// Concurrent calls are serialized; the loser of the race then reports
    /// `AlreadyStaged`/`UpToDate` instead of downloading twice.
    pub async fn check_and_download(
        &self,
        config: &UpdateConfig,
        mut on_progress: impl FnMut(u64, u64) + Send,
    ) -> Result<UpdateOutcome> {
        let activation = self.shared.activation()?;
        let _serialized = activation.update_lock().lock().await;
        let client = http_client()?;
        let manifest = fetch_verified_manifest(&client, config).await?;
        let store = activation.store();
        if let Some(refusal) = gate(&manifest, &store.load_state(), activation.embedded_version())
        {
            return Ok(refusal);
        }

        // Download to a temp file under bundles/: deleted on drop on any
        // failure, and swept by boot GC even after a hard kill.
        let bundles_dir = store.bundles_dir();
        fs::create_dir_all(&bundles_dir)?;
        let mut archive_file = tempfile::Builder::new()
            .prefix("dl-")
            .suffix(".part")
            .tempfile_in(&bundles_dir)?;
        download_archive(
            &client,
            &manifest.archive,
            archive_file.as_file_mut(),
            &mut on_progress,
        )
        .await?;

        // Extract into a temp dir next to the final location (same
        // filesystem, so the promotion below is a true atomic rename).
        let extract_dir = tempfile::Builder::new()
            .prefix("extract-")
            .tempdir_in(&bundles_dir)?;
        let archive_path = archive_file.path().to_path_buf();
        let target = extract_dir.path().to_path_buf();
        tokio::task::spawn_blocking(move || crate::extract::extract_tar_gz(&archive_path, &target))
            .await
            .map_err(|e| Error::Io(std::io::Error::other(e)))??;
        drop(archive_file);

        // Promote: allocate a never-used seq, atomically rename the fully
        // extracted dir into place, then flip the staged pointer. A crash
        // between rename and stage leaves an orphan dir for boot GC.
        let seq = store.allocate_seq(&store.load_state());
        let final_dir = store.bundle_dir(seq);
        fs::rename(extract_dir.keep(), &final_dir)?;
        let meta = BundleMeta {
            version: manifest.version.clone(),
            archive_sha256: manifest.archive.sha256.clone(),
        };
        match store.update(|state| machine::stage(state, seq, meta))? {
            Ok(effects) => {
                store.apply_effects(&effects);
                log::info!(
                    "hot-update: bundle v{} staged as seq {seq} for the next launch",
                    manifest.version
                );
                Ok(UpdateOutcome::Staged {
                    seq,
                    version: manifest.version,
                })
            }
            Err(refused) => {
                // State changed between the gate check and staging (e.g. a
                // reset). The machine's refusal stands; discard the dir.
                let _ = fs::remove_dir_all(&final_dir);
                Err(Error::StageRefused(refused))
            }
        }
    }
}

/// Fetch manifest + detached signature, verify against the trusted keys,
/// parse, validate. Nothing downstream sees unverified bytes.
async fn fetch_verified_manifest(
    client: &reqwest::Client,
    config: &UpdateConfig,
) -> Result<Manifest> {
    let manifest_bytes = fetch_capped(client, &config.manifest_url, MANIFEST_MAX_BYTES).await?;
    let signature_url = format!("{}.minisig", config.manifest_url);
    let signature_bytes = fetch_capped(client, &signature_url, SIGNATURE_MAX_BYTES).await?;
    let signature = String::from_utf8(signature_bytes)
        .map_err(|_| Error::ManifestSignature("signature file is not UTF-8".into()))?;
    manifest::verify_and_parse(&manifest_bytes, &signature, &config.pubkeys)
}

/// Manifest-accept gates, in refusal-precedence order. `None` means the
/// manifest is applicable. `machine::stage` re-checks the watermark and
/// blacklist at staging time — these gates simply refuse earlier, before
/// any bytes are downloaded.
fn gate(manifest: &Manifest, state: &State, shell: &Version) -> Option<UpdateOutcome> {
    if let Some(watermark) = &state.max_version_seen {
        if manifest.version <= *watermark {
            return Some(UpdateOutcome::UpToDate {
                offered: manifest.version.clone(),
                watermark: watermark.clone(),
            });
        }
    }
    if state.failed.contains(&manifest.archive.sha256) {
        return Some(UpdateOutcome::Blacklisted {
            version: manifest.version.clone(),
        });
    }
    if manifest.min_shell_version > *shell {
        return Some(UpdateOutcome::ShellTooOld {
            required: manifest.min_shell_version.clone(),
            shell: shell.clone(),
        });
    }
    if let Some(seq) = state.staged {
        if state
            .versions
            .get(&seq)
            .is_some_and(|meta| meta.archive_sha256 == manifest.archive.sha256)
        {
            return Some(UpdateOutcome::AlreadyStaged {
                seq,
                version: manifest.version.clone(),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests;
