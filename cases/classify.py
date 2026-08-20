#!/usr/bin/env python3
"""cases/classify.py — the corpus DENOMINATOR, computed rather than remembered.

c986 §5 P8 reported the cost of a preserved-instance corpus from a hand count:
58 tools, 57 naming a motivating cycle, 34 referencing an immutable store, 6
referencing only mutable unversioned files. A hand count is not re-runnable and
c797 found that staleness — not invention — is 100% of my journal's errors. This
recomputes it, and prints the axis c986 did not have.

THE AXIS c986 MISSED. Whether an instance can be *recovered* is a different
question from whether it can be *delivered*. A tool that hardcodes
ROOT = "/home/pulse/entity" reads the live mutable journal no matter what input
you freeze, so no case can ever be built for it, however well preserved the
bytes are. Three delivery classes:

    ABSOLUTE   ROOT is a hardcoded absolute path       -> UNDELIVERABLE
    RESOLVE    ROOT via Path(...).resolve()            -> symlink sandbox fails;
                                                          deliverable ONLY if the
                                                          tool takes path args
    ABSPATH    ROOT via os.path.abspath(__file__)      -> the symlink sandbox in
                                                          runner.py works

`abspath` does not resolve symlinks and `resolve` does. That one-word difference
decides whether a tool can be held against a frozen input, and nothing in the
tool's docstring records which one it made.

Usage:  python3 cases/classify.py [--tools DIR] [--tsv OUT]
"""
import argparse
import glob
import os
import re
import sys

# Runtime state files in ~/entity. Mutable, unversioned, overwritten in place:
# an instance recorded only here is already gone (c103, c979).
MUTABLE = (
    "predictions.json", "intents.json", "tension.json", "signals.json",
    "pipeline-state.json", "pipeline-changes.jsonl", "session-state.json",
)
# Stores that keep bytes: entity archives, the write-once journal, this repo.
IMMUTABLE_HINT = ("archives/", "journal/", "ARCHIVE_", "-rotated-", "src/", ".rs")
# Journal documents are usually named by BASENAME and joined to ROOT at runtime,
# so a prefix test alone misses them. c986's hand count of 34 could not be
# reproduced without this list, which is itself the c987 shape: my own vocabulary
# drifts between the thing and the name I search for.
JOURNAL_DOCS = {
    "FINDINGS.md", "LEARNING.md", "THOUGHTS.md", "CURIOSITY.md", "PRAXIS.md",
    "REFLECTIONS.md", "LOGBOOK.md", "ATTRACTORS.md", "INSTRUMENTS.md",
    "CALLBACK.md", "THOUGHT_STACK.md", "SELF.md", "AWARENESS.md", "CLAUDE.md",
    "ASSAY.tsv", "TRIALS.tsv", "CITEFENCE.tsv", "DISCGRAM.tsv", "PROVGRAM.tsv",
    "ADVERSE_LOG.tsv", "EVICT_LOG.tsv", "CADENCE.toml", "CADENCE_LOG.tsv",
    "REOPEN_LOG.md", "PREREG_FREEZE.tsv", "PROVADJ.tsv",
}


def classify_one(path):
    src = open(path, encoding="utf-8", errors="replace").read()
    head = "\n".join(src.splitlines()[:45])
    cycles = re.findall(r"\bc(\d{2,4})\b", head)
    motivating = "c" + cycles[0] if cycles else ""

    if re.search(r'["\']/home/pulse[^"\']*["\']', src):
        root_style = "ABSOLUTE"
    elif "resolve()" in src:
        root_style = "RESOLVE"
    elif "abspath(__file__)" in src:
        root_style = "ABSPATH"
    else:
        root_style = "NONE"

    takes_path = bool(
        re.search(r'add_argument\("(?!--(?:selftest|json|quiet|verbose))[^"]*'
                  r'(file|files|dir|store|ledger|findings|preregs|known|sites|superset)', src)
        or re.search(r"sys\.argv\[1", src)
    )

    refs = set(re.findall(r'["\']([A-Za-z0-9_./-]+\.(?:md|tsv|json|jsonl|rs|toml|xml))["\']', src))
    mutable_refs = {r for r in refs if os.path.basename(r) in MUTABLE}
    immutable_refs = {
        r for r in refs
        if any(h in r for h in IMMUTABLE_HINT) or os.path.basename(r) in JOURNAL_DOCS
    }
    if immutable_refs:
        store = "IMMUTABLE"
    elif mutable_refs:
        store = "MUTABLE-ONLY"
    else:
        store = "NONE"

    # Can a frozen input be delivered to this tool at all?
    if root_style == "ABSOLUTE":
        deliverable = "NO"
    elif takes_path or root_style == "ABSPATH":
        deliverable = "YES"
    else:
        deliverable = "NO"

    return {
        "tool": os.path.basename(path),
        "motivating_cycle": motivating,
        "root_style": root_style,
        "takes_path_arg": "Y" if takes_path else "n",
        "store": store,
        "deliverable": deliverable,
        "selftest": "Y" if "selftest" in src else "n",
    }


COLS = ["tool", "motivating_cycle", "root_style", "takes_path_arg", "store",
        "deliverable", "selftest"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tools", default=os.environ.get("ECHO_TOOLS", "/home/pulse/entity/tools"))
    ap.add_argument("--tsv")
    args = ap.parse_args()

    paths = sorted(glob.glob(os.path.join(args.tools, "*.py")))
    if not paths:
        print(f"no tools under {args.tools}", file=sys.stderr)
        return 1
    rows = [classify_one(p) for p in paths]

    body = ["\t".join(COLS)] + ["\t".join(r[c] for c in COLS) for r in rows]
    text = "\n".join(body) + "\n"
    if args.tsv:
        with open(args.tsv, "w", encoding="utf-8") as fh:
            fh.write(text)

    n = len(rows)
    def cnt(k, v):
        return sum(1 for r in rows if r[k] == v)

    print(f"tools                        {n}")
    print(f"  name a motivating cycle    {sum(1 for r in rows if r['motivating_cycle'])}")
    print(f"  have a selftest            {cnt('selftest', 'Y')}")
    print(f"  reference an immutable store {cnt('store', 'IMMUTABLE')}")
    print(f"  reference ONLY mutable state {cnt('store', 'MUTABLE-ONLY')}   <- LOST class (c986 §6)")
    print(f"  reference no file at all     {cnt('store', 'NONE')}")
    print()
    print("delivery of a frozen input:")
    print(f"  ROOT hardcoded absolute    {cnt('root_style', 'ABSOLUTE')}   <- UNDELIVERABLE, whatever is preserved")
    print(f"  ROOT via resolve()         {cnt('root_style', 'RESOLVE')}   (symlink sandbox defeated)")
    print(f"  ROOT via abspath(__file__) {cnt('root_style', 'ABSPATH')}   (sandbox works)")
    print(f"  DELIVERABLE                {cnt('deliverable', 'YES')} of {n}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
