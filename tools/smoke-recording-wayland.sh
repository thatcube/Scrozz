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
  cargo build -p scrozz --features linux-recording,rav1e-fallback
  BIN="target/debug/scrozz"
fi
if [[ "$BIN" != /* ]]; then
  BIN="$ROOT/$BIN"
fi

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/scrozz-record-wayland.XXXXXX")"
OUTPUT="$SCRATCH/wayland-smoke.mp4"
SOCKET="$SCRATCH/scrozz.sock"
OWNER_OUT="$SCRATCH/owner.json"
OWNER_ERR="$SCRATCH/owner.err"
OWNER_PID=""

cleanup() {
  set +e
  if [[ -n "$OWNER_PID" ]] && kill -0 "$OWNER_PID" 2>/dev/null; then
    kill "$OWNER_PID"
    wait "$OWNER_PID" 2>/dev/null
  fi
  rm -f "$OUTPUT" "$SOCKET" "$OWNER_OUT" "$OWNER_ERR" "$SCRATCH/stop.json"
  rmdir "$SCRATCH" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

export XDG_SESSION_TYPE=wayland
export SCROZZ_IPC_SOCKET="$SOCKET"
echo "recording-wayland-smoke: compositor=${XDG_CURRENT_DESKTOP:-unknown}; approve the portal picker"
echo "recording-wayland-smoke: mixed-scale layouts are checked for geometry, not fractional-coordinate conversion"

"$BIN" --json record --all-displays --fps 10 --quality 60 \
  --resolution 50% --codec "${SCROZZ_SMOKE_CODEC:-auto}" --output "$OUTPUT" \
  >"$OWNER_OUT" 2>"$OWNER_ERR" &
OWNER_PID=$!

for _ in {1..900}; do
  if [[ -S "$SOCKET" ]]; then
    break
  fi
  if ! kill -0 "$OWNER_PID" 2>/dev/null; then
    echo "recording-wayland-smoke: recorder exited during portal setup" >&2
    cat "$OWNER_ERR" >&2
    exit 1
  fi
  sleep 0.1
done
if [[ ! -S "$SOCKET" ]]; then
  echo "recording-wayland-smoke: portal selection did not complete within 90 seconds" >&2
  exit 1
fi

sleep 3
"$BIN" --json record --stop >"$SCRATCH/stop.json"
wait "$OWNER_PID"
OWNER_PID=""

test -s "$OUTPUT"
ffprobe -v error -select_streams v:0 \
  -show_entries stream=codec_name,width,height \
  -show_entries format=format_name,duration \
  -of json "$OUTPUT" >/dev/null
grep -q '"salvageability":"playable"' "$SCRATCH/stop.json"

echo "recording-wayland-smoke: portal/PipeWire recording is playable"
