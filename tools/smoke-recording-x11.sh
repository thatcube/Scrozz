#!/usr/bin/env bash
# Record a real X11 desktop through the shared recording contract and probe it.
set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "recording-x11-smoke: skipped (requires Linux)"
  exit 0
fi

for command in Xvfb xdpyinfo xsetroot ffprobe; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "recording-x11-smoke: skipped ($command is not installed)"
    exit 0
  fi
done

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

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/scrozz-record-x11.XXXXXX")"
OUTPUT="$SCRATCH/x11-smoke.mp4"
OWNER_OUT="$SCRATCH/owner.json"
OWNER_ERR="$SCRATCH/owner.err"
XVFB_LOG="$SCRATCH/xvfb.log"
DISPLAY_NUMBER=":$((90 + ($$ % 900)))"
XVFB_PID=""

cleanup() {
  set +e
  if [[ -n "$XVFB_PID" ]] && kill -0 "$XVFB_PID" 2>/dev/null; then
    kill "$XVFB_PID"
    wait "$XVFB_PID" 2>/dev/null
  fi
  rm -f "$OUTPUT" "$OWNER_OUT" "$OWNER_ERR" "$XVFB_LOG"
  rmdir "$SCRATCH" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

Xvfb "$DISPLAY_NUMBER" -screen 0 1280x720x24 -ac -nolisten tcp >"$XVFB_LOG" 2>&1 &
XVFB_PID=$!
for _ in {1..100}; do
  if DISPLAY="$DISPLAY_NUMBER" xdpyinfo >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 "$XVFB_PID" 2>/dev/null; then
    echo "recording-x11-smoke: Xvfb exited during startup" >&2
    cat "$XVFB_LOG" >&2
    exit 1
  fi
  sleep 0.05
done
DISPLAY="$DISPLAY_NUMBER" xdpyinfo >/dev/null

export DISPLAY="$DISPLAY_NUMBER"
export XDG_SESSION_TYPE=x11
unset WAYLAND_DISPLAY
xsetroot -solid "#315b7d"

if ! SCROZZ_SMOKE_CODEC=av1 \
  SCROZZ_SMOKE_FPS=5 \
  SCROZZ_SMOKE_SCALE=25 \
  SCROZZ_SMOKE_DURATION=2 \
  SCROZZ_SMOKE_PAUSE=1 \
  "$BIN" "$OUTPUT" >"$OWNER_OUT" 2>"$OWNER_ERR"; then
  cat "$OWNER_ERR" >&2
  exit 1
fi

test -s "$OUTPUT"
ffprobe -v error -select_streams v:0 \
  -show_entries stream=codec_name,width,height \
  -show_entries format=format_name,duration \
  -of json "$OUTPUT" >/dev/null
grep -q '^completion=complete$' "$OWNER_OUT"
grep -q '^salvageability=playable$' "$OWNER_OUT"

echo "recording-x11-smoke: recorded and probed a 320x180 AV1 fragmented MP4"
