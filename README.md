# tauri-plugin-hot-update

Hot update / OTA live updates for Tauri v2 mobile (iOS, Android) and desktop
apps — CodePush-style, self-hosted. Ships frontend bundle updates (JS/CSS/HTML
assets) to installed apps without a store release, applied on the next cold
launch, with an automatic anti-brick rollback state machine (a bad bundle can
never permanently break an installed app — the embedded bundle is the floor).

**Under construction.** The core serving layer and rollback state machine are
implemented and exhaustively tested; downloader, signing, and the JavaScript
API are in progress. Not yet published to crates.io / npm.

Dual-licensed under MIT or Apache-2.0, at your option.
