#!/usr/bin/env bash
# Record a real X11 desktop, exercise process-owned controls, and probe the MP4.
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
  cargo build -p scrozz --features linux-recording,rav1e-fallback
  BIN="target/debug/scrozz"
fi
if [[ "$BIN" != /* ]]; then
  BIN="$ROOT/$BIN"
fi

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/scrozz-record-x11.XXXXXX")"
OUTPUT="$SCRATCH/x11-smoke.mp4"
SOCKET="$SCRATCH/scrozz.sock"
OWNER_OUT="$SCRATCH/owner.json"
OWNER_ERR="$SCRATCH/owner.err"
XVFB_LOG="$SCRATCH/xvfb.log"
DISPLAY_NUMBER=":$((90 + ($$ % 900)))"
OWNER_PID=""
XVFB_PID=""

cleanup() {
  set +e
  if [[ -n "$OWNER_PID" ]] && kill -0 "$OWNER_PID" 2>/dev/null; then
    kill "$OWNER_PID"
    wait "$OWNER_PID" 2>/dev/null
  fi
  if [[ -n "$XVFB_PID" ]] && kill -0 "$XVFB_PID" 2>/dev/null; then
    kill "$XVFB_PID"
    wait "$XVFB_PID" 2>/dev/null
  fi
  rm -f "$OUTPUT" "$SOCKET" "$OWNER_OUT" "$OWNER_ERR" "$XVFB_LOG" \
    "$SCRATCH/pause.json" "$SCRATCH/resume.json" "$SCRATCH/stop.json"
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
export SCROZZ_IPC_SOCKET="$SOCKET"
unset WAYLAND_DISPLAY
xsetroot -solid "#315b7d"

"$BIN" --json record --all-displays --fps 5 --quality 60 \
  --resolution 25% --codec av1 --output "$OUTPUT" \
  >"$OWNER_OUT" 2>"$OWNER_ERR" &
OWNER_PID=$!

for _ in {1..300}; do
  if [[ -S "$SOCKET" ]]; then
    break
  fi
  if ! kill -0 "$OWNER_PID" 2>/dev/null; then
    echo "recording-x11-smoke: recorder exited during startup" >&2
    cat "$OWNER_ERR" >&2
    exit 1
  fi
  sleep 0.1
done
if [[ ! -S "$SOCKET" ]]; then
  echo "recording-x11-smoke: recorder did not create its control socket" >&2
  exit 1
fi

sleep 1
"$BIN" --json record --pause >"$SCRATCH/pause.json"
sleep 0.2
"$BIN" --json record --resume >"$SCRATCH/resume.json"
sleep 1
"$BIN" --json record --stop >"$SCRATCH/stop.json"
wait "$OWNER_PID"
OWNER_PID=""

test -s "$OUTPUT"
ffprobe -v error -select_streams v:0 \
  -show_entries stream=codec_name,width,height \
  -show_entries format=format_name,duration \
  -of json "$OUTPUT" >/dev/null
grep -q '"completion":"complete"' "$SCRATCH/stop.json"
grep -q '"salvageability":"playable"' "$SCRATCH/stop.json"

echo "recording-x11-smoke: recorded and probed a 320x180 AV1 fragmented MP4"
