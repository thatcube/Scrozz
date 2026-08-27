#!/usr/bin/env bash
# Package the built binary into something a person can actually install.
#
# # Why this is separate from release.yml
#
# `.github/workflows/release.yml` already packages Scrozz — at a tag, for a
# draft GitHub release, with signing and notarisation gated on secrets. That is
# the shipping path and this is deliberately not it.
#
# This script exists for the *per-commit* path: every green CI run should leave
# behind an artifact someone can download and run, on the commit that produced
# it. That is what turns "CI is green" from a claim into something testable, and
# it is what lets a maintainer bisect a behavioural regression without building
# three platforms locally. It is unsigned by construction — no secret is read,
# invented, or implied here.
#
# # What each platform gets, and why
#
#   macOS    Scrozz.app, zipped with `ditto`. Not a bare binary: a CLI
#            executable has no bundle identity for macOS to attach a TCC grant
#            to, so Screen Recording is refused no matter how often it is
#            approved (the whole reason tools/make-app-bundle.sh exists). An
#            artifact that cannot be granted permission cannot be used to test
#            the feature the app is for. `ditto` rather than `zip` because it is
#            the only archiver that reliably preserves the bundle's symlinks and
#            the code signature.
#
#   Windows  scrozz.exe in a zip. No installer: there is nothing to install:
#            one self-contained executable and no registry state.
#
#   Linux    An AppDir, tarred. Not an .AppImage — building one needs
#            appimagetool and a FUSE-capable runner, and a half-working AppImage
#            is worse than an honest precursor. What is here is the complete
#            AppDir layout (AppRun, .desktop, icon, usr/bin) that appimagetool
#            takes as its input, so producing the AppImage later is one command
#            with no repackaging. It is also directly runnable as-is.
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1
# shellcheck disable=SC1091
source "$HOME/.cargo/env" 2>/dev/null || true

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  cat <<'USAGE'
Usage:
  tools/package.sh [output-dir]

Packages the already-built release binary for the current platform.
Default output directory is dist/.

Environment:
  SCROZZ_BIN        path to the built executable
                    (default: target/release/scrozz[.exe])
  SCROZZ_STAMP      identifier baked into the archive name, normally the short
                    commit sha (default: the current git short sha, else "dev")

Exit status:
  0   an archive was produced
  1   packaging failed
USAGE
  exit 0
fi

DIST="${1:-dist}"

case "${RUNNER_OS:-$(uname -s)}" in
  Darwin | macOS) OS="macos" ;;
  Linux) OS="linux" ;;
  Windows | MINGW* | MSYS* | CYGWIN*) OS="windows" ;;
  *)
    echo "package: unsupported platform '$(uname -s)'" >&2
    exit 1
    ;;
esac

case "$(uname -m)" in
  arm64 | aarch64) ARCH="arm64" ;;
  x86_64 | amd64) ARCH="x86_64" ;;
  *) ARCH="$(uname -m)" ;;
esac

BIN="${SCROZZ_BIN:-}"
if [[ -z "$BIN" ]]; then
  if [[ "${CI:-}" != "true" &&
        "${GITHUB_ACTIONS:-}" != "true" &&
        "${SCROZZ_CARGO_LEASE_HELD:-0}" != "1" &&
        -z "${CARGO_TARGET_DIR:-}" ]]; then
    echo "package: refusing an unowned target/release binary." >&2
    echo "package: build and package one leased binary with: tools/dev.sh package" >&2
    exit 1
  fi
  TARGET_DIR="${CARGO_TARGET_DIR:-target}"
  BIN="$TARGET_DIR/release/scrozz"
  [[ -x "$BIN" ]] || BIN="$TARGET_DIR/release/scrozz.exe"
fi
if [[ ! -x "$BIN" ]]; then
  echo "package: no executable at '$BIN'." >&2
  echo "package: build and package one leased binary with: tools/dev.sh package" >&2
  exit 1
fi

STAMP="${SCROZZ_STAMP:-$(git rev-parse --short HEAD 2>/dev/null || echo dev)}"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' apps/scrozz/Cargo.toml | head -1)"
[[ -n "$VERSION" ]] || VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
[[ -n "$VERSION" ]] || VERSION="0.0.0"

NAME="scrozz-${VERSION}-${STAMP}-${OS}-${ARCH}"

# --- preview labelling ------------------------------------------------------
#
# An artifact from a platform whose capture gate is closed still builds, runs,
# opens its store and drives its GUI — but it cannot take a screenshot, which
# is the thing the app is for. Shipping that under the same name as a working
# build would be the most misleading thing in this pipeline, so it gets a
# suffix and a notice. tools/preview-check.sh decides, by probing the binary.
PREVIEW="$(SCROZZ_VERSION="$VERSION" SCROZZ_STAMP="$STAMP" \
  tools/preview-check.sh probe "$BIN")"
if [[ "$PREVIEW" -eq 1 ]]; then
  NAME="${NAME}-preview"
fi

rm -rf "$DIST"
mkdir -p "$DIST"

echo "==> packaging $NAME"
echo "    binary:  $BIN"
echo "    version: $VERSION"

ARCHIVE=""

# The notice that travels with a gated build, written into the archive where
# there is a natural place for it and always alongside the archive so the
# caveat is visible in the artifact listing without downloading anything.
write_preview_notice() {
  SCROZZ_VERSION="$VERSION" SCROZZ_STAMP="$STAMP" SCROZZ_PLATFORM="$OS/$ARCH" \
    tools/preview-check.sh notice "$1"
}

case "$OS" in
  macos)
    STAGE="$DIST/stage"
    mkdir -p "$STAGE"
    # Reuse the real bundler rather than reimplementing it, so the artifact and
    # a developer's local build are the same thing. It builds through cargo, so
    # point it at the target directory the release binary already lives in and
    # the build is a no-op rebuild rather than a second full compile.
    if ! CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target}" \
      SCROZZ_PREBUILT_BIN="$BIN" \
      SCROZZ_BUILD_NUMBER="${SCROZZ_BUILD_NUMBER:-1}" \
      tools/make-app-bundle.sh "$STAGE/Scrozz.app"; then
      echo "package: make-app-bundle.sh failed" >&2
      exit 1
    fi
    ARCHIVE="$DIST/$NAME.zip"
    # --keepParent so the archive expands to Scrozz.app, not its contents.
    if ! ditto -c -k --keepParent "$STAGE/Scrozz.app" "$ARCHIVE"; then
      echo "package: ditto failed" >&2
      exit 1
    fi
    rm -rf "$STAGE"
    ;;

  windows)
    STAGE="$DIST/$NAME"
    mkdir -p "$STAGE"
    cp "$BIN" "$STAGE/scrozz.exe"
    [[ -f README.md ]] && cp README.md "$STAGE/"
    [[ -f LICENSE ]] && cp LICENSE "$STAGE/"
    [[ "$PREVIEW" -eq 1 ]] && write_preview_notice "$STAGE"
    ARCHIVE="$DIST/$NAME.zip"
    # 7z ships on the hosted Windows image; the others are fallbacks for a
    # developer's machine.
    if command -v 7z >/dev/null 2>&1; then
      (cd "$DIST" && 7z a -bso0 -bsp0 "$NAME.zip" "$NAME") || exit 1
    elif command -v zip >/dev/null 2>&1; then
      (cd "$DIST" && zip -qr "$NAME.zip" "$NAME") || exit 1
    elif command -v powershell >/dev/null 2>&1; then
      powershell -NoProfile -Command \
        "Compress-Archive -Path '$STAGE' -DestinationPath '$ARCHIVE' -Force" || exit 1
    else
      echo "package: no archiver available (tried 7z, zip, powershell)" >&2
      exit 1
    fi
    rm -rf "$STAGE"
    ;;

  linux)
    APPDIR="$DIST/Scrozz.AppDir"
    mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" \
      "$APPDIR/usr/share/icons/hicolor/256x256/apps"
    cp "$BIN" "$APPDIR/usr/bin/scrozz"
    chmod +x "$APPDIR/usr/bin/scrozz"

    # appimagetool requires the icon and .desktop file at the AppDir root, and
    # conventionally also under usr/share. Both are provided so the directory is
    # valid input without any fixing-up step later.
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
Exec=scrozz
Icon=scrozz
Categories=Graphics;Utility;
Terminal=false
DESKTOP
    cp "$APPDIR/scrozz.desktop" "$APPDIR/usr/share/applications/scrozz.desktop"
    [[ "$PREVIEW" -eq 1 ]] && write_preview_notice "$APPDIR"

    cat >"$APPDIR/AppRun" <<'APPRUN'
#!/bin/sh
# AppImage entry point. Resolves the AppDir from this script's own location so
# the bundle works from any mount point, which is what an AppImage needs.
HERE="$(dirname "$(readlink -f "$0")")"
export PATH="$HERE/usr/bin:$PATH"
exec "$HERE/usr/bin/scrozz" "$@"
APPRUN
    chmod +x "$APPDIR/AppRun"

    ARCHIVE="$DIST/$NAME.tar.gz"
    if ! tar -czf "$ARCHIVE" -C "$DIST" Scrozz.AppDir; then
      echo "package: tar failed" >&2
      exit 1
    fi
    rm -rf "$APPDIR"
    ;;
esac

if [[ ! -f "$ARCHIVE" ]]; then
  echo "package: no archive was produced" >&2
  exit 1
fi

# Also beside the archive, so the artifact listing itself shows the caveat.
if [[ "$PREVIEW" -eq 1 ]]; then
  write_preview_notice "$DIST"
  echo
  echo "PREVIEW: capture is gated off in this build (probed via 'list displays')."
fi

# A checksum so a downloaded artifact can be matched to the run that built it.
SUM=""
if command -v shasum >/dev/null 2>&1; then
  SUM="$(shasum -a 256 "$ARCHIVE" | awk '{print $1}')"
elif command -v sha256sum >/dev/null 2>&1; then
  SUM="$(sha256sum "$ARCHIVE" | awk '{print $1}')"
fi
[[ -n "$SUM" ]] && echo "$SUM  $(basename "$ARCHIVE")" >"$ARCHIVE.sha256"

SIZE="$(wc -c <"$ARCHIVE" | tr -d ' ')"

echo
echo "built: $ARCHIVE"
echo "bytes: $SIZE"
[[ -n "$SUM" ]] && echo "sha256: $SUM"

if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  {
    echo "### Artifact — $OS ($ARCH)"
    echo
    if [[ "$PREVIEW" -eq 1 ]]; then
      echo "> [!WARNING]"
      echo "> **Preview build — this artifact cannot take a screenshot.**"
      echo "> Capture is gated off on this platform, so capture, recording and"
      echo "> display enumeration refuse with exit 12. The CLI, JSON envelope,"
      echo "> store, hotkey config generation and headless GUI loop all work and"
      echo "> are verified by the smoke checks above. See \`PREVIEW.txt\`."
      echo
    fi
    echo "| field | value |"
    echo "| --- | --- |"
    echo "| file | \`$(basename "$ARCHIVE")\` |"
    echo "| bytes | $SIZE |"
    echo "| sha256 | \`${SUM:-unavailable}\` |"
    echo "| commit | \`$STAMP\` |"
    echo "| capture | $([[ "$PREVIEW" -eq 1 ]] && echo 'gated off (preview)' || echo 'enabled') |"
    echo
    case "$OS" in
      macos) echo "_An ad-hoc signed \`.app\` bundle, so Screen Recording can be granted to it at all — a bare binary cannot hold that grant. An ad-hoc signature is a development identity: it changes with the build, so macOS will ask again after a rebuild. Stable identity across releases needs Developer ID signing, which \`release.yml\` does when the secrets exist._" ;;
      linux) echo "_An AppDir: runnable as \`Scrozz.AppDir/AppRun\`, and valid input for \`appimagetool\` without further fixing-up._" ;;
      windows) echo "_A self-contained executable. Nothing to install._" ;;
    esac
  } >>"$GITHUB_STEP_SUMMARY"
fi

exit 0
