# Publishing checklist

The exact steps to take `tauri-plugin-hot-update` from this repo to crates.io,
npm, and GitHub. Run them in order. Nothing here is automated — no CI publishes
on push.

## Accounts / credentials required

- **crates.io** — logged in as the crate owner (`cargo login <token>`); this is the
  first publish of the `tauri-plugin-hot-update` name, so it must be available.
- **npm** — logged in as the owner of the `tauri-plugin-hot-update-api` name
  (`npm whoami` should succeed); first publish of that name.
- **GitHub** — `gh auth status` green, able to create a repo under `yanqianglu`.
- **minisign secret key** — only needed to sign real OTA releases, not to publish
  the plugin. Not touched here.

## 0. Pre-flight (all must pass)

```bash
# Rust: format, lint, full test suite
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test

# The signing CLI compiles under its feature
cargo build --features cli --bin hot-update-sign

# Mobile targets compile (the whole point of the plugin)
rustup target add aarch64-apple-ios aarch64-linux-android
cargo check --target aarch64-apple-ios
cargo check --target aarch64-linux-android

# Packaged crate is valid & contains what we expect (no network publish)
cargo publish --dry-run
cargo package --list      # eyeball the file list

# TypeScript package builds & typechecks
cd guest-js
npm install
npm run typecheck
npm run build             # emits dist/ (gitignored; prepublishOnly re-runs it)
npm pack --dry-run        # eyeball the tarball contents (dist/ + README.md)
cd ..
```

Last verified locally: `cargo test` → 130 passed; `cargo clippy --all-targets` → clean.

## 1. Final metadata review

- `Cargo.toml`: `version`, `description`, `keywords` (max 5), `categories`,
  `repository`, `homepage`, `documentation`, `readme`, `license` — all set.
- `guest-js/package.json`: `version` matches the crate, `keywords`, `repository`,
  `license`, `files` (`dist` + `README.md`).
- `README.md` renders on GitHub (check the tables and the 中文 section).
- Bump the version in **both** `Cargo.toml` and `guest-js/package.json` together if
  this is not the first `0.1.0` publish.

## 2. Publish the crate (crates.io)

```bash
cargo publish
```

- docs.rs builds documentation automatically from the published crate — no action
  needed; the `docs.rs` badge goes live within minutes.
- The `hot-update-sign` binary ships in the crate behind the `cli` feature; users
  get it via `cargo install tauri-plugin-hot-update --features cli`.

## 3. Publish the npm package

```bash
cd guest-js
npm publish            # prepublishOnly runs `npm run build` first
cd ..
```

- The package is public and scopeless (`tauri-plugin-hot-update-api`); if npm 2FA
  is on, have the OTP ready.

## 4. Create the GitHub repo + push

```bash
gh repo create yanqianglu/tauri-plugin-hot-update \
  --public \
  --description "Hot update / OTA / live update (CodePush-style, self-hosted) for Tauri v2 mobile and desktop apps" \
  --source . --remote origin --push
```

(Or `git remote add origin git@github.com:yanqianglu/tauri-plugin-hot-update.git && git push -u origin main` if the repo already exists.)

## 5. Set GitHub topics (discoverability)

```bash
gh repo edit yanqianglu/tauri-plugin-hot-update \
  --add-topic tauri \
  --add-topic tauri-plugin \
  --add-topic tauri-v2 \
  --add-topic hot-update \
  --add-topic ota \
  --add-topic over-the-air \
  --add-topic live-update \
  --add-topic codepush \
  --add-topic codepush-alternative \
  --add-topic expo-updates \
  --add-topic hotfix \
  --add-topic self-hosted \
  --add-topic ios \
  --add-topic android \
  --add-topic mobile \
  --add-topic rust
```

## 6. Tag the release

```bash
git tag v0.1.0
git push origin v0.1.0
gh release create v0.1.0 --generate-notes
```

## Post-publish sanity

- `cargo add tauri-plugin-hot-update` in a scratch project resolves.
- `npm view tauri-plugin-hot-update-api` shows the version and keywords.
- The crates.io / npm / docs.rs badges in the README all render green.
- Search "tauri hot update", "tauri codepush", "tauri 热更新" on GitHub/crates.io
  after indexing — the repo/crate should surface.
