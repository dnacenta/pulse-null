#!/usr/bin/env bash
# Receipt smoke check. No root, no network, no systemd.
#
# Dev path: a receipt file in a temp directory, not a systemd fd. The rows
# are the pinned genesis, boot, open, and close from the receipt vectors
# (one owner turn). This script does not start a daemon or a provider.
# It then anchors that file to a local backend and expects verify to exit 1
# after one byte changes.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
if [[ -n "${PULSE_NULL:-}" ]]; then
  bin=$PULSE_NULL
elif [[ -x "$root/target/debug/pulse-null" ]]; then
  bin=$root/target/debug/pulse-null
elif command -v pulse-null >/dev/null 2>&1; then
  bin=$(command -v pulse-null)
else
  echo "pulse-null binary not found; set PULSE_NULL or build target/debug/pulse-null" >&2
  exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/echo"
receipt=$work/echo/receipts.jsonl

# hash, then the canonical JSON. The torn line and its recover row stay out.
awk 'NR>=2 && NR<=5 { sub(/^[^ ]+ /, ""); print }' \
  "$root/tests/fixtures/receipts/v1-vectors.expected" >"$receipt"

"$bin" receipts verify --file "$receipt"

config=$work/anchor.toml
printf '%s\n' '[[backend]]' 'kind = "local"' >"$config"
chmod 0644 "$config"
"$bin" receipts anchor --pulse echo --file "$receipt" --config "$config"
grep -q '^v1 echo ' "$work/echo/anchors.log"

python3 - "$receipt" <<'PY'
import pathlib, sys
path = pathlib.Path(sys.argv[1])
data = bytearray(path.read_bytes())
data[10] ^= 1
path.write_bytes(data)
PY

set +e
"$bin" receipts verify --file "$receipt"
code=$?
set -e
if [[ $code -ne 1 ]]; then
  echo "verify after a one-byte change exited $code, expected 1" >&2
  exit 1
fi
