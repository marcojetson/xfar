#!/usr/bin/env bash
#
# build.sh — build the xfar workspace.
#
# Build the workspace. Extra arguments are passed to `cargo build`:
#   ./scripts/build.sh              # debug build
#   ./scripts/build.sh --release    # release build

set -euo pipefail

# Make cargo available in non-login shells (e.g. `multipass exec`), where
# ~/.cargo/env is not sourced automatically.
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

echo "Building xfar workspace..."
exec cargo build --workspace "$@"
