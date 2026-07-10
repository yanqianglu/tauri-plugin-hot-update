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
`hot-update-sign` release CLI (`--features cli`). The JavaScript API (IPC
commands) is in progress. Not yet published to crates.io / npm.

Dual-licensed under MIT or Apache-2.0, at your option.
