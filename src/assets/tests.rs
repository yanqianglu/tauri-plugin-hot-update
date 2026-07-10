//! Provider tests: OTA-dir-first serving, embedded fallback, path-traversal
//! strictness, and the source-keyed CSP rule — against real files.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use semver::Version;
use tauri::Wry;
use tempfile::TempDir;

use super::*;
use crate::machine::{stage, BundleMeta};
use crate::runtime::initialize;
use crate::store::Store;

/// Real test double for the compiled-in `EmbeddedAssets` (the one true
/// system boundary here): serves from a map and returns a recognizable CSP
/// marker so delegation is observable.
struct FakeEmbedded {
    files: HashMap<String, Vec<u8>>,
}

const EMBEDDED_CSP_MARKER: &str = "sha256-EMBEDDED-MARKER";

impl FakeEmbedded {
    fn new(files: &[(&str, &str)]) -> Self {
        Self {
            files: files
                .iter()
                .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
                .collect(),
        }
    }
}

impl Assets<Wry> for FakeEmbedded {
    fn get(&self, key: &AssetKey) -> Option<Cow<'_, [u8]>> {
        self.files.get(key.as_ref()).map(|bytes| Cow::Borrowed(bytes.as_slice()))
    }

    fn iter(&self) -> Box<AssetsIter<'_>> {
        Box::new(
            self.files
                .iter()
                .map(|(k, v)| (Cow::Borrowed(k.as_str()), Cow::Borrowed(v.as_slice()))),
        )
    }

    fn csp_hashes(&self, _html_path: &AssetKey) -> Box<dyn Iterator<Item = CspHash<'_>> + '_> {
        Box::new(std::iter::once(CspHash::Script(EMBEDDED_CSP_MARKER)))
    }
}

fn provider(shared: &Arc<Shared>, embedded: FakeEmbedded) -> HotUpdateAssets<Wry> {
    HotUpdateAssets::new(Box::new(embedded), Arc::clone(shared))
}

/// Activate an OTA bundle through the real boot path: extract, stage, boot.
fn activate_bundle(root: &Path, files: &[(&str, &str)]) -> Arc<Shared> {
    let store = Store::new(root.to_path_buf());
    let dir = store.bundle_dir(1);
    for (name, contents) in files {
        let path = dir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    store
        .update(|state| {
            stage(
                state,
                1,
                BundleMeta {
                    version: Version::parse("1.1.0").unwrap(),
                    archive_sha256: "sha-1".to_string(),
                },
            )
        })
        .unwrap()
        .unwrap();

    let shared = Arc::new(Shared::default());
    initialize(&shared, root.to_path_buf(), Version::parse("1.0.0").unwrap());
    assert_eq!(shared.active_dir(), Some(&store.bundle_dir(1)));
    shared
}

fn get_str(assets: &HotUpdateAssets<Wry>, key: &str) -> Option<String> {
    Assets::<Wry>::get(assets, &AssetKey::from(key))
        .map(|bytes| String::from_utf8(bytes.into_owned()).unwrap())
}

fn csp_directives(assets: &HotUpdateAssets<Wry>, key: &str) -> Vec<String> {
    Assets::<Wry>::csp_hashes(assets, &AssetKey::from(key))
        .map(|hash| hash.hash().to_string())
        .collect()
}

// ------------------------------------------------------------------- serving

#[test]
fn unactivated_provider_serves_embedded_only() {
    let shared = Arc::new(Shared::default());
    let assets = provider(&shared, FakeEmbedded::new(&[("/index.html", "EMBEDDED")]));
    assert_eq!(get_str(&assets, "/index.html").as_deref(), Some("EMBEDDED"));
    assert_eq!(get_str(&assets, "/missing.js"), None);
}

#[test]
fn active_bundle_wins_over_embedded_per_file() {
    let tmp = TempDir::new().unwrap();
    let shared = activate_bundle(
        tmp.path(),
        &[("index.html", "OTA HTML"), ("assets/app.js", "OTA JS")],
    );
    let assets = provider(
        &shared,
        FakeEmbedded::new(&[
            ("/index.html", "EMBEDDED HTML"),
            ("/assets/app.js", "EMBEDDED JS"),
            ("/only-embedded.css", "EMBEDDED CSS"),
        ]),
    );

    assert_eq!(get_str(&assets, "/index.html").as_deref(), Some("OTA HTML"));
    assert_eq!(get_str(&assets, "/assets/app.js").as_deref(), Some("OTA JS"));
    // Missing in the bundle → per-file fallback to embedded.
    assert_eq!(
        get_str(&assets, "/only-embedded.css").as_deref(),
        Some("EMBEDDED CSS")
    );
    // Missing everywhere → None.
    assert_eq!(get_str(&assets, "/nowhere.txt"), None);
}

#[test]
fn provider_answers_exact_keys_only_no_spa_fallback() {
    // SPA-route fallback (`/route` → `/index.html`) belongs to tauri's
    // get_asset, which retries per candidate — never to the provider.
    let tmp = TempDir::new().unwrap();
    let shared = activate_bundle(tmp.path(), &[("index.html", "OTA HTML")]);
    let assets = provider(&shared, FakeEmbedded::new(&[("/index.html", "EMBEDDED")]));
    assert_eq!(get_str(&assets, "/quick-entry"), None);
    assert_eq!(get_str(&assets, "/quick-entry/index.html"), None);
}

// -------------------------------------------------------------- path safety

#[test]
fn traversal_keys_never_escape_the_bundle_dir() {
    let tmp = TempDir::new().unwrap();
    let shared = activate_bundle(tmp.path(), &[("index.html", "OTA")]);
    // Plant secrets around the bundle dir, shaped like the app's real
    // neighbors (state.json lives one level above bundles/).
    fs::write(tmp.path().join("secret.txt"), "TOP SECRET").unwrap();
    fs::write(tmp.path().join("bundles").join("sibling.txt"), "SIBLING").unwrap();

    let assets = provider(&shared, FakeEmbedded::new(&[]));
    for key in [
        "/../sibling.txt",
        "/../../secret.txt",
        "/../../state.json",
        "../secret.txt",
        "/foo/../../../secret.txt",
        "/./secret.txt",
        "/..",
        "/",
        "",
    ] {
        assert_eq!(
            get_str(&assets, key),
            None,
            "key {key:?} must not resolve outside the bundle"
        );
    }
}

#[test]
fn safe_join_accepts_normal_paths_and_rejects_everything_else() {
    let dir = Path::new("/bundle");
    assert_eq!(
        safe_join(dir, "/assets/index-abc.js"),
        Some(dir.join("assets").join("index-abc.js"))
    );
    assert_eq!(safe_join(dir, "index.html"), Some(dir.join("index.html")));
    // `a/./b` normalizes to `a/b` — inside the dir, allowed.
    assert_eq!(safe_join(dir, "a/./b"), Some(dir.join("a").join("b")));

    for bad in ["", "/", "..", "../x", "a/../b", "./x", "/../x"] {
        assert_eq!(safe_join(dir, bad), None, "must reject {bad:?}");
    }
}

// ---------------------------------------------------------------------- CSP

#[test]
fn csp_hashes_are_keyed_to_the_active_source_per_key() {
    let tmp = TempDir::new().unwrap();
    let shared = activate_bundle(tmp.path(), &[("index.html", "OTA HTML")]);
    let assets = provider(
        &shared,
        FakeEmbedded::new(&[("/index.html", "EMB"), ("/settings.html", "EMB")]),
    );

    // OTA-served HTML: compile-time hashes would be stale → empty, which
    // leaves a configured 'unsafe-inline' active (plain-web-deploy behavior).
    assert!(csp_directives(&assets, "/index.html").is_empty());
    // HTML that falls back to embedded gets the matching embedded hashes.
    assert_eq!(
        csp_directives(&assets, "/settings.html"),
        vec![EMBEDDED_CSP_MARKER.to_string()]
    );
}

#[test]
fn csp_hashes_delegate_to_embedded_when_not_activated() {
    let shared = Arc::new(Shared::default());
    let assets = provider(&shared, FakeEmbedded::new(&[("/index.html", "EMB")]));
    assert_eq!(
        csp_directives(&assets, "/index.html"),
        vec![EMBEDDED_CSP_MARKER.to_string()]
    );
}

// ---------------------------------------------------------------- iteration

#[test]
fn iter_delegates_to_embedded() {
    let shared = Arc::new(Shared::default());
    let assets = provider(&shared, FakeEmbedded::new(&[("/a.js", "A")]));
    let entries: Vec<String> = Assets::<Wry>::iter(&assets)
        .map(|(key, _)| key.into_owned())
        .collect();
    assert_eq!(entries, vec!["/a.js".to_string()]);
}
