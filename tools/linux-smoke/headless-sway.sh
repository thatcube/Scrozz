#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "SKIP: headless Sway smoke requires Linux (found $(uname -s))"
  exit 0
fi
if ! command -v sway >/dev/null 2>&1; then
  echo "SKIP: headless Sway smoke requires the CI-installable sway package"
  exit 0
fi
if ! command -v cargo >/dev/null 2>&1; then
  echo "FAIL: cargo is required for the headless Sway smoke" >&2
  exit 1
fi

runtime_dir="$(mktemp -d "${TMPDIR:-/tmp}/scrozz-sway.XXXXXX")"
chmod 700 "$runtime_dir"
config="$runtime_dir/config"
log="$runtime_dir/sway.log"
printf '%s\n' \
  'output HEADLESS-1 resolution 1280x720' \
  'default_border none' \
  'focus_follows_mouse no' >"$config"

WLR_BACKENDS=headless \
WLR_RENDERER=pixman \
WLR_LIBINPUT_NO_DEVICES=1 \
XDG_RUNTIME_DIR="$runtime_dir" \
sway --unsupported-gpu --config "$config" >"$log" 2>&1 &
sway_pid=$!

cleanup() {
  if kill -0 "$sway_pid" 2>/dev/null; then
    kill "$sway_pid"
    wait "$sway_pid" 2>/dev/null || true
  fi
  rm -r -- "$runtime_dir"
}
trap cleanup EXIT

wayland_socket=""
for _ in $(seq 1 100); do
  wayland_socket="$(find "$runtime_dir" -maxdepth 1 -type s -name 'wayland-*' -print -quit)"
  if [[ -n "$wayland_socket" ]]; then
    break
  fi
  if ! kill -0 "$sway_pid" 2>/dev/null; then
    echo "FAIL: headless Sway exited before creating a Wayland socket" >&2
    sed 's/^/  /' "$log" >&2
    exit 1
  fi
  sleep 0.1
done

if [[ -z "$wayland_socket" ]]; then
  echo "FAIL: headless Sway did not create a Wayland socket within 10 seconds" >&2
  sed 's/^/  /' "$log" >&2
  exit 1
fi

XDG_RUNTIME_DIR="$runtime_dir" \
WAYLAND_DISPLAY="$(basename "$wayland_socket")" \
XDG_SESSION_TYPE=wayland \
XDG_CURRENT_DESKTOP=sway \
cargo run --quiet -p scrozz-shell --example linux_overlay_smoke -- wlroots
