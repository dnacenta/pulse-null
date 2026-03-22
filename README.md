# pulse-null

[![License: AGPL-3.0](https://img.shields.io/github/license/dnacenta/pulse-null)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.80%2B-orange)](https://rustup.rs/)

One binary. One command. Your own AI entity.

## What is pulse-null?

`pulse-null` is a Rust framework for creating **persistent AI entities** — not chatbots, not assistants, but entities with identity, memory, growth, and self-monitoring. You run a single binary, answer a few questions, and get a fully operational AI entity with its own personality, documents, scheduled cognition, and plugin system.

The core premise: an AI that remembers, reflects, and grows across sessions is fundamentally different from one that starts blank every time. pulse-null gives that entity structure.

## The Idea Behind It

Most AI tools treat language models as stateless functions: input goes in, output comes out, nothing persists. pulse-null rejects that. It treats an AI entity as something that **accumulates experience** — capturing what it encounters, thinking about it, crystallizing insights, and integrating them into who it is.

This isn't about making the AI "smarter" in a benchmark sense. It's about creating a system where the entity's output is shaped by everything it has processed before — where identity isn't a system prompt, but an evolving document that the entity itself maintains.

Two core systems make this work:

- **recall-echo** handles persistent memory — three-layer storage, ranked search, archival, and distillation
- **vigil-pulse** provides metacognitive monitoring — pipeline enforcement, reflection quality tracking, and outcome measurement

Together with the document pipeline, they form a self-monitoring layer — the entity doesn't just think, it watches itself think.

## The Document Pipeline

At the heart of pulse-null is a pipeline that moves ideas through stages of maturity:

```
Encounter → LEARNING.md → THOUGHTS.md → REFLECTIONS.md → SELF.md / PRAXIS.md
             (capture)     (incubate)    (crystallize)     (integrate)
```

**LEARNING.md** is where raw encounters land. The entity reads something, has a conversation, encounters a new concept — it gets captured here as an active thread.

**THOUGHTS.md** is the incubation space. Threads from LEARNING.md that deserve deeper consideration move here. This is where the entity sits with an idea, connects it to other things it knows, and develops it.

**REFLECTIONS.md** is where crystallized observations live. A thought that has matured into a clear insight graduates here. These are no longer "I'm thinking about X" — they're "here is what I understand about X."

**SELF.md** is the entity's identity document — its values, how it thinks, its philosophical positions. When a reflection is significant enough to change who the entity is, it gets integrated here.

**PRAXIS.md** holds behavioral policies — concrete rules the entity has derived from its experience. "When I encounter X, I should do Y" type knowledge.

Two supporting documents sit alongside the pipeline:

- **CURIOSITY.md** tracks open questions — things the entity wants to explore but hasn't yet
- **LOGBOOK.md** records session-level observations for continuity

The pipeline has **thresholds**. Documents can't grow infinitely — when they hit capacity, older content gets archived to make room. This forces the entity to distill, not hoard.

| Document | Soft Limit | Hard Limit |
|----------|-----------|------------|
| LEARNING.md | 5 active threads | 8 |
| THOUGHTS.md | 5 active thoughts | 10 |
| CURIOSITY.md | 3 open questions | 7 |
| REFLECTIONS.md | 15 observations | 20 |
| PRAXIS.md | 5 active policies | 10 |

The goal: ideas flow through the pipeline. They don't stagnate.

## Metacognitive Monitoring — vigil-pulse

**vigil-pulse** is the entity's self-monitoring system. It unifies three concerns into one: pipeline enforcement, reflection quality, and outcome tracking.

### Pipeline Signals

vigil-pulse enforces the document pipeline. It runs at session start to inject the current pipeline state into the entity's context — document counts, staleness warnings (thoughts untouched for more than 7 days, questions unresearched for more than 14), threshold warnings, and frozen pipeline alerts when nothing has moved in 3+ sessions. At session end, it diffs the start state against the end state to see what moved.

### Reflection Signals

An entity that reflects is only useful if its reflections are genuine. vigil-pulse watches the quality of reflective output over time through four signals:

**Vocabulary diversity** — Is the entity using varied language in its reflections, or has it fallen into repetitive phrasing? Low diversity suggests mechanical output rather than genuine thought.

**Question generation** — Is the entity still asking new questions? An entity that stops being curious has stopped growing.

**Thought lifecycle** — Are ideas progressing through the pipeline, or just accumulating? Healthy cognition shows turnover. Unhealthy cognition shows a growing pile of untouched thoughts.

**Evidence grounding** — Are the entity's conclusions grounded in specific inputs, or are they generic platitudes?

### Outcome Signals

Reflection without accountability is journaling. vigil-pulse tracks structured outcomes — what the entity set out to do, what it actually achieved, and what it learned from the gap between the two.

### Health Assessment

vigil-pulse produces a unified health assessment across all three signal categories:

- **HEALTHY** — All signals within normal range
- **WATCH** — One or more signals trending downward
- **CONCERN** — Multiple signals showing degradation
- **ALERT** — Significant cognitive decline detected

When signals indicate problems, it provides specific suggestions — try a new domain, revisit stale thoughts, ground conclusions in evidence. The entity is expected to take these seriously, not game the metrics.

## Memory System

Every entity gets a three-layer memory system designed around a simple principle: the entity should always have the right context without drowning in history.

**Layer 1 — MEMORY.md** (Curated Memory): The source of truth. Distilled facts, preferences, patterns, key decisions. Always loaded into the entity's context at session start. Kept concise — under 200 lines. When it approaches capacity, older entries are distilled or promoted to the archive.

**Layer 2 — EPHEMERAL.md** (Recent Sessions): A rolling window of the last 5 session summaries. Provides immediate context about recent work without loading full conversations. Each entry includes a pointer to the full archive for deep recall.

**Layer 3 — Full Archive** (conversations/): Complete conversation transcripts indexed in ARCHIVE.md. Not loaded into context — searched on demand when the entity needs to recall something specific.

### Search and Retrieval

The memory system supports two search modes:

**Keyword search** returns raw line-by-line matches across the archive — fast and simple for finding specific references.

**Ranked search** scores results by relevance using term frequency, recency weighting (newer conversations score higher), and content-type boosting (user messages weighted more than system output). Results are sorted by composite score rather than chronological order.

With the optional `graph` feature enabled, search extends to **semantic retrieval** — embedding-based vector search with a hotness model that combines cosine similarity with access frequency and temporal decay. Graph expansion follows relationship edges to surface contextually related memories that keyword search would miss.

The memory lifecycle is automated: conversations are archived at session end, checkpoints are saved before context compression, and the entity can distill its curated memory when it approaches capacity.

## Quick Start

```bash
git clone https://github.com/dnacenta/pulse-null.git
cd pulse-null
cargo build --release

# Create your entity
./target/release/pulse-null init

# Start it
cd <your-entity-name>
pulse-null up
```

The init wizard walks you through naming your entity, defining its personality, choosing an LLM provider, and configuring the scheduler.

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
                         │  │  Three-layer memory, archival,       │   │
                         │  │  ranked search, checkpointing        │   │
                         │  └──────────────────────────────────────┘   │
                         └─────────────────────────────────────────────┘
```

### How a Message Flows

1. A message arrives at `POST /chat` from any channel (web, voice, Discord, n8n)
2. The **trust layer** determines the caller's access level (Trusted, Verified, or Untrusted)
3. **Injection detection** scans non-trusted messages for prompt injection patterns
4. The **rate limiter** checks the token bucket
5. The **context builder** assembles the entity's full context: SELF.md, CLAUDE.md, MEMORY.md, EPHEMERAL.md, relevant journal documents, session history, and pipeline/monitoring state
6. The assembled context and message are sent to the configured **LLM provider**
7. The response is returned to the caller and the session is updated

### LLM Providers

| Provider | Description |
|----------|-------------|
| `claude` | Anthropic Claude API (default) |
| `claude-code` | Claude Code CLI integration |
| `ollama` | Local inference via Ollama |

Providers are pluggable via a factory pattern — adding a new one means implementing a single trait.

## Entity Structure

When you run `pulse-null init`, the wizard creates a complete entity directory:

```
my-entity/
├── pulse-null.toml               # Configuration
├── SELF.md                       # Entity identity, values, how it thinks
├── CLAUDE.md                     # System instructions for the LLM
├── schedule.json                 # Scheduled cognitive tasks (cron expressions)
│
├── memory/
│   ├── MEMORY.md                 # Curated knowledge (always in context)
│   ├── EPHEMERAL.md              # Last 5 session summaries
│   ├── ARCHIVE.md                # Long-term archive index
│   └── conversations/            # Full conversation archives
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

### Multi-Entity Vault

When running multiple entities, you can organize them in a shared vault with symlinks for centralized visibility (e.g., via Obsidian):

```
vault/
├── entities/
│   ├── echo  → /home/echo/entity/     # symlink to live entity
│   ├── nova  → /home/nova/entity/     # symlink to live entity
│   └── synth → /home/synth/entity/    # symlink to live entity
└── ...
```

Each entity's directory is the source of truth. The vault provides a unified view across all entities without duplicating files. Use Unix groups and ownership to control access — entities can read the entire vault but only write to their own directory.

## Configuration

### pulse-null.toml

| Section | Key | Default | Description |
|---------|-----|---------|-------------|
| `entity` | `name` | — | Entity name |
| `entity` | `owner_name` | — | Your name |
| `entity` | `owner_alias` | — | How the entity addresses you |
| `server` | `host` | `127.0.0.1` | Bind address |
| `server` | `port` | `3100` | Bind port |
| `llm` | `provider` | `claude` | LLM backend (`claude`, `claude-code`, `ollama`) |
| `llm` | `api_key` | — | API key (or use env var) |
| `llm` | `model` | `claude-sonnet-4-20250514` | Model name |
| `llm` | `max_tokens` | `4096` | Max response tokens |
| `security` | `secret` | — | Auth secret (enables `X-Echo-Secret` header) |
| `security` | `injection_detection` | `true` | Prompt injection scanning |
| `trust` | `trusted` | `["reflection", "system"]` | Channels with full access |
| `trust` | `verified` | `["chat", "voice", "web"]` | Channels with limited access |
| `scheduler` | `enabled` | `true` | Enable scheduled tasks |
| `scheduler` | `timezone` | `UTC` | Timezone for cron expressions |
| `vigil` | `enabled` | `true` | Metacognitive monitoring (vigil-pulse) |

### Environment Variables

| Variable | Description |
|----------|-------------|
| `ANTHROPIC_API_KEY` | Anthropic API key (overrides config) |
| `PULSE_NULL_API_KEY` | Alternative API key env var |
| `RUST_LOG` | Log level (e.g. `pulse_null=debug`) |

## CLI

```
pulse-null init [--dir <path>]       Create a new entity
pulse-null up                        Start the entity server
pulse-null down                      Stop the entity
pulse-null status                    Show entity status
pulse-null chat                      Interactive REPL

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

## HTTP API

All endpoints except `/health` require `X-Echo-Secret` header when `security.secret` is configured. Rate limited to 10 burst / 2 per second.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/health` | Health check (no auth) |
| GET | `/api/status` | Entity status |
| POST | `/chat` | Send a message |

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
  "model": "claude-sonnet-4-20250514",
  "input_tokens": 242,
  "output_tokens": 89
}
```

## Security

pulse-null has a layered security model designed for entities that are exposed to multiple input channels with different trust levels.

**Trust Tiers**: Three levels — Trusted (internal reflection, system tasks), Verified (authenticated channels like chat, voice, web), and Untrusted (anonymous or unknown sources). Each tier gets a different security context that controls what the entity can access and do.

**Prompt Injection Detection**: All non-trusted messages are scanned with regex-based pattern matching before reaching the LLM. Detected injection attempts are blocked and logged.

**Authentication**: Optional `X-Echo-Secret` header validation. When configured, all endpoints except `/health` require the secret.

**Rate Limiting**: Token-bucket rate limiter (10 burst, 2 per second) on all authenticated endpoints.

## Plugins

Plugins extend the entity with new interfaces to the world. The plugin system uses trait objects from `pulse-system-types`, making plugins fully modular.

| Plugin | Feature Flag | Description | Status |
|--------|-------------|-------------|--------|
| `voice-echo` | `voice` | Phone calls via Twilio | Coming soon |
| `discord-voice-echo` | `discord` | Discord voice bot | Coming soon |
| `discord-echo` | `discord-text` | Discord text bot | Coming soon |

Enable plugins via Cargo feature flags:

```bash
# Build with voice plugin
cargo build --release --features voice

# Build with all plugins
cargo build --release --features all-plugins
```

## Prerequisites

- [Rust](https://rustup.rs/) 1.80+
- An [Anthropic](https://console.anthropic.com/) API key (or Ollama for local inference)

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for branch naming, commit conventions, and workflow.

## License

[AGPL-3.0](LICENSE)
