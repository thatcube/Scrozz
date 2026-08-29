#!/usr/bin/env bash
# Build Scrozz.app.
#
# A bare CLI binary has no identity macOS can attach a TCC grant to, so Screen
# Recording is refused no matter how many times you approve it — the grant lands
# on the *responsible process*, usually Terminal, and does not follow the binary.
# A bundle with its own bundle id fixes that: the grant attaches to Scrozz and
# persists across rebuilds, provided the id and the signature stay stable.
set -euo pipefail

cd "$(dirname "$0")/.."
source "$HOME/.cargo/env" 2>/dev/null || true

# Install where Finder's Applications sidebar actually points. Using
# `$HOME/Applications` made the app valid but effectively invisible to someone
# looking in the normal /Applications folder, and future rebuilds then updated
# the wrong copy.
APP="${1:-/Applications/Scrozz.app}"
TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/scrozz-rel}"
BUILD_NUMBER="${SCROZZ_BUILD_NUMBER:-$(date +%s)}"
SIGN_IDENTITY="${SCROZZ_CODESIGN_IDENTITY:--}"

echo "==> building release binary"
CARGO_TARGET_DIR="$TARGET_DIR" cargo build -p scrozz --release

echo "==> assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$TARGET_DIR/release/scrozz" "$APP/Contents/MacOS/Scrozz"
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

# Diagnostic/legacy escape hatch only. On macOS 26 this opts back into the
# compatibility container, so release packaging must not enable it blindly.
if [[ "${SCROZZ_INCLUDE_LEGACY_ICON:-0}" == "1" ]]; then
  /usr/libexec/PlistBuddy -c "Add :CFBundleIconFile string Scrozz" \
    "$APP/Contents/Info.plist"
fi

if [[ "$SIGN_IDENTITY" == "-" ]]; then
  echo "==> signing (ad-hoc, stable identifier)"
else
  echo "==> signing ($SIGN_IDENTITY)"
fi
# An ad-hoc signature is enough for local use, while an installed Apple
# Development identity keeps one stable designated requirement for hands-on
# builds. Without --identifier every rebuild looks like a different app.
codesign --force --sign "$SIGN_IDENTITY" --identifier com.thatcube.Scrozz "$APP"

echo
echo "built: $APP"
echo
echo "First run needs Screen Recording:"
echo "  open $APP"
echo "  System Settings > Privacy & Security > Screen & System Audio Recording"
echo "  switch Scrozz on, then quit and reopen it"
