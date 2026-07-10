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
//!                                 booting ──(notifyAppReady ack)──▶ committed
//!                                 booting ──(next boot, no ack)───▶ failed (by archive sha256)
//! ```
//!
//! Invariants enforced here:
//! - Absence of the ack **is** the failure signal: a `booting` pointer found
//!   at boot means the previous launch never acked → that bundle's archive
//!   sha256 is blacklisted and serving falls back to committed/embedded.
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
    /// never acked ⇒ rollback.
    pub booting: Option<u64>,
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
    // 1. Unacked trial boot from a previous launch ⇒ that bundle failed.
    //    Blacklist by archive sha256; the dir itself is swept by GC below.
    let rolled_back = state.booting.take();
    if let Some(seq) = rolled_back {
        if let Some(meta) = state.versions.get(&seq) {
            state.failed.insert(meta.archive_sha256.clone());
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
    if let Some(seq) = state.staged.take() {
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
// Exercised by the transition tests today; the WP3 downloader is the
// production caller. Remove the allow when it lands.
#[allow(dead_code)]
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
