#!/usr/bin/env bash
# Interactive smoke test for a real Wayland compositor and ScreenCast portal.
set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "recording-wayland-smoke: skipped (requires Linux)"
  exit 0
fi
if [[ -z "${WAYLAND_DISPLAY:-}" ]]; then
  echo "recording-wayland-smoke: skipped (no live Wayland session)"
  exit 0
fi
if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then
  echo "recording-wayland-smoke: skipped (no session D-Bus for xdg-desktop-portal)"
  exit 0
fi
if [[ "${SCROZZ_WAYLAND_SMOKE:-0}" != "1" ]]; then
  echo "recording-wayland-smoke: skipped (set SCROZZ_WAYLAND_SMOKE=1; the portal picker is interactive)"
  exit 0
fi
if ! command -v ffprobe >/dev/null 2>&1; then
  echo "recording-wayland-smoke: skipped (ffprobe is not installed)"
  exit 0
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

BIN="${SCROZZ_RECORD_BIN:-}"
if [[ -z "$BIN" ]]; then
  cargo build -p scrozz-record --example linux_record_smoke \
    --features linux-native,rav1e-fallback
  BIN="target/debug/examples/linux_record_smoke"
fi
if [[ "$BIN" != /* ]]; then
  BIN="$ROOT/$BIN"
fi

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/scrozz-record-wayland.XXXXXX")"
OUTPUT="$SCRATCH/wayland-smoke.mp4"
OWNER_OUT="$SCRATCH/owner.json"
OWNER_ERR="$SCRATCH/owner.err"

cleanup() {
  set +e
  rm -f "$OUTPUT" "$OWNER_OUT" "$OWNER_ERR"
  rmdir "$SCRATCH" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

export XDG_SESSION_TYPE=wayland
echo "recording-wayland-smoke: compositor=${XDG_CURRENT_DESKTOP:-unknown}; approve the portal picker"
echo "recording-wayland-smoke: mixed-scale layouts are checked for geometry, not fractional-coordinate conversion"

if ! SCROZZ_SMOKE_CODEC="${SCROZZ_SMOKE_CODEC:-auto}" \
  SCROZZ_SMOKE_FPS=10 \
  SCROZZ_SMOKE_SCALE=50 \
  SCROZZ_SMOKE_DURATION=3 \
  "$BIN" "$OUTPUT" >"$OWNER_OUT" 2>"$OWNER_ERR"; then
  cat "$OWNER_ERR" >&2
  exit 1
fi

test -s "$OUTPUT"
ffprobe -v error -select_streams v:0 \
  -show_entries stream=codec_name,width,height \
  -show_entries format=format_name,duration \
  -of json "$OUTPUT" >/dev/null
grep -q '^salvageability=playable$' "$OWNER_OUT"

echo "recording-wayland-smoke: portal/PipeWire recording is playable"
