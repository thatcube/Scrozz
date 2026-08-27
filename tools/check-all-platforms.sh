#!/usr/bin/env bash
# Type-check every crate against all three platforms from one machine.
#
# `cargo check` does not link, so pure-Rust platform bindings need no foreign
# SDK. Native library build scripts are the exception: GTK/ATK-backed Linux
# crates still need target pkg-config metadata when checked from another OS.
# See docs/platforms.md for the exact boundary.
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1
# shellcheck disable=SC1091
source "$HOME/.cargo/env" 2>/dev/null || true

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  cat <<'USAGE'
Usage:
  tools/check-all-platforms.sh                        check all three targets
  tools/check-all-platforms.sh <target> [target...]   check a subset

Environment:
  SCROZZ_XCHECK_EXCLUDE   space-separated crate names to leave out of the
                          *cross* targets only. The host target is always
                          checked in full. See the C build-script note in
                          this script for the one case where that is needed.
USAGE
  exit 0
fi

TARGETS=(
  "aarch64-apple-darwin"
  "x86_64-pc-windows-msvc"
  "x86_64-unknown-linux-gnu"
)

# Any positional arguments replace the default target list, so CI (or a
# developer chasing one platform) can check a subset without editing this file.
if [[ "$#" -gt 0 ]]; then
  TARGETS=("$@")
fi

HOST_TRIPLE="$(rustc -vV 2>/dev/null | awk '/^host: / { print $2 }')"

# --- The C build-script problem --------------------------------------------
#
# `cargo check` does not link, but it *does* run build scripts, and a build
# script that compiles C compiles it for the *target*. `rusqlite`'s `bundled`
# feature builds sqlite3.c, so checking `scrozz-store` (and `scrozz`, which
# depends on it) against a foreign target needs a cross C toolchain and sysroot
# that no ordinary machine has:
#
#   fatal error: 'stdlib.h' file not found      # cc --target=x86_64-pc-windows-msvc
#
# That is a toolchain limitation, not a defect in Scrozz. Setting
# SCROZZ_XCHECK_EXCLUDE lets the cross targets skip those crates while the host
# target still checks everything. The Windows bindings and non-GTK Linux
# platform crates remain cross-checkable; scrozz-shell's complete Linux path is
# checked by native Linux CI.
#
# Drop the exclusion the day `rusqlite` stops bundling its own sqlite.
EXCLUDES=()
if [[ -n "${SCROZZ_XCHECK_EXCLUDE:-}" ]]; then
  # Deliberately unquoted: the variable is a space-separated crate list.
  # shellcheck disable=SC2206
  EXCLUDES=(${SCROZZ_XCHECK_EXCLUDE})
fi

LOG_DIR="target/xcheck-logs"
mkdir -p "$LOG_DIR"

failed=0
FAILED_TARGETS=()

for target in "${TARGETS[@]}"; do
  echo "=== $target ==="

  args=(check --workspace --target "$target")
  if [[ "$target" != "$HOST_TRIPLE" && "${#EXCLUDES[@]}" -gt 0 ]]; then
    for crate in "${EXCLUDES[@]}"; do
      args+=(--exclude "$crate")
    done
    echo "  (cross target: excluding ${EXCLUDES[*]} — see the C build-script note in this script)"
  fi

  log="$LOG_DIR/$target.log"

  # A separate target dir per platform stops artifacts thrashing each other.
  if CARGO_TARGET_DIR="target/xcheck-$target" \
       cargo "${args[@]}" 2>&1 | tee "$log" | tail -n 15; then
    echo "  ok"
  else
    echo "  FAILED"
    echo "  full log: $log"

    # Name the failure mode rather than leaving somebody to infer it at 2am.
    if grep -q "error occurred in cc-rs" "$log" 2>/dev/null; then
      echo
      echo "  This is a C build script cross-compiling to $target."
      echo "  It means a missing cross toolchain, NOT a bug in your Rust code."
      echo "  Re-run with the offending crate excluded, for example:"
      echo "    SCROZZ_XCHECK_EXCLUDE='scrozz-store scrozz' $0 $target"
    elif grep -q "pkg-config has not been configured to support cross-compilation" "$log" 2>/dev/null; then
      echo
      echo "  This target needs native-library pkg-config metadata (GTK/ATK for scrozz-shell)."
      echo "  A foreign host cannot check that crate without a target sysroot."
      echo "  Check the Rust-only Linux paths here, then rely on native Linux CI for scrozz-shell:"
      echo "    cargo check --target $target -p scrozz-core -p scrozz-capture -p scrozz-record -p scrozz-ocr -p scrozz-ui --all-targets"
    elif grep -q "may not be installed" "$log" 2>/dev/null; then
      echo
      echo "  The target's standard library is missing. Install it with:"
      echo "    rustup target add $target"
    fi

    failed=1
    FAILED_TARGETS+=("$target")
  fi
  echo
done

# Leave a readable verdict on the GitHub Actions run summary, not just in the
# scrollback of a job nobody expands.
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  {
    echo "### Cross-target type check"
    echo
    echo "| Target | Result |"
    echo "|---|---|"
    for target in "${TARGETS[@]}"; do
      result="ok"
      for bad in ${FAILED_TARGETS[@]+"${FAILED_TARGETS[@]}"}; do
        [[ "$bad" == "$target" ]] && result="**FAILED**"
      done
      echo "| \`$target\` | $result |"
    done
    echo
    if [[ "$failed" != "0" ]]; then
      echo "Reproduce locally — this needs no Windows or Linux machine:"
      echo
      echo '```sh'
      echo "tools/check-all-platforms.sh ${FAILED_TARGETS[*]}"
      echo '```'
    fi
  } >>"$GITHUB_STEP_SUMMARY"
fi

exit "$failed"
