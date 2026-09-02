#!/usr/bin/env bash
# The one entry point for working on Scrozz locally.
#
#   tools/dev.sh <command>
#
# Everything CI does, you can do here first, with the same flags. If a command
# passes locally and fails in CI, that is a bug in this script and worth
# reporting — the two are meant to be the same thing.
#
# On macOS, `build` is also the development deployment path: after a successful
# signed bundle build it installs the canonical app and relaunches it. CI never
# invokes this command, and `SCROZZ_BUILD_NO_LAUNCH=1` keeps the install while
# suppressing the relaunch for an explicitly headless/local build.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd -P)" || exit 1
SCRIPT_PATH="$SCRIPT_DIR/$(basename "$0")"
cd "$SCRIPT_DIR/.." || exit 1
# shellcheck disable=SC1091
source "$HOME/.cargo/env" 2>/dev/null || true

# Local compilation is leased from a bounded pool so parallel worktrees do not
# each retain a full copy of the dependency graph. CI remains job-local, and an
# explicit CARGO_TARGET_DIR remains the caller's responsibility.
case "${1:-help}" in
  fmt | fmt-check | help | -h | --help | linux-deps | lock | update) ;;
  *)
    if [[ "${SCROZZ_CARGO_LEASE_HELD:-0}" != "1" &&
          "${CI:-}" != "true" &&
          "${GITHUB_ACTIONS:-}" != "true" &&
          -z "${CARGO_TARGET_DIR:-}" ]]; then
      exec "$SCRIPT_DIR/cargo-pool.sh" "$SCRIPT_PATH" "$@"
    fi
    ;;
esac

# Crates whose build scripts compile C, which cannot cross-compile without a
# foreign toolchain. See tools/check-all-platforms.sh for the full explanation.
XCHECK_EXCLUDE="scrozz-store scrozz"

usage() {
  cat <<'USAGE'
Scrozz developer commands

  Everyday
    tools/dev.sh fmt          format the workspace
    tools/dev.sh check        type-check the workspace for this machine
    tools/dev.sh lint         clippy, warnings denied (what CI enforces)
    tools/dev.sh test         run the test suite
    tools/dev.sh build        build the app; on macOS install + relaunch it
    tools/dev.sh run -- ...   run the CLI or GUI with Scrozz arguments
    tools/dev.sh lock         refresh Cargo.lock after manifest changes
    tools/dev.sh update ...   update Cargo.lock without creating artifacts
    tools/dev.sh smoke        build and smoke-test one release binary
    tools/dev.sh package      build and package one release binary

  macOS build override
    SCROZZ_BUILD_NO_LAUNCH=1 tools/dev.sh build
                              install the bundle without relaunching it

  Cross-platform (docs/platforms.md)
    tools/dev.sh platforms    type-check macOS + Windows + Linux from here
    tools/dev.sh golden       headless golden-image tests (D25)
    tools/dev.sh golden-update  re-record the golden baselines, then review them

  Supply chain
    tools/dev.sh deny         licence + advisory audit (installs cargo-deny)

  Everything
    tools/dev.sh ci           run the whole local equivalent of CI
    tools/dev.sh linux-deps   install the Linux system packages (Linux only)

  Before pushing, `tools/dev.sh ci` is the short answer.
USAGE
}

step() {
  echo
  echo "── $* ──────────────────────────────────────────" | cut -c1-72
}

# Track results so `ci` can report everything rather than stopping at the first
# failure — the same reason the CI matrix runs with fail-fast disabled. One
# broken thing should not hide three others.
RESULTS=()
run_step() {
  local label="$1"
  shift
  step "$label"
  if "$@"; then
    RESULTS+=("ok   $label")
    return 0
  fi
  RESULTS+=("FAIL $label")
  return 1
}

report() {
  echo
  echo "───────────────────────────────────────────────────────"
  local failed=0
  for r in ${RESULTS[@]+"${RESULTS[@]}"}; do
    echo "  $r"
    [[ "$r" == FAIL* ]] && failed=1
  done
  echo "───────────────────────────────────────────────────────"
  if [[ "$failed" == "1" ]]; then
    echo "Something failed. Scroll up — each step printed its own reason."
    return 1
  fi
  echo "All good."
  return 0
}

cmd_fmt() { cargo fmt --all; }
cmd_fmt_check() { cargo fmt --all -- --check; }
cmd_check() { cargo check --workspace --all-targets; }
cmd_lint() { cargo clippy --workspace --all-targets -- -D warnings; }
cmd_test() { cargo test --workspace; }
cmd_lock() { "$SCRIPT_DIR/cargo-pool.sh" --refresh-lock; }
cmd_update() { "$SCRIPT_DIR/cargo-pool.sh" --update-lock "$@"; }
cmd_run() {
  [[ "${1:-}" == "--" ]] && shift
  cargo run -p scrozz -- "$@"
}

MACOS_APP="/Applications/Scrozz.app"

macos_app_pids() {
  local executable="$MACOS_APP/Contents/MacOS/Scrozz"
  ps -axo pid=,command= |
    awk -v executable="$executable" '$2 == executable { print $1 }'
}

stop_macos_app() {
  local pids pid attempt running
  pids="$(macos_app_pids)"
  [[ -n "$pids" ]] || return 0

  for pid in $pids; do
    case "$pid" in
      "" | *[!0-9]*) continue ;;
    esac
    kill -TERM "$pid" 2>/dev/null || true
  done

  for attempt in {1..50}; do
    running=0
    for pid in $pids; do
      if kill -0 "$pid" 2>/dev/null; then
        running=1
        break
      fi
    done
    [[ "$running" == "0" ]] && return 0
    sleep 0.1
  done

  echo "build: the existing Scrozz process did not stop; the new app was installed but not opened" >&2
  return 1
}

launch_macos_app() {
  local attempt
  echo "==> opening $MACOS_APP"
  open "$MACOS_APP" || return 1
  for attempt in {1..50}; do
    if [[ -n "$(macos_app_pids)" ]]; then
      echo "==> Scrozz is running"
      return 0
    fi
    sleep 0.1
  done
  echo "build: LaunchServices accepted Scrozz, but its process did not stay running" >&2
  return 1
}

cmd_build() {
  if [[ "$(uname -s)" != "Darwin" ]]; then
    cargo build --workspace
    return
  fi

  tools/make-app-bundle.sh "$MACOS_APP" || return
  if [[ "${SCROZZ_BUILD_NO_LAUNCH:-0}" == "1" ]]; then
    echo "==> installed $MACOS_APP (relaunch suppressed)"
    return
  fi
  stop_macos_app || return
  launch_macos_app
}

release_binary() {
  local target_dir="${CARGO_TARGET_DIR:-target}"
  local binary="$target_dir/release/scrozz"
  [[ -x "$binary" ]] || binary="$target_dir/release/scrozz.exe"
  printf '%s\n' "$binary"
}

cmd_smoke() {
  cargo build --release --locked -p scrozz || return
  tools/smoke.sh "$(release_binary)"
}

cmd_package() {
  cargo build --release --locked -p scrozz || return
  SCROZZ_BIN="$(release_binary)" tools/package.sh "$@"
}

cmd_platforms() {
  SCROZZ_XCHECK_EXCLUDE="$XCHECK_EXCLUDE" tools/check-all-platforms.sh
}

cmd_golden() { tools/golden.sh; }
cmd_golden_update() { tools/golden.sh --update; }
cmd_linux_deps() { tools/ci-linux-deps.sh; }

cmd_deny() {
  if ! command -v cargo-deny >/dev/null 2>&1; then
    echo "cargo-deny not found; installing (this takes a couple of minutes once)"
    cargo install --locked cargo-deny || return 1
  fi
  cargo deny --config tools/deny.toml check
}

cmd_ci() {
  # Deliberately mirrors .github/workflows/ci.yml, in the same order.
  run_step "rustfmt"          cmd_fmt_check
  run_step "cross-target check" cmd_platforms
  run_step "clippy"           cmd_lint
  run_step "tests"            cmd_test
  run_step "golden images"    cmd_golden
  run_step "supply chain"     cmd_deny
  report
}

case "${1:-help}" in
  fmt) cmd_fmt ;;
  fmt-check) cmd_fmt_check ;;
  check) cmd_check ;;
  lint | clippy) cmd_lint ;;
  test) cmd_test ;;
  build) cmd_build ;;
  lock) cmd_lock ;;
  update)
    shift
    cmd_update "$@"
    ;;
  run)
    shift
    cmd_run "$@"
    ;;
  smoke) cmd_smoke ;;
  package)
    shift
    cmd_package "$@"
    ;;
  platforms | check-all) cmd_platforms ;;
  golden) cmd_golden ;;
  golden-update) cmd_golden_update ;;
  deny | audit) cmd_deny ;;
  linux-deps) cmd_linux_deps ;;
  ci) cmd_ci ;;
  help | -h | --help) usage ;;
  *)
    echo "Unknown command: $1" >&2
    echo >&2
    usage >&2
    exit 2
    ;;
esac
