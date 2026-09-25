#!/usr/bin/env bash
#
# install.sh — build xfar (release) and install the binary for the current user.
#
# Build in release mode and install to `$XFAR_PREFIX/bin` (default
# `~/.local/bin`). Run in the Linux build environment.
#
#   ./scripts/install.sh
#   XFAR_PREFIX=/usr/local ./scripts/install.sh   # system-wide (needs write perms)

set -euo pipefail

# Make cargo available in non-login shells (e.g. `multipass exec`).
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

PREFIX="${XFAR_PREFIX:-$HOME/.local}"

echo "==> Installing xfar (release) into $PREFIX/bin"
cargo install --path crates/compositor --bin xfar --root "$PREFIX" --force

BIN="$PREFIX/bin/xfar"
if [ ! -x "$BIN" ]; then
    echo "Error: expected installed binary at $BIN was not found" >&2
    exit 1
fi
echo "==> Installed: $BIN"

case ":$PATH:" in
    *":$PREFIX/bin:"*) ;;
    *) echo "    Note: $PREFIX/bin is not on your PATH; add it to run 'xfar' directly." ;;
esac
