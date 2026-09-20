#!/usr/bin/env bash
# Full macOS release build: stages the privileged helper sidecar, then
# builds the application bundle and DMG with Tauri.
set -euo pipefail

cd "$(dirname "$0")/.."

if [ "$(uname -s)" != "Darwin" ]; then
  echo "This script builds the macOS release and must be run on macOS." >&2
  exit 1
fi

./scripts/prepare-sidecar.sh

echo "Building RustyWriter for macOS..."
cargo tauri build --bundles dmg
