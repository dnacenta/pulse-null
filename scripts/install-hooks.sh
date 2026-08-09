#!/usr/bin/env bash
# Install the repo's git hooks (currently: pre-push -> scripts/gate.sh).
#
# One-time setup per clone/worktree:
#   scripts/install-hooks.sh
#
# This points core.hooksPath at .githooks so the gate runs before every
# push. The server-side copy of the same gate (.github/workflows/ci.yml)
# is the authoritative one — this hook just saves the round-trip.

set -euo pipefail
cd "$(dirname "$0")/.."

git config core.hooksPath .githooks
echo "core.hooksPath -> .githooks (pre-push gate active)"
