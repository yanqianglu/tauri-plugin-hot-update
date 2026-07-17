//! Release-side signing: build the bundle archive, write the manifest, sign
//! it. This is the code behind the `hot-update-sign` CLI (feature `cli`) and
//! it is deliberately compiled under `cfg(test)` too, so the test suite
//! round-trips the *actual* signing code against the verify path with real
//! keys — never a reimplementation.
//!
//! The archive build is deterministic (sorted entries, zeroed mtimes/owners,
//! fixed modes): re-signing an identical dist yields an identical archive
//! and therefore an identical sha256. Only files are archived — extraction
//! recreates parent directories — and symlinks in the dist are followed on
//! the publisher's machine, landing as plain file content.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use semver::Version;
use sha2::{Digest, Sha256};

use crate::download::to_hex;
use crate::manifest::{ArchiveInfo, Manifest};

/// What to sign and where to put it.
#[derive(Debug)]
pub struct SignOptions<'a> {
    /// The built frontend dist directory (archive root = this directory).
    pub dist_dir: &'a Path,
    /// Bundle version (must exceed installed watermarks to be applied).
    pub version: Version,
    /// Minimum shell (app) version able to run this bundle.
    pub min_shell_version: Version,
    /// Public base URL the archive will be served from; the manifest's
    /// `archive.url` becomes `{base_url}/{archive file name}`.
    pub base_url: &'a str,
    /// Output directory for the three artifacts (created if missing).
    pub out_dir: &'a Path,
}

/// The three artifacts to upload, plus the manifest that was signed.
#[derive(Debug)]
pub struct SignedRelease {
    pub archive_path: PathBuf,
    pub manifest_path: PathBuf,
    pub signature_path: PathBuf,
    pub manifest: Manifest,
}

#[derive(Debug, thiserror::Error)]
pub enum SignError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("dist dir {0:?} contains no files")]
    EmptyDist(PathBuf),
    #[error("signing failed: {0}")]
    Minisign(#[from] minisign::PError),
}

/// Produce `bundle-<version>.tar.gz`, `manifest.json`, and
/// `manifest.json.minisig` in `out_dir`.
///
/// The signature is computed over the exact manifest bytes written to disk;
/// the manifest embeds the archive's real size and sha256. The secret key
/// must already be decrypted ([`minisign::SecretKey::from_file`] handles
/// passwords).
pub fn sign_release(
    options: &SignOptions<'_>,
    secret_key: &minisign::SecretKey,
) -> Result<SignedRelease, SignError> {
    fs::create_dir_all(options.out_dir)?;

    let archive_name = format!("bundle-{}.tar.gz", options.version);
    let archive_path = options.out_dir.join(&archive_name);
    build_archive(options.dist_dir, &archive_path)?;

    let archive_bytes = fs::read(&archive_path)?;
    let manifest = Manifest {
        version: options.version.clone(),
        created_at: created_at_now(),
        min_shell_version: options.min_shell_version.clone(),
        archive: ArchiveInfo {
            url: format!("{}/{archive_name}", options.base_url.trim_end_matches('/')),
            sha256: to_hex(&Sha256::digest(&archive_bytes)),
            size: archive_bytes.len() as u64,
        },
    };

    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(io::Error::other)?;
    let manifest_path = options.out_dir.join("manifest.json");
    fs::write(&manifest_path, &manifest_bytes)?;

    let signature = minisign::sign(
        None,
        secret_key,
        io::Cursor::new(&manifest_bytes),
        Some(&format!("hot-update manifest v{}", manifest.version)),
        None,
    )?;
    let signature_path = options.out_dir.join("manifest.json.minisig");
    fs::write(&signature_path, signature.into_string())?;

    Ok(SignedRelease {
        archive_path,
        manifest_path,
        signature_path,
        manifest,
    })
}

/// Deterministic tar.gz of every file under `dist_dir` (sorted relative
/// paths, mode 0644, uid/gid 0, mtime 0).
fn build_archive(dist_dir: &Path, archive_path: &Path) -> Result<(), SignError> {
    let mut files = Vec::new();
    collect_files(dist_dir, dist_dir, &mut files)?;
    if files.is_empty() {
        return Err(SignError::EmptyDist(dist_dir.to_path_buf()));
    }
    files.sort();

    let gz =
        flate2::write::GzEncoder::new(fs::File::create(archive_path)?, flate2::Compression::best());
    let mut tar = tar::Builder::new(gz);
    for relative in &files {
        let mut header = tar::Header::new_gnu();
        let data = fs::read(dist_dir.join(relative))?;
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        // append_data sets the path (with GNU long-name fallback) + cksum.
        tar.append_data(&mut header, relative, io::Cursor::new(data))?;
    }
    tar.into_inner()?.finish()?.sync_all()?;
    Ok(())
}

/// Relative paths of all files under `dir`, recursively; symlinks followed.
fn collect_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else {
            let relative = path
                .strip_prefix(root)
                .map_err(io::Error::other)?
                .to_path_buf();
            out.push(relative);
        }
    }
    Ok(())
}

/// RFC 3339 UTC timestamp for the manifest's informational `createdAt`.
fn created_at_now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("RFC 3339 formatting of the current UTC time cannot fail")
}

#[cfg(test)]
mod tests;
