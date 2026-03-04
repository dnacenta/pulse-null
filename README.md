# pulse-null

[![License: AGPL-3.0](https://img.shields.io/github/license/dnacenta/pulse-null)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.80%2B-orange)](https://rustup.rs/)

One binary. One command. Your own AI entity.

`pulse-null` is a Rust binary that creates and runs a persistent AI entity with identity, memory, scheduled cognition, and a plugin system. Your entity has its own documents, personality, growth pipeline, and can be extended with plugins for voice, Discord, n8n workflows, and more.

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
                        ┌──────────────────────────────────────────┐
                        │            pulse-null (axum)            │
                        │                                          │
  Browser ◄──► chat-echo ◄──►│  POST /chat ──► Claude API ──► response │
                        │       │         │                        │
                        │   trust layer   injection detection      │
                        │   auth middleware   rate limiter          │
                        │                                          │
                        │  ┌───────────────────────────────────┐   │
                        │  │  Scheduler (cron)                 │   │
                        │  │  - cognitive cycles               │   │
                        │  │  - research, reflection           │   │
                        │  │  - health checks                  │   │
                        │  └───────────────────────────────────┘   │
                        │                                          │
                        │  ┌───────────────────────────────────┐   │
                        │  │  Pipeline Enforcement             │   │
                        │  │  - document thresholds            │   │
                        │  │  - staleness detection            │   │
                        │  │  - auto-archival                  │   │
                        │  └───────────────────────────────────┘   │
                        │                                          │
                        │  ┌───────────────────────────────────┐   │
                        │  │  Metacognitive Monitoring         │   │
                        │  │  - vocabulary diversity            │   │
                        │  │  - question generation             │   │
                        │  │  - thought lifecycle               │   │
                        │  │  - cognitive health assessment     │   │
                        │  └───────────────────────────────────┘   │
                        │                                          │
                        │  ┌───────────────────────────────────┐   │
                        │  │  Plugin System                    │   │
                        │  │  - voice-echo (voice calls)       │   │
                        │  │  - discord-echo (Discord bot)     │   │
                        │  │  - n8n-integration (workflows)    │   │
                        │  └───────────────────────────────────┘   │
                        └──────────────────────────────────────────┘
```

## What Gets Created

When you run `pulse-null init`, the wizard creates a complete entity directory:

```
my-entity/
├── pulse-null.toml          # Configuration
├── SELF.md                    # Entity identity and values
├── CLAUDE.md                  # System instructions for the LLM
├── schedule.json              # Scheduled cognitive tasks
├── memory/
│   ├── MEMORY.md              # Curated knowledge (always in context)
│   ├── EPHEMERAL.md           # Session summaries
│   └── ARCHIVE.md             # Long-term archive index
├── journal/
│   ├── LEARNING.md            # Research notes
│   ├── THOUGHTS.md            # Incubation space
│   ├── REFLECTIONS.md         # Crystallized observations
│   ├── CURIOSITY.md           # Open questions and themes
│   ├── PRAXIS.md              # Behavioral policies
│   └── LOGBOOK.md             # Session records
├── monitoring/
│   └── signals.json           # Cognitive health signals
├── archives/                  # Overflow from journal documents
├── plugins/                   # Plugin data directories
├── static/                    # Web UI files
└── logs/
```

## Configuration

### pulse-null.toml

| Section        | Key                      | Default              | Description                                    |
|----------------|--------------------------|----------------------|------------------------------------------------|
| `entity`       | `name`                   | --                   | Entity name                                    |
| `entity`       | `owner_name`             | --                   | Your name                                      |
| `entity`       | `owner_alias`            | --                   | How the entity addresses you                   |
| `server`       | `host`                   | `127.0.0.1`          | Bind address                                   |
| `server`       | `port`                   | `3100`               | Bind port                                      |
| `llm`          | `provider`               | `claude`             | LLM provider                                   |
| `llm`          | `api_key`                | --                   | API key (or use `ANTHROPIC_API_KEY` env var)    |
| `llm`          | `model`                  | `claude-sonnet-4-20250514` | Model name                               |
| `llm`          | `max_tokens`             | `4096`               | Max response tokens                            |
| `security`     | `secret`                 | --                   | Auth secret (enables `X-Echo-Secret` header)   |
| `security`     | `injection_detection`    | `true`               | Prompt injection scanning                      |
| `trust`        | `trusted`                | `["reflection", "system"]` | Channels with full access                |
| `trust`        | `verified`               | `["chat", "voice", "web"]` | Channels with limited access              |
| `scheduler`    | `enabled`                | `true`               | Enable scheduled tasks                         |
| `scheduler`    | `timezone`               | `UTC`                | Timezone for cron expressions                  |
| `pipeline`     | `enabled`                | `true`               | Document threshold enforcement                 |
| `monitoring`   | `enabled`                | `true`               | Metacognitive signal tracking                  |

### Environment variables

| Variable             | Description                           |
|----------------------|---------------------------------------|
| `ANTHROPIC_API_KEY`  | Anthropic API key (overrides config)  |
| `PULSE_NULL_API_KEY`| Alternative API key env var           |
| `RUST_LOG`           | Log level (e.g. `pulse_null=debug`) |

## CLI Commands

```
pulse-null init [--dir <path>]     Create a new entity
pulse-null up                      Start the entity server
pulse-null down                    Stop the entity
pulse-null status                  Show entity status

pulse-null schedule list           List scheduled tasks
pulse-null schedule add            Add a scheduled task
pulse-null schedule remove <id>    Remove a scheduled task
pulse-null schedule enable <id>    Enable a task
pulse-null schedule disable <id>   Disable a task

pulse-null pipeline health         Document counts and thresholds
pulse-null pipeline stale          List stale documents

pulse-null archive list            List archived files
pulse-null archive run <doc>       Manually archive a document

pulse-null plugin list             List available plugins
pulse-null plugin add <name>       Install a plugin
pulse-null plugin remove <name>    Remove a plugin
```

## HTTP API

All endpoints (except `/health`) require `X-Echo-Secret` header when `security.secret` is configured. Rate limited to 10 burst / 2 per second.

| Method | Path          | Description              |
|--------|---------------|--------------------------|
| GET    | `/health`     | Health check (no auth)   |
| GET    | `/api/status` | Entity status            |
| POST   | `/chat`       | Send a message           |

### `POST /chat`

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

- **Authentication**: Optional `X-Echo-Secret` header when `security.secret` is set
- **Trust levels**: Three tiers (Trusted, Verified, Untrusted) with per-level security contexts
- **Injection detection**: Scans non-trusted messages for prompt injection patterns
- **Rate limiting**: Token-bucket rate limiter on all endpoints except `/health`

## Plugins

Plugins extend your entity with new capabilities. The plugin system is built but individual plugins are coming soon:

| Plugin            | Description                      | Status       |
|-------------------|----------------------------------|--------------|
| `voice-echo`      | Phone calls via Twilio           | Coming soon  |
| `discord-echo`    | Discord bot interface            | Coming soon  |
| `n8n-integration` | Workflow automation via n8n      | Coming soon  |

## Prerequisites

- [Rust](https://rustup.rs/) 1.80+
- An [Anthropic](https://console.anthropic.com/) API key

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for branch naming, commit conventions, and workflow.

## License

[AGPL-3.0](LICENSE)
