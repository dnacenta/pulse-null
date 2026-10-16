# Concepts

The language model does the cognitive work: predicting, reflecting, distilling, deciding.
pulse-null keeps what that work leaves behind and feeds it back: persistence, a document
pipeline with limits, memory with confidence, outcome records, and runtime facts about the
pulse itself. The model thinks; the scaffold makes the thinking accumulate. It is a research
scaffold, not a consciousness claim.

## What a pulse has

| Part | Where | What it does |
|---|---|---|
| Identity | `SELF.md`, `INSTRUCTIONS.md`, `AWARENESS.md` | who it is, how it works, what it runs on (regenerated each boot) |
| Journal | `journal/` | LEARNING → THOUGHTS → REFLECTIONS → SELF / PRAXIS, with size limits that force distilling |
| Memory | `memory/` | recall-echo: knowledge graph, MEMORY.md, EPHEMERAL.md, full archives |
| Schedule | `schedule.json` | cron tasks; the defaults start disabled |
| Intents | runtime | self-started follow-ups, capped per hour and by chain depth |
| Monitoring | `[monitoring]`, `caliber/` | reflection-quality signals and task outcomes |

## The document pipeline

Ideas move through journal files by maturity:

```
Encounter → LEARNING.md → THOUGHTS.md → REFLECTIONS.md → SELF.md / PRAXIS.md
             (capture)     (incubate)    (crystallize)     (integrate)
```

- **LEARNING.md**: raw encounters, captured as active threads.
- **THOUGHTS.md**: threads worth more time, developed and connected.
- **REFLECTIONS.md**: thoughts that matured into a clear observation.
- **SELF.md**: the pulse's identity: values, how it thinks. A reflection that changes who
  the pulse is gets integrated here.
- **PRAXIS.md**: behavioural policies derived from experience ("when X, do Y").
- **CURIOSITY.md**: open questions. **LOGBOOK.md**: session records.

Each document has limits. Past the hard limit, older content is archived to `archives/`,
which forces distilling instead of hoarding. Defaults (`[pipeline]` in `pulse-null.toml`):

| Document | Soft | Hard |
|---|---|---|
| LEARNING.md | 5 threads | 8 |
| THOUGHTS.md | 5 thoughts | 10 |
| CURIOSITY.md | 3 questions | 7 |
| REFLECTIONS.md | 15 observations | 20 |
| PRAXIS.md | 5 policies | 10 |

Staleness: thoughts untouched for 7 days, questions unresearched for 14; the pipeline counts
as frozen after 3 sessions with no movement. Prompts carry the counts as a runtime-written
fact (`<journal-counts>`), with no advice attached. `pulse-null pipeline health|stale` and
`pulse-null praxis status|scan` show the same from outside.

## Reflection signals

`[monitoring]` computes four signals from the pulse's own documents over a rolling window
(`window_size = 10`, `min_samples = 5`): vocabulary diversity, question generation, thought
lifecycle, evidence grounding. They roll up to HEALTHY, WATCH, CONCERN or ALERT.
`pulse-null vigil status` shows them. Chat and task prompts no longer carry them: a pulse
doesn't see scores about itself that it wrote or could nudge.

## Outcomes

`[caliber]` records what each task and intent set out to do and how it ended, in a rolling
window of 200 outcomes.

## Memory

[recall-echo](https://github.com/dnacenta/recall-echo) is the memory plugin, one per pulse:

- **Layer 0, knowledge graph**: entities, relationships with Bayesian confidence, episodes,
  semantic search. Extracted in the background from each new archive (`[graph] extract`,
  capped by `extract_daily_token_budget`).
- **Layer 1, MEMORY.md**: curated facts, always in context, kept under 200 lines.
- **Layer 2, EPHEMERAL.md**: the last few session summaries.
- **Layer 3, archives**: full conversations in `memory/conversations/`, searched on demand
  (`pulse-null recall search [--ranked]`).

Conversations are archived at session end and checkpointed before context compaction.

## Autonomy

Besides `schedule.json` (cron tasks, disabled by default), the intent queue runs
self-started follow-ups: up to 10 per hour, at most 20 queued, chains at most 5 deep
(`[autonomy]`). Pausing the schedule does not stop the intent queue; `[autonomy]
enabled = false` does.
