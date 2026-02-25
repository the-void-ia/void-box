#!/usr/bin/env bash
# Cargo runner for voidbox: on macOS, codesigns the binary before executing.
# Required because Apple's Virtualization.framework needs com.apple.security.virtualization.
#
# Used via .cargo/config.toml [[bin]] runner for voidbox.

set -euo pipefail
BINARY="$1"
shift

if [[ "$(uname -s)" == "Darwin" ]]; then
  ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
  ENTITLEMENTS="${ROOT}/voidbox.entitlements"
  if [[ -f "$ENTITLEMENTS" ]]; then
    codesign --force --sign - --entitlements "$ENTITLEMENTS" "$BINARY" 2>/dev/null || true
  fi
fi

exec "$BINARY" "$@"
