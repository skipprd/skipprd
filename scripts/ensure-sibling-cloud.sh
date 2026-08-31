#!/usr/bin/env bash
# skippr-tables-client path-deps ../../../cloud/crates/guest-broker (sibling of skipprd).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ -f "$ROOT/../cloud/crates/guest-broker/Cargo.toml" ]]; then
  exit 0
fi

PARENT="$(cd "$ROOT/.." && pwd)"
DEST="$PARENT/cloud"
if [[ -f "$DEST/crates/guest-broker/Cargo.toml" ]]; then
  exit 0
fi

REF="${SKIPPR_CLOUD_GIT_REF:-}"
echo "cloning skipprd/cloud next to skipprd for guest-broker ($DEST${REF:+ @$REF})" >&2
clone=(git clone --depth 1)
if [[ -n "$REF" ]]; then
  clone+=(--branch "$REF")
fi
if [[ -n "${GITHUB_TOKEN:-}" ]]; then
  "${clone[@]}" "https://x-access-token:${GITHUB_TOKEN}@github.com/skipprd/cloud.git" "$DEST"
else
  "${clone[@]}" git@github.com:skipprd/cloud.git "$DEST"
fi
