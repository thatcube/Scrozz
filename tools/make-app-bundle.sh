#!/usr/bin/env bash
# Build Scrozz.app.
#
# A bare CLI binary has no identity macOS can attach a TCC grant to, so Screen
# Recording is refused no matter how many times you approve it — the grant lands
# on the *responsible process*, usually Terminal, and does not follow the binary.
# A bundle with its own bundle id fixes that: the grant attaches to Scrozz
# rather than to whatever launched it.
#
# How durable that grant is depends on how the bundle is signed. This script
# uses an installed Apple Development identity when one is available, giving
# changed local builds one stable identity. Machines without one fall back to
# an ad-hoc signature, whose effective identity is the binary's changing cdhash
# and therefore requires Screen Recording approval after changed builds. Public
# releases still need Developer ID signing and notarisation — see release.yml.
# Set SCROZZ_SIGNING_MODE (ad-hoc-dev, developer-id-release, external-release)
# to require one exact signing outcome instead of the automatic choice below;
# release.yml does, so a release can never silently fall back to ad-hoc.
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
case "/$APP/" in
  */../* | */./*)
    echo "make-app-bundle: destination must not contain '.' or '..' path components." >&2
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
if [[ -L "$APP" ]]; then
  echo "make-app-bundle: refusing to replace symlink '$APP'." >&2
  exit 1
fi
if [[ -e "$APP" && ! -d "$APP" ]]; then
  echo "make-app-bundle: '$APP' exists and is not a directory." >&2
  exit 1
fi
if [[ -d "$APP" ]]; then
  EXISTING_PLIST="$APP/Contents/Info.plist"
  if [[ ! -f "$EXISTING_PLIST" ]]; then
    echo "make-app-bundle: refusing to replace unrecognized bundle '$APP'." >&2
    exit 1
  fi
  if command -v /usr/libexec/PlistBuddy >/dev/null 2>&1; then
    EXISTING_BUNDLE_ID="$(
      /usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' \
        "$EXISTING_PLIST" 2>/dev/null || true
    )"
    if [[ "$EXISTING_BUNDLE_ID" != "com.thatcube.Scrozz" ]]; then
      echo "make-app-bundle: refusing to replace foreign bundle '$APP'." >&2
      exit 1
    fi
  elif ! grep -q '<string>com\.thatcube\.Scrozz</string>' "$EXISTING_PLIST"; then
    echo "make-app-bundle: refusing to replace foreign bundle '$APP'." >&2
    exit 1
  fi
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

BUILD_PATTERN='^[0-9]+(\.[0-9]+){0,2}$'
BASE_APP_VERSION="${APP_VERSION%%-*}"
PRERELEASE=""
if [[ "$APP_VERSION" == *-* ]]; then
  PRERELEASE="${APP_VERSION#*-}"
fi
if [[ ! "$BASE_APP_VERSION" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] ||
  [[ "$APP_VERSION" == *- && -z "$PRERELEASE" ]] ||
  { [[ -n "$PRERELEASE" ]] &&
    [[ ! "$PRERELEASE" =~ ^[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*$ ]]; }; then
  echo "make-app-bundle: invalid app version '$APP_VERSION'." >&2
  echo "                 Expected a version such as 2026.8.27 or 2026.8.27-rc.1." >&2
  exit 1
fi
if [[ -n "$PRERELEASE" ]]; then
  IFS='.' read -r -a PRERELEASE_PARTS <<<"$PRERELEASE"
  for PART in "${PRERELEASE_PARTS[@]}"; do
    if [[ "$PART" =~ ^[0-9]+$ && ! "$PART" =~ ^(0|[1-9][0-9]*)$ ]]; then
      echo "make-app-bundle: invalid app version '$APP_VERSION'." >&2
      echo "                 Numeric prerelease components cannot have leading zeroes." >&2
      exit 1
    fi
  done
fi
if [[ ! "$BUILD_NUMBER" =~ $BUILD_PATTERN ]]; then
  echo "make-app-bundle: invalid build number '$BUILD_NUMBER'." >&2
  echo "                 Expected one to three numeric components." >&2
  exit 1
fi

# --- deployment floor -------------------------------------------------------
#
# ScreenCaptureKit, the Vision recogniser and the overlay behaviours all assume
# this floor. Declaring it in the bundle makes an older macOS refuse to launch
# rather than fail at the first unavailable symbol.
MINIMUM_MACOS_VERSION="12.3"
DEPLOYMENT_TARGET="${SCROZZ_MACOS_DEPLOYMENT_TARGET:-$MINIMUM_MACOS_VERSION}"
if [[ ! "$DEPLOYMENT_TARGET" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(\.(0|[1-9][0-9]*))?$ ]]; then
  echo "make-app-bundle: invalid macOS deployment target '$DEPLOYMENT_TARGET'" >&2
  exit 1
fi
TARGET_MAJOR="${BASH_REMATCH[1]}"
TARGET_MINOR="${BASH_REMATCH[2]}"
if ((10#$TARGET_MAJOR < 12)) ||
  ((10#$TARGET_MAJOR == 12 && 10#$TARGET_MINOR < 3)); then
  echo "make-app-bundle: macOS deployment target '$DEPLOYMENT_TARGET' is below the true minimum $MINIMUM_MACOS_VERSION" >&2
  exit 1
fi

# --- signing mode -----------------------------------------------------------
#
# Optional. Unset keeps the developer default further down, which picks the
# best identity present. A release sets it so the outcome is a checked promise
# rather than whatever the machine happened to have in its keychain.
SIGNING_MODE="${SCROZZ_SIGNING_MODE:-}"
SIGN_IDENTITY="${SCROZZ_SIGN_IDENTITY:-}"
case "$SIGNING_MODE" in
  "" | ad-hoc-dev) ;;
  developer-id-release)
    if [[ "$SIGN_IDENTITY" != "Developer ID Application:"* ]]; then
      echo "make-app-bundle: developer-id-release requires" >&2
      echo "  SCROZZ_SIGN_IDENTITY='Developer ID Application: ...'" >&2
      exit 1
    fi
    ;;
  external-release)
    if [[ "${SCROZZ_ALLOW_EXTERNAL_SIGNING:-0}" != "1" ]]; then
      echo "make-app-bundle: external-release requires SCROZZ_ALLOW_EXTERNAL_SIGNING=1" >&2
      exit 1
    fi
    ;;
  *)
    echo "make-app-bundle: unknown SCROZZ_SIGNING_MODE '$SIGNING_MODE'" >&2
    echo "  expected ad-hoc-dev, developer-id-release, or external-release" >&2
    exit 1
    ;;
esac

# A dry contract check for callers that only want the arguments validated.
if [[ "${SCROZZ_BUNDLE_VALIDATE_ONLY:-0}" == "1" ]]; then
  if [[ -z "$SIGNING_MODE" ]]; then
    echo "make-app-bundle: validation requires an explicit SCROZZ_SIGNING_MODE." >&2
    exit 1
  fi
  echo "validated: $APP"
  exit 0
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
  # `--features cloud` is what makes a distributed bundle able to share: the
  # feature is off by default so a plain `cargo build` links neither a TLS
  # stack nor a platform credential vault, and every shipped artefact turns it
  # on. See `apps/scrozz/Cargo.toml`.
  echo "==> building release binary (Scrozz $APP_VERSION)"
  SCROZZ_VERSION="$APP_VERSION" \
    SCROZZ_BUILD_NUMBER="$BUILD_NUMBER" \
    CARGO_TARGET_DIR="$TARGET_DIR" cargo build -p scrozz --release --features cloud
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
    --minimum-deployment-target "$DEPLOYMENT_TARGET" \
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
  <key>NSSupportsAutomaticTermination</key><false/>

  <!-- Shown in the permission prompt, so it says why rather than just asking. -->
  <key>NSCameraUsageDescription</key>
  <string>Scrozz uses the camera for webcam overlays while recording.</string>
  <key>NSMicrophoneUsageDescription</key>
  <string>Scrozz records microphone audio when you ask it to narrate a recording.</string>
  <key>NSInputMonitoringUsageDescription</key>
  <string>Scrozz observes clicks and display-ready shortcuts only while you record with interaction overlays enabled.</string>
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
  -c "Set :CFBundleShortVersionString $BASE_APP_VERSION" \
  "$APP/Contents/Info.plist"

# The floor the capture and OCR stacks actually require. Stated in the bundle
# so an older macOS refuses to launch rather than failing at the first API.
/usr/libexec/PlistBuddy \
  -c "Set :LSMinimumSystemVersion $DEPLOYMENT_TARGET" \
  "$APP/Contents/Info.plist"

# Diagnostic/legacy escape hatch only. On macOS 26 this opts back into the
# compatibility container, so release packaging must not enable it blindly.
if [[ "${SCROZZ_INCLUDE_LEGACY_ICON:-0}" == "1" ]]; then
  /usr/libexec/PlistBuddy -c "Add :CFBundleIconFile string Scrozz" \
    "$APP/Contents/Info.plist"
fi

# An explicit mode is a promise about the outcome, so it is checked rather than
# approximated. Without one the developer path picks the best identity it can
# find, which is what makes a local rebuild keep its Screen Recording grant.
case "$SIGNING_MODE" in
  ad-hoc-dev)
    echo "==> signing with an ad-hoc development identity"
    echo "    Screen Recording consent may be requested again after bytes change."
    codesign --force --sign - --identifier com.thatcube.Scrozz "$APP"
    codesign --verify --strict --verbose=2 "$APP"
    ;;
  developer-id-release)
    echo "==> signing with Developer ID release identity '$SIGN_IDENTITY'"
    codesign --force --options runtime --timestamp \
      --sign "$SIGN_IDENTITY" "$APP"
    codesign --verify --strict --verbose=2 "$APP"
    ;;
  external-release)
    echo "==> leaving bundle unsigned for the caller's immediate release-signing step"
    ;;
  "")
    if [[ "${SCROZZ_SKIP_SIGN:-0}" == "1" ]]; then
      echo "==> signing skipped (SCROZZ_SKIP_SIGN=1); caller owns the signature"
    else
      SIGN_IDENTITY="${SCROZZ_SIGN_IDENTITY:-}"
      if [[ -z "$SIGN_IDENTITY" ]] && command -v security >/dev/null 2>&1; then
        SIGN_IDENTITY="$(
          security find-identity -v -p codesigning 2>/dev/null |
            awk '/"Apple Development:/ { print $2; exit }'
        )"
      fi

      if [[ -n "$SIGN_IDENTITY" && "$SIGN_IDENTITY" != "-" ]]; then
        echo "==> signing with a stable Apple Development identity"
        codesign --force --sign "$SIGN_IDENTITY" --identifier com.thatcube.Scrozz \
          --timestamp=none "$APP"
      else
        echo "==> signing ad-hoc (Screen Recording approval will not survive changed builds)"
        codesign --force --sign - --identifier com.thatcube.Scrozz "$APP"
      fi
    fi
    ;;
  *)
    echo "make-app-bundle: unknown SCROZZ_SIGNING_MODE '$SIGNING_MODE'" >&2
    echo "  expected ad-hoc-dev, developer-id-release, or external-release" >&2
    exit 1
    ;;
esac


echo
echo "built: $APP"
echo "version: $APP_VERSION ($VERSION_SOURCE)"
echo "build: $BUILD_NUMBER ($BUILD_SOURCE)"
echo
echo "Scrozz asks for Screen & System Audio Recording only when you capture."
echo "If you choose direct access, the setting lives at:"
echo "  open $APP"
echo "  System Settings > Privacy & Security > Screen & System Audio Recording"
echo "Apple's limited Window/Screen picker remains available where supported."
