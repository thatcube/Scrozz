#!/usr/bin/env bash
# Strict structural checks for the Windows recording engine.
# This proves cross-target compilation, not WGC/MF/WASAPI runtime behaviour.
set -euo pipefail

cd "$(dirname "$0")/.." || exit 1
# shellcheck disable=SC1091
source "$HOME/.cargo/env" 2>/dev/null || true

target="x86_64-pc-windows-msvc"
if ! rustup target list --installed | grep -qx "$target"; then
  echo "missing Rust target: $target" >&2
  echo "install it with: rustup target add $target" >&2
  exit 1
fi

cargo fmt --all -- --check
cargo test -p scrozz-record --test windows
cargo check -p scrozz-record --target "$target" --all-targets
cargo clippy -p scrozz-record --target "$target" --all-targets -- -D warnings

echo "Windows recording checks passed (native desktop smoke test not run)."
