# tauri-plugin-hot-update

Hot update / OTA live updates for Tauri v2 mobile (iOS, Android) and desktop
apps — CodePush-style, self-hosted. Ships frontend bundle updates (JS/CSS/HTML
assets) to installed apps without a store release, applied on the next cold
launch, with an automatic anti-brick rollback state machine (a bad bundle can
never permanently break an installed app — the embedded bundle is the floor).

**Under construction.** Implemented and exhaustively tested: the core serving
layer, the rollback state machine, and the update-acquisition pipeline —
minisign-verified manifests (trusted-key list for rotation, downgrade-replay
watermark), sha256-verified streaming download, hardened tar.gz extraction
(symlink/hardlink/traversal rejection, zip-bomb caps), atomic staging, and the
`hot-update-sign` release CLI (`--features cli`). The public API surface is
also in: five IPC commands (`check`, `download`, `notify_app_ready`,
`current_bundle`, `reset`) configured via `plugins.hot-update` in
`tauri.conf.json` (validated at startup; `"enabled": false` dark-ships),
throttled `hot-update://progress` events, per-command permissions with a
`hot-update:default` set, and the TypeScript package in `guest-js/`
(npm: `tauri-plugin-hot-update-api`). Not yet published to crates.io / npm.

Dual-licensed under MIT or Apache-2.0, at your option.
