# 9Router Rust desktop

Native Rust desktop preview of the 9router 0.5.116 migration. The backend runs
without Node or Python. This is still an alpha: it does **not** yet preserve
all upstream features. The full scope remains in [RUST_MIGRATION.md](RUST_MIGRATION.md).

Implemented:

- SQLite compatibility with all 11 upstream tables, private files, transactional
  entity updates, and explicit consistent import including live WAL data.
- Model/provider resolution, custom nodes, aliases and nested fallback combos.
  Core model resolution matches 1,174 JavaScript reference inputs.
- Native Axum HTTP server: OpenAI Chat, Anthropic Messages, Gemini generateContent
  and native Responses endpoints, with `/api/v1/*` compatibility paths.
- Generic API-key transports using endpoints/auth descriptors captured from the
  upstream registry; custom OpenAI/Anthropic-compatible nodes are supported.
  Specialized executors, OAuth and browser credentials return 501 before dispatch.
- Priority account fallback, per-account/model cooldown, combo fallback after
  a model rejection, API-key model/combo restrictions and filtered model lists.
- Same-protocol SSE streams relay bytes without buffering the entire response;
  disconnect cancels the upstream read. Cross-protocol streaming returns 501.
- Non-streaming OpenAI Chat ↔ Claude/Gemini conversion for the tested message,
  tool, inline-image, reasoning and usage subset. Unsupported fields/signatures
  fail explicitly; this is not full translator parity.
- Management APIs for connections, custom nodes, combos, aliases, keys and
  settings. Provider secrets are omitted from responses; newly generated client
  keys are returned at creation. Unknown settings are preserved for migration.
- Tauri desktop window with a Vietnamese dashboard for connections, models,
  combos, aliases, client keys, test chat and settings. The native IPC bootstrap
  passes the loopback admin credential in memory. No token is put in a URL or
  browser storage. This UI covers eight views, not all 28 upstream pages.

Run from the repository root (Rust + native GUI development prerequisites):

```sh
cargo run --locked --manifest-path desktop/Cargo.toml --features desktop-app -- \
  --data-dir "$PWD/desktop/.data/dev"
```

For the HTTP server alone:

```sh
cargo run --locked --manifest-path desktop/Cargo.toml -- --headless \
  --data-dir "$PWD/desktop/.data/dev" --listen 127.0.0.1:20128
```

The default data directory is `~/.9router-rust` (`%LOCALAPPDATA%/.9router-rust`
on Windows). It is separate from the original `~/.9router`. The local admin
credential is `DATA_DIR/admin-token`, or `NINE_ROUTER_ADMIN_TOKEN` when supplied.
Use a generated client key for inference clients. Only loopback binding is
supported in this preview. HTTP provider URLs are allowed only on loopback;
remote provider URLs require HTTPS. Redirects are disabled. Configured proxies
currently return 501 rather than sending the request directly.

Import is explicit and requires a new destination database:

```sh
cargo run --locked --manifest-path desktop/Cargo.toml -- \
  --data-dir "$PWD/desktop/.data/imported" --import-db /path/to/source/data.sqlite
```

Native installers (Node is used only to run the pinned Tauri build CLI):

```sh
bash desktop/scripts/package-macos.sh       # macOS: DMG
bash desktop/scripts/package-linux.sh       # Debian/Ubuntu: DEB
# Windows PowerShell:
# ./desktop/scripts/package-windows.ps1     # Windows: EXE installer
```

Output is under `desktop/target/release/bundle/{dmg,nsis,deb}`. Each installer
must be built on its actual platform. The GitHub workflow `Rust desktop
installers` runs native macOS, Windows and Linux jobs and uploads their artifacts.
macOS previews use an ad-hoc signature. Developer ID/notarization and Windows
distribution signing are not configured.

Verification:

```sh
node desktop/scripts/snapshot-upstream.mjs --check
node desktop/scripts/check-ui.mjs
cargo test --locked --manifest-path desktop/Cargo.toml
cargo clippy --locked --manifest-path desktop/Cargo.toml --all-targets -- -D warnings
cargo check --locked --manifest-path desktop/Cargo.toml --features desktop-app
python3 desktop/scripts/smoke-native.py desktop/target/release/nine-router
```

Gateway tests use isolated temporary databases and loopback provider fixtures;
no live accounts or credentials are used. The upstream JavaScript suite and
real-provider authentication are separate validation. The immutable catalog
covers 143 providers and 175 API route files; catalog coverage does not mean
all adapters or routes are implemented. See the status endpoint `/api/status`
and the migration ledger for remaining functionality.
