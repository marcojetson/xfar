#!/usr/bin/env bash
#
# test.sh — run xfar's automated checks (formatting + tests).
#
# Extra arguments are passed to `cargo test`:
#   ./scripts/test.sh                 # fmt check + all tests
#   ./scripts/test.sh reports_crate   # fmt check + tests matching a filter

set -euo pipefail

# Make cargo available in non-login shells (e.g. `multipass exec`), where
# ~/.cargo/env is not sourced automatically.
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

echo "==> Checking formatting (cargo fmt --all --check)"
cargo fmt --all --check

echo "==> Running tests (cargo test --workspace)"
cargo test --workspace "$@"
