#!/usr/bin/env python3
"""C, D, E: one real TUI, direct and through the proxy, side by side in real Herdr
panes. Usage: tui.py NAME PROXY OUTDIR SCRATCH COMMAND..."""
import difflib, json, os, re, shutil, subprocess, sys, time
name, proxy, outdir, scratch = sys.argv[1], os.path.abspath(sys.argv[2]), os.path.abspath(sys.argv[3]), os.path.abspath(sys.argv[4])
command = " ".join(sys.argv[5:])
os.makedirs(outdir, exist_ok=True)
# Herdr recognizes an agent by its foreground process name, so the proxy carries the
# harness's name. That makes Herdr's status a same-session truth for the proxied run.
os.makedirs(f"{outdir}/alias-{name}", exist_ok=True)
named = f"{outdir}/alias-{name}/{os.path.basename(sys.argv[5])}"
shutil.copy2(proxy, named)
def h(*a):
    out = subprocess.run(["herdr", *a], capture_output=True, text=True)
    try: return json.loads(out.stdout)
    except Exception: return {}
def text(pane):
    return subprocess.run(["herdr", "pane", "read", pane, "--source", "visible"], capture_output=True, text=True).stdout
def status(pane):
    return (h("pane", "get", pane).get("result") or {}).get("pane", {}).get("agent_status")
def agent(pane):
    for item in (h("agent", "list").get("result") or {}).get("agents", []):
        if item.get("pane_id") == pane:
            return item.get("agent")
ws = h("workspace", "create", "--cwd", scratch, "--label", f"pty-spike-{name}", "--no-focus")["result"]
wid = ws["workspace"]["workspace_id"]; direct = ws["root_pane"]["pane_id"]
second = h("pane", "split", "--pane", direct, "--direction", "right", "--no-focus")["result"]
proxied = (second.get("pane") or second.get("root_pane") or second)["pane_id"]
panes = {"direct": direct, "proxied": proxied}
prefix = f"{outdir}/{name}"
report = {"panes": panes, "command": command}
offsets = {}
def snap(tag):
    for n, p in panes.items(): open(f"{prefix}.{tag}.{n}.txt", "w").write(text(p))
    # Where this screen ends in the raw stream, for replaying it through each emulator.
    offsets[tag] = os.path.getsize(prefix + ".raw") if os.path.exists(prefix + ".raw") else 0
    try: state = json.load(open(prefix + ".state"))
    except Exception: state = {}
    report.setdefault("snaps", {})[tag] = {"offset": offsets[tag], "rows": state.get("rows"), "cols": state.get("cols")}
def say(*a): print(name, *a, flush=True)
BUSY = re.compile(r"esc to interrupt|ctrl\+c to (?:interrupt|stop|cancel)|esc to cancel|Thinking|Generating", re.I)
def sample(seconds, tag, rows):
    end = time.time() + seconds
    while time.time() < end:
        row = {"t": round(time.time(), 1), "tag": tag}
        for n, p in panes.items(): row[n] = status(p)
        screen = text(proxied); row["busy_screen"] = bool(BUSY.search(screen))
        try:
            st = json.load(open(prefix + ".state"))
            row["quiet_ms"], row["bytes"], row["title"] = st["quiet_ms"], st["bytes"], st["title"]
            emu = open(prefix + ".screen").read()
            row["ratio"] = round(difflib.SequenceMatcher(None, [l.rstrip() for l in emu.splitlines() if l.strip()], [l.rstrip() for l in screen.splitlines() if l.strip()]).ratio(), 3)
        except Exception: pass
        rows.append(row); time.sleep(0.25)
rows = []
try:
    h("pane", "run", direct, f"cd {scratch} && clear && {command}; echo TUI_EXIT=$?")
    h("pane", "run", proxied, f"cd {scratch} && clear && {named} {prefix} -- {command}; echo TUI_EXIT=$?")
    sample(6, "start", rows); snap("start")
    for attempt in range(4):
        pressed = False
        for n, p in panes.items():
            s = text(p).lower()
            if "trust this workspace" in s and "trusting workspace" not in s:
                say("answering Cursor's trust prompt in", n); h("pane", "send-keys", p, "a"); pressed = True
            elif "trust and continue" in s:
                say("answering Codex's trust menu in", n); h("pane", "send-keys", p, "Enter"); pressed = True
            elif "try the new fullscreen renderer" in s:
                say("declining Claude's renderer offer in", n); h("pane", "send-keys", p, "Down"); h("pane", "send-keys", p, "Enter"); pressed = True
            elif "yes, i trust this folder" in s:
                # Claude Code's menu starts on "No, exit".
                say("answering Claude's trust menu in", n); h("pane", "send-keys", p, "Down"); h("pane", "send-keys", p, "Enter"); pressed = True
            elif re.search(r"do you trust|trust the (contents|files)|do you want to work in this folder|yes, proceed|press enter to continue", s):
                say("answering a start screen in", n); h("pane", "send-keys", p, "Enter"); pressed = True
        if not pressed: break
        sample(4, "trust", rows)
    snap("ready")
    report["agents"] = {n: agent(p) for n, p in panes.items()}; say("agents", report["agents"])
    paste = "\x1b[200~first pasted line\nsecond pasted line\x1b[201~"
    for p in panes.values(): h("pane", "send-text", p, paste)
    sample(2, "paste", rows); snap("paste")
    for p in panes.values():
        h("pane", "send-keys", p, "ctrl+u"); h("pane", "send-keys", p, "ctrl+c")
    sample(2, "clear", rows); snap("cleared")
    LONG = "Run this exact shell command with your shell tool and wait for it: ping -c {} 127.0.0.1 > /dev/null. Then reply with the single word DONE."
    def ask(prompt):
        for p in panes.values():
            h("pane", "send-text", p, prompt); time.sleep(0.3); h("pane", "send-keys", p, "Enter")
    ask(LONG.format(25))
    sample(60, "tool", rows); snap("tool")
    ask(LONG.format(120))
    sample(15, "before_interrupt", rows); snap("before_interrupt")
    for p in panes.values(): h("pane", "send-keys", p, "ctrl+c")
    sample(10, "after_interrupt", rows); snap("after_interrupt")
    report["after_interrupt"] = {n: status(p) for n, p in panes.items()}
    h("pane", "resize", "--pane", direct, "--direction", "right", "--amount", "10")
    sample(3, "resize", rows); snap("resized")
    h("pane", "resize", "--pane", direct, "--direction", "left", "--amount", "10")
    sample(3, "resize_back", rows); snap("resized_back")
    for p in panes.values(): h("pane", "send-keys", p, "ctrl+c")
    time.sleep(0.6)
    for p in panes.values(): h("pane", "send-keys", p, "ctrl+c")
    sample(4, "quit", rows); snap("quit")
    report["still_running"] = {n: agent(p) for n, p in panes.items()}
finally:
    json.dump(rows, open(prefix + ".rows.json", "w")); json.dump(report, open(prefix + ".report.json", "w"), indent=1)
    h("workspace", "close", wid); say("closed", wid)
