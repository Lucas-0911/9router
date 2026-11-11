# 9Router Rust desktop

Desktop alpha based on upstream 9router 0.5.116. The app reuses all 28 original
page routes, dashboard components, styles, languages and provider login dialogs.
The packaged backend is Rust; Node is needed only at build time. Full upstream
feature parity is still pending; see [RUST_MIGRATION.md](RUST_MIGRATION.md).

## Browser login

Open **Providers → Muse Web → Login via Browser → Open Browser & Login**.
Sign in on the provider website in the Chrome/Chromium window opened by the app.
The Rust backend saves the session privately; no provider API key is required.
An installed Chrome/Chromium is required for web login and Muse execution.
`NINEROUTER_BROWSER_BIN` can select the browser executable.

The same native capture supports the six original web providers. This does not
mean all six inference executors have been ported: Muse chat/image/video is
implemented; the other specialized web executors remain pending. Native OAuth
authorization, PKCE, device-code polling and refresh are implemented for common
providers. Codex Responses and Chat Completions (including streaming), Gemini CLI
and generic OAuth transports are wired to inference. Codex Chat preserves text,
parallel function calls, refusals, reasoning summaries and signed native reasoning,
annotations and optional usage. Invalid/incomplete streams report an error without
a successful finish marker. Hosted tools use the native Responses endpoint.
Cursor, Zed, Xiaomi, Trae and Windsurf need specialized auth adapters.
Real-provider authentication and generation require an opt-in account test.

## Build and run

Install Rust, Node 22+ and the native Tauri development prerequisites. Build the
original frontend once before running the desktop:

```sh
npm --prefix desktop ci
npm --prefix desktop run build
cargo run --locked --manifest-path desktop/Cargo.toml --features desktop-app -- \
  --data-dir "$PWD/desktop/.data/dev"
```

HTTP server only (there is no standalone web dashboard server in this mode):

```sh
cargo run --locked --manifest-path desktop/Cargo.toml -- --headless \
  --data-dir "$PWD/desktop/.data/dev" --listen 127.0.0.1:20128
```

Native installers, built on their respective operating systems:

```sh
bash desktop/scripts/package-macos.sh       # .app and .dmg
bash desktop/scripts/package-linux.sh       # .deb
# Windows PowerShell:
# ./desktop/scripts/package-windows.ps1     # .exe installer
```

Output: `desktop/target/release/bundle/{macos,dmg,nsis,deb}`. The
`Rust desktop installers` GitHub workflow builds the three native targets.
macOS local builds use ad-hoc signing; public distribution still needs a
Developer ID certificate and notarization. Old alpha.1 artifacts contain the
superseded custom UI; use alpha.2 or newer for the restored original screens.

## Data and verification

Default data: `~/.9router-rust` (`%LOCALAPPDATA%/.9router-rust` on Windows), separate
from upstream `~/.9router`. Browser profiles are app-owned. Quit stops owned
login workers. The admin credential stays in memory after native IPC bootstrap;
it is never put in a URL or browser storage. Use generated client keys for API
clients. Provider secrets are omitted from management responses. Local database
and profile files contain credentials and must remain private.

Explicit import creates a new database and preserves the source:

```sh
cargo run --locked --manifest-path desktop/Cargo.toml -- \
  --data-dir "$PWD/desktop/.data/imported" --import-db /path/to/source/data.sqlite
```

```sh
cargo test --locked --manifest-path desktop/Cargo.toml
node desktop/scripts/check-ui.mjs
node desktop/scripts/snapshot-upstream.mjs --check
node desktop/scripts/snapshot-oauth.mjs --check
node desktop/scripts/snapshot-muse-dom.mjs --check
node desktop/scripts/snapshot-codex.mjs --check
npm --prefix desktop run smoke
```

The smoke script uses isolated fixtures and an installed Chrome. It checks page
rendering and login interactions, not full backend parity. `/api/status` reports
implemented and pending subsystems. Unsupported operations return an explicit
error; storing settings does not activate an unported service. Configured
unported proxies fail before network dispatch.
