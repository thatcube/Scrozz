#!/usr/bin/env bash
# Build Scrozz.app.
#
# A bare CLI binary has no identity macOS can attach a TCC grant to, so Screen
# Recording is refused no matter how many times you approve it — the grant lands
# on the *responsible process*, usually Terminal, and does not follow the binary.
# A bundle with its own bundle id fixes that: the grant attaches to Scrozz
# rather than to whatever launched it.
#
# How durable that grant is depends on how the bundle is signed, and the honest
# answer is "not very, for an ad-hoc signature". TCC keys on the code-signing
# identity, and for an ad-hoc signature that identity is effectively the cdhash,
# which changes every time the binary changes. So an ad-hoc build is a
# *development* convenience: it usually survives an unchanged rebuild and it
# reliably does not survive a changed one, and macOS will ask again. A stable
# identity across releases needs a real Developer ID signature — see the signing
# step at the end of this script.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd -P)"
SCRIPT_PATH="$SCRIPT_DIR/$(basename "$0")"
cd "$SCRIPT_DIR/.."
# shellcheck disable=SC1091
source "$HOME/.cargo/env" 2>/dev/null || true

if [[ -z "${SCROZZ_PREBUILT_BIN:-}" &&
      "${SCROZZ_CARGO_LEASE_HELD:-0}" != "1" &&
      "${CI:-}" != "true" &&
      "${GITHUB_ACTIONS:-}" != "true" &&
      -z "${CARGO_TARGET_DIR:-}" ]]; then
  exec "$SCRIPT_DIR/cargo-pool.sh" "$SCRIPT_PATH" "$@"
fi

# Install where Finder's Applications sidebar actually points. Using
# `$HOME/Applications` made the app valid but effectively invisible to someone
# looking in the normal /Applications folder, and future rebuilds then updated
# the wrong copy.
#
# An explicitly-empty argument is a caller bug — almost always an unset
# variable that expanded to nothing — and must not be read as "use the
# default", because the default installs into /Applications.
if [[ "$#" -gt 0 && -z "$1" ]]; then
  echo "make-app-bundle: destination argument is empty." >&2
  echo "                 Pass a path, or pass nothing to use the default." >&2
  exit 1
fi
APP="${1:-/Applications/Scrozz.app}"

# --- guarding the destructive step -----------------------------------------
#
# Further down this script does `rm -rf "$APP"`, and $APP is caller-supplied.
# A typo, an unset variable in a caller, or a stray argument is otherwise one
# keystroke away from deleting a home directory. The bundle is always a
# freshly-assembled directory, so refusing anything that is not a plausible
# .app path costs nothing and removes the entire class of accident.
case "$APP" in
  "" | "/" | "/*")
    echo "make-app-bundle: refusing to build at '$APP'." >&2
    exit 1
    ;;
esac
if [[ "$APP" != *.app ]]; then
  echo "make-app-bundle: destination must end in .app, got '$APP'." >&2
  echo "make-app-bundle: this path is deleted before assembly, so it is not" >&2
  echo "                 allowed to name anything that is not a bundle." >&2
  exit 1
fi
APP_PARENT="$(dirname "$APP")"
if [[ ! -d "$APP_PARENT" ]]; then
  echo "make-app-bundle: parent directory '$APP_PARENT' does not exist." >&2
  echo "                 Create it first; this script will not mkdir -p a" >&2
  echo "                 path it is also about to rm -rf." >&2
  exit 1
fi
if [[ -e "$APP" && ! -d "$APP" ]]; then
  echo "make-app-bundle: '$APP' exists and is not a directory." >&2
  exit 1
fi

TARGET_DIR="${CARGO_TARGET_DIR:-target}"
if [[ -n "${SCROZZ_APP_VERSION:-}" ]]; then
  APP_VERSION="$SCROZZ_APP_VERSION"
  VERSION_SOURCE="SCROZZ_APP_VERSION override"
else
  YEAR="$(date +%Y)"
  MONTH="$(date +%m)"
  DAY="$(date +%d)"
  APP_VERSION="${YEAR}.$((10#$MONTH)).$((10#$DAY))"
  VERSION_SOURCE="CalVer build date"
fi

if [[ -n "${SCROZZ_BUILD_NUMBER:-}" ]]; then
  BUILD_NUMBER="$SCROZZ_BUILD_NUMBER"
  BUILD_SOURCE="SCROZZ_BUILD_NUMBER override"
elif BUILD_NUMBER="$(git rev-list --count HEAD 2>/dev/null)" &&
     [[ -n "$BUILD_NUMBER" ]]; then
  BUILD_SOURCE="git commit count"
  if [[ -n "$(git status --porcelain 2>/dev/null)" ]]; then
    COUNTER_FILE=".scrozz-dev-build"
    DEV_NUMBER=1
    if [[ -f "$COUNTER_FILE" ]]; then
      STORED_BASE="$(awk '{print $1}' "$COUNTER_FILE" 2>/dev/null || true)"
      STORED_NUMBER="$(awk '{print $2}' "$COUNTER_FILE" 2>/dev/null || true)"
      if [[ "$STORED_BASE" == "$BUILD_NUMBER" &&
            "$STORED_NUMBER" =~ ^[0-9]+$ ]]; then
        DEV_NUMBER=$((STORED_NUMBER + 1))
      fi
    fi
    printf '%s %s\n' "$BUILD_NUMBER" "$DEV_NUMBER" >"$COUNTER_FILE"
    BUILD_NUMBER="${BUILD_NUMBER}.${DEV_NUMBER}"
    BUILD_SOURCE="git commit count + dirty-tree dev suffix"
  fi
else
  BUILD_NUMBER=1
  BUILD_SOURCE="fallback"
fi

VERSION_PATTERN='^[0-9]+(\.[0-9]+){1,2}$'
BUILD_PATTERN='^[0-9]+(\.[0-9]+){0,2}$'
if [[ ! "$APP_VERSION" =~ $VERSION_PATTERN ]]; then
  echo "make-app-bundle: invalid app version '$APP_VERSION'." >&2
  echo "                 Expected a dotted numeric version such as 2026.8.27." >&2
  exit 1
fi
if [[ ! "$BUILD_NUMBER" =~ $BUILD_PATTERN ]]; then
  echo "make-app-bundle: invalid build number '$BUILD_NUMBER'." >&2
  echo "                 Expected one to three numeric components." >&2
  exit 1
fi

# --- where the executable comes from ---------------------------------------
#
# By default this script builds one, which is what a developer wants. The
# release workflow does not: it has already built for two architectures and
# lipo'd them into a universal binary, and rebuilding here would silently
# replace that with a host-only one. SCROZZ_PREBUILT_BIN lets it hand over the
# artefact it means to ship, so there is exactly one implementation of "what a
# Scrozz bundle looks like" rather than a second copy in the workflow that
# quietly drifts from this one.
PREBUILT="${SCROZZ_PREBUILT_BIN:-}"
if [[ -n "$PREBUILT" ]]; then
  if [[ ! -f "$PREBUILT" ]]; then
    echo "make-app-bundle: SCROZZ_PREBUILT_BIN='$PREBUILT' does not exist." >&2
    exit 1
  fi
  echo "==> using prebuilt binary $PREBUILT"
  SOURCE_BIN="$PREBUILT"
else
  echo "==> building release binary (Scrozz $APP_VERSION)"
  SCROZZ_VERSION="$APP_VERSION" \
    CARGO_TARGET_DIR="$TARGET_DIR" cargo build -p scrozz --release
  SOURCE_BIN="$TARGET_DIR/release/scrozz"
fi

echo "==> assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$SOURCE_BIN" "$APP/Contents/MacOS/Scrozz"
chmod +x "$APP/Contents/MacOS/Scrozz"
cp assets/icons/Scrozz.icns "$APP/Contents/Resources/Scrozz.icns"

# macOS 26 puts legacy .icns artwork in a white/silver compatibility container
# ("icon jail"). Compile the layered Icon Composer source into Assets.car so
# Tahoe uses its native system-shaped plate, while CFBundleIconFile below keeps
# the .icns fallback for Sequoia and older.
ICON_DEVELOPER_DIR="${ICON_DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"
if [[ -d assets/icons/Scrozz.icon && -x "$ICON_DEVELOPER_DIR/usr/bin/actool" ]]; then
  DEVELOPER_DIR="$ICON_DEVELOPER_DIR" xcrun actool \
    assets/icons/Scrozz.icon \
    --compile "$APP/Contents/Resources" \
    --app-icon Scrozz.icon \
    --enable-on-demand-resources NO \
    --development-region en \
    --target-device mac \
    --platform macosx \
    --enable-icon-stack-fallback-generation=disabled \
    --include-all-app-icons \
    --minimum-deployment-target 12.3 \
    --output-partial-info-plist /dev/null
fi

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>              <string>Scrozz</string>
  <key>CFBundleDisplayName</key>       <string>Scrozz</string>
  <key>CFBundleExecutable</key>        <string>Scrozz</string>
  <key>CFBundleIdentifier</key>        <string>com.thatcube.Scrozz</string>
  <!--
    Native macOS 26 layered icon from Assets.car; no extension here.
    Do not also declare CFBundleIconFile by default: Finder list view gives the
    legacy .icns key precedence and puts that rendition in the gray/silver
    compatibility container even though a native icon stack is present.
  -->
  <key>CFBundleIconName</key>          <string>Scrozz</string>
  <key>CFBundlePackageType</key>       <string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.0.0</string>
  <key>CFBundleVersion</key>           <string>1</string>
  <key>LSMinimumSystemVersion</key>    <string>12.3</string>

  <!-- Scrozz lives in the menu bar and shows no window at rest (D27). -->
  <key>LSUIElement</key>               <true/>

  <!-- Shown in the permission prompt, so it says why rather than just asking. -->
  <key>NSCameraUsageDescription</key>
  <string>Scrozz uses the camera for webcam overlays while recording.</string>
  <key>NSMicrophoneUsageDescription</key>
  <string>Scrozz records microphone audio when you ask it to narrate a recording.</string>
</dict>
</plist>
PLIST

# Finder/IconServices keys icon caches by bundle identity and build. Reusing
# build 1 while iterating served an old 16px jailed icon even after Assets.car
# and the .icns changed.
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $BUILD_NUMBER" \
  "$APP/Contents/Info.plist"

# Match Plozz's user-facing version model: an unpadded CalVer date by default,
# with a separate build number distinguishing same-day builds. Tagged releases
# pass the same value explicitly so bundle metadata, filenames and `--version`
# cannot disagree.
/usr/libexec/PlistBuddy \
  -c "Set :CFBundleShortVersionString $APP_VERSION" \
  "$APP/Contents/Info.plist"

# Diagnostic/legacy escape hatch only. On macOS 26 this opts back into the
# compatibility container, so release packaging must not enable it blindly.
if [[ "${SCROZZ_INCLUDE_LEGACY_ICON:-0}" == "1" ]]; then
  /usr/libexec/PlistBuddy -c "Add :CFBundleIconFile string Scrozz" \
    "$APP/Contents/Info.plist"
fi

echo "==> signing (ad-hoc, stable identifier)"
# --identifier pins the bundle id into the signature so it does not vary with
# the file name. That is worth doing, but it is not what makes a TCC grant
# durable, and the difference matters:
#
#   ad-hoc (here)   No certificate. TCC effectively keys on the cdhash, which
#                   changes with every rebuild that changes a byte, so Screen
#                   Recording has to be re-approved after most rebuilds. Fine
#                   for development; not an identity.
#
#   Developer ID    A real certificate. The identity is stable across builds
#                   and versions, so the grant persists, and Gatekeeper accepts
#                   the app on a machine that did not build it. This is what a
#                   release needs — see .github/workflows/release.yml, where
#                   signing and notarisation are gated on secrets that only
#                   Brandon can create.
#
# Do not describe an ad-hoc build as preserving permissions. It does not.
#
# SCROZZ_SKIP_SIGN exists for the release path, which signs with a real
# identity immediately afterwards. `codesign --force` would overwrite an ad-hoc
# signature happily, but not doing pointless work makes the log honest about
# which signature the shipped bundle actually carries.
if [[ "${SCROZZ_SKIP_SIGN:-0}" == "1" ]]; then
  echo "    skipped (SCROZZ_SKIP_SIGN=1); caller signs this bundle itself"
else
  codesign --force --sign - --identifier com.thatcube.Scrozz "$APP"
fi

echo
echo "built: $APP"
echo "version: $APP_VERSION ($VERSION_SOURCE)"
echo "build: $BUILD_NUMBER ($BUILD_SOURCE)"
echo
echo "First run needs Screen Recording:"
echo "  open $APP"
echo "  System Settings > Privacy & Security > Screen & System Audio Recording"
echo "  switch Scrozz on, then quit and reopen it"
