#!/usr/bin/env bash
set -euo pipefail

# Codesign locally-built voidbox binaries with the virtualization entitlement.
#
# A `cargo build` on macOS produces an unsigned binary, but Apple's
# Virtualization.framework rejects any process that does not carry
# `com.apple.security.virtualization`. Run this after a build to apply
# the entitlement via ad-hoc signing — no Developer ID required.
#
# Usage:
#   scripts/sign-macos.sh
#
# Idempotent: safe to re-run after every rebuild.

case "$(uname -s)" in
  Darwin) ;;
  *)
    echo "[sign-macos] macOS only — skipping on $(uname -s)"
    exit 0
    ;;
esac

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENTITLEMENTS="${ROOT_DIR}/voidbox.entitlements"

if [[ ! -f "$ENTITLEMENTS" ]]; then
  echo "[sign-macos] ERROR: entitlements file not found: $ENTITLEMENTS" >&2
  exit 1
fi

found_any=0
exit_code=0

for profile in debug release; do
  binary="${ROOT_DIR}/target/${profile}/voidbox"
  if [[ ! -f "$binary" ]]; then
    continue
  fi
  found_any=1

  if ! codesign --force --sign - --entitlements "$ENTITLEMENTS" "$binary" >/dev/null 2>&1; then
    echo "[sign-macos] FAIL: codesign failed for $binary"
    exit_code=1
    continue
  fi

  if codesign --display --entitlements - "$binary" 2>/dev/null \
      | grep -q "com.apple.security.virtualization"; then
    echo "[sign-macos] OK: signed $binary"
  else
    echo "[sign-macos] FAIL: virtualization entitlement missing on $binary"
    exit_code=1
  fi
done

if [[ "$found_any" -eq 0 ]]; then
  echo "[sign-macos] no voidbox binary found under target/{debug,release} — build first"
  exit 1
fi

exit "$exit_code"
