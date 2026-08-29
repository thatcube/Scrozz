#!/usr/bin/env bash
# Exercise decoded video and captured audio through the native preview runtime.
set -euo pipefail

cd "$(dirname "$0")/.."
source "$HOME/.cargo/env" 2>/dev/null || true

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "playback smoke skipped; requires macOS"
  exit 0
fi

if [[ "${SCROZZ_PLAYBACK_SMOKE:-0}" != "1" ]]; then
  echo "playback smoke skipped; set SCROZZ_PLAYBACK_SMOKE=1 (plays a quiet two-second fixture)"
  exit 0
fi

cargo run -p scrozz-record --example macos_playback_smoke
