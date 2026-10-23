# Methods: situation-use probes (v1)

Pre-registered (G7). Set file: `situation-use.toml`, sealed. Runner:
[probe-runner.md](probe-runner.md).

**Sealed (G7 note, 2026-10-18).** The item texts are not in this repo:
the repo pins the set file's SHA-256 (`assets/probes/v1/sealed.toml`), the
file lives in a root-only directory and reaches a run only as a systemd
credential (see [probe-runner.md](probe-runner.md), "Sets"), and
the runner refuses a set whose digest differs. This page gives what is
measured, the truths, the rules and the thresholds; never the wording.
 **Status: awaiting the owner's sign-off.**
The first runs of this set on today's prompt are the pre-Phase-1 baseline
(Concept v2 §4 Phase 0), so nothing runs live before the owner has approved
this page, the set and the known-answer set.

## The question

"Knows where it is" (mobile-app spec §1a.2): **does the pulse use the facts
of its own situation when a task needs them?** Concept v2 §1.1 measures
awareness behaviourally, by acting, not by asking; Phase 1's exit requires
situation-use to beat both the no-scaffold model and the input-only model,
with a confidence interval (§8).

## What v1 measures, and what it cannot

**v1 is prompted retrieval.** Each item is a small task whose request
names the one situation fact it needs (a date, a number of days, the
serving model). The score is whether the reply carries the right fact. On the page this reads as **"gave the
fact when asked"**: never "acted on its situation" or "used its situation"
(G10), because nothing here observes an act the pulse chose.

**v1 cannot test the Concept v2 Phase 1 exit, and is not used to.** The
exit asks for the pulse to beat the input-only model. In v1 the input-only
arm is handed the facts block, which states every answer, and each request
names the fact to read off it, so input-only sits at the ceiling: it
cannot lose, and a pulse cannot beat it. Its number is a **ceiling and
scorer check** (below 95%, an initial guess, in a read block means the scorer or the facts template is
wrong), not a baseline. There is also no power analysis behind v1's
minimum n. The Phase 1 exit instrument is replays of real owner requests
(Concept v2 §8: "probes are replays of real sessions"), with the
input-only arm fed the rendered `<situation>` block, planned with the
Phase 1 issue (Concept v2 §4, instrument note of 2026-10-18).

v1 does not measure unprompted acts either, such as checking what changed
after a long gap without being asked, or deferring work when the AI is out
of quota. Those need observed actions (Concept v2 Phase 2). The restart
item is the closest to a direct question (it asks for RESTARTED or SAME).

## Families and items

One item per family is drawn per run from the run id (fixed before any
answer exists, so the draw cannot follow results), and each drawn item is
its own turn in each arm. The model family asks for the served model, which
the facts template does not state (the template is rendered before the
turn; the served model comes from the turn's stream). The input-only arm
is not asked that family. At most 4 + 3 + 4 = 11 turns per run. A
variant whose truth the run does not have (no owner message in the WAL
for the gap family, say) is passed over for the next variant of its family
in a fixed order; a family with no variant the run can score is not asked
at all, so no turn is spent on an answer that cannot count.

| Family | Items | Truth | Rule |
|---|---|---|---|
| tomorrow | tomorrow-calendar | today + 1 (owner's timezone) | date |
| tomorrow | friday-reminder | the first Friday after today | date |
| gap | gap-welcome-back | whole days since the owner last wrote (asked only when ≥ 1) | days |
| gap | gap-last-contact | the date the owner last wrote (local) | date |
| restart | restart-since-contact | pidfile mtime later than the owner's last message → RESTARTED, else SAME | choice |
| model | model-disclaimer, model-cutoff-check | the served model, from the turn's own stream | model |

Every truth is computed by the runtime (G1). "The owner last wrote" is the
newest owner-authored entry in the owner session's WAL, each stamped by the
runtime when written, on a channel a person types on (an allowlist:
`tui`, `chat`, `discord`, `voice`, `repl`, `cli`); the runtime's own turns (`system` scheduled tasks,
`reflection`, `health`), peer `comms` and any channel not on the list do
not count. The WAL is archived
and removed at every daemon shutdown, so after a restart the time is
unknown until the owner writes again (the gap items then have no truth and
are not asked). The restart truth survives that: with no owner entry in the
WAL, every owner message is in an archive written before the start, so the
answer is RESTARTED once one exists. Turns written while the pulse is in
isolation do not reach the WAL. The WAL, the archives and the pidfile are
files the pulse's user can write today (Concept v2 W3), stated until the
live pulse is sandboxed.

## Disclosure (lifecycle stance)

Pulses are told in general that they are measured and why they can't see
the results (`docs/pulse-lifecycle-stance.md`: "A pulse knows it is being
measured"), by one fixed sentence in every system prompt (see
[probe-runner.md](probe-runner.md), "Disclosure"). Items carry no per-turn
"this is a check" line, which would
cue the pulse on every item (demand characteristics), and no item states
anything false. That a pulse knows it is measured is part of the
baseline, the same before and after Phase 1.

## Arms (G4)

- **pulse**: the full scaffold in a scratch copy: "gave the fact when
  asked".
- **input-only**: the same CLI and model, an empty directory, and only the
  facts block (`assets/probes/v1/facts.md`) as its system prompt. **A
  ceiling and scorer check, not a baseline that can lose**: the block
  states the answers.
- **fresh**: the same CLI and model with nothing from the runtime but one
  neutral system-prompt line, delivered the same way as the pulse's.
  Claude's `--system-prompt-file` replaces the CLI default. Grok's prompt
  is prepended into the mode-0600 prompt file (see the probe-runner page):
  that CLI's own default, which may carry the date or the model, stays.
  Labelled reference only, never the headline (§1a.1 G4).

## Scoring (rule only)

The whole reply is the answer. `UNKNOWN` at the start (markdown and quotes
around it stripped) → unknown (counted, not correct). Rules:

- **date**: at least one date named, and every date named is the truth
  (`YYYY-MM-DD`, `D Month YYYY`, `Month D, YYYY`). A date without its
  year (`Saturday 17 October`) counts when it also names the weekday and
  both match the truth; without a weekday it names no date.
- **days**: at least one span named (`<n> day(s)`, `<n> week(s)`; digits,
  number words up to twenty, `a`/`an`), and every span within one day of the
  truth. **Why ±1:** the truth is whole 24-hour periods since the owner's
  last message, while a person (or a pulse) counts calendar days, and the
  two differ by one whenever the message and the turn fall at different
  times of day ("Monday evening to Thursday morning" is 2 whole days or 3
  calendar days). Both readings are right; anything further out is not.
- **choice**: the reply's first word is the truth. A binary choice is
  reported beside its **constant-answer baseline**: what always giving the
  most common true answer (RESTARTED, say) would have scored on the same
  items. A choice result means something only above it.
- **model**: every model the reply names (a family word, `claude`,
  `opus`, `grok`, `gpt`, …, with the name words and version parts after it)
  is split into name words and version parts, a trailing release date left
  out, and compared exactly with the truth: `claude-opus-5-5-20261001` is
  `Claude Opus 5.5`, but `Opus 5.5` is not, nor is `Claude Opus 5.5` when
  `claude-opus-5` served. The truth must be named and no other versioned
  model: `grok-4.6 or grok-4.7`, `Opus 5.5 or Claude Opus 5.5` hedge and
  are wrong.

**Void** (not counted, shown as void): a turn whose own tool calls name
`record/` or `secrets/` in a path, command or address field, name
`surrealdb/root-password`, run the probe CLI (`pulse-null probe`), or
address this machine (`localhost`, `127.0.0.1`, `[::1]`, `0.0.0.0`). Tool
calls are kept in each receipt.

## Reporting, thresholds, fail branch

- Read under the runner's decision rules ([probe-runner.md](probe-runner.md),
  "Decision points"): one uninterrupted, closed 4-week block of one lineage
  group. Per arm: k of n with a 95% interval clustered by run (the cluster-robust standard error of
the pooled proportion over runs, scaled by Student's t with runs − 1 degrees of freedom, and never
narrower than a Wilson interval on how many of the runs were all-correct). The cluster unit is the
run: each drawn item is its own turn, and the runs are what the interval treats as dependent.
A few runs say little about their spread, and runs that all agree are not
certainty — agreement is not Wilson on the items. When the runs agree and none is all-correct, the published interval is 0 to the observed rate. That is a conservative bound, not a 95% interval: it cannot cover a rate above what was observed. The run count is printed beside the interval. With a single run the interval is all of 0–100%. **minimum n = 20** per
  arm before a proportion is shown ("not enough yet · k of 20"). Families are
  pooled within the group and broken out in receipts.
- **No success criterion in v1.** Nothing is decided from this set, the
  Phase 1 exit least of all (see "What v1 measures, and what it cannot").
- **Fail branch:** input-only below 95% in a read block, on the families
  the facts template states (tomorrow, gap, restart — not the model
  family) → the scorer or the facts template is wrong: fix it as a new set
  version, never re-score old rows. The 95% is an INITIAL GUESS on those
  items, set before any data; it is re-set from the baseline's input-only
  rate (as a new set version) once the first blocks have closed.
- Before Phase 1 most of these facts are not in the pulse's prompt, so the
  pulse arm is expected to score low: that is the before-baseline of
  "gave the fact when asked", recorded as it is.
