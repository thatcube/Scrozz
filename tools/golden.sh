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
# The harness is part of the product contract now. If its corpus disappears,
# this script fails rather than reporting a green run that compared no pixels.
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
GOLDEN_TEST="$CRATE_DIR/tests/golden.rs"
if [[ ! -f "$GOLDEN_TEST" ]] ||
  ! grep -q 'fn golden_corpus_matches_baselines' "$GOLDEN_TEST"; then
  note "### Golden images: FAILED"
  note ""
  note "The required corpus test \`golden_corpus_matches_baselines\` is missing"
  note "from \`$GOLDEN_TEST\`. Restore it before accepting UI changes."
  if [[ -n "${GITHUB_ACTIONS:-}" ]]; then
    echo "::error title=Golden corpus missing::The D25 screenshot harness has no corpus test to run."
  fi
  exit 1
fi

# --- Run --------------------------------------------------------------------
GOLDEN_PLATFORM="${RUNNER_OS:-$(uname -s)}"

if [[ "$UPDATE" == "1" ]]; then
  # The repository's GoldenStore reads this.
  export UPDATE_SNAPSHOTS=1
  echo "golden: re-recording baselines on $GOLDEN_PLATFORM"
fi

echo "golden: running $CRATE tests (platform=$GOLDEN_PLATFORM)"
cargo test --package "$CRATE" --tests -- --nocapture
status=$?

if [[ "$UPDATE" == "1" ]]; then
  echo "golden: baselines rewritten. Review the diff before committing —"
  echo "  'the test passes now' and 'the UI is correct now' are different claims."
  exit "$status"
fi

if [[ "$status" == "0" ]]; then
  echo "golden: all baselines match on $GOLDEN_PLATFORM"
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
    \( -path '*snapshots*' -o -name '*.actual.*' -o -name '*.compare.*' -o -name '*.diff.*' \) \
    -print0 2>/dev/null
)

note "### Golden images: FAILED on ${GOLDEN_PLATFORM}"
note ""
if [[ "$found" == "0" ]]; then
  note "The tests failed but produced no images, so this is probably a *compile*"
  note "or panic failure rather than a pixel diff. Read the job log above."
else
  note "Collected **$found** image(s). Download the \`golden-${GOLDEN_PLATFORM}\`"
  note "artifact from this run's summary page and compare them:"
  note ""
  note "- \`<name>.png\` — the committed baseline (what we expected)"
  note "- \`<name>.actual.png\` — what this run actually rendered"
  note "- \`<name>.diff.png\` — the per-pixel difference"
  note "- \`<name>.compare.png\` — expected, actual and diff side by side"
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
