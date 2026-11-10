# 9Router Rust — migration in progress

This directory is the start of a full Rust backend port of 9Router 0.5.116.
It is not yet a desktop application or a replacement for the original server.
The user requires all upstream features; a Node sidecar is not the target.

Implemented and verified on macOS arm64:

- Compile-time reference catalog: 143 providers, 175 API route files, 28 UI pages.
- Native SQLite creation with all 11 upstream tables, private permissions,
  compatible columns, atomic JSON KV transactions and corruption reporting.
- Explicit SQLite backup/import from a read-only source, including live WAL
  data. Failed imports never publish an empty or partial destination; existing
  destination files are never overwritten. No live user database
  is imported or modified automatically.
- Native `open-sse/services/model.js` model parsing/provider resolution,
  including registry aliases and bare-model inference. Rust outputs match
  JavaScript reference outputs for 1,174 inputs. This does not yet include the
  app-side custom-node/combo integration in `src/sse/services/model.js`.

Run from the repository root:

```sh
node desktop/scripts/snapshot-upstream.mjs --check
cargo test --locked --manifest-path desktop/Cargo.toml
cargo clippy --locked --manifest-path desktop/Cargo.toml --all-targets -- -D warnings
cargo build --release --locked --manifest-path desktop/Cargo.toml
```

Node runs only the development reference/snapshot generator. The Rust crate
does not launch Node or Python. The crate currently links the host SQLite
library; Linux development requires SQLite headers and pkg-config.

The current seven Rust test functions cover persistence and the model-resolution
oracle; they are not the upstream JavaScript test suite or live provider tests.

Still required before release: HTTP/API server, all provider executors and
OAuth/browser sessions, protocol/SSE translations, routing/fallback/quotas,
RTK/media/proxy/tunnel/MCP/CLI services, all management APIs, the desktop UI and
native installers. See [RUST_MIGRATION.md](RUST_MIGRATION.md) for the full scope.
No DMG/EXE/DEB for the Rust app has been built yet.
