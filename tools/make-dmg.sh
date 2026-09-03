#!/usr/bin/env bash
# Build a conventional drag-to-Applications macOS disk image.
set -euo pipefail

cd "$(dirname "$0")/.."
REPO_ROOT="$PWD"

if [[ "$#" -ne 2 ]]; then
  echo "Usage: tools/make-dmg.sh SOURCE_APP OUTPUT_DMG" >&2
  exit 2
fi

APP="$1"
OUTPUT="$2"
VOLUME_NAME="${SCROZZ_DMG_VOLUME_NAME:-Scrozz}"

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
if [[ -z "$VOLUME_NAME" || "$VOLUME_NAME" == */* ]]; then
  echo "make-dmg: volume name must be nonempty and contain no slash" >&2
  exit 1
fi

mkdir -p "$(dirname "$OUTPUT")"
APP="$(cd "$(dirname "$APP")" && pwd -P)/$(basename "$APP")"
OUTPUT_DIR="$(cd "$(dirname "$OUTPUT")" && pwd -P)"
OUTPUT="$OUTPUT_DIR/$(basename "$OUTPUT")"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/scrozz-dmg.XXXXXX")"
WORK="$(cd "$WORK" && pwd -P)"
LEGAL="$WORK/.background"
NO_INDEX="$WORK/.metadata_never_index"
MOUNT="$WORK/mount"
BACKGROUND="$REPO_ROOT/packaging/macos/dmg-background.png"
BACKGROUND_2X="$REPO_ROOT/packaging/macos/dmg-background@2x.png"
RETINA_BACKGROUND="$WORK/dmg-background.tiff"
SETTINGS="$REPO_ROOT/packaging/macos/dmg-settings.py"
DMGBUILD_WHEEL="$REPO_ROOT/packaging/macos/vendor/dmgbuild-1.6.7-py3-none-any.whl"
DS_STORE_WHEEL="$REPO_ROOT/packaging/macos/vendor/ds_store-1.3.3-py3-none-any.whl"
ALIAS_WHEEL="$REPO_ROOT/packaging/macos/vendor/mac_alias-2.2.3-py3-none-any.whl"
cleanup() {
  if mount | grep -Fq " on $MOUNT "; then
    echo "make-dmg: retaining work directory because its verification image is still mounted: $WORK" >&2
    return
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

sha256() {
  shasum -a 256 "$1" | awk '{print $1}'
}

[[ -f "$BACKGROUND" ]] || {
  echo "make-dmg: installer background is missing" >&2
  exit 1
}
[[ -f "$BACKGROUND_2X" ]] || {
  echo "make-dmg: Retina installer background is missing" >&2
  exit 1
}
[[ -f "$SETTINGS" ]] || {
  echo "make-dmg: installer settings are missing" >&2
  exit 1
}
[[ "$(sha256 "$DMGBUILD_WHEEL")" == \
  "37ee5771c377beb3203d9164aae8046ffed8531c06edf9227f5788b3c599b1bf" ]] || {
  echo "make-dmg: dmgbuild wheel checksum mismatch" >&2
  exit 1
}
[[ "$(sha256 "$DS_STORE_WHEEL")" == \
  "b92a371efbf1b4ccce2a04d1ed13fceacc4736c81ba09cf5aefb74c088160a35" ]] || {
  echo "make-dmg: ds_store wheel checksum mismatch" >&2
  exit 1
}
[[ "$(sha256 "$ALIAS_WHEEL")" == \
  "7362b521d2132ef92f606a37abfed5fcd849ceb2f28b6f9743e014b02af92f0d" ]] || {
  echo "make-dmg: mac_alias wheel checksum mismatch" >&2
  exit 1
}

if [[ -e "/Volumes/$VOLUME_NAME" ]]; then
  echo "make-dmg: /Volumes/$VOLUME_NAME already exists; eject it before packaging" >&2
  exit 1
fi

mkdir -p "$LEGAL/Legal" "$MOUNT"
touch "$NO_INDEX"
for file in LICENSE TRADEMARK.md README.md; do
  [[ -f "$file" ]] && cp "$file" "$LEGAL/Legal/"
done

rm -f "$OUTPUT"
tiffutil -cathidpicheck "$BACKGROUND" "$BACKGROUND_2X" -out "$RETINA_BACKGROUND"
DEFINES=(
  -D "app=$APP"
  -D "background=$RETINA_BACKGROUND"
  -D "legal=$LEGAL"
  -D "no_index=$NO_INDEX"
  -D "volume_icon=$REPO_ROOT/assets/icons/Scrozz.icns"
)
if [[ -f "$(dirname "$APP")/PREVIEW.txt" ]]; then
  DEFINES+=(-D "preview=$(dirname "$APP")/PREVIEW.txt")
fi
PYTHONPATH="$DMGBUILD_WHEEL:$DS_STORE_WHEEL:$ALIAS_WHEEL" \
  python3 tools/run-dmgbuild.py \
    --detach-retries 8 \
    --no-hidpi \
    --settings "$SETTINGS" \
    "${DEFINES[@]}" \
    "$VOLUME_NAME" \
    "$OUTPUT"
hdiutil verify "$OUTPUT" >/dev/null

PYTHONPATH="$DMGBUILD_WHEEL:$DS_STORE_WHEEL:$ALIAS_WHEEL" \
  python3 tools/verify-dmg.py "$OUTPUT" "$MOUNT" "$VOLUME_NAME"

echo "built: $OUTPUT"
