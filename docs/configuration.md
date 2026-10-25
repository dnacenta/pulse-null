# Configuration

`pulse-null.toml` lives in the pulse's directory. `pulse-null init` writes a complete one;
everything below has a default. The core:

```toml
[pulse]
name = "atlas"
owner_name = "Ada"
owner_alias = "Ada"
# rules_require_root = false

[server]
host = "127.0.0.1"
port = 3200

[llm]
provider = "cli"
adapter = "claude"
model = "claude-opus-5-5"

[scheduler]
enabled = true
timezone = "UTC"
```

Every key:

| Section | Key | Default | Description |
|---|---|---|---|
| `pulse` | `name` | — | Pulse name |
| `pulse` | `owner_name` | — | Your name |
| `pulse` | `owner_alias` | — | How the pulse addresses you |
| `pulse` | `rules_dir` | — | Shared protocol files directory |
| `pulse` | `rules_require_root` | `false` | Pulse rules load only from root-owned files under directories below the pulse root (through the rules directory) that root owns and that are not group- or world-writable. The pulse root itself is not checked because the pulse owns its home. Strict mode enforces the same on the rule file (0644). Strict mode also rejects a path on a FUSE filesystem. Where the pulse user can still mount filesystems, deny that too (`NoNewPrivileges=yes`, `PrivateDevices=yes` or no `/dev/fuse`, no setuid fusermount for the pulse user): the loader can only check what it can see. `false` still reads a file the pulse user or root owns. The key is in the pulse's `pulse-null.toml`, which a pulse that owns its home can edit. For a real guarantee set `Environment=PULSE_NULL_RULES_REQUIRE_ROOT=1` in the root-owned systemd unit (`1`, `true`, and `yes` force strict mode; `0` and `false` leave the config key; any other value is ignored and warned once). Optionally make `pulse-null.toml` root-owned and immutable (`chattr +i`, an operator option, not verified here). |
| `server` | `host` | `127.0.0.1` | Bind address |
| `server` | `port` | `3100` (init suggests from 3200) | Bind port |
| `llm` | `provider` | set at init (`anthropic` if missing) | `cli`, `anthropic`, `ollama` |
| `llm` | `adapter` | set at init | CLI behind `cli`: `claude`, `grok`, `codex` |
| `llm` | `api_key` | — | API key (or env var; not needed for `cli` / `ollama`) |
| `llm` | `model` | set at init | Passed through to the provider |
| `llm` | `max_tokens` | `4096` | Max response tokens |
| `llm` | `base_url` | `http://localhost:11434` | API base URL (`ollama`) |
| `llm` | `cli_bin` | the adapter's binary | CLI path; `PULSE_CLI_BIN` fills in when unset |
| `llm` | `reasoning_effort` | `low` | Reasoning-effort hint for CLIs that take one |
| `llm` | `context_budget` | `150000` | Estimated-token ceiling before conversation compaction |
| `llm` | `fallback_model` | — | Model retried on a usage-policy refusal (empty disables) |
| `llm` | `fallback_on_refusal` | `true` | Master switch for the refusal fallback |
| `security` | `secret` | — | Enables the `X-Echo-Secret` header check (the owner) |
| `security` | `voice_secret` | — | Separate `X-Echo-Secret` value for the voice stream pool. Must differ from `secret`. Unset means no voice credential |
| `security` | `injection_detection` | `true` | Prompt-injection scanning |
| `trust` | `trusted` | init: `["reflection", "system"]` | Channels with full access |
| `trust` | `verified` | init: `["chat", "voice", "web"]` | Channels with limited access |
| `memory` | `memory_max_lines` | `200` | MEMORY.md line cap |
| `memory` | `archive_max_logs` | `100` | Archive cap |
| `scheduler` | `enabled` | `true` | Run `schedule.json` |
| `scheduler` | `timezone` | `UTC` | Timezone for cron expressions |
| `scheduler.liveness` | `enabled` | `true` | Alert on consecutive or intermittent task failures |
| `pipeline` | `*_soft`, `*_hard` | see [concepts](concepts.md) | Journal document limits |
| `monitoring` | `enabled` | `true` | Reflection signals (`pulse-null vigil`) |
| `monitoring` | `window_size`, `min_samples` | `10`, `5` | Signal window |
| `caliber` | `enabled` | `true` | Record task and intent outcomes |
| `caliber` | `max_outcomes` | `200` | Rolling window of outcomes |
| `autonomy` | `enabled` | `true` | Intent queue (independent of the scheduler) |
| `autonomy` | `max_intents_per_hour` | `10` | Intent rate cap |
| `autonomy` | `max_queue_size` | `20` | Queued intents |
| `autonomy` | `max_chain_depth` | `5` | How deep intents may spawn intents |
| `graph` | `extract` | init: `true` | Background graph extraction per new archive |
| `graph` | `extract_daily_token_budget` | init: `2000000` | Extraction token cap per UTC day (`0` = none) |
| `context_buffer` | `pulse_filter` | `true` | On shared channels, drop other pulses' messages from context |
| `tui` | `theme`, `motion`, `nerd_font` | `gruvbox`, `full`, `auto` | See [tui.md](tui.md) |
| `plugin_turns` | `daily_turns` | `150` | Guest turns per plugin, and HTTP voice turns as their own day, per UTC day; counted in memory, so a restart starts the day again; the owner's turns don't count (`0` = no cap) |
| `plugin_turns` | `voice_turns_per_call` | `20` | Turns one voice call may take, per plugin and for HTTP voice turns as their own (`0` = no cap) |
| `plugin_turns` | `guest_burst`, `guest_per_minute` | `5`, `6` | Per-sender token bucket for text-channel guests, per plugin (`guest_burst = 0` = no bucket). HTTP voice turns are calls on their own ledger, not this bucket |
| `probe` | `enabled` | `false` | Growth-record probe runs ([methods](methods/probe-runner.md)) |
| `probe` | `cadence` | `max` | `max`: due Mon/Wed/Fri · `pro`: every 14 days |
| `probe` | `quiet_start`, `quiet_end` | `02:00`, `06:00` | Window every run must start in (`[scheduler] timezone`) |
| `probe` | `weekly_turns`, `max_turns_per_run` | `81`, `27` | Measurement budget in model turns (three `max` runs of up to 27: every item set and three due paired facts) |

Also present, with defaults: `[owner]`, `[prediction]`, `[tension]`, `[outreach]`,
`[sessions]`, `[session_health]`, `[platform]`, `[system_prompt_budget]`, `[peers]`,
`[plugins]`.

## Environment

| Variable | Description |
|---|---|
| `ANTHROPIC_API_KEY` | Anthropic API key when `[llm] api_key` is unset |
| `PULSE_NULL_API_KEY` | Alternative API key variable |
| `PULSE_CLI_BIN` | CLI binary when `[llm] cli_bin` is unset |
| `PULSE_NULL_ALLOW_ROOT` | `1` lets `init` / `up` run as root (CI) |
| `RUST_LOG` | Log level, e.g. `pulse_null=debug` |

## Older configs

- `[entity]` is read as `[pulse]`; a caliber `[pulse]` table beside it is read as
  `[caliber]`; `entity_filter` as `pulse_filter`.
- `provider = "claude-code"` loads as `cli` + `adapter = "claude"`, `claude_bin` as
  `cli_bin`, and `provider = "claude"` as `anthropic`, each with a one-line deprecation
  warning at load.
