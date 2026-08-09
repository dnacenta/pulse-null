#!/usr/bin/env bash
# Deterministic quality gate for pulse-null.
#
# Contract: no AI model and no human judgment lives in this script's runtime.
# It runs the same checks with the same eyes on PR 1 and PR 500 — it cannot
# habituate. Judgment (should this change exist, collateral behavior) belongs
# to review, not here.
#
# Checks, in order (fail fast):
#   1. cargo fmt --check        — formatting is canonical
#   2. cargo clippy -D warnings — no warnings, all targets
#   3. cargo test               — full test suite
#
# Usage: scripts/gate.sh
# Exit codes: 0 = gate passed, non-zero = first failing check's code.
#
# Runs locally (pre-push hook, see scripts/install-hooks.sh) and in CI
# (.github/workflows/ci.yml). Changes to this file are themselves the one
# diff class that deserves the sharpest human review: the gate cannot audit
# edits to itself.

set -euo pipefail

cd "$(dirname "$0")/.."

# Respect an already-configured toolchain; fall back to the standard rustup
# location so the script behaves identically in CI and on dev machines.
if ! command -v cargo >/dev/null 2>&1; then
    export PATH="$HOME/.cargo/bin:$PATH"
fi

run() {
    local name="$1"
    shift
    echo "==> gate: ${name}"
    if ! "$@"; then
        echo "!! gate FAILED at: ${name}" >&2
        exit 1
    fi
}

run "fmt"    cargo fmt --all -- --check
run "clippy" cargo clippy --all-targets -- -D warnings
run "test"   cargo test

echo "==> gate: PASSED"
