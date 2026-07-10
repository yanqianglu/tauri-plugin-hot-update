import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

// ── Types ──────────────────────────────────────────────────────────
// These mirror the plugin's Rust wire shapes exactly; every one is pinned
// by a golden serde test in src/commands/tests.rs. Versions are semver
// strings (e.g. "1.2.3"). Errors reject with a descriptive string.

/** Where the bundle archive lives and what it must hash to. */
export interface ArchiveInfo {
  url: string;
  /** Hex sha256 of the archive bytes. */
  sha256: string;
  /** Exact archive byte count. */
  size: number;
}

/** The signed update manifest, as verified by the plugin. */
export interface Manifest {
  /** Bundle version; strictly newer than anything this install has seen. */
  version: string;
  /** Informational publish timestamp (RFC 3339). */
  createdAt: string;
  /** Minimum shell (app) version able to run this bundle. */
  minShellVersion: string;
  archive: ArchiveInfo;
}

/**
 * Result of a check or download pass. Refusals are first-class outcomes,
 * not errors — the pipeline saying "this manifest is not for us", with the
 * reason preserved:
 *
 * - `available` — a verified, applicable update is offered (`check` only).
 * - `staged` — downloaded, verified, and staged; it becomes active on the
 *   next cold launch (`download` only).
 * - `upToDate` — the offered version is not newer than what this install
 *   has already seen (the everyday "no update" answer).
 * - `blacklisted` — the offered archive previously failed a trial boot; a
 *   fixed release must ship under a new hash.
 * - `shellTooOld` — the bundle needs a newer app binary; a store update is
 *   required first.
 * - `alreadyStaged` — exactly this archive is already waiting for its
 *   trial boot.
 */
export type UpdateOutcome =
  | { status: "available"; manifest: Manifest }
  | { status: "staged"; seq: number; version: string }
  | { status: "upToDate"; offered: string; watermark: string }
  | { status: "blacklisted"; version: string }
  | { status: "shellTooOld"; required: string; shell: string }
  | { status: "alreadyStaged"; seq: number; version: string };

/** {@link check} never downloads, so it never reports `staged`. */
export type CheckOutcome = Exclude<UpdateOutcome, { status: "staged" }>;

/** {@link download} resolves what happened, never a bare offer. */
export type DownloadOutcome = Exclude<UpdateOutcome, { status: "available" }>;

/**
 * Result of {@link notifyAppReady}:
 *
 * - `committed` — this launch's trial bundle is now the last-known-good.
 * - `alreadyCommitted` — steady state; the ack is idempotent.
 * - `embeddedNoop` — serving embedded assets (or the plugin is disabled);
 *   nothing to commit.
 * - `stale` — the booted bundle no longer matches on-disk state (e.g. a
 *   `reset()` ran mid-session); refusing to commit is the safe answer.
 */
export type AckOutcome =
  | { status: "committed"; seq: number }
  | { status: "alreadyCommitted"; seq: number }
  | { status: "embeddedNoop" }
  | { status: "stale"; seq: number };

/** Where the currently served frontend comes from. */
export type BundleSource = "embedded" | "ota";

/** Snapshot of what this process is serving. */
export interface CurrentBundle {
  source: BundleSource;
  /** Bundle sequence number when `source` is `"ota"`, otherwise null. */
  seq: number | null;
  version: string;
}

/** Payload of the download progress event (bytes). */
export interface DownloadProgress {
  downloaded: number;
  total: number;
}

// ── Commands ───────────────────────────────────────────────────────

/**
 * Fetch and verify the signed manifest, then report whether an update
 * applies. Never downloads the archive. The app owns timing — the plugin
 * does not auto-poll; call this on launch/resume.
 *
 * Rejects on transport or signature-verification failures, and when the
 * plugin is disabled by config.
 */
export async function check(): Promise<CheckOutcome> {
  return invoke("plugin:hot-update|check");
}

/**
 * The full pipeline: check, and if an update applies — download, verify,
 * extract, and stage it for the next cold launch. Emits throttled
 * {@link onDownloadProgress} events while the archive streams.
 *
 * Concurrent calls are serialized; the loser reports `alreadyStaged` /
 * `upToDate` instead of downloading twice. Rejects on transport or
 * verification failures, and when the plugin is disabled by config.
 */
export async function download(): Promise<DownloadOutcome> {
  return invoke("plugin:hot-update|download");
}

/**
 * Commit the bundle this launch booted as last-known-good.
 *
 * Call once per launch, as soon as the app shell has mounted and rendered —
 * deliberately independent of network reachability or auth, so a backend
 * outage can never condemn a good bundle. A launch that never acks is
 * rolled back and its bundle blacklisted on the next boot.
 *
 * Idempotent, and safe to call unconditionally: on embedded assets or with
 * the plugin disabled it resolves `{ status: "embeddedNoop" }`.
 */
export async function notifyAppReady(): Promise<AckOutcome> {
  return invoke("plugin:hot-update|notify_app_ready");
}

/** What is being served right now. */
export async function currentBundle(): Promise<CurrentBundle> {
  return invoke("plugin:hot-update|current_bundle");
}

/**
 * Debug/support escape hatch: wipe all OTA state and bundles. The current
 * session keeps serving what it booted; the next launch reverts to the
 * embedded assets. A no-op while the plugin is disabled.
 */
export async function reset(): Promise<void> {
  return invoke("plugin:hot-update|reset");
}

// ── Events ─────────────────────────────────────────────────────────

/**
 * Listen to {@link download} progress. Events are throttled plugin-side
 * (at most ~10/s); the final 100% event (`downloaded === total`) is always
 * delivered.
 *
 * Returns an unlisten function.
 */
export async function onDownloadProgress(
  handler: (progress: DownloadProgress) => void,
): Promise<UnlistenFn> {
  return listen<DownloadProgress>("hot-update://progress", (event) => {
    handler(event.payload);
  });
}
