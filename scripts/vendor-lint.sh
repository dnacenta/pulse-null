#!/usr/bin/env bash
# Vendor-name lint: generic code must not name a provider.
#
# The subprocess provider drives several vendors' CLIs through adapters, and
# the entity bootstrap wires each entity for the adapter it chose. Anything
# that names a vendor belongs in that vendor's adapter (src/cli_provider/
# adapters/<vendor>.rs) or in a provider-specific HTTP adapter. This lint
# fails the gate when a vendor name leaks into generic source.
#
# Scope: production lines only — comments (// and ///) and everything after
# the first `#[cfg(test)]` in a file are skipped. A line that legitimately
# names a vendor in generic code (a deprecated alias being folded, a vendor
# subsystem's on-disk path) carries a `vendor-ok:` marker with the reason,
# on the line itself or on the line just above or below it (rustfmt moves
# trailing comments off match arms).
set -euo pipefail
cd "$(dirname "$0")/.."

allowed_files=(
  src/cli_provider/adapters/
  src/anthropic_provider.rs
  src/praxis/          # vigil-pulse harness dirs (~/.claude) — separate subsystem, own issue
  src/vigil/
  src/cli/praxis.rs
  src/cli_provider/tests.rs   # test-only module file (declared under #[cfg(test)] in mod.rs)
)
pattern='claude|grok|codex'
status=0
while IFS= read -r file; do
  skip=0
  for a in "${allowed_files[@]}"; do [[ "$file" == "$a"* ]] && skip=1; done
  [[ $skip -eq 1 ]] && continue
  awk -v f="$file" -v pat="$pattern" '
    NR == FNR { if ($0 ~ /vendor-ok:/) { ok[FNR-1]=1; ok[FNR]=1; ok[FNR+1]=1 } next }
    /#\[cfg\(test\)\]/ { intest=1 }
    intest { next }
    /^[[:space:]]*\/\// { next }
    tolower($0) ~ pat && !(FNR in ok) { printf "%s:%d: %s\n", f, FNR, $0; bad=1 }
    END { exit bad ? 1 : 0 }
  ' "$file" "$file" || status=1
done < <(git ls-files 'src/**/*.rs' 'src/*.rs')
if [[ $status -ne 0 ]]; then
  echo "!! vendor names in generic code (move into an adapter, or mark the line 'vendor-ok: <reason>')" >&2
fi
exit $status
