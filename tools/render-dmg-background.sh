#!/usr/bin/env bash
# Regenerate the committed standard and Retina DMG backgrounds with bundled Inter.
set -euo pipefail

cd "$(dirname "$0")/.."

command -v python3 >/dev/null || {
  echo "render-dmg-background: python3 is required" >&2
  exit 1
}
python3 -c 'import cairosvg' 2>/dev/null || {
  echo "render-dmg-background: Python package cairosvg is required" >&2
  exit 1
}

WORK="$(mktemp -d "${TMPDIR:-/tmp}/scrozz-dmg-art.XXXXXX")"
cleanup() {
  rm -rf "$WORK"
}
trap cleanup EXIT

FONT_DIR="$PWD/crates/scrozz-ui/assets/fonts"
cat >"$WORK/fonts.conf" <<EOF
<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
<fontconfig>
  <dir>$FONT_DIR</dir>
  <cachedir>$WORK/cache</cachedir>
</fontconfig>
EOF

FONTCONFIG_FILE="$WORK/fonts.conf" python3 - <<'PY'
import cairosvg

source = "packaging/macos/dmg-background.svg"
cairosvg.svg2png(
    url=source,
    write_to="packaging/macos/dmg-background.png",
    output_width=720,
    output_height=460,
)
cairosvg.svg2png(
    url=source,
    write_to="packaging/macos/dmg-background@2x.png",
    output_width=1440,
    output_height=920,
)
PY

echo "rendered standard and Retina DMG backgrounds with bundled Inter"
