#!/usr/bin/env bash
# Builds the Olaz Linux packages: AppImage + .deb.
#
#   linux/scripts/build-appimage.sh              # cả hai
#   linux/scripts/build-appimage.sh --bundles appimage
#
# Tauri needs its CLI; this uses `npx @tauri-apps/cli` so nothing global is
# installed. `app.asar` is NOT bundled — the user points OLAZ_ASAR at their own
# copy of Zalo PC's renderer (the repo ships no VNG code).
set -euo pipefail
cd "$(dirname "$0")/.."

if ! pkg-config --exists webkit2gtk-4.1; then
  echo "thiếu webkit2gtk-4.1 (dev). Cài: sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev" >&2
  exit 1
fi

# TLS/HTTP-2 fingerprint sidecar (shared Go helper, built for linux/amd64).
# tauri.conf.json lists it under `externalBin`, so it lands in the AppImage as
# usr/bin/olaz-fingerprint-helper, next to the app binary.
"$(dirname "$0")/build-fingerprint-helper.sh"

# Default to both bundles; the caller can override with --bundles.
if [[ "$*" != *--bundles* ]]; then
  set -- --bundles "appimage,deb" "$@"
fi

exec npx --yes @tauri-apps/cli@2 build \
  --config src-tauri/tauri.conf.json \
  "$@"
