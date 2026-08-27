#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "SKIP: wlroots smoke requires Linux (found $(uname -s))"
  exit 0
fi
desktop="$(printf '%s' "${XDG_CURRENT_DESKTOP:-}" | tr '[:upper:]' '[:lower:]')"
if [[ "${XDG_SESSION_TYPE:-}" != "wayland" || -z "${WAYLAND_DISPLAY:-}" ]]; then
  echo "SKIP: wlroots smoke requires XDG_SESSION_TYPE=wayland and WAYLAND_DISPLAY"
  exit 0
fi
if [[ ! "$desktop" =~ (^|:)(sway|hyprland|river|wayfire)(:|$) ]]; then
  echo "SKIP: wlroots smoke requires sway, Hyprland, river, or Wayfire"
  exit 0
fi
if ! command -v cargo >/dev/null 2>&1; then
  echo "FAIL: cargo is required in a matching wlroots session" >&2
  exit 1
fi

exec cargo run --quiet -p scrozz-shell --example linux_overlay_smoke -- wlroots
