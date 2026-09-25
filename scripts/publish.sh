#!/usr/bin/env bash
# Publish a scrubbed copy of xfar to the public GitHub repo.
#
# Clone the private repository, remove internal-only files from every commit,
# and force-push the rewritten main branch. The private history is unchanged.
#
# Run this periodically (or before opening the repo up), then flip visibility:
#
#     gh repo edit marcojetson/xfarwm --visibility public
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d /tmp/xfar-publish.XXXXXX)"
trap 'rm -rf "$work"' EXIT

private_paths=(AGENTS.md PRD.md DECISIONS.md TODO.md HANDOFF.md)
public="$(git -C "$repo_root" remote get-url origin)"

clone="$work/xfar"
git clone --no-hardlinks --quiet "$repo_root" "$clone"

(
    cd "$clone"
    # strip the internal-only docs from the whole history
    args=()
    for p in "${private_paths[@]}"; do
        args+=(--path "$p")
    done
    git filter-repo --force --invert-paths "${args[@]}"

    # self-check: none of the internal docs may remain in the scrubbed history
    for p in "${private_paths[@]}"; do
        if git log --all --oneline -- "$p" | grep -q .; then
            echo "scrub failed: '$p' still in history" >&2
            exit 1
        fi
    done

    git remote add origin "$public"
    git push --force origin main
)
echo "Published scrubbed history ($clone)."