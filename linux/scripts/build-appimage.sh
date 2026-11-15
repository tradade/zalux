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
# Audio engine (zrtc): cpal cần libasound2-dev, opus cần libopus-dev. Runtime
# libs (libopus.so.0 / libasound) sẽ được linuxdeploy bundle vào AppImage.
if ! pkg-config --exists alsa; then
  echo "thiếu ALSA (dev) cho audio engine. Cài: sudo apt install libasound2-dev" >&2
  exit 1
fi
if ! pkg-config --exists opus; then
  echo "thiếu libopus (dev) cho audio engine. Cài: sudo apt install libopus-dev" >&2
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

# Phiên bản package theo tag release (linux-v1.0.8 → 1.0.8) — tên file
# AppImage/.deb và metadata theo đó. CI đặt sẵn APP_RELEASE_TAG; chạy tay thì
# lấy tag git gần nhất, không có tag thì về phiên bản trong Cargo.toml.
version="${APP_RELEASE_TAG:-}"
if [[ -z "$version" ]]; then
  version="$(git describe --tags --match 'linux-v*' --abbrev=0 2>/dev/null || true)"
fi
version="${version#linux-v}"
if [[ -z "$version" ]]; then
  version="$(sed -n 's/^version = "\(.*\)"/\1/p' src-tauri/Cargo.toml | head -1)"
fi
echo ">> build Olaz version $version"
config_json="{\"version\":\"$version\"}"

exec npx --yes @tauri-apps/cli@2 build \
  --config "$config_json" \
  "$@"
