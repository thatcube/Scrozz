#!/usr/bin/env bash
# Exercise the built binary on the machine it was built for.
#
# # Why this exists
#
# Layer 1 (tools/check-all-platforms.sh) proves the other two platforms still
# *compile*. `cargo test` proves the units behave. Neither runs the shipped
# artifact, and neither can: a `cargo check` never links, and a unit test never
# resolves a data directory, opens the real SQLite file, binds an IPC socket or
# drives the GUI's event loop to a deadline. Every one of those is a thing that
# works on the developer's Mac and can be broken on Windows by a path separator.
#
# So this script runs the actual binary, on the actual runner, and asserts the
# *documented contract* rather than a hoped-for success. That distinction is the
# whole design:
#
#   - Phase 0 is a tree of contracts. Most commands correctly refuse to work.
#     A smoke test that demanded exit 0 everywhere would either fail forever or
#     force someone to weaken it into meaninglessness.
#   - So a check passes when the binary produces the *documented* status for
#     this platform. `history list` returning exit 12 is a pass, and a valuable
#     one — see the store check below for why.
#   - A check that needs hardware or a permission CI does not have SKIPs, loudly
#     and by name. It never passes by default. `docs/platforms.md` layer 4 is
#     the only thing that can cover those, and pretending otherwise here would
#     be worse than not testing at all.
#
# # The one status that is never acceptable
#
# Exit 101 is a Rust panic. Per the module docs in apps/scrozz/src/platform.rs,
# the entire point of the platform seam is that an unfinished backend reaches
# the user as a clean `NotImplemented`, never as a crash. Any check that exits
# 101 (or dies on a signal, 128+n) is a hard failure regardless of what else it
# printed, and is reported as such.
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1
# shellcheck disable=SC1091
source "$HOME/.cargo/env" 2>/dev/null || true

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  cat <<'USAGE'
Usage:
  tools/smoke.sh [path/to/scrozz]

Runs the native smoke checks against a built binary. With no argument, uses
$SCROZZ_BIN, else target/release/scrozz (plus .exe on Windows).

Every check asserts the documented contract for the current platform. Checks
that need a desktop, a display server or a TCC grant are skipped by name and
never silently pass.

Exit status:
  0   every check passed or skipped
  1   at least one check failed
USAGE
  exit 0
fi

# --- locating the binary ----------------------------------------------------

BIN="${1:-${SCROZZ_BIN:-}}"
if [[ -z "$BIN" ]]; then
  if [[ "${CI:-}" != "true" &&
        "${GITHUB_ACTIONS:-}" != "true" &&
        "${SCROZZ_CARGO_LEASE_HELD:-0}" != "1" &&
        -z "${CARGO_TARGET_DIR:-}" ]]; then
    echo "smoke: refusing an unowned target/release binary." >&2
    echo "smoke: build and test one leased binary with: tools/dev.sh smoke" >&2
    exit 1
  fi
  TARGET_DIR="${CARGO_TARGET_DIR:-target}"
  BIN="$TARGET_DIR/release/scrozz"
  [[ -x "$BIN" ]] || BIN="$TARGET_DIR/release/scrozz.exe"
fi

if [[ ! -x "$BIN" ]]; then
  echo "smoke: no executable at '$BIN'." >&2
  echo "smoke: build and test one leased binary with: tools/dev.sh smoke" >&2
  exit 1
fi
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"

case "${RUNNER_OS:-$(uname -s)}" in
  Darwin | macOS) OS="macos" ;;
  Linux) OS="linux" ;;
  Windows | MINGW* | MSYS* | CYGWIN*) OS="windows" ;;
  *) OS="unknown" ;;
esac

# --- isolation --------------------------------------------------------------
#
# A private socket path per run, for a reason found the hard way: with the
# default path this script picks up *any* Scrozz already listening on the
# machine — a developer's real menu-bar app, or a previous run that has not
# finished dying — and the GUI check then exits 13 (`ipc-failed`) having tested
# nothing. On a CI runner that is a rare flake; on a developer's Mac it is
# reproducible and confusing. An isolated socket makes the check depend on this
# binary and nothing else.
SMOKE_TMP="$(mktemp -d 2>/dev/null || mktemp -d -t scrozz-smoke)"
cleanup() { rm -rf "$SMOKE_TMP"; }
trap cleanup EXIT

export SCROZZ_IPC_SOCKET="$SMOKE_TMP/scrozz.sock"
export HOME="$SMOKE_TMP/home"
export XDG_CONFIG_HOME="$SMOKE_TMP/config"
export XDG_DATA_HOME="$SMOKE_TMP/data"
mkdir -p "$HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME"

PASS=0
FAIL=0
SKIP=0
ROWS=""

note() { echo "::notice title=smoke::$1"; }

row() { ROWS="${ROWS}| $1 | $2 | $3 |
"; }

pass() {
  PASS=$((PASS + 1))
  echo "  ok       $1"
  row "$1" "pass" "$2"
}

fail() {
  FAIL=$((FAIL + 1))
  echo "  FAILED   $1 — $2" >&2
  row "$1" "**fail**" "$2"
}

skip() {
  SKIP=$((SKIP + 1))
  echo "  skipped  $1 — $2"
  note "skipped '$1' on $OS: $2"
  row "$1" "skip" "$2"
}

# Runs the binary, capturing combined output and status.
#
# Combined rather than separate because the stream contract is asserted by the
# unit tests; what matters here is that the process produced the right status
# and a parseable document somewhere.
OUT=""
STATUS=0
run_scrozz() {
  OUT="$("$BIN" "$@" 2>&1)"
  STATUS=$?
  return 0
}

# A panic is never a documented outcome. 128+n means a signal.
crashed() {
  if [[ "$STATUS" -eq 101 ]]; then
    echo "panicked (exit 101)"
    return 0
  fi
  if [[ "$STATUS" -gt 128 ]]; then
    echo "died on signal $((STATUS - 128))"
    return 0
  fi
  return 1
}

# Asserts a documented exit status, catching panics first so that a crash is
# never mistaken for "the wrong but survivable code".
expect_status() {
  local name="$1" want="$2" why="$3" reason
  if reason="$(crashed)"; then
    fail "$name" "$reason — the platform seam must never surface a crash"
    return 1
  fi
  if [[ "$STATUS" != "$want" ]]; then
    fail "$name" "expected exit $want ($why), got $STATUS: $(echo "$OUT" | head -1 | cut -c1-120)"
    return 1
  fi
  return 0
}

# The five-key envelope from apps/scrozz/src/report.rs, checked positionally.
# Both the success and error forms have the same keys in the same order, which
# is the property a consumer relies on, so it is the property worth asserting.
expect_envelope() {
  local name="$1"
  case "$OUT" in
    '{"schema":1,"ok":'*'"command":'*'"data":'*'"error":'*) return 0 ;;
  esac
  fail "$name" "not the stable JSON envelope: $(echo "$OUT" | head -c 120)"
  return 1
}

echo "==> smoke: $BIN"
echo "    platform: $OS"

# --- 1. the binary runs at all ---------------------------------------------

run_scrozz --help
if expect_status "help" 0 "--help always succeeds"; then
  case "$OUT" in
    *Usage:*) pass "help" "prints usage" ;;
    *) fail "help" "no usage block in output" ;;
  esac
fi

run_scrozz --version
if expect_status "version" 0 "--version always succeeds"; then
  case "$OUT" in
    scrozz\ *) pass "version" "$(echo "$OUT" | head -1)" ;;
    *) fail "version" "unexpected version line: $(echo "$OUT" | head -1)" ;;
  esac
fi

# Exit 2 is pinned to clap's own parse failure (see apps/scrozz/src/exit.rs), so
# "bad invocation" is one number whoever objected. Worth asserting because it is
# the one code this project does not get to choose.
run_scrozz --definitely-not-a-real-flag
expect_status "usage-error" 2 "a rejected argument" && pass "usage-error" "exit 2 as documented"

# --- 2. the JSON contract ---------------------------------------------------

run_scrozz --json settings get
if expect_status "json-schema" 0 "reading settings needs no platform support"; then
  expect_envelope "json-schema" && pass "json-schema" "schema 1, five keys, ok=true"
fi

# The error envelope is the half that actually ships today, so assert it too
# rather than trusting that it mirrors the success form.
run_scrozz --json history get 1
if ! crashed >/dev/null; then
  expect_envelope "json-error-envelope" &&
    case "$OUT" in
      *'"ok":false'*'"kind":'*'"code":'*) pass "json-error-envelope" "ok=false with kind and code" ;;
      *) fail "json-error-envelope" "error object missing kind/code" ;;
    esac
else
  fail "json-error-envelope" "$(crashed)"
fi

# --- 3. the store ----------------------------------------------------------
#
# History is implemented. Run it against the isolated profile above so a smoke
# test never migrates or reads the developer's real captures.
run_scrozz --json history list
if expect_status "store-opens" 0 "the store opens and returns an empty first page"; then
  case "$OUT" in
    *'"ok":true'*'"total":0'*'"captures":[]'*) pass "store-opens" "sqlite opened and listed isolated history" ;;
    *) fail "store-opens" "unexpected history response: $(echo "$OUT" | head -c 160)" ;;
  esac
fi

# --- 4. config generation ---------------------------------------------------
#
# Pure computation with no platform dependency, which is the point: it must work
# identically on all three runners even though the compositors it describes only
# exist on one of them. A Windows runner generating a correct sway config is a
# real signal that nothing in this path is accidentally host-conditional.
for compositor in sway hyprland; do
  run_scrozz --json hotkey generate-config --compositor "$compositor"
  if expect_status "hotkey-config-$compositor" 0 "generation is pure computation"; then
    expect_envelope "hotkey-config-$compositor" &&
      case "$OUT" in
        *'"bindings":['*) pass "hotkey-config-$compositor" "emitted bindings" ;;
        *) fail "hotkey-config-$compositor" "no bindings in output" ;;
      esac
  fi
done

# --- 5. the headless GUI lifecycle -----------------------------------------
#
# The real event loop, started and stopped, with no window and no keyboard:
#
#   SCROZZ_GUI_HEADLESS=1  the window-less host (cards are reported, not drawn)
#   SCROZZ_GUI_TRAY=0      no menu-bar item, which CI cannot host
#   SCROZZ_GUI_HOTKEYS=    empty, so the run never registers a global shortcut —
#                          without this an automated run would grab real keys
#                          from whatever else is on the machine
#   SCROZZ_GUI_TIMEOUT_MS  a deadline, so the loop cannot hang the job
#
# The deadline is what makes this safe to run unattended, so the check asserts
# the run actually reached it rather than exiting early for some other reason.
GUI_OUT="$(
  SCROZZ_GUI_HEADLESS=1 \
    SCROZZ_GUI_TRAY=0 \
    SCROZZ_GUI_HOTKEYS='' \
    SCROZZ_GUI_TIMEOUT_MS="${SCROZZ_SMOKE_GUI_MS:-2000}" \
    "$BIN" --json gui 2>&1
)"
GUI_STATUS=$?
if [[ "$GUI_STATUS" -eq 0 ]]; then
  case "$GUI_OUT" in
    *"the run deadline passed"*) pass "gui-headless-lifecycle" "started, ran, hit its deadline, exited 0" ;;
    *) fail "gui-headless-lifecycle" "exited 0 without reaching the deadline: $(echo "$GUI_OUT" | head -c 160)" ;;
  esac
elif [[ "$GUI_STATUS" -eq 101 || "$GUI_STATUS" -gt 128 ]]; then
  fail "gui-headless-lifecycle" "crashed with status $GUI_STATUS"
else
  case "$GUI_OUT" in
    *'"kind":"unsupported"'* | *'"kind":"permission-denied"'*)
      skip "gui-headless-lifecycle" "the runner refused the GUI host: $(echo "$GUI_OUT" | head -c 120)"
      ;;
    *) fail "gui-headless-lifecycle" "exit $GUI_STATUS: $(echo "$GUI_OUT" | head -c 160)" ;;
  esac
fi

# --- 6. decode + OCR --------------------------------------------------------
#
# One command covers both halves of the pipeline: `scrozz ocr <file>` decodes a
# PNG through scrozz-export (the single decode path) and hands the frame to the
# system recogniser. The input is a committed golden image, so the check is
# deterministic and needs no fixture generation.
#
# scrozz_ocr::SystemOcr::is_available() is true on macOS and Windows only, so
# Linux has no engine to test. That is a documented platform boundary, not a
# failure — but the *refusal* is still worth asserting, because a clean
# `NotImplemented`/`Unsupported` and a panic are very different outcomes.
OCR_FIXTURE="crates/scrozz-ui/snapshots/golden/stack-single--rest.png"
if [[ ! -f "$OCR_FIXTURE" ]]; then
  skip "decode-ocr" "fixture $OCR_FIXTURE is not present"
else
  run_scrozz --json ocr "$OCR_FIXTURE"
  if crashed >/dev/null; then
    fail "decode-ocr" "$(crashed)"
  elif [[ "$STATUS" -eq 0 ]]; then
    case "$OUT" in
      *'"block_count":0'*) fail "decode-ocr" "decoded but recognised nothing in a text-bearing image" ;;
      *'"block_count":'*) pass "decode-ocr" "decoded PNG and recognised text ($(echo "$OUT" | sed -n 's/.*"block_count":\([0-9]*\).*/\1/p') blocks)" ;;
      *) fail "decode-ocr" "exit 0 without a block count: $(echo "$OUT" | head -c 160)" ;;
    esac
  else
    case "$OUT" in
      *'"kind":"not-implemented"'* | *'"kind":"unsupported"'*)
        if [[ "$OS" == "linux" ]]; then
          skip "decode-ocr" "no OCR engine on Linux (SystemOcr::is_available() is macOS/Windows only); refusal was clean, exit $STATUS"
        else
          fail "decode-ocr" "$OS has an OCR engine but the binary refused: $(echo "$OUT" | head -c 160)"
        fi
        ;;
      *'"kind":"permission-denied"'*)
        skip "decode-ocr" "the runner withheld a permission the recogniser needs"
        ;;
      *) fail "decode-ocr" "exit $STATUS: $(echo "$OUT" | head -c 160)" ;;
    esac
  fi
fi

# --- 7. display enumeration -------------------------------------------------
#
# Deliberately not asserted as a success anywhere. Enumeration goes through
# capture_guard(), so it is live on macOS and a guarded NotImplemented on the
# other two — and on macOS it needs a window server the runner may or may not
# provide. Both real outcomes are documented, and a runner without a desktop
# must skip rather than fail.
run_scrozz --json list displays
if crashed >/dev/null; then
  fail "list-displays" "$(crashed)"
elif [[ "$STATUS" -eq 0 ]]; then
  case "$OUT" in
    *'"ok":true'*) pass "list-displays" "enumerated a live display list" ;;
    *) fail "list-displays" "exit 0 with a non-success envelope" ;;
  esac
else
  case "$OUT" in
    *'"kind":"not-implemented"'*)
      pass "list-displays" "guarded backend refused cleanly (exit $STATUS), as documented off macOS"
      ;;
    *'"kind":"permission-denied"'* | *'"kind":"unsupported"'* | *'"kind":"platform"'*)
      skip "list-displays" "no usable desktop on this runner: $(echo "$OUT" | head -c 120)"
      ;;
    *) fail "list-displays" "undocumented failure, exit $STATUS: $(echo "$OUT" | head -c 160)" ;;
  esac
fi

# --- 8. capture -------------------------------------------------------------
#
# Never attempted. On macOS the backend is stable and unguarded, so invoking it
# would try a real screen capture — which needs a TCC Screen Recording grant
# that a hosted runner does not have and cannot be given non-interactively, and
# which would write into the runner's Pictures folder if it somehow succeeded.
# Layer 4 in docs/platforms.md is the only honest place for this.
skip "capture" "needs an interactive desktop session and a Screen Recording grant; layer 4 territory"

# --- summary ----------------------------------------------------------------

echo
echo "==> smoke: $PASS passed, $SKIP skipped, $FAIL failed"

if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  {
    echo "### Native smoke — $OS"
    echo
    echo "| check | result | detail |"
    echo "| --- | --- | --- |"
    printf '%s' "$ROWS"
    echo
    echo "$PASS passed, $SKIP skipped, $FAIL failed."
    echo
    echo "_Skips are platform boundaries, not passes. Capture and any check needing"
    echo "a desktop session are covered by layer 4 in \`docs/platforms.md\`._"
  } >>"$GITHUB_STEP_SUMMARY"
fi

[[ "$FAIL" -eq 0 ]]
