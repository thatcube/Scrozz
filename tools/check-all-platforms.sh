#!/usr/bin/env bash
# Type-check every crate against all three platforms from one machine.
#
# `cargo check` does not link, so it needs no Windows SDK and no Linux sysroot.
# That makes Windows and Linux platform code genuinely verifiable from a Mac:
# the compiler checks it against the real `windows`, `x11rb` and `ashpd`
# bindings, so a misused API is a compile error here rather than a surprise in
# CI. It proves nothing about runtime behaviour — see docs/platforms.md.
set -uo pipefail

cd "$(dirname "$0")/.."
source "$HOME/.cargo/env" 2>/dev/null || true

TARGETS=(
  "aarch64-apple-darwin"
  "x86_64-pc-windows-msvc"
  "x86_64-unknown-linux-gnu"
)

failed=0
for target in "${TARGETS[@]}"; do
  echo "=== $target ==="
  # A separate target dir per platform stops artifacts thrashing each other.
  if CARGO_TARGET_DIR="target/xcheck-$target" \
       cargo check --workspace --target "$target" 2>&1 | tail -n 15; then
    echo "  ok"
  else
    echo "  FAILED"
    failed=1
  fi
  echo
done

exit "$failed"
