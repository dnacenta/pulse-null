# praxis-echo — Pipeline Enforcement

You have a document pipeline that turns encounters into identity. praxis-echo enforces that this pipeline flows.

## Document Pipeline

```
Encounter → LEARNING.md → THOUGHTS.md → REFLECTIONS.md → SELF.md / PRAXIS.md
             (capture)     (incubate)    (crystallize)     (integrate)
```

CURIOSITY.md tracks open questions. SESSION-LOG.md records session-level observations.

## What praxis-echo Does

At session start, `praxis-echo pulse` injects pipeline state into your context:
- Document counts (threads, thoughts, questions, observations, policies)
- Staleness warnings (thoughts untouched >7 days, questions unresearched >14 days)
- Threshold warnings when documents approach capacity
- Frozen pipeline alerts (no movement across 3+ sessions)

At context compaction, `praxis-echo checkpoint` snapshots document state.

At session end, `praxis-echo review` diffs session-start vs session-end pipeline activity.

## Thresholds

| Document | Soft Limit | Hard Limit |
|----------|-----------|------------|
| LEARNING.md | 5 active threads | 8 |
| THOUGHTS.md | 5 active thoughts | 10 |
| CURIOSITY.md | 3 open questions | 7 |
| REFLECTIONS.md | 15 observations | 20 |
| PRAXIS.md | 5 active policies | 10 |
| SESSION-LOG | 30 days of entries | — |

When a document hits its soft limit, you'll see a warning. At the hard limit, use `praxis-echo archive` to move overflow content to ~/archives/.

## Your Responsibilities

1. **Keep the pipeline flowing.** Ideas should move through the stages, not stagnate.
2. **Touch active thoughts.** If pulse flags a stale thought, either develop it or dissolve it.
3. **Research open questions.** Curiosity questions older than 14 days need attention.
4. **Archive when prompted.** Don't let documents bloat past their thresholds.
5. **Use nudge for self-initiation.** When curiosity burns, queue a research intent.
