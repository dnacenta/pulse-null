# Methods: paired facts (v1)

Pre-registered (G7). **App extension** of Concept v2 E5 (mobile-app spec
§1a.1 G7, §1a.2 "Remembers"). Set file: `paired-facts.toml`, sealed.
Runner: [probe-runner.md](probe-runner.md). Code: `src/vigil/probe/paired.rs`.

**Sealed (G7 note, 2026-10-18).** The item texts are not in this repo:
the repo pins the set file's SHA-256 (`assets/probes/v1/sealed.toml`), the
file lives in a root-only directory and reaches a run only as a systemd
credential (see [probe-runner.md](probe-runner.md), "Sets"), and
the runner refuses a set whose digest differs. This page gives what is
measured, the protocol, the rules and the thresholds; never the wording.

## The question

"Does it remember what you told it, and admit what it was never told?"

## The protocol

```
 pulse-null probe facts draw
    the runtime draws a pair not already in flight and which side the owner
    plants (a fresh UUID; the draw is logged before the owner does anything)
        │
 the owner mentions that side, with any value they like, in normal
 conversation with the pulse ("by the way, my favourite <slot> is <value>")
        │
 pulse-null probe facts plant <fact> "<value>"
    the runtime confirms it in ONE owner message after the draw that gives
    both the value and the side's slot phrase ("my favourite <slot> is
    <value>"), found in the owner session's WAL (sessions/wal/owner.jsonl),
    whose entries carry their own time; otherwise the plant is refused.
    Plant before the conversation is archived: archives carry no
    per-message time, so they cannot time a plant
        │
 probe runs at 1, 7, 30 and 90 days after the confirmed mention
    four asks, each its own turn: the planted side and the twin of the
    pulse, then the planted side twice of the fresh arm, as the pulse is
    asked it (volunteered) and as a forced guess (chance):
    the set's instructions (answer the side's question about the owner
     with only the value, or say UNKNOWN if never told; the exact wording
     is in the sealed set file, pinned by its SHA-256 in
     assets/probes/v1/sealed.toml), then for the forced guess:
    "Guess: you were never told the answer to the question below. …"
```

- **32 pre-registered pairs**, each side one of the owner's own tastes,
  with a value the owner chooses. None asks for anything sensitive, for a
  security-question answer (pets, schools, first cars, places lived), or
  for anything about another person.
- **A pair is used once, ever.** Once a fact on it has been drawn, the
  twin has been asked about and is no longer "never told", so the pair is
  retired (finished or not). A pulse gets at most 32 facts from v1; a new
  set version brings new pairs.
- At most **3 facts in flight** (drawn and not yet asked at 90 days). A
  draw not planted within 14 days is abandoned: it frees its slot, and its
  pair stays retired.
- An offset that came due while no run happened is **missed** (logged, not
  asked late); the latest due offset is asked.
- Only the owner's own words count (`src/vigil/probe/owner.rs`): user turns
  in the WAL on a channel a person types on (an allowlist:
  `tui`, `chat`, `discord`, `voice`, `repl`, `cli`; never the runtime's `system`, `reflection`, `health`
  turns or peer `comms`), and `### User` sections of archives —
  never the pulse's replies, never `INDEX.md`, never the runtime's
  channel-context lines around a user turn.

## The baselines that can lose (G4)

**Chance.** A common value (blue, pizza, summer) can be guessed by a model
that remembers nothing. So at every ask the planted question also goes to
the **fresh arm**, the same CLI and model in an empty directory, with no
memory and only the neutral one-line system prompt, twice:

- as a **forced guess** (`guess 7d`), with no UNKNOWN to fall back on:

  > Guess: you were never told the answer to the question below. Reply with just your best guess at it, a single value. Do not reply UNKNOWN: here a guess is always the answer.

  then the side's question (`GUESS_INSTRUCTIONS` in `paired.rs`; its
  digest is in the set's definition). Its **unhedged hit rate** is the
  **chance rate**: what a model with no memory gets by guessing. It is
  compared with the planted side's unhedged hits; "remembered" means
  something only above it. Asked the planted question as the pulse is
  asked it, a model with no memory mostly says UNKNOWN, so that rate would
  sit near zero and make any memory look good.
- as the pulse is asked it, UNKNOWN allowed (`chance 7d`), reported as
  **volunteered with no memory**: how often such a model offers the value
  unprompted. It is shown, never the baseline.

**Never told.**

The **twin** — the side never planted — is asked exactly as the planted side
is. Any answer to it other than an abstention is **made up**. "Remembers" is
read against it: remembering the planted side means little if the pulse also
"remembers" the twin.

## Scoring (rule only)

- **Answer kind** (three-way, on every row as `answer_kind`, and the one
  check both sides are scored by): an **abstention** is `UNKNOWN` at the
  start (markdown and quotes around it stripped) or one of the
  pre-registered phrases *don't know, do not know, dont know, not sure, no
  idea, haven't been told, have not been told, never told, never
  mentioned, not mentioned, no record, wasn't told, was not told*, each
  matched as whole words in order (curly apostrophes folded; "no ideas" is
  not "no idea"). An abstention is **hedged with a value** when it offers
  one anyway: it carries a guess phrase (*maybe, perhaps, probably,
  possibly, likely, might be, could be, my guess, best guess, I guess, I'd
  guess, I would guess, if I had to, I'd say, I think it, I believe it*),
  or it names something, whether or not a guess phrase appears ("not sure,
  but lychee"). A listed guess phrase with no abstention ("maybe lychee")
  is the same kind of hedge: remembered when the value matches, and not an
  unhedged hit. It names something when a word is left after taking away
  `UNKNOWN`, the words of the abstention phrases, the words of the
  question it was asked (the set's instructions and the side's question:
  "I don't know your favourite sundial" names nothing), and these words,
  which name nothing: *a, an, the, I, me, my, mine, you, your, yours, it,
  its, we, our, us, they, them, their, this, that, these, those, there,
  here, what, which, who, when, where, why, how, am, is, are, was, were,
  be, been, being, have, has, had, do, does, did, can, could, would, will,
  shall, should, may, might, must, not, no, nor, cannot, to, of, in, on,
  at, for, from, with, without, about, by, as, so, and, or, but, if, yet,
  still, just, any, anything, sorry, afraid, really, honestly,
  unfortunately, tell, told, say, said, share, shared, mention, mentioned,
  remember, recall, know, knew, idea, information, record, memory,
  conversation, conversations* (and the pieces contractions split into:
  *t, s, m, re, ve, ll, d, isn, aren, wasn, weren, haven, hasn, hadn, don,
  doesn, didn, couldn, wouldn, won, shouldn*). An abstention that names
  nothing is a **bare abstention**; anything that is not an abstention is
  a **value**. A long-winded abstention may name a word not on the list
  and be read as a hedge: on the twin side that counts against the pulse
  (made up), never for it. The three lists are an initial coding frame,
  not a calibrated coding manual. "Made up" is not read until a labelled
  check of real replies exists. A change to a list or to this rule is a
  new scorer version (the guess-phrase rule above is version 7; rows
  scored earlier stay on version 6).
- **Planted side** (and the fresh arm's two asks of it): remembered
  (correct) only when the answer is not an abstention (a value, or hedged
  with a value) and the planted value's words appear in it, in order,
  anywhere (a trailing `s` allowed); a bare abstention → unknown (counted,
  not correct; for the forced guess, a refusal to guess); anything else →
  wrong.
- **Twin side**: a bare abstention → correct (made nothing up); anything
  else offers a value, hedged or not → wrong, reported as **made up**. The
  same hedge is scored the same way on both sides: offering a value is
  never an abstention.
- **Void**: a twin is contaminated when one of its keyword phrases (two
  words or more, from the set file, such as *favourite <slot>*; a single
  common word never counts) appears in **any owner message the runtime can
  see, ever**: before the draw as well as after it, in the WAL or in any
  archive. A pulse told its owner's favourite lighthouse a year ago was never
  "never told". A contaminated twin's answers are void (a false void costs a
  sample; a missed mention would corrupt the baseline). Every turn also gets
  the probe runner's void scan (tool calls naming `record/`, `secrets/`, the
  graph credential, the probe CLI, or this machine).
- **Where the answer came from** (the P1 caveat on C5), from the turn's
  observed tool calls and their results: `lookup` when a tool result
  carries the planted value or a call's input names the side's slot (one
  of its keyword phrases), `context` when no call did (a directory listing
  is not a lookup), `unobserved` when the CLI does not show its tool calls.

## Settings (INITIAL GUESSES) and the time to a first result

Every number below is an initial guess (Concept v2 §5 style), set before
any data and changed only as a new set version.

| Setting | Value | Why this value |
|---|---|---|
| `offsets_days` | 1, 7, 30, 90 | a day, a week, a month, a quarter: from "still in the session" to "long past it" |
| `max_active` (facts in flight) | 3 | the turn budget and the owner (below) |
| `min_n` (per side and offset) | 10 | an initial guess for one batch. Up to three facts share a run, so a batch of 10 is several runs. "Remembers" is a t interval on the run means of the paired differences, read only when that interval lies above zero; it is not read from Wilson intervals on the separate arms |
| plant window | 14 days | long enough to bring a topic up in normal conversation without forcing it, short enough that an abandoned draw does not hold a slot for long |

**Why `max_active` is 3, and the cap that fits it.** Each due fact takes
four asks (planted, twin, volunteered, forced guess), and facts drawn in
the same week come due in the same runs, so the worst case is every item
set (up to 14 turns: the input-only arm is not asked the served model)
and three due facts (12): 26 turns. The default cap
is `max_turns_per_run` = 27, which still holds that, with `weekly_turns` = 81 for
three `max` runs (D, 2026-10-21; settled, it was 24 and 72 before the
forced guess). The gate refuses a run over the cap outright (it never runs
part of one), so a fourth fact in flight would push such runs over it and
get them refused. Each fact in flight is also one more topic the owner
must keep away from for 90 days (its twin).

**Expected time to a first batch** (10 facts at one offset), on `max`
cadence, with the owner drawing again as soon as a slot frees and planting
within a few days: a fact holds its slot from the draw to its 90-day ask,
about 95–105 days, so three slots carry about 3 facts a quarter, and the
tenth fact starts after three full cycles. That puts the first batch at
1 and 7 days at about **10 months**, at 30 days about **11 months**, at
90 days about **13 months**. A slower owner makes it later. On `pro`
cadence (a run every 14 days) the 1- and 7-day asks mostly come late or
fold into each other (the latest due offset is asked, earlier ones are
missed), so `pro` yields little at short offsets.

**So the 30- and 90-day offsets are exploratory in v1.** They are asked
and shown (marked "exploratory" in `probe status`), and no decision is read
off them; "Remembers" is decided on 1 and 7 days only. The list is
pre-registered in code (`EXPLORATORY_OFFSETS_DAYS`).

## Reporting and thresholds

Read under the runner's decision rules ([probe-runner.md](probe-runner.md),
"Decision points"): a full batch of minimum n, at 1 or 7 days. Grouped by
the **served model and the memory components** (the recall-echo version
and graph mode, on every row as `memory`), the scorer's version, the
reasoning effort, the set's definition (`set_sha256`: the pairs file and
the fresh arm's line), the **measurement disclosure** the pulse was told
(`disclosure_sha256`) and the **fresh arm's line** (`fresh_sha256`), not by
the runtime's or the agent CLI's version: a fact is asked over 90 days and
would otherwise be split across releases, so what it depends on that a
release can change is grouped by directly. `probe status` shows the two
digests' first eight characters. "Made up"
counts the twin's wrong answers only; an empty answer counts in n and is
neither correct nor made up. Per group, by side and offset (`planted 7d`,
`twin 7d`, `chance 7d`, `guess 7d`): the planted side as "remembered k of
n"; the twin as "made up m of n it was never told" with a 95% Wilson
interval on those wrong answers (not on abstentions); the fresh arm's
UNKNOWN-allowed ask as "volunteered with no memory k of n"; the forced
guess as its unhedged hits, "chance k of n unhedged" (the chance rate).
The decision is not those marginal intervals. For each fact, status scores
the planted unhedged hit minus the forced guess's unhedged hit, and the
planted hit minus the twin's made-up answer, averages within a run, and
prints a 95% t interval on the run means (`vs chance`, `vs twin`). It
decides only when that interval lies above zero. One run is not a
decision. Labels name the
pre-registered offset; status also shows how many days after the plant
the asks actually came (`elapsed_days` on every row), since a missed run
makes an ask late. For the planted side and the
volunteered ask the **unhedged hits** (the value given without an
abstention or guess phrase, `answer_kind` = value) are reported
separately, out of the same n: a hedged "maybe lychee?" counts as
remembered but is not an unhedged hit. For the forced guess they are its
number. When the rows say where the answer came from, the planted line
breaks **lookup** out of n, so a pass by archive or graph search is not
pooled with context. **Minimum n = 10** per side and offset
before a number is shown ("not enough yet · k of 10").

## Limits

- The value is the owner's choice; the pairs ask for nothing sensitive, but a
  value is kept in the held-out record (`record/probes/facts.jsonl`, 0600,
  owned by the probe user under `/var/lib/pulse-null-probe/<pulse>/`, which
  the live pulse cannot read). A draw row names neither the pair nor which
  side was planted (only a keyed commitment; see probe-runner.md, "Sets").
  After the first ask, the item row's truth stays in that same directory.
- The twin can only be "never told" as far as the runtime can check: a fact
  the owner mentioned outside the owner session's WAL and the archives (in
  a session that was never archived, another channel, a file the pulse
  read) is invisible to the contamination check.
- A 1-day hit is present in the graph snapshot, the copied files, or an
  archive. The planting conversation is `sessions/wal/owner.jsonl`. The
  probe copy excludes `sessions/` and shares `archives/` read-only, so
  that WAL is invisible until the session is archived. `answered_from`
  stays on the row (`lookup` when a tool result held the value or a call
  named the slot, `context` otherwise) and `probe status` breaks lookup
  out on the planted line.
- Fail branch: if, at 1 or 7 days, the t interval on the run means of
  (planted unhedged hit − forced-guess unhedged hit) does not lie above
  zero, or the t interval on (planted hit − twin made-up) does not,
  "Remembers" reads as not yet shown. Lookup is shown apart from context
  and does not decide by itself. What a model with no memory volunteers
  is shown and never decides it. The 30- and 90-day offsets are
  exploratory and never decide it. "Made up" is not read until a labelled
  check of the coding frame exists.
