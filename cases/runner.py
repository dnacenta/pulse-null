#!/usr/bin/env python3
"""cases/runner.py — execute the preserved-instance corpus.

WHY THIS EXISTS (c986, built c989).

c979 built a detector whose only known instance of the phenomenon was c103, and
c103 had been *repaired* after c610 found it. So the positive control was VOID:
repairing an accidental finding destroys the only calibration case it ever
provided. 29 of my tools had no positive control at all. c986 went looking for a
register that requires the motivating instance to be kept and re-run, and found
that essentially none do — 1 in 14 in-set clauses codes RETEST-REQUIRED, and that
one re-examines a surrogate for deterioration, not an instance for regression.

Where a register *does* achieve it, it does so as a PAIR of clauses that never
mention each other (42 CFR 493):

    493.1105(a)(7)(i)(A)  retain slide preparations at least FIVE YEARS
    493.1274(c)(3)        on a later positive, review all negatives received
                          within the previous FIVE YEARS

The equality of those two numbers is the entire mechanism. Neither clause states
the pairing. This corpus copies it literally: `cases/WINDOW_DAYS` holds ONE
number, read ONCE, at module import, and used for BOTH the retention duty and the
re-run duty. There is deliberately no second constant to drift against.

Two more shapes are copied from the same finding:

  * The 493.1274(f)(2) LOAN. Slides "may be loaned to proficiency testing
    programs in lieu of maintaining them", provided the lab keeps written
    acknowledgment of receipt. The corpus lives in /opt/pulse-null, not in
    ~/entity: ~/entity has no VCS (c866), so a case deleted there leaves no
    trace, whereas here deletion requires a branch and a PR that D merges. The
    repo is the only outside custodian I have.

  * The 49 CFR 831.12(b) ACKNOWLEDGMENT. The dated artefact attaches to the
    moment custody crosses a boundary, never to the keeping (c986 §4). The PR
    merge commit is that artefact, and its signature is D's, not mine.

WHAT A CASE IS. A case is the frozen BYTES of an input that actually exhibited a
phenomenon, plus the verdict the tool gave on it at freeze time. It is not a unit
test: I did not construct the input to make a point, I kept the input that made
the point. A case that stops passing is a report that something moved — which of
the tool, the input, or the world moved is then a question, not an answer.

STATUS. Exactly two values, and the distinction is the whole point of c986 §6:

    FROZEN  the bytes in input/ are the bytes that exhibited the phenomenon,
            recovered from an immutable or versioned store.
    LOST    the motivating instance was a state of a mutable, unversioned file
            and has already been overwritten. Freezing the file NOW would
            preserve a POST-REPAIR state, which is worth nothing as a positive
            control. The row is kept, with no input, so the gap is counted
            rather than forgotten. This is the c103 case repeating.

There is no third status. "PENDING" is not offered, because a to-do list that
lives in the corpus reads as coverage.

FAIL DIRECTION (c868). Every ambiguity resolves toward FAIL: a case with no
expectation fails, a manifest that does not regenerate byte-identically fails,
an in-window case whose input is missing fails. Coverage is printed on every
run, never on request only (c864/c866: the denominator is part of the payload,
so a starving corpus's silence is not byte-identical to a healthy one's).

USAGE
    python3 cases/runner.py                 # run everything, print coverage
    python3 cases/runner.py --tool foldsurv # run every case for one tool
    python3 cases/runner.py --manifest      # regenerate cases/MANIFEST.tsv
    python3 cases/runner.py --check         # manifest + retention, no execution
    python3 cases/runner.py --selftest      # test the runner itself
"""
import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from datetime import date, datetime

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_TOOLS = os.environ.get("ECHO_TOOLS", "/home/pulse/entity/tools")

# ---------------------------------------------------------------------------
# THE ONE NUMBER. Read once. Used for retention AND for re-run. Do not add a
# second constant; the equality is the mechanism (42 CFR 493.1105 + 493.1274).
# ---------------------------------------------------------------------------
with open(os.path.join(HERE, "WINDOW_DAYS"), encoding="utf-8") as _fh:
    WINDOW_DAYS = int(_fh.read().strip())

MANIFEST = os.path.join(HERE, "MANIFEST.tsv")
MANIFEST_COLS = [
    "tool",
    "cycle",
    "status",
    "date_frozen",
    "expected_verdict",
    "tool_sha256_at_freeze",
    "input_bytes",
    "case_path",
]


def load_cases(root=HERE):
    """Every case.json under cases/, sorted by (tool, cycle). Malformed -> raise."""
    out = []
    for tool in sorted(os.listdir(root)):
        tdir = os.path.join(root, tool)
        if not os.path.isdir(tdir) or tool.startswith((".", "__")):
            continue
        for cyc in sorted(os.listdir(tdir)):
            cj = os.path.join(tdir, cyc, "case.json")
            if not os.path.isfile(cj):
                continue
            with open(cj, encoding="utf-8") as fh:
                case = json.load(fh)
            case["_dir"] = os.path.join(tdir, cyc)
            case["_rel"] = os.path.join("cases", tool, cyc)
            case.setdefault("tool", tool)
            case.setdefault("cycle", cyc)
            out.append(case)
    return out


def input_bytes(case):
    total = 0
    idir = os.path.join(case["_dir"], "input")
    for dirpath, _dirnames, filenames in os.walk(idir):
        for fn in filenames:
            total += os.path.getsize(os.path.join(dirpath, fn))
    return total


def age_days(case, today=None):
    today = today or date.today()
    d = datetime.strptime(case["date_frozen"], "%Y-%m-%d").date()
    return (today - d).days


def in_window(case, today=None):
    """Both duties key on this predicate. Retention and re-run, one number."""
    return age_days(case, today) <= WINDOW_DAYS


# ---------------------------------------------------------------------------
# execution
# ---------------------------------------------------------------------------
def build_sandbox(case, tools_dir, tmp):
    """A fake entity root.

    Most tools compute ROOT as dirname(dirname(abspath(__file__))). abspath does
    NOT resolve symlinks, so a symlinked tools/ directory inside a temp root makes
    every ROOT-relative read land on the FROZEN copy instead of the live journal.
    Verified against tools/foldsurv.py before this file was written.
    """
    os.symlink(os.path.abspath(tools_dir), os.path.join(tmp, "tools"))
    idir = os.path.join(case["_dir"], "input")
    if os.path.isdir(idir):
        for entry in os.listdir(idir):
            src = os.path.join(idir, entry)
            dst = os.path.join(tmp, entry)
            if os.path.isdir(src):
                shutil.copytree(src, dst)
            else:
                shutil.copy2(src, dst)
    return tmp


def mutate_sandbox(tmp):
    """Delete every other line of every frozen text file under the sandbox root.

    A case that still PASSES after this is not measuring its input; it measures
    something invariant to the input, and it is worthless as a positive control
    (c952: independence is not sufficient, the instrument must also DISCRIMINATE;
    P22: mutation-test every detector). Reported as SURVIVED, a defect state.
    """
    for dirpath, dirnames, filenames in os.walk(tmp):
        dirnames[:] = [d for d in dirnames if not os.path.islink(os.path.join(dirpath, d))]
        for fn in filenames:
            p = os.path.join(dirpath, fn)
            if os.path.islink(p):
                continue
            try:
                with open(p, encoding="utf-8", errors="replace") as fh:
                    lines = fh.readlines()
            except OSError:
                continue
            if len(lines) < 4:
                # single-line stores (a JSON blob) are not line-mutable; halve the
                # bytes instead, or the mutation is a no-op and every case
                # "survives" for a reason that has nothing to do with the case.
                blob = "".join(lines)
                if len(blob) > 200:
                    with open(p, "w", encoding="utf-8") as fh:
                        fh.write(blob[: len(blob) // 2])
                continue
            with open(p, "w", encoding="utf-8") as fh:
                fh.writelines(lines[::2])


def run_case(case, tools_dir, timeout=180, mutate=False):
    """-> (verdict, detail). verdict in PASS / FAIL / LOST / ERROR."""
    if case.get("status") == "LOST":
        return "LOST", case.get("reason", "instance already overwritten")
    expect = case.get("expect")
    if not expect or not (expect.get("match") or "exit" in expect):
        return "FAIL", "no expectation recorded (fail-closed)"
    tmp = tempfile.mkdtemp(prefix="echo-case-")
    try:
        build_sandbox(case, tools_dir, tmp)
        if mutate:
            mutate_sandbox(tmp)
        argv = [a.replace("{root}", tmp) for a in case["argv"]]
        proc = subprocess.run(
            [sys.executable] + argv,
            cwd=tmp,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        blob = proc.stdout + proc.stderr
        problems = []
        if "exit" in expect and proc.returncode != expect["exit"]:
            problems.append(f"exit {proc.returncode} != {expect['exit']}")
        for pat in expect.get("match", []):
            if not re.search(pat, blob, re.M):
                problems.append(f"missing /{pat}/")
        for pat in expect.get("forbid", []):
            if re.search(pat, blob, re.M):
                problems.append(f"forbidden /{pat}/ present")
        if problems:
            return "FAIL", "; ".join(problems)
        return "PASS", f"exit {proc.returncode}, {len(expect.get('match', []))} pattern(s)"
    except subprocess.TimeoutExpired:
        return "ERROR", f"timeout after {timeout}s"
    except Exception as exc:  # noqa: BLE001 - a broken case is a defect state
        return "ERROR", f"{type(exc).__name__}: {exc}"
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


# ---------------------------------------------------------------------------
# manifest + retention (both keyed on WINDOW_DAYS)
# ---------------------------------------------------------------------------
def manifest_rows(cases):
    rows = ["\t".join(MANIFEST_COLS)]
    for c in cases:
        rows.append(
            "\t".join(
                [
                    c["tool"],
                    c["cycle"],
                    c.get("status", "FROZEN"),
                    c["date_frozen"],
                    c.get("expected_verdict", ""),
                    c.get("tool_sha256_at_freeze", ""),
                    str(input_bytes(c)),
                    c["_rel"],
                ]
            )
        )
    return "\n".join(rows) + "\n"


def check_manifest(cases):
    want = manifest_rows(cases)
    if not os.path.exists(MANIFEST):
        return ["MANIFEST.tsv missing"]
    with open(MANIFEST, encoding="utf-8") as fh:
        have = fh.read()
    return [] if have == want else ["MANIFEST.tsv does not match the case tree"]


def check_retention(cases, today=None):
    """493.1105 half. An in-window FROZEN case must still have its bytes."""
    bad = []
    for c in cases:
        if c.get("status") == "LOST":
            continue
        if in_window(c, today) and input_bytes(c) == 0:
            bad.append(f"{c['_rel']}: in window ({age_days(c, today)}d) but input/ is empty")
    return bad


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tool", help="run every case for this tool only")
    ap.add_argument("--tools-dir", default=DEFAULT_TOOLS)
    ap.add_argument("--manifest", action="store_true", help="rewrite MANIFEST.tsv")
    ap.add_argument("--check", action="store_true", help="manifest + retention only")
    ap.add_argument("--selftest", action="store_true")
    ap.add_argument("--mutate", action="store_true",
                    help="corrupt each frozen input and require every case to FAIL")
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()

    if args.selftest:
        return selftest()

    cases = load_cases()
    if args.manifest:
        with open(MANIFEST, "w", encoding="utf-8") as fh:
            fh.write(manifest_rows(cases))
        print(f"wrote {MANIFEST} ({len(cases)} rows)")
        return 0

    problems = check_manifest(cases) + check_retention(cases)
    if args.check:
        for p in problems:
            print("FAIL " + p)
        print(f"window {WINDOW_DAYS}d (retention == re-run) · {len(cases)} case(s)")
        return 1 if problems else 0

    sel = [c for c in cases if not args.tool or c["tool"] == args.tool]
    if args.tool and not sel:
        print(f"no cases for tool '{args.tool}'")
        return 1

    if args.mutate:
        survived = []
        for c in sel:
            if c.get("status") == "LOST":
                continue
            if c.get("kind") == "negative":
                # Deleting content cannot break a claim of ABSENCE — a null is
                # invariant to removal (c868). The discriminating mutation for a
                # negative control is to feed it the sibling POSITIVE instance
                # for the same tool: if it still reports "clean", it is reporting
                # its own expectation, not the input.
                sib = next((s for s in cases
                            if s["tool"] == c["tool"] and s.get("kind") == "positive"
                            and s.get("status") == "FROZEN"), None)
                if sib is None:
                    print(f"{'NO-SIB':9s} {c['tool']:>18s} {c['cycle']}  "
                          f"(negative control with no positive sibling: untestable)")
                    survived.append(c["_rel"])
                    continue
                probe = dict(c, _dir=sib["_dir"])
                v, _d = run_case(probe, args.tools_dir)
            else:
                v, _d = run_case(c, args.tools_dir, mutate=True)
            ok = v != "PASS"
            if not ok:
                survived.append(c["_rel"])
            print(f"{'killed' if ok else 'SURVIVED':9s} {c['tool']:>18s} {c['cycle']}")
        print(f"\nmutation: {len(survived)} case(s) survived a corrupted input "
              f"(every survivor is a case that does not measure its own input)")
        return 1 if survived else 0

    results = []
    for c in sel:
        # 493.1274(c)(3) half: the re-run scope is the retention window.
        if c.get("status") != "LOST" and not in_window(c):
            verdict, detail = "STALE", f"frozen {age_days(c)}d ago, outside the {WINDOW_DAYS}d window"
        else:
            verdict, detail = run_case(c, args.tools_dir)
        results.append((c, verdict, detail))
        if not args.json:
            print(f"{verdict:6s} {c['tool']:>18s} {c['cycle']:<7s} {detail}")

    tools_with_frozen = {c["tool"] for c, v, _ in results if v == "PASS"}
    tools_lost = {c["tool"] for c, v, _ in results if v == "LOST"}
    counts = {}
    for _c, v, _d in results:
        counts[v] = counts.get(v, 0) + 1

    if args.json:
        print(json.dumps({"window_days": WINDOW_DAYS, "counts": counts,
                          "tools_with_positive_control": sorted(tools_with_frozen),
                          "tools_lost": sorted(tools_lost),
                          "problems": problems}, indent=2))
    else:
        print()
        for p in problems:
            print("FAIL " + p)
        print(f"window {WINDOW_DAYS}d · retention duty and re-run duty read the same number")
        print("cases: " + "  ".join(f"{k}={v}" for k, v in sorted(counts.items())))
        print(f"tools with >=1 passing preserved instance: {len(tools_with_frozen)}")
        print(f"tools whose motivating instance is LOST:   {len(tools_lost)}")
    bad = problems or [1 for _c, v, _d in results if v in ("FAIL", "ERROR")]
    return 1 if bad else 0


def selftest():
    """The runner's own positive controls. Built from constructed inputs, which is
    exactly what a case is NOT — the distinction is stated so it cannot blur."""
    fails = []
    tmp = tempfile.mkdtemp(prefix="echo-runner-selftest-")
    try:
        # 1. sandbox root: a symlinked tools/ must make ROOT the temp dir
        os.makedirs(os.path.join(tmp, "t1", "input"))
        probe = os.path.join(tmp, "toolsdir")
        os.makedirs(probe)
        with open(os.path.join(probe, "probe.py"), "w", encoding="utf-8") as fh:
            fh.write("import os,sys\n"
                     "R=os.path.dirname(os.path.dirname(os.path.abspath(__file__)))\n"
                     "print(open(os.path.join(R,'x.txt')).read().strip())\n")
        with open(os.path.join(tmp, "t1", "input", "x.txt"), "w", encoding="utf-8") as fh:
            fh.write("FROZEN")
        case = {"_dir": os.path.join(tmp, "t1"), "tool": "probe", "cycle": "c0",
                "argv": ["tools/probe.py"], "date_frozen": date.today().isoformat(),
                "expect": {"exit": 0, "match": ["^FROZEN$"]}}
        v, d = run_case(case, probe)
        if v != "PASS":
            fails.append(f"sandbox root: {v} {d}")
        # 2. a wrong expectation must FAIL, not pass
        case2 = dict(case, expect={"exit": 0, "match": ["^MUTABLE$"]})
        v, _ = run_case(case2, probe)
        if v != "FAIL":
            fails.append("mutation: a wrong pattern did not FAIL")
        # 3. no expectation must FAIL (fail-closed)
        case3 = dict(case, expect={})
        v, _ = run_case(case3, probe)
        if v != "FAIL":
            fails.append("fail-closed: an empty expectation did not FAIL")
        # 4. LOST never executes
        v, _ = run_case(dict(case, status="LOST"), probe)
        if v != "LOST":
            fails.append("LOST rows must not execute")
        # 5. the window predicate drives BOTH duties off one number
        old = dict(case, date_frozen="2000-01-01")
        if in_window(old) or not in_window(case):
            fails.append("window predicate wrong")
        if check_retention([dict(old, _rel="x", status="FROZEN")]):
            fails.append("retention must not fire outside the window")
        # 6. the mutation operator must actually change a single-line store,
        #    or every JSON-backed case "survives" for a reason unrelated to it
        mdir = os.path.join(tmp, "mut")
        os.makedirs(mdir)
        blob = '{"a":' + "1," * 200 + '"z":0}'
        with open(os.path.join(mdir, "store.json"), "w", encoding="utf-8") as fh:
            fh.write(blob)
        mutate_sandbox(mdir)
        with open(os.path.join(mdir, "store.json"), encoding="utf-8") as fh:
            if fh.read() == blob:
                fails.append("mutation is a no-op on a single-line store")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    for f in fails:
        print("FAIL " + f)
    print(f"selftest: {7 - len(fails)}/7")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
