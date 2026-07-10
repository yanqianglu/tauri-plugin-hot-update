# tauri-plugin-hot-update-api

TypeScript bindings for [`tauri-plugin-hot-update`](https://github.com/yanqianglu/tauri-plugin-hot-update)
— hot update / OTA live updates (CodePush-style, self-hosted) for Tauri v2
mobile and desktop apps. Ship frontend bundle updates to installed apps
without a store release, guarded by an automatic anti-brick rollback state
machine.

Requires the Rust plugin to be installed and configured (manifest URL +
minisign public keys in `tauri.conf.json`) — see the
[plugin README](https://github.com/yanqianglu/tauri-plugin-hot-update) for
setup, signing, and publishing.

## Usage

```ts
import {
  check,
  download,
  notifyAppReady,
  currentBundle,
  onDownloadProgress,
} from "tauri-plugin-hot-update-api";

// 1. On every launch, as soon as your app shell has rendered:
//    commits the running bundle as last-known-good. A launch that never
//    acks is rolled back on the next boot. Safe to call unconditionally.
await notifyAppReady();

// 2. Whenever the app decides (launch/resume — the plugin never auto-polls):
const unlisten = await onDownloadProgress(({ downloaded, total }) => {
  console.log(`OTA download: ${Math.round((downloaded / total) * 100)}%`);
});
const outcome = await download();
unlisten();

if (outcome.status === "staged") {
  // The update applies on the next cold launch.
  console.log(`v${outcome.version} ready — restart to apply`);
}

// Introspection:
const bundle = await currentBundle();
// { source: "ota" | "embedded", seq: number | null, version: string }
```

`check()` fetches and verifies the manifest without downloading, for apps
that want to ask before pulling the archive.

## Permissions

Grant the plugin's commands in your capability file:

```json
{ "permissions": ["hot-update:default"] }
```

Dual-licensed under MIT or Apache-2.0, at your option.
