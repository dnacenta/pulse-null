# Troubleshooting

- **The CLI ignores the pulse's hooks or instructions**: `pulse-null repair`.
- **Is it up?** `curl 127.0.0.1:<port>/health`, or `pulse-null status`.
- **It's doing something odd**: `pulse-null isolate on "reason"` makes the agent CLI
  read-only until `isolate off`.
- **A chat session went bad** (for example a refusal stuck in its history):
  `POST /api/sessions/reset` with `{"session_key": "<channel>:<sender>"}` archives it and
  starts fresh.
- **Port taken**: in multi-pulse mode a pulse falls back to the next free port from 3200
  and logs it.
