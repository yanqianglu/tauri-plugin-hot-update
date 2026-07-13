//! Filesystem tests for the store: real files in tempdirs, no mocks.

use std::fs;

use semver::Version;
use tempfile::TempDir;

use super::*;
use crate::machine::{stage, BundleMeta, State};

fn store() -> (TempDir, Store) {
    let tmp = TempDir::new().unwrap();
    let store = Store::new(tmp.path().join("hot-update"));
    (tmp, store)
}

fn populated_state() -> State {
    let mut state = State {
        committed: Some(2),
        last_good: Some(2),
        staged: Some(3),
        booting: None,
        // Non-default so the roundtrip/wire-format tests actually exercise the
        // field (a 0 would roundtrip even if serialization dropped it).
        booting_strikes: 1,
        max_version_seen: Some(Version::parse("1.2.0").unwrap()),
        ..State::default()
    };
    state.failed.insert("sha-dead".to_string());
    for (seq, version, sha) in [(2u64, "1.2.0", "sha-2"), (3, "1.3.0", "sha-3")] {
        state.versions.insert(
            seq,
            BundleMeta {
                version: Version::parse(version).unwrap(),
                archive_sha256: sha.to_string(),
            },
        );
    }
    state
}

// -------------------------------------------------------------- persistence

#[test]
fn save_load_roundtrip_preserves_every_field() {
    let (_tmp, store) = store();
    let state = populated_state();
    store.save_state(&state).unwrap();
    assert_eq!(store.load_state(), state);
}

#[test]
fn state_json_uses_the_designed_camel_case_wire_format() {
    // Locks the on-disk schema to the design doc; breaking this test means
    // breaking every installed app's state file.
    let (_tmp, store) = store();
    store.save_state(&populated_state()).unwrap();

    let raw = fs::read(store.root().join("state.json")).unwrap();
    let json: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    let obj = json.as_object().unwrap();
    for key in [
        "committed",
        "lastGood",
        "staged",
        "booting",
        "bootingStrikes",
        "failed",
        "maxVersionSeen",
        "versions",
    ] {
        assert!(obj.contains_key(key), "missing key {key} in {json}");
    }
    assert_eq!(json["committed"], 2);
    assert_eq!(json["bootingStrikes"], 1);
    assert_eq!(json["maxVersionSeen"], "1.2.0");
    assert_eq!(json["versions"]["2"]["version"], "1.2.0");
    assert_eq!(json["versions"]["2"]["archiveSha256"], "sha-2");
    assert_eq!(json["failed"][0], "sha-dead");
}

#[test]
fn missing_state_file_loads_fresh() {
    let (_tmp, store) = store();
    assert_eq!(store.load_state(), State::default());
}

#[test]
fn corrupt_state_file_loads_fresh_without_panicking() {
    let (_tmp, store) = store();
    fs::create_dir_all(store.root()).unwrap();
    for garbage in [
        &b"not json at all {{{"[..],
        b"",
        b"\x00\xff\xfe binary",
        br#"{"committed": "not-a-number"}"#,
        br#"{"maxVersionSeen": "not-a-semver"}"#,
    ] {
        fs::write(store.root().join("state.json"), garbage).unwrap();
        assert_eq!(store.load_state(), State::default(), "garbage: {garbage:?}");
    }
}

#[test]
fn truncated_state_file_loads_fresh() {
    let (_tmp, store) = store();
    store.save_state(&populated_state()).unwrap();
    let full = fs::read(store.root().join("state.json")).unwrap();
    fs::write(store.root().join("state.json"), &full[..full.len() / 2]).unwrap();
    assert_eq!(store.load_state(), State::default());
}

#[test]
fn unknown_fields_from_newer_versions_are_tolerated() {
    let (_tmp, store) = store();
    fs::create_dir_all(store.root()).unwrap();
    fs::write(
        store.root().join("state.json"),
        br#"{"committed": 4, "futureField": {"a": 1}}"#,
    )
    .unwrap();
    assert_eq!(store.load_state().committed, Some(4));
}

#[test]
fn pre_two_strike_state_without_booting_strikes_loads_as_zero() {
    // An app upgraded from a pre-2-strike plugin has a state.json with no
    // `bootingStrikes` key. It must deserialize with the field defaulted to 0
    // (a clean trial), never a parse failure that wipes a committed bundle.
    let (_tmp, store) = store();
    fs::create_dir_all(store.root()).unwrap();
    fs::write(
        store.root().join("state.json"),
        br#"{
            "committed": 2,
            "lastGood": 2,
            "staged": null,
            "booting": 3,
            "failed": ["sha-dead"],
            "maxVersionSeen": "1.2.0",
            "versions": {"3": {"version": "1.3.0", "archiveSha256": "sha-3"}}
        }"#,
    )
    .unwrap();
    let state = store.load_state();
    assert_eq!(state.booting_strikes, 0, "missing field defaults to a clean trial");
    assert_eq!(state.committed, Some(2), "the rest of the state still parses");
    assert_eq!(state.booting, Some(3));
    assert!(state.failed.contains("sha-dead"));
}

#[test]
fn save_replaces_atomically_and_leaves_no_temp_file() {
    let (_tmp, store) = store();
    store.save_state(&State::default()).unwrap();
    store.save_state(&populated_state()).unwrap();
    assert_eq!(store.load_state(), populated_state());
    let leftovers: Vec<_> = fs::read_dir(store.root())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "state.json" && name != "bundles")
        .collect();
    assert!(leftovers.is_empty(), "unexpected files: {leftovers:?}");
}

#[test]
fn update_persists_the_transition_result() {
    let (_tmp, store) = store();
    let meta = BundleMeta {
        version: Version::parse("1.1.0").unwrap(),
        archive_sha256: "sha-1".to_string(),
    };
    let result = store
        .update(|state| stage(state, 1, meta.clone()))
        .unwrap();
    assert!(result.is_ok());
    let reloaded = store.load_state();
    assert_eq!(reloaded.staged, Some(1));
    assert_eq!(reloaded.versions.get(&1), Some(&meta));
}

// ------------------------------------------------------------- bundle layout

#[test]
fn bundle_dir_follows_the_seq_layout() {
    let (_tmp, store) = store();
    assert_eq!(store.bundle_dir(7), store.root().join("bundles").join("seq-7"));
}

#[test]
fn present_seqs_sees_only_real_seq_dirs() {
    let (_tmp, store) = store();
    for seq in [1u64, 12] {
        fs::create_dir_all(store.bundle_dir(seq)).unwrap();
    }
    let bundles = store.root().join("bundles");
    fs::create_dir_all(bundles.join("seq-x")).unwrap(); // unparseable
    fs::create_dir_all(bundles.join("tmp-extract")).unwrap(); // foreign dir
    fs::write(bundles.join("seq-9"), b"a FILE, not a dir").unwrap();

    assert_eq!(store.present_seqs(), [1, 12].into_iter().collect());
    let foreign: Vec<String> = store
        .foreign_entries()
        .into_iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(foreign.len(), 3, "seq-x, tmp-extract, and the seq-9 file: {foreign:?}");
}

#[test]
fn present_seqs_is_empty_without_a_bundles_dir() {
    let (_tmp, store) = store();
    assert!(store.present_seqs().is_empty());
    assert!(store.foreign_entries().is_empty());
}

#[test]
fn allocate_seq_never_reuses_disk_or_state_seqs() {
    let (_tmp, store) = store();
    assert_eq!(store.allocate_seq(&State::default()), 1);

    // An orphan dir from a crashed extraction must not be aliased by the
    // next download, even though no state references it.
    fs::create_dir_all(store.bundle_dir(7)).unwrap();
    assert_eq!(store.allocate_seq(&State::default()), 8);

    let state = State {
        staged: Some(9),
        ..State::default()
    };
    assert_eq!(store.allocate_seq(&state), 10);
}

#[test]
fn apply_effects_deletes_bundle_dirs_best_effort() {
    let (_tmp, store) = store();
    fs::create_dir_all(store.bundle_dir(1)).unwrap();
    fs::write(store.bundle_dir(1).join("index.html"), b"x").unwrap();

    store.apply_effects(&[Effect::DeleteBundle(1), Effect::DeleteBundle(42)]);
    assert!(!store.bundle_dir(1).exists());
    // Deleting a nonexistent dir (42) must not panic or error.
}

#[test]
fn sweep_removes_foreign_debris_but_keeps_seq_dirs() {
    let (_tmp, store) = store();
    fs::create_dir_all(store.bundle_dir(1)).unwrap();
    let bundles = store.root().join("bundles");
    fs::create_dir_all(bundles.join("tmp-extract")).unwrap();
    fs::write(bundles.join("stray.tar.gz"), b"debris").unwrap();

    store.sweep_foreign_entries();
    assert!(store.bundle_dir(1).exists());
    assert!(!bundles.join("tmp-extract").exists());
    assert!(!bundles.join("stray.tar.gz").exists());
}
