#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "SKIP: GNOME Wayland smoke requires Linux (found $(uname -s))"
  exit 0
fi
desktop="$(printf '%s' "${XDG_CURRENT_DESKTOP:-}" | tr '[:upper:]' '[:lower:]')"
if [[ "${XDG_SESSION_TYPE:-}" != "wayland" || -z "${WAYLAND_DISPLAY:-}" ]]; then
  echo "SKIP: GNOME Wayland smoke requires XDG_SESSION_TYPE=wayland and WAYLAND_DISPLAY"
  exit 0
fi
if [[ ! "$desktop" =~ (^|:)(gnome|gnome-classic|ubuntu|pop)(:|$) ]]; then
  echo "SKIP: GNOME Wayland smoke requires a GNOME-family XDG_CURRENT_DESKTOP"
  exit 0
fi
if ! command -v cargo >/dev/null 2>&1; then
  echo "FAIL: cargo is required in a matching GNOME Wayland session" >&2
  exit 1
fi

exec cargo run --quiet -p scrozz-shell --example linux_overlay_smoke -- gnome-wayland
