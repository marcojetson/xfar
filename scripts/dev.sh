#!/usr/bin/env bash
#
# dev.sh — build and launch xfar for a development session.
#
# Run in the Linux VM. Arguments are passed to the xfar binary.
#
# Uses an existing Wayland/X11 display or starts Xvfb when no display is set.

set -euo pipefail

# Make cargo available in non-login shells (e.g. `multipass exec`), where
# ~/.cargo/env is not sourced automatically.
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# The winit backend runs xfar nested inside a host display. If none is present
# (e.g. a headless VM), start a virtual X server (Xvfb) with software GL.
if [ -n "${WAYLAND_DISPLAY:-}" ]; then
    echo "==> Wayland session detected (WAYLAND_DISPLAY=$WAYLAND_DISPLAY)"
elif [ -n "${DISPLAY:-}" ]; then
    echo "==> X11 session detected (DISPLAY=$DISPLAY)"
elif command -v Xvfb >/dev/null 2>&1; then
    echo "==> No display detected — starting Xvfb :99 with software GL"
    Xvfb :99 -screen 0 1280x800x24 >/tmp/xfar-xvfb.log 2>&1 &
    xvfb_pid=$!
    trap 'kill "$xvfb_pid" 2>/dev/null' EXIT
    export DISPLAY=:99
    export LIBGL_ALWAYS_SOFTWARE=1
    echo "    Watch it live with:  x11vnc -display :99 -localhost   (then a VNC client)"
else
    echo "==> No display and no Xvfb; the winit backend needs a display." >&2
    echo "    Install Xvfb (provisioned by scripts/provision-vm.sh) or run in a" >&2
    echo "    graphical session." >&2
    exit 1
fi

echo "==> Launching xfar (cargo run)"
cargo run --bin xfar -- "$@"
