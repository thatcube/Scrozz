#!/usr/bin/env bash
# Exercise the release executable without touching the user's real state.
set -euo pipefail

cd "$(dirname "$0")/.."

binary="${1:-${SCROZZ_BIN:-}}"
if [[ -z "$binary" ]]; then
  binary="target/release/scrozz"
  [[ -x "$binary" ]] || binary="target/release/scrozz.exe"
fi
if [[ ! -x "$binary" ]]; then
  echo "smoke: no executable at '$binary'" >&2
  exit 1
fi
binary="$(cd "$(dirname "$binary")" && pwd -P)/$(basename "$binary")"

root="$(mktemp -d "${TMPDIR:-/tmp}/scrozz-smoke.XXXXXX")"
cleanup() {
  rm -rf "$root"
}
trap cleanup EXIT

export SCROZZ_CONFIG_DIR="$root/config"
export SCROZZ_CONFIG_HOME="$root/config-home"
export SCROZZ_DATA_HOME="$root/data"
export SCROZZ_HOME="$root/home"
export USER="scrozz-smoke-$$"
mkdir -p "$SCROZZ_CONFIG_DIR" "$SCROZZ_CONFIG_HOME" "$SCROZZ_DATA_HOME" "$SCROZZ_HOME"

platform="${RUNNER_OS:-${OS:-$(uname -s)}}"
case "$platform" in
  Windows | Windows_NT | MINGW* | MSYS* | CYGWIN*)
    # A filesystem path is not a valid Windows named-pipe endpoint.
    export SCROZZ_IPC_SOCKET="\\\\.\\pipe\\scrozz-smoke-$$"
    ;;
  *)
    export SCROZZ_IPC_SOCKET="$root/scrozz.sock"
    ;;
esac

"$binary" --help >/dev/null
"$binary" --version >/dev/null
"$binary" --json settings get capture.cursor |
  grep -Fq '"value":"false"'

gui_output="$(
  SCROZZ_GUI_HEADLESS=1 \
    SCROZZ_GUI_TRAY=0 \
    SCROZZ_GUI_HOTKEYS='' \
    SCROZZ_GUI_TIMEOUT_MS=150 \
    "$binary" --json gui
)"
grep -Fq '"command":"gui"' <<<"$gui_output"
grep -Fq 'the run deadline passed' <<<"$gui_output"

echo "native smoke checks passed"
