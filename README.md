# tauri-plugin-hot-update

[![crates.io](https://img.shields.io/crates/v/tauri-plugin-hot-update.svg)](https://crates.io/crates/tauri-plugin-hot-update)
[![npm](https://img.shields.io/npm/v/tauri-plugin-hot-update-api.svg)](https://www.npmjs.com/package/tauri-plugin-hot-update-api)
[![docs.rs](https://img.shields.io/docsrs/tauri-plugin-hot-update)](https://docs.rs/tauri-plugin-hot-update)
[![license](https://img.shields.io/crates/l/tauri-plugin-hot-update.svg)](#license)

**Hot updates / OTA (over-the-air) live updates for Tauri v2 mobile and desktop apps — self-hosted, CodePush-style, with automatic anti-brick rollback.**

`tauri-plugin-hot-update` brings **hot updates** — **over-the-air (OTA)**, **live updates** for your app's web frontend — to **Tauri v2** apps on **iOS, Android, macOS, Windows, and Linux**. It is a **self-hosted, CodePush-style / Expo-Updates-style** update channel for Tauri: ship JS/CSS/HTML fixes, **hotfixes**, and content changes to already-installed apps in seconds, without waiting on an App Store or Play Store review. If you have searched for a **CodePush alternative for Tauri**, **Expo Updates for Tauri**, **Tauri OTA update**, **Tauri live update**, **Tauri hotfix**, or the Chinese-language **Tauri 热更新** — this is that capability, delivered from your own server or CDN with no third-party SaaS in the loop.

> **Status:** feature-complete and exhaustively unit-tested; pre-1.0. See [Platform support](#platform-support) for what has been exercised on-device.

## Contents

- [Why](#why)
- [What it does & the safety model](#what-it-does--the-safety-model)
- [How it works](#how-it-works)
- [Install](#install)
- [Integration quickstart](#integration-quickstart)
- [Publishing a bundle](#publishing-a-bundle)
- [Platform support](#platform-support)
- [Store compliance (is OTA allowed on iOS & Android?)](#store-compliance-is-ota-allowed-on-ios--android)
- [Security](#security)
- [Comparison with alternatives](#comparison-with-alternatives)
- [中文说明（Tauri 热更新）](#中文说明tauri-热更新)
- [License](#license)

## Why

Every frontend change in a Tauri mobile app — even a one-line CSS fix — normally requires a full native rebuild, a store upload, and a review cycle before installed users see it. The official [`tauri-plugin-updater`](https://v2.tauri.app/plugin/updater/) replaces the **entire** desktop binary and [does not support mobile](https://github.com/orgs/tauri-apps/discussions/8467). Native mobile ecosystems solved this years ago with **CodePush** (React Native / Cordova) and **Expo Updates** (React Native) — background OTA delivery of the interpreted (JS) layer. This plugin brings that same workflow to Tauri v2, self-hosted, with a rollback model designed so a bad push can never brick the fleet.

## What it does & the safety model

- **Ships frontend bundles OTA.** Your built `dist/` (JS/CSS/HTML/assets) is tarred, signed, and served from any static host (R2, S3, a CDN, your own box). Installed apps download it in the background and apply it on the **next cold launch** — launch stays instant and works offline, because assets are always served from local disk.
- **A bad bundle can never brick an app.** Rollback is governed by a three-state pointer — **`staged → booting → committed`** — with **no timers and no render-timing heuristics**:
  1. **Download** verifies and stages a bundle atomically (`staged = N`).
  2. On the next cold launch, before the webview exists, the plugin promotes `staged → booting` and **persists that state before serving a single byte** of the new bundle.
  3. Your frontend calls **`notifyAppReady()`** once the app shell has mounted — that commits the running bundle as the new last-known-good.
  4. If a launch finds a `booting` bundle that was never acked (crash, white screen, wedge), that bundle's archive hash is **blacklisted**, and serving falls back to the last-known-good bundle or the compiled-in embedded assets. Failed hashes are never retried; a fix ships under a new hash.
- **The embedded bundle is the permanent floor.** The assets compiled into the store binary are always the fallback of last resort. Absence of the ready-ack *is* the failure signal — deliberately independent of network or auth, so a backend outage can never condemn a good bundle fleet-wide.
- **Signed manifests + downgrade-replay protection.** Every manifest is [minisign](https://jedisct1.github.io/minisign/)-signed; the shell trusts a **list** of public keys (so keys can be rotated). A monotonic version watermark rejects an older, still-validly-signed manifest replayed by a stale cache or a MITM.

## How it works

Tauri serves your frontend through a single extension point: `tauri::Context::assets`, a public `Box<dyn Assets<R>>` whose `get()` method is the one call site behind the `tauri://` scheme handler on **every** platform. This plugin **swaps that provider** for one that resolves each asset from the active on-disk OTA bundle directory, falling back to the embedded assets compiled from `frontendDist`.

Because it reuses the **existing `tauri://` scheme** rather than registering a new one, the webview origin never changes — `tauri://localhost` on iOS/macOS, `http://tauri.localhost` on Android. Your `localStorage`, `IndexedDB`, cookies, and auth/session state are **preserved by construction**: an OTA update does not log your users out. (Registering a second scheme *would* change the origin and silently wipe web storage — this plugin avoids that landmine entirely.)

The provider is armed inside the plugin's setup hook, which Tauri runs **before any window or webview is created** — so the `staged → booting` promotion is durably persisted before the new bundle serves anything, on every platform including Android (where the app data dir cannot even be resolved until the app exists).

## Install

### Rust

```toml
# src-tauri/Cargo.toml
[dependencies]
tauri-plugin-hot-update = "0.1"
```

Or track git directly:

```toml
tauri-plugin-hot-update = { git = "https://github.com/yanqianglu/tauri-plugin-hot-update" }
```

TLS backend is selectable, mirroring `tauri-plugin-updater`. The default `rustls-tls` cross-compiles cleanly for `aarch64-apple-ios` and `aarch64-linux-android`; use `native-tls` if your app standardizes on platform TLS:

```toml
tauri-plugin-hot-update = { version = "0.1", default-features = false, features = ["native-tls"] }
```

### JavaScript / TypeScript

```bash
npm install tauri-plugin-hot-update-api
# or: pnpm add / bun add / yarn add
```

Requires `@tauri-apps/api` >= 2.0.0 as a peer dependency.

### Release-signing CLI

The `hot-update-sign` binary lives behind the `cli` feature so the full minisign implementation (secret-key handling) never enters your app builds:

```bash
cargo install tauri-plugin-hot-update --features cli
```

## Integration quickstart

### 1. Swap assets + register the plugin (`src-tauri/src/main.rs`)

Integration is **two steps sharing one handle**. The assets swap must happen on the `Context` before it is consumed; path resolution only works once the app is being built:

```rust
fn main() {
    let mut context = tauri::generate_context!();

    // Step 1 — swap the embedded assets for the hot-update provider.
    let hot_update = tauri_plugin_hot_update::install(&mut context);

    tauri::Builder::default()
        // Step 2 — register the plugin FIRST, so nothing observes assets
        // before its setup hook arms/rolls back the bundle store.
        .plugin(tauri_plugin_hot_update::init(hot_update))
        // ... your other plugins
        .run(context)
        .expect("error while running tauri application");
}
```

### 2. Configure the update source (`tauri.conf.json`)

```json
{
  "plugins": {
    "hot-update": {
      "manifestUrl": "https://updates.example.com/manifest.json",
      "pubkeys": ["RWT...your-minisign-public-key..."],
      "enabled": true
    }
  }
}
```

- `manifestUrl` — a plain file URL (no query string); the detached signature is fetched from `<manifestUrl>.minisig`.
- `pubkeys` — one or more trusted minisign public keys (raw `RW…` base64 or full `minisign.pub` contents). Ship the old **and** new key during a rotation.
- `enabled` — set `false` to **dark-ship**: the plugin registers but stays inert, running purely on embedded assets.

The config is the **only** update source — JS cannot pass a manifest URL or keys, so compromised webview content can never redirect updates. A malformed config aborts startup on the developer's machine, never silently in the field.

### 3. Grant the commands (capability file)

```json
{ "permissions": ["hot-update:default"] }
```

This grants all five commands: `check`, `download`, `notify_app_ready`, `current_bundle`, `reset`.

### 4. Drive it from the frontend

```ts
import {
  check,
  download,
  notifyAppReady,
  currentBundle,
  reset,
  onDownloadProgress,
} from "tauri-plugin-hot-update-api";

// On every launch, once your app shell has mounted and rendered:
// commits the running bundle as last-known-good. Safe to call
// unconditionally — a no-op on embedded assets or when disabled.
await notifyAppReady();

// Whenever your app decides to look for an update (launch/resume — the
// plugin never auto-polls; you own the timing):
const unlisten = await onDownloadProgress(({ downloaded, total }) => {
  console.log(`OTA: ${Math.round((downloaded / total) * 100)}%`);
});
const outcome = await download();
unlisten();

if (outcome.status === "staged") {
  // Applies on the next cold launch.
  console.log(`v${outcome.version} ready — restart to apply`);
}

// Introspection & escape hatch:
await currentBundle(); // { source: "ota" | "embedded", seq, version }
await reset();         // wipe OTA state, revert to embedded on next launch
```

### TypeScript API surface

| Function | Returns | Purpose |
|---|---|---|
| `check()` | `CheckOutcome` | Fetch + verify the manifest; report whether an update applies. Never downloads. |
| `download()` | `DownloadOutcome` | Full pipeline: check → download → verify → extract → stage for next launch. |
| `notifyAppReady()` | `AckOutcome` | Commit the booted bundle as last-known-good. Call once per launch. |
| `currentBundle()` | `CurrentBundle` | What is being served right now (`ota` vs `embedded`, seq, version). |
| `reset()` | `void` | Wipe all OTA state; next launch reverts to embedded. Debug/support hatch. |
| `onDownloadProgress(fn)` | `UnlistenFn` | Throttled byte-progress events during `download()`. |

Refusals are first-class outcomes, not thrown errors — `check`/`download` resolve with a `status` of `available` / `staged` / `upToDate` / `blacklisted` / `shellTooOld` / `alreadyStaged`, each carrying the relevant context. Every wire shape is pinned by a golden serde test.

## Publishing a bundle

### One-time: generate a signing keypair

Use the [`minisign`](https://jedisct1.github.io/minisign/) tool (or `rsign2`) once, and keep the secret key off CI where possible:

```bash
minisign -G -p hot-update.pub -s hot-update.key
```

Put the contents of `hot-update.pub` (or just its `RW…` line) into `pubkeys` in `tauri.conf.json`.

### Every release: build, sign, upload

```bash
# 1. Build your frontend as usual -> produces dist/
npm run build

# 2. Tar + sign + emit manifest.json + manifest.json.minisig
hot-update-sign \
  --dist dist \
  --version 1.2.0 \
  --min-shell 1.1.0 \
  --key ~/.keys/hot-update.key \
  --base-url https://updates.example.com \
  --out release/

# 3. Upload release/* to the host that backs manifestUrl
#    (bundle-1.2.0.tar.gz, manifest.json, manifest.json.minisig)
```

The secret-key password is read from `HOT_UPDATE_KEY_PASSWORD` when set (use an empty value for unencrypted keys in CI); otherwise it prompts interactively.

### Manifest format

`hot-update-sign` emits this JSON, signed detached into `manifest.json.minisig`:

```json
{
  "version": "1.2.0",
  "createdAt": "2026-07-09T00:00:00Z",
  "minShellVersion": "1.1.0",
  "archive": {
    "url": "https://updates.example.com/bundle-1.2.0.tar.gz",
    "sha256": "…64 hex chars…",
    "size": 4194304
  }
}
```

- `version` must be **strictly newer** than anything the install has already seen (downgrade-replay watermark).
- `minShellVersion` gates the bundle to shells new enough to run it — an old shell simply stays put until a store release reaches it. **Corollary:** your deployed frontend must feature-detect native capabilities rather than assume them.

## Platform support

| Platform | Serving | OTA apply | Rollback | Notes |
|---|---|---|---|---|
| iOS 13+ | `tauri://localhost` | Next cold launch | Yes | Session/auth state preserved (origin unchanged). |
| Android 7+ (API 24) | `http://tauri.localhost` | Next cold launch | Yes | Uses bundled CA roots — see note below. |
| macOS / Windows / Linux | `tauri://localhost` | Next cold launch | Yes | The `Assets` swap is platform-uniform; desktop works for free. |

The plugin is **pure Rust** — no Kotlin or Swift bridge — because the assets swap and file I/O are platform-uniform.

> **Why we bundle CA roots (Android note for Tauri mobile devs).** `reqwest`'s rustls feature set pulls in `rustls-platform-verifier`, whose Android backend needs a JNI context that a Tauri plugin does not wire up — the TLS handshake **hangs indefinitely** on Android as a result. This plugin instead hands `reqwest` a rustls config backed by the bundled [`webpki-roots`](https://crates.io/crates/webpki-roots) (Mozilla's CA set), so certificate verification is self-contained and works identically on every platform. If you are writing your own Tauri mobile plugin that makes HTTPS requests, this is a trap worth knowing about.

## Store compliance (is OTA allowed on iOS & Android?)

Both stores permit OTA updates of **interpreted** code (JS/HTML/CSS in a webview) within limits. This plugin updates exactly that layer — never native code. **This is a summary, not legal advice; you are responsible for your own review outcome.**

**Apple — Developer Program License Agreement §3.3.1(B)** (verbatim):

> "Except as set forth in the next paragraph, an Application may not download or install executable code. Interpreted code may be downloaded to an Application but only so long as such code: (a) does not change the primary purpose of the Application by providing features or functionality that are inconsistent with the intended and advertised purpose of the Application (b) does not bypass signing, sandbox, or other security features of the OS; and (c) for Applications distributed on the App Store, does not create a store or storefront for other Applications."

**Google Play — Device and Network Abuse policy** (verbatim):

> "An app distributed via Google Play may not modify, replace, or update itself using any method other than Google Play's update mechanism. Likewise, an app may not download executable code (such as dex, JAR, .so files) from a source other than Google Play. This restriction does not apply to code that runs in a virtual machine or an interpreter where either provides indirect access to Android APIs (such as JavaScript in a webview or browser)."

**The discipline that keeps you inside both policies:** use OTA for UI refinements, fixes, and content — never a functionality pivot. Ship feature-scale changes (and any native/Rust changes) through a normal store release, even when they are technically OTA-able. Neither policy is a blanket green light; both retain "don't change what the app fundamentally does" language.

## Security

The trust anchor is the **signature over the manifest**, verified before any archive byte is fetched. Transport is HTTPS, but security does not depend on it.

- **Signing.** minisign signature verified over the **raw manifest bytes** against a trusted-key list, *then* the bytes are parsed. Nothing downstream ever touches unverified data.
- **Key rotation.** The shell embeds a **list** of trusted public keys. Rotation = a store release that adds the new key; manifests may be signed with either key during the transition; the old key is dropped in a later release. No downtime, no forced upgrade.
- **Downgrade / replay protection.** A monotonic `maxVersionSeen` watermark rejects any manifest whose version is not strictly newer — even one with a perfectly valid signature — defeating a MITM or stale cache replaying an older, vulnerable bundle. A store release shipping newer embedded assets also discards a now-stale OTA bundle.
- **Integrity.** The archive is verified against the manifest's `sha256` and exact `size` before extraction.
- **Hardened extraction.** The tar.gz is unpacked with symlink/hardlink rejection, path-traversal (zip-slip) guards, and uncompressed-size caps (zip-bomb protection).
- **No JS-controlled update source.** The manifest URL and keys come only from `tauri.conf.json`, baked into the signed store binary — compromised webview content cannot redirect updates.

**What the trust model does *not* cover:** a leaked signing key (rotate immediately and ship a store release dropping the old key), and native-code integrity (native code only ever changes via a store release, which the OS signs).

## Comparison with alternatives

Honest and factual. Expo Updates and CodePush are listed as the mental model — they target React Native, **not** Tauri.

| Project | Framework | Tauri v2 | Mobile (iOS + Android) | Self-hosted | Signing | Rollback | License / notes |
|---|---|---|---|---|---|---|---|
| **tauri-plugin-hot-update** (this) | Tauri | ✅ | ✅ both | ✅ any static host / CDN | ✅ minisign, key-list rotation | ✅ 3-state, hash-blacklist, downgrade-replay guard | MIT/Apache-2.0 |
| [`denniskribl/tauri-plugin-hotswap`](https://github.com/denniskribl/tauri-plugin-hotswap) | Tauri | ✅ | ✅ claimed (README) | ✅ | ✅ minisign | ✅ ack-based | MIT; single maintainer, CI is Linux/macOS/Windows only (no mobile CI). Same `Assets`-swap architecture — convergent, independently arrived at. |
| [`@capgo/tauri-updater`](https://github.com/Cap-go/tauri-updater) | Tauri | ✅ | ⚠️ unstated; modeled on Capacitor | ✅ or Capgo cloud | via composed plugins | via composed plugins | Pure TypeScript orchestration of official fs/http/process plugins — **no native Rust plugin code**; ability to redirect the mobile webview asset load is unverified. |
| CrabNebula OTA (`tauri-plugin-ota-updater`) | Tauri | ✅ | ⚠️ not explicit for mobile | ❌ cloud-mediated (CrabNebula Cloud) | ✅ (cloud) | ✅ | PolyForm-Noncommercial — **not fully OSS**; paid/cloud offering. |
| [Expo Updates](https://docs.expo.dev/versions/latest/sdk/updates/) | **React Native** | ❌ | ✅ | ✅ or EAS | ✅ code signing | ✅ | The React Native reference model; not applicable to Tauri. |
| [Microsoft CodePush](https://learn.microsoft.com/en-us/appcenter/distribution/codepush/) | RN / Cordova | ❌ | ✅ | ❌ MS-hosted | ✅ | ✅ | App Center CodePush **retired March 2025**; this plugin brings the *pattern* to Tauri. |

## 中文说明（Tauri 热更新）

`tauri-plugin-hot-update` 为 **Tauri v2** 的移动端（iOS、Android）与桌面端应用提供**热更新 / OTA（空中下载）/ 线上更新 / 增量发布**能力：把前端产物（JS/CSS/HTML）以签名压缩包的形式**自托管**在你自己的服务器或 CDN 上，已安装的 App 在后台静默下载，并在**下次冷启动**时应用——无需重新过应用商店审核。它相当于 Tauri 生态里的 **CodePush / Expo Updates 替代方案**，常用于 UI 修复、**线上修复（hotfix）**、文案与样式调整等。（关键词：热更新、OTA、空中更新、增量更新、线上修复、动态更新。）

**安全与防变砖（核心设计）：**

- **三态回滚指针** `staged → booting → committed`：坏包永远无法把 App 变砖。新包在下次冷启动前先落盘为 `booting` 状态，前端调用 `notifyAppReady()` 才提交为“已知可用”；若某次启动发现 `booting` 包始终未确认（崩溃、白屏），则按包哈希拉黑并回退到上一个可用包，最终兜底到内置包。
- **签名清单 + 防降级重放**：每份 manifest 由 minisign 签名，客户端信任一组公钥（便于轮换）；单调版本水位线会拒绝被重放的旧版本清单——即使签名合法。
- **保留登录态**：复用既有 `tauri://` 协议，Webview 源（origin）不变，`localStorage`/`IndexedDB`/登录态在更新后自动保留，用户不会被登出。

**快速开始：** 在 `src-tauri` 中先 `install(&mut context)` 交换资源、再 `init(handle)` 注册插件；在 `tauri.conf.json` 的 `plugins.hot-update` 中填写 `manifestUrl` 与 `pubkeys`；前端调用 `notifyAppReady()` / `download()`。完整步骤见上文英文 [Integration quickstart](#integration-quickstart)。发布用 `hot-update-sign` CLI 生成签名清单，见 [Publishing a bundle](#publishing-a-bundle)。**商店合规**：Apple 与 Google 均允许更新 WebView 中的“解释型代码”（JS/HTML/CSS），但不得改变 App 的核心用途——详见 [Store compliance](#store-compliance-is-ota-allowed-on-ios--android)。

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT License](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual-licensed as above, without any additional terms or conditions.
