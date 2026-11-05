#!/usr/bin/env python3
"""A again, with tmux as the outer terminal: the replies come from a host that is not
Herdr. Own server socket, removed at the end. Usage: fidelity_tmux.py PROXY OUTDIR"""
import json, os, subprocess, sys, time
proxy, outdir = os.path.abspath(sys.argv[1]), os.path.abspath(sys.argv[2])
os.makedirs(outdir, exist_ok=True)
probe = os.path.join(os.path.dirname(os.path.abspath(__file__)), "term_probe.py")
def t(*a): return subprocess.run(["tmux", "-L", "pty-spike", *a], capture_output=True, text=True)
MOUSE = "\x1b[<0;10;5M\x1b[<0;10;5m"
try:
    for name in ("direct", "proxied"):
        out = f"{outdir}/{name}.json"
        for f in (out, out + ".stop"):
            if os.path.exists(f): os.remove(f)
        cmd = f"python3 {probe} {out}" if name == "direct" else f"{proxy} {outdir}/proxy -- python3 {probe} {out}"
        # An interactive shell, as in a real pane: Ctrl+C then reaches only the probe.
        t("new-session", "-d", "-s", name, "-x", "120", "-y", "40", "zsh -f")
        time.sleep(0.3)
        t("send-keys", "-t", name, "-l", f"{cmd}; echo EXIT=$?; stty -a > {out}.stty\n")
    def wait(field):
        for _ in range(200):
            try:
                if all(field in json.load(open(f"{outdir}/{n}.json")) for n in ("direct", "proxied")): return True
            except Exception: pass
            time.sleep(0.2)
        return False
    print("ready1", wait("ready1"), flush=True); time.sleep(0.5)
    screens = {n: t("capture-pane", "-p", "-t", n).stdout for n in ("direct", "proxied")}
    for n in ("direct", "proxied"):
        t("send-keys", "-t", n, "-l", "pasted line one\npasted line two\n" + MOUSE)
        for key in ("Enter", "Tab", "Up", "F5", "BSpace", "C-a", "M-x", "BTab"):
            t("send-keys", "-t", n, key); time.sleep(0.05)
        t("send-keys", "-t", n, "-l", "KEYSEND")
    print("ready2", wait("ready2"), flush=True); time.sleep(0.5)
    for n in ("direct", "proxied"):
        t("send-keys", "-t", n, "C-c"); time.sleep(0.3)
        t("resize-window", "-t", n, "-x", "100", "-y", "30"); time.sleep(0.5)
        open(f"{outdir}/{n}.json.stop", "w").close()
    time.sleep(1.5)
    end = {n: t("capture-pane", "-p", "-t", n).stdout for n in ("direct", "proxied")}
    a, b = (json.load(open(f"{outdir}/{n}.json")) for n in ("direct", "proxied"))
    skip = {"ready1", "ready2"}
    print("differ:", {k: (a.get(k), b.get(k)) for k in sorted(set(a) | set(b) - skip) if k not in skip and a.get(k) != b.get(k)})
    print("same fields:", sum(a.get(k) == b.get(k) for k in a if k not in skip), "of", len([k for k in a if k not in skip]))
    print("screen same:", screens["direct"] == screens["proxied"], " end same:", end["direct"] == end["proxied"])
    print("keys", a["keys_hex"][-120:]); print("winch", a["winch"], b["winch"], "sigint", a["sigint"], b["sigint"])
    print("end direct:", end["direct"].strip()[-60:], "| proxied:", end["proxied"].strip()[-60:])
    sd, sp = open(f"{outdir}/direct.json.stty").read(), open(f"{outdir}/proxied.json.stty").read()
    print("stty same:", sd == sp)
finally:
    t("kill-server")
