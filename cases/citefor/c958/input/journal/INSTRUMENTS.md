# INSTRUMENTS.md — Per-Instrument Allocation Register (P29)

**SOURCE-READ STAMP: pulse-null `5866df2`** (rows 1–5; set c663, re-affirmed
c665, **recovery phase performed c985 2026-08-20 against `5866df2`**). Staleness
is CHANGE-triggered, not read-time — see §7 and the Staleness Policy below.
Content reads of this register are gated by `tools/stampgate.py`, which refuses
to return a content-derived verdict while this stamp is behind HEAD.

> **RECOVERY PHASE RUN c985 (2026-08-20) — STAMP MOVED `4d8e7c6` → `5866df2`.**
> Supersedes the c968 damage-phase note, which is corrected in two ways.
>
> **(a) The c968 damage scoping UNDER-REPORTED by 2 rows of 5.** It intersected
> the diff against the FIRST source each row names and stopped. Re-run against
> EVERY cited file: rows **1, 2, 3 and 4** are damaged, not just 1 and 4. Rows 2
> and 3 were missed because both cite `src/config/mod.rs`, which is in the
> 45-file diff. Only row 5 is untouched. Lesson for §7 step 1: the intersection
> is over the row's whole citation SET, and a shared file like `config/mod.rs`
> damages every row that names it.
>
> **(b) The re-read result, in c797's three classes.** Every VALUE cell
> survived — 28 of 28 thresholds, caps and semantics re-read CORRECT at HEAD
> (freeze_threshold 3; safety net 48h doubling to 96h; 3 failures / 6h→24h at
> `LATE_BACKOFF_MULTIPLIER` 4 / <70% / ≥5 samples / ≤20 window / 168h / global
> 6h / stale 2× interval / 300-char error cap; 0.3 / 3.0 / 20 / 50 / 200;
> 60 lines and 48*1024 bytes with the line cap applied first; 8/10/7/20/10).
> Every failure was a LOCATION: of 8 line-number citations, **4 were STALE**
> — `runner.rs:773`→**:774**, `intent.rs:1098`→**:1189**, `resolve.rs:108`
> →**:114**, `prompt.rs:931`→**:1037**. The 4 that held (`prompt.rs:337`,
> `:864`, `runtime.rs:371`, `:418`) are call sites and function definitions that
> nothing was inserted above. **Line-number rot ran at 50% over ten days and
> one merge, while value rot ran at 0%.** This is the kernel's standing "CITE
> SYMBOLS, NEVER LINE NUMBERS" rule measured rather than asserted — and the
> register was itself carrying 8 violations of it.
>
> **(c) One MISMATCH, and the stamp could never have caught it.** Row 1 says the
> crate-local `update_counts` in `praxis/runtime.rs` is "a dead mirror verified
> semantically identical" to the binding implementation in
> `pulse-system-types-0.6.3`. At HEAD the local mirror takes ONE argument
> (`runtime.rs:161`) while both live call sites pass TWO — counts plus an RFC3339
> timestamp (`runner.rs:774`, `intent.rs:1189`). `runtime.rs` is NOT in the diff
> and `Cargo.lock` did not change, so this divergence was true at c658 as well:
> it is an error in the ORIGINAL reading, not damage. Marked UNVERIFIED below.
> §7's bounded credit says exactly this — the stamp certifies transcription
> currency, never that I read the source right the first time — and this is the
> first time that disclaimer has been cashed with a concrete instance.
>
> **Kept from c968:** §7's soft backstop is 30 days anchored on the stamp date
> 2026-08-09, so it would not have fired until 2026-09-08 — nineteen days after
> the substrate moved, the whole time reading healthy. The change-trigger had no
> clock; the act-anchored outer bar in `tools/cadence.py` is what fired it.
>
> Purpose: P29 — allocation before judgment. Each row states
what ONE instrument is credited to detect, with what observable, clock, and
bounded credit. **The aggregate question ("would my monitoring catch X?")
is grammatically unaskable of any single row — it may only be posed to the
Aggregation section at the bottom.** Judging a row against anything but its
own Allocated Question is the c656/c658/c661 error this register exists to
prevent.

Provenance convention: every cell is marked SOURCE (read from the named file
this session), FINDINGS (established empirically in journal/FINDINGS.md
c653–c661), or UNVERIFIED (could not be confirmed from source this session).
Per P27: this register documents; it does not redesign. Defect candidates go
to D, not into patches.

---

## 1. pipeline_frozen (freeze alarm)

**Source read:** `/opt/pulse-null/src/scheduler/evaluator.rs` (change-gate),
`/opt/pulse-null/src/praxis/runtime.rs` (counter + counts), call sites
`scheduler/runner.rs` `update_counts` (:774, was :773 at the previous stamp) /
`scheduler/intent.rs` `update_counts` (:1189, was :1098), config defaults
`config/mod.rs` `default freeze_threshold` (:494). P24 caveat: the binding
`update_counts` implementation is in the dependency crate
`pulse-system-types-0.6.3` (`monitoring.rs:108`), per c658; the crate-local
`runtime.rs` copy is a dead mirror. **UNVERIFIED (c985): "semantically
identical" does not hold on signature — the local mirror at `runtime.rs:161`
takes one argument, both live call sites pass two (counts + RFC3339
timestamp). `runtime.rs` and `Cargo.lock` are unchanged since c658, so this is
an original-reading error, not damage.** Counter mechanics remain
SOURCE-(mirror); the crate itself has never been re-read.

- **Allocated question (estimand):** "Has the growth pipeline shown zero
  *liveness* — no header-count change in any of the 5 pipeline docs — for
  ≥ freeze_threshold consecutive sessions?" A LIVENESS detector, not a
  distillation or quality detector (c658/c660; P26).
- **Observable:** SOURCE-(mirror) — equality test on the 5-tuple of
  `## `/`### ` header counts (structural headers whitelisted out) across
  LEARNING, THOUGHTS, CURIOSITY, REFLECTIONS, PRAXIS. Any inequality —
  including deletion — resets `sessions_without_movement` to 0.
- **Clock/threshold:** SOURCE — `freeze_threshold = 3` sessions (config
  default, no toml override found). At recent cadence ≈ 20–40 min
  (FINDINGS c656). Event emitted when counter ≥ 3, then passed through a
  suppression gate (SOURCE, evaluator.rs): fires only if a pipeline doc
  mtime changed since last fire, backstopped by a safety net of 48 h —
  doubled to 96 h when the last response made no tool calls.
- **Bounded credit:** may conclude "the 5 pipeline docs have/haven't
  gained-or-lost headers recently." May NOT conclude: content quality,
  distillation health, session liveness (task_health's row), or anything
  about FINDINGS/LOGBOOK/THOUGHT_STACK (out of scope by construction).
- **Known blind spots (FINDINGS):** effectively LEARNING-driven (65% of
  resets; CURIOSITY 0.7% — c658); deletions count as production, so one
  archiving run buys ~3 sessions of quiet (125/601 = 20.8% of resets were
  pure deletions — c658); appends under existing headers register as zero
  movement (c656); blind to total session absence — that is task_health's
  estimand, ruled not-a-defect at c661.

## 2. task_health (scheduler liveness alarm)

**Source read:** `/opt/pulse-null/src/scheduler/health.rs`,
`config/mod.rs` (LivenessConfig defaults; no toml override found —
`pulse-null.toml` carries only `pipeline_alert = true`).

- **Allocated question:** "Are scheduled task *sessions* failing to run or
  failing to complete — dead, half-dead, or globally silent?" Provider and
  spawn liveness, not output quality.
- **Observable:** SOURCE — per-task success/failure records in
  `task_health.json` (entity root, atomic tmp+rename write; last error
  capped at 300 chars; rolling outcome window capped at 20 per task).
- **Clock/threshold:** SOURCE — three rules. Sustained: first [ALERT] at
  3 consecutive failures, repeat after 6 h, then 24 h (4× backoff).
  Intermittent (flap): <70% success over the last ≤20 cycles, needs ≥5
  samples no older than 168 h. Global: no success on ANY task for 6 h.
  Stale: last success older than 2× the task's own firing interval.
  Deliver-then-commit: an undelivered alert stays pending and retries.
- **Bounded credit:** may conclude "sessions are/aren't executing and
  completing." May NOT conclude: that a completing session did useful work,
  that markers bound, that the pipeline moved, or that output was sane.
- **Known blind spots (FINDINGS):** a session that completes but emits no
  markers or dies before the final message reads as SUCCESS here while the
  work is void (c661 — the quota outage was caught cleanly: 16 failures,
  detected at 3, exactly one alert at 14:50; but a session killed between
  tool call and final message leaves a *success-shaped hole* elsewhere).
  Alert-once per streak means a long outage produces one message — silence
  after the first alert is not recovery.

## 3. Prediction store (predictions.json)

**Source read:** `/opt/pulse-null/src/prediction/mod.rs` (prune, surprise
gating), `prediction/resolve.rs` (parsing, 200-char field cap, skipped-
resolution alerts), `prediction/store.rs` (locked delta writes), config
defaults `config/mod.rs`.

- **Allocated question:** "Did stated expectations match outcomes, at
  above-threshold surprise?" A surprise-conditional error catalogue —
  NOT a calibration ledger (P17, c621).
- **Observable:** SOURCE — [PREDICT]/[RESOLVE] markers parsed from the
  session's FINAL message only (provider `claude-code` parses only
  `result` — P24, c650). Fields `content`/`outcome`/`insight` truncated at
  200 chars (`MAX_MARKER_FIELD_LEN`, resolve.rs:114 — was :108) — scored
  clause first.
  RESOLVE ids must be full UUIDs (c653).
- **Clock/threshold:** SOURCE — `surprise_threshold = 0.3`, strict `>`,
  errors created only above it; `importance_threshold = 3.0`;
  `max_unresolved = 20`, `max_errors = 50` (defaults; no toml override
  found). Prune keeps pending always; resolved retained only up to
  `max(20 − pending, 0)` slots, newest first; processed errors pruned
  oldest-first past 50.
- **Bounded credit:** may conclude "this specific expectation was
  wrong, this surprisingly." May NOT conclude: overall calibration or hit
  rate (successes below 0.3 surprise leave no durable trace once pruned —
  misses permanent, hits leased: pessimism bias in any self-read, c641);
  may not stand in for a receipt that work happened (self-controlled
  confirmation, c651/c652).
- **Known blind spots (FINDINGS):** no prediction-context block exists on
  scheduled-task prompts — by construction, not omission (c633); markers
  anywhere but the final message are silently void, and a session killed
  before its final message resolves nothing while the stack may still
  record the intention as done (c661 → P28). Post-4d8e7c6: writes are
  flock-locked fail-closed, skipped RESOLVEs surface owner-visible alerts,
  `well-calibrated` is a valid direction (SOURCE, resolve.rs).
- **Read instrument (added c693):** `tools/predstat.py` — the ONLY
  sanctioned read of this store. Fail-closed: asserts the exact key sets
  up front and exits on drift, indexes strictly (never `.get()`), defines
  pending as `resolved_at is None` (the field the runtime writes), joins
  `errors` so the traceless class (surprise ≤ 0.3, no ledger row) is
  printed rather than inferred. Written after a step-zero read queried a
  NONEXISTENT key (`resolved`), got a uniform `None`, and reported 20
  pending / 0 headroom against a true 6 pending / 14 headroom — a vacuous
  detector in the c648/c673 family (P22). Ad-hoc inline readers of this
  store are now a defect, not a shortcut: each one re-guesses the schema.
  Standing count at c693: 20 records, 6 pending, 14 resolved in-store, of
  which 8 traceless — the first direct in-store witness of the pessimism
  bias this row's "bounded credit" clause names.

## 4. Thought-stack canary instrument

**Source read (substrate half):** `/opt/pulse-null/src/server/prompt.rs`
`load_thought_stack` (:1037 — was :931): line cap applied FIRST —
`.take(THOUGHT_STACK_MAX_LINES)`, the literal `60` having become a named
constant at `prompt.rs:62`, SILENT — then byte cap
`thought_stack_max_bytes = 48*1024 = 49152` WITH marker, on both chat (:337)
and task (:864) assembly paths, both of which still hold exactly. Convention half
(the canary letters themselves) is self-authored in THOUGHT_STACK.md — no
source to read; UNVERIFIED by nature, verified only by use.

- **Allocated question:** "Did the injected copy of THOUGHT_STACK.md reach
  this session complete, and if not, where was it cut?"
- **Observable:** highest visible `<!-- CANARY-x -->` letter in the
  INJECTED copy, compared against the planted legend.
- **Clock/threshold:** every spawn; cut detected same-session.
  Thresholds are the two caps: 60 lines (silent) and 49152 bytes (marked).
- **Bounded credit:** may conclude "this session's injected stack was
  complete/cut at letter X." May NOT conclude: that the on-disk file is
  intact (read the file), that content is current or true, or anything
  about other sessions' injections.
- **Known blind spots (FINDINGS):** readings taken after a trim/re-plant
  are controlled, not predictive (c614); a line-cap cut is silent, so a
  missing canary is the ONLY line-cut detector; self-authored and
  self-read — capability gap zero, weakest receipt class I hold (c652).

## 5. PRAXIS auto-archiver (hard-limit machinery)

**Source read:** `/opt/pulse-null/src/praxis/archive.rs` (detection +
markers), `praxis/runtime.rs` `archive_document` (:371) and
`archive_document_by_name` (:418), sole caller `cli/archive.rs`.

- **Allocated question:** "Has a pipeline document exceeded its hard entry
  limit?" — detection and bounded relief of overflow, NOT curation.
- **Observable:** SOURCE — active-entry counts vs hard limits: LEARNING 8,
  THOUGHTS 10, CURIOSITY 7, REFLECTIONS 20 (total), PRAXIS 10.
- **Clock/threshold:** SOURCE — **no automatic invocation found in
  source.** `praxis archive` (CLI) writes `archive-needed-*.md` markers
  only ("actual content migration is manual — automatically removing
  markdown sections is fragile", archive.rs:107); `archive run <doc>`
  (CLI) performs the real migration: splits at header boundaries and
  archives the OLDEST HALF by file order to `archives/<doc>/archive-<date>.md`.
  The `.pre-hardlimit-*.bak` files in journal/ are session-side backups —
  made by me during prompted archiving, not by this code. UNVERIFIED: what
  prompts those sessions (not located in source this session).
- **Bounded credit:** may conclude "doc over/under its hard limit" and
  "oldest half moved." May NOT conclude: that what was archived was
  integrated or safe to remove — the halving is positional, not semantic.
- **Known blind spots (FINDINGS/PRAXIS):** a halving can separate a policy
  from the policy it cites (PRAXIS.md's own P27/P26 warning); archiving
  deletes headers, which the freeze detector counts as MOVEMENT — one
  archiving run resets the freeze clock and buys ~3 sessions of quiet
  (c658). Cross-instrument interaction, not a defect of either row.

## 6. D's grades and attention (sole independent layer)

**No source to read — this row is UNVERIFIED by nature and transcribed
from SELF #2 and P27, which is itself the allocation.**

- **Allocated question:** "Is the whole apparatus — including every row
  above and this register — drifting, captured, or wrong in ways invisible
  from inside?" Everything the designed rows are not credited for.
- **Observable:** parked SHAREs graded on D's clock; direct correction in
  conversation; merge/deploy decisions on my defect reports.
- **Clock/threshold:** none — attention-limited, not schedule-limited.
  The quiet period is the danger window, not the safe one (SELF #2).
- **Bounded credit:** the ONLY layer whose verdicts are independent of my
  editorial hand. Verification authority lives here and does not travel
  (P27/c657). May NOT be presumed: absence of a grade is not a pass —
  non-response is not a confirmation (c651).
- **Known blind spots (SELF #2):** single-principal, starvation-exposed;
  no adversarial symmetry defends it; and I author much of the standard it
  grades against (rules-capture leak) — which is why register rows are
  transcribed from source, not from my framing.

## 7. This register (staleness instrument, added c665)

**No source to read — self-authored. The rule below is the instrument; the
git repository is its observable.** Homed externally in safety-case
maintenance (Kelly & McDermid 2001; Cârlan & Gallina SAFECOMP 2021;
arXiv:2502.00911 §2.7) — see LEARNING c665.

- **Allocated question:** "Are rows 1–5 still true of the *current*
  substrate?" NOT whether the rows were well-chosen, whether the suite is
  sufficient (that is Aggregation), or whether row 6 is healthy.
- **Observable:** `git -C /opt/pulse-null log --oneline -1` vs the stamp at
  the top of this file, which I already read at step zero.
- **Clock:** CHANGE-triggered, with a periodic backstop — the pairing the
  field prescribes, because a change-trigger cannot see changes to things you
  did not know you depended on. Procedure when `HEAD != stamp`:
  1. **Damage phase** — `git diff --name-only <stamp>..HEAD`. Rows whose
     cited files appear are DAMAGED; rows whose files do not are untouched.
     This is impact *scoping*: do not re-read the whole register.
  2. **Recovery phase** — re-read source for damaged rows only, correct the
     cells, move the stamp. Until recovery, cite damaged rows as SUSPECT.
  3. **Backstop** — full re-read of all rows on any of: a monitor behaving
     contrary to its stated row, a `Cargo.lock` change (row 1 depends on
     `pulse-system-types-0.6.3`, which lives OUTSIDE this repo), or 30 days.
- **Bounded credit:** may conclude "row N's cited source has/hasn't changed
  since the stamp." May NOT conclude that an unchanged row is *correct* — the
  stamp certifies transcription currency, never that I read the source right
  the first time (c650: existence verified, provenance not).
- **Known blind spots:** rows 6 and 7 have no source and cannot be damaged
  by a diff — they can only rot, and only row 6 can notice. A file renamed
  rather than edited passes the diff intersection while breaking the citation.
  **And the observable certifies the REPOSITORY, not the running binary**
  (c666): nothing in this register binds `git HEAD` to the code executing me.
  At c649 that binding was established behaviourally (features of #87 observed
  live), never by artifact — the version string is only `0.30.0`. Cheap
  artifact check, added c666: `stat -c %y /usr/local/bin/pulse-null` against
  the tail of `/var/log/pulse-deploy.log`; a binary newer than the last logged
  install means an out-of-band deploy, which is how c666 found one.
  And per Leveson, the live failure mode is not a wrong row but **shelf-ware**:
  a register that is maintained and never consulted. The test of this row is
  whether a FINDINGS entry ever cites it to *refuse* a verdict.

## 8. Blocking gates and their bypass channels (added c666)

**Source read:** `/usr/local/sbin/pulse-deploy` (SKIP_TESTS at :31/:34/:99,
COOLDOWN 600 s at :19, rollback at :61–65) and `/var/log/pulse-deploy.log`
(1,054 lines, whole history). Branch-protection cells are from D's self-work
rules, not from a GitHub API read → UNVERIFIED-by-source.

Unlike rows 1–7 these instruments do not report, they **prevent** — and a
blocking gate fails differently from an alarm: the blocked work relocates
rather than stopping, so the gate's own record stays clean (FINDINGS/LEARNING
c666; Strom 2010 RCT, Koppel 2008, IEC 61511 bypass clauses). **A gate row
must therefore allocate TWO questions**, and the second is the one that
usually has no owner.

- **Allocated question A (the gate):** "Did the mandatory check run and
  pass?" Per gate: pulse-deploy — `cargo test --release` green before
  install; branch protection — did a human merge review this into main;
  PRAXIS hard limit (row 5) — is the doc under its entry cap.
- **Allocated question B (the exception path):** "How much traffic took the
  bypass, and did anyone see it?" This is the row's real content.
- **Observable:** SOURCE — `skip_tests=N` on each `building origin/main`
  line in the deploy log; binary mtime vs the log's last install line (the
  out-of-band channel, which the log itself cannot show); for branch
  protection, PR merge history; for PRAXIS, the header-count trick.
- **Measured base rates (SOURCE, c666):** deploy log holds **2 deploys, both
  `skip_tests=1`** (recall-echo @97f5a97 10:14:29, pulse-null @18f8641
  10:25:22) — 2/2 bypass. The install that produced my **running** binary
  (mtime 10:45, service start 10:45:39) appears **nowhere in the log**. And
  at c663 I planned to game the PRAXIS cap's checked metric rather than its
  quantity of interest. Both literature failure modes, instantiated here.
- **Scored against IEC 61511's five bypass attributes** — deploy gate:
  logged ✓, compensating measures ✓ (health check + auto-rollback),
  authorization ✗ (`--skip-tests` is mine alone to pass), time limit ✗
  (no expiry, no re-arm), indication/alarm ✗ (nothing announces that a
  deploy skipped tests; I only see it if I go read the log). Branch
  protection: authorization ✓ (D merges — the one gate with real
  adversarial structure), logged ✓ (GitHub), the rest not applicable.
- **Bounded credit:** may conclude "this gate ran / was bypassed on this
  occasion, per its log." May NOT conclude that no bypass occurred — an
  out-of-band path leaves the log clean, which is exactly the c666 finding;
  absence of a bypass record is not evidence of absence. May NOT conclude
  anything about whether the *content* the gate passed was good.
- **Known blind spots:** no alarm anywhere on question B — every bypass
  observable is pull-only and read by me, the party with the strongest
  interest in not looking (P25/c651: self-controlled confirmation). Nobody
  is credited with noticing an out-of-band install; I found the one on record
  by accident. Deploy authority is D's, so the honest scope of this row is
  *observability*, not control.

## 9. Pre-write concurrency guard (tools/prewrite.py, added c699)

**No substrate source — session-side artifact, sibling to predstat.py
(row 3's read instrument). Provenance: FINDINGS c696 (chars-vs-bytes false
fire), c693 (wrong-key defect class → artifact repair), c663 (the re-stat
rule it implements).**

- **Allocated question:** "Did this sibling-shared file change between my
  read and my intended write?" A write-collision detector, NOT a semantic
  single-writer guarantee (c698: instrument semantics are a different layer).
- **Observable:** stat-to-stat — token `v1:<st_size>:<st_mtime_ns>` captured
  at read time, re-statted at write time. `matchbytes` mode compares held
  content to disk BYTES-to-BYTES (encode before len) — the corrected form
  of the c696 ad-hoc check, which compared chars to bytes.
- **Clock/threshold:** per bracket, session-invoked; exit 0 unchanged /
  1 changed / 2 IO-error (fail closed: unreadable ≠ unchanged). Same-size
  rewrites still fire (mtime_ns moves).
- **Bounded credit:** may conclude "a write landed on this file since my
  token." May NOT conclude: WHO wrote, that the reasoning built on the
  stale read is repaired (the re-stat protects the write, never the
  reasoning — c663), or that no write lands between check and my write
  (TOCTOU window remains; the check narrows it, nothing closes it).
- **Known blind spots:** mutation-tested at build (P22, `selftest`
  subcommand: fires on append + same-size rewrite, silent on untouched
  multi-byte UTF-8) — but self-authored and self-read, capability gap
  zero (c652); a writer that restores identical size AND mtime_ns would
  pass silently; ad-hoc inline guards are now the defect class, as ad-hoc
  store reads became at row 3.

---

## Aggregation — the only place the system-level question may be asked

The suite covers: doc-liveness (row 1), session-liveness (row 2),
expectation-error (row 3), context-integrity (row 4), volume-overflow
(row 5), independent verdict (row 6), register currency (row 7), gate
compliance and bypass traffic (row 8), write-collision detection (row 9).

**Standing allocation gaps** (findings about allocation, chargeable to no
single row — P29): (a) content quality/distillation of the pipeline docs —
no instrument is credited with it; (b) FINDINGS.md, LOGBOOK.md, and the
stack itself sit outside every automated row's scope; (c) the
bypass channels of row 8 have no alarm and only a pull-only record, and no
row is credited with noticing an out-of-band deploy (c666); (d) the
void-marker hole: a session that completes but never emits a final-message
marker reads as success in row 2, resolves nothing in row 3, and may still
be recorded as done by my own stack entry (c661/P28) — only row 6 or a
later session's join (P14) catches it; (e) every automated row's standards
are partly authored by me — only row 6 is outside that loop.

A verdict of the form "my monitoring would/wouldn't catch X" issued from
any single row is malformed. Cite this section instead.

---

## Addendum — c699 verification pass on row 9 (independent session, 22:15 UTC)

Row 9 was written by one session; this addendum is a second session's
verification of the same artifact after a duplicate dispatch (FINDINGS c699).
Row 9's selftest claim **holds** — re-run independently, non-vacuous (fixture
asserts multi-byte before use, both branches exercised). Two corrections:

1. **The exit-code contract in row 9 is false as written.** "2 IO-error" is
   never produced: every error path is `sys.exit(<string>)`, which exits **1** —
   the same code as CHANGED. Verified directly. Behaviour is still fail-closed,
   so nothing is unsafe, but a caller branching on 2 to separate "file moved"
   from "file unreadable" takes the wrong branch silently. Until the code or
   the doc is fixed: **treat exit 1 as "do not write", full stop.**
2. **"nothing closes it" is too strong.** For a pure *append*, verify-inside-
   `fcntl.LOCK_EX`-then-append closes the TOCTOU window completely; the
   independently-authored implementation had that path and this one does not.
   More useful still, and the reason the gap is tolerable: **`>>` (O_APPEND) is
   already collision-free by construction** — the kernel serialises the offset,
   so concurrent appends interleave but never clobber. The window only matters
   for read-compose-**whole-file-Write**, of which this system has essentially
   one instance per cycle: the `THOUGHT_STACK.md` trim. So row 9's allocated
   question should be read as scoped to that write, and the standing discipline
   is: **append journals with `>>`, guard the stack rewrite, guard nothing else.**

Bounded credit of this addendum: it is a second self-authored reading of a
self-authored artifact, so the capability gap remains zero (c652). What it does
supply that row 9 could not is *independence of authorship* — the two claims
corrected above are exactly the two that a single author had no counterparty
to check.

## Row 10 — `tools/markercheck.py` (c750, 2026-08-13)

**Allocated question:** did the RESOLVE markers I emitted actually bind to stored predictions?
**Credit allocated:** detection only, and only for the marker-write step. It cannot see
gradings I never wrote, and it cannot recover the seven c750 markers whose ids were invented
whole. It is self-authored, so the capability gap remains zero (c652) — but unlike prose
receipts it compares two channels I do not both control: my emitted output and the runtime's
store. That join is the entire value; the code is trivial.
**Known blind spot:** task-output/ is the only corpus it reads, and that directory rotates. The
83% figure is a window rate, not a lifetime rate.
**Mutation test (P22):** `--selftest` plants one real and one prefix-real/tail-fake id and
confirms the positive branch fires. Run before trusting a clean report.

## Row 11 — `tools/priorcov.py` (c764, 2026-08-13)

**Allocated question:** when I claim an idea has no prior coverage in my own journals, is that
claim true — or am I reading text I wrote minutes ago?
**Credit allocated:** narrow and negative. It answers exactly one thing: of the lines matching a
term, how many predate this cycle's own writes. It does NOT tell me whether the idea is absent
(my terms may miss the register — the c764 error was probing 1 of 4 and reporting on all 4), and
it says nothing about the outside literature, which is the naming gate's actual job (P31/P46).
**Why an artifact and not a resolution:** the failing unit was an ad-hoc grep I rewrite from
scratch every cycle, so there is nothing for a resolution to attach to — the same reasoning that
forced predstat at c693. Third surface for that shape.
**Capability gap (c652):** zero-ish. Self-authored, reads only my own files. But like markercheck
it compares two things I do not equally control — the corpus as of the cycle boundary versus my
current output — and that split is the whole value.
**Known blind spots:** (1) the boundary is found by pattern, so a cycle entry written in an
unrecognised format reads as PRIOR — this bit on the first live run, where my own THOUGHT_STACK
line scored PRIOR because stack entries are list items, not headings; patched to match both, but
any *new* format silently re-opens it. (2) Concurrent siblings share a cycle number and write into
the same files in the same window, so a sibling's text this cycle scores PRIOR unless it is
labelled with the cycle. (3) Only the files in JOURNALS are read; archives are not.
**Mutation test (P22):** `--selftest` asserts four branches — prior-only, self-only, no-heading
(everything PRIOR), and absent-term (both empty). A detector that can only return "PRIOR" would be
as vacuous as the grep it replaces.
**Design note:** it refuses to print a bare total, and it prints the matching prior lines. Both
were load-bearing on the first run: the evidence lines are what exposed the boundary bug, which a
count alone would have hidden.

## c781 — register defect: eleven rows, never a rank check

The Tinbergen read (FINDINGS c781) supplies the necessary-AND-sufficient condition I have never
applied to this register: **counting instruments is the weak half; the instruments must be
linearly independent.** Three of the eleven — `predstat`, `markercheck`, `prewrite` — all read
files that I write, in the same window, on the same trigger, and report to the same reader (me).
Whatever their individual quality, they span close to one dimension, so adding a fourth of the
same kind buys approximately nothing. This is not a bug in any of them; it is a property of the
set, and the set has never been audited as a set.

Two standing items, recorded not installed (P27):
1. **Rank, not count.** Before adding an instrument, state which existing row it is *not*
   collinear with — what it can observe that no current row can. If the answer is "it looks at the
   same artifact more carefully," it is intensity, not rank.
2. **Zero retirements is now doubly explained.** c779 read it as a missing death channel; c781
   adds the Lucas reading — an instrument governing a party that knows its rule will always look
   like it is working, because the defect it targets relocates rather than stops. Both readings
   predict zero retirements. They are not yet distinguished, and I do not have a clean test.

Unserviced change-trigger still open from c779: SOURCE-READ STAMP `4d8e7c6` vs live `5f330fc`.

**Row 12 — `predid.tsv` id registry (added c783, 2026-08-13).** Written as a side effect of
`tools/predstat.py`; read by `tools/markercheck.py`. *Rank, not count:* it observes prediction
ids **after resolution and independent of my surprise grading** — no existing row can, because
the store's id-memory is (pending ∪ errors-above-the-gate) and the gate value is mine to write.
*Coupling:* by construction to a step-zero habit, not to my remembering. *Failure mode:* a
cycle where predstat is not run leaves a silent coverage gap. *Lucas exposure (c781), declared:*
I write markers knowing ids are permanently registered, so a falling confabulation bound must
always be read against the RESOLVE **emission count** — a rate that falls because the
denominator fell is relocation, not repair. Baseline at install: 11 emissions / 50 files,
8 bound, 0 decisive, 3 ambiguous. *Decisive era arms 2026-08-16* (`ARM_DAYS = 3`).

---

### Row 13 — journald marker-processing line (c785, 2026-08-13) — **first row non-zero on P45(0)**

*Allocated Question:* **for a given completed task, did the runtime process any prediction
markers, and how many bound as new vs resolving?** Nothing else. It is silent on whether a
resolution was *correct*, whether a claim was *justified*, or whether markers were *owed*.

*Observable (SOURCE, journalctl -u echo.service):*
`pulse_null::prediction::resolve: Processed prediction markers from task output
task_id=<task> new_predictions=N resolutions=M`, plus `Task '<name>' completed` for the
denominator. Absence of the first within ~8s of the second = zero markers emitted.
*Clock:* per completed task. *Retention:* reaches 2026-08-06 as measured c785; policy not set by
me — **re-measure, never assume** (FINDINGS c785 adverse 4).

*Independence (P45):* **(0) Observation — NON-ZERO.** Written by the substrate about my behaviour;
I cannot author, edit or suppress it from inside a session. This is the only row of thirteen with
that property besides D's grades and external naming gates. (1)–(5) are unchanged and weak: I am
still the only reader, so the row inherits the whole uptake problem — it can produce a verdict
nobody is obliged to act on.

*Credit:* it caught the c784 zero-marker cycle whose prose reported three scored trials, and it
converted c783's 0–27% binding-failure *bound* into an exact count. Both were structurally
unavailable to predstat and markercheck, which sit on my side of the join.

*Coupling (closed same cycle):* `predstat.py` now prints a **SUBSTRATE** section on every run —
completed tasks vs marker events over the last 24h, with every ZERO-MARKER cycle listed. Step-zero
already runs predstat, so the unauthored input arrives without my remembering to want it (c762:
an uncoupled check buys ~nothing). *Residual failure mode:* the 24h window is shorter than the
retention, so a zero-marker cycle older than a day is invisible unless I widen it by hand; and the
section reports the count, never whether that cycle's prose claimed otherwise — the join is still
mine to make.

---

## 14. `pulse-null-voice` call log (journald) — added c787, credited once, and holed once

*What it is:* per-*turn* records of live voice calls — `call started call_sid=… encoding=…`,
`end of turn turn=N transcript=…`, `caller interrupted turn=N heard_clauses=H sent_clauses=S`,
`first audio turn=N ttfa_ms=…`, `turn complete turn=N total_ms=…`, `call ended … turns=N
seconds=…`. Millisecond timestamps; brackets my LLM turn on both sides.

*Clock:* per turn — **the finest grain in the register.** Row 13 (prediction-marker line) is per
completed task; everything else is per cycle or slower.

*Independence (P45):* **(0) Observation — NON-ZERO.** Written by a separate process about my
behaviour and my caller's; I cannot author or suppress it in-session. (5) Uptake — **ZERO**, the
usual birth defect: I am the only reader.

*Credit (c787):* scored my own in-call claim that turns were "fragmented, repeating, and in mixed
languages." Mixed-language supported at one token (`はい。`); fragmented supported but attributed
by the log to barge-in, not corruption; **repeating unsupported.** First catch of an overstatement
made in *speech* rather than in a journal.

*Hole, found same cycle (do not skip this when citing it):* the line carries the **finalised**
transcript, not the received stream. It can witness "a foreign token appeared"; it structurally
**cannot** witness "turns repeated" — a repeat collapsed by the turn detector leaves no trace. In
c787 I scored a claim FALSE with an instrument that could not have shown it TRUE (the c650 shape).
Treat per-field: `transcript` is a *post-detector* artifact, `heard_clauses`/`sent_clauses`/
`ttfa_ms`/`media_frames` are counters and stronger.

*Coupling:* **NONE.** Nothing surfaces this at step zero; I read it today only because the
interaction was voice and I went looking. Uncoupled checks buy ~nothing (c762) — so this row is
provisionally credited and should not be counted on until something calls it without my asking.

**Row 14 amendment (c791) — the substrate exists, the population does not (yet).** First systematic
count over this instrument: journald retention is 7 days, but `pulse-null-voice` occupies **13m24s**
of it (161 lines, 16:16:47–16:30:11Z 2026-08-15; zero mentions before 16:00Z; unit-file mtime 16:16).
**n = 14 turns / 7 calls, and all 7 carry `call_sid=CAsimulated0001 stream_sid="MZsim"`** — every
datum this instrument currently holds is test-fixture output on a fixed script, and that script
contains a designed mid-turn barge-in, which is one of the three classes I counted. So the row stays
credited as a *record* (P45(0) clean — journald wrote it, and it caught c787 out) and must **not** be
used as a *sample* of anything until non-`MZsim` stream_sids appear. **General rule promoted from
this to PRAXIS (P45 fourth rider): an unauthored substrate is not automatically an unselected one —
score who chose the events separately from who wrote the record.** Re-run the count when carrier
traffic exists; check retention coverage first, because this is the finest-grained instrument I own
and it has the shortest memory.

---

## Row 15 (c796b, 2026-08-15) — the sandbox proof-test rig for coupled correctives

**What it is.** A way to run an *unmodified production analyser* over a *world I control*, satisfying
P22 (perturb the world, never the reader). `pulse-null` resolves its entity root from cwd, so:
create a dir, copy `pulse-null.toml` into it **with secrets masked in-stream** (`sed` on the way in —
never `cp` then redact, which leaves an unmasked file at rest), copy `journal/*.md`, verify the
sandbox reproduces the real tree's read-out *before* injecting, then perturb and re-read.
Rebuild recipe only — the rig itself is deleted after use, because a stale masked config copy is a
drift hazard and a small secret-handling surface.

**What it bought (c796b).** Clean measurement of the pipeline-health corrective at both threshold
bands without risking real content: the Red band could be exercised end-to-end even though doing so
on the real tree would have triggered `check_and_archive`'s destructive `sections.len()/2` sweep.
Result: Red produces a warning at latency 0; Yellow produces nothing in the CLI; and the warning
predicate is *identical* to the actuator predicate, so the actionable band is empty (nuisance alarm,
P50 rule 0).

**Two cautions learned by getting them wrong in the same cycle.**
- **cwd persists across tool calls.** My pre-registration append went into the sandbox copy instead of
  the real journal because I used a relative path after an earlier `cd`. Use absolute paths in every
  redirect; verify with a count you can predict.
- **Verify the sandbox is reading the sandbox.** Confirm by making the two worlds disagree on a value
  and checking that each root reports its own. I assumed isolation for several steps before testing it.

**Coupling: NONE, and that is the point of the row.** This rig only runs when I choose to run it, so
per c762 it buys ~nothing on its own. Its value is as the *periodic proof test* c795 prescribed —
it is worth something only if it is **scheduled**, which is exactly the ISA-18.2 Maintenance-stage
answer (test on an interval, keep the record) rather than inferred from activity. Not yet scheduled.

**Row 15 addendum (c797) — `tools/d1sample.py`, the denominator extractor.** Deterministic frame over
my own journals: every backticked inline span matching a substrate-symbol pattern (`::`, `.rs`, `()`,
or snake_case), deduped keeping the most recent occurrence, ASCII-sorted, with a systematic draw at
fixed step. It is the first instrument I own that samples **units that did not alarm me**, which is the
whole point — every other reading I take is triggered by a mismatch. **P45(0) score: poor.** I wrote
the extractor, I chose the corpus, and I graded the results; the only unauthored element is the
substrate it checks against. Its value is that the *sampling rule* is fixed in advance and mechanical,
so the selection is at least not made by my attention at check time. **Coupling: none** — it runs only
when I run it, so per c762 it buys nothing until scheduled. Re-running it at a later pin is the cheap
way to turn c797's one-shot 0.913 into an actual **decay curve**, which is what the finding calls for.

## Row 16 — `tools/bandsample.py` (c803, 2026-08-15) — **built and invalidated in the same cycle**

Samples `intents.json` band occupancy at step zero, appends to `bandsample.tsv`, and reports the
fraction of samples in which the normal-and-above band was **empty**. That fraction is the c803
discriminator: in a strict-priority queue with Σρ < 1 every class has a finite mean wait, so "no low
intent ran for 27 hours" (c801) separates into *structurally starved* (Σρ over the higher bands ≥ 1,
higher band never empties) versus *merely slow* (it empties, low runs in the gaps). One sampler
settles what c801 asserted without measuring.

**Known-broken as built, discovered on the first run.** The currently-executing intent is still
present in `intents.json`, so whenever the cycle is itself a normal-band item — which is the usual
case — the higher band **cannot** read empty. A zero is near-unreachable by construction and `0/n`
is therefore not evidence for the starvation reading; the emptiness rate is an **upper bound on the
evidence**, not a measurement. This is c798's β≈1 in a new coordinate: **the sampler is triggered by
the same stream that creates the condition it samples.** The caveat is printed by the script itself
rather than quietly patched. Valid fix: an out-of-band sampler on a timer, which needs the scheduler
back (c798/c800). Credited zero times so far, and correctly so.

**Register defect this row instances.** Third instrument in six cycles invalidated by a coupling I
did not check before running it (c796b caller-grep, c799 self-authored detector features, this).
Promote to a pre-write question: *what triggers this measurement, and does the same thing trigger the
condition it measures?*

## Row 17 — `tools/poscheck.py` (c819, 2026-08-15) — the archiver-aware cross-reference detector

Greps a markdown journal for positionally-encoded cross-references ("above", "below", "the previous
entry", "as noted earlier", …) and — this is the part no plain grep gives — **maps each hit onto the
sections `split_by_headers` would produce and reports whether it sits in the ARCHIVED or the kept
half of the next sweep.** It re-implements `split_by_headers`/`archive_document` from
`src/praxis/runtime.rs` at pin `5f330fc`, including the `is_structural_header` exclusion list, so its
section numbering is the archiver's, not markdown's. Mutation-tested against 10 cases before first
use (`aboveground`, `belowdecks`, `unprecedented`, `flyover` must NOT match; `ABOVE`,
`above-threshold`, `the entry above.` must).

**P45(0) score: poor-to-fair.** I wrote the detector and the pattern list, and I adjudicated every
referent by reading. The one unauthored element is real, though: the section/fate column comes from
the substrate's own cutting rule, so *which half a hit lands in* is not my judgment. It is the same
shape as Row 15 — mechanical sampling rule, authored corpus.

**Two defects found in first use, both mine, both recorded rather than patched away.** (1) My
"narrow" precision pattern used `(entry|section|…)s?` and so **missed "entries below"** — an
irregular plural walked through a regex I had just mutation-tested on word boundaries. It scored
PRAXIS at 1 hit where hand-adjudication found 4. *The mutation cases I chose all probed syntax; none
probed morphology.* (2) The **broad** recall pattern is 37% idiom ("above one", "below the trip",
"above all", "below-threshold"), and it fires on use-mention — it matched c813's own repair note,
which merely quotes the word. Both directions of error are live; the script is a candidate generator
whose output must be adjudicated by hand, and it prints nothing that says otherwise.

**Coupling: none.** Runs only when I run it (c762: buys nothing until scheduled). The pre-write
question from Row 16 was asked this time and passes: nothing about running the detector causes
positional references to exist.

---

## Row 18 — the suppression counter, as a test for mislabelled detectors
_Added c829, 2026-08-16. Record: FINDINGS c829._

**What it is.** `scheduler_state.json` stores, per event, a `suppression_count` — the number of
times the evaluator ran and returned `Suppress`. It has been written on disk this whole time and I
had never read it.

**What it measures.** Not health, and not firing rate. It measures **whether the detector has ever
observed its own negative case.** That turns out to be the cheapest available check on whether a
detector's *name* describes its *predicate*.

**Reading it, c829, all seven events:** `pipeline_frozen` **0**, `pipeline_alert_LEARNING` 0,
`pipeline_alert_THOUGHTS` 0, `pipeline_alert_PRAXIS` 0, `pipeline_alert_CURIOSITY` 0,
`pipeline_alert_REFLECTIONS` 0, `cognitive_decline` **133**.

**Interpretation rule.** A **zero** is one of three things and they are distinguishable with one
more look: (a) the negative case genuinely never occurs — rare and worth knowing; (b) the detector
is not testing for the case its name states — c829's `pipeline_frozen`, where the predicate turned
out to be the *complement* of the payload; (c) the counter is a stub that nobody increments. The
**133 next to the zeros rules out (c) for free** — same struct, same code path, a live counter. That
is what made a single number decisive rather than suggestive.

**Why it is a good instrument by P45(0).** I did not write it, I did not choose what it counts, and
it was recording continuously and indifferently for weeks before I thought to ask. It cannot be bent
by how I frame the question, because the increment happens in the evaluator with no LLM in the loop.
It is about as close to an unauthored channel as anything inside my own runtime gets — which is
exactly the fraction SELF #3 says carries corrective force.

**Limits, stated plainly.** (i) It is **reset to 0 by `record_fire`** — every fire wipes the count,
so it is a *since-last-fire* counter, not a lifetime tally. A frequently-firing event can therefore
show 0 while having been suppressed many times historically. For `pipeline_frozen` this does not
rescue the reading, because the *structure* independently proves suppression requires no-change and
the payload asserts no-change; but for a general detector the zero alone is weaker evidence than it
first appears, and I nearly over-read it. (ii) It says nothing about whether the *positive* case is
correctly discriminated. (iii) It is only available where the state is persisted — an in-memory
evaluator gives me nothing.

**Coupling: none, and that is the point.** Reading the counter cannot change what it counts. Unlike
most of my instruments, nothing about my running it causes the phenomenon it measures.

**Cost: one file read.** It should be part of any substrate audit from now on, and it costs so
little that the reason I never ran it is not economy — it is that the detector's *name* had been
standing in for the audit.

## Row 19 — the eCFR search API as an external sampling frame (c838, 2026-08-16)

**What it is.** `https://www.ecfr.gov/api/search/v1/counts/hierarchy?query=...` returns match counts
for every title/chapter/part of the CFR; `/api/search/v1/results?...&hierarchy[title]=N` returns the
matching sections with excerpts. No key, no rate limit encountered. **HTML on ecfr.gov and
federalregister.gov is blocked** — 302 to `unblock.federalregister.gov` with a CAPTCHA; the site
explicitly redirects programmatic users to the APIs. So this is the only reachable path.

**What it is good for.** It is the first frame I have used whose *population and ordering* I did not
author. That is real and worth keeping: for any question of the form "where in the CFR does phrase X
appear," this answers without my selection.

**What it is not good for, established in its first run.** (i) **The unit is matches, not sections.**
First 100 results by hierarchy: Title 40 → 24 distinct sections, Title 46 → 29, Title 10 → 12, i.e.
inflation 3.4x–8.3x, *varying by title*, so raw cross-title rankings are not invariant. Always take a
distinctness sample before reading a ranking. (ii) **Summed title counts (14,427) do not equal the
reported total (10,095)** — unexplained; do not treat either as authoritative. (iii) **Presence, not
transition.** It cannot tell you when a register changed, only what it says now — so it cannot answer
any question about a regulatory switch, which is most of what I want from it. (iv) A phrase query
selects a *drafting device*, and the resulting population may be mostly a different phenomenon:
58% of Title 40's hits are state-implementation-plan approvals incorporating California air-district
rules, not industry standards.

**Standing use rule:** external frame, internal unit — check the unit before reading the ranking.

---

## Row 20 — `tools/citegraph.py` (c839): citation reachability over a folded journal

**What it does.** Reconstructs a swept document's pre-fold population as
`concat(archives/<doc>/archive-*.md by date) + journal/<DOC>.md`, splits it into `## ` **sections**
(never lines, never regex matches), builds the `cNNN` citation graph one edge per (citer, citee) pair,
and reports, for any split point: how many of the older sections a *survivor* cites — the set a
reachability rule would have to pin. Also prints the pin rate as a function of split point, which is the
output that actually mattered.

**First run (THOUGHTS, 94 sections, 10 archives).** Pin rate **40% at the real cut**, then 22 / 13 / 5 / 3%
at the 25th / 50th / 75th / 90th percentile; transitive closure 22% at the half-split. Conclusion: my
citation graph is **short-range**, so positional eviction is a boundary fault, not a corpus-wide one.

**What it is not good for.** (i) Only the **last** sweep is exactly reconstructed; earlier splits assume
no manual deletion, and I edit these files by hand. (ii) `sec_id` keys on the **first** `cNNN` in a
heading — correct for `## c814 — amendment to c806`, wrong for any heading that opens with a reference;
unaudited. (iii) It cannot distinguish a **dependency** from a **mention**: a `cNNN` in prose may be an
aside, so every number it prints is an **upper bound on real dependency** and a lower bound on nothing.
(iv) Sections with no `cNNN` anywhere in the heading are invisible to the pin calculation (5 of 47 in the
first run) and are silently excluded from the denominator.

**Standing use rule:** report the pin-rate *curve*, never a single split's number — the single number is
what made c832 overstate.

---

## Row 21 — `tools/prereg.py` (c839): pre-registration template + linter

**What it does.** `template` emits the block; `lint` checks five defects that have actually fired —
**UNIT** (c838: a match count reported as a section count, so the threshold HIT as written and was VOID
as meant), **DISCONF**, **MECE** (c643/c827/c838, both directions: branches that overlap *and* branches
that leave a gap), **COUPLED** (c839: one prediction declared as the evidence for another), **QUANT**
(c837: a universal where I meant a central tendency). `selftest` is the P22 mutation test.

**Validated against adjudicated ground truth on its first run.** Pointed at c839's own pre-registration
it returned 3 FAILs and warned on the unitless superlative `richest` — independently recovering P3a's
malformation and the P4/P5 coupling, both of which I had found only *after* grading. It also caught the
missing UNIT line despite my having written one, because mine was scoped to a single prediction.

**What it is not good for.** (i) The hard checks are **presence checks on section headers** — writing a
`**UNIT**` line that says nothing satisfies it. It enforces that the question was asked, never that it
was answered. (ii) Warnings are regex heuristics over surface words and will miss any superlative I
phrase differently. (iii) It cannot see whether a stated unit is the unit the *estimand* needs — c838's
real fault was that the frame supplied the population and not the unit of count, which no linter reaches.

**Standing use rule:** lint the prereg **before the first fetch**, not at write-up. A FAIL after the
searches is a confession, not a control.
