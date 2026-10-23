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
| POST | `/api/chat/stream` | Same turn, as server-sent events. Closing the connection cancels the turn. Contract below |
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

## POST /api/chat/stream

The same turn as `POST /chat`, delivered as server-sent events. The request body is the same:

```json
{ "message": "Hello, how are you?", "channel": "tui" }
```

`channel` defaults to `chat`. A channel equal to `voice`, in any case, draws from the voice stream pool. Every other channel draws from the chat stream pool. Each pool allows 4 streams at once; a further request gets `503` with `too many open streams; try again shortly`. The response is `text/event-stream`, with a comment keep-alive every 15 seconds.

Each event is one SSE frame: the `event` line names it, and the `data` line is the JSON below, with no extra whitespace and with object keys in alphabetical order. A stream ends with exactly one of `done` or `error`.

```sse
event: status
data: {"status":"thinking"}

event: status
data: {"status":"responding"}

event: delta
data: {"text":"Hello"}

event: segment
data: {"index":0,"tool":true}

event: status
data: {"name":"file_read","status":"tool"}

event: segment
data: {"index":1,"tool":false}

event: reset
data: {"reason":"fallback"}

event: reset
data: {"reason":"guard"}

event: done
data: {"isolation":false,"model":"m","text":"Hello","tokens_in":3,"tokens_out":null,"truncated":true}

event: error
data: {"message":"upstream model error","status":500}
```

`status` is `thinking` when a round starts, `responding` when text starts, and `tool` (with `name`) while a tool runs. `delta` is best-effort text as the provider yields it, unvalidated. A client that stops reading loses deltas; the turn does not wait. `segment` marks the end of one assistant message. `index` counts from 0 within the turn. `tool` is true when a tool call follows that message. `reset` means text already sent on this turn is void. `reason` is `fallback` when a refusal fallback replaced a draft, or `guard` when the reply was cut short. `done` is the validated reply and the token counts. `truncated` is true when the reply was cut short. `error` ends a turn that was not saved; `status` is the HTTP status the blocking endpoint would have returned.

Ordering:

- `status thinking` comes before a round's first delta. `status responding` comes before that round's deltas.
- `segment` comes after the deltas of the message it closes. When `tool` is true it comes before `status tool`.
- A provider that reports no message boundary and calls no tool sends no `segment`. That stream is deltas, then `done`.
- `reset` with `reason` `fallback` is sent only when a delta already went out, and it comes before the fallback reply's delta.
- `reset` with `reason` `guard` comes after the raw deltas and before `done`. `done.text` is the cut reply, and `truncated` is true.
- A provider error after deltas ends the stream with exactly one `error`. The user turn is rolled back.

Closing the connection cancels the turn. Nothing from that turn is saved: the user message is rolled back the same way a failed turn is.

`done.text` replaces whatever a reader showed from deltas. A speech client uses the events as they arrive:

1. Speak `delta` text clause by clause as it arrives.
2. On `segment` with `tool` true: discard any not-yet-spoken text from that segment. Optionally play a short filler while `status tool` lasts.
3. On `reset`: stop speaking, drop queued text, then speak `done.text` minus what was already said (or a short repair phrase).
4. On `error`: speak a short apology; the turn was not saved.
5. Never rely on `done` arriving to start speaking; use it for metadata and the authoritative transcript.

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
