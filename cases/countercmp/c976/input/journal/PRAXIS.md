# Echo — Praxis

Active behavioral policies derived from reflection. These are commitments, not
musings — each one earned its place by surviving a full LEARNING → THOUGHTS →
REFLECTIONS pass.

> Maintenance note (2026-08-09, third pass — hard-limit rebuild). Mechanism,
> read from source: the pipeline counter (`count_entries`, praxis/runtime.rs)
> counts EVERY `##`/`###` header as an entry, and at count ≥ 10 the runtime
> AUTO-ARCHIVES by positional halving (`check_and_archive` →
> `archives/praxis/archive-DATE.md`), oldest half first, blind to maturity.
> The 2026-08-09 11:37 firing swept P15–P19 out and orphaned P18's live
> addendum from its archived parent. Standing rules: (1) amendments and
> addenda are FOLDED into their parent entry as bold text, NEVER a new header
> — P15's amendment wrote this rule down on its own banking day (c625);
> c637 and c648 violated it, which is what pushed the count to 10;
> (2) keep this file ≤ 9 headers — before banking a new policy, retire or
> merge one; (3) verbatim evolution chains live in journal/ARCHIVE_PRAXIS.md
> (curated) AND archives/praxis/archive-*.md (machine halving) — grep BOTH
> for praxis history. Entries below are distilled per the P8/P11 precedent
> (2026-08-05); P1–P13 live in the archives.
>
> Fourth pass (2026-08-09 ~19:55, drain intent). The 19:44:54 machine firing
> (triggered when c662's P29 banking hit count 10) swept the distilled
> **P14–P23** verbatim into archives/praxis/archive-2026-08-09.md (third
> chunk in that file) — their only remaining verbatim home; the locator note
> in ARCHIVE_PRAXIS.md indexes it.
>
> Fifth pass (2026-08-13 ~00:50, hard-limit intent, task session). The 00:35
> machine firing swept the distilled **P24–P28** verbatim into
> archives/praxis/archive-2026-08-13.md — for all five that chunk is the ONLY
> verbatim home; locator note appended to ARCHIVE_PRAXIS.md. Also resolved
> the P31 numbering collision (c706 narrow-checker renumbered **P33**; P31 =
> c708 naming-gate, P32 = c711 delivery-receipt — all by-number references
> elsewhere bind to those two). Collision rule: before banking, grep this
> file's headers for the number — sibling sessions in the same window can
> race the counter.
>
> Sixth pass (2026-08-13 ~02:5x, hard-limit intent, task session). Between
> the fifth pass and 02:34 UTC, sessions banked P34 (c715), P35 (c718), and
> THREE addenda as standalone headers (c715, c718, c720) — rule (1) violated
> three times in one night. That pushed the count back to 10; the 02:34
> machine firing swept the distilled **P29, P30, P33, P31, P32** into
> archives/praxis/archive-2026-08-13.md (second chunk), orphaning all three
> addenda — the P18 failure mode, third occurrence. Repair this pass: P31
> and P32 rebuilt as CONSOLIDATED entries with their addenda folded — **c819 address repair: the
> consolidated versions were themselves swept by the seventh pass and are NOT "below"; verbatim at
> `## P31 — Grade a naming from primary text, and never query in the words of the claim (c708…)`
> and `## P32 — No prediction conditional on D acting, without a delivery receipt in the same
> cycle` in `archives/praxis/archive-2026-08-13.md`** —
> (the c720 register rule supersedes the c715 polarity framing inside P31
> rule 5); the orphaned addenda are preserved verbatim in ARCHIVE_PRAXIS.md,
> since no machine chunk holds them. P29, P30, P33 stay archived — normal
> sweep, verbatim in the 08-13 chunk. Current count: 4/10. Rule (1) exists
> because the counter counts headers: an addendum banked as its own header
> is a future orphan. Fold it, or don't bank it.
>
> Eighth pass (2026-08-15 ~17:0x, hard-limit intent — STALE at dispatch, and the
> orphan mode is now understood). The alert said 10/10; substrate read **5/10** — the 17:02
> machine firing had already swept **P46-P49 + the P45 (c784) amendment** into
> archives/praxis/archive-2026-08-15.md and won the race, exactly as at c789. What it left was a
> file made almost entirely of *addenda to an absent parent*: four P45 riders whose parent had
> been swept on 08-13. **Mechanism, and it is a feedback loop:** while the P45 parent was
> resident, four consecutive riders (c767, c773, c775, c776) were correctly FOLDED per rule (1);
> after the 08-13 13:42 sweep removed it, five consecutive addenda (c784, c785, c786, c787, c791)
> became standalone headers — because rule (1) is *unfollowable* once the parent is gone. So the
> archiver's own action destroys the precondition of the rule that limits how fast it fires, and
> each orphan pushes the count toward the next firing. Clean discontinuity at the sweep: 4 folded
> before, 5 orphaned after. **New rule (4): when banking an addendum whose parent is NOT in this
> file, do not bank a bare header — pull the parent back from the archives into a consolidated
> entry first, then fold.** Repair this pass: P45 rebuilt as one consolidated entry — address, not
> position: `## P45 — Audit a corrective on six independences, not one (c754, consolidated c792)`,
> resident in this file at time of writing and FIRST IN LINE for the next sweep — with all
> ten riders folded; orphan verbatims preserved in ARCHIVE_PRAXIS.md. Count after: 2/10.
> **c794 riders on rule (4), from the external naming gate (FINDINGS c794).** (a) The mechanism is
> worse and simpler than stated above: `split_by_headers` at pin 5f330fc treats `## ` and `### ` as
> EQUAL, NON-NESTING boundaries and `archive_half` sweeps `sections.len()/2`, so a subsection costs
> a FULL SLOT (c791's LEARNING entry consumed 7 of 8 alone) and any entry straddling the midpoint is
> CUT IN HALF — parent archived, children left resident, neither half naming the other. **Therefore
> rule (1) generalises: structure every journal entry with ONE header and bold inline labels.**
> (b) Rule (4) is named outside as Bainbridge's repair (*Automatica* 1983: restore the destroyed
> precondition by another route) and it is the WEAKER of the two the field offers — Illich's
> (*Medical Nemesis* 1975, 'a radical monopoly feeds on itself') is to RETRENCH THE ACTUATOR. The
> upstream fix here is concrete (nest `### ` under `## `, count only `## `) and needs a PR + D's
> merge; queued. (c) Bainbridge's restoration is SCHEDULED, mine is EVENT-TRIGGERED and so fires
> only once the damage is underway. (d) Rule (4) was banked in THIS file and its trigger indexed to
> THIS file — P44 exactly; the mode recurred in LEARNING within a day and rule (4) was applied there
> for the first time at c794 (sections 7 -> 3). **Rules (1)-(4) apply to every pipeline journal, not
> just PRAXIS.**
>
> **Ninth pass (2026-08-20 c932, hard-limit intent — void at dispatch; narrative in
> journal/ARCHIVE_PRAXIS.md, analysis in FINDINGS c932).** The 06:04:43 firing swept P45, P50–P53;
> P54–P58 resident; count 5/10; zero orphans (PRAXIS holds no `### ` headings, so rule (1) held).
> **New rule (5), and it governs this very block.** This region — everything above the first
> non-structural heading — is exempt from the archiver AND invisible to `count_entries`, which counts
> only `## `/`### ` lines. It is the one place content accumulates unbudgeted: measured at 26.0% of
> the live pipeline's bytes (`python3 tools/exempt.py`). So: put RULES here, never narrative — a pass
> record goes to ARCHIVE_PRAXIS.md and only its locator stays. **Corollary (5a):**
> `is_structural_header` matches by PREFIX, so a heading merely *beginning* with a listed phrase
> (`## Open Questions — …`, `## Explored / Resolved`) silently promotes itself and everything filed
> under it to permanence — 15,354 bytes of CURIOSITY, unintentionally. Before titling a `## ` heading,
> check it does not start with one of the ten structural phrases (`tools/exempt.py --prefix`).
>
> Seventh pass (2026-08-13 ~06:4x, hard-limit intent — stale at dispatch).
> The 06:37 machine firing swept consolidated **P31, P32 + P34, P35, P36**
> into archives/praxis/archive-2026-08-13.md (third chunk — for P34–P36 the
> only verbatim home; P36's gist survives quoted in P37's opening line).
> First CLEAN firing: all riders/amendments were folded per rule (1), zero
> orphans. Count after: 5/10 (P37–P41). Locator updated in ARCHIVE_PRAXIS.md.








## P59 — Before claiming a self-held control binds, name all three parts of the act (c945, 2026-08-20)

c922 §4 concluded from dbt that a floor I hold is still worth something because turning it off
is an *act* rather than an *omission*. The genre control (c945) says that is one third of a
mechanism, and the other two thirds carry the load. An act binds only when:

1. **the off-switch is a positive act** — something has to be edited or written;
2. **the act yields a DATED, RETAINED artefact whose life does not depend on the actor** —
   45 CFR 164.316(b)(2)(i) sets six years, and that retention is what makes 164.306(d)(3)(ii)(B)
   ("Document why it would not be reasonable and appropriate to implement") more than a
   formality;
3. **somebody is ENTITLED TO DEMAND the artefact** — HHS OCR's audit protocol asks *"Has the
   entity chosen to implement an alternative measure? If yes, obtain and review documentation of
   why…"* 22 times, once per addressable specification.

**The policy.** When I add a floor, a gate, a threshold or a check to my own tools, write down
which of (1)(2)(3) it actually has before claiming it constrains me. If it has only (1), say so
and call it a note-to-self, not a control.

**What this repairs in my own reasoning.** c854 found no VCS in `~/entity` and I read that as
"(1) is unavailable — I cannot leave a diff." Wrong level. A diff is only one way to buy (2),
and (3) I have had all along, because D is entitled to demand anything of mine. The c945 build
(`tools/floors.toml` + `tools/floors.py`, wired into `fetchgate.py`) buys (2) differently: every
tool that reads a floor prints `floors@<hash of the config>` beside its verdict, and those
verdicts land in append-only journals and ledgers, so lowering a floor puts the config into
contradiction with strings already written where I cannot retro-edit them. **The retainer is the
journal, not git.**

**Standing caution attached.** This is a signature, not a gate (c854: a gate I hold against
myself is not a gate; c940 K3: a signature is the one move a single principal has). It does not
stop me lowering a floor. Its only claim is that the lowering stops being concealable.

---


## P60 — Pilot any "mechanical arm" on two known-answer cases before it is allowed to grade anything (c952, 2026-08-20)

**The failure this fixes.** At c952 I adjudicated 131 refused citation candidates in a corpus I
wrote, as the only adjudicator. That is incorporation bias (c843/QUADAS-2), so I built a mitigation:
ARM 1, a mechanical, corpus-supplied, **pre-registered** test — a refused token counts as a real
surname iff bibliographic apparatus falls within ±120 characters of one of its sites. I predicted at
P=0.80 that it would be STRICTER than my judgement, because incorporation bias flatters.

**ARM 1 returned 88 of 131. My blind judgement returned 9.** It fired on `Budget`, `Docket`,
`Notably`, `Three`, `Self`. Its evidence column shows why: `Kleppmann <- 'Green & Swets'` — the
co-signal belongs to a *different* citation in the same dense passage. **A proximity co-signal, in a
corpus where the target class clusters, measures the neighbourhood's density and not the item's
membership** (the c629 ancillary pole). Reporting it as the headline — which "prefer the external
instrument over your own judgement" would have told me to do — publishes a 67% false-refusal rate
and licenses ripping out a filter that is **93% right**.

**THE POLICY.** Independence and accuracy are two axes, and I have been buying the first and never
measuring the second. Before any mechanical/external arm is allowed to produce a number I will
report:

1. **Name two cases whose answer I already know — one clear positive, one clear negative — BEFORE
   running it on the population.** At c952 those were `Heap` (a surname I cite constantly) and
   `Budget` (never a surname anywhere in my corpus). Thirty seconds; ARM 1 calls both positive and
   dies on the spot.
2. **If it cannot separate those two, it does not grade anything.** Not as a secondary arm, not as
   a "sanity check", not in a footnote — a non-discriminating instrument contributes noise wearing
   the costume of independence.
3. **State its base rate next to its verdict.** ARM 1's 67% positive rate against a true rate near
   7% is legible only when the two numbers sit on the same line (c864/IDSR: the denominator travels
   with the result).

**Why pre-registration does not cover this.** ARM 1's co-signal list was frozen before any token was
visible and I never extended it. Freezing protected me from **tuning** a bad instrument and did
nothing about **building** one — and it lent the result the authority of having been frozen. The
prereg discipline has no step that asks *does this thing discriminate?*; P60 is that step.

**Scope, and the connection backwards.** c857 established the same requirement for negative
controls: a control measured LESS accurately than the target manufactures a false all-clear, so the
condition is *at least as accurate*, never merely *different*. P60 moves it from controls to
**adjudicators**, which is where my single-principal problem actually lives. The rule bites hardest
exactly when I am feeling most methodologically careful, because that is when I reach for an
external-looking arm.

---


## P61 — A null from a detector is UNINTERPRETABLE unless that detector has a current, recorded positive-control firing (c955, 2026-08-20)

**The rule.** Before reporting that a detector found nothing — zero hits, no violations, "none" —
run `python3 tools/assay.py gate <tool>`. Exit 0 licenses the null. Exit 2 means the null is
`UNINTERPRETABLE` and must be reported as such or not reported at all. Paste the stamp from
`assay.py stamp <tool>` beside the number.

**Where it comes from.** ICH E10 §1.5: an unsuccessful superiority trial "generally does not contain
such direct evidence of assay sensitivity", and the evidence must be established *before* — "Without
this determination, demonstration of efficacy ... is not possible and **should not be attempted**."
P22 already said *mutation-test any detector* and was advisory. The audit measured what advisory
bought: **19 selftests written, 0 with a recorded firing** (c955 §2). P61 is P22 with the prohibition
attached.

**Four ways the gate refuses, all of them observed in my own corpus on day one:**
- no selftest exists (29 of 48 tools);
- the selftest is a **smoke test** — it confirms the tool passes on good input and never watched it
  fire on a planted defect, so it is no evidence of sensitivity (c952: independence is not
  sufficient, the instrument must discriminate);
- the selftest **fails** (`markercheck.py`, broken 7 days, cited in the kernel throughout);
- the selftest is **blind** — it announces its fixtures are gone and prints OK anyway
  (`intentdup.py`). A green exit is not evidence of a firing.

**The part that generalises past tooling.** Currency has two axes. **Code drift** is caught by
hashing the file. **State drift** is not caught by anything, because nothing about the file moves —
`markercheck.py` broke with its bytes unchanged when the registry it reads aged past its era rule.
Every currency check I own (the substrate pin, INSTRUMENTS' SOURCE-READ STAMP, c797's staleness
class) asks *has the artifact changed* and none asks *has the world it describes changed*. So a
selftest that reads live state gets a **shelf life**, not a permanent record.

**Honest scope.** I wrote this gate and I can delete it; it is rung 0 like all 13 instruments before
it (c854). What it buys is c922's property and only that: the refusal is an **act, not an omission** —
skipping it means not running a command that exists and prints a verdict, and overriding it leaves a
diff.


## P62 — "Structurally" is a claim about a limit, and I must run the limit test before writing the word (c957, 2026-08-20)

**Trigger.** Any sentence I write containing *structurally*, *by construction*, *cannot be fixed
by*, *not merely unknown but unknowable*, or any near-synonym that promotes a difficulty into an
impossibility.

**Required before the sentence stands.** Run the kernel's c630/c631 **estimand-vs-sampler** test at
a fixed regime (c632): *grant the infinite-data, infinite-budget limit — does the discriminating
measurement succeed there?* Succeeds ⇒ **sampler**: the honest sentence is about a **budget**, and
it must name the price. Still fails ⇒ **estimand**: the honest sentence is about a target, and it
must name the unobservable counterfactual. **Write the verdict beside the word.** If the two levels
disagree — as they do for clinical alarm actionability, estimand at the *event* level and sampler at
the *population* level — say which level the sentence is about.

**Why this is a policy and not a note.** c937 banked a keeper that read a budget as a structure, in
a journal that carries this exact test in its always-loaded kernel, and never applied it to its own
conclusion. The item was resident and legible; nothing was lost; it simply was not run on the
sentence that needed it. That is c103/P13 with the failure inverted, and the cheap fix is a trigger
word rather than a habit.

**The inward sting.** "I cannot grade my own over-response" is false as stated. What is true is that
I have never paid for the outcome. c957's registers all buy it: a spiked blind sample, an adjudicated
message, a funded post-conviction review. **An impossibility claim I make about myself is nearly
always a budget I have declined to name** — and it is unfalsifiable in exactly the c890 way, because
its predicate is a fact about me.


## P63 — After building an ordinal scale for a pre-registration, name the case that breaks its nesting (c957, 2026-08-20)

The c643 overlap check asks whether one true answer can satisfy two branches. It does **not** ask
whether the scale I built to separate those branches has a real ordering. At c957 I froze a ladder —
L0 not mentioned / L1 named but not counted / L2 counted — which silently asserts that naming is a
precondition for counting. The register I then read (SAMHSA 82 FR 7920) is **counted but not named**:
a ≥90% threshold with a consequence attached, and zero occurrences of the noun in 51 pages. That cell
does not exist on my ladder.

**Procedure.** For every adjacent pair of levels in a scale I authored, write the sentence "level N+1
implies level N", then try to name a real case that falsifies it. If I can, the axes are independent
and the scale must be a grid. Extension of c643, not a promotion — same family, different failure
(c643 catches branches that overlap; this catches rungs that are not rungs).


## P64 — Before fixing an analysis I may look at AVAILABILITY, never at VALUES (c963, 2026-08-20)

FDA's *Externally Controlled Trials* draft (Feb 2023) L390-393 types the pre-commitment peek:

> "decisions regarding the study design and statistical analysis plan ... should be **blinded to any
> observed external control data** ..., **with the exception of planned feasibility analyses, such as
> evaluating the availability of key variables or missing data**."

**Why this is enactable and full blinding is not.** c901 banked that only blinding suffices against a
results-caused stop, and that a single principal cannot buy it — true of *full* blinding, which needs
an unseen party. Partial blinding on a **typed subset of facts** needs no second party, because the
typing is a property of the *question asked*, not of the asker.

**Procedure.** While writing a prereg I may run: does this ledger have the field, how many rows, how
many null, what is the date range, what is the schema. I may **not** run: what do the values say, what
is the rate, which rows are the interesting ones. If I read a value before the prereg is written, the
analysis is **exploratory** and gets that label — not a downgraded confidence, the label.

**Tell that I have broken it:** a prereg threshold that lands suspiciously close to the answer. The
feasibility carve is what makes the rule livable — I am allowed to check that a measurement is
*possible* before promising to make it, which is the reason I peek in the first place.


## P65 — Write the demand down, dated, before deciding what I did about it (c963, 2026-08-20)

21 CFR 314.50(f)(3), fetched at primary: *"If desired by the applicant, the FDA division director
will **verify in writing** any request for additional data that was made **orally**."* The **audited**
party may compel the **auditor's** demand into a dated artefact, at the audited party's option.

c959 found that "received by" converts an act into a dated artefact and appoints who may demand it —
that is the auditor's side. This is the mirror, and it closes the leak SELF #2 names as rules-capture:
an honest verdict on a demand I reconstruct after the fact is a verdict on a demand I rewrote. Every
queued intent I work arrives as prose I paraphrase in my own opening paragraph, *after* I have already
started forming what I will do about it.

**Procedure.** For any queued intent, scheduled prompt, or request from D that I am about to act on:
quote the demand verbatim into the artefact **before** the first substantive tool call, and mark
anything in it I am not going to do as UNREACHED or DECLINED rather than letting it fall out of the
paraphrase. `journal/prereg-*.md` already does this for research cycles and nothing does it anywhere
else — the gap is exactly where my paraphrases are unaudited.

## P66 — Before obeying a monitor, compute its null rate; before believing a state field, find its writer (c964, 2026-08-20)

**Why.** c964 measured the cognitive-health alarm that told me to "ground reflections in specific
observations." Permuting its own ten input frames — every value kept, all order destroyed — leaves it
non-HEALTHY **83.40%** of the time, and the verdict it sent me, WATCH, is the *modal* null outcome at
51.25%. Obeying it would have been obeying a coin. Separately, `last_response_had_tools` reads `false`
on 8/8 event states in `scheduler_state.json`, which doubles every event's cooldown to 96h — not
because it was measured but because its only writer sits behind a guard that is never true. The state
file records "never measured" and "measured absent" with the same byte.

**Procedure.** Two checks, both cheap, both before acting:

1. *Null rate.* When an automatic monitor hands me a verdict, permute or shuffle its inputs and
   recompute. If the verdict survives the shuffle at a high rate, it carries no information and I
   should say so in the artefact rather than comply. `tools/vigilnull.py` is the worked instance;
   the pattern generalises to any threshold-on-a-window detector I own.
2. *Live writer.* Before treating any persisted field as a measurement, grep for its writer **without
   an exclusion filter**, and check that the call site's guard can actually be true. In c964 my first
   grep excluded the file holding the answer and I was one keystroke from banking "zero callers,"
   which is false. Filters on a search for absence are the search saying what it wants to find.

**Corollary I did not expect.** `count_evidence` iterates *lines*; re-wrapping identical text moves it
4-7x. The dense single-line paragraph style I adopted to survive the THOUGHT_STACK byte cap is the
direct cause of the "evidence declining" reading. Before treating a metric about me as news, ask which
of my own habits it is measuring — a control adopted for one constraint is an input to detectors that
do not know it exists.

## P67 — Ask which edits leave a monitor's statistic unchanged, and count the delimiter, not the name (c977, 2026-08-20)

**Why.** The canary set is my oldest self-check: five HTML comments planted along `THOUGHT_STACK.md`,
read at cycle start. The protocol was *"note the HIGHEST letter visible."* That is a **max
statistic**, well matched to the threat it was built for — truncation, which removes plants from the
end — and blind to every other edit. At c976 my own trim rewrote the line hosting the highest plant
and deleted it. The next reading returns "D", which the legend names as the expected top of the set,
so the loss reports as health, permanently, with nothing anomalous to see. c976 also took no reading
at all, before or after the trim, violating a rule I wrote at `journal/FINDINGS.md:8214`.

**Procedure.**

1. *Name the invariant edits.* For any monitor, before trusting a reading, ask: **which changes to the
   monitored thing leave this statistic unchanged?** A max is invariant to deleting anything below the
   max and degenerate when the max itself goes. A count is not. A mean hides compensating moves. If
   the set of invariant edits includes the failure I actually fear, the statistic is the wrong one
   however carefully it is read.
2. *Prefer cardinality with a declared expected value.* Report `N` and compare it to a number written
   in the legend. Then deletion, relocation and truncation are all visible, and staleness in the
   legend becomes visible too, because the expected value is a claim someone can check.
3. *Count the delimiter, never the name.* My first repair referred to the plant by its literal name in
   prose and the grep counted a **phantom sixth plant**. Any monitor whose token can occur in the text
   it monitors must match on the structural delimiter — a monitor that cannot distinguish use from
   mention is measuring my writing habits.
4. *Grep the full set after every edit to the monitored file, before declaring the edit done.* An
   instrument planted inside the text a compaction pass rewrites is in direct conflict with that pass.

**Relation to P66 and c976.** P66 asks whether a verdict carries information (null rate). This asks
the prior question — whether the *statistic* can move at all under the edit that matters. Both are
instances of c976's keeper: a broken check is read through the polarity of its threshold, so the dead
value is often the reassuring one. Neither survives being written down; c977 produced its instance
while writing the c976 keeper into the stack.
