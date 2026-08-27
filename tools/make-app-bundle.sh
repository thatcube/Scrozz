#!/usr/bin/env bash
# Build Scrozz.app.
#
# A bare CLI binary has no identity macOS can attach a TCC grant to, so Screen
# Recording is refused no matter how many times you approve it — the grant lands
# on the *responsible process*, usually Terminal, and does not follow the binary.
# A bundle with its own bundle id fixes that: the grant attaches to Scrozz
# rather than to whatever launched it. A development ad-hoc signature changes
# identity when its bytes change; a Developer ID release identity is stable.
set -euo pipefail

cd "$(dirname "$0")/.."
source "$HOME/.cargo/env" 2>/dev/null || true

# An explicitly empty argument is almost always an unset variable. It must not
# silently select /Applications, because this script deletes the destination.
if [[ "$#" -gt 0 && -z "$1" ]]; then
  echo "make-app-bundle: destination argument is empty" >&2
  exit 1
fi
APP="${1:-/Applications/Scrozz.app}"

# Resolve an existing parent before validating the one path this script may
# recursively delete. Absolute paths are valid; only root, traversal, symlinks,
# non-bundles, and unrecognised existing directories are rejected.
case "$APP" in
  "" | "/" | "." | ".." | */../* | ../* | */..)
    echo "make-app-bundle: refusing unsafe destination '$APP'" >&2
    exit 1
    ;;
esac
if [[ "$(basename "$APP")" != "Scrozz.app" ]]; then
  echo "make-app-bundle: destination must name Scrozz.app, got '$APP'" >&2
  exit 1
fi
APP_PARENT="$(dirname "$APP")"
if [[ ! -d "$APP_PARENT" ]]; then
  echo "make-app-bundle: parent directory '$APP_PARENT' does not exist" >&2
  exit 1
fi
APP_PARENT="$(cd "$APP_PARENT" && pwd -P)"
APP="$APP_PARENT/Scrozz.app"
if [[ -L "$APP" ]]; then
  echo "make-app-bundle: refusing symlink destination '$APP'" >&2
  exit 1
fi
if [[ -e "$APP" && ! -d "$APP" ]]; then
  echo "make-app-bundle: destination exists and is not a directory: '$APP'" >&2
  exit 1
fi
if [[ -d "$APP" ]]; then
  EXISTING_PLIST="$APP/Contents/Info.plist"
  if [[ ! -f "$EXISTING_PLIST" ]] ||
    [[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$EXISTING_PLIST" 2>/dev/null || true)" != "com.thatcube.Scrozz" ]]; then
    echo "make-app-bundle: refusing to delete an unrecognised directory at '$APP'" >&2
    exit 1
  fi
fi

BUILD_NUMBER="${SCROZZ_BUILD_NUMBER:-$(date +%s)}"
if [[ ! "$BUILD_NUMBER" =~ ^[0-9]+(\.[0-9]+){0,2}$ ]]; then
  echo "make-app-bundle: invalid CFBundleVersion '$BUILD_NUMBER'" >&2
  exit 1
fi

APP_VERSION="${SCROZZ_APP_VERSION:-0.1.0}"
if [[ ! "$APP_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "make-app-bundle: invalid CFBundleShortVersionString '$APP_VERSION'" >&2
  exit 1
fi

if [[ "${SCROZZ_BUNDLE_VALIDATE_ONLY:-0}" == "1" ]]; then
  echo "validated: $APP"
  exit 0
fi

TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/scrozz-rel}"

PREBUILT="${SCROZZ_PREBUILT_BIN:-}"
if [[ -n "$PREBUILT" ]]; then
  if [[ ! -f "$PREBUILT" ]]; then
    echo "make-app-bundle: prebuilt binary does not exist: '$PREBUILT'" >&2
    exit 1
  fi
  SOURCE_BIN="$PREBUILT"
  echo "==> using prebuilt binary $SOURCE_BIN"
else
  echo "==> building release binary"
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
  <key>CFBundleURLTypes</key>
  <array>
    <dict>
      <key>CFBundleURLName</key>        <string>com.thatcube.Scrozz.url</string>
      <key>CFBundleURLSchemes</key>
      <array><string>scrozz</string></array>
      <key>CFBundleTypeRole</key>       <string>Editor</string>
    </dict>
  </array>
  <!--
    Native macOS 26 layered icon from Assets.car; no extension here.
    Do not also declare CFBundleIconFile by default: Finder list view gives the
    legacy .icns key precedence and puts that rendition in the gray/silver
    compatibility container even though a native icon stack is present.
  -->
  <key>CFBundleIconName</key>          <string>Scrozz</string>
  <key>CFBundlePackageType</key>       <string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
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

/usr/libexec/PlistBuddy \
  -c "Set :CFBundleShortVersionString $APP_VERSION" \
  "$APP/Contents/Info.plist"

# Diagnostic/legacy escape hatch only. On macOS 26 this opts back into the
# compatibility container, so release packaging must not enable it blindly.
if [[ "${SCROZZ_INCLUDE_LEGACY_ICON:-0}" == "1" ]]; then
  /usr/libexec/PlistBuddy -c "Add :CFBundleIconFile string Scrozz" \
    "$APP/Contents/Info.plist"
fi

SIGNING_MODE="${SCROZZ_SIGNING_MODE:-ad-hoc-dev}"
case "$SIGNING_MODE" in
  ad-hoc-dev)
    echo "==> signing with an ad-hoc development identity"
    echo "    Screen Recording consent may be requested again after bytes change."
    codesign --force --sign - --identifier com.thatcube.Scrozz "$APP"
    ;;
  developer-id-release)
    SIGN_IDENTITY="${SCROZZ_SIGN_IDENTITY:-}"
    if [[ "$SIGN_IDENTITY" != "Developer ID Application:"* ]]; then
      echo "make-app-bundle: developer-id-release requires" >&2
      echo "  SCROZZ_SIGN_IDENTITY='Developer ID Application: ...'" >&2
      exit 1
    fi
    echo "==> signing with Developer ID release identity '$SIGN_IDENTITY'"
    codesign --force --options runtime --timestamp \
      --sign "$SIGN_IDENTITY" "$APP"
    codesign --verify --strict --verbose=2 "$APP"
    ;;
  external-release)
    if [[ "${SCROZZ_ALLOW_EXTERNAL_SIGNING:-0}" != "1" ]]; then
      echo "make-app-bundle: external-release requires SCROZZ_ALLOW_EXTERNAL_SIGNING=1" >&2
      exit 1
    fi
    echo "==> leaving bundle unsigned for the caller's immediate release-signing step"
    ;;
  *)
    echo "make-app-bundle: unknown SCROZZ_SIGNING_MODE '$SIGNING_MODE'" >&2
    echo "  expected ad-hoc-dev, developer-id-release, or external-release" >&2
    exit 1
    ;;
esac

echo
echo "built: $APP"
echo
echo "First run needs Screen Recording:"
echo "  open $APP"
echo "  System Settings > Privacy & Security > Screen & System Audio Recording"
echo "  switch Scrozz on, then quit and reopen it"
