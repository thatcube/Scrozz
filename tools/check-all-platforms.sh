#!/usr/bin/env bash
# Type-check every crate against all three platforms from one machine.
#
# `cargo check` does not link, so Rust bindings need no Windows SDK or Linux
# linker. Build scripts still run, however, and native `-sys` dependencies need
# target pkg-config metadata even for a check. That distinction matters when a
# macOS host reaches scrozz-shell's Linux GTK/libappindicator dependencies. See
# docs/platforms.md for the exact coverage and limitation.
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
                          checked in full. See the native build-script notes
                          in this script for the cases where that is needed.
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

# --- Native build-script limits --------------------------------------------
#
# `cargo check` does not link, but it *does* run build scripts, and a build
# script that compiles C compiles it for the *target*. `rusqlite`'s `bundled`
# feature builds sqlite3.c, so checking `scrozz-store` (and `scrozz`, which
# depends on it) against a foreign target needs a cross C toolchain and sysroot
# that no ordinary machine has:
#
#   fatal error: 'stdlib.h' file not found      # cc --target=x86_64-pc-windows-msvc
#
# A second class does not compile C but still probes target-native libraries.
# On Linux, scrozz-shell reaches GTK through tray-icon/libappindicator, and the
# glib-sys, gobject-sys, gio-sys, pango-sys and gtk-sys build scripts invoke
# pkg-config. A macOS host has no Linux GLib/GTK sysroot or .pc files, so the
# full Linux workspace check correctly stops there. Setting
# PKG_CONFIG_ALLOW_CROSS would only point the Linux build at Darwin libraries
# and is not a fix.
#
# These are host-toolchain limitations, not defects in Scrozz. Setting
# SCROZZ_XCHECK_EXCLUDE lets the cross targets skip those crates while the host
# target still checks everything. scrozz-store and scrozz are the standard
# exclusions because they contain no platform code. scrozz-shell is deliberately
# not excluded: doing so would make the command green while hiding Linux platform
# code. CI runs this gate on Ubuntu with the native packages installed.
#
# The Wayland implementation remains independently cross-checkable from macOS:
#
#   cargo check --package scrozz-capture --all-targets \
#     --target x86_64-unknown-linux-gnu
#
# Drop the exclusion the day `rusqlite` stops bundling its own sqlite.
EXCLUDES=()
if [[ -n "${SCROZZ_XCHECK_EXCLUDE:-}" ]]; then
  # Deliberately unquoted: the variable is a space-separated crate list.
  # shellcheck disable=SC2206
  EXCLUDES=(${SCROZZ_XCHECK_EXCLUDE})
fi

TARGET_ROOT="${CARGO_TARGET_DIR:-target}"
LOG_DIR="$TARGET_ROOT/xcheck-logs"
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

  # A lease wrapper may provide one collision-safe absolute artifact root. Never
  # replace it: Cargo already namespaces cross artifacts by target triple inside
  # that root. Standalone runs retain the historical per-platform directories.
  target_dir="${CARGO_TARGET_DIR:-target/xcheck-$target}"
  if CARGO_TARGET_DIR="$target_dir" cargo "${args[@]}" 2>&1 | tee "$log" | tail -n 15; then
    echo "  ok"
  else
    echo "  FAILED"
    echo "  full log: $log"

    # Name the failure mode rather than leaving somebody to infer it at 2am.
    if grep -q "pkg-config has not been configured to support cross-compilation" "$log" 2>/dev/null; then
      echo
      echo "  A target-native -sys crate needs pkg-config metadata for $target."
      echo "  On macOS, scrozz-shell's Linux tray reaches GLib/GObject/GIO/GTK"
      echo "  through tray-icon/libappindicator, but no Linux sysroot is installed."
      echo "  PKG_CONFIG_ALLOW_CROSS is not a substitute for target libraries."
      echo "  The Wayland capture target can still be checked independently:"
      echo "    cargo check --package scrozz-capture --all-targets --target $target"
      echo "  See docs/platforms.md."
    elif grep -q "error occurred in cc-rs" "$log" 2>/dev/null; then
      echo
      echo "  This is a C build script cross-compiling to $target."
      echo "  It means a missing cross toolchain, NOT a bug in your Rust code."
      echo "  Re-run with the offending crate excluded, for example:"
      echo "    SCROZZ_XCHECK_EXCLUDE='scrozz-store scrozz' $0 $target"
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
