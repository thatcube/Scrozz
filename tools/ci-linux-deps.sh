#!/usr/bin/env bash
# Install the Linux system packages Scrozz needs to build, test and render.
#
# This is the single source of truth for that list. CI calls it, and so can a
# Linux contributor on a fresh machine. If you add a dependency that needs a
# `-sys` crate, add its `-dev` package HERE rather than inline in a workflow,
# or the three Linux jobs will drift apart and only one of them will be right.
#
# Debian/Ubuntu only. It is a no-op on any other platform, so it is safe to
# call unconditionally from a cross-platform script.
set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "ci-linux-deps: not Linux ($(uname -s)) — nothing to do."
  exit 0
fi

if ! command -v apt-get >/dev/null 2>&1; then
  echo "ci-linux-deps: no apt-get found. This script only knows Debian/Ubuntu." >&2
  echo "  Install the equivalents of: GTK 3, ayatana-appindicator, xdo, xkbcommon," >&2
  echo "  wayland, X11/xcb, GL/EGL headers, and a software Vulkan driver." >&2
  exit 1
fi

SUDO=""
if [[ "$(id -u)" != "0" ]]; then
  SUDO="sudo"
fi

# --- Why each group is here ------------------------------------------------
#
# Every one of these is load-bearing for a crate already in Cargo.lock. Deleting
# a line does not make CI faster; it makes CI fail with a `pkg-config` error
# roughly four minutes in.
PACKAGES=(
  # Build plumbing. `rusqlite`'s `bundled` feature compiles sqlite3.c, so a C
  # compiler is mandatory, not optional.
  pkg-config
  build-essential
  clang
  libclang-dev

  # GTK 3 — pulled in by `tray-icon` via gtk-sys/atk-sys/gdk-sys/pango-sys.
  # This one package drags in most of the transitive -dev set.
  libgtk-3-dev

  # System tray icon. `libappindicator-sys` resolves the pkg-config module
  # `ayatana-appindicator3-0.1`; see the fallback below for older images.
  libayatana-appindicator3-dev

  # `libxdo-sys` — used by muda/tray-icon for menu and focus handling.
  libxdo-dev

  # winit/glutin: keyboard handling, Wayland, and the X11 fallback path.
  libxkbcommon-dev
  libxkbcommon-x11-dev
  libwayland-dev
  libx11-dev
  libxcursor-dev
  libxi-dev
  libxrandr-dev
  libxcb-render0-dev
  libxcb-shape0-dev
  libxcb-xfixes0-dev

  # OpenGL/EGL headers for the `glow` backend. Deliberately the libglvnd names
  # (`libgl-dev`, `libegl-dev`) rather than `libgl1-mesa-dev`/`libegl1-mesa-dev`:
  # the mesa-prefixed ones are transitional packages that have come and gone
  # between Ubuntu releases, and the runner image is upgraded without warning.
  libgl-dev
  libegl-dev

  # Software rasterisers, for the headless golden-image job (D25).
  #
  # `egui_kittest` renders through wgpu, which needs *some* GPU adapter. A CI
  # runner has no GPU, so we supply software ones: lavapipe (Vulkan) from
  # mesa-vulkan-drivers, and llvmpipe (GL) from libgl1-mesa-dri. Without these
  # the golden tests fail with "no suitable adapter found" — which reads like a
  # code bug and is not one.
  mesa-vulkan-drivers
  libgl1-mesa-dri
  libvulkan1

  # PipeWire, for Wayland screen capture.
  #
  # This is the *runtime* library only, deliberately not `libpipewire-0.3-dev`.
  # scrozz-capture dlopens `libpipewire-0.3.so.0` rather than linking it, so
  # there are no headers to compile against and no pkg-config module to resolve
  # — which is exactly what keeps `cargo check --target x86_64-unknown-linux-gnu`
  # working from a Mac, and what lets an X11-only machine run Scrozz at all
  # instead of failing at load time with an unresolved DT_NEEDED.
  #
  # Two things are NOT installed here because they cannot be made to work in a
  # headless CI container, and installing them would imply otherwise:
  #
  #   pipewire                  the daemon; needs a user session bus
  #   xdg-desktop-portal-*      the portal backend; needs a live compositor
  #
  # tools/wayland-smoke.sh checks for both at runtime and skips with exit 77
  # when they are missing, rather than reporting a pass for a test that never
  # ran. See docs/platforms.md for the full picture. The runtime package is
  # selected after apt metadata is refreshed because Ubuntu 24.04 renamed it
  # with the t64 transition.
)

echo "ci-linux-deps: updating package lists"
# GitHub's apt mirrors fail transiently often enough to be worth a retry. A
# one-off mirror hiccup should not look like a broken build.
for attempt in 1 2 3; do
  if $SUDO apt-get update -qq; then
    break
  fi
  echo "  apt-get update failed (attempt $attempt/3); retrying in 5s" >&2
  sleep 5
  if [[ "$attempt" == "3" ]]; then
    echo "ci-linux-deps: apt-get update failed three times — this is almost" >&2
    echo "  certainly a transient mirror problem, not your change. Re-run the job." >&2
    exit 1
  fi
done

if apt-cache show libpipewire-0.3-0t64 >/dev/null 2>&1; then
  PACKAGES+=(libpipewire-0.3-0t64)
elif apt-cache show libpipewire-0.3-0 >/dev/null 2>&1; then
  PACKAGES+=(libpipewire-0.3-0)
else
  echo "ci-linux-deps: no PipeWire 0.3 runtime package is available." >&2
  exit 1
fi

echo "ci-linux-deps: installing ${#PACKAGES[@]} packages"
if ! $SUDO apt-get install -y --no-install-recommends "${PACKAGES[@]}"; then
  # The appindicator package was renamed across distro generations. Retry once
  # with the pre-Ayatana name before giving up, so a runner image bump does not
  # take the Linux jobs down.
  echo "ci-linux-deps: bulk install failed; retrying with the legacy" >&2
  echo "  libappindicator3-dev name in place of libayatana-appindicator3-dev" >&2
  FALLBACK=()
  for pkg in "${PACKAGES[@]}"; do
    if [[ "$pkg" == "libayatana-appindicator3-dev" ]]; then
      FALLBACK+=("libappindicator3-dev")
    else
      FALLBACK+=("$pkg")
    fi
  done
  $SUDO apt-get install -y --no-install-recommends "${FALLBACK[@]}"
fi

echo "ci-linux-deps: done"
