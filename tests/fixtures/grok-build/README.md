# Grok Build CLI fixtures (PN-91)

Captured from Grok Build **1.0.0 (3cd0d0cbce)**, linux-x86_64, 2026-08-11 (help
surfaces, unauth) and 2026-08-24 (authenticated probes). Findings doc:
`pulse-vault/pulse-null/grok-build-cli-findings.md`.

## Probes (authenticated, SuperGrok subscription)

- `probe-claude-shape.*` — pulse-null's exact `invoke_args` argv against raw
  grok: exit 2, `unexpected argument '--system-prompt-file'`.
- `probe-claude-shape-b.*` — same minus the file flag: exit 2 on
  `--no-session-persistence`.
- `probe-minimal.json` — working headless shape. Response field is `text` (not
  `result`); `usage` is snake_case and matches what `parse_response` reads;
  extra `reasoning_tokens`, cache fields, `total_cost_usd`, `modelUsage`.
- `probe-bad-model.*` — exit 1, stdout `{"type":"error","message":...}`.

## Key adapter facts

- Accepted claude-compat flags: `--model`, `--output-format json`,
  `--dangerously-skip-permissions`, `--disallowedTools`.
- Not accepted: `--system-prompt-file`, `--no-session-persistence`; `-p -`
  treats `-` as the literal prompt (no stdin read); `--prompt-file <path>`
  alone triggers headless mode.
- Auth'd model list: `grok-4.6` (default), `grok-4.5` — no fast tier.
- Sessions always persist (`~/.grok/sessions`); cross-session memory off via
  `--no-memory`; Claude/Cursor/Codex harness compat disabled via
  `[compat.*]` in `~/.grok/config.toml` (grok auto-loads foreign config
  otherwise — see `inspect-root-cwd.txt`).

## grok-claude-shim.sh

Deployed copy of `/usr/local/bin/grok-claude-shim` — the translation layer
that let the unmodified claude-code provider drive Grok for the 2026-08-24
Echo-on-grok live test. Reference input for the Phase 2 native
`grok_build_provider.rs`; the shim's bridged gaps are the adapter's spec.

## 2026-08-24 addendum — final-message extraction

`--output-format json`'s `text` field CONCATENATES every assistant turn, so
pre-tool narration ("I'll check X...") leaked into chat replies. The stream
format's final `result` event (see `probe-streaming-messages.ndjson`) is
already Claude Code wire format — `result` holds ONLY the last assistant
message, with snake_case `usage`, `is_error`, `stop_reason`. Shim v2 forces
`streaming-messages-json` and emits that event verbatim; the adapter should
do the same.
