#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
wrapper="$root/tools/native-evidence.sh"
scratch="$(mktemp -d "${TMPDIR:-/tmp}/scrozz-native-evidence-test.XXXXXX")"
trap 'rm -rf "$scratch"' EXIT

source_root="$scratch/source"
mkdir "$source_root"
source_root="$(cd "$source_root" && pwd -P)"
git -C "$source_root" init -q
git -C "$source_root" config user.name "Scrozz Test"
git -C "$source_root" config user.email "scrozz-test@example.invalid"
printf 'clean\n' >"$source_root/fixture.txt"
git -C "$source_root" add fixture.txt
git -C "$source_root" commit -q -m fixture
expected_sha="$(git -C "$source_root" rev-parse HEAD)"

clean_evidence="$scratch/clean-evidence"
(
  cd "$source_root"
  "$wrapper" \
    --output "$clean_evidence" \
    --label source-root-clean \
    -- /bin/pwd
)

test "$(cat "$clean_evidence/source-root.txt")" = "$source_root"
test "$(cat "$clean_evidence/source-sha.txt")" = "$expected_sha"
test ! -s "$clean_evidence/source-status.txt"
test "$(cat "$clean_evidence/stdout.log")" = "$source_root"

printf 'dirty\n' >"$source_root/fixture.txt"
dirty_evidence="$scratch/dirty-evidence"
(
  cd "$source_root"
  "$wrapper" \
    --output "$dirty_evidence" \
    --label source-root-dirty \
    -- /usr/bin/true
)

grep -Fxq ' M fixture.txt' "$dirty_evidence/source-status.txt"
test "$(cat "$dirty_evidence/source-sha.txt")" = "$expected_sha"

echo "native evidence shell source-root checks passed"
