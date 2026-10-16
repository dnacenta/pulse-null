# Pulses

## Layout

`pulse-null init` creates a pulse directory under the one you run it from:

```
~/pulse-null/
└── atlas/
    ├── pulse-null.toml        # configuration
    ├── SELF.md                # identity
    ├── INSTRUCTIONS.md        # instructions for the agent CLI (CLAUDE.md / AGENTS.md point here)
    ├── AWARENESS.md           # runtime facts, regenerated at every boot
    ├── schedule.json          # cron tasks
    ├── .claude/               # recall-echo hooks and rules, for the claude adapter
    ├── memory/                # recall-echo: MEMORY.md, EPHEMERAL.md, archives, graph
    ├── journal/               # LEARNING, THOUGHTS, REFLECTIONS, CURIOSITY, PRAXIS, LOGBOOK
    ├── archives/              # journal overflow
    ├── monitoring/  plugins/  logs/
```

A `pulses/` subdirectory (or the older `entities/`) is honoured if one exists.

## Several pulses, one user

Each pulse is self-contained: its own config, memory, journal and agent-CLI integration.
Nothing is shared through `$HOME/.claude`, so pulses don't collide.

- `pulse-null up` from anywhere opens the terminal UI's Home: every pulse you own, with its
  state; Enter starts or attaches.
- `pulse-null up --headless` inside a pulse runs that pulse only. This is what production
  wants: one systemd unit per pulse, its own `WorkingDirectory`, its own environment file
  for provider credentials, independent restarts.
- `pulse-null up --headless` from the parent directory boots every pulse in one process.
  Each binds the port in its own config; if that port is taken it falls back to the next
  free one from 3200 and logs it.

Every subcommand acts on the pulse whose directory you run it from.

## Brains

| `provider` | Runs | Cost |
|---|---|---|
| `cli` + `adapter = "claude"` | Claude Code | your subscription |
| `cli` + `adapter = "grok"` | Grok Build CLI | your subscription |
| `cli` + `adapter = "codex"` | Codex CLI | your subscription |
| `anthropic` | Anthropic API | per token |
| `ollama` | local model | free |

Each adapter lives in one file under `src/cli_provider/adapters/`; adding a CLI means one trait.

## The agent CLI integration

With `provider = "cli"`, the pulse runs its CLI from inside its own directory with
`RECALL_ECHO_HOME` pointing at it, so the CLI reads that pulse's instruction file, hooks and
rules, and recall-echo writes that pulse's memory.

| `adapter` | CLI | Instruction file | Session capture |
|---|---|---|---|
| `claude` | Claude Code | `CLAUDE.md` (imports `INSTRUCTIONS.md`) | recall-echo hooks in `.claude/settings.json` |
| `grok` | Grok Build | `AGENTS.md` | recall-echo's background sweep |
| `codex` | Codex CLI | `AGENTS.md` | recall-echo's background sweep |

CLIs that don't load `.claude/rules/` (grok, codex) get the pulse's rules inlined into the
prompt. The adapter is also written into `memory/.recall-echo.toml` as recall-echo's
`[llm] provider` and `[capture] sources`, so the CLI that thinks is the one recall-echo
extracts with and captures from.

`pulse-null repair` re-creates any of those files, rewrites recall-echo hooks from older
versions (`--entity-root` → `--pulse-root`; needs recall-echo 4.6.0+), and retires
user-level symlinks left by older installs.

## Limits

Pulses under one unix user share that user's rights: each runs its agent CLI with
permission prompts disabled and can read and write its siblings' directories. They don't
collide, but they aren't isolated. Where isolation matters, give each pulse its own user.

Don't run `init` or `up` as root: files would end up root-owned and agent CLIs refuse to
skip permission prompts under root. Both commands refuse; `PULSE_NULL_ALLOW_ROOT=1`
overrides for CI.
