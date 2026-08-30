#!/usr/bin/env bash
# Clones/pins the three target repos into .context/ (gitignored). This is a RUNNER-side setup
# step — it runs before any agent starts, and neither agent (octocode or bash) ever calls this
# script or has network access. Idempotent: skips a repo already checked out at the pinned SHA.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONTEXT="$HERE/.context"
mkdir -p "$CONTEXT"

clone_pinned() {
  local name="$1" url="$2" sha="$3"
  local dest="$CONTEXT/$name"
  if [ -d "$dest/.git" ]; then
    local current
    current="$(git -C "$dest" rev-parse HEAD 2>/dev/null || echo "")"
    if [[ "$current" == "$sha"* ]]; then
      echo "[$name] already pinned at $sha, skipping clone"
      return 0
    fi
    echo "[$name] present but at $current, expected $sha — leaving as-is (remove $dest to reclone)"
    return 0
  fi
  echo "[$name] cloning $url @ $sha"
  git clone --quiet "$url" "$dest"
  git -C "$dest" checkout --quiet "$sha"
  git -C "$dest" log -1 --oneline
}

clone_pinned click    https://github.com/pallets/click.git   2c8cd3a
clone_pinned zustand  https://github.com/pmndrs/zustand.git  b57db4f86ef179285da216eeb291266da82c361c
clone_pinned openui   https://github.com/thesysdev/openui.git 78913a1f9f57ba2eb2f4b792d3b6a2134f130c6f

echo "done: $CONTEXT now contains click/, zustand/, openui/"
