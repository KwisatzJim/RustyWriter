#!/usr/bin/env bash
# Full Linux release build: stages the sidecar helper, then runs
# `cargo tauri build`.
#
# NO_STRIP=true is set unconditionally here because on Arch-family
# systems (CachyOS, Arch, EndeavourOS, etc), the system `strip` that
# ships with newer binutils produces ELF sections (like .relr.dyn)
# that linuxdeploy's own bundled `strip` doesn't recognize, which
# makes AppImage bundling fail with "failed to run linuxdeploy" even
# though the actual app built fine. This isn't a RustyWriter bug -
# it's a linuxdeploy/binutils compatibility gap - and setting
# NO_STRIP=true (skip stripping debug symbols from bundled libraries)
# is the standard workaround. It's harmless to leave on for
# distros that don't need it too, just produces a slightly larger
# AppImage.
set -euo pipefail

cd "$(dirname "$0")/.."

./scripts/prepare-sidecar.sh

echo "Running cargo tauri build (NO_STRIP=true)..."
NO_STRIP=true cargo tauri build
