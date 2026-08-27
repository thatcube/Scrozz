#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "SKIP: KDE Wayland smoke requires Linux (found $(uname -s))"
  exit 0
fi
desktop="$(printf '%s' "${XDG_CURRENT_DESKTOP:-}" | tr '[:upper:]' '[:lower:]')"
if [[ "${XDG_SESSION_TYPE:-}" != "wayland" || -z "${WAYLAND_DISPLAY:-}" ]]; then
  echo "SKIP: KDE Wayland smoke requires XDG_SESSION_TYPE=wayland and WAYLAND_DISPLAY"
  exit 0
fi
if [[ ! "$desktop" =~ (^|:)(kde|plasma)(:|$) ]]; then
  echo "SKIP: KDE Wayland smoke requires XDG_CURRENT_DESKTOP=KDE or Plasma"
  exit 0
fi
if ! command -v cargo >/dev/null 2>&1; then
  echo "FAIL: cargo is required in a matching KDE Wayland session" >&2
  exit 1
fi

exec cargo run --quiet -p scrozz-shell --example linux_overlay_smoke -- kde-wayland
