#!/usr/bin/env bash
# Build the Unterm native terminal and install it as a Unity macOS plugin bundle.
#
# Unity loads native plugins on macOS from a `.bundle`. A cdylib `.dylib` works
# fine when renamed, so we lipo the per-arch builds into Assets/Plugins.
set -euo pipefail

cd "$(dirname "$0")"

PROFILE="${1:-release}"
case "$PROFILE" in
  release) CARGO_FLAGS=(--release); TARGET_DIR="release" ;;
  debug)   CARGO_FLAGS=();          TARGET_DIR="debug"   ;;
  *) echo "usage: $0 [release|debug]" >&2; exit 1 ;;
esac

# Universal binary so the plugin runs on Apple Silicon and Intel editors.
ARCHS=(aarch64-apple-darwin x86_64-apple-darwin)
for arch in "${ARCHS[@]}"; do
  rustup target add "$arch" >/dev/null 2>&1 || true
done

echo "==> building unterm ($PROFILE)"
for arch in "${ARCHS[@]}"; do
  cargo build -p unterm "${CARGO_FLAGS[@]}" --target "$arch"
done

DEST="../Packages/dev.tnayuki.unterm/Editor/Plugins/macOS/unterm.bundle"
mkdir -p "$(dirname "$DEST")"

LIBS=()
for arch in "${ARCHS[@]}"; do
  LIBS+=("target/$arch/${TARGET_DIR}/libunterm.dylib")
done

echo "==> lipo -> $DEST"
lipo -create "${LIBS[@]}" -output "$DEST"

echo "==> done: $DEST"
lipo -info "$DEST"
