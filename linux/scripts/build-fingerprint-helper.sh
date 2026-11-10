#!/usr/bin/env bash
# Builds the repository's Go fingerprint helper (FingerprintHelper/) for Linux and
# places it where Tauri expects a sidecar: src-tauri/binaries/<name>-<target-triple>.
# `tauri build` then bundles it into the AppImage as usr/bin/olaz-fingerprint-helper.
set -euo pipefail

cd "$(dirname "$0")/.." # linux/
repo="$(cd .. && pwd)"
triple="${OLAZ_TARGET_TRIPLE:-x86_64-unknown-linux-gnu}"
arch=amd64
case "$triple" in
  aarch64* | arm64*) arch=arm64 ;;
esac

command -v go >/dev/null || {
  echo "cần Go toolchain để build fingerprint helper" >&2
  exit 1
}

out="$repo/linux/src-tauri/binaries/olaz-fingerprint-helper-$triple"
mkdir -p "$(dirname "$out")"
(
  cd "$repo/FingerprintHelper"
  CGO_ENABLED=0 GOOS=linux GOARCH="$arch" go build -trimpath -ldflags="-s -w" -o "$out" .
)
chmod +x "$out"
echo "Built $out ($(du -h "$out" | cut -f1))"
