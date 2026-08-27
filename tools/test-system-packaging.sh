#!/usr/bin/env bash
# Focused safety checks for the system-integration packaging hooks.
set -euo pipefail

cd "$(dirname "$0")/.."

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/scrozz-package-test.XXXXXX")"
cleanup() {
  rm -rf "$ROOT"
}
trap cleanup EXIT

fail() {
  echo "test-system-packaging: $1" >&2
  exit 1
}

expect_rejected() {
  if SCROZZ_BUNDLE_VALIDATE_ONLY=1 tools/make-app-bundle.sh "$1" >/dev/null 2>&1; then
    fail "unsafe bundle destination was accepted: '$1'"
  fi
}

expect_build_number_rejected() {
  if SCROZZ_BUNDLE_VALIDATE_ONLY=1 SCROZZ_BUILD_NUMBER="$1" \
    tools/make-app-bundle.sh "$ROOT/safe/Scrozz.app" >/dev/null 2>&1; then
    fail "invalid bundle build number was accepted: '$1'"
  fi
}

expect_app_version_rejected() {
  if SCROZZ_BUNDLE_VALIDATE_ONLY=1 SCROZZ_APP_VERSION="$1" \
    tools/make-app-bundle.sh "$ROOT/safe/Scrozz.app" >/dev/null 2>&1; then
    fail "invalid bundle app version was accepted: '$1'"
  fi
}

mkdir -p "$ROOT/safe"
SCROZZ_BUNDLE_VALIDATE_ONLY=1 \
  tools/make-app-bundle.sh "$ROOT/safe/Scrozz.app" >/dev/null
SCROZZ_BUNDLE_VALIDATE_ONLY=1 \
  SCROZZ_BUILD_NUMBER=123.4.5 \
  SCROZZ_APP_VERSION=1.2.3 \
  tools/make-app-bundle.sh "$ROOT/safe/Scrozz.app" >/dev/null

expect_rejected "/"
expect_rejected "$ROOT/safe/not-an-app"
expect_rejected "$ROOT/safe/../safe/Scrozz.app"

mkdir -p "$ROOT/unrecognised/Scrozz.app"
expect_rejected "$ROOT/unrecognised/Scrozz.app"

mkdir -p "$ROOT/real"
ln -s "$ROOT/real" "$ROOT/safe/Scrozz.app"
expect_rejected "$ROOT/safe/Scrozz.app"
rm "$ROOT/safe/Scrozz.app"

expect_build_number_rejected "1;touch /tmp/scrozz-injected"
expect_build_number_rejected "1.2.3.4"
expect_build_number_rejected "build-1"
expect_app_version_rejected "1.2"
expect_app_version_rejected "1.2.3-beta"
expect_app_version_rejected '1.2.3</string><key>Injected</key><true/>'

ln -s / "$ROOT/root-output"
if tools/package.sh "$ROOT/root-output" >/dev/null 2>&1; then
  fail "filesystem root was accepted through an output-directory symlink"
fi

grep -q '<key>CFBundleURLTypes</key>' tools/make-app-bundle.sh ||
  fail "bundle URL type is absent"
grep -q 'developer-id-release)' tools/make-app-bundle.sh ||
  fail "Developer ID release signing mode is absent"
grep -q '"signed_manifest": false' tools/package.sh ||
  fail "package metadata does not keep signing explicitly gated"

echo "system packaging checks passed"
