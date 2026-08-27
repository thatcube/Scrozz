#!/usr/bin/env bash
# Run the Wayland capture smoke test, or say clearly why it could not run.
#
# The unit tests in crates/scrozz-capture/tests/linux.rs cover every part of the
# Wayland path that can be checked without a compositor, and they run everywhere.
# This script covers the other half: the PipeWire C ABI, the SPA POD the server
# actually accepts, the portal dialog, and the restore-token round trip. None of
# those can be exercised without a real session.
#
# The important property is what happens when it cannot run. A CI job that skips
# and exits 0 is indistinguishable from one that passed, so this exits 77 — the
# automake "skipped" convention — with a reason on stderr. Anything that treats
# a non-zero exit as failure will notice; anything that only looks for zero will
# not silently record a pass for a test that never ran.
#
# Usage:
#   tools/wayland-smoke.sh              run it, or exit 77 with a reason
#   tools/wayland-smoke.sh --require    turn every skip into a failure
#   tools/wayland-smoke.sh --require --stale-token
#                                        exercise invalidation and one retry;
#                                        requires an isolated XDG_STATE_HOME
#
# CI, on a native Ubuntu runner with a Wayland session:
#   tools/ci-linux-deps.sh
#   tools/wayland-smoke.sh
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1
# shellcheck disable=SC1091
source "$HOME/.cargo/env" 2>/dev/null || true

EXIT_SKIP=77
REQUIRE=0
STALE_TOKEN=0
for argument in "$@"; do
  case "$argument" in
    --require) REQUIRE=1 ;;
    --stale-token) STALE_TOKEN=1 ;;
    -h|--help)
      sed -n '2,28p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "wayland-smoke: unknown argument '$argument'. Try --help." >&2
      exit 2
      ;;
  esac
done

# Emits a skip, or a failure when --require was passed. Every skip path goes
# through here so the two behaviours cannot drift apart.
skip() {
  if [[ "$REQUIRE" == "1" ]]; then
    echo "wayland-smoke: FAIL (--require): $1" >&2
    exit 1
  fi
  echo "wayland-smoke: SKIP: $1" >&2
  echo "  This test did NOT run. Exiting $EXIT_SKIP so that is not mistaken for a pass." >&2
  exit "$EXIT_SKIP"
}

# --- Preconditions ---------------------------------------------------------
#
# Each of these is checked here rather than in the example so the reason names
# the missing piece. A missing PipeWire surfaced as "capture failed" sends
# somebody looking for a bug in the wrong place.

if [[ "$(uname -s)" != "Linux" ]]; then
  skip "this is a Linux-only test and the host is $(uname -s)."
fi

if [[ -z "${WAYLAND_DISPLAY:-}" ]]; then
  skip "WAYLAND_DISPLAY is unset, so there is no Wayland session. On X11 the X11
  backend is used instead and none of this code path applies."
fi

if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" && ! -S "${XDG_RUNTIME_DIR:-/nonexistent}/bus" ]]; then
  skip "no D-Bus session bus, so xdg-desktop-portal cannot be reached. This is
  normal in a container or over a bare SSH session."
fi

# The library, not the daemon: the backend dlopens it, so its absence is a
# skip with an install command rather than a crash.
# Do not use grep -q here: under pipefail its early exit can give ldconfig
# SIGPIPE and turn a successful lookup into pipeline status 141.
if ! ldconfig -p 2>/dev/null | grep 'libpipewire-0\.3\.so\.0' >/dev/null; then
  skip "libpipewire-0.3.so.0 was not found by ldconfig. Install it with:
      sudo apt-get install libpipewire-0.3-0
  On Fedora the package is pipewire-libs; on Arch, pipewire."
fi

# The daemon has to be running as well — the library alone connects to nothing.
if [[ ! -S "${XDG_RUNTIME_DIR:-/nonexistent}/pipewire-0" ]]; then
  skip "no PipeWire socket at \$XDG_RUNTIME_DIR/pipewire-0, so the daemon is not
  running. Start it with:
      systemctl --user start pipewire pipewire-session-manager"
fi

# Finally the portal itself. `busctl` is part of systemd and present on any
# machine that has a session bus at all, so its absence is not worth a skip of
# its own — the check is simply relaxed.
if command -v busctl >/dev/null 2>&1; then
  if ! busctl --user status org.freedesktop.portal.Desktop >/dev/null 2>&1; then
    skip "org.freedesktop.portal.Desktop is not on the session bus. Install the
  backend for your compositor:
      GNOME    xdg-desktop-portal-gnome
      KDE      xdg-desktop-portal-kde
      wlroots  xdg-desktop-portal-wlr    (sway, Hyprland, river)
  plus the common xdg-desktop-portal package."
  fi
fi

echo "wayland-smoke: preconditions met"
echo "  compositor:  ${XDG_CURRENT_DESKTOP:-<unset>} / ${XDG_SESSION_TYPE:-<unset>}"
echo "  display:     ${WAYLAND_DISPLAY}"
echo

# Build once, then exercise exactly those bytes for every process in the
# restore matrix. Re-running `cargo run` could silently rebuild between stages.
if ! cargo build --locked --release --package scrozz-capture --example wayland-smoke; then
  echo "wayland-smoke: release example build failed" >&2
  exit 1
fi
artifact="${CARGO_TARGET_DIR:-target}/release/examples/wayland-smoke"
if [[ ! -x "$artifact" ]]; then
  echo "wayland-smoke: built example is missing or not executable at $artifact" >&2
  exit 1
fi
source_sha=$(git rev-parse HEAD) || exit 1
if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
  echo "wayland-smoke: tracked source is dirty; commit it before recording native evidence." >&2
  exit 1
fi
binary_sha=$(sha256sum "$artifact" | awk '{print $1}') || exit 1
echo "wayland-smoke: build provenance"
echo "  source SHA:  $source_sha"
echo "  binary:      $artifact"
echo "  binary SHA:  $binary_sha"
echo "  rustc:       $(rustc --version)"
echo "  cargo:       $(cargo --version)"
echo

if [[ "$STALE_TOKEN" == "1" ]]; then
  if [[ -z "${XDG_STATE_HOME:-}" || "${XDG_STATE_HOME}" != /* ]]; then
    skip "--stale-token requires an absolute, isolated XDG_STATE_HOME so the
  operator's real portal grants are never overwritten."
  fi
  if ! restore_key=$("$artifact" --require --print-restore-key); then
    echo "wayland-smoke: could not derive the selected display's exact token key" >&2
    exit 1
  fi
  if [[ "$restore_key" != display:* ]]; then
    echo "wayland-smoke: invalid exact-display token key '$restore_key'" >&2
    exit 1
  fi
  token_dir="$XDG_STATE_HOME/scrozz"
  if ! mkdir -p "$token_dir"; then
    echo "wayland-smoke: could not create isolated token directory $token_dir" >&2
    exit 1
  fi
  if ! printf '%s\n' \
    '# Intentionally invalid token for the Wayland stale-restore smoke path.' \
    "$restore_key"$'\tscrozz-intentionally-stale-restore-token' \
    >"$token_dir/portal-tokens"; then
    echo "wayland-smoke: could not write the isolated stale token" >&2
    exit 1
  fi
  echo "wayland-smoke: planted an intentionally stale token for $restore_key in isolated state"
fi

# --- Run -------------------------------------------------------------------
#
# `--` separates cargo's arguments from the example's. The example repeats the
# precondition checks it can see from Rust; that redundancy is deliberate, since
# it can also be run directly by a developer who bypassed this script.
args=()
if [[ "$REQUIRE" == "1" ]]; then
  args+=("--require")
fi

trace_log=$(mktemp "${TMPDIR:-/tmp}/scrozz-wayland-smoke.XXXXXX") || exit 1
restore_log=$(mktemp "${TMPDIR:-/tmp}/scrozz-wayland-restore.XXXXXX") || {
  rm -f "$trace_log"
  exit 1
}
trap 'rm -f "$trace_log" "$restore_log"' EXIT

RUST_LOG="${RUST_LOG:-warn},scrozz_capture=debug" \
  "$artifact" "${args[@]}" 2>&1 | tee "$trace_log"
status=${PIPESTATUS[0]}

if [[ "$STALE_TOKEN" == "1" && "$status" == "0" ]]; then
  retry_count=$(grep -Fc \
    'the stored portal restore token was not accepted; asking again without it' \
    "$trace_log" || true)
  if [[ "$retry_count" != "1" ]]; then
    echo "wayland-smoke: stale-token run observed $retry_count classified retries; expected exactly 1." >&2
    status=1
  else
    echo "wayland-smoke: stale restore token was rejected and retried exactly once"
  fi
fi

if [[ "$status" == "0" ]]; then
  issued_count=$(grep -Fc 'the desktop portal issued a restore token' "$trace_log" || true)
  if [[ "$issued_count" == "0" ]]; then
    echo "wayland-smoke: the portal issued no persistent restore token." >&2
    status=1
  else
    rotated_count=$(grep -Fc 'replaced=true' "$trace_log" || true)
    echo "wayland-smoke: portal issued $issued_count restore token response(s); $rotated_count replaced a prior token"
  fi
fi

if [[ "$status" == "0" && ! -t 0 ]]; then
  echo "wayland-smoke: cannot prove silent restore without an interactive terminal confirming that no picker appeared." >&2
  status=1
fi

if [[ "$status" == "0" ]] && ! command -v timeout >/dev/null 2>&1; then
  echo "wayland-smoke: GNU timeout is required to bound the fresh-process restore check." >&2
  status=1
fi

if [[ "$status" == "0" ]]; then
  echo
  echo "wayland-smoke: starting a fresh process with the token persisted to disk"
  echo "wayland-smoke: OBSERVE THE DESKTOP — no portal picker should appear this time."
  RUST_LOG="${RUST_LOG:-warn},scrozz_capture=debug" \
    timeout --signal=TERM --kill-after=5s 90s \
    "$artifact" "${args[@]}" --fresh-process-restore 2>&1 | tee "$restore_log"
  status=${PIPESTATUS[0]}
  if [[ "$status" == "124" || "$status" == "137" ]]; then
    echo "wayland-smoke: fresh-process restore timed out; an ignored token may have opened an unattended picker." >&2
    status=1
  fi
fi

if [[ "$status" == "0" ]]; then
  stored_count=$(grep -Fc 'stored_token=true' "$restore_log" || true)
  fresh_retry_count=$(grep -Fc \
    'the stored portal restore token was not accepted; asking again without it' \
    "$restore_log" || true)
  if [[ "$stored_count" == "0" || "$fresh_retry_count" != "0" ]]; then
    echo "wayland-smoke: fresh-process restore saw stored_token=$stored_count and retries=$fresh_retry_count; expected one loaded token with no classified rejection." >&2
    status=1
  elif ! grep -Fq \
    'fresh-process restore succeeded and its session teardown completed' \
    "$restore_log"; then
    echo "wayland-smoke: fresh-process restore did not reach clean session teardown." >&2
    status=1
  else
    fresh_picker=""
    if ! read -r -p "wayland-smoke: Did a portal picker appear for the fresh process? Type 'no' to confirm silent restore: " fresh_picker; then
      echo "wayland-smoke: no operator confirmation was received." >&2
      status=1
    elif [[ "${fresh_picker,,}" != "no" ]]; then
      echo "wayland-smoke: fresh-process restore was not confirmed picker-free." >&2
      status=1
    else
      echo "wayland-smoke: operator confirmed that the fresh process restored without a picker"
    fi
  fi
fi

case "$status" in
  0)
    echo "wayland-smoke: PASS — a real frame was captured through PipeWire."
    ;;
  "$EXIT_SKIP")
    echo "wayland-smoke: the test skipped from inside the example; see the reason above." >&2
    ;;
  *)
    echo "wayland-smoke: FAILED (exit $status)." >&2
    echo "  Re-run with RUST_LOG=scrozz_capture=debug for the portal and PipeWire trace." >&2
    ;;
esac

exit "$status"
