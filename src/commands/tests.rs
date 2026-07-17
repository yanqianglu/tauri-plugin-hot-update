//! Command-layer tests: the full plugin (assets install + init + IPC) on
//! `tauri::test::MockRuntime`, with commands invoked through the real IPC
//! route (ACL, argument parsing, serde response) against a real on-disk
//! store — plus golden tests pinning every JSON shape the TypeScript
//! package (`guest-js/`) replicates. Renaming a Rust field must fail here
//! before it can break the cross-language contract.
//!
//! Each `build_app` call is one "cold launch": a fresh `Shared` (new
//! process) over a persistent store root, exactly like the runtime tests.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use semver::Version;
use serde_json::{json, Value};
use tauri::ipc::{CallbackFn, InvokeBody};
use tauri::test::{get_ipc_response, mock_builder, mock_context, noop_assets, MockRuntime};
use tauri::utils::acl::ExecutionContext;
use tauri::webview::InvokeRequest;
use tauri::{App, Listener, WebviewWindow, WebviewWindowBuilder};
use tempfile::TempDir;

use super::*;
use crate::machine::{stage, BundleMeta};
use crate::manifest::{ArchiveInfo, Manifest};
use crate::store::Store;
use crate::testutil::Fixture;

fn ver(s: &str) -> Version {
    Version::parse(s).unwrap()
}

/// One cold launch: full plugin (install + init) over `root`, with all five
/// commands allowed for the local origin. `plugin_config` is the raw
/// `plugins.hot-update` value from tauri.conf.json.
fn try_build_app(root: &Path, plugin_config: Option<Value>) -> tauri::Result<App<MockRuntime>> {
    let mut context = mock_context(noop_assets());
    if let Some(config) = plugin_config {
        context
            .config_mut()
            .plugins
            .0
            .insert("hot-update".into(), config);
    }
    for cmd in [
        "check",
        "download",
        "notify_app_ready",
        "current_bundle",
        "reset",
    ] {
        context
            .runtime_authority_mut()
            .__allow_command(format!("plugin:hot-update|{cmd}"), ExecutionContext::Local);
    }
    let handle = crate::install(&mut context);
    mock_builder()
        .plugin(crate::init_for_test(handle, root.to_path_buf()))
        .build(context)
}

fn build_app(root: &Path, plugin_config: Value) -> (App<MockRuntime>, WebviewWindow<MockRuntime>) {
    let app = try_build_app(root, Some(plugin_config)).expect("app builds");
    let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .expect("webview builds");
    (app, webview)
}

/// Invoke a plugin command through the real IPC route.
fn invoke(webview: &WebviewWindow<MockRuntime>, cmd: &str) -> std::result::Result<Value, Value> {
    let url = if cfg!(any(windows, target_os = "android")) {
        "http://tauri.localhost"
    } else {
        "tauri://localhost"
    };
    get_ipc_response(
        webview,
        InvokeRequest {
            cmd: format!("plugin:hot-update|{cmd}"),
            callback: CallbackFn(0),
            error: CallbackFn(1),
            url: url.parse().unwrap(),
            body: InvokeBody::default(),
            headers: Default::default(),
            invoke_key: tauri::test::INVOKE_KEY.to_string(),
        },
    )
    .map(|body| body.deserialize::<Value>().unwrap())
}

/// A config that passes init validation without any network expectations.
fn enabled_config(fx: &Fixture) -> Value {
    let update = fx.config();
    json!({ "manifestUrl": update.manifest_url, "pubkeys": update.pubkeys })
}

/// Simulate the downloader having staged a bundle (as in the runtime tests).
fn stage_bundle(root: &Path, seq: u64, version: &str, sha: &str) {
    let store = Store::new(root.to_path_buf());
    let dir = store.bundle_dir(seq);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("index.html"), format!("bundle {seq}")).unwrap();
    store
        .update(|state| {
            stage(
                state,
                seq,
                BundleMeta {
                    version: ver(version),
                    archive_sha256: sha.to_string(),
                },
            )
        })
        .unwrap()
        .expect("stage gates should pass");
}

fn raw_state(root: &Path) -> Value {
    serde_json::from_slice(&fs::read(root.join("state.json")).unwrap()).unwrap()
}

// ---------------------------------------------------------- wire shapes
// Golden tests: the exact JSON the TS types in guest-js/index.ts replicate.

#[test]
fn update_outcome_wire_shapes_are_pinned() {
    let manifest = Manifest {
        version: ver("1.1.0"),
        created_at: "2026-07-10T00:00:00Z".into(),
        min_shell_version: ver("1.0.0"),
        archive: ArchiveInfo {
            url: "https://cdn.example.com/bundle.tar.gz".into(),
            sha256: "aa".repeat(32),
            size: 4096,
        },
    };
    let cases = [
        (
            UpdateOutcome::Available { manifest },
            json!({
                "status": "available",
                "manifest": {
                    "version": "1.1.0",
                    "createdAt": "2026-07-10T00:00:00Z",
                    "minShellVersion": "1.0.0",
                    "archive": {
                        "url": "https://cdn.example.com/bundle.tar.gz",
                        "sha256": "aa".repeat(32),
                        "size": 4096,
                    },
                },
            }),
        ),
        (
            UpdateOutcome::Staged {
                seq: 3,
                version: ver("1.1.0"),
            },
            json!({ "status": "staged", "seq": 3, "version": "1.1.0" }),
        ),
        (
            UpdateOutcome::UpToDate {
                offered: ver("1.0.0"),
                watermark: ver("1.1.0"),
            },
            json!({ "status": "upToDate", "offered": "1.0.0", "watermark": "1.1.0" }),
        ),
        (
            UpdateOutcome::Blacklisted {
                version: ver("1.1.0"),
            },
            json!({ "status": "blacklisted", "version": "1.1.0" }),
        ),
        (
            UpdateOutcome::ShellTooOld {
                required: ver("2.0.0"),
                shell: ver("1.0.0"),
            },
            json!({ "status": "shellTooOld", "required": "2.0.0", "shell": "1.0.0" }),
        ),
        (
            UpdateOutcome::AlreadyStaged {
                seq: 3,
                version: ver("1.1.0"),
            },
            json!({ "status": "alreadyStaged", "seq": 3, "version": "1.1.0" }),
        ),
    ];
    for (outcome, expected) in cases {
        assert_eq!(
            serde_json::to_value(&outcome).unwrap(),
            expected,
            "{outcome:?}"
        );
    }
}

#[test]
fn ack_result_wire_shapes_are_pinned() {
    let cases = [
        (
            AckResult::Committed { seq: 2 },
            json!({ "status": "committed", "seq": 2 }),
        ),
        (
            AckResult::AlreadyCommitted { seq: 2 },
            json!({ "status": "alreadyCommitted", "seq": 2 }),
        ),
        (AckResult::EmbeddedNoop, json!({ "status": "embeddedNoop" })),
        (
            AckResult::Stale { seq: 2 },
            json!({ "status": "stale", "seq": 2 }),
        ),
    ];
    for (result, expected) in cases {
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            expected,
            "{result:?}"
        );
    }
}

#[test]
fn current_bundle_wire_shapes_are_pinned() {
    let ota = CurrentBundle {
        source: BundleSource::Ota,
        seq: Some(1),
        version: ver("1.1.0"),
    };
    assert_eq!(
        serde_json::to_value(&ota).unwrap(),
        json!({ "source": "ota", "seq": 1, "version": "1.1.0" })
    );
    let embedded = CurrentBundle {
        source: BundleSource::Embedded,
        seq: None,
        version: ver("1.0.0"),
    };
    assert_eq!(
        serde_json::to_value(&embedded).unwrap(),
        json!({ "source": "embedded", "seq": null, "version": "1.0.0" })
    );
}

#[test]
fn download_progress_wire_shape_is_pinned() {
    let progress = DownloadProgress {
        downloaded: 1024,
        total: 4096,
    };
    let value = serde_json::to_value(progress).unwrap();
    assert_eq!(value, json!({ "downloaded": 1024, "total": 4096 }));
    // Round-trips for Rust-side listeners of PROGRESS_EVENT.
    assert_eq!(
        serde_json::from_value::<DownloadProgress>(value).unwrap(),
        progress
    );
}

// ------------------------------------------------------------- throttle

#[test]
fn throttle_gates_by_time_but_always_passes_first_and_final() {
    let mut throttle = ProgressThrottle::new(Duration::from_secs(3600));
    assert!(throttle.should_emit(1, 100), "first chunk always emits");
    assert!(
        !throttle.should_emit(2, 100),
        "within the interval: suppressed"
    );
    assert!(!throttle.should_emit(50, 100));
    assert!(throttle.should_emit(100, 100), "final chunk always emits");

    let mut unthrottled = ProgressThrottle::new(Duration::ZERO);
    assert!(unthrottled.should_emit(1, 100));
    assert!(
        unthrottled.should_emit(2, 100),
        "elapsed >= zero interval: emits"
    );
}

// ------------------------------------------------------------ IPC round-trips

#[test]
fn fresh_install_reports_embedded_and_acks_as_noop_via_ipc() {
    let fx = Fixture::new();
    let (_app, webview) = build_app(&fx.root, enabled_config(&fx));

    // Mock apps run at package version 0.1.0.
    assert_eq!(
        invoke(&webview, "current_bundle").unwrap(),
        json!({ "source": "embedded", "seq": null, "version": "0.1.0" })
    );
    assert_eq!(
        invoke(&webview, "notify_app_ready").unwrap(),
        json!({ "status": "embeddedNoop" })
    );
}

#[test]
fn download_via_ipc_stages_a_signed_release_and_emits_progress() {
    let fx = Fixture::new();
    let (manifest, archive_path) = fx.publish("0.2.0", "0.1.0");
    let (app, webview) = build_app(&fx.root, enabled_config(&fx));

    let seen: Arc<Mutex<Vec<DownloadProgress>>> = Arc::default();
    let sink = Arc::clone(&seen);
    app.listen_any(PROGRESS_EVENT, move |event| {
        sink.lock()
            .unwrap()
            .push(serde_json::from_str(event.payload()).unwrap());
    });

    // check() first: available, nothing downloaded.
    assert_eq!(
        invoke(&webview, "check").unwrap(),
        json!({
            "status": "available",
            "manifest": {
                "version": "0.2.0",
                "createdAt": manifest.created_at,
                "minShellVersion": "0.1.0",
                "archive": {
                    "url": manifest.archive.url,
                    "sha256": manifest.archive.sha256,
                    "size": manifest.archive.size,
                },
            },
        })
    );
    assert_eq!(fx.server.request_count(&archive_path), 0);

    assert_eq!(
        invoke(&webview, "download").unwrap(),
        json!({ "status": "staged", "seq": 1, "version": "0.2.0" })
    );
    assert_eq!(
        fs::read(fx.bundle_dir(1).join("index.html")).unwrap(),
        b"<html>ota v-next</html>"
    );

    let seen = seen.lock().unwrap();
    assert!(!seen.is_empty(), "progress must be observable from JS");
    assert!(
        seen.windows(2).all(|w| w[0].downloaded < w[1].downloaded),
        "monotonic: {seen:?}"
    );
    assert!(seen.iter().all(|p| p.total == manifest.archive.size));
    let last = seen.last().unwrap();
    assert_eq!(last.downloaded, last.total, "the 100% event is guaranteed");

    // A second check now reports the staged bundle.
    assert_eq!(
        invoke(&webview, "check").unwrap(),
        json!({ "status": "alreadyStaged", "seq": 1, "version": "0.2.0" })
    );
}

#[test]
fn trial_ack_commit_cycle_through_the_command_layer() {
    let fx = Fixture::new();
    fx.publish("0.2.0", "0.1.0");

    // Launch 1: download and stage.
    {
        let (_app, webview) = build_app(&fx.root, enabled_config(&fx));
        assert_eq!(invoke(&webview, "download").unwrap()["status"], "staged");
    }

    // Launch 2 (cold boot): trial serving, then the ack commits.
    {
        let (_app, webview) = build_app(&fx.root, enabled_config(&fx));
        assert_eq!(
            invoke(&webview, "current_bundle").unwrap(),
            json!({ "source": "ota", "seq": 1, "version": "0.2.0" })
        );
        assert_eq!(
            invoke(&webview, "notify_app_ready").unwrap(),
            json!({ "status": "committed", "seq": 1 })
        );
        assert_eq!(
            invoke(&webview, "notify_app_ready").unwrap(),
            json!({ "status": "alreadyCommitted", "seq": 1 }),
            "the ack is idempotent"
        );
    }
    assert_eq!(raw_state(&fx.root)["committed"], 1);

    // Launch 3: steady state.
    let (_app, webview) = build_app(&fx.root, enabled_config(&fx));
    assert_eq!(invoke(&webview, "current_bundle").unwrap()["source"], "ota");
}

#[test]
fn unacked_trial_rolls_back_through_the_command_layer() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("hot-update");
    let config = json!({
        "manifestUrl": "https://updates.example.com/manifest.json",
        "pubkeys": [minisign::KeyPair::generate_unencrypted_keypair().unwrap().pk.to_base64()],
    });
    try_build_app(&root, Some(config.clone())).unwrap(); // boot 0: init state
    stage_bundle(&root, 1, "0.2.0", "sha-1");

    // Trial boot: served, but the app "crashes" — no ack.
    {
        let (_app, webview) = build_app(&root, config.clone());
        assert_eq!(invoke(&webview, "current_bundle").unwrap()["source"], "ota");
    }

    // Two-strike softening (design §4): the first unacked relaunch re-arms the
    // bundle (still serving OTA) instead of blacklisting it.
    {
        let (_app, webview) = build_app(&root, config.clone());
        assert_eq!(invoke(&webview, "current_bundle").unwrap()["source"], "ota");
        assert!(raw_state(&root)["failed"].as_array().unwrap().is_empty());
    }

    // Second unacked boot: rolled back to embedded, archive hash blacklisted.
    let (_app, webview) = build_app(&root, config);
    assert_eq!(
        invoke(&webview, "current_bundle").unwrap(),
        json!({ "source": "embedded", "seq": null, "version": "0.1.0" })
    );
    assert_eq!(raw_state(&root)["failed"][0], "sha-1");
}

#[test]
fn reset_via_ipc_reverts_to_embedded_on_the_next_launch() {
    let fx = Fixture::new();
    fx.publish("0.2.0", "0.1.0");
    {
        let (_app, webview) = build_app(&fx.root, enabled_config(&fx));
        assert_eq!(invoke(&webview, "download").unwrap()["status"], "staged");
    }
    {
        let (_app, webview) = build_app(&fx.root, enabled_config(&fx));
        assert_eq!(
            invoke(&webview, "notify_app_ready").unwrap()["status"],
            "committed"
        );
        assert_eq!(invoke(&webview, "reset").unwrap(), Value::Null);
        // The running process keeps serving what it booted…
        assert_eq!(invoke(&webview, "current_bundle").unwrap()["source"], "ota");
    }
    assert_eq!(raw_state(&fx.root)["committed"], Value::Null);

    // …and the next launch is back on embedded.
    let (_app, webview) = build_app(&fx.root, enabled_config(&fx));
    assert_eq!(
        invoke(&webview, "current_bundle").unwrap()["source"],
        "embedded"
    );
}

// ------------------------------------------------------------- dark-ship

#[test]
fn disabled_plugin_is_inert_but_report_and_ack_commands_stay_total() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("hot-update");
    let (_app, webview) = build_app(&root, json!({ "enabled": false }));

    // Boot code can call these unconditionally.
    assert_eq!(
        invoke(&webview, "current_bundle").unwrap(),
        json!({ "source": "embedded", "seq": null, "version": "0.1.0" })
    );
    assert_eq!(
        invoke(&webview, "notify_app_ready").unwrap(),
        json!({ "status": "embeddedNoop" })
    );
    assert_eq!(invoke(&webview, "reset").unwrap(), Value::Null);

    // Update commands refuse loudly.
    for cmd in ["check", "download"] {
        let err = invoke(&webview, cmd).unwrap_err();
        assert!(
            err.as_str().unwrap().contains("disabled"),
            "{cmd} must explain it is disabled, got {err}"
        );
    }
    assert!(!root.exists(), "a disabled plugin must not touch the disk");
}

// ------------------------------------------------------ config init gate

#[test]
fn missing_plugin_config_aborts_startup() {
    let tmp = TempDir::new().unwrap();
    let err = try_build_app(&tmp.path().join("hot-update"), None).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("plugins.hot-update"), "{msg}");
    assert!(
        msg.contains("enabled"),
        "must point at the dark-ship escape: {msg}"
    );
}

#[test]
fn invalid_config_aborts_startup() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("hot-update");

    let bad_key = json!({
        "manifestUrl": "https://updates.example.com/manifest.json",
        "pubkeys": ["not-a-minisign-key"],
    });
    let msg = try_build_app(&root, Some(bad_key)).unwrap_err().to_string();
    assert!(msg.contains("public key"), "{msg}");

    let query_url = json!({
        "manifestUrl": "https://updates.example.com/manifest.json?v=2",
        "pubkeys": ["irrelevant"],
    });
    let msg = try_build_app(&root, Some(query_url))
        .unwrap_err()
        .to_string();
    assert!(msg.contains("query string"), "{msg}");

    let typo = json!({
        "manifestUrl": "https://updates.example.com/manifest.json",
        "pubKeys": ["RW..."],
    });
    let msg = try_build_app(&root, Some(typo)).unwrap_err().to_string();
    assert!(
        msg.contains("pubKeys"),
        "typos must not be silently ignored: {msg}"
    );
}
