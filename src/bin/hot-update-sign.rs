//! `hot-update-sign` — produce a signed OTA release from a built dist dir:
//! `bundle-<version>.tar.gz`, `manifest.json`, `manifest.json.minisig`.
//!
//! ```text
//! hot-update-sign \
//!   --dist path/to/dist \
//!   --version 1.2.0 \
//!   --min-shell 1.1.0 \
//!   --base-version 1.1.1 \
//!   --key ~/.keys/hot-update.key \
//!   --base-url https://cdn.example.com/ota \
//!   --out release/
//! ```
//!
//! The minisign secret key password is read from the
//! `HOT_UPDATE_KEY_PASSWORD` environment variable when set (use an empty
//! value for unencrypted keys in CI); otherwise an interactive prompt asks
//! for it. Requires the `cli` feature.

use std::path::PathBuf;
use std::process::ExitCode;

use semver::Version;
use tauri_plugin_hot_update::sign::{sign_release, SignOptions};

const USAGE: &str = "usage: hot-update-sign --dist <dir> --version <semver> \
--min-shell <semver> [--base-version <semver>] --key <secret-key-file> --base-url <url> --out <dir> [--application-id <id> --channel <name> --state-format <format> --native-revision <sha> --public-key <file>]";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("hot-update-sign: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let password = std::env::var("HOT_UPDATE_KEY_PASSWORD").ok();
    let boxed = std::fs::read_to_string(&args.key).map_err(|e| e.to_string())?;
    let key_box = minisign::SecretKeyBox::from_string(&boxed).map_err(|e| e.to_string())?;
    let secret_key = minisign::SecretKey::from_unencrypted_box(key_box)
        .or_else(|_| minisign::SecretKey::from_file(&args.key, password))
        .map_err(|e| format!("cannot load secret key {}: {e}", args.key.display()))?;

    let options = SignOptions {
        dist_dir: &args.dist,
        version: args.version,
        min_shell_version: args.min_shell,
        base_version: args.base_version,
        contract: args.contract,
        base_url: &args.base_url,
        out_dir: &args.out,
    };
    let release = sign_release(&options, &secret_key).map_err(|e| e.to_string())?;
    if let Some(public_key) = args.public_key {
        let key = std::fs::read_to_string(public_key).map_err(|error| error.to_string())?;
        let bytes = std::fs::read(&release.manifest_path).map_err(|error| error.to_string())?;
        let signature =
            std::fs::read_to_string(&release.signature_path).map_err(|error| error.to_string())?;
        tauri_plugin_hot_update::verify_and_parse(&bytes, &signature, &[key])
            .map_err(|error| error.to_string())?;
    }

    println!("signed hot-update release v{}", release.manifest.version);
    println!("  archive:   {}", release.archive_path.display());
    println!("  manifest:  {}", release.manifest_path.display());
    println!("  signature: {}", release.signature_path.display());
    println!("  sha256:    {}", release.manifest.archive.sha256);
    println!("  size:      {} bytes", release.manifest.archive.size);
    println!("  url:       {}", release.manifest.archive.url);
    Ok(())
}

struct Args {
    contract: Option<tauri_plugin_hot_update::ReleaseContract>,
    public_key: Option<PathBuf>,
    dist: PathBuf,
    version: Version,
    min_shell: Version,
    base_version: Option<Version>,
    key: PathBuf,
    base_url: String,
    out: PathBuf,
}

fn parse_args() -> Result<Args, String> {
    let mut application_id = None;
    let mut channel = None;
    let mut state_format = None;
    let mut native_revision = None;
    let mut public_key = None;
    let mut dist = None;
    let mut version = None;
    let mut min_shell = None;
    let mut base_version = None;
    let mut key = None;
    let mut base_url = None;
    let mut out = None;

    let mut argv = std::env::args().skip(1);
    while let Some(flag) = argv.next() {
        if flag == "--help" || flag == "-h" {
            return Err(USAGE.to_string());
        }
        let value = argv
            .next()
            .ok_or_else(|| format!("missing value for {flag}\n{USAGE}"))?;
        match flag.as_str() {
            "--application-id" => application_id = Some(value),
            "--channel" => channel = Some(value),
            "--state-format" => state_format = Some(value),
            "--native-revision" => native_revision = Some(value),
            "--public-key" => public_key = Some(PathBuf::from(value)),
            "--dist" => dist = Some(PathBuf::from(value)),
            "--version" => version = Some(parse_version(&flag, &value)?),
            "--min-shell" => min_shell = Some(parse_version(&flag, &value)?),
            "--base-version" => base_version = Some(parse_version(&flag, &value)?),
            "--key" => key = Some(PathBuf::from(value)),
            "--base-url" => base_url = Some(value),
            "--out" => out = Some(PathBuf::from(value)),
            other => return Err(format!("unknown flag {other}\n{USAGE}")),
        }
    }

    let missing = |name: &str| format!("missing required flag {name}\n{USAGE}");
    let contract = if application_id.is_some()
        || channel.is_some()
        || state_format.is_some()
        || native_revision.is_some()
    {
        let contract = tauri_plugin_hot_update::ReleaseContract {
            application_id: application_id.ok_or_else(|| missing("--application-id"))?,
            channel: channel.ok_or_else(|| missing("--channel"))?,
            state_format: state_format.ok_or_else(|| missing("--state-format"))?,
            native_revision: native_revision.ok_or_else(|| missing("--native-revision"))?,
        };
        contract.validate().map_err(|error| error.to_string())?;
        if public_key.is_none() {
            return Err(missing("--public-key"));
        }
        if base_version.as_ref() != min_shell.as_ref() || base_version.is_none() {
            return Err("scoped releases require the same --base-version and --min-shell".into());
        }
        Some(contract)
    } else {
        None
    };
    Ok(Args {
        contract,
        public_key,
        dist: dist.ok_or_else(|| missing("--dist"))?,
        version: version.ok_or_else(|| missing("--version"))?,
        min_shell: min_shell.ok_or_else(|| missing("--min-shell"))?,
        base_version,
        key: key.ok_or_else(|| missing("--key"))?,
        base_url: base_url.ok_or_else(|| missing("--base-url"))?,
        out: out.ok_or_else(|| missing("--out"))?,
    })
}

fn parse_version(flag: &str, value: &str) -> Result<Version, String> {
    Version::parse(value).map_err(|e| format!("{flag} {value:?} is not a semver version: {e}"))
}
