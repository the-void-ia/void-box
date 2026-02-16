#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: scripts/logo_to_ascii.sh <image-path> [output-path] [width]" >&2
  echo "example: scripts/logo_to_ascii.sh /tmp/void-box.png assets/logo/void-box.txt 80" >&2
  exit 1
fi

IMG_PATH="$1"
OUT_PATH="${2:-assets/logo/void-box.txt}"
WIDTH="${3:-80}"

if [[ ! -f "$IMG_PATH" ]]; then
  echo "error: image not found: $IMG_PATH" >&2
  exit 1
fi

mkdir -p "$(dirname "$OUT_PATH")"

if command -v chafa >/dev/null 2>&1; then
  chafa "$IMG_PATH" \
    --size "${WIDTH}x0" \
    --symbols vhalf \
    --format symbols \
    --colors none \
    --relative off \
    > "$OUT_PATH"
  echo "wrote ASCII logo with chafa -> $OUT_PATH"
  exit 0
fi

if command -v jp2a >/dev/null 2>&1; then
  jp2a --width="$WIDTH" "$IMG_PATH" > "$OUT_PATH"
  echo "wrote ASCII logo with jp2a -> $OUT_PATH"
  exit 0
fi

cat >&2 <<MSG
error: no ASCII renderer found.
install one of:
  - chafa
  - jp2a
MSG
exit 1
