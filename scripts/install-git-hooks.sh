#!/usr/bin/env bash
# Copy versioned hooks into .git/hooks. Does not change git config.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GIT_DIR="$(git -C "$ROOT" rev-parse --git-dir)"
if [[ "$GIT_DIR" != /* ]]; then
  GIT_DIR="$ROOT/$GIT_DIR"
fi
mkdir -p "$GIT_DIR/hooks"
cp "$ROOT/.githooks/pre-commit" "$GIT_DIR/hooks/pre-commit"
chmod +x "$ROOT/.githooks/pre-commit" "$ROOT/scripts/precommit.sh" "$GIT_DIR/hooks/pre-commit"
echo "installed $GIT_DIR/hooks/pre-commit"
