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
  tools/run-macos-recording-smoke.sh interactions-package
  SCROZZ_RECORD_INTERACTION_SMOKE=1 tools/run-macos-recording-smoke.sh interactions

The window probe runs as a terminal child and closes its own disposable window.

The microphone probe is different on purpose: it builds and ad-hoc signs a
real .app carrying NSMicrophoneUsageDescription, then runs the probe from that
bundle. It may show Screen & System Audio Recording and Microphone prompts.
Ordinary cargo tests never execute this harness or request microphone access.
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
  microphone|microphone-package|interactions|interactions-package)
    if [[ "$MODE" == "microphone" && "${SCROZZ_RECORD_MIC_SMOKE:-0}" != "1" ]]; then
      echo "Refusing to run without SCROZZ_RECORD_MIC_SMOKE=1." >&2
      usage >&2
      exit 2
    fi
    if [[ "$MODE" == "interactions" && "${SCROZZ_RECORD_INTERACTION_SMOKE:-0}" != "1" ]]; then
      echo "Refusing to run without SCROZZ_RECORD_INTERACTION_SMOKE=1." >&2
      usage >&2
      exit 2
    fi

    TARGET_DIR="${CARGO_TARGET_DIR:-target}"
    APP="${SCROZZ_RECORD_SMOKE_APP:-/tmp/Scrozz Recording Smoke.app}"
    BIN="$TARGET_DIR/debug/examples/macos_recording_smoke"
    if [[ "$APP" != *.app || "$APP" == "/" ]]; then
      echo "SCROZZ_RECORD_SMOKE_APP must name a specific .app path." >&2
      exit 2
    fi
    echo "==> building microphone smoke executable"
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
  <key>NSInputMonitoringUsageDescription</key>
  <string>Scrozz observes clicks and display-ready shortcuts only for this explicitly requested native smoke test.</string>
</dict>
</plist>
PLIST
    codesign --force --sign - --identifier com.thatcube.Scrozz.RecordingSmoke "$APP"
    codesign --verify --strict "$APP"
    if [[ "$MODE" == "microphone-package" || "$MODE" == "interactions-package" ]]; then
      echo "Packaged recording smoke app at $APP (not launched; no permission requested)."
      exit 0
    fi
    echo
    echo "This explicit probe may request recording permissions:"
    echo "  System Settings > Privacy & Security > Screen & System Audio Recording"
    echo "  System Settings > Privacy & Security > Microphone"
    echo "  System Settings > Privacy & Security > Input Monitoring"
    echo "  System Settings > Privacy & Security > Accessibility (synthetic smoke events only)"
    echo "Relaunch this command after granting Screen & System Audio Recording."
    echo
    if [[ "$MODE" == "microphone" ]]; then
      SCROZZ_RECORD_MIC_SMOKE=1 "$APP/Contents/MacOS/ScrozzRecordingSmoke" microphone
    else
      SCROZZ_RECORD_INTERACTION_SMOKE=1 "$APP/Contents/MacOS/ScrozzRecordingSmoke" interactions
    fi
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
