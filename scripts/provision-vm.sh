#!/usr/bin/env bash
#
# provision-vm.sh — make a clean Ubuntu LTS VM able to build and test xfar.
#
# Target: Ubuntu LTS (apt-based).
#
# Installs the C/C++ and Rust toolchains, Wayland development libraries, and
# packages used by the nested and headless test sessions.
#
# Usage (inside the VM, as the normal dev user — NOT as root):
#   ./scripts/provision-vm.sh
#
# The script is idempotent: re-running it updates rather than duplicating.
# apt steps use sudo; the Rust toolchain is installed into the invoking user's
# home so cargo is not owned by root.

set -euo pipefail

log() { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }
err() { printf '\033[1;31mError: %s\033[0m\n' "$*" >&2; }

# --- Preconditions ----------------------------------------------------------

if ! command -v apt-get >/dev/null 2>&1; then
    err "apt-get not found. This script targets Ubuntu/Debian."
    exit 1
fi

if [ "$(id -u)" -eq 0 ]; then
    err "Run as the normal dev user, not root. apt steps use sudo internally."
    err "Running rustup as root would install cargo into /root."
    exit 1
fi

if command -v sudo >/dev/null 2>&1; then
    SUDO="sudo"
else
    err "sudo not found but is required for package installation."
    exit 1
fi

export DEBIAN_FRONTEND=noninteractive

# --- System packages --------------------------------------------------------

# Build toolchain. clang/libclang are needed by bindgen-based -sys crates
# (e.g. input-sys, libseat-sys, drm-sys, gbm-sys) pulled in by Smithay.
BUILD_PACKAGES=(
    build-essential
    pkg-config
    cmake
    clang
    libclang-dev
    git
    curl
    ca-certificates
)

# Wayland + Smithay build dependencies.
# libegl1-mesa-dev / libgles2-mesa-dev are transitional but resolve correctly
# on both 22.04 and 24.04, keeping the list valid across current LTS releases.
WAYLAND_PACKAGES=(
    libwayland-dev
    wayland-protocols
    libxkbcommon-dev
    libinput-dev
    libudev-dev
    libgbm-dev
    libdrm-dev
    libseat-dev
    libpixman-1-dev
    libegl1-mesa-dev
    libgles2-mesa-dev
    libdbus-1-dev
    libsystemd-dev
    # xkb keymap data, needed by the compositor's keyboard (libxkbcommon)
    xkb-data
)

# Runtime used for nested/manual Wayland testing:
#   seatd  — seat management daemon (session backend in a headless VM)
#   weston — reference compositor to nest xfar inside, plus weston-terminal
#            as a simple test client.
TEST_RUNTIME_PACKAGES=(
    seatd
    weston
    # wayland-info: a small client for verifying the compositor's socket + globals
    wayland-utils
    # Headless display path for the nested winit backend:
    #   xvfb            — virtual X server (no GPU/monitor needed)
    #   mesa dri/gl     — software GL (llvmpipe) for EGL/GLES rendering
    #   x11vnc          — optional: view the virtual display from the host
    #   imagemagick     — screenshot the virtual display for verification
    xvfb
    libgl1-mesa-dri
    libglx-mesa0
    mesa-utils
    x11vnc
    imagemagick
    # xdotool: inject keyboard/pointer events into the virtual display for tests
    xdotool
)

log "Updating apt package index"
$SUDO apt-get update -y

log "Installing build toolchain"
$SUDO apt-get install -y "${BUILD_PACKAGES[@]}"

log "Installing Wayland / Smithay build dependencies"
$SUDO apt-get install -y "${WAYLAND_PACKAGES[@]}"

log "Installing nested-testing runtime"
$SUDO apt-get install -y "${TEST_RUNTIME_PACKAGES[@]}"

# seatd must be running for the libseat session backend to work in the VM.
if command -v systemctl >/dev/null 2>&1; then
    log "Enabling seatd service"
    $SUDO systemctl enable --now seatd || err "Could not enable seatd (non-fatal in some VMs)"
    # Some distros gate the seatd socket behind a 'seat' group. Add the user to
    # it only if it exists: Ubuntu 26.04 ships no such group and relies on
    # logind instead, so blindly adding it would fail.
    if getent group seat >/dev/null 2>&1; then
        $SUDO usermod -aG seat "$USER" || err "Could not add $USER to 'seat' group (non-fatal)"
    fi
fi

# --- Rust toolchain ---------------------------------------------------------

if command -v cargo >/dev/null 2>&1 || [ -x "$HOME/.cargo/bin/cargo" ]; then
    log "Rust already installed; updating via rustup"
    "$HOME/.cargo/bin/rustup" update stable 2>/dev/null || rustup update stable
else
    log "Installing Rust toolchain via rustup (stable)"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain stable --profile default
fi

# Make cargo available for the rest of this script.
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

log "Ensuring rustfmt and clippy components are present"
rustup component add rustfmt clippy

# Keep build artifacts off the mounted repo. The repo is shared from the host
# over a mount that is measurably slower for cargo output (~2.5x slower cold
# builds); a VM-local target dir avoids that. This config lives only in the VM
# user's home, so host builds are unaffected.
CARGO_CONFIG="$HOME/.cargo/config.toml"
if ! grep -q 'cache/xfar/target' "$CARGO_CONFIG" 2>/dev/null; then
    log "Pointing cargo at a VM-local target directory"
    mkdir -p "$HOME/.cache/xfar"
    cat >>"$CARGO_CONFIG" <<EOF
[build]
target-dir = "$HOME/.cache/xfar/target"
EOF
fi

# --- Verification -----------------------------------------------------------

log "Verifying toolchain"
cargo --version
rustc --version
rustfmt --version
pkg-config --exists wayland-server && echo "wayland-server: OK"
pkg-config --exists xkbcommon && echo "xkbcommon: OK"
pkg-config --exists libinput && echo "libinput: OK"
pkg-config --exists libseat && echo "libseat: OK"

log "Provisioning complete."
echo "If your user was just added to the 'seat' group, log out and back in"
echo "(or reboot the VM) for group membership to take effect."
