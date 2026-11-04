#!/usr/bin/env python3
"""Status agreement and screen comparison from the rows and raw streams tui.py wrote.
Usage: analyze.py DIR READ_SCREEN_BINARY ROWS COLS (the pane size before any resize)"""
import collections, difflib, json, os, subprocess, sys, tempfile
d, reader, fixed_rows, fixed_cols = sys.argv[1], os.path.abspath(sys.argv[2]), sys.argv[3], sys.argv[4]
def lines(text): return [l.rstrip() for l in text.splitlines() if l.strip()]
def ratio(a, b): return round(difflib.SequenceMatcher(None, lines(a), lines(b)).ratio(), 3)
total = collections.Counter()
for n in ("claude", "codex", "cursor"):
    rows, rep = json.load(open(f"{d}/{n}.rows.json")), json.load(open(f"{d}/{n}.report.json"))
    print(f"== {n} agents {rep['agents']} after_interrupt {rep.get('after_interrupt')} rows {len(rows)}")
    both = [x for x in rows if x["direct"] not in (None, "unknown") and x["proxied"] not in (None, "unknown")]
    print(" Herdr status, direct pane against proxied pane:", sum(x["direct"] == x["proxied"] for x in both), "of", len(both))
    live = [x for x in rows if x.get("quiet_ms") is not None and x["proxied"] not in (None, "unknown")]
    working = [x for x in live if x["proxied"] == "working"]
    resting = [x for x in live if x["proxied"] in ("idle", "done")]
    blocked = [x for x in live if x["proxied"] == "blocked"]
    print(f" same session: working {len(working)} idle/done {len(resting)} blocked {len(blocked)}; longest silence while working {max([x['quiet_ms'] for x in working], default=None)} ms")
    for q in (500, 1000, 2000, 3000, 5000):
        fp, fn = sum(x["quiet_ms"] >= q for x in working), sum(x["quiet_ms"] < q for x in resting)
        agree = len(working) + len(resting) - fp - fn
        print(f"  silent {q:>4} ms means idle: agree {agree}/{len(working)+len(resting)}, said idle while working {fp}, said working while idle {fn}")
        total[q, "fp"] += fp; total[q, "fn"] += fn; total[q, "n"] += len(working) + len(resting)
    total["working"] += len(working)
    print(" titles working", collections.Counter((x.get("title") or "")[:1] for x in working).most_common(5), "resting", collections.Counter((x.get("title") or "")[:1] for x in resting).most_common(5))
    print(" busy text: working", sum(x["busy_screen"] for x in working), "/", len(working), "resting", sum(x["busy_screen"] for x in resting), "/", len(resting))
    tags = [t for t in rep.get("snaps", {})]
    raw = open(f"{d}/{n}.raw", "rb").read() if os.path.exists(f"{d}/{n}.raw") else b""
    work = tempfile.mkdtemp()
    for tag in tags:
        a, b = open(f"{d}/{n}.{tag}.direct.txt").read(), open(f"{d}/{n}.{tag}.proxied.txt").read()
        snap = rep["snaps"][tag]; rows_, cols = fixed_rows, fixed_cols
        out = ""
        if rows_ and cols and raw and tag not in ("resized", "resized_back", "quit"):
            subprocess.run([reader, str(rows_), str(cols), f"{work}/{tag}"], input=raw[:snap["offset"]])
            v, al = open(f"{work}/{tag}.vt100.txt").read(), open(f"{work}/{tag}.alacritty.txt").read()
            out = f" vt100 {ratio(v, b)} alacritty {ratio(al, b)}"
        print(f"  {tag:18} direct~proxied {ratio(a, b)}{out}")
    settled = {}
    for x in rows:
        if x.get("ratio") is not None: settled[x["tag"]] = x["ratio"]
    print(" proxy's vt100 against Herdr at the last sample of each phase", settled)
print("all", dict((str(k), v) for k, v in total.items()))
