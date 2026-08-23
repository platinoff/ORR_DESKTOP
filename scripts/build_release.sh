#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

echo "[orr] Building release binary..."
cargo build --release

REL_DIR="target/release"
DIST_DIR="dist"
rm -rf "$DIST_DIR"
mkdir -p "$DIST_DIR"

cp "$REL_DIR/orr_desktop.exe" "$DIST_DIR/"
if [ -f README.md ]; then cp README.md "$DIST_DIR/"; fi
if [ -f ORR_DESKTOP.md ]; then cp ORR_DESKTOP.md "$DIST_DIR/"; fi

echo "[orr] Packaging portable release into dist/orr_desktop_portable.zip..."
powershell -Command "Compress-Archive -Path 'dist\*' -DestinationPath 'dist\orr_desktop_portable.zip' -Force"

echo "[orr] Release build complete: dist/orr_desktop_portable.zip"
