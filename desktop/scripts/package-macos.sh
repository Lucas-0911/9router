#!/usr/bin/env bash
set -euo pipefail
if [[ "$(uname -s)" != Darwin ]]; then
  echo "Build the DMG on macOS with Xcode Command Line Tools installed." >&2
  exit 1
fi
cd -- "$(dirname -- "$0")/.."
export npm_config_cache="${npm_config_cache:-$PWD/.npm-cache}"
export APPLE_SIGNING_IDENTITY="${APPLE_SIGNING_IDENTITY:--}"
npm exec --yes --package @tauri-apps/cli@2.12.4 -- tauri build --features desktop-app --bundles app,dmg "$@" -- --locked
codesign --verify --deep --strict target/release/bundle/macos/9Router.app
for image in target/release/bundle/dmg/*.dmg; do hdiutil verify "$image"; done
