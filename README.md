# pulse-null

[![License: AGPL-3.0](https://img.shields.io/github/license/dnacenta/pulse-null)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.80%2B-orange)](https://rustup.rs/)
[![Website](https://img.shields.io/badge/website-dnacenta.github.io%2Fpulse--null-b48ead)](https://dnacenta.github.io/pulse-null/)

One binary. One command. Your own AI pulse.

## What is pulse-null?

`pulse-null` is a **cognitive architecture scaffold** for long-running AI pulses, written in Rust. A **pulse** is one such agent: a long-lived process with its own identity, memory, and schedule. You run a single binary, answer a few questions, and get a persistent pulse with its own identity documents, memory graph, scheduled cognition, and self-monitoring — one that accumulates experience across sessions instead of starting blank every time.

The honest framing: the language model performs the cognitive operations — predicting, reflecting, distilling, deciding. pulse-null provides everything that makes those operations *accumulate into something*: persistence, feedback routing, prediction-error tracking, Bayesian memory, and measurement. The model thinks; the architecture makes the thinking compound.

This is not a consciousness claim. It is a research scaffold for studying what a language model becomes when its predictions are tracked against outcomes, its memory is structured and confidence-weighted, and its self-observations feed back into its behavior.

## The Idea Behind It

Most AI tools treat language models as stateless functions: input goes in, output comes out, nothing persists. pulse-null treats a pulse as a process that **accumulates experience** — capturing what it encounters, thinking about it, crystallizing insights, and integrating them into an identity document the pulse itself maintains.

Three mechanisms make the accumulation real rather than decorative:

- **A prediction-error loop** — the pulse makes typed predictions about its own trajectory, which persist, resurface in later context, get resolved against what actually happened, and accumulate surprise that gates when deeper reflection runs. Inspired by predictive processing; explicitly *not* an implementation of active inference (the code says so too).
- **recall-echo** handles persistent memory — four-layer storage (knowledge graph, curated facts, recent sessions, full archives), semantic and ranked search, Beta-Binomial edge confidence with temporal decay, archival, and distillation
- **vigil-pulse** provides behavioral metacognition — pipeline enforcement, reflection quality signals computed from the running pulse's own output, and outcome tracking

Together with the document pipeline, they form a self-monitoring layer — the pulse doesn't just think, it watches itself think, and the watching has consequences.

## The Document Pipeline

At the heart of pulse-null is a pipeline that moves ideas through stages of maturity:

```
Encounter → LEARNING.md → THOUGHTS.md → REFLECTIONS.md → SELF.md / PRAXIS.md
             (capture)     (incubate)    (crystallize)     (integrate)
```

**LEARNING.md** is where raw encounters land. The pulse reads something, has a conversation, encounters a new concept — it gets captured here as an active thread.

**THOUGHTS.md** is the incubation space. Threads from LEARNING.md that deserve deeper consideration move here. This is where the pulse sits with an idea, connects it to other things it knows, and develops it.

**REFLECTIONS.md** is where crystallized observations live. A thought that has matured into a clear insight graduates here. These are no longer "I'm thinking about X" — they're "here is what I understand about X."

**SELF.md** is the pulse's identity document — its values, how it thinks, its philosophical positions. When a reflection is significant enough to change who the pulse is, it gets integrated here.

**PRAXIS.md** holds behavioral policies — concrete rules the pulse has derived from its experience. "When I encounter X, I should do Y" type knowledge.

Two supporting documents sit alongside the pipeline:

- **CURIOSITY.md** tracks open questions — things the pulse wants to explore but hasn't yet
- **LOGBOOK.md** records session-level observations for continuity

The pipeline has **thresholds**. Documents can't grow infinitely — when they hit capacity, older content gets archived to make room. This forces the pulse to distill, not hoard.

| Document | Soft Limit | Hard Limit |
|----------|-----------|------------|
| LEARNING.md | 5 active threads | 8 |
| THOUGHTS.md | 5 active thoughts | 10 |
| CURIOSITY.md | 3 open questions | 7 |
| REFLECTIONS.md | 15 observations | 20 |
| PRAXIS.md | 5 active policies | 10 |

The goal: ideas flow through the pipeline. They don't stagnate.

## Metacognitive Monitoring — vigil-pulse

**vigil-pulse** is the running pulse's self-monitoring system. It unifies three concerns into one: pipeline enforcement, reflection quality, and outcome tracking.

### Pipeline Signals

vigil-pulse enforces the document pipeline. It runs at session start to inject the current pipeline state into the running pulse's context — document counts, staleness warnings (thoughts untouched for more than 7 days, questions unresearched for more than 14), threshold warnings, and frozen pipeline alerts when nothing has moved in 3+ sessions. At session end, it diffs the start state against the end state to see what moved.

### Reflection Signals

A pulse that reflects is only useful if its reflections are genuine. vigil-pulse watches the quality of reflective output over time through four signals:

**Vocabulary diversity** — Is the pulse using varied language in its reflections, or has it fallen into repetitive phrasing? Low diversity suggests mechanical output rather than genuine thought.

**Question generation** — Is the pulse still asking new questions? A pulse that stops being curious has stopped growing.

**Thought lifecycle** — Are ideas progressing through the pipeline, or just accumulating? Healthy cognition shows turnover. Unhealthy cognition shows a growing pile of untouched thoughts.

**Evidence grounding** — Are the pulse's conclusions grounded in specific inputs, or are they generic platitudes?

### Outcome Signals

Reflection without accountability is journaling. vigil-pulse tracks structured outcomes — what the pulse set out to do, what it actually achieved, and what it learned from the gap between the two.

### Health Assessment

vigil-pulse produces a unified health assessment across all three signal categories:

- **HEALTHY** — All signals within normal range
- **WATCH** — One or more signals trending downward
- **CONCERN** — Multiple signals showing degradation
- **ALERT** — Significant cognitive decline detected

When signals indicate problems, it provides specific suggestions — try a new domain, revisit stale thoughts, ground conclusions in evidence. The pulse is expected to take these seriously, not game the metrics.

## Memory System

Every pulse gets a four-layer memory system designed around a simple principle: the pulse should always have the right context without drowning in history.

**Layer 0 — Knowledge Graph**: An embedded SurrealDB graph database with FastEmbed local embeddings. Stores entities (people, projects, tools, concepts), relationships with Bayesian confidence scoring, and conversation episodes. Semantic search finds memories by meaning, not just keywords. Re-extracted relationships gain confidence through Bayesian corroboration over time.

**Layer 1 — MEMORY.md** (Curated Memory): The source of truth. Distilled facts, preferences, patterns, key decisions. Always loaded into the pulse's context at session start. Kept concise — under 200 lines. When it approaches capacity, older entries are distilled or promoted to the archive.

**Layer 2 — EPHEMERAL.md** (Recent Sessions): A rolling window of the last 5 session summaries. Provides immediate context about recent work without loading full conversations. Each entry includes a pointer to the full archive for deep recall.

**Layer 3 — Full Archive** (conversations/): Complete conversation transcripts indexed in ARCHIVE.md. Not loaded into context — searched on demand when the pulse needs to recall something specific.

### Search and Retrieval

The memory system supports two search modes:

**Keyword search** returns raw line-by-line matches across the archive — fast and simple for finding specific references.

**Ranked search** scores results by relevance using term frequency, recency weighting (newer conversations score higher), and content-type boosting (user messages weighted more than system output). Results are sorted by composite score rather than chronological order.

With the optional `graph` feature enabled, search extends to **semantic retrieval** — embedding-based vector search with a hotness model that combines cosine similarity with access frequency and temporal decay. Graph expansion follows relationship edges to surface contextually related memories that keyword search would miss.

The memory lifecycle is automated: conversations are archived at session end, checkpoints are saved before context compression, and the pulse can distill its curated memory when it approaches capacity.

## Quick Start

```bash
git clone https://github.com/dnacenta/pulse-null.git
cd pulse-null
cargo build --release

# Create your pulse
./target/release/pulse-null init

# Start it
cd <your-pulse-name>
pulse-null up
```

The init wizard walks you through naming your pulse, defining its personality, choosing an LLM provider, and configuring the scheduler.

## Architecture

```
                         ┌─────────────────────────────────────────────┐
                         │             pulse-null (axum)               │
                         │                                             │
     Plugins ◄──────────►│  POST /chat ──► LLM Provider ──► Response  │
     (voice, discord,    │       │              │                      │
      n8n, web)          │  trust layer    context builder             │
                         │  auth middleware    (identity docs,         │
                         │  rate limiter        memory, journal)       │
                         │  injection detection                        │
                         │                                             │
                         │  ┌──────────────────────────────────────┐   │
                         │  │  Scheduler (cron)                    │   │
                         │  │  Cognitive cycles, research,         │   │
                         │  │  reflection, health checks,          │   │
                         │  │  intent queue (self-initiated tasks)  │   │
                         │  └──────────────────────────────────────┘   │
                         │                                             │
                         │  ┌──────────────────────────────────────┐   │
                         │  │  vigil-pulse (Metacognitive Monitor)  │   │
                         │  │  Pipeline enforcement, reflection    │   │
                         │  │  quality, outcome tracking            │   │
                         │  └──────────────────────────────────────┘   │
                         │                                             │
                         │  ┌──────────────────────────────────────┐   │
                         │  │  recall-echo (Memory System)         │   │
                         │  │  Four-layer memory, knowledge graph, │   │
                         │  │  Bayesian confidence, semantic search │   │
                         │  └──────────────────────────────────────┘   │
                         └─────────────────────────────────────────────┘
```

### How a Message Flows

1. A message arrives at `POST /chat` from any channel (web, voice, Discord, n8n)
2. The **trust layer** determines the caller's access level (Trusted, Verified, or Untrusted)
3. **Injection detection** scans non-trusted messages for prompt injection patterns
4. The **rate limiter** checks the token bucket
5. The **context builder** assembles the pulse's full context: SELF.md, CLAUDE.md, MEMORY.md, EPHEMERAL.md, relevant journal documents, session history, and pipeline/monitoring state
6. The assembled context and message are sent to the configured **LLM provider**
7. The response is returned to the caller and the session is updated

### LLM Providers

pulse-null is not tied to any vendor, and there is no baked-in default: **you choose the pulse's brain in the init wizard**.

| Provider | Description |
|----------|-------------|
| `cli` | Drives an installed agent CLI as a subprocess through an adapter — no API key, uses the CLI's own login |
| `anthropic` | Anthropic HTTP API — per-token billing, needs an API key |
| `ollama` | Local inference via Ollama — fully offline |

The `cli` provider is generic. Everything a particular CLI does differently — its flags, how the prompt and system prompt reach it, its output format, what a policy refusal looks like, which files it reads from the pulse directory — lives in that CLI's adapter under `src/cli_provider/adapters/`. Three ship today:

| `adapter` | CLI | Instruction file | Hooks |
|-----------|-----|------------------|-------|
| `claude` | Claude Code | `CLAUDE.md` (imports `INSTRUCTIONS.md`) | recall-echo hooks in `.claude/settings.json` |
| `grok` | Grok Build | `AGENTS.md` | none — recall-echo's session sweep captures grok sessions |
| `codex` | Codex CLI | `AGENTS.md` | none — same, via the sweep |

The adapter you pick is written into the pulse's `memory/.recall-echo.toml` as recall-echo's `[llm] provider` and `[capture] sources`, so the CLI that does the pulse's thinking is the one recall-echo extracts with and captures from. Adding a CLI means implementing one trait; nothing outside its adapter file may name it — `scripts/gate.sh` lints for that.

Pre-PN-106 configs keep working: `provider = "claude-code"` loads as `cli` + `adapter = "claude"`, `claude_bin` as `cli_bin`, and `provider = "claude"` as `anthropic`, each with a one-line deprecation warning at load.

## Pulse Structure

When you run `pulse-null init`, the wizard creates a complete pulse directory:

```
my-pulse/
├── pulse-null.toml               # Configuration
├── SELF.md                       # Pulse identity, values, how it thinks
├── CLAUDE.md                     # System instructions for the LLM
├── schedule.json                 # Scheduled cognitive tasks (cron expressions)
│
├── memory/
│   ├── MEMORY.md                 # Curated knowledge (always in context)
│   ├── EPHEMERAL.md              # Last 5 session summaries
│   ├── ARCHIVE.md                # Long-term archive index
│   ├── conversations/            # Full conversation archives
│   └── graph/                    # Knowledge graph (SurrealDB + embeddings)
│
├── journal/
│   ├── LEARNING.md               # Active research threads (capture)
│   ├── THOUGHTS.md               # Ideas being developed (incubate)
│   ├── REFLECTIONS.md            # Crystallized observations (crystallize)
│   ├── CURIOSITY.md              # Open questions and recurring themes
│   ├── PRAXIS.md                 # Behavioral policies (integrate)
│   └── LOGBOOK.md                # Session records
│
├── caliber/                      # Outcome tracking (vigil-pulse)
│   └── outcomes.json
├── monitoring/
│   └── signals.json              # Cognitive health metrics (vigil-pulse)
│
├── archives/                     # Overflow storage when documents hit thresholds
├── plugins/                      # Plugin-specific data directories
├── static/                       # Web UI assets
└── logs/                         # Service logs
```

### Multiple Pulses

One unix user can create and run any number of pulses. Each pulse is a directory holding its own `pulse-null.toml`, memory, journal and Claude Code integration — nothing is shared through `$HOME/.claude`, so pulses never collide. The recommended layout is flat, under an install root:

```
~/pulse-null/
├── echo/                        # one pulse
│   ├── pulse-null.toml
│   ├── CLAUDE.md  SELF.md  AWARENESS.md
│   ├── .claude/
│   │   ├── settings.json        # recall-echo hooks, carrying this pulse's root
│   │   └── rules/recall-echo.md # memory protocol, pulse-relative paths
│   └── memory/  journal/  archives/  …
└── synth/                       # another pulse, same shape, its own port
```

Create pulses from the install root and run each one from its own directory:

```bash
cd ~/pulse-null
pulse-null init                  # creates ~/pulse-null/<name>/

cd ~/pulse-null/echo
pulse-null up --headless         # single-pulse mode: this pulse only
```

Running each pulse from its own directory is what production wants: one systemd unit per pulse, each with its own `WorkingDirectory`, its own environment file for provider credentials, and independent restarts. Every `pulse-null` subcommand is scoped to the pulse whose directory you run it from.

`pulse-null up` from anywhere opens the terminal UI's Home page, which lists every pulse you own with its state and starts one only when you pick it. `pulse-null up --headless` from the install root boots every pulse in one process; each binds the host and port from its own `pulse-null.toml`, and if that port is already taken it falls back to the next free port from 3200 upward and says so in the log. Pulses kept in a `pulses/` subdirectory (or the older `entities/`) are recognized too.

When the provider is `cli`, the pulse runs its agent CLI from inside its own directory with `RECALL_ECHO_HOME` pointing at it, so the CLI picks up the pulse's instruction file, hooks and rules, and recall-echo reads and writes that pulse's memory. `pulse-null repair` re-creates any of those files for the pulse's adapter, rewrites recall-echo hooks written by older versions (`--entity-root` becomes `--pulse-root`; recall-echo 4.6.0 or later is required), and retires leftover user-level symlinks from older installs.

Pulses under one unix user share that user's rights: each runs its agent CLI with permission prompts disabled and can read and write its siblings' directories. They do not collide, but they are not isolated from each other. Where isolation matters, give each pulse its own unix user.

**Do not run `init` or `up` as root.** Files would end up root-owned, and agent CLIs refuse to skip permission prompts under root, so a `cli` pulse could never reach its provider. Both commands refuse and explain; `PULSE_NULL_ALLOW_ROOT=1` overrides for CI.

## Configuration

### pulse-null.toml

| Section | Key | Default | Description |
|---------|-----|---------|-------------|
| `pulse` | `name` | — | Pulse name |
| `pulse` | `owner_name` | — | Your name |
| `pulse` | `owner_alias` | — | How the pulse addresses you |
| `server` | `host` | `127.0.0.1` | Bind address |
| `server` | `port` | `3100` | Bind port |
| `llm` | `provider` | set at init | LLM backend (`cli`, `anthropic`, `ollama`) — chosen in the wizard; a config missing the key falls back to `anthropic` |
| `llm` | `adapter` | set at init | Which agent CLI drives the `cli` provider (`claude`, `grok`, `codex`) |
| `llm` | `api_key` | — | API key (or use env var; not needed for `cli`/`ollama`) |
| `llm` | `model` | set at init | Model name, passed through to the provider — the wizard suggests a per-provider default |
| `llm` | `max_tokens` | `4096` | Max response tokens |
| `llm` | `base_url` | `http://localhost:11434` | API base URL (used by `ollama`) |
| `llm` | `cli_bin` | the adapter's binary | Path to the agent CLI binary for the `cli` provider; when unset, `PULSE_CLI_BIN` fills in before the adapter's default |
| `llm` | `reasoning_effort` | `low` | Reasoning-effort hint for CLIs that take one |
| `llm` | `context_budget` | `150000` | Estimated-token ceiling before conversation compaction |
| `llm` | `fallback_model` | — | Model retried on a usage-policy refusal (empty disables) |
| `llm` | `fallback_on_refusal` | `true` | Master switch for the refusal fallback |
| `security` | `secret` | — | Auth secret (enables `X-Echo-Secret` header) |
| `security` | `injection_detection` | `true` | Prompt injection scanning |
| `trust` | `trusted` | `["reflection", "system"]` | Channels with full access |
| `trust` | `verified` | `["chat", "voice", "web"]` | Channels with limited access |
| `scheduler` | `enabled` | `true` | Enable scheduled tasks |
| `scheduler` | `timezone` | `UTC` | Timezone for cron expressions |
| `vigil` | `enabled` | `true` | Metacognitive monitoring (vigil-pulse) |
| `caliber` | `enabled` | `true` | Record task and intent outcomes (caliber-echo) |
| `caliber` | `max_outcomes` | `200` | Rolling window of recorded outcomes |
| `context_buffer` | `pulse_filter` | `true` | On shared channels, drop other pulses' messages from the injected context |

Configs written before the rename still load: `[entity]` is read as `[pulse]` (a caliber `[pulse]` table beside it is read as `[caliber]`), and `entity_filter` as `pulse_filter`.

### Environment Variables

| Variable | Description |
|----------|-------------|
| `ANTHROPIC_API_KEY` | Anthropic API key (overrides config) |
| `PULSE_NULL_API_KEY` | Alternative API key env var |
| `RUST_LOG` | Log level (e.g. `pulse_null=debug`) |

## CLI

```
pulse-null init [--dir <path>]       Create a new pulse
pulse-null up                        Terminal UI: Home lists your pulses, pick one to start or attach
pulse-null up --headless             Daemon only (systemd, servers)
pulse-null down                      Stop the pulse
pulse-null status                    Show pulse status
pulse-null chat                      Terminal UI, straight into this pulse's conversation (Home elsewhere)

pulse-null schedule list             List scheduled tasks
pulse-null schedule add              Add a scheduled task
pulse-null schedule remove <id>      Remove a scheduled task
pulse-null schedule enable <id>      Enable a task
pulse-null schedule disable <id>     Disable a task

pulse-null pipeline health           Document counts and thresholds
pulse-null pipeline stale            List stale documents

pulse-null archive list              List archived files
pulse-null archive run <doc>         Manually archive a document

pulse-null plugin list               List available plugins
pulse-null plugin add <name>         Install a plugin
pulse-null plugin remove <name>      Remove a plugin

pulse-null intent <subcommand>       Manage the self-initiated intent queue
pulse-null recall <subcommand>       Memory system tools
pulse-null vigil                     Full metacognitive health check
pulse-null vigil pipeline            Document flow signals
pulse-null vigil reflection          Cognitive quality signals
pulse-null vigil outcomes            Effectiveness signals
```

## Terminal UI

`pulse-null up` (without `--headless`) and `pulse-null chat` open the terminal UI. It is a client of the running daemon: if one is up it attaches over HTTP, otherwise it starts one in-process and stops it cleanly on exit. It never owns a provider or writes session files itself.

**Home** is the first screen under the logo: one *Talk to <pulse>* row per pulse you own (found in `~/pulse-null/<name>`, the directory you are in, and the legacy `~/entity`), each showing `up`, `stopped` or `unreachable`, probed every two seconds; then *Create a new pulse*, which runs the setup wizard and lists the result; then *Exit*. `Enter` on a stopped pulse starts its daemon in this process and opens Talk; on a running one it attaches. `:home` goes back to the menu without stopping anything; Exit stops the daemons Home started. `pulse-null chat` inside a pulse directory skips Home.

The window model follows Hyprland: panes with a one-cell gap, one accent border on the focused pane, `Ctrl+h/j/k/l` to move focus, `f` for fullscreen. A one-line bar shows the pulse, model, page, cognitive status (it says *no signal yet* until there is data), alert count and clock.

**Talk** is the conversation page. Your message appears the instant you press Enter; the reply streams token by token as the provider produces it; typing works during a reply and Enter queues one message; `Ctrl+c` cancels a reply (the daemon rolls the turn back); scrolling up during a reply holds your place and shows `↓ new`, `G` glides back to the tail. Further pages (Watch, Remember, Setup) arrive in later releases; `:` lists them.

`:` opens the command line (`Tab` completes): `:home`, `:theme <name|system>`, `:motion <full|reduced|off>`, `:quit`, `:help`. `?` shows the keys for the focused pane.

```toml
[tui]
theme = "gruvbox"     # gruvbox (default), tokyo-night, catppuccin, everforest, rose-pine, nord, or "system"
motion = "full"       # full | reduced | off — drops to reduced by itself on a slow link
nerd_font = "auto"    # auto | on | off
```

The look is Gruvbox dark by default. With `theme = "system"` on an [Omarchy](https://omarchy.org) desktop the palette is read from `~/.config/omarchy/current/theme/colors.toml` and crossfades when you switch themes. Logs go to `logs/tui.log` in the pulse directory while the UI is up.

## HTTP API

All endpoints except `/health` require `X-Echo-Secret` header when `security.secret` is configured. Rate limited to 10 burst / 2 per second.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/health` | Health check (no auth) |
| GET | `/api/status` | Pulse status |
| GET | `/api/dashboard` | Pipeline and cognitive health |
| POST | `/chat` | Send a message, get the whole reply |
| POST | `/api/chat/stream` | Same turn as `/chat`, streamed as server-sent events: `status`, `delta`, then `done` or `error`. Closing the connection cancels the turn. |
| GET | `/api/session/{channel}` | The conversation on a channel as the TUI shows it |
| GET | `/api/events` | Live ledger of what the pulse does (SSE, `Last-Event-ID` replay) |
| GET | `/api/ledger` | Ledger backfill from disk (`since`, `kind`, `limit`) |
| GET | `/api/schedule` | Scheduled tasks with cadence, last run, next fire |
| POST | `/api/schedule/{id}/enable`, `…/disable` | Toggle a task (same path as the CLI) |
| GET | `/api/schedule/{id}/last` | Stored output of a task's last run |
| GET | `/api/alerts/peek`, POST `/api/alerts/drain` | Task alerts |

### POST /chat

```json
{
  "message": "Hello, how are you?",
  "channel": "chat",
  "sender": "user"
}
```

Response:

```json
{
  "response": "I'm doing well, thanks for asking.",
  "model": "claude-opus-5",
  "input_tokens": 242,
  "output_tokens": 89
}
```

## Security

pulse-null has a layered security model designed for pulses that are exposed to multiple input channels with different trust levels.

**Trust Tiers**: Three levels — Trusted (internal reflection, system tasks), Verified (authenticated channels like chat, voice, web), and Untrusted (anonymous or unknown sources). Each tier gets a different security context that controls what the pulse can access and do.

**Prompt Injection Detection**: All non-trusted messages are scanned with regex-based pattern matching before reaching the LLM. Detected injection attempts are blocked and logged.

**Authentication**: Optional `X-Echo-Secret` header validation. When configured, all endpoints except `/health` require the secret.

**Rate Limiting**: Token-bucket rate limiter (10 burst, 2 per second) on all authenticated endpoints.

## Plugins

Plugins extend the pulse with new interfaces to the world. The plugin system uses trait objects from `pulse-system-types`, making plugins fully modular.

| Plugin | Feature Flag | Description | Status |
|--------|-------------|-------------|--------|
| `voice-echo` | `voice` | Phone calls via Twilio | Available |
| `discord-echo` | `discord-text` | Discord text bot | Available — running in production |

Discord *voice* is handled by a separate sidecar binary (`discord-voice-echo`), not a feature flag.

Enable plugins via Cargo feature flags:

```bash
# Build with voice plugin
cargo build --release --features voice

# Build with all plugins
cargo build --release --features all-plugins
```

## Prerequisites

- [Rust](https://rustup.rs/) 1.80+
- A brain: an [Anthropic](https://console.anthropic.com/) API key, an installed agent CLI (Grok, Claude, ChatGPT — see [LLM Providers](#llm-providers)), or Ollama for local inference

> **Note on toolchain:** CI lints with the **latest stable** Rust toolchain, so new clippy
> lints land as Rust releases. Before pushing, run `rustup update stable` and CI's exact
> gate, `scripts/gate.sh` (`cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
> `cargo test` — dead code is a hard error). A clippy pass on an older local toolchain
> does not guarantee a green CI.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for branch naming, commit conventions, and workflow.

## License

[AGPL-3.0](LICENSE)
