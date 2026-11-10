#!/usr/bin/env bash
set -euo pipefail
if [[ "$(uname -s)" != Linux ]]; then
  echo "Build the DEB on Debian/Ubuntu with the Tauri WebKitGTK build dependencies installed." >&2
  exit 1
fi
cd -- "$(dirname -- "$0")/.."
export npm_config_cache="${npm_config_cache:-$PWD/.npm-cache}"
if [[ ! -d node_modules ]]; then npm ci; fi
npm exec --yes --package @tauri-apps/cli@2.12.4 -- tauri build --features desktop-app --bundles deb "$@" -- --locked
