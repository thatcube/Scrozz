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

MANIFEST="packaging/windows/AppxManifest.xml.in"
WINDOWS_PACKAGE="tools/package-windows.ps1"
WINDOWS_PACKAGE_TEST="tools/test-windows-packaging.ps1"
NATIVE_SMOKE="tools/smoke.sh"
RELEASE_WORKFLOW=".github/workflows/release.yml"
[[ -f "$MANIFEST" ]] || fail "Windows AppxManifest template is absent"
[[ -f "$WINDOWS_PACKAGE" ]] || fail "Windows package script is absent"
[[ -f "$WINDOWS_PACKAGE_TEST" ]] || fail "Windows artifact test is absent"
[[ -f "$NATIVE_SMOKE" ]] || fail "native artifact smoke script is absent"
grep -Fq 'SCROZZ_IPC_SOCKET="\\\\.\\pipe\\scrozz-smoke-$$"' "$NATIVE_SMOKE" ||
  fail "native smoke does not select an isolated named-pipe endpoint on Windows"
grep -Fq "packaged smoke requires an installed app-execution alias" tools/windows-smoke.ps1 ||
  fail "Windows smoke does not require the installed alias for packaged identity"
if command -v xmllint >/dev/null 2>&1; then
  xmllint --noout "$MANIFEST" ||
    fail "Windows AppxManifest template is not well-formed XML"
fi
grep -q 'uap10:RuntimeBehavior="packagedClassicApp"' "$MANIFEST" ||
  fail "MSIX app does not declare packaged classic runtime behavior"
grep -q 'uap10:TrustLevel="mediumIL"' "$MANIFEST" ||
  fail "MSIX app does not declare medium integrity"
grep -q 'Id="Scrozz"' "$MANIFEST" ||
  fail "MSIX application id drifted"
grep -q '<rescap:Capability Name="runFullTrust"' "$MANIFEST" ||
  fail "MSIX app does not declare full-trust capability"
grep -q 'Category="windows.protocol"' "$MANIFEST" ||
  fail "MSIX protocol registration is absent"
grep -q '<uap:Protocol Name="scrozz"' "$MANIFEST" ||
  fail "MSIX protocol name drifted"
grep -q 'uap10:Parameters="url handle"' "$MANIFEST" ||
  fail "MSIX protocol activation does not route through the allow-listed URL command"
grep -q 'Category="windows.startupTask"' "$MANIFEST" ||
  fail "MSIX startup task is absent"
grep -q 'TaskId="ScrozzStartup"' "$MANIFEST" ||
  fail "MSIX startup task id drifted"
grep -q 'Enabled="false"' "$MANIFEST" ||
  fail "MSIX startup task is not opt-in"
grep -q 'uap10:Parameters="gui"' "$MANIFEST" ||
  fail "MSIX startup task does not launch the GUI command"
grep -q 'Category="windows.appExecutionAlias"' "$MANIFEST" ||
  fail "MSIX CLI execution alias is absent"
grep -q '<uap5:ExecutionAlias Alias="scrozz.exe"' "$MANIFEST" ||
  fail "MSIX CLI execution alias drifted"
if grep -Eq 'internetClient|broadFileSystemAccess' "$MANIFEST"; then
  fail "MSIX manifest requests an unnecessary broad capability"
fi
grep -q 'pack /o /h SHA256 /f' "$WINDOWS_PACKAGE" ||
  fail "MSIX package does not pin SHA-256 and a deterministic mapping"
grep -q '1980, 1, 1' "$WINDOWS_PACKAGE" ||
  fail "Windows package inputs do not receive a reproducible timestamp"
grep -q 'SCROZZ_MSIX_VERIFY_DETERMINISM' "$WINDOWS_PACKAGE" ||
  fail "Windows package has no byte-for-byte determinism check"
grep -q 'SCROZZ_WINDOWS_VERIFY_DETERMINISM' "$WINDOWS_PACKAGE" ||
  fail "Windows package has no all-artifact determinism check"
grep -q 'determinism-check.zip' "$WINDOWS_PACKAGE" ||
  fail "portable ZIP is excluded from reproducibility verification"
grep -q 'SCROZZ_MSIX_SIGN_PFX' "$WINDOWS_PACKAGE" ||
  fail "Windows package has no external PFX signing hook"
grep -q 'SCROZZ_MSIX_SIGN_CERT_SHA1' "$WINDOWS_PACKAGE" ||
  fail "Windows package has no certificate-store signing hook"
grep -Fq "signed_manifest = \$false" "$WINDOWS_PACKAGE" ||
  fail "Windows metadata does not keep update signing human-gated"
if ! grep -Fq -- '-PackageKind "portable"' "$WINDOWS_PACKAGE" ||
  ! grep -Fq -- '-OcrBackend "tesseract"' "$WINDOWS_PACKAGE"; then
  fail "portable metadata does not declare the Tesseract backend"
fi
grep -q 'SCROZZ_TESSERACT_DIR' "$WINDOWS_PACKAGE" ||
  fail "portable packaging does not require an explicit Tesseract payload"
grep -q 'Copy-PortableTesseract' "$WINDOWS_PACKAGE" ||
  fail "portable packaging does not stage its local Tesseract payload"
if ! grep -Fq -- '-PackageKind "msix"' "$WINDOWS_PACKAGE" ||
  ! grep -Fq -- '-OcrBackend "windows-media-ocr"' "$WINDOWS_PACKAGE"; then
  fail "MSIX metadata does not declare the Windows.Media.Ocr backend"
fi
if ! grep -Fq '"windows-portable-zip"' "$WINDOWS_PACKAGE_TEST" ||
  ! grep -Fq '"tesseract"' "$WINDOWS_PACKAGE_TEST"; then
  fail "Windows artifact test does not verify the portable OCR contract"
fi
if ! grep -Fq '"windows-msix"' "$WINDOWS_PACKAGE_TEST" ||
  ! grep -Fq '"windows-media-ocr"' "$WINDOWS_PACKAGE_TEST"; then
  fail "Windows artifact test does not verify the MSIX OCR contract"
fi
grep -q 'tesseract/tessdata/eng.traineddata' "$WINDOWS_PACKAGE_TEST" ||
  fail "Windows artifact test does not verify the packaged English OCR model"
grep -q 'SCROZZ_WINDOWS_VERIFY_DETERMINISM' "$WINDOWS_PACKAGE_TEST" ||
  fail "Windows artifact test does not exercise reproducible packaging"
grep -q 'package-windows.ps1' tools/package.sh ||
  fail "the cross-platform package hook does not delegate Windows packaging"
grep -q 'WINDOWS_TESSERACT_ARCHIVE_URL' "$RELEASE_WORKFLOW" ||
  fail "release packaging does not use a human-configured Tesseract payload"
grep -q 'WINDOWS_TESSERACT_ARCHIVE_SHA256' "$RELEASE_WORKFLOW" ||
  fail "release packaging does not pin the Tesseract payload digest"
grep -q -- '--proto-redir "=https"' "$RELEASE_WORKFLOW" ||
  fail "release payload redirects are not constrained to HTTPS"
grep -q -- '--max-filesize 536870912' "$RELEASE_WORKFLOW" ||
  fail "release payload downloads are not size bounded"
for asset in Square44x44Logo.png Square150x150Logo.png StoreLogo.png; do
  [[ -f "packaging/windows/Assets/$asset" ]] ||
    fail "MSIX asset is absent: $asset"
done

echo "system packaging checks passed"
