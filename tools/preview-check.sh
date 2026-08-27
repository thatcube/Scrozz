#!/usr/bin/env bash
# Decide whether a built Scrozz is a *preview* — a build that runs, but cannot
# take a screenshot — and write the notice that has to travel with it.
#
# Why this exists as its own script
# ---------------------------------
# Two pipelines package Scrozz: tools/package.sh (per-commit CI artifacts) and
# .github/workflows/release.yml (tagged releases). Both have to answer the same
# question and say the same thing about the answer. Written twice, the two
# copies drift, and the first thing to rot is the caveat — which is the part a
# person downloading the file actually needs. So it lives here once.
#
# How the answer is decided
# -------------------------
# It is *probed from the binary*, not hardcoded per platform. Capture is gated
# in apps/scrozz/src/platform.rs: a backend counts as stable only on a platform
# where it has been exercised against a real display, and everywhere else
# `capture_guard` refuses with NotImplemented. `list displays` goes through that
# same guard, so a clean not-implemented envelope from it is a direct reading of
# the gate.
#
# The consequence is that nobody has to remember to update this. When a
# platform's gate opens, the next build stops calling itself a preview.
#
# Note what is deliberately *not* treated as a preview: a permission-denied or
# a "no displays" answer means the gate is open and the runner simply has no
# desktop. That is an environment fact, not a shipping caveat, and mislabelling
# it would make the warning meaningless where it matters.
#
# Usage
# -----
#   tools/preview-check.sh probe  <binary>        prints 1 (gated) or 0 (not)
#   tools/preview-check.sh notice <directory>     writes PREVIEW.txt into it
#
# Environment read by `notice`, all optional and only used for the heading:
#   SCROZZ_VERSION, SCROZZ_STAMP, SCROZZ_PLATFORM
set -uo pipefail

MODE="${1:-}"
ARG="${2:-}"

case "$MODE" in
  probe)
    if [[ -z "$ARG" || ! -x "$ARG" ]]; then
      echo "preview-check: probe needs an executable, got '$ARG'" >&2
      exit 2
    fi
    # 2>&1 because a gated refusal is written to stderr by some shells' idea of
    # a failing command; the envelope is what is being matched either way.
    OUT="$("$ARG" --json list displays 2>&1)"
    case "$OUT" in
      *'"kind":"not-implemented"'*) echo 1 ;;
      *) echo 0 ;;
    esac
    ;;

  notice)
    if [[ -z "$ARG" || ! -d "$ARG" ]]; then
      echo "preview-check: notice needs an existing directory, got '$ARG'" >&2
      exit 2
    fi
    VERSION="${SCROZZ_VERSION:-unknown}"
    STAMP="${SCROZZ_STAMP:-unknown}"
    PLATFORM="${SCROZZ_PLATFORM:-this platform}"
    cat >"$ARG/PREVIEW.txt" <<NOTICE
Scrozz $VERSION ($STAMP) — $PLATFORM — PREVIEW BUILD

This build cannot take a screenshot.

Screen capture is gated per platform. Scrozz only treats a capture backend as
stable on a platform where it has been exercised against a real display, and
this build is from a platform where that has not happened yet. Capture,
recording and display enumeration refuse with exit code 12 ("not-implemented")
and a message naming the crate that owes the work. That refusal is deliberate:
a clean, documented "not yet" is worth more than a plausible-looking failure.

What does work here, and is verified by CI on this exact commit:
  - the CLI surface, its argument parsing and its exit codes
  - the stable JSON envelope (schema 1) on both success and failure
  - the capture-history store: it opens, creates and migrates
  - compositor hotkey config generation
  - the GUI event loop, headless, to a deadline

Setting SCROZZ_UNSTABLE_BACKENDS=1 lifts the guard. That exists so the wiring
can be exercised the moment a backend lands; it does not make an unfinished
backend work, and it can fail in uglier ways than the guarded path would have.

See docs/platforms.md for what each verification layer does and does not prove.
NOTICE
    ;;

  *)
    echo "usage: preview-check.sh probe <binary> | notice <directory>" >&2
    exit 2
    ;;
esac
