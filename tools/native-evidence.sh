#!/usr/bin/env bash
# Preserve one native runtime command without interpreting its result.
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  tools/native-evidence.sh \
    --output /absolute/path/to/new-run-directory \
    --label LABEL \
    [--source-sha SHA] \
    [--artifact /path/to/artifact] \
    -- COMMAND [ARG...]

Runs COMMAND once and preserves its stdout, stderr, exit status, selected desktop
session variables, host identity, source revision and optional artifact digest.
The output directory must be outside the repository and must not already exist.

This script never calls an exit-zero command a pass. run.properties always says
classification=unreviewed, and skip_marker=present warns that the command may
have skipped. Review the retained evidence before adding a matrix result.
USAGE
}

die() {
  echo "native-evidence: $*" >&2
  exit 2
}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
OUTPUT=""
LABEL=""
SOURCE_SHA=""
ARTIFACT=""

while [[ "$#" -gt 0 ]]; do
  case "$1" in
    --output)
      [[ "$#" -ge 2 ]] || die "--output needs a value"
      OUTPUT="$2"
      shift 2
      ;;
    --label)
      [[ "$#" -ge 2 ]] || die "--label needs a value"
      LABEL="$2"
      shift 2
      ;;
    --source-sha)
      [[ "$#" -ge 2 ]] || die "--source-sha needs a value"
      SOURCE_SHA="$2"
      shift 2
      ;;
    --artifact)
      [[ "$#" -ge 2 ]] || die "--artifact needs a value"
      ARTIFACT="$2"
      shift 2
      ;;
    --help | -h)
      usage
      exit 0
      ;;
    --)
      shift
      break
      ;;
    *)
      die "unknown option '$1'"
      ;;
  esac
done

[[ -n "$OUTPUT" ]] || die "--output is required"
[[ "$OUTPUT" == /* ]] || die "--output must be an absolute path"
[[ -n "$LABEL" ]] || die "--label is required"
[[ "$LABEL" != *$'\n'* && "$LABEL" != *$'\t'* ]] ||
  die "--label cannot contain tabs or newlines"
[[ "$#" -gt 0 ]] || die "a command is required after --"
[[ ! -e "$OUTPUT" ]] || die "output already exists: $OUTPUT"

OUTPUT_PARENT="$(dirname "$OUTPUT")"
mkdir -p "$OUTPUT_PARENT" || die "cannot create output parent: $OUTPUT_PARENT"
OUTPUT="$(cd "$OUTPUT_PARENT" && pwd -P)/$(basename "$OUTPUT")"
case "$OUTPUT/" in
  "$ROOT/"*) die "--output must be outside the repository" ;;
esac

if [[ -z "$SOURCE_SHA" ]]; then
  SOURCE_SHA="$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || true)"
  [[ -n "$SOURCE_SHA" ]] || SOURCE_SHA="unknown"
fi

artifact_hash=""
if [[ -n "$ARTIFACT" ]]; then
  [[ -f "$ARTIFACT" ]] || die "artifact is not a file: $ARTIFACT"
  ARTIFACT="$(cd "$(dirname "$ARTIFACT")" && pwd -P)/$(basename "$ARTIFACT")"
  if command -v sha256sum >/dev/null 2>&1; then
    artifact_hash="$(sha256sum "$ARTIFACT" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    artifact_hash="$(shasum -a 256 "$ARTIFACT" | awk '{print $1}')"
  else
    die "sha256sum or shasum is required when --artifact is used"
  fi
fi

umask 077
mkdir "$OUTPUT" || die "cannot create output directory: $OUTPUT"

printf '%s\n' "$LABEL" >"$OUTPUT/label.txt"
printf '%s\n' "$SOURCE_SHA" >"$OUTPUT/source-sha.txt"
printf '%q ' "$@" >"$OUTPUT/command.sh"
printf '\n' >>"$OUTPUT/command.sh"

{
  printf 'captured_utc=%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
  uname -a 2>/dev/null || true
  if [[ -r /etc/os-release ]]; then
    cat /etc/os-release
  fi
  if command -v sw_vers >/dev/null 2>&1; then
    sw_vers
  fi
  if command -v cmd.exe >/dev/null 2>&1; then
    cmd.exe /d /c ver 2>/dev/null | tr -d '\r'
  fi
} >"$OUTPUT/host.txt"

env |
  grep -E '^(DBUS_SESSION_BUS_ADDRESS|DESKTOP_SESSION|DISPLAY|PROCESSOR_ARCHITECTURE|SESSIONNAME|WAYLAND_DISPLAY|XDG_CURRENT_DESKTOP|XDG_RUNTIME_DIR|XDG_SESSION_TYPE)=' |
  sort >"$OUTPUT/session-environment.txt" || true

git -C "$ROOT" status --porcelain=v1 >"$OUTPUT/source-status.txt" 2>/dev/null || true

if [[ -n "$ARTIFACT" ]]; then
  printf '%s\n' "$ARTIFACT" >"$OUTPUT/artifact.path"
  printf '%s  %s\n' "$artifact_hash" "$(basename "$ARTIFACT")" >"$OUTPUT/artifact.sha256"
fi

set +e
"$@" >"$OUTPUT/stdout.log" 2>"$OUTPUT/stderr.log"
status=$?
set -e

skip_marker="absent"
if grep -Eiq '(^|[^[:alpha:]])skip(ped)?([^[:alpha:]]|$)' \
  "$OUTPUT/stdout.log" "$OUTPUT/stderr.log"; then
  skip_marker="present"
fi

{
  printf 'schema=1\n'
  printf 'classification=unreviewed\n'
  printf 'command_exit=%s\n' "$status"
  printf 'skip_marker=%s\n' "$skip_marker"
} >"$OUTPUT/run.properties"

echo "native-evidence: retained $OUTPUT"
echo "native-evidence: command exit $status; classification remains unreviewed"
if [[ "$skip_marker" == "present" ]]; then
  echo "native-evidence: skip marker found; this run is not pass evidence" >&2
fi

exit "$status"
