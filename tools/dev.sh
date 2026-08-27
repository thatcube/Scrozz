#!/usr/bin/env bash
# The one entry point for working on Scrozz locally.
#
#   tools/dev.sh <command>
#
# Everything CI does, you can do here first, with the same flags. If a command
# passes locally and fails in CI, that is a bug in this script and worth
# reporting — the two are meant to be the same thing.
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1
# shellcheck disable=SC1091
source "$HOME/.cargo/env" 2>/dev/null || true

# Crates whose build scripts compile C, which cannot cross-compile without a
# foreign toolchain. See tools/check-all-platforms.sh for the full explanation.
XCHECK_EXCLUDE="scrozz-store scrozz-ui scrozz"

usage() {
  cat <<'USAGE'
Scrozz developer commands

  Everyday
    tools/dev.sh fmt          format the workspace
    tools/dev.sh check        type-check the workspace for this machine
    tools/dev.sh lint         clippy, warnings denied (what CI enforces)
    tools/dev.sh test         run the test suite
    tools/dev.sh build        debug build of the app

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
cmd_build() { cargo build --workspace; }

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
