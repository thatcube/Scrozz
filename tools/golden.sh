#!/usr/bin/env bash
# Run the D25 headless golden-image tests, and make a failure *inspectable*.
#
#   tools/golden.sh            run the golden tests
#   tools/golden.sh --update   re-record the baselines from current rendering
#
# Why this is a script and not three lines of YAML
# ------------------------------------------------
# A pixel-diff failure reported as "assertion failed: images differ" is useless.
# Somebody has to be able to *look* at what changed. So on failure this collects
# every baseline, every freshly rendered image and every diff into one directory
# that CI uploads as an artifact, and prints where they went.
#
# It also has to survive the harness not existing yet, so the honest behaviour
# is to skip loudly rather than train people to ignore a permanently red job.
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1
# shellcheck disable=SC1091
source "$HOME/.cargo/env" 2>/dev/null || true

UPDATE=0
if [[ "${1:-}" == "--update" ]]; then
  UPDATE=1
fi

CRATE="scrozz-ui"
CRATE_DIR="crates/scrozz-ui"
ARTIFACT_DIR="target/golden-artifacts"

# Tell CI a thing worth reading on the run summary page, not just in the log.
note() {
  echo "$*"
  if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    echo "$*" >>"$GITHUB_STEP_SUMMARY"
  fi
}

# --- Is there anything to run? ---------------------------------------------
#
# Two independent signals, because either one alone gives a false answer:
#   * the public harness module supplies the deterministic software renderer;
#   * the golden integration test drives the committed corpus.
have_harness=0
have_tests=0

if [[ -f "$CRATE_DIR/src/lib.rs" ]] && grep -q '^pub mod harness;' "$CRATE_DIR/src/lib.rs"; then
  have_harness=1
fi

if [[ -f "$CRATE_DIR/tests/golden.rs" ]]; then
  have_tests=1
fi

if [[ "$have_harness" == "0" || "$have_tests" == "0" ]]; then
  note "### Golden images: skipped"
  note ""
  note "The headless screenshot harness (decision D25) is not wired up yet, so"
  note "there is nothing to diff. This is expected during Phase 0 and is **not**"
  note "a failure."
  note ""
  note "| Precondition | Found |"
  note "|---|---|"
  note "| public \`scrozz_ui::harness\` module | $([[ $have_harness == 1 ]] && echo yes || echo '**no**') |"
  note "| a test file in \`$CRATE_DIR/tests/\` | $([[ $have_tests == 1 ]] && echo yes || echo '**no**') |"
  note ""
  note "This job starts enforcing itself the moment both are present: the"
  note "deterministic harness module and \`$CRATE_DIR/tests/golden.rs\`."
  note "Baselines belong in \`$CRATE_DIR/snapshots/golden/\`."
  if [[ -n "${GITHUB_ACTIONS:-}" ]]; then
    echo "::notice title=Golden images skipped::The D25 screenshot harness is not implemented yet; no baselines to diff."
  fi
  exit 0
fi

# --- Run --------------------------------------------------------------------
#
# The harness can use this to pick a per-platform baseline directory. Font
# rasterisation, hinting and DPI rounding genuinely differ between macOS,
# Windows and Linux, so one committed baseline cannot be correct on all three
# — and pretending otherwise produces the permanently-red suite D25 warns about.
export SCROZZ_GOLDEN_PLATFORM="${RUNNER_OS:-$(uname -s)}"

if [[ "$UPDATE" == "1" ]]; then
  # The env var egui_kittest itself reads.
  export UPDATE_SNAPSHOTS=1
  echo "golden: re-recording baselines for $SCROZZ_GOLDEN_PLATFORM"
fi

echo "golden: running $CRATE tests (platform=$SCROZZ_GOLDEN_PLATFORM)"
cargo test --package "$CRATE" --tests -- --nocapture
status=$?

if [[ "$UPDATE" == "1" ]]; then
  echo "golden: baselines rewritten. Review the diff before committing —"
  echo "  'the test passes now' and 'the UI is correct now' are different claims."
  exit "$status"
fi

if [[ "$status" == "0" ]]; then
  echo "golden: all baselines match on $SCROZZ_GOLDEN_PLATFORM"
  exit 0
fi

# --- Collect the evidence ---------------------------------------------------
echo "golden: tests failed — collecting images into $ARTIFACT_DIR"
rm -rf "$ARTIFACT_DIR"
mkdir -p "$ARTIFACT_DIR"

found=0
while IFS= read -r -d '' img; do
  dest="$ARTIFACT_DIR/${img#./}"
  mkdir -p "$(dirname "$dest")"
  cp "$img" "$dest"
  found=$((found + 1))
done < <(
  find ./crates ./target -type f \
    \( -name '*.png' -o -name '*.webp' \) \
    \( -path '*snapshots*' -o -name '*.new.*' -o -name '*.diff.*' -o -name '*.old.*' \) \
    -print0 2>/dev/null
)

note "### Golden images: FAILED on ${SCROZZ_GOLDEN_PLATFORM}"
note ""
if [[ "$found" == "0" ]]; then
  note "The tests failed but produced no images, so this is probably a *compile*"
  note "or panic failure rather than a pixel diff. Read the job log above."
else
  note "Collected **$found** image(s). Download the \`golden-${SCROZZ_GOLDEN_PLATFORM}\`"
  note "artifact from this run's summary page and compare them:"
  note ""
  note "- \`<name>.png\` — the committed baseline (what we expected)"
  note "- \`<name>.new.png\` — what this run actually rendered"
  note "- \`<name>.diff.png\` — the per-pixel difference"
  note ""
  note "If the new rendering is **correct**, re-record and commit the baseline:"
  note ""
  note '```sh'
  note "tools/golden.sh --update"
  note '```'
  note ""
  note "Do that on the platform whose baseline changed. If the change was not"
  note "intended, you have found a visual regression — which is the entire point."
fi

exit 1
