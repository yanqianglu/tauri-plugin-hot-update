//! The rollback state machine, as pure Rust.
//!
//! Every transition takes the current [`State`] plus an event and returns the
//! new state together with [`Effect`]s for the I/O shell ([`crate::store`],
//! [`crate::runtime`]) to execute. **No I/O happens here** — that is what
//! makes every anti-brick scenario unit-testable.
//!
//! The three-state pointer (design doc, "State machine"):
//!
//! ```text
//! download ──▶ staged ──(next cold boot, persisted BEFORE serving)──▶ booting
//!                            booting ──(notifyAppReady ack)──────▶ committed
//!                            booting ──(1st boot, no ack)──▶ booting (re-armed)
//!                            booting ──(2nd boot, no ack)──▶ failed (by archive sha256)
//! ```
//!
//! Invariants enforced here:
//! - Absence of the ack **is** the failure signal (no boot timer): a `booting`
//!   pointer found at boot means the previous launch never acked. Desktop
//!   launches are rare and sessions long, so a single miss only *re-arms* the
//!   same bundle for one more trial; a *second* consecutive unacked launch
//!   blacklists its archive sha256 and serving falls back to
//!   committed/embedded. A bundle a newer embedded frontend has caught up to
//!   is discarded outright (not blacklisted) — the embedded frontend wins.
//! - `max_version_seen` is a monotonic watermark (downgrade-replay and
//!   embedded-newer-than-OTA protection). It only ever rises.
//! - Failed archive hashes are never staged or armed again.
//! - The ack commits the seq the process actually booted (passed in by the
//!   caller from memory), never a value re-read from disk — a download that
//!   finishes mid-session must not be committed untried.

use std::collections::{BTreeMap, BTreeSet};

use semver::Version;
use serde::{Deserialize, Serialize};

/// Per-bundle metadata recorded at staging time.
///
/// `archive_sha256` is the identity used for the failure blacklist: a fixed
/// deploy ships under a new hash, so blacklisting by hash never blocks a fix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleMeta {
    pub version: Version,
    pub archive_sha256: String,
}

/// The persisted contents of `state.json`.
///
/// Serialized as camelCase per the design doc. Unknown fields from newer
/// plugin versions are ignored on read (forward compatibility); a file that
/// fails to parse at all is treated as fresh by the store (never a panic).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct State {
    /// The last bundle that completed a trial boot and was acked.
    pub committed: Option<u64>,
    /// Last-known-good bundle. Moves together with `committed` in v1; kept
    /// as a separate pointer per the design for future recovery flows.
    pub last_good: Option<u64>,
    /// Downloaded and extracted, waiting for the next cold boot.
    pub staged: Option<u64>,
    /// Armed for a trial boot. Present at boot time ⇒ the previous trial
    /// never acked.
    pub booting: Option<u64>,
    /// Consecutive unacked launches of the current `booting` bundle. Desktop
    /// launches are rare and sessions long, so a *good* bundle whose process
    /// quit before `notifyAppReady()` must not be blacklisted on a single
    /// miss: the bundle is re-armed once (strike 1) and only blacklisted after
    /// a *second* consecutive unacked launch (strike 2). Reset to 0 on commit
    /// and whenever a fresh bundle is promoted from `staged`. `#[serde(default)]`
    /// so a pre-2-strike `state.json` (which lacks the field) loads as 0.
    #[serde(default)]
    pub booting_strikes: u32,
    /// Blacklist of archive sha256 hashes that failed a trial boot.
    pub failed: BTreeSet<String>,
    /// Monotonic watermark: max(embedded version, highest committed OTA
    /// version). Manifests/bundles at or below it are refused.
    pub max_version_seen: Option<Version>,
    /// seq → metadata for every bundle the state still references.
    pub versions: BTreeMap<u64, BundleMeta>,
}

impl State {
    /// Seqs the state still references after a transition — everything else
    /// on disk is garbage.
    fn referenced(&self) -> BTreeSet<u64> {
        [self.committed, self.last_good, self.staged, self.booting]
            .into_iter()
            .flatten()
            .collect()
    }
}

/// Which source the provider serves this process lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Active {
    Embedded,
    Ota(u64),
}

/// Side effects for the shell to execute after persisting the new state.
/// Deletions are best-effort: a failure leaves an orphan dir that the next
/// boot's GC removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    DeleteBundle(u64),
}

/// Result of boot-time resolution.
#[derive(Debug)]
pub struct BootOutcome {
    pub state: State,
    /// What this process must serve. The shell must persist `state` BEFORE
    /// serving a single byte when this is a newly armed bundle
    /// (`state.booting`), otherwise a crash would evade rollback detection.
    pub active: Active,
    /// Seq blamed for the previous unacked trial boot, if any.
    pub rolled_back: Option<u64>,
    pub effects: Vec<Effect>,
}

/// Boot-time resolution: rollback detection, watermark fold-in,
/// embedded-newer-than-OTA discard, staged→booting promotion, GC.
///
/// `present` is the set of bundle dirs that actually exist on disk; pointers
/// to absent dirs are dropped rather than armed/served.
pub fn resolve_boot(mut state: State, embedded_version: &Version, present: &BTreeSet<u64>) -> BootOutcome {
    // 1. Unacked trial boot from a previous launch. The bundle pointed at by
    //    `booting` was armed last launch and never acked. Two-strike softening
    //    (design §4, "desktop-hardened rollback"): desktop launches are rare
    //    and a good bundle is often quit before `notifyAppReady()`, so a
    //    single miss must not condemn it.
    let mut rolled_back = None;
    if let Some(seq) = state.booting.take() {
        let meta = state.versions.get(&seq).cloned();
        let superseded = meta
            .as_ref()
            .is_some_and(|m| m.version <= *embedded_version);

        if superseded {
            // A newer embedded frontend (e.g. a Sparkle/native update that
            // landed while this trial was mid-flight) has caught up to the
            // trial bundle. It is stale, not broken: discard it (GC sweeps the
            // dir) and let embedded win — no blacklist, not a failure rollback.
            state.booting_strikes = 0;
        } else if state.booting_strikes == 0 && present.contains(&seq) && meta.is_some() {
            // Strike 1: still applicable and on disk. Give it exactly one more
            // launch to ack before condemning it. `booting` stays occupied, so
            // step 4 will not promote a waiting staged bundle over it.
            state.booting = Some(seq);
            state.booting_strikes = 1;
        } else {
            // Strike 2 (a genuine second unacked launch), a bundle whose dir
            // vanished, or a corrupt pointer with no metadata: this bundle can
            // never be trusted again. Blacklist its archive sha256 when known
            // (a fixed redeploy ships under a new hash and is unaffected) and
            // roll back to committed/embedded.
            if let Some(m) = &meta {
                state.failed.insert(m.archive_sha256.clone());
            }
            state.booting_strikes = 0;
            rolled_back = Some(seq);
        }
    }

    // 2. Fold the embedded version into the monotonic watermark.
    state.max_version_seen = Some(match state.max_version_seen.take() {
        Some(seen) => seen.max(embedded_version.clone()),
        None => embedded_version.clone(),
    });

    // 3. Embedded-newer-than-OTA discard: a store update whose embedded
    //    frontend is at least as new as the committed OTA bundle wins, and a
    //    committed pointer whose dir vanished cannot be served.
    let ota_beats_embedded = |seq: u64, state: &State| -> bool {
        present.contains(&seq)
            && state
                .versions
                .get(&seq)
                .is_some_and(|meta| meta.version > *embedded_version)
    };
    if let Some(seq) = state.committed {
        if !ota_beats_embedded(seq, &state) {
            state.committed = None;
        }
    }
    if let Some(seq) = state.last_good {
        if !ota_beats_embedded(seq, &state) {
            state.last_good = None;
        }
    }

    // 4. Staged promotion, re-gated against the *updated* watermark and the
    //    blacklist. The download gates already checked these, but arming is
    //    the last line of defense and the situation can legitimately change
    //    between staging and boot (e.g. a store update raised the watermark).
    //    Skipped while `booting` is still occupied by a strike-1 re-arm: the
    //    queued bundle stays staged and waits for the next boot rather than
    //    jumping ahead of the bundle currently on its second trial.
    let promote = if state.booting.is_some() {
        None
    } else {
        state.staged.take()
    };
    if let Some(seq) = promote {
        let arm = present.contains(&seq)
            && state.versions.get(&seq).is_some_and(|meta| {
                !state.failed.contains(&meta.archive_sha256)
                    && state
                        .max_version_seen
                        .as_ref()
                        .map_or(true, |seen| meta.version > *seen)
            });
        if arm {
            state.booting = Some(seq);
            // A freshly promoted bundle starts its trial with a clean strike
            // count, even if a leftover count somehow survived a prior commit.
            state.booting_strikes = 0;
        }
        // Not armable ⇒ silently discarded; GC removes the dir.
    }

    // 5. Active source for this process lifetime.
    let active = match state.booting.or(state.committed) {
        Some(seq) => Active::Ota(seq),
        None => Active::Embedded,
    };

    // 6. GC: every seq on disk or in the versions map that the final state
    //    no longer references is garbage (orphans from a kill between
    //    extraction and state write, rolled-back bundles, discarded staged).
    let referenced = state.referenced();
    let version_seqs: BTreeSet<u64> = state.versions.keys().copied().collect();
    let mut effects = Vec::new();
    for seq in present.union(&version_seqs).copied() {
        if !referenced.contains(&seq) {
            if present.contains(&seq) {
                effects.push(Effect::DeleteBundle(seq));
            }
            state.versions.remove(&seq);
        }
    }

    BootOutcome {
        state,
        active,
        rolled_back,
        effects,
    }
}

/// Result of an ack ([`ack`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckOutcome {
    /// The trial bundle is now committed and last-good.
    Committed(u64),
    /// Steady state: the booted bundle was already committed (every launch
    /// calls `notifyAppReady`, and the call is idempotent).
    AlreadyCommitted(u64),
    /// Serving embedded — nothing to commit.
    EmbeddedNoop,
    /// The booted seq no longer matches the on-disk `booting` pointer (e.g.
    /// a `reset()` ran mid-session). Refusing to commit is the safe answer.
    Stale(u64),
}

/// `notifyAppReady`: commit the bundle this process booted.
///
/// `booted` MUST be the in-memory value captured at boot resolution — never
/// re-read from disk, because a download finishing mid-session may have
/// re-written `staged` (and `staged` must stay untouched for the next boot).
pub fn ack(mut state: State, booted: Active) -> (State, AckOutcome, Vec<Effect>) {
    let seq = match booted {
        Active::Embedded => return (state, AckOutcome::EmbeddedNoop, Vec::new()),
        Active::Ota(seq) => seq,
    };
    if state.booting != Some(seq) {
        let outcome = if state.committed == Some(seq) {
            AckOutcome::AlreadyCommitted(seq)
        } else {
            AckOutcome::Stale(seq)
        };
        return (state, outcome, Vec::new());
    }

    state.booting = None;
    // A bundle that acks once is good: clear any strike accumulated during a
    // re-armed trial so it can never contaminate the next bundle's trial.
    state.booting_strikes = 0;
    let previous = state.committed.replace(seq);
    state.last_good = Some(seq);
    if let Some(meta) = state.versions.get(&seq) {
        state.max_version_seen = Some(match state.max_version_seen.take() {
            Some(seen) => seen.max(meta.version.clone()),
            None => meta.version.clone(),
        });
    }

    // The bundle this one replaces is no longer referenced (unless a pointer
    // still names it, e.g. it is also the staged seq in a corrupt file).
    let mut effects = Vec::new();
    if let Some(prev) = previous {
        if !state.referenced().contains(&prev) {
            state.versions.remove(&prev);
            effects.push(Effect::DeleteBundle(prev));
        }
    }

    (state, AckOutcome::Committed(seq), effects)
}

/// Why a staging request was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StageError {
    #[error("bundle version {version} is not newer than the watermark {watermark}")]
    VersionNotNewer { version: Version, watermark: Version },
    #[error("archive sha256 {0} previously failed a trial boot and is blacklisted")]
    HashBlacklisted(String),
    #[error("seq {0} is already referenced by the state")]
    SeqInUse(u64),
}

/// Stage a freshly extracted bundle (the WP3 downloader's final step).
///
/// Gates: the version must be strictly above the watermark (downgrade-replay
/// protection) and the archive hash must not be blacklisted. Replacing an
/// earlier staged-but-never-armed bundle orphans it (delete effect).
pub fn stage(
    mut state: State,
    seq: u64,
    meta: BundleMeta,
) -> (State, Result<Vec<Effect>, StageError>) {
    if let Some(watermark) = &state.max_version_seen {
        if meta.version <= *watermark {
            let err = StageError::VersionNotNewer {
                version: meta.version,
                watermark: watermark.clone(),
            };
            return (state, Err(err));
        }
    }
    if state.failed.contains(&meta.archive_sha256) {
        let err = StageError::HashBlacklisted(meta.archive_sha256);
        return (state, Err(err));
    }
    if state.referenced().contains(&seq) || state.versions.contains_key(&seq) {
        return (state, Err(StageError::SeqInUse(seq)));
    }

    let mut effects = Vec::new();
    if let Some(prev) = state.staged.replace(seq) {
        state.versions.remove(&prev);
        effects.push(Effect::DeleteBundle(prev));
    }
    state.versions.insert(seq, meta);
    (state, Ok(effects))
}

/// `reset()`: the debug/support escape hatch — factory state, embedded
/// serving, watermark restarted from the embedded version.
///
/// The bundle the process is *currently serving* (if any) is deliberately not
/// deleted — yanking files out from under the running webview could tear the
/// UI. It is unreferenced by the fresh state, so the next boot's GC sweeps it.
pub fn reset(state: State, embedded_version: &Version, active: Active) -> (State, Vec<Effect>) {
    let keep = match active {
        Active::Embedded => None,
        Active::Ota(seq) => Some(seq),
    };
    let effects = state
        .versions
        .keys()
        .copied()
        .filter(|seq| Some(*seq) != keep)
        .map(Effect::DeleteBundle)
        .collect();
    let fresh = State {
        max_version_seen: Some(embedded_version.clone()),
        ..State::default()
    };
    (fresh, effects)
}

#[cfg(test)]
mod tests;
