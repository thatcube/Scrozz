#!/usr/bin/env bash
# Measure the CPU-bound shutter-to-usable-card path without live screen access.
set -euo pipefail

cd "$(dirname "$0")/.."

if [[ "${SCROZZ_CARGO_LEASE_HELD:-0}" != "1" &&
      "${CI:-}" != "true" &&
      "${GITHUB_ACTIONS:-}" != "true" &&
      -z "${CARGO_TARGET_DIR:-}" ]]; then
  exec tools/cargo-pool.sh "$0" "$@"
fi

CONSTRAINED=0
if [[ "${1:-}" == "--constrained" ]]; then
  CONSTRAINED=1
  shift
fi
if [[ "$#" -ne 0 ]]; then
  echo "usage: tools/benchmark-capture-readiness.sh [--constrained]" >&2
  exit 2
fi

echo "host: $(uname -smr)"
if [[ "$(uname -s)" == "Darwin" ]]; then
  echo "cpu: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo Apple Silicon)"
fi

COMMAND=(
  cargo test
  --release
  --locked
  -p scrozz
  --all-features
  --bin scrozz
  benchmark_capture_readiness
  --
  --ignored
  --nocapture
)

if [[ "$CONSTRAINED" == "1" && "$(uname -s)" == "Darwin" ]]; then
  echo "policy: macOS background QoS"
  exec taskpolicy -b "${COMMAND[@]}"
fi

echo "policy: normal"
exec "${COMMAND[@]}"
