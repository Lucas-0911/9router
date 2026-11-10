# 9Router Rust desktop migration

User requirements: fork `decolua/9router`, preserve every existing feature,
rewrite the backend in Rust, ship a desktop app on macOS, Windows and Linux.
A Node sidecar does not satisfy the backend requirement.

Reference commit: `1339107e624d1ab150c6a0c2c2ee22caa7e9db2f` (0.5.116).
Remote fork: https://github.com/Lucas-0911/9router

The original source remains the behavioral reference. `upstream-contract.json`
records the API routes, pages, registered providers, schema and source digests;
`node desktop/scripts/snapshot-upstream.mjs --check` detects reference drift.
Listing a provider in the catalog does not establish a working Rust adapter.

Build order and acceptance:

1. Catalog and SQLite compatibility: preserve upstream identifiers, all schema
   tables, safe persistence and the complete provider/model metadata. Test only
   isolated databases; importing an existing installation must be explicit and
   use a backup. Never silently migrate the user's live `~/.9router` database.
2. Routing and protocols: aliases/combos, account selection, refresh/retry,
   cancellation and backpressure; OpenAI, Anthropic, Responses and Gemini
   request/response and SSE translations, including tools, thinking and media.
3. Every provider: API-key, OAuth and browser sessions; port each executor and
   credential flow against fixtures and opt-in authenticated integration checks.
4. All management APIs: auth/SSO, keys, models, quota/pricing/usage, media,
   proxies/tunnels/MITM, RTK, MCP, CLI integration, plugins and update behavior.
5. Desktop UI: Tauri window, native app lifecycle and all 28 existing UI routes;
   no Node or Python server required at runtime. Quit must stop owned workers.
6. Platform builds: native macOS DMG, Windows EXE installer and Debian DEB.
   Windows/Linux require actual target builds and runtime verification; changing
   an artifact's extension is not a target build.

Use upstream fixtures for differential tests. Preserve documented baseline
failures rather than claiming the upstream suite is fully green. Do not mark
the migration complete while any upstream capability lacks a Rust implementation
and appropriate verification. Real-provider tests require the user's explicitly
selected accounts; credentials must never be copied into the fork or logs.

The initial native crate below is a migration foundation, not a release or a
feature-complete replacement. The existing MuseRouter installers are unrelated
to this project and must not be presented as 9Router Rust builds.
