# HTTP API

The daemon listens on `[server] host:port` (localhost by default). Every endpoint except
`/health` requires the `X-Echo-Secret` header when `[security] secret` is set. Rate limit:
burst of 10, then 2 per second.

| Method | Path | Description |
|---|---|---|
| GET | `/health` | Health check (no auth) |
| GET | `/api/session-health` | Session health |
| GET | `/api/status` | Pulse status |
| GET | `/api/dashboard` | Pipeline and cognitive health |
| POST | `/chat` | Send a message, get the whole reply |
| POST | `/api/chat/stream` | Same turn, as server-sent events: `status`, `delta`, then `done` or `error`. Closing the connection cancels the turn |
| GET | `/api/session/{channel}` | A channel's conversation as the TUI shows it |
| POST | `/api/sessions/reset` | Archive a session with a handoff and start fresh: `{"session_key": "<channel>:<sender>"}` |
| POST | `/api/comms` | Start a dialogue with a peer (`{"peer": {"name", "host"?, "port"?}, "topic"?, "max_turns"?}`): 202 with its id, 409 while one runs. Owner only; one per daemon |
| GET | `/api/comms` | The current dialogue's status (`turn`, `max_turns`, `phase`), or 404 |
| GET | `/api/comms/{id}/stream` | Turns so far, then live, as SSE: `turn`, `status`, then `done` or `error` |
| POST / DELETE | `/api/comms/{id}/pause`, `…/resume`, `/api/comms/{id}` | Pause, resume, stop (turns so far are archived) |
| GET | `/api/events` | Live ledger of what the pulse does (SSE, `Last-Event-ID` replay) |
| GET | `/api/ledger` | Ledger backfill from disk (`since`, `kind`, `limit`) |
| GET | `/api/schedule` | Scheduled tasks with cadence, last run, next fire |
| POST | `/api/schedule/{id}/enable`, `…/disable` | Toggle a task (same path as the CLI) |
| GET | `/api/schedule/{id}/last` | Stored output of a task's last run |
| GET / POST | `/api/alerts/peek`, `/api/alerts/drain` | Task alerts |

## POST /chat

```json
{ "message": "Hello, how are you?", "channel": "chat", "sender": "user" }
```

```json
{ "response": "I'm doing well, thanks for asking.", "model": "claude-opus-5-5", "input_tokens": 242, "output_tokens": 89 }
```

## Security model

- **Trust tiers**: Trusted (internal reflection, system tasks), Verified (authenticated
  channels such as chat, voice, web) and Untrusted (anonymous or unknown). Each gets a
  different security context. Channels per tier: `[trust]`.
- **Prompt-injection detection**: non-trusted messages are scanned with regex patterns
  before reaching the model; hits are blocked and logged.
- **Authentication**: optional `X-Echo-Secret` header, from `[security] secret`.
- **Rate limiting**: token bucket on authenticated endpoints.

## Plugins

Built in with Cargo features, from `pulse-system-types` trait objects:

| Plugin | Feature | What it adds |
|---|---|---|
| [voice-echo](https://github.com/pulse-null/voice-echo) | `voice` | Phone calls via Twilio |
| [discord-echo](https://github.com/pulse-null/discord-echo) | `discord-text` | Discord text bot |

`--features all-plugins` builds both.
