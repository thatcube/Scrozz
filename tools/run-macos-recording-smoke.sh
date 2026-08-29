#!/usr/bin/env bash
# Explicit native recording probes. Neither mode runs from ordinary test suites.
set -euo pipefail

cd "$(dirname "$0")/.."
source "$HOME/.cargo/env" 2>/dev/null || true

usage() {
  cat <<'EOF'
Usage:
  SCROZZ_RECORD_WINDOW_SMOKE=1 tools/run-macos-recording-smoke.sh window-disappearance
  tools/run-macos-recording-smoke.sh microphone-package
  SCROZZ_RECORD_MIC_SMOKE=1 tools/run-macos-recording-smoke.sh microphone
  tools/run-macos-recording-smoke.sh camera-package
  SCROZZ_RECORD_CAMERA_SMOKE=1 tools/run-macos-recording-smoke.sh camera
  SCROZZ_CAMERA_PREVIEW_SMOKE=1 tools/run-macos-recording-smoke.sh camera-preview

The window probe runs as a terminal child and closes its own disposable window.

The microphone and camera probes are different on purpose: they build and
ad-hoc sign a real .app carrying the matching usage descriptions, then run from
that bundle. They may show Screen & System Audio Recording, Microphone, or
Camera prompts. Ordinary cargo tests never execute this harness or request
camera/microphone access.
EOF
}

MODE="${1:-}"
case "$MODE" in
  window-disappearance)
    if [[ "${SCROZZ_RECORD_WINDOW_SMOKE:-0}" != "1" ]]; then
      echo "Refusing to run without SCROZZ_RECORD_WINDOW_SMOKE=1." >&2
      usage >&2
      exit 2
    fi
    cargo run -p scrozz-record --example macos_recording_smoke -- window-disappearance
    ;;
  microphone|microphone-package|camera|camera-package|camera-preview)
    if [[ "$MODE" == "microphone" && "${SCROZZ_RECORD_MIC_SMOKE:-0}" != "1" ]]; then
      echo "Refusing to run without SCROZZ_RECORD_MIC_SMOKE=1." >&2
      usage >&2
      exit 2
    fi
    if [[ "$MODE" == "camera" && "${SCROZZ_RECORD_CAMERA_SMOKE:-0}" != "1" ]]; then
      echo "Refusing to run without SCROZZ_RECORD_CAMERA_SMOKE=1." >&2
      usage >&2
      exit 2
    fi
    if [[ "$MODE" == "camera-preview" && "${SCROZZ_CAMERA_PREVIEW_SMOKE:-0}" != "1" ]]; then
      echo "Refusing to run without SCROZZ_CAMERA_PREVIEW_SMOKE=1." >&2
      usage >&2
      exit 2
    fi

    TARGET_DIR="${CARGO_TARGET_DIR:-target}"
    APP="${SCROZZ_RECORD_SMOKE_APP:-/tmp/Scrozz Recording Smoke.app}"
    BUNDLE_ID="${SCROZZ_RECORD_SMOKE_BUNDLE_ID:-com.thatcube.Scrozz.RecordingSmoke}"
    BIN="$TARGET_DIR/debug/examples/macos_recording_smoke"
    if [[ "$APP" != *.app || "$APP" == "/" ]]; then
      echo "SCROZZ_RECORD_SMOKE_APP must name a specific .app path." >&2
      exit 2
    fi
    echo "==> building ${MODE%-package} smoke executable"
    cargo build -p scrozz-record --example macos_recording_smoke
    echo "==> assembling $APP"
    rm -rf "$APP"
    mkdir -p "$APP/Contents/MacOS"
    cp "$BIN" "$APP/Contents/MacOS/ScrozzRecordingSmoke"
    cat >"$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Scrozz Recording Smoke</string>
  <key>CFBundleDisplayName</key><string>Scrozz Recording Smoke</string>
  <key>CFBundleExecutable</key><string>ScrozzRecordingSmoke</string>
  <key>CFBundleIdentifier</key><string>com.thatcube.Scrozz.RecordingSmoke</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>LSUIElement</key><true/>
  <key>NSMicrophoneUsageDescription</key>
  <string>Scrozz records microphone audio only for this explicitly requested native smoke test.</string>
  <key>NSCameraUsageDescription</key>
  <string>Scrozz uses the camera only for this explicitly requested native smoke test.</string>
</dict>
</plist>
PLIST
    /usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier $BUNDLE_ID" "$APP/Contents/Info.plist"
    codesign --force --sign - --identifier "$BUNDLE_ID" "$APP"
    codesign --verify --strict "$APP"
    if [[ "$MODE" == "microphone-package" || "$MODE" == "camera-package" ]]; then
      echo "Packaged ${MODE%-package} smoke app at $APP (not launched; no permission requested)."
      exit 0
    fi
    echo
    echo "This explicit probe may request these permissions:"
    if [[ "$MODE" != "camera-preview" ]]; then
      echo "  System Settings > Privacy & Security > Screen & System Audio Recording"
    fi
    if [[ "$MODE" == camera* ]]; then
      echo "  System Settings > Privacy & Security > Camera"
    else
      echo "  System Settings > Privacy & Security > Microphone"
    fi
    echo "Relaunch this command after granting any requested permission."
    echo
    LOG_DIR="$(mktemp -d /tmp/scrozz-recording-smoke.XXXXXX)"
    STDOUT_LOG="$LOG_DIR/stdout.log"
    STDERR_LOG="$LOG_DIR/stderr.log"
    if [[ "$MODE" == "camera" ]]; then
      open -W -n --stdout "$STDOUT_LOG" --stderr "$STDERR_LOG" \
        --env SCROZZ_RECORD_CAMERA_SMOKE=1 "$APP" --args camera
      SUCCESS_PATTERN="camera smoke encoded"
    elif [[ "$MODE" == "camera-preview" ]]; then
      open -W -n --stdout "$STDOUT_LOG" --stderr "$STDERR_LOG" \
        --env SCROZZ_CAMERA_PREVIEW_SMOKE=1 "$APP" --args camera-preview
      SUCCESS_PATTERN="camera preview smoke captured"
    else
      open -W -n --stdout "$STDOUT_LOG" --stderr "$STDERR_LOG" \
        --env SCROZZ_RECORD_MIC_SMOKE=1 "$APP" --args microphone
      SUCCESS_PATTERN="microphone smoke encoded"
    fi
    cat "$STDOUT_LOG"
    cat "$STDERR_LOG" >&2
    if ! grep -q "$SUCCESS_PATTERN" "$STDOUT_LOG"; then
      rm -f "$STDOUT_LOG" "$STDERR_LOG"
      rmdir "$LOG_DIR"
      echo "native smoke did not report success" >&2
      exit 1
    fi
    rm -f "$STDOUT_LOG" "$STDERR_LOG"
    rmdir "$LOG_DIR"
    ;;
  -h|--help|"")
    usage
    ;;
  *)
    echo "Unknown smoke mode: $MODE" >&2
    usage >&2
    exit 2
    ;;
esac
