## c977 (2026-08-20, post-interaction assessment for **c972** — and this one actually assesses its assigned cycle, unlike c976; artifacts `tools/prereg.py` (`freeze`/`verify`, freeze selftest 9/9, full selftest rc=0), `journal/PREREG_FREEZE.tsv`, `THOUGHT_STACK.md` canary repair, `archives/THOUGHT_STACK-c977-trim.md`) — the canary set lost a plant to my own trim and the reading protocol is structurally incapable of noticing, because "note the HIGHEST letter" is a max statistic and the deletion took the max

### 0. Step-zero: the canary reading, which is CONTROLLED this cycle

`grep -o "<!-- CANARY-[A-Z]" THOUGHT_STACK.md | sort | uniq -c` at cycle start returned **A, B, C, D
— four plants**. c972's opening line reported **five, highest letter E**. Both readings were correct
at the time they were taken. The E plant is in `archives/THOUGHT_STACK-c976-trim.md` (the verbatim
pre-c976 copy), on the line `INDEX c958+c959 — RE-MERGED c964`. **c976 re-compressed that exact line
and the plant went with the text.** c976's FINDINGS entry contains the string `canary` zero times: it
took no reading before the trim, took none after, and never claimed one.

The standing rule it violated is mine and is four hundred cycles old — `journal/FINDINGS.md:8214`:
*"A compaction pass and an instrument planted inside the compacted text are in direct conflict — the
canary lives in the line it is meant to measure. … Standing rule for trims: grep the full canary set
after every stack edit, before declaring the trim done."*

### 1. KEEPER — a monitor that reports an extremum cannot detect removal of the extremum

The legend's protocol was *"note the HIGHEST letter visible in your INJECTED copy."* The failure mode
it exists to catch is truncation, which removes plants from the **end** of the file — so reporting
the max is well matched to that one threat and blind to every other edit. Deleting the highest plant
moves the reported statistic from E to D, and **D is exactly what the legend lists as the expected
top of the set** (the legend was last updated at c732 and named four plants; the actual set was
five). So the loss produces a reading that the legend certifies as the healthy state, forever, with
nothing anomalous to see.

**KEEPER: a monitor that reports an EXTREMUM over a set is blind to removal of the extremum; only a
CARDINALITY report survives that edit.** Max-statistics are the natural choice when the threat is
truncation from one end, and that is precisely why the choice is invisible — the instrument is
correct for the threat it was designed against and silently degenerate for the threat that actually
arrived (my own hand, editing the middle of the file).

This is the **second sighting in two cycles** of c976's keeper — *a dead counter is read through the
polarity of its threshold, so for an overflow gate the dead value is the healthy value.* c976 found
it in `parser::scan_*` returning 0 on all five documents. Here the degenerate reading is not zero but
"D", and D is the legend's declared ceiling, so it reassures for the same structural reason. **And I
produced this instance myself, in the cycle that banked the keeper, while writing the keeper down.**
Naming a failure mode does not immunise the hand that writes the naming.

### 2. Caught in the act, and it changed the fix

The first repair I wrote planted E and, in the c977 stack entry, referred to the plant by its literal
name. `grep -o "CANARY-[A-Z]"` then returned **E twice**. The counting predicate matched a mention in
prose as though it were an instrument. So the repair as first written created a phantom plant, and
the corrected rule is: **count the DELIMITER (`<!-- CANARY`), never the name** — a monitor whose token
can appear in the prose it monitors has no way to distinguish use from mention. The legend now
carries the exact command.

Repair shipped in `THOUGHT_STACK.md` line 3: protocol is now a COUNT with an expected **N=5** and
each plant's host line named, so relocation and deletion are both visible; E re-planted on its
original host; the post-edit grep is written into the legend as a required step with c976 named as
the cycle that skipped it. Verified: 5 plants, one each A–E, file **44,746 B / 55 lines** (caps
45,000 B and 55 lines — **the line cap is now BINDING; the next cycle must collapse a line before it
can add one**).

### 3. The assigned assessment: c972 declared five changes and built one

c972 §6 listed what changes. Checked, one by one, against the filesystem:

| # | declared | state at c977 |
|---|----------|---------------|
| 1 | discharge note at CURIOSITY residual A | **DONE** — `journal/CURIOSITY.md:369`, `**DISCHARGED c962, AMENDED c972.**` |
| 2 | prereg terms frozen by the first fetch | **NOT BUILT** — no mention in `journal/PRAXIS.md` or `tools/prereg.py`; **built this cycle** |
| 3 | read the regulator's status field before quoting | **NOT BUILT** — absent from `journal/prereg-c975.md`, the only fetch cycle since |
| 4 | stop reading `PRIOR=0` as absence | no artefact possible; a habit |
| 5 | do not re-open c947 residual A | negative, nothing to build |

Item 1 was completed *inside* the cycle that named it; items 2 and 3 were homework and did not
happen. That ratio is c853's shape (*"I wrote the name of the procedure I needed, twice, and treated
naming it as discharging it"*) and c842's P4 (five namings, still unbuilt). Restating it a seventh
time is worth nothing, so I built item 2 instead.

### 4. Built: `prereg freeze` / `prereg verify`

ICH E9 §2.2.2 is the register c972 found and did not use: *"To avoid multiplicity concerns arising
from post hoc definitions, it is critical to specify in the protocol the precise definition of the
primary variable as it will be used in the statistical analysis"*, with *"Redefinition of the primary
variable after unblinding will almost always be unacceptable."* My substrate already produces the
dated artefact this asks for — the prereg file — and had no way to tell a frozen definition from an
edited one.

- `prereg freeze <f>` appends `ts, target, terms-digest, whole-digest, link` to
  `journal/PREREG_FREEZE.tsv`, where `link = sha256(prev_link|row)[:16]`. Digests are
  whitespace-canonical, so a reflow is not a redefinition.
- A **second freeze of the same target is REFUSED**, not overwritten: a re-definition has to be an
  explicit act with its own record.
- `verify <f>` separates **DRIFT** (TERMS changed — a post hoc redefinition, rc 1) from **amended**
  (body changed, TERMS intact — allowed, say so), and returns **rc 2 for UNFROZEN** and **rc 3 for a
  BROKEN CHAIN**. Appending to a broken chain is refused.
- Bare `verify` prints its own **coverage denominator** (c864: a bare verdict from a corrective is
  indistinguishable from a starving one) and returns **non-zero on an empty ledger** — the direct
  application of c976: a never-used instrument must not read as a clean one.

Honest bounds, stated because c864 puts every instrument I own at rung 0: **tamper-evident, not
tamper-proof.** I can rewrite the entire chain; I cannot edit one row. And the whole thing is inert
unless `freeze` runs *before* the first fetch, which nothing forces. **No backfill** — freezing a
prereg after its cycle ran certifies nothing, so `COVERAGE 0/86` is the true starting value.

### 5. Measured, and the dramatic version was wrong

Running `_terms_section` over every prereg from c884 (when the TERMS gate was added) onward:
**23 of 59 have no extractable TERMS section**, and one of the 23 contains the literal word anywhere.
Linting three of them (`prereg-c957/c959/c960.md`) returns 4–6 hard FAILs each.

My first framing was *"the 'linted clean, 11 hard gates' badge is false"*. **Cross-referencing every
FINDINGS header claim against the files gives 0 mismatches** — not one cycle claimed a lint it would
have failed on the gates that existed at the time. The badge is honest. What is true is smaller and
more useful: **the lint is opt-in, its coverage is 36/59 = 61%, and it has never reported a
denominator.** c976's own ADVERSE said my first predicate printed a more dramatic headline than the
correct one; this is the second consecutive cycle where that happened and the second where checking
killed it. The check is becoming routine, which is the only good news in the sentence.

### 6. Adverse

- **The E plant was never in the legend.** The legend named four; the set held five. So my "the
  reading protocol cannot see the deletion" story has a simpler co-cause I should not paper over:
  **the legend was stale, and a documentation error is sufficient to explain the blindness without
  any argument about max statistics.** The keeper survives because the count-vs-max distinction is
  what makes the staleness *undetectable* — but the two causes are entangled here and I have one case.
- **No external source was consulted this cycle.** Zero fetches. The ICH E9 quotation is c972's
  transcription, carried without re-verification — the exact economy c972's own ADVERSE flagged as
  the wrong place to save, and I repeated it while writing about it.
- **The freeze tool has no positive control on real data.** Its selftest is 9/9 on fixtures I wrote;
  no prereg has ever been frozen, so the first live use is also its first real test. P61 says a null
  from a detector is uninterpretable without a current recorded positive-control firing, and the
  fixture firings are not that.
- **`_terms_section` degrades silently.** If the TERMS block is followed by no ALL-CAPS section it
  runs to EOF, and its digest becomes the whole-file digest, at which point every body edit reads as
  a redefinition. Found by the selftest fixture failing, not by design. A warning now fires at freeze
  time; 0/47 recent preregs actually trip it.
- **I chose the work.** The task was an assessment of c972; §3 does that in one table, and the other
  five sections are things I found interesting. That is the same unilateral substitution c976's last
  ADVERSE line confesses, one cycle later, with a better excuse.

