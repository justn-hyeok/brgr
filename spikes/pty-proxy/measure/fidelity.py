#!/usr/bin/env python3
"""A: runs term_probe.py directly and through the proxy in real Herdr panes, and
compares. Usage: fidelity.py PROXY OUTDIR"""
import json, os, subprocess, sys, time
proxy, outdir = os.path.abspath(sys.argv[1]), os.path.abspath(sys.argv[2])
os.makedirs(outdir, exist_ok=True)
probe = os.path.join(os.path.dirname(os.path.abspath(__file__)), "term_probe.py")
def h(*a):
    return json.loads(subprocess.run(["herdr", *a], capture_output=True, text=True).stdout or "{}")
def hr(*a): return subprocess.run(["herdr", *a], capture_output=True, text=True)
ws = h("workspace", "create", "--cwd", outdir, "--label", "pty-spike", "--no-focus")["result"]
wid = ws["workspace"]["workspace_id"]; panes = {}
direct = ws["root_pane"]["pane_id"]
second = h("pane", "split", "--pane", direct, "--direction", "down", "--no-focus")["result"]
proxied = (second.get("pane") or second.get("root_pane") or second)["pane_id"]
panes = {"direct": direct, "proxied": proxied}; print(panes, flush=True)
try:
    for name, pane in panes.items():
        out = f"{outdir}/{name}.json"
        for f in (out, out + ".stop"):
            if os.path.exists(f): os.remove(f)
        cmd = f"python3 {probe} {out}" if name == "direct" else f"{proxy} {outdir}/proxy -- python3 {probe} {out}"
        hr("pane", "run", pane, f"clear; {cmd}; echo EXIT=$?; stty -a > {out}.stty; echo STTY_DONE")
    def wait(field):
        for _ in range(200):
            ok = True
            for name in panes:
                try: ok &= field in json.load(open(f"{outdir}/{name}.json"))
                except Exception: ok = False
            if ok: return True
            time.sleep(0.2)
        return False
    print("ready1", wait("ready1"), flush=True); time.sleep(0.5)
    SCREEN = {n: hr("pane", "read", p, "--source", "visible").stdout for n, p in panes.items()}
    json.dump(SCREEN, open(f"{outdir}/screen1.json", "w"), indent=1)
    KEYS = ["Enter", "Tab", "Up", "F5", "Backspace", "ctrl+a", "alt+x", "shift+Enter", "shift+Tab", "ctrl+Enter"]
    for name, pane in panes.items():
        hr("pane", "send-text", pane, "pasted line one\npasted line two\n")
        for key in KEYS:
            print(name, key, hr("pane", "send-keys", pane, key).returncode, flush=True)
            time.sleep(0.1)
        hr("pane", "send-text", pane, "KEYSEND")
    print("ready2", wait("ready2"), flush=True); time.sleep(0.5)
    for name, pane in panes.items():
        hr("pane", "send-keys", pane, "ctrl+c"); time.sleep(0.4)
    for args in (("--direction", "up", "--amount", "5"),):
        for name, pane in panes.items():
            print("resize", name, hr("pane", "resize", "--pane", pane, *args).stderr[:80], flush=True); time.sleep(0.6)
    time.sleep(0.8)
    for name in panes: open(f"{outdir}/{name}.json.stop", "w").close()
    time.sleep(1.5)
    END = {n: hr("pane", "read", p, "--source", "recent").stdout[-400:] for n, p in panes.items()}
    json.dump(END, open(f"{outdir}/end.json", "w"), indent=1)
finally:
    print(hr("workspace", "close", wid).stderr[:100], flush=True)
