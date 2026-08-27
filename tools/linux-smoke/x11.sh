#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "SKIP: X11 smoke requires Linux (found $(uname -s))"
  exit 0
fi
if [[ "${XDG_SESSION_TYPE:-}" != "x11" || -z "${DISPLAY:-}" ]]; then
  echo "SKIP: X11 smoke requires XDG_SESSION_TYPE=x11 and DISPLAY"
  exit 0
fi
if ! command -v cargo >/dev/null 2>&1; then
  echo "FAIL: cargo is required in a matching X11 session" >&2
  exit 1
fi

exec cargo run --quiet -p scrozz-shell --example linux_overlay_smoke -- x11
