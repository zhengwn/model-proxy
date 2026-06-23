#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

TARGET="${MAC_TARGET:-aarch64-apple-darwin}"
BUNDLES="${MAC_BUNDLES:-dmg}"
SIGN="${MAC_SIGN:-0}"
VERBOSE="${MAC_VERBOSE:-0}"

fail() {
  echo "error: $*" >&2
  exit 1
}

note() {
  echo "==> $*"
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "$1 is required but was not found in PATH"
}

[[ "$(uname -s)" == "Darwin" ]] || fail "macOS packaging must run on macOS"

require_command npm
require_command cargo
require_command rustup
require_command hdiutil
require_command xcode-select

xcode-select -p >/dev/null 2>&1 || fail "Xcode Command Line Tools are missing. Run: xcode-select --install"

TAURI_BIN="$ROOT_DIR/node_modules/.bin/tauri"
[[ -x "$TAURI_BIN" ]] || fail "Tauri CLI is missing. Run: npm install"

case "$TARGET" in
  aarch64-apple-darwin | x86_64-apple-darwin)
    REQUIRED_TARGETS=("$TARGET")
    ;;
  universal-apple-darwin)
    REQUIRED_TARGETS=("aarch64-apple-darwin" "x86_64-apple-darwin")
    ;;
  *)
    fail "unsupported MAC_TARGET: $TARGET"
    ;;
esac

INSTALLED_TARGETS="$(rustup target list --installed)"
for REQUIRED_TARGET in "${REQUIRED_TARGETS[@]}"; do
  if ! grep -qx "$REQUIRED_TARGET" <<<"$INSTALLED_TARGETS"; then
    fail "Rust target $REQUIRED_TARGET is not installed. Run: rustup target add $REQUIRED_TARGET"
  fi
done

ARGS=(build)
if [[ "$VERBOSE" == "1" || "$VERBOSE" == "true" ]]; then
  ARGS+=(-vv)
fi
ARGS+=(--bundles "$BUNDLES" --target "$TARGET")
if [[ "$SIGN" != "1" && "$SIGN" != "true" ]]; then
  ARGS+=(--no-sign)
fi

note "Building macOS package"
note "target=$TARGET bundles=$BUNDLES sign=$SIGN"
note "If hdiutil prints 'device not configured', rerun this command from a normal Terminal outside restricted sandboxes."

"$TAURI_BIN" "${ARGS[@]}"

note "Artifacts"
find "target/$TARGET/release/bundle" -maxdepth 3 \( -name "*.dmg" -o -name "*.app" \) -print 2>/dev/null || true
