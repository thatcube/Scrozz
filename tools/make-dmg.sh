#!/usr/bin/env bash
# Build a conventional drag-to-Applications macOS disk image.
set -euo pipefail

cd "$(dirname "$0")/.."

if [[ "$#" -ne 2 ]]; then
  echo "Usage: tools/make-dmg.sh SOURCE_APP OUTPUT_DMG" >&2
  exit 2
fi

APP="$1"
OUTPUT="$2"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "make-dmg: macOS is required" >&2
  exit 1
fi
if [[ ! -d "$APP/Contents" || "$(basename "$APP")" != *.app ]]; then
  echo "make-dmg: '$APP' is not an application bundle" >&2
  exit 1
fi
if [[ "$OUTPUT" != *.dmg ]]; then
  echo "make-dmg: output must end in .dmg" >&2
  exit 1
fi

mkdir -p "$(dirname "$OUTPUT")"
STAGE="$(mktemp -d "${TMPDIR:-/tmp}/scrozz-dmg-stage.XXXXXX")"
MOUNT="$(mktemp -d "${TMPDIR:-/tmp}/scrozz-dmg-mount.XXXXXX")"
MOUNTED=0
cleanup() {
  if [[ "$MOUNTED" == "1" ]]; then
    hdiutil detach "$MOUNT" -force >/dev/null 2>&1 || true
  fi
  rm -rf "$STAGE" "$MOUNT"
}
trap cleanup EXIT

ditto "$APP" "$STAGE/Scrozz.app"
ln -s /Applications "$STAGE/Applications"
for file in LICENSE TRADEMARK.md README.md; do
  [[ -f "$file" ]] && cp "$file" "$STAGE/"
done
if [[ -f "$(dirname "$APP")/PREVIEW.txt" ]]; then
  cp "$(dirname "$APP")/PREVIEW.txt" "$STAGE/"
fi

rm -f "$OUTPUT"
hdiutil create \
  -quiet \
  -volname Scrozz \
  -srcfolder "$STAGE" \
  -format UDZO \
  -ov \
  "$OUTPUT"
hdiutil verify "$OUTPUT" >/dev/null

hdiutil attach "$OUTPUT" -readonly -nobrowse -mountpoint "$MOUNT" >/dev/null
MOUNTED=1
[[ -L "$MOUNT/Applications" ]] || {
  echo "make-dmg: Applications shortcut is missing" >&2
  exit 1
}
codesign --verify --strict --verbose=2 "$MOUNT/Scrozz.app"
hdiutil detach "$MOUNT" >/dev/null
MOUNTED=0

echo "built: $OUTPUT"
