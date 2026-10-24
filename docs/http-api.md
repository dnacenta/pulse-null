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

`channel` defaults to `chat`. It does not select the stream pool, and a value of `voice` does not select the call session. Both follow the caller's identity.

The voice stream pool is taken only by a voice identity. That is the in-process voice plugin bound to the `voice` channel (any case), or a client that presents `[security] voice_secret` in the `X-Echo-Secret` header. The credential is configured on the server and must differ from `[security] secret`. A voice credential may speak only on the `voice` channel; any other channel is `403` and takes no permit. A voice turn must send a non-blank `sender`. A missing, empty, or whitespace-only sender is `400` and takes no permit, on `POST /chat` and on this stream, even when the voice pool is full. A sender that contains an ASCII control character is `400` for every caller.

A voice identity is a guest call under `[plugin_turns]`. `voice_turns_per_call` bounds that call. `daily_turns` bounds the credential's own UTC day, `http-voice`, separate from any plugin. Past a cap, `POST /chat` and this stream answer `429` with a short sentence and, on the stream, no event frame. A turn counts once it is admitted, even if the provider fails or the client disconnects.

Every other caller uses the chat stream pool, whatever `channel` says. That includes the owner, a peer, and every caller when `[security] secret` is unset (those requests are the owner). A non-voice caller that sends `channel` equal to `voice` is not rejected. The turn runs on the chat pool, in that principal's ordinary session, and does not enter the call set.

Each pool allows 4 streams at once. A further request gets `503` with `too many open streams; try again shortly`. The response is `text/event-stream`, with a comment keep-alive every 15 seconds.

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

event: reset
data: {"reason":"budget"}

event: reset
data: {"reason":"refusal"}

event: reset
data: {"reason":"error"}

event: done
data: {"isolation":false,"model":"m","text":"Hello","tokens_in":3,"tokens_out":null,"truncated":true}

event: error
data: {"message":"upstream model error","status":500}
```

`status` is `thinking` when a round starts, `responding` when text starts, and `tool` (with `name`) while a tool runs. `delta` is best-effort text. A screen stream forwards it as the provider yields it, unvalidated, and `done.text` replaces the transcript. A voice-pool stream releases guarded text clause by clause after a 250 ms first-release hold, and holds the rest of the open segment until that round's guard has kept it. A client that stops reading loses deltas; the turn does not wait. `segment` marks the end of one assistant message. `index` counts from 0 within the turn. `tool` is true when a tool call follows that message. `reset` means text already sent on this turn is void. `reason` is `fallback` when a refusal fallback replaced a draft, `guard` when the reply was cut short before any voice text was released, `budget` when held speech crossed the turn's output budget, `refusal` when speech already left and the turn was then declined, or `error` when speech already left and the turn then failed. `done` is the validated reply and the token counts. `truncated` is true when the reply was cut short. A speech client discards unspoken text when `truncated` is true even if no `reset` arrived. `error` ends a turn that was not saved; `status` is the HTTP status the blocking endpoint would have returned.

Ordering:

- `status thinking` comes before a round's first delta. `status responding` comes before that round's deltas.
- `segment` comes after the deltas of the message it closes. When `tool` is true it comes before `status tool`. A message that arrived with no token deltas is still spoken: its text is a `delta` before a `segment` with `tool` false, unless that same text already arrived as deltas. A speech client that sees no `delta` before `done` speaks `done.text`.
- A provider that reports no message boundary and calls no tool sends no `segment`. That stream is deltas, then `done`.
- `reset` is written before any delta the client has not yet received, including deltas already sitting in the progress channel. `reason` `fallback` comes before the fallback reply's delta. On a screen stream it is sent only when a delta was produced. On a voice-pool stream a held draft is dropped, and `reset` is sent with no raw tail before it.
- On a screen stream, `reset` with `reason` `guard` still leaves the raw tail in the stream, and it comes before `done`. On a voice-pool stream that has not released text, the raw tail is not sent: `reset` comes first, then a delta of the kept text, then `done`. Once a voice-pool stream has released text, `reset` with `reason` `guard` is not sent; only the kept remainder follows what was already said. `done.text` is the cut reply, and `truncated` is true. A speech client discards unspoken text on `reset` and speaks `done.text` after it.
- A provider error after deltas ends the stream with exactly one `error`. The user turn is rolled back. On a voice-pool stream that has already released text, `reset` with `reason` `error` is written before that `error`. Nothing released sends no such `reset`.
- A provider refusal on a voice-credential stream ends with `error`. When nothing was released there is no `delta` of the refused draft and no `reset`. When speech was already released, `reset` with `reason` `refusal` is written before that `error`. On an owner stream the fallback follows `reset` with reason `fallback`.

Closing the connection cancels the turn. Nothing from that turn is saved: the user message is rolled back the same way a failed turn is.

`done.text` replaces whatever a reader showed from deltas. A speech client uses the events as they arrive:

1. Speak `delta` text clause by clause as it arrives.
2. On `segment` with `tool` true: discard any not-yet-spoken text from that segment. Optionally play a short filler while `status tool` lasts.
3. On `reset`: stop speaking, drop queued text, then speak `done.text` minus what was already said (or a short repair phrase). `reason` `budget` means speak `done.text` minus what was already said. `done.truncated` means the same discard even when `reset` is absent.
4. On `error`: speak a short apology; the turn was not saved.
5. Never rely on `done` arriving to start speaking; use it for metadata and the authoritative transcript.

## Security model

- **Trust tiers**: Trusted (internal reflection, system tasks), Verified (authenticated
  channels such as chat, voice, web) and Untrusted (anonymous or unknown). Each gets a
  different security context. Channels per tier: `[trust]`.
- **Prompt-injection detection**: non-trusted messages are scanned with regex patterns
  before reaching the model; hits are blocked and logged.
- **Authentication**: optional `X-Echo-Secret` header. It matches `[security] secret` (the owner) or, when set and different, `[security] voice_secret` (the voice stream pool).
- **Voice guest caps**: a voice identity is a guest call under `[plugin_turns]`. `voice_turns_per_call` bounds the call, and `daily_turns` applies to that credential's own `http-voice` day. Past a cap, `POST /chat` and `POST /api/chat/stream` answer `429` with a short sentence, before any server-sent event. A turn counts once admitted, even if it fails or the client disconnects.
- **Rate limiting**: token bucket on authenticated endpoints.

## Plugins

Built in with Cargo features, from `pulse-system-types` trait objects:

| Plugin | Feature | What it adds |
|---|---|---|
| [voice-echo](https://github.com/pulse-null/voice-echo) | `voice` | Phone calls via Twilio |
| [discord-echo](https://github.com/pulse-null/discord-echo) | `discord-text` | Discord text bot |

`--features all-plugins` builds both.
