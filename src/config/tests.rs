//! Config deserialization + validation: the init-time gate that turns a bad
//! `plugins.hot-update` entry into a startup error.

use minisign::KeyPair;
use serde_json::json;

use super::*;

/// Deserialize as tauri's plugin initializer does (`serde_json::from_value`
/// on the `plugins.hot-update` value).
fn parse(value: serde_json::Value) -> serde_json::Result<Config> {
    serde_json::from_value(value)
}

fn valid_pubkey() -> String {
    KeyPair::generate_unencrypted_keypair()
        .unwrap()
        .pk
        .to_base64()
}

#[test]
fn full_config_validates_into_the_update_config() {
    let key = valid_pubkey();
    let config = parse(json!({
        "manifestUrl": "https://updates.example.com/manifest.json",
        "pubkeys": [key],
    }))
    .unwrap();
    assert!(config.enabled, "enabled defaults to true");
    let update = config.validate().unwrap().expect("enabled config");
    assert_eq!(
        update.manifest_url,
        "https://updates.example.com/manifest.json"
    );
    assert_eq!(update.pubkeys, vec![key]);
}

#[test]
fn full_minisign_pub_file_contents_are_accepted_as_a_key() {
    let keypair = KeyPair::generate_unencrypted_keypair().unwrap();
    let pub_file = format!(
        "untrusted comment: minisign public key\n{}\n",
        keypair.pk.to_base64()
    );
    let config = parse(json!({
        "manifestUrl": "https://updates.example.com/manifest.json",
        "pubkeys": [pub_file],
    }))
    .unwrap();
    assert!(config.validate().unwrap().is_some());
}

#[test]
fn disabled_config_is_inert_and_never_validated() {
    // Dark-shipping with placeholder garbage must not brick the app.
    let config = parse(json!({
        "enabled": false,
        "manifestUrl": "not a url",
        "pubkeys": ["not a key"],
    }))
    .unwrap();
    assert_eq!(config.validate().unwrap(), None);
    assert_eq!(
        parse(json!({ "enabled": false }))
            .unwrap()
            .validate()
            .unwrap(),
        None
    );
}

#[test]
fn missing_manifest_url_is_an_init_error_when_enabled() {
    let config = parse(json!({ "pubkeys": [valid_pubkey()] })).unwrap();
    let err = config.validate().unwrap_err();
    assert!(
        matches!(&err, Error::Config(msg) if msg.contains("manifestUrl")),
        "{err}"
    );

    let config = parse(json!({ "manifestUrl": "  ", "pubkeys": [valid_pubkey()] })).unwrap();
    assert!(matches!(config.validate(), Err(Error::Config(_))));
}

#[test]
fn non_http_and_query_string_urls_are_init_errors() {
    let config = parse(json!({
        "manifestUrl": "ftp://updates.example.com/manifest.json",
        "pubkeys": [valid_pubkey()],
    }))
    .unwrap();
    assert!(matches!(config.validate(), Err(Error::Config(msg)) if msg.contains("http")));

    let config = parse(json!({
        "manifestUrl": "https://updates.example.com/manifest.json?v=2",
        "pubkeys": [valid_pubkey()],
    }))
    .unwrap();
    assert!(matches!(config.validate(), Err(Error::Config(msg)) if msg.contains("query string")));
}

#[test]
fn missing_or_malformed_pubkeys_are_init_errors() {
    let config = parse(json!({ "manifestUrl": "https://u.example.com/manifest.json" })).unwrap();
    let err = config.validate().unwrap_err();
    assert!(
        matches!(&err, Error::Config(msg) if msg.contains("pubkeys")),
        "{err}"
    );

    // One malformed key among valid ones is still a hard stop: a broken
    // trust anchor must surface, not be silently skipped.
    let config = parse(json!({
        "manifestUrl": "https://u.example.com/manifest.json",
        "pubkeys": [valid_pubkey(), "definitely-not-a-key"],
    }))
    .unwrap();
    assert!(matches!(config.validate(), Err(Error::InvalidPublicKey)));
}

#[test]
fn unknown_config_fields_are_rejected_at_deserialization() {
    // Catches typos like `pubKeys` that would otherwise silently weaken
    // security-relevant config.
    let err = parse(json!({
        "manifestUrl": "https://u.example.com/manifest.json",
        "pubKeys": ["RW..."],
    }))
    .unwrap_err();
    assert!(err.to_string().contains("pubKeys"), "{err}");
}
