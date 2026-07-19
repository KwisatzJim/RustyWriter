#!/usr/bin/env bash
# Builds rustywriter-helper in release mode and stages it where Tauri's
# "externalBin" (sidecar) bundling expects to find it: next to the
# app, suffixed with the host target triple.
#
# Run this once before `cargo tauri build`. `cargo tauri dev` doesn't
# need this - flash.rs falls back to reading the plain binary straight
# out of target/debug in dev builds.
set -euo pipefail

cd "$(dirname "$0")/.."

echo "Building rustywriter-helper (release)..."
cargo build --release -p rustywriter-helper

TRIPLE="$(rustc -vV | sed -n 's/host: //p')"
if [ -z "$TRIPLE" ]; then
  echo "Couldn't determine host target triple from rustc -vV" >&2
  exit 1
fi

mkdir -p src-tauri/binaries
SRC="target/release/rustywriter-helper"
DEST="src-tauri/binaries/rustywriter-helper-${TRIPLE}"

if [ ! -f "$SRC" ]; then
  echo "Expected $SRC to exist after building - check the build output above." >&2
  exit 1
fi

cp "$SRC" "$DEST"
chmod +x "$DEST"
echo "Staged sidecar at $DEST"
echo "You can now run: cargo tauri build"
