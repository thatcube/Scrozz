#!/usr/bin/env bash
# Detect capture-gated builds and write the warning that travels with them.
set -euo pipefail

mode="${1:-}"
argument="${2:-}"

case "$mode" in
  probe)
    if [[ -z "$argument" || ! -x "$argument" ]]; then
      echo "preview-check: probe needs an executable, got '$argument'" >&2
      exit 2
    fi

    set +e
    output="$("$argument" --json list displays 2>&1)"
    status=$?
    set -e
    if [[ "$status" -eq 12 ]] &&
      [[ "$output" == *'"ok":false'* ]] &&
      [[ "$output" == *'"kind":"not-implemented"'* ]]; then
      echo 1
    elif [[ "$status" -eq 0 && "$output" == *'"ok":true'* ]]; then
      echo 0
    else
      echo "preview-check: capture probe failed with status $status" >&2
      printf '%s\n' "$output" >&2
      exit 1
    fi
    ;;

  notice)
    if [[ -z "$argument" || ! -d "$argument" ]]; then
      echo "preview-check: notice needs an existing directory, got '$argument'" >&2
      exit 2
    fi
    cat >"$argument/PREVIEW.txt" <<NOTICE
Scrozz ${SCROZZ_VERSION:-unknown} (${SCROZZ_STAMP:-unknown}) — ${SCROZZ_PLATFORM:-unknown} — PREVIEW BUILD

This build runs, but its screen-capture backend is still gated off on this
platform. Capture and display enumeration return the documented
"not-implemented" result instead of attempting an unvalidated backend.

The artifact is retained for installation, packaging, CLI, storage, GUI, and
system-integration testing. It must not be presented as a complete release
until the native capture gate opens.
NOTICE
    ;;

  *)
    echo "usage: preview-check.sh probe <binary> | notice <directory>" >&2
    exit 2
    ;;
esac
