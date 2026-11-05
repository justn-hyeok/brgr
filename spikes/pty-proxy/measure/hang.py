#!/usr/bin/env python3
"""The proxy must end when its child does and when its outer terminal goes, even if
a grandchild keeps the inner PTY open. Usage: hang.py PROXY_BINARY [RUNS]"""
import os, pty, subprocess, sys, tempfile, time, tty

proxy = os.path.abspath(sys.argv[1])
runs = int(sys.argv[2]) if len(sys.argv) > 2 else 30
work = tempfile.mkdtemp()
ECHO = ["python3", "-c", "import os,sys,tty;tty.setraw(0)\nwhile True:\n d=os.read(0,4096)\n if b'\\x03' in d or not d: break\n os.write(1,d)"]
HOLDER = ["sh", "-c", "sleep 8 & exit 5"]
STUBBORN = ["python3", "-c", "import signal,time\nsignal.signal(signal.SIGHUP,signal.SIG_DFL)\nsignal.signal(signal.SIGTERM,signal.SIG_IGN)\ntime.sleep(60)"]

def start(cmd, i):
    m, s = pty.openpty()
    tty.setraw(s)
    p = subprocess.Popen([proxy, f"{work}/p{i}", "--", *cmd], stdin=s, stdout=s, stderr=subprocess.DEVNULL, start_new_session=True)
    os.close(s)
    return p, m

def finish(p, limit=4):
    try:
        return p.wait(limit)
    except subprocess.TimeoutExpired:
        p.kill(); p.wait()
        return "HUNG"

results = {"outer closed, then SIGTERM": [], "outer closed only": [], "child exits, grandchild holds the PTY": []}
for i in range(runs):
    p, m = start(ECHO, f"a{i}"); time.sleep(0.2)
    os.write(m, b"a"); os.read(m, 10); os.write(m, b"\x03"); os.close(m); p.terminate()
    results["outer closed, then SIGTERM"].append(finish(p))
    p, m = start(STUBBORN, f"b{i}"); time.sleep(0.2)
    os.close(m)
    results["outer closed only"].append(finish(p))
    p, m = start(HOLDER, f"c{i}"); time.sleep(0.1)
    code = finish(p)
    results["child exits, grandchild holds the PTY"].append(code)
    os.close(m)
for name, codes in results.items():
    print(f"{name}: {sum(c == 'HUNG' for c in codes)} hung of {len(codes)}; codes {sorted(set(map(str, codes)))}")
sys.exit(1 if any("HUNG" in c for c in results.values()) else 0)
