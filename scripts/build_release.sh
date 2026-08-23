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

# Copy MinGW GNU runtime DLLs so the binary is standalone on clean Windows machines
MINGW_BIN="/ucrt64/bin"
if [ -d "$MINGW_BIN" ]; then
    echo "[orr] Copying MinGW runtime DLLs..."
    cp "$MINGW_BIN/libstdc++-6.dll" "$DIST_DIR/" || true
    cp "$MINGW_BIN/libgcc_s_seh-1.dll" "$DIST_DIR/" || true
    cp "$MINGW_BIN/libwinpthread-1.dll" "$DIST_DIR/" || true
fi

if [ -f README.md ]; then cp README.md "$DIST_DIR/"; fi
if [ -f ORR_DESKTOP.md ]; then cp ORR_DESKTOP.md "$DIST_DIR/"; fi

echo "[orr] Packaging portable release into dist/orr_desktop_portable.zip..."
powershell -Command "Compress-Archive -Path 'dist\*' -DestinationPath 'dist\orr_desktop_portable.zip' -Force"

echo "[orr] Release build complete: dist/orr_desktop_portable.zip"
