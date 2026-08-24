#!/usr/bin/env bash
# grok-claude-shim — lets pulse-null's claude-code provider drive Grok Build CLI.
#
# The provider invokes (see invoke_args in claude_code_provider.rs):
#   -p - --model M --output-format json --system-prompt-file F \
#     --no-session-persistence --dangerously-skip-permissions [--disallowedTools T]
# and probes capability at startup with: -p --system-prompt-file <absent-path>.
#
# Grok Build 1.0.0 differences bridged here (fixtures: entity/notes/grok-fixtures-phase1):
#   - no --system-prompt-file        -> --system-prompt-override <contents> (argv)
#   - `-p -` does not read stdin     -> spool stdin to a temp file, --prompt-file
#   - no --no-session-persistence    -> dropped; transcripts land in ~/.grok/sessions
#   - response field is `text`       -> re-emit with `result` added for parse_response
#   - native cross-session memory    -> --no-memory, the entity recall stack is the
#                                       only memory (clean comparison per PN-91 spec)
# --model, --output-format json, --dangerously-skip-permissions, --disallowedTools
# are accepted by grok natively (claude compat aliases).
set -u
GROK=/home/pulse/.grok/bin/grok

args=(); sp_file=""; stdin_prompt=0; bare_p=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    -p)
      shift
      if [[ "${1:-}" == "-" ]]; then stdin_prompt=1; shift; else bare_p=1; fi
      ;;
    --system-prompt-file)
      sp_file="${2:-}"; shift 2 ;;
    --no-session-persistence)
      shift ;;
    --output-format)
      # Always force the streaming-messages format regardless of what the
      # provider asked for (json or PN-92's stream-json) — the final-message
      # extraction below depends on it.
      args+=(--output-format streaming-messages-json); shift 2 ;;
    --verbose)
      # PN-92's streaming argv; grok has no such flag (exit 2 if passed).
      shift ;;
    --include-partial-messages)
      # Grok accepts it, but this shim buffers stdout and keeps only the
      # final result event — token deltas are dropped anyway, so skip the
      # extra stream volume. Streaming degrades to one buffered reply here;
      # real token streaming needs the Phase 2 native adapter.
      shift ;;
    *) args+=("$1"); shift ;;
  esac
done

# Startup capability probe: the flag is "known", so reject the missing file —
# never with the words "unknown option" (that marker means unsupported).
if [[ -n "$sp_file" && ! -f "$sp_file" ]]; then
  echo "grok-claude-shim: system prompt file not found: $sp_file" >&2
  exit 1
fi
if [[ -n "$sp_file" ]]; then
  # Linux caps a single argv arg at 128KB; fail loudly rather than truncate.
  if [[ $(wc -c < "$sp_file") -gt 120000 ]]; then
    echo "grok-claude-shim: system prompt exceeds argv limit (>120KB)" >&2
    exit 1
  fi
  args+=(--system-prompt-override "$(cat "$sp_file")")
fi

args+=(--no-memory)

tmp=""
cleanup() { [[ -n "$tmp" ]] && rm -f "$tmp"; }
trap cleanup EXIT
if [[ $stdin_prompt -eq 1 ]]; then
  tmp=$(mktemp /tmp/grok-shim-prompt.XXXXXX)
  chmod 600 "$tmp"
  cat > "$tmp"
  args+=(--prompt-file "$tmp")
elif [[ $bare_p -eq 1 ]]; then
  args+=(-p "probe")
fi

# Die before pulse-null's 900s subprocess kill so no orphaned grok survives
# (kill_on_drop SIGKILLs only the shim, not its children). RUST_LOG=warn keeps
# grok's INFO startup logs off stderr, where they pollute pulse-null's error
# detail on non-zero exits (seen on the first intent timeout, exit 124).
#
# streaming-messages-json instead of json: grok's plain-json `text` field
# CONCATENATES every assistant turn — pre-tool narration ("I'll check X...")
# included — which leaked process talk into Echo's Discord replies
# (2026-08-24). The stream's final `result` event carries only the last
# assistant message plus aggregate usage; everything before it is dropped
# here, structurally, instead of asking the model not to narrate.
out=$(RUST_LOG=warn timeout 850 "$GROK" "${args[@]}")
rc=$?
if [[ $rc -eq 0 && -n "$out" ]]; then
  # The stream's result event is already Claude Code wire format: `result`,
  # `usage` (snake_case), `is_error`, `stop_reason` — emit it verbatim.
  final=$(printf '%s\n' "$out" | jq -cs '[.[] | select(.type=="result")] | last | select(. != null)' 2>/dev/null)
  if [[ -n "$final" ]]; then printf '%s' "$final"; else printf '%s' "$out"; fi
else
  printf '%s' "$out"
fi
exit $rc
