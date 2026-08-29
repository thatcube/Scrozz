#!/usr/bin/env bash
# Package an already-built Scrozz executable and emit updater-ready metadata.
#
# This hook never installs, signs an update manifest, or reads a private key.
# The detached Ed25519 signature remains a human-gated release step.
set -euo pipefail

cd "$(dirname "$0")/.."
# shellcheck disable=SC1091
source "$HOME/.cargo/env" 2>/dev/null || true

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  cat <<'USAGE'
Usage: tools/package.sh [output-directory]

Packages the already-built release executable for the current host.

Environment:
  SCROZZ_BIN           executable to package
  SCROZZ_STAMP         archive-name suffix (default: short commit)
  SCROZZ_APP_VERSION   packaged version (default: workspace version)
  SCROZZ_BUILD_NUMBER  macOS CFBundleVersion
  SCROZZ_SIGNING_MODE  required macOS bundle mode: ad-hoc-dev,
                       developer-id-release, or external-release
  SCROZZ_SIGN_IDENTITY Developer ID Application identity for release mode
  SCROZZ_MSIX_IDENTITY_NAME
                        Partner Center package identity (Windows)
  SCROZZ_MSIX_PUBLISHER
                        Certificate/Store publisher DN (Windows)
  SCROZZ_MSIX_PUBLISHER_DISPLAY_NAME
                        Store-facing publisher name (Windows)
  SCROZZ_MSIX_VERSION   four-part package version override (Windows)
  SCROZZ_TESSERACT_DIR  absolute portable OCR payload directory containing
                        tesseract.exe and tessdata/eng.traineddata (Windows)
  SCROZZ_MSIX_SIGN_PFX  optional external package-signing certificate
  SCROZZ_MSIX_SIGN_PFX_PASSWORD
                        optional PFX password
  SCROZZ_MSIX_SIGN_CERT_SHA1
                        optional certificate-store thumbprint
  SCROZZ_WINDOWS_VERIFY_DETERMINISM
                        rebuild and compare both Windows artifacts

Each output includes <artifact>.sha256 and <artifact>.artifact.json. The JSON is
unsigned artifact metadata, not an update manifest and not installation
authority.
USAGE
  exit 0
fi

DIST="${1:-dist}"
case "$DIST" in
  "" | "/" | "." | ".." | */../* | ../* | */..)
    echo "package: refusing unsafe output directory '$DIST'" >&2
    exit 1
    ;;
esac
mkdir -p "$DIST"
DIST="$(cd "$DIST" && pwd -P)"
if [[ "$DIST" == "/" ]]; then
  echo "package: refusing filesystem root as output directory" >&2
  exit 1
fi

case "${RUNNER_OS:-$(uname -s)}" in
  Darwin | macOS)
    OS="macos"
    ;;
  Linux)
    OS="linux"
    ;;
  Windows | MINGW* | MSYS* | CYGWIN*)
    OS="windows"
    ;;
  *)
    echo "package: unsupported host '$(uname -s)'" >&2
    exit 1
    ;;
esac

case "$(uname -m)" in
  arm64 | aarch64)
    ARCH="aarch64"
    ;;
  x86_64 | amd64)
    ARCH="x86_64"
    ;;
  *)
    echo "package: unsupported architecture '$(uname -m)'" >&2
    exit 1
    ;;
esac
PLATFORM="$OS-$ARCH"

VERSION="${SCROZZ_APP_VERSION:-}"
if [[ -z "$VERSION" ]]; then
  VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' apps/scrozz/Cargo.toml | head -1)"
fi
if [[ -z "$VERSION" ]]; then
  VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
fi
SEMVER_PATTERN='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?(\+[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$'
if [[ ! "$VERSION" =~ $SEMVER_PATTERN ]]; then
  echo "package: invalid version '$VERSION'" >&2
  exit 1
fi
BUNDLE_VERSION="${BASH_REMATCH[1]}.${BASH_REMATCH[2]}.${BASH_REMATCH[3]}"
VERSION_WITHOUT_BUILD="${VERSION%%+*}"
if [[ "$VERSION_WITHOUT_BUILD" == *-* ]]; then
  PRERELEASE="${VERSION_WITHOUT_BUILD#*-}"
  IFS='.' read -r -a PRERELEASE_PARTS <<<"$PRERELEASE"
  for part in "${PRERELEASE_PARTS[@]}"; do
    if [[ "$part" =~ ^0[0-9]+$ ]]; then
      echo "package: numeric prerelease identifiers cannot have leading zeroes: '$VERSION'" >&2
      exit 1
    fi
  done
fi
NATIVE_VERSION="$VERSION"

STAMP="${SCROZZ_STAMP:-$(git rev-parse --short HEAD 2>/dev/null || printf dev)}"
if [[ ! "$STAMP" =~ ^[0-9A-Za-z._-]+$ ]]; then
  echo "package: unsafe archive stamp '$STAMP'" >&2
  exit 1
fi
NAME="scrozz-$VERSION-$STAMP-$PLATFORM"

BIN="${SCROZZ_BIN:-target/release/scrozz}"
if [[ "$OS" == "windows" && ! -f "$BIN" ]]; then
  BIN="target/release/scrozz.exe"
fi
if [[ ! -f "$BIN" ]]; then
  echo "package: no built executable at '$BIN'" >&2
  echo "package: run 'cargo build --release --locked -p scrozz' first" >&2
  exit 1
fi

STAGE="$(mktemp -d "$DIST/.scrozz-package.XXXXXX")"
cleanup() {
  rm -rf "$STAGE"
}
trap cleanup EXIT

copy_documents() {
  local destination="$1"
  for document in README.md LICENSE TRADEMARK.md; do
    [[ -f "$document" ]] && cp "$document" "$destination/"
  done
}

case "$OS" in
  macos)
    SIGNING_MODE="${SCROZZ_SIGNING_MODE:-}"
    if [[ -z "$SIGNING_MODE" ]]; then
      echo "package: SCROZZ_SIGNING_MODE must be explicit for a macOS artifact" >&2
      exit 1
    fi
    if [[ "${SCROZZ_BUNDLE_VALIDATE_ONLY:-0}" != "0" ]]; then
      echo "package: SCROZZ_BUNDLE_VALIDATE_ONLY cannot produce an artifact" >&2
      exit 1
    fi
    NATIVE_VERSION="$BUNDLE_VERSION"
    PACKAGE_ROOT="$STAGE/$NAME"
    mkdir -p "$PACKAGE_ROOT"
    SCROZZ_PREBUILT_BIN="$BIN" \
      SCROZZ_APP_VERSION="$VERSION" \
      SCROZZ_BUILD_NUMBER="${SCROZZ_BUILD_NUMBER:-1}" \
      SCROZZ_SIGNING_MODE="$SIGNING_MODE" \
      SCROZZ_SIGN_IDENTITY="${SCROZZ_SIGN_IDENTITY:-}" \
      tools/make-app-bundle.sh "$PACKAGE_ROOT/Scrozz.app"
    if [[ ! -x "$PACKAGE_ROOT/Scrozz.app/Contents/MacOS/Scrozz" ||
      ! -f "$PACKAGE_ROOT/Scrozz.app/Contents/Info.plist" ]]; then
      echo "package: make-app-bundle did not produce a complete Scrozz.app" >&2
      exit 1
    fi
    plutil -lint "$PACKAGE_ROOT/Scrozz.app/Contents/Info.plist" >/dev/null
    if [[ "$SIGNING_MODE" == "external-release" ]]; then
      if codesign --verify --strict "$PACKAGE_ROOT/Scrozz.app" >/dev/null 2>&1; then
        echo "package: external-release unexpectedly produced a signed app bundle" >&2
        exit 1
      fi
    else
      codesign --verify --strict "$PACKAGE_ROOT/Scrozz.app"
    fi
    copy_documents "$PACKAGE_ROOT"
    ARCHIVE="$DIST/$NAME.zip"
    rm -f "$ARCHIVE" "$ARCHIVE.sha256" "$ARCHIVE.artifact.json"
    ditto -c -k --sequesterRsrc --keepParent "$PACKAGE_ROOT" "$ARCHIVE"
    ;;
  windows)
    if command -v powershell.exe >/dev/null 2>&1; then
      POWERSHELL="powershell.exe"
    elif command -v powershell >/dev/null 2>&1; then
      POWERSHELL="powershell"
    else
      echo "package: PowerShell is required for deterministic ZIP and MSIX output" >&2
      exit 1
    fi
    "$POWERSHELL" \
      -NoLogo \
      -NoProfile \
      -NonInteractive \
      -ExecutionPolicy Bypass \
      -File tools/package-windows.ps1 \
      -OutputDirectory "$DIST" \
      -Binary "$BIN" \
      -Version "$VERSION" \
      -Stamp "$STAMP" \
      -Architecture "$ARCH"
    exit
    ;;
  linux)
    APPDIR="$STAGE/Scrozz.AppDir"
    mkdir -p \
      "$APPDIR/usr/bin" \
      "$APPDIR/usr/share/applications" \
      "$APPDIR/usr/share/icons/hicolor/256x256/apps"
    cp "$BIN" "$APPDIR/usr/bin/scrozz"
    chmod +x "$APPDIR/usr/bin/scrozz"
    copy_documents "$APPDIR"
    if [[ -f assets/icons/icon-256.png ]]; then
      cp assets/icons/icon-256.png "$APPDIR/scrozz.png"
      cp assets/icons/icon-256.png \
        "$APPDIR/usr/share/icons/hicolor/256x256/apps/scrozz.png"
    fi
    cat >"$APPDIR/scrozz.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=Scrozz
Comment=Screenshots and screen recording
Exec=scrozz gui
Icon=scrozz
Categories=Graphics;Utility;
Terminal=false
DESKTOP
    cp "$APPDIR/scrozz.desktop" "$APPDIR/usr/share/applications/scrozz.desktop"
    cat >"$APPDIR/AppRun" <<'APPRUN'
#!/bin/sh
HERE="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
export PATH="$HERE/usr/bin:$PATH"
exec "$HERE/usr/bin/scrozz" "$@"
APPRUN
    chmod +x "$APPDIR/AppRun"
    ARCHIVE="$DIST/$NAME.tar.gz"
    rm -f "$ARCHIVE" "$ARCHIVE.sha256" "$ARCHIVE.artifact.json"
    tar -czf "$ARCHIVE" -C "$STAGE" Scrozz.AppDir
    ;;
esac

if command -v shasum >/dev/null 2>&1; then
  SHA256="$(shasum -a 256 "$ARCHIVE" | awk '{print $1}')"
elif command -v sha256sum >/dev/null 2>&1; then
  SHA256="$(sha256sum "$ARCHIVE" | awk '{print $1}')"
else
  echo "package: no SHA-256 tool is available" >&2
  exit 1
fi
SIZE="$(wc -c <"$ARCHIVE" | tr -d '[:space:]')"
FILE_NAME="$(basename "$ARCHIVE")"
PACKAGE_KIND="portable-archive"
PACKAGE_IDENTITY=""
IDENTITY_MODE="none"
SIGNED=false
PAYLOAD_SIGNED=false
NOTARIZED=false
if [[ "$OS" == "macos" ]]; then
  PACKAGE_KIND="app-bundle-archive"
  PACKAGE_IDENTITY="com.thatcube.Scrozz"
  IDENTITY_MODE="$SIGNING_MODE"
  if [[ "$SIGNING_MODE" != "external-release" ]]; then
    PAYLOAD_SIGNED=true
  fi
fi
printf '%s  %s\n' "$SHA256" "$FILE_NAME" >"$ARCHIVE.sha256"
cat >"$ARCHIVE.artifact.json" <<JSON
{
  "schema": 1,
  "platform": "$PLATFORM",
  "version": "$VERSION",
  "file": "$FILE_NAME",
  "sha256": "$SHA256",
  "size": $SIZE,
  "package_kind": "$PACKAGE_KIND",
  "native_package_version": "$NATIVE_VERSION",
  "package_identity": "$PACKAGE_IDENTITY",
  "identity_mode": "$IDENTITY_MODE",
  "signed": $SIGNED,
  "payload_signed": $PAYLOAD_SIGNED,
  "notarized": $NOTARIZED,
  "signed_manifest": false
}
JSON

echo "built: $ARCHIVE"
echo "sha256: $SHA256"
echo "bytes: $SIZE"
echo "metadata: $ARCHIVE.artifact.json"
