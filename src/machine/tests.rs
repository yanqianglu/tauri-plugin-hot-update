//! Exhaustive transition tests for the rollback state machine — every
//! anti-brick scenario from the WP2 plan, as pure state-in/state-out checks.

use std::collections::BTreeSet;

use super::*;

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

fn meta(version: &str, sha: &str) -> BundleMeta {
    BundleMeta {
        version: v(version),
        archive_sha256: sha.to_string(),
    }
}

fn present(seqs: &[u64]) -> BTreeSet<u64> {
    seqs.iter().copied().collect()
}

fn stage_ok(state: State, seq: u64, m: BundleMeta) -> (State, Vec<Effect>) {
    let (state, result) = stage(state, seq, m);
    let effects = result.expect("stage should succeed");
    (state, effects)
}

/// Build a state with `seq` committed at `version` via real transitions
/// (fresh boot → stage → trial boot → ack), embedded at 1.0.0.
fn committed_state(seq: u64, version: &str, sha: &str) -> State {
    let boot = resolve_boot(State::default(), &v("1.0.0"), &present(&[]));
    let (state, _) = stage_ok(boot.state, seq, meta(version, sha));
    let boot = resolve_boot(state, &v("1.0.0"), &present(&[seq]));
    assert_eq!(boot.active, Active::Ota(seq));
    let (state, outcome, _) = ack(boot.state, boot.active);
    assert_eq!(outcome, AckOutcome::Committed(seq));
    state
}

// ---------------------------------------------------------------- fresh boot

#[test]
fn fresh_install_serves_embedded_and_seeds_watermark() {
    let outcome = resolve_boot(State::default(), &v("1.0.0"), &present(&[]));
    assert_eq!(outcome.active, Active::Embedded);
    assert_eq!(outcome.rolled_back, None);
    assert!(outcome.effects.is_empty());
    assert_eq!(outcome.state.max_version_seen, Some(v("1.0.0")));
    assert_eq!(outcome.state, State {
        max_version_seen: Some(v("1.0.0")),
        ..State::default()
    });
}

#[test]
fn boot_with_nothing_pending_is_idempotent() {
    let first = resolve_boot(State::default(), &v("1.0.0"), &present(&[]));
    let second = resolve_boot(first.state.clone(), &v("1.0.0"), &present(&[]));
    assert_eq!(second.state, first.state);
    assert_eq!(second.active, Active::Embedded);
    assert!(second.effects.is_empty());
}

// ------------------------------------------------- staged → booting → commit

#[test]
fn staged_bundle_is_armed_on_next_boot() {
    let boot = resolve_boot(State::default(), &v("1.0.0"), &present(&[]));
    let (state, effects) = stage_ok(boot.state, 1, meta("1.1.0", "sha-1"));
    assert!(effects.is_empty());
    assert_eq!(state.staged, Some(1));

    let boot = resolve_boot(state, &v("1.0.0"), &present(&[1]));
    assert_eq!(boot.active, Active::Ota(1));
    assert_eq!(boot.state.booting, Some(1));
    assert_eq!(boot.state.staged, None, "staged pointer consumed by arming");
    assert_eq!(boot.state.committed, None, "not committed until acked");
    assert!(boot.effects.is_empty());
}

#[test]
fn ack_commits_the_armed_bundle_and_raises_watermark() {
    let boot = resolve_boot(State::default(), &v("1.0.0"), &present(&[]));
    let (state, _) = stage_ok(boot.state, 1, meta("1.1.0", "sha-1"));
    let boot = resolve_boot(state, &v("1.0.0"), &present(&[1]));

    let (state, outcome, effects) = ack(boot.state, boot.active);
    assert_eq!(outcome, AckOutcome::Committed(1));
    assert_eq!(state.committed, Some(1));
    assert_eq!(state.last_good, Some(1));
    assert_eq!(state.booting, None);
    assert_eq!(state.max_version_seen, Some(v("1.1.0")));
    assert!(effects.is_empty(), "nothing replaced on first commit");
    assert!(state.failed.is_empty());
}

#[test]
fn ack_is_idempotent() {
    let state = committed_state(1, "1.1.0", "sha-1");
    let (after, outcome, effects) = ack(state.clone(), Active::Ota(1));
    assert_eq!(outcome, AckOutcome::AlreadyCommitted(1));
    assert_eq!(after, state);
    assert!(effects.is_empty());
}

#[test]
fn ack_while_serving_embedded_is_a_noop() {
    let boot = resolve_boot(State::default(), &v("1.0.0"), &present(&[]));
    let (after, outcome, effects) = ack(boot.state.clone(), Active::Embedded);
    assert_eq!(outcome, AckOutcome::EmbeddedNoop);
    assert_eq!(after, boot.state);
    assert!(effects.is_empty());
}

#[test]
fn ack_replacing_a_previous_commit_deletes_it() {
    let state = committed_state(1, "1.1.0", "sha-1");
    let (state, _) = stage_ok(state, 2, meta("1.2.0", "sha-2"));
    let boot = resolve_boot(state, &v("1.0.0"), &present(&[1, 2]));
    assert_eq!(boot.active, Active::Ota(2));

    let (state, outcome, effects) = ack(boot.state, boot.active);
    assert_eq!(outcome, AckOutcome::Committed(2));
    assert_eq!(state.committed, Some(2));
    assert_eq!(effects, vec![Effect::DeleteBundle(1)]);
    assert!(!state.versions.contains_key(&1), "replaced bundle pruned");
}

// --------------------------------------------------- rollback (no-ack ⇒ fail)

#[test]
fn unacked_trial_boot_rolls_back_and_blacklists_by_hash() {
    let boot = resolve_boot(State::default(), &v("1.0.0"), &present(&[]));
    let (state, _) = stage_ok(boot.state, 1, meta("1.1.0", "sha-1"));
    let boot = resolve_boot(state, &v("1.0.0"), &present(&[1]));
    assert_eq!(boot.state.booting, Some(1));

    // Next boot arrives with `booting` still set: the trial never acked.
    let boot = resolve_boot(boot.state, &v("1.0.0"), &present(&[1]));
    assert_eq!(boot.rolled_back, Some(1));
    assert_eq!(boot.active, Active::Embedded, "no committed bundle to fall back to");
    assert_eq!(boot.state.booting, None);
    assert!(boot.state.failed.contains("sha-1"), "blacklisted by archive hash");
    assert_eq!(boot.effects, vec![Effect::DeleteBundle(1)]);
    assert!(!boot.state.versions.contains_key(&1));
}

#[test]
fn rollback_falls_back_to_the_committed_bundle() {
    let state = committed_state(1, "1.1.0", "sha-1");
    let (state, _) = stage_ok(state, 2, meta("1.2.0", "sha-2"));
    let boot = resolve_boot(state, &v("1.0.0"), &present(&[1, 2]));
    assert_eq!(boot.active, Active::Ota(2));

    let boot = resolve_boot(boot.state, &v("1.0.0"), &present(&[1, 2]));
    assert_eq!(boot.rolled_back, Some(2));
    assert_eq!(boot.active, Active::Ota(1), "last-good keeps serving");
    assert!(boot.state.failed.contains("sha-2"));
    assert_eq!(boot.effects, vec![Effect::DeleteBundle(2)]);
    assert_eq!(boot.state.committed, Some(1));
}

#[test]
fn failed_hash_is_never_staged_again() {
    let state = committed_state(1, "1.1.0", "sha-1");
    let (state, _) = stage_ok(state, 2, meta("1.2.0", "sha-2"));
    let boot = resolve_boot(state, &v("1.0.0"), &present(&[1, 2]));
    let boot = resolve_boot(boot.state, &v("1.0.0"), &present(&[1, 2]));
    assert!(boot.state.failed.contains("sha-2"));

    // The same archive under a new seq is refused; a *fixed* deploy ships
    // under a new hash and passes.
    let (state, result) = stage(boot.state, 3, meta("1.2.0", "sha-2"));
    assert_eq!(result, Err(StageError::HashBlacklisted("sha-2".into())));
    let (_, result) = stage(state, 3, meta("1.2.0", "sha-2-fixed"));
    assert!(result.is_ok());
}

#[test]
fn boot_never_arms_a_blacklisted_hash() {
    // Defense in depth: even if a staged pointer references a blacklisted
    // archive (downloader bug, hand-edited state), arming refuses it.
    let mut state = committed_state(1, "1.1.0", "sha-1");
    state.staged = Some(2);
    state.versions.insert(2, meta("1.2.0", "sha-bad"));
    state.failed.insert("sha-bad".to_string());

    let boot = resolve_boot(state, &v("1.0.0"), &present(&[1, 2]));
    assert_eq!(boot.active, Active::Ota(1));
    assert_eq!(boot.state.booting, None);
    assert_eq!(boot.effects, vec![Effect::DeleteBundle(2)]);
}

#[test]
fn crash_loop_converges_to_committed() {
    let mut state = committed_state(1, "1.1.0", "sha-1");
    for (seq, sha) in [(2, "sha-2"), (3, "sha-3")] {
        let (s, _) = stage_ok(state, seq, meta(&format!("1.{seq}.0"), sha));
        let armed = resolve_boot(s, &v("1.0.0"), &present(&[1, seq]));
        assert_eq!(armed.active, Active::Ota(seq));
        // Crash: no ack. Next boot rolls back.
        let rolled = resolve_boot(armed.state, &v("1.0.0"), &present(&[1, seq]));
        assert_eq!(rolled.active, Active::Ota(1));
        state = rolled.state;
    }
    assert!(state.failed.contains("sha-2") && state.failed.contains("sha-3"));

    // With nothing staged the state is now a fixed point.
    let again = resolve_boot(state.clone(), &v("1.0.0"), &present(&[1]));
    assert_eq!(again.active, Active::Ota(1));
    assert_eq!(again.state, state);
    assert!(again.effects.is_empty());
}

#[test]
fn crash_loop_with_no_committed_converges_to_embedded() {
    let boot = resolve_boot(State::default(), &v("1.0.0"), &present(&[]));
    let (state, _) = stage_ok(boot.state, 1, meta("1.1.0", "sha-1"));
    let armed = resolve_boot(state, &v("1.0.0"), &present(&[1]));
    let rolled = resolve_boot(armed.state, &v("1.0.0"), &present(&[1]));
    assert_eq!(rolled.active, Active::Embedded);

    let again = resolve_boot(rolled.state.clone(), &v("1.0.0"), &present(&[]));
    assert_eq!(again.active, Active::Embedded);
    assert_eq!(again.state, rolled.state);
}

#[test]
fn rollback_of_a_seq_without_metadata_does_not_panic() {
    let state = State {
        booting: Some(9), // corrupt-ish: no versions entry for 9
        ..State::default()
    };
    let boot = resolve_boot(state, &v("1.0.0"), &present(&[]));
    assert_eq!(boot.rolled_back, Some(9));
    assert_eq!(boot.active, Active::Embedded);
    assert!(boot.state.failed.is_empty(), "no hash known, nothing to blacklist");
}

// --------------------------------- download finishing during a trial session

#[test]
fn ack_commits_the_booted_seq_not_a_freshly_staged_one() {
    let boot = resolve_boot(State::default(), &v("1.0.0"), &present(&[]));
    let (state, _) = stage_ok(boot.state, 1, meta("1.1.0", "sha-1"));
    let boot = resolve_boot(state, &v("1.0.0"), &present(&[1]));
    assert_eq!(boot.active, Active::Ota(1));

    // A download completes mid-session and stages seq 2.
    let (state, effects) = stage_ok(boot.state, 2, meta("1.2.0", "sha-2"));
    assert!(effects.is_empty());
    assert_eq!(state.booting, Some(1));
    assert_eq!(state.staged, Some(2));

    // The ack must commit seq 1 (the in-memory booted seq) and leave the
    // fresh staged bundle for its own trial boot.
    let (state, outcome, _) = ack(state, boot.active);
    assert_eq!(outcome, AckOutcome::Committed(1));
    assert_eq!(state.committed, Some(1));
    assert_eq!(state.staged, Some(2), "fresh download stays staged");

    let boot = resolve_boot(state, &v("1.0.0"), &present(&[1, 2]));
    assert_eq!(boot.active, Active::Ota(2), "staged bundle gets its own trial");
}

// ------------------------------------------------- embedded-newer-than-OTA

#[test]
fn store_update_newer_than_committed_ota_discards_it() {
    let state = committed_state(1, "1.1.0", "sha-1");
    let boot = resolve_boot(state, &v("1.2.0"), &present(&[1]));
    assert_eq!(boot.active, Active::Embedded);
    assert_eq!(boot.state.committed, None);
    assert_eq!(boot.state.last_good, None);
    assert_eq!(boot.effects, vec![Effect::DeleteBundle(1)]);
    assert_eq!(boot.state.max_version_seen, Some(v("1.2.0")));
}

#[test]
fn store_update_equal_to_committed_ota_discards_it() {
    // A store release at the same version wins: never shadow it.
    let state = committed_state(1, "1.1.0", "sha-1");
    let boot = resolve_boot(state, &v("1.1.0"), &present(&[1]));
    assert_eq!(boot.active, Active::Embedded);
    assert_eq!(boot.effects, vec![Effect::DeleteBundle(1)]);
}

#[test]
fn older_embedded_keeps_serving_the_committed_ota() {
    let state = committed_state(1, "1.1.0", "sha-1");
    let boot = resolve_boot(state, &v("1.0.5"), &present(&[1]));
    assert_eq!(boot.active, Active::Ota(1));
    assert_eq!(boot.state.committed, Some(1));
    assert!(boot.effects.is_empty());
}

#[test]
fn store_update_newer_than_staged_discards_it_before_arming() {
    let boot = resolve_boot(State::default(), &v("1.0.0"), &present(&[]));
    let (state, _) = stage_ok(boot.state, 1, meta("1.1.0", "sha-1"));

    // Store update to 1.1.0 lands before the next boot: staged 1.1.0 is not newer.
    let boot = resolve_boot(state, &v("1.1.0"), &present(&[1]));
    assert_eq!(boot.active, Active::Embedded);
    assert_eq!(boot.state.booting, None);
    assert_eq!(boot.effects, vec![Effect::DeleteBundle(1)]);
}

#[test]
fn staged_ota_newer_than_the_store_update_still_arms() {
    let boot = resolve_boot(State::default(), &v("1.0.0"), &present(&[]));
    let (state, _) = stage_ok(boot.state, 1, meta("1.3.0", "sha-1"));
    let boot = resolve_boot(state, &v("1.2.0"), &present(&[1]));
    assert_eq!(boot.active, Active::Ota(1));
}

// ------------------------------------------------------ watermark monotonicity

#[test]
fn watermark_never_goes_down() {
    let boot = resolve_boot(State::default(), &v("1.3.0"), &present(&[]));
    assert_eq!(boot.state.max_version_seen, Some(v("1.3.0")));

    // A boot under an older embedded version (should not happen, but the
    // watermark must be monotonic regardless).
    let boot = resolve_boot(boot.state, &v("1.2.0"), &present(&[]));
    assert_eq!(boot.state.max_version_seen, Some(v("1.3.0")));

    // Committing raises it to the bundle version.
    let (state, _) = stage_ok(boot.state, 1, meta("1.5.0", "sha-1"));
    let boot = resolve_boot(state, &v("1.2.0"), &present(&[1]));
    let (state, _, _) = ack(boot.state, boot.active);
    assert_eq!(state.max_version_seen, Some(v("1.5.0")));

    let boot = resolve_boot(state, &v("1.4.0"), &present(&[1]));
    assert_eq!(boot.state.max_version_seen, Some(v("1.5.0")));
}

#[test]
fn stage_refuses_versions_at_or_below_the_watermark() {
    // Downgrade replay: an old validly-signed manifest must be refused.
    let state = committed_state(1, "1.3.0", "sha-1");
    let (state, result) = stage(state, 2, meta("1.3.0", "sha-2"));
    assert!(matches!(result, Err(StageError::VersionNotNewer { .. })));
    let (state, result) = stage(state, 2, meta("1.2.9", "sha-3"));
    assert!(matches!(result, Err(StageError::VersionNotNewer { .. })));
    let (state, result) = stage(state, 2, meta("1.3.1", "sha-4"));
    assert!(result.is_ok());
    assert_eq!(state.staged, Some(2));
}

// ------------------------------------------------------------ staging details

#[test]
fn staging_over_a_previous_staged_bundle_orphans_it() {
    let boot = resolve_boot(State::default(), &v("1.0.0"), &present(&[]));
    let (state, _) = stage_ok(boot.state, 1, meta("1.1.0", "sha-1"));
    let (state, effects) = stage_ok(state, 2, meta("1.2.0", "sha-2"));
    assert_eq!(state.staged, Some(2));
    assert_eq!(effects, vec![Effect::DeleteBundle(1)]);
    assert!(!state.versions.contains_key(&1));
}

#[test]
fn stage_refuses_a_seq_already_in_use() {
    let state = committed_state(1, "1.1.0", "sha-1");
    let (_, result) = stage(state, 1, meta("1.2.0", "sha-2"));
    assert_eq!(result, Err(StageError::SeqInUse(1)));
}

// ----------------------------------------------------- disk-truth validation

#[test]
fn orphan_dirs_are_ignored_and_garbage_collected() {
    // A kill between extraction and the state write leaves a dir with no
    // pointer to it: never served, swept at boot.
    let boot = resolve_boot(State::default(), &v("1.0.0"), &present(&[5]));
    assert_eq!(boot.active, Active::Embedded);
    assert_eq!(boot.effects, vec![Effect::DeleteBundle(5)]);
}

#[test]
fn staged_pointer_without_a_dir_is_discarded() {
    let boot = resolve_boot(State::default(), &v("1.0.0"), &present(&[]));
    let (state, _) = stage_ok(boot.state, 1, meta("1.1.0", "sha-1"));
    let boot = resolve_boot(state, &v("1.0.0"), &present(&[]));
    assert_eq!(boot.active, Active::Embedded);
    assert_eq!(boot.state.booting, None);
    assert!(!boot.state.versions.contains_key(&1));
}

#[test]
fn committed_pointer_without_a_dir_falls_back_to_embedded() {
    let state = committed_state(1, "1.1.0", "sha-1");
    let boot = resolve_boot(state, &v("1.0.0"), &present(&[]));
    assert_eq!(boot.active, Active::Embedded);
    assert_eq!(boot.state.committed, None);
    assert!(boot.effects.is_empty(), "nothing on disk to delete");
}

// --------------------------------------------------------------------- reset

#[test]
fn reset_returns_to_factory_state_but_spares_the_active_dir() {
    let state = committed_state(1, "1.1.0", "sha-1");
    let (state, _) = stage_ok(state, 2, meta("1.2.0", "sha-2"));

    let (fresh, effects) = reset(state, &v("1.0.0"), Active::Ota(1));
    assert_eq!(fresh, State {
        max_version_seen: Some(v("1.0.0")),
        ..State::default()
    });
    // Seq 1 is being served right now — deleting it under the webview would
    // tear the UI; the next boot's GC sweeps it instead.
    assert_eq!(effects, vec![Effect::DeleteBundle(2)]);

    // Next boot: factory behavior, leftover dir swept.
    let boot = resolve_boot(fresh, &v("1.0.0"), &present(&[1]));
    assert_eq!(boot.active, Active::Embedded);
    assert_eq!(boot.effects, vec![Effect::DeleteBundle(1)]);
}

#[test]
fn reset_while_serving_embedded_deletes_everything() {
    let boot = resolve_boot(State::default(), &v("1.0.0"), &present(&[]));
    let (state, _) = stage_ok(boot.state, 1, meta("1.1.0", "sha-1"));
    let (fresh, effects) = reset(state, &v("1.0.0"), Active::Embedded);
    assert_eq!(effects, vec![Effect::DeleteBundle(1)]);
    assert_eq!(fresh.staged, None);
}

#[test]
fn ack_after_a_mid_session_reset_is_refused() {
    // reset() rewrote state while a trial boot was live: the ack no longer
    // matches and must not resurrect the bundle.
    let boot = resolve_boot(State::default(), &v("1.0.0"), &present(&[]));
    let (state, _) = stage_ok(boot.state, 1, meta("1.1.0", "sha-1"));
    let boot = resolve_boot(state, &v("1.0.0"), &present(&[1]));
    assert_eq!(boot.active, Active::Ota(1));

    let (fresh, _) = reset(boot.state, &v("1.0.0"), boot.active);
    let (after, outcome, effects) = ack(fresh.clone(), boot.active);
    assert_eq!(outcome, AckOutcome::Stale(1));
    assert_eq!(after, fresh);
    assert!(effects.is_empty());
}
