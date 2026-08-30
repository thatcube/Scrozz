#!/usr/bin/env bash
# Focused safety checks for the system-integration packaging hooks.
set -euo pipefail

cd "$(dirname "$0")/.."

ROOT="$PWD/.scrozz-package-test.$$.$RANDOM"
mkdir -m 700 "$ROOT"
cleanup() {
  rm -rf "$ROOT"
}
trap cleanup EXIT

fail() {
  echo "test-system-packaging: $1" >&2
  exit 1
}

expect_rejected() {
  if SCROZZ_BUNDLE_VALIDATE_ONLY=1 SCROZZ_SIGNING_MODE=ad-hoc-dev \
    tools/make-app-bundle.sh "$1" >/dev/null 2>&1; then
    fail "unsafe bundle destination was accepted: '$1'"
  fi
}

expect_build_number_rejected() {
  if SCROZZ_BUNDLE_VALIDATE_ONLY=1 SCROZZ_SIGNING_MODE=ad-hoc-dev \
    SCROZZ_BUILD_NUMBER="$1" \
    tools/make-app-bundle.sh "$ROOT/safe/Scrozz.app" >/dev/null 2>&1; then
    fail "invalid bundle build number was accepted: '$1'"
  fi
}

expect_app_version_rejected() {
  if SCROZZ_BUNDLE_VALIDATE_ONLY=1 SCROZZ_SIGNING_MODE=ad-hoc-dev \
    SCROZZ_APP_VERSION="$1" \
    tools/make-app-bundle.sh "$ROOT/safe/Scrozz.app" >/dev/null 2>&1; then
    fail "invalid bundle app version was accepted: '$1'"
  fi
}

mkdir -p "$ROOT/safe"
SCROZZ_BUNDLE_VALIDATE_ONLY=1 SCROZZ_SIGNING_MODE=ad-hoc-dev \
  tools/make-app-bundle.sh "$ROOT/safe/Scrozz.app" >/dev/null
SCROZZ_BUNDLE_VALIDATE_ONLY=1 \
  SCROZZ_SIGNING_MODE=ad-hoc-dev \
  SCROZZ_BUILD_NUMBER=123.4.5 \
  SCROZZ_APP_VERSION=1.2.3-beta.4 \
  tools/make-app-bundle.sh "$ROOT/safe/Scrozz.app" >/dev/null

if SCROZZ_BUNDLE_VALIDATE_ONLY=1 \
  tools/make-app-bundle.sh "$ROOT/safe/Scrozz.app" >/dev/null 2>&1; then
  fail "an implicit ad-hoc signing identity was accepted"
fi
if SCROZZ_BUNDLE_VALIDATE_ONLY=1 SCROZZ_SIGNING_MODE=external-release \
  tools/make-app-bundle.sh "$ROOT/safe/Scrozz.app" >/dev/null 2>&1; then
  fail "external release signing was accepted without its explicit handoff guard"
fi
if SCROZZ_BUNDLE_VALIDATE_ONLY=1 SCROZZ_SIGNING_MODE=ad-hoc-dev \
  SCROZZ_MACOS_DEPLOYMENT_TARGET=12.2.9 \
  tools/make-app-bundle.sh "$ROOT/safe/Scrozz.app" >/dev/null 2>&1; then
  fail "a deployment target below macOS 12.3 was accepted"
fi
SCROZZ_BUNDLE_VALIDATE_ONLY=1 SCROZZ_SIGNING_MODE=ad-hoc-dev \
  SCROZZ_MACOS_DEPLOYMENT_TARGET=12.10 \
  tools/make-app-bundle.sh "$ROOT/safe/Scrozz.app" >/dev/null

if [[ "$(uname -s)" == "Darwin" ]]; then
  GENERATED_APP="$ROOT/generated/Scrozz.app"
  mkdir -p "$(dirname "$GENERATED_APP")"
  SCROZZ_PREBUILT_BIN=/usr/bin/true \
    SCROZZ_APP_VERSION=1.2.3-beta.4 \
    SCROZZ_BUILD_NUMBER=123.4.5 \
    SCROZZ_MACOS_DEPLOYMENT_TARGET=12.10 \
    SCROZZ_SIGNING_MODE=ad-hoc-dev \
    tools/make-app-bundle.sh "$GENERATED_APP" >/dev/null
  [[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' \
    "$GENERATED_APP/Contents/Info.plist")" == "1.2.3" ]] ||
    fail "prerelease macOS bundle did not emit a numeric short version"
  [[ "$(/usr/libexec/PlistBuddy -c 'Print :LSMinimumSystemVersion' \
    "$GENERATED_APP/Contents/Info.plist")" == "12.10" ]] ||
    fail "macOS bundle did not preserve the structurally valid deployment target"
  codesign --verify --strict "$GENERATED_APP" ||
    fail "ad-hoc macOS fixture is not actually signed"

  if SCROZZ_BIN=/usr/bin/true \
    SCROZZ_APP_VERSION=1.2.3 \
    SCROZZ_STAMP=validate-only \
    SCROZZ_SIGNING_MODE=ad-hoc-dev \
    SCROZZ_BUNDLE_VALIDATE_ONLY=1 \
    tools/package.sh "$ROOT/validate-only-output" >/dev/null 2>&1; then
    fail "validate-only mode emitted success-shaped macOS package metadata"
  fi

  if SCROZZ_PREBUILT_BIN=/usr/bin/true \
    SCROZZ_APP_VERSION=1.2.3 \
    SCROZZ_SIGNING_MODE=developer-id-release \
    SCROZZ_SIGN_IDENTITY='Developer ID Application: Missing Scrozz Test (0000000000)' \
    tools/make-app-bundle.sh "$GENERATED_APP" >/dev/null 2>&1; then
    fail "a missing Developer ID identity produced a release bundle"
  fi

  PACKAGE_OUT="$ROOT/package-output"
  SCROZZ_BIN=/usr/bin/true \
    SCROZZ_APP_VERSION=1.2.3-beta.4 \
    SCROZZ_STAMP=fixture \
      SCROZZ_SIGNING_MODE=ad-hoc-dev \
      tools/package.sh "$PACKAGE_OUT" >/dev/null
    case "$(uname -m)" in
      arm64|aarch64) PACKAGE_ARCH=arm64 ;;
      *) PACKAGE_ARCH=x86_64 ;;
    esac
    PACKAGE_METADATA="$PACKAGE_OUT/scrozz-1.2.3-beta.4-fixture-macos-$PACKAGE_ARCH.zip.artifact.json"
  ruby -rjson -e '
    metadata = JSON.parse(File.read(ARGV.fetch(0)))
    raise "native version" unless metadata["native_package_version"] == "1.2.3"
    raise "container signature" unless metadata["signed"] == false
    raise "payload signature" unless metadata["payload_signed"] == true
    raise "identity mode" unless metadata["identity_mode"] == "ad-hoc-dev"
  ' "$PACKAGE_METADATA" ||
    fail "macOS package metadata does not distinguish container and payload identity"
fi

expect_rejected "/"
expect_rejected "$ROOT/safe/not-an-app"
expect_rejected "$ROOT/safe/../safe/Scrozz.app"

mkdir -p "$ROOT/unrecognised/Scrozz.app"
expect_rejected "$ROOT/unrecognised/Scrozz.app"

mkdir -p "$ROOT/real"
ln -s "$ROOT/real" "$ROOT/safe/Scrozz.app"
expect_rejected "$ROOT/safe/Scrozz.app"
rm "$ROOT/safe/Scrozz.app"

expect_build_number_rejected "1;touch $ROOT/scrozz-injected"
expect_build_number_rejected "1.2.3.4"
expect_build_number_rejected "build-1"
expect_app_version_rejected "1.2"
expect_app_version_rejected "1.2.3-"
expect_app_version_rejected "01.2.3"
expect_app_version_rejected "1.2.3-01"
expect_app_version_rejected '1.2.3</string><key>Injected</key><true/>'

ln -s / "$ROOT/root-output"
if tools/package.sh "$ROOT/root-output" >/dev/null 2>&1; then
  fail "filesystem root was accepted through an output-directory symlink"
fi

grep -q 'developer-id-release)' tools/make-app-bundle.sh ||
  fail "Developer ID release signing mode is absent"
if grep -q 'SCROZZ_SIGNING_MODE:-ad-hoc-dev' tools/make-app-bundle.sh tools/package.sh; then
  fail "macOS packaging still silently falls back to an ad-hoc identity"
fi
grep -q '"signed_manifest": false' tools/package.sh ||
  fail "package metadata does not keep signing explicitly gated"

MANIFEST="packaging/windows/AppxManifest.xml.in"
WINDOWS_PACKAGE="tools/package-windows.ps1"
WINDOWS_PACKAGE_TEST="tools/test-windows-packaging.ps1"
WINDOWS_PAYLOAD_MANIFEST="packaging/windows/tesseract-payload.json"
WINDOWS_README="packaging/windows/README.md"
RELEASE_WORKFLOW=".github/workflows/release.yml"
[[ -f "$MANIFEST" ]] || fail "Windows AppxManifest template is absent"
[[ -f "$WINDOWS_PACKAGE" ]] || fail "Windows package script is absent"
[[ -f "$WINDOWS_PACKAGE_TEST" ]] || fail "Windows artifact test is absent"
[[ -f "$WINDOWS_PAYLOAD_MANIFEST" ]] ||
  fail "Windows Tesseract payload manifest is absent"
[[ -f "$WINDOWS_README" ]] || fail "Windows packaging documentation is absent"
[[ -f "$RELEASE_WORKFLOW" ]] || fail "release workflow is absent"
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
MAKEAPPX_MAPPING_LINE="$(
  cat <<'POWERSHELL'
        $Lines.Add(('"{0}" "{1}"' -f $File, $Relative))
POWERSHELL
)"
grep -Fqx "$MAKEAPPX_MAPPING_LINE" "$WINDOWS_PACKAGE" ||
  fail "MakeAppx mapping formatting is not passed as one PowerShell argument"
grep -q '1980, 1, 1' "$WINDOWS_PACKAGE" ||
  fail "Windows package inputs do not receive a reproducible timestamp"
grep -q 'SCROZZ_MSIX_VERIFY_DETERMINISM' "$WINDOWS_PACKAGE" ||
  fail "Windows package has no byte-for-byte determinism check"
grep -q 'SCROZZ_WINDOWS_VERIFY_DETERMINISM' "$WINDOWS_PACKAGE" ||
  fail "Windows package has no all-artifact determinism check"
grep -Fq '".scz-{0}-{1}" -f $PID, $ScratchToken' "$WINDOWS_PACKAGE" ||
  fail "Windows package staging can exceed legacy MAX_PATH on ordinary output roots"
grep -q 'determinism-check.zip' "$WINDOWS_PACKAGE" ||
  fail "portable ZIP is excluded from reproducibility verification"
grep -Fq 'function Normalize-MsixZipTimestamps' "$WINDOWS_PACKAGE" ||
  fail "Windows package has no non-recompressing MSIX timestamp normalizer"
grep -Fq '$CentralTimestampOffsets.Add($CentralHeaderOffset + 12)' \
  "$WINDOWS_PACKAGE" ||
  fail "MSIX normalizer does not derive central-header timestamp offsets"
grep -Fq '$LocalTimestampOffsets.Add($LocalHeaderOffset + 10)' \
  "$WINDOWS_PACKAGE" ||
  fail "MSIX normalizer does not derive local-header timestamp offsets"
grep -Fq '$DiskNumber -eq [UInt16]::MaxValue' "$WINDOWS_PACKAGE" ||
  fail "MSIX normalizer does not recognize MakeAppx ZIP64 sentinels"
grep -Fq '    Normalize-MsixZipTimestamps $MsixStaged' "$WINDOWS_PACKAGE" ||
  fail "primary MSIX output is not normalized before validation"
grep -Fq '        Normalize-MsixZipTimestamps $SecondMsix' "$WINDOWS_PACKAGE" ||
  fail "comparison MSIX output is not normalized before determinism checking"
grep -Fq 'Confirm-MakeAppxPackage' "$WINDOWS_PACKAGE" ||
  fail "normalized MSIX output is not validated by MakeAppx"
grep -Fq 'Assert-ArchiveTimestamps $Msix "MSIX"' "$WINDOWS_PACKAGE_TEST" ||
  fail "Windows artifact test does not verify canonical MSIX timestamps"
grep -q 'SCROZZ_TESSERACT_DIR' "$WINDOWS_PACKAGE" ||
  fail "portable packaging does not require an explicit Tesseract payload"
grep -q 'Confirm-TesseractPayload' "$WINDOWS_PACKAGE" ||
  fail "portable packaging does not verify its Tesseract payload manifest"
grep -q 'Tesseract runtime DLL closure differs from the manifest' "$WINDOWS_PACKAGE" ||
  fail "portable packaging does not verify the runtime DLL closure"
grep -q 'Get-FileHash -LiteralPath \$Source -Algorithm SHA256' "$WINDOWS_PACKAGE" ||
  fail "portable packaging does not checksum every payload file"
grep -q 'foreach (\$PayloadFile in \$TesseractPayloadFiles)' "$WINDOWS_PACKAGE" ||
  fail "portable packaging does not limit copied files to the verified manifest"
grep -q 'Prerelease artifacts require SCROZZ_MSIX_VERSION' "$WINDOWS_PACKAGE" ||
  fail "Windows packaging permits colliding prerelease package versions"
grep -Fq '1.2.3+build.7' "$WINDOWS_PACKAGE_TEST" ||
  fail "Windows packaging tests do not cover semantic build metadata"
grep -Fq '$NativeMajor = $SemanticMajor + 1' "$WINDOWS_PACKAGE" ||
  fail "local stable MSIX mapping does not make the first component nonzero"
grep -Fq '$NativeMinor = ($SemanticMinor * 256) + $SemanticPatch' \
  "$WINDOWS_PACKAGE" ||
  fail "local stable MSIX mapping does not encode semantic minor and patch"
grep -Fq '$Candidate = "$NativeMajor.$NativeMinor.65535.0"' "$WINDOWS_PACKAGE" ||
  fail "local stable MSIX mapping does not reserve build 65535 and revision 0"
grep -q 'first MSIX version component must be between 1 and 65535' \
  "$WINDOWS_PACKAGE" ||
  fail "Windows packaging accepts a zero first version component"
grep -q 'fourth MSIX version component must be 0' "$WINDOWS_PACKAGE" ||
  fail "Windows packaging accepts a nonzero fourth version component"
grep -q 'native_package_version' "$WINDOWS_PACKAGE" ||
  fail "artifact metadata does not record the native package version"
grep -q 'payload_signed' "$WINDOWS_PACKAGE" ||
  fail "artifact metadata does not distinguish payload and package signing"
grep -Fq 'tessdata\eng.traineddata' "$WINDOWS_PACKAGE" ||
  fail "portable packaging does not require English trained data"
grep -Fq '"tesseract.exe"' "$WINDOWS_PACKAGE_TEST" ||
  fail "Windows artifact test does not verify the Tesseract executable"
grep -Fq 'tessdata/eng.traineddata"' "$WINDOWS_PACKAGE_TEST" ||
  fail "Windows artifact test does not verify English trained data"
grep -q 'SCROZZ_MSIX_SIGN_PFX' "$WINDOWS_PACKAGE" ||
  fail "Windows package has no external PFX signing hook"
grep -q 'SCROZZ_MSIX_SIGN_CERT_SHA1' "$WINDOWS_PACKAGE" ||
  fail "Windows package has no certificate-store signing hook"
grep -q 'PROCESSOR_ARCHITECTURE -eq "ARM64"' "$WINDOWS_PACKAGE" ||
  fail "Windows package does not prefer host-native ARM64 SDK tools"
ARCHITECTURE_LOOP_LINE="$(
  grep -n 'foreach (\$ToolArchitecture in \$ToolArchitectures)' "$WINDOWS_PACKAGE" |
    head -n 1 | cut -d: -f1
)"
DIRECTORY_LOOP_LINE="$(
  grep -n 'foreach (\$Directory in \$SdkDirectories)' "$WINDOWS_PACKAGE" |
    head -n 1 | cut -d: -f1
)"
[[ -n "$ARCHITECTURE_LOOP_LINE" && -n "$DIRECTORY_LOOP_LINE" &&
  "$ARCHITECTURE_LOOP_LINE" -lt "$DIRECTORY_LOOP_LINE" ]] ||
  fail "Windows package searches newer x64 SDKs before older native ARM64 tools"
grep -q 'SCROZZ_MSIX_IDENTITY_MODE' "$WINDOWS_PACKAGE" ||
  fail "Windows package does not require an explicit identity mode"
grep -q 'development identity values are never a release fallback' \
  "$WINDOWS_PACKAGE" ||
  fail "Store packaging can silently fall back to a development identity"
grep -q 'development identity mode rejects Store identity' "$WINDOWS_PACKAGE" ||
  fail "Development packaging can retain a production Store identity"
grep -q 'PayloadSignature.SignerCertificate' "$WINDOWS_PACKAGE" ||
  fail "Store packaging does not bind payload and package signing identities"
grep -q 'PayloadHasEmbeddedSignature' "$WINDOWS_PACKAGE" ||
  fail "Store packaging accepts catalog-only executable signatures"
grep -q 'store-artifact-test' "$WINDOWS_PACKAGE_TEST" ||
  fail "Windows artifact tests never produce a signed Store-mode package"
grep -q 'New-UnsignedPeFixture' "$WINDOWS_PACKAGE_TEST" ||
  fail "Windows signing test does not create a fresh unsigned real PE fixture"
grep -q 'IMAGE_DIRECTORY_ENTRY_SECURITY' "$WINDOWS_PACKAGE_TEST" ||
  fail "Windows signing fixture does not remove its inherited Authenticode signature"
grep -q 'catalog-signed' "$WINDOWS_PACKAGE_TEST" ||
  fail "Windows signing fixture cannot use catalog-signed Windows PowerShell"
grep -q 'TimeDateStamp' "$WINDOWS_PACKAGE_TEST" ||
  fail "Windows catalog-signed fixture is not detached from its catalog hash"
grep -q 'accepted different payload and package signers' "$WINDOWS_PACKAGE_TEST" ||
  fail "Windows artifact tests do not reject a mismatched payload signer"
grep -q -- '-DeleteKey' "$WINDOWS_PACKAGE_TEST" ||
  fail "Windows signing test leaves its ephemeral private key behind"
grep -q 'SCROZZ_TEST_ALLOW_UNTRUSTED_SIGNATURE' "$WINDOWS_PACKAGE" ||
  fail "Windows signing test has no explicit self-signed certificate gate"
grep -q 'test_signature' "$WINDOWS_PACKAGE" ||
  fail "self-signed test artifacts are not labelled in metadata"
grep -q 'Get-CanonicalPayloadContract' "$WINDOWS_PACKAGE" ||
  fail "self-signed test guard compares mutable manifest serialization"
if grep -q 'SCROZZ_TEST_ALLOW_UNTRUSTED_SIGNATURE' "$RELEASE_WORKFLOW"; then
  fail "release packaging enables the self-signed test certificate gate"
fi
if grep -q 'Import-Certificate' "$WINDOWS_PACKAGE_TEST"; then
  fail "Windows signing test writes to a trust store"
fi
grep -Fq "signed_manifest = \$false" "$WINDOWS_PACKAGE" ||
  fail "Windows metadata does not keep update signing human-gated"
if ! grep -Fq -- '-PackageKind "portable"' "$WINDOWS_PACKAGE" ||
  ! grep -Fq -- '-OcrBackend "tesseract"' "$WINDOWS_PACKAGE"; then
  fail "portable metadata does not declare the Tesseract backend"
fi
if ! grep -Fq -- '-PackageKind "msix"' "$WINDOWS_PACKAGE" ||
  ! grep -Fq -- '-OcrBackend "windows-media-ocr"' "$WINDOWS_PACKAGE"; then
  fail "MSIX metadata does not declare the Windows.Media.Ocr backend"
fi
grep -Fq 'Test-ArtifactMetadata $Portable "portable" "tesseract" "1.2.3" "" "none"' \
  "$WINDOWS_PACKAGE_TEST" ||
  fail "Windows artifact test does not verify the portable OCR contract"
if ! grep -Fq '"2.515.65535.0"' "$WINDOWS_PACKAGE_TEST" ||
  ! grep -Fq '"development"' "$WINDOWS_PACKAGE_TEST"; then
  fail "Windows artifact test does not pin the stable Store version mapping"
fi
grep -q '"2.515.42.0"' "$WINDOWS_PACKAGE_TEST" ||
  fail "Windows artifact test does not pin a compliant prerelease version"
grep -q 'accepted a changed Tesseract runtime DLL' "$WINDOWS_PACKAGE_TEST" ||
  fail "Windows artifact test does not reject changed payload checksums"
grep -q 'accepted an unexpected runtime DLL' "$WINDOWS_PACKAGE_TEST" ||
  fail "Windows artifact test does not reject runtime closure drift"
grep -q '"0.515.42.0".*"first MSIX version component"' "$WINDOWS_PACKAGE_TEST" ||
  fail "Windows artifact test does not reject a zero first component"
grep -q '"2.515.42.1".*"fourth MSIX version component"' "$WINDOWS_PACKAGE_TEST" ||
  fail "Windows artifact test does not reject a nonzero fourth component"
FIXTURE_LINE="$(grep -n 'WriteAllBytes(\$Binary' "$WINDOWS_PACKAGE_TEST" |
  head -n 1 | cut -d: -f1)"
REJECTION_LINE="$(grep -n '\$RejectedPrereleaseVersion' "$WINDOWS_PACKAGE_TEST" |
  head -n 1 | cut -d: -f1)"
[[ -n "$FIXTURE_LINE" && -n "$REJECTION_LINE" &&
  "$FIXTURE_LINE" -lt "$REJECTION_LINE" ]] ||
  fail "Windows prerelease rejection runs before its executable fixture exists"

if ! ruby -rjson - "$WINDOWS_PAYLOAD_MANIFEST" <<'RUBY'
manifest = JSON.parse(File.read(ARGV.fetch(0)))
raise "schema" unless manifest["schema"] == 1

package = manifest.fetch("chocolatey_package")
raise "package id" unless package["id"] == "tesseract"
raise "package version" unless package["version"] == "5.5.0.20241111"
raise "package URL" unless package["url"] ==
  "https://community.chocolatey.org/api/v2/package/tesseract/5.5.0.20241111"
raise "package hash" unless package["sha256"] ==
  "56659a4c01e6ea75a0b710ba7e8bb16e9cc6675978d2861323751812aeea6183"
raise "installer hash" unless package["installer_sha256"] ==
  "f3fc4236425b690c8be756f35793f77394ee004be0a6460a440c754d892f68bc"

model = manifest.fetch("language_model")
raise "model commit" unless model["commit"] ==
  "87416418657359cb625c412a48b6e1d6d41c29bd"
raise "model URL is mutable" unless model["url"].include?(model["commit"])
raise "model hash" unless model["sha256"] ==
  "7d4322bd2a7749724879683fc3912cb542f19906c83bcc1a52132556427170b2"

files = manifest.fetch("payload_files")
paths = files.map { |entry| entry.fetch("path") }
raise "payload count" unless files.length == 59
raise "duplicate payload path" unless paths.uniq.length == paths.length
raise "runtime closure count" unless paths.grep(/\.dll\z/i).length == 56
%w[tesseract.exe doc/LICENSE libtesseract-5.dll libleptonica-6.dll
   tessdata/eng.traineddata].each do |required|
  raise "missing #{required}" unless paths.include?(required)
end
files.each do |entry|
  raise "bad payload hash" unless entry.fetch("sha256").match?(/\A[0-9a-f]{64}\z/)
end
model_entry = files.find { |entry| entry["path"] == "tessdata/eng.traineddata" }
raise "model/payload hash drift" unless model_entry["sha256"] == model["sha256"]
license_entry = files.find { |entry| entry["path"] == "doc/LICENSE" }
raise "license hash" unless license_entry["sha256"] ==
  "cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30"
RUBY
then
  fail "Windows Tesseract checksum manifest is invalid"
fi

grep -q -- '--require-checksums' "$RELEASE_WORKFLOW" ||
  fail "release workflow does not require payload checksum verification"
grep -Fq '"--source=$DownloadRoot"' "$RELEASE_WORKFLOW" ||
  fail "release workflow does not install only from the verified local nupkg"
grep -q -- '--ignore-dependencies' "$RELEASE_WORKFLOW" ||
  fail "release workflow can resolve mutable Chocolatey dependencies"
grep -q 'installer_sha256' "$RELEASE_WORKFLOW" ||
  fail "release workflow does not verify the embedded Tesseract installer"
grep -q 'Invoke-WebRequest -Uri \$Model.url' "$RELEASE_WORKFLOW" ||
  fail "release workflow does not replace the English model from its pinned URL"
grep -q 'Assert-Sha256 \$InstalledPath \$PayloadFile.sha256' "$RELEASE_WORKFLOW" ||
  fail "release workflow does not verify the installed payload closure"
grep -Fq '& $TesseractExecutable' "$RELEASE_WORKFLOW" ||
  fail "release workflow tests Tesseract through PATH instead of its exact path"
grep -q 'SCROZZ_TESSERACT_DIR=' "$RELEASE_WORKFLOW" ||
  fail "release workflow does not pass the acquired artifact-local payload"
grep -q 'SCROZZ_MSIX_VERSION=' "$RELEASE_WORKFLOW" ||
  fail "release workflow does not select an explicit MSIX package version"
grep -q 'GITHUB_RUN_NUMBER' "$RELEASE_WORKFLOW" ||
  fail "prerelease MSIX versions are not ordered by release build"
grep -q 'NATIVE_MAJOR=\$((SEMVER_MAJOR + 1))' "$RELEASE_WORKFLOW" ||
  fail "release MSIX mapping does not make native major nonzero"
grep -q 'NATIVE_MINOR=\$((SEMVER_MINOR \* 256 + SEMVER_PATCH))' \
  "$RELEASE_WORKFLOW" ||
  fail "release MSIX mapping does not encode semantic minor and patch"
grep -q 'NATIVE_BUILD=65535' "$RELEASE_WORKFLOW" ||
  fail "stable MSIX versions are not ordered above prereleases"
grep -q 'MSIX_VERSION=.*\.0"' "$RELEASE_WORKFLOW" ||
  fail "release MSIX versions do not have a zero fourth component"
grep -q 'cargo metadata --locked --no-deps' "$RELEASE_WORKFLOW" ||
  fail "release workflow does not read the Cargo workspace package version"
grep -q 'Release version mismatch' "$RELEASE_WORKFLOW" ||
  fail "release workflow does not reject a tag/Cargo version mismatch"
grep -q "format('refs/tags/{0}', steps.tag.outputs.tag)" "$RELEASE_WORKFLOW" ||
  fail "manual release checkout is not constrained to the tag namespace"
grep -q 'ref:.*needs.resolve.outputs.sha' "$RELEASE_WORKFLOW" ||
  fail "release jobs do not build the resolver-pinned tag commit"
grep -q 'environment: release-signing' "$RELEASE_WORKFLOW" ||
  fail "release signing is not protected by an approval environment"
grep -q 'EVENT_COMMIT=.*EVENT_SHA.*\^{commit\}' "$RELEASE_WORKFLOW" ||
  fail "tag-push releases are not pinned to the event commit"
grep -q 'git ls-remote --tags origin' "$RELEASE_WORKFLOW" ||
  fail "draft creation does not revalidate the remote tag"
grep -q 'REMOTE_SHA.*PINNED_SHA' "$RELEASE_WORKFLOW" ||
  fail "draft creation can attach artifacts to a moved tag"
grep -q 'RELEASE_TAG_RULESET_ID' "$RELEASE_WORKFLOW" ||
  fail "release workflow does not require immutable v* tag rules"
grep -q 'RELEASE_RULESET_TOKEN' "$RELEASE_WORKFLOW" ||
  fail "release workflow cannot see tag-ruleset bypass actors"
grep -q 'environment: release-ruleset-audit' "$RELEASE_WORKFLOW" ||
  fail "ruleset write-level readback token is not protected by approval"
grep -q 'Administration write permission' "$RELEASE_WORKFLOW" ||
  fail "ruleset token permissions are documented incorrectly"
grep -q 'has("bypass_actors")' "$RELEASE_WORKFLOW" ||
  fail "release ruleset validation treats hidden bypass actors as absent"
grep -q 'ref_name.exclude.*length' "$RELEASE_WORKFLOW" ||
  fail "release tag ruleset permits unprotected tag exclusions"
grep -q 'index("update")' "$RELEASE_WORKFLOW" ||
  fail "release tag ruleset does not forbid tag updates"
grep -q 'index("deletion")' "$RELEASE_WORKFLOW" ||
  fail "release tag ruleset does not forbid tag deletion"
grep -q 'bypass_actors.*length' "$RELEASE_WORKFLOW" ||
  fail "release tag ruleset permits bypass actors"
if grep -q 'REF=".*inputs.tag' "$RELEASE_WORKFLOW"; then
  fail "release tag is interpolated directly into a shell program"
fi
grep -q 'tools/make-app-bundle.sh "\$PWD/dist/Scrozz.app"' \
  "$RELEASE_WORKFLOW" ||
  fail "macOS release does not assemble Scrozz.app with the bundle script"
grep -q 'NATIVE_PACKAGE_VERSION="${VERSION%%-\*}"' "$RELEASE_WORKFLOW" ||
  fail "macOS artifact metadata does not report the app bundle version"
grep -q -- '--sign "\$MACOS_SIGN_IDENTITY" dist/Scrozz.app' \
  "$RELEASE_WORKFLOW" ||
  fail "macOS release does not sign the app bundle"
grep -q 'ditto -c -k --keepParent dist/Scrozz.app' "$RELEASE_WORKFLOW" ||
  fail "macOS release does not submit the app bundle for notarization"
grep -q 'xcrun stapler staple dist/Scrozz.app' "$RELEASE_WORKFLOW" ||
  fail "macOS release does not staple the app bundle"
grep -q 'glob("\*.artifact.json")' "$RELEASE_WORKFLOW" ||
  fail "signing summary is not derived from emitted artifact metadata"
grep -q "metadata\\['payload_signed'\\]" "$RELEASE_WORKFLOW" ||
  fail "signing summary does not distinguish payload signatures"
grep -q 'SCROZZ_MSIX_IDENTITY_MODE=store' "$RELEASE_WORKFLOW" ||
  fail "release packaging does not select the fail-closed Store identity mode"
grep -q 'WINDOWS_MSIX_PUBLISHER_DISPLAY_NAME.*must all be configured' \
  "$RELEASE_WORKFLOW" ||
  fail "release packaging does not require the complete Store identity"

if ! ruby -ryaml - "$RELEASE_WORKFLOW" <<'RUBY'
workflow = YAML.safe_load(File.read(ARGV.fetch(0)), aliases: true)
resolve = workflow.fetch("jobs").fetch("resolve")
build = workflow.fetch("jobs").fetch("build")
draft = workflow.fetch("jobs").fetch("checksums-and-draft")
raise "ruleset audit environment" unless
  resolve["environment"] == "release-ruleset-audit"
raise "build does not depend on resolver" unless build["needs"] == "resolve"
raise "signing environment" unless build["environment"] == "release-signing"
raise "resolver outputs" unless resolve.fetch("outputs").keys.sort == %w[sha tag]
raise "build job secrets" if build.key?("env")
raise "draft job secrets" if draft.key?("env")

secret_locations = []
walk = lambda do |value, path|
  case value
  when Hash
    value.each { |key, child| walk.call(child, path + [key.to_s]) }
  when Array
    value.each_with_index { |child, index| walk.call(child, path + [index.to_s]) }
  when String
    secret_locations << path if value.include?("${{ secrets.")
  end
end
walk.call(workflow, [])
raise "secret outside step env" unless secret_locations.all? { |path|
  path.include?("steps") && path[-2] == "env"
}

build_steps = build.fetch("steps").to_h { |step| [step["name"], step] }
raise "acquisition inherits secrets" if
  build_steps.fetch("Acquire pinned Windows Tesseract payload").key?("env")
%w[
  Sign\ and\ notarise\ macOS\ app\ (when\ configured)
  Sign\ Windows\ executable
  Package\ portable\ ZIP\ and\ MSIX
].each do |name|
  raise "missing signing env for #{name}" unless
    build_steps.fetch(name).fetch("env").keys.any?
end
draft_steps = draft.fetch("steps").to_h { |step| [step["name"], step] }
raise "missing GPG step env" unless
  draft_steps.fetch("Sign SHA256SUMS (when configured)").fetch("env").keys.any?
RUBY
then
  fail "release workflow leaks signing secrets outside their consuming steps"
fi

if grep -q 'scrozz autostart enable' "$WINDOWS_README"; then
  fail "Windows documentation advertises a nonexistent autostart command"
fi
grep -q 'Windows Settings' "$WINDOWS_README" ||
  fail "Windows documentation omits the actual Startup Apps flow"
grep -q 'native major = semantic major + 1' "$WINDOWS_README" ||
  fail "Windows documentation omits the Store version mapping"
grep -q 'tesseract-payload.json' "$WINDOWS_README" ||
  fail "Windows documentation omits its checksum manifest"
grep -q 'SCROZZ_WINDOWS_VERIFY_DETERMINISM' "$WINDOWS_PACKAGE_TEST" ||
  fail "Windows artifact test does not exercise reproducible packaging"
grep -q 'package-windows.ps1' tools/package.sh ||
  fail "the cross-platform package hook does not delegate Windows packaging"
for asset in Square44x44Logo.png Square150x150Logo.png StoreLogo.png; do
  [[ -f "packaging/windows/Assets/$asset" ]] ||
    fail "MSIX asset is absent: $asset"
done

echo "system packaging checks passed"
