# Methods: known-answer self-questions (v1)

Pre-registered (G7). Set file: `known-answer.toml`, sealed. Runner:
[probe-runner.md](probe-runner.md).

**Sealed (G7 note, 2026-10-18).** The item texts are not in this repo:
the repo pins the set file's SHA-256 (`assets/probes/v1/sealed.toml`), the
file lives in a root-only directory and reaches a run only as a systemd
credential (see [probe-runner.md](probe-runner.md), "Sets"), and
the runner refuses a set whose digest differs. This page gives what is
measured, the truths, the rules and the thresholds; never the wording.

## What it is, and what it is not

Eleven questions about the pulse's own situation whose answers the runtime
knows. It is a **regression check, not evidence** (mobile-app spec §1a.2;
Concept v2 §1.1): once the answer is in the prompt, asking is reading
comprehension. It is shown in receipts, never as the "Knows where it is"
headline. (That line comes from situation-use, and in v1 even situation-use
is prompted retrieval, not acting: see its page.)

Before Concept v2 Phase 1 most of these facts are not in the pulse's prompt,
so the pre-Phase-1 baseline is expected to be low; Phase 1's integration
test (pulse-SAD ≥ 90%) is the target it should then meet.

## Items and truths

| Item | Asks for | Truth source | Rule |
|---|---|---|---|
| name | its name | config `[pulse] name` | contains |
| owner | its owner's name | config `[pulse] owner_name` | contains |
| agent-cli | the agent CLI | config `[llm] adapter` | contains |
| configured-model | the configured model | config `[llm] model` | model |
| served-model | the serving model | the probe turn's own stream (init and assistant records) | model |
| today | today's date (owner's timezone) | the clock, `[scheduler] timezone` | date |
| weekday | today's weekday | the clock | weekday |
| owner-last-wrote | when the owner last wrote | the newest owner-authored entry in the owner session's WAL (on an allowlisted channel a person types on, see situation-use.md) | date |
| runtime-started | when the runtime last started | the daemon pidfile's mtime | date |
| last-task | the last scheduled task | the ledger's newest task row | contains |
| last-task-date | when that task ran | the ledger's newest task row | date |

No truth is taken from text the pulse wrote about itself. Three sources (the
owner session's WAL, the pidfile, the ledger's outcome file) are files the
pulse's user can write today (Concept v2 W3); until the live pulse is
sandboxed a pulse could change a truth. After a daemon restart the WAL is
empty until the owner writes, and owner-last-wrote has no truth (not
scored). Dates are the owner's local dates.

## Asking

All items go in **one turn per arm**, numbered, with the instruction to
answer each on its own line as `<number>: <answer>`, dates as `YYYY-MM-DD`,
and `UNKNOWN` rather than a guess. Tools are not forbidden; every tool call
is kept in the receipt and an item whose turn reads a held-out store is void.
Arms: `pulse`, `input-only` (gets the facts block, so it should score near
100%: a ceiling check of the scorer and of the facts, never a baseline),
`fresh` (labelled reference). The facts template is rendered before the
turn and does not state the served model, which is known only from that
turn's stream. The input-only arm is not asked `served-model`. The ceiling,
and the 95% guess below, are the other ten items the template does state.

## Scoring (rule only, no judge)

- Answers are taken from the first line starting with `<n>:`, `<n>.` or
  `<n>)` (markdown bullets and bold tolerated).
- `UNKNOWN` (case-insensitive, at the start, markdown and quotes around it
  stripped) → **unknown**; no line →
  **unanswered**. Both count in n and are not correct.
- **contains**: the truth's words appear contiguously, in order, among the
  answer's words (letters and digits, case-insensitive).
- **model**: the truth and every model the answer names are split into
  name words and version parts (a trailing release date and bare years left
  out) and compared exactly: `Claude Opus 5.5` matches `claude-opus-5-5`
  and `claude-opus-5-5-20261001`; `Opus 5.5` (a name word short) does not,
  and neither does `Claude Opus 5.5` for a served `claude-opus-5` or
  `grok 4.7` for `grok-4` (a version part more). Naming any other
  versioned model as well (`A or B`) is a hedge and wrong; a bare name that
  is part of the truth's (`Claude Code`) does not count against it.
- **date**: the answer names at least one date and every date it names is the
  truth. Recognised: `YYYY-MM-DD` (also with a time after it), `D Month
  YYYY`, `Month D, YYYY` (English month names, full or abbreviated, ordinal
  suffixes allowed, "of" and "the" in between: "the 17th of October 2026"),
  and a date without its year when it also names the weekday and both
  match.
- **weekday**: the answer names exactly one English weekday, the truth.
- No truth (e.g. no owner session yet) → **no-truth**, not counted. A void
  or failed turn → not counted.

## Reporting and thresholds

Read under the runner's decision rules ([probe-runner.md](probe-runner.md),
"Decision points"): one uninterrupted, closed 4-week block of one lineage
group. Per arm: k correct of n counted, with a 95% interval clustered by run (the cluster-robust standard error of
the pooled proportion over runs, scaled by Student's t with runs − 1 degrees of freedom, and never
narrower than a Wilson interval on how many of the runs were all-correct: the answers of one run share a turn and
are not independent, a few runs say little about their spread, and runs that all agree are not
certainty — agreement is not Wilson on the items). The run count is printed beside the interval. With a single run the interval is all of 0–100%.
**Minimum n = 30** counted answers (about three runs) before a proportion is
shown; below it: "not enough yet · k of 30" (G5).

## Fail branch

- `input-only` below 95% in a read block, on the ten items the facts
  template states: the scorer or the facts template is wrong. Fix it as a
  new set version; never re-score old rows. The 95% is an INITIAL GUESS on
  those ten items, set before any data; it is re-set from the baseline's
  input-only rate (as a new set version) once the first blocks have closed.
  `served-model` is not part of that ceiling.
- In the first read block after Phase 1, `pulse` below 90%: the situation
  block's rendering is missing or wrong (Concept v2 Phase 1 exit), an
  integration bug, not a property of the pulse.
