#!/usr/bin/env python3
"""B: throughput and keypress-to-echo latency, direct against proxied, both read
from the same kind of outer PTY. Usage: perf.py PROXY_BINARY THROUGHPUT_BINARY"""
import os, pty, select, statistics as st, subprocess, sys, tempfile, time, tty

proxy, producer = os.path.abspath(sys.argv[1]), os.path.abspath(sys.argv[2])
work = tempfile.mkdtemp()
wrap = lambda cmd, i: [proxy, f"{work}/p{i}", "--", *cmd]

def run(cmd):
    master, slave = pty.openpty()
    tty.setraw(slave)
    p = subprocess.Popen(cmd, stdin=slave, stdout=slave, stderr=subprocess.DEVNULL, start_new_session=True)
    os.close(slave)
    return p, master

N = 16 * 1024 * 1024

def throughput(cmd, n=N):
    p, m = run(cmd)
    start, total = time.time(), 0
    try:
        while total < n:
            r, _, _ = select.select([m], [], [], 10)
            if not r: break
            try: data = os.read(m, 1 << 16)
            except OSError:
                # macOS reports EIO before the child has opened the terminal.
                if p.poll() is not None: break
                time.sleep(0.005)
                continue
            if not data: break
            total += len(data)
        elapsed = time.time() - start
    finally:
        os.close(m); p.terminate(); p.wait()
    return elapsed, total

def latency(cmd, keys=500):
    p, m = run(cmd)
    time.sleep(0.5)
    samples = []
    for i in range(keys):
        b = bytes([97 + i % 26])
        start = time.perf_counter()
        os.write(m, b)
        r, _, _ = select.select([m], [], [], 5)
        assert r, "no echo"
        os.read(m, 4096)
        samples.append((time.perf_counter() - start) * 1000)
        time.sleep(0.002)
    os.write(m, b"\x03"); os.close(m); p.terminate(); p.wait()
    samples.sort()
    return st.median(samples), samples[int(len(samples) * 0.99)], samples[-1]

ECHO = ["python3", "-c", "import os,sys,tty;tty.setraw(0)\nwhile True:\n d=os.read(0,4096)\n if b'\\x03' in d or not d: break\n os.write(1,d)"]
print("throughput 16 MiB, seconds (5 runs each)", flush=True)
for name, cmd in (("direct", [producer, str(N)]), ("proxied", None)):
    runs = []
    for i in range(5):
        c = cmd or wrap([producer, str(N)], i)
        t, total = throughput(c)
        runs.append(t)
    print(f"  {name:8} median {st.median(runs):.3f}  min {min(runs):.3f}  max {max(runs):.3f}  bytes {total}", flush=True)
print("keypress to echo, ms (500 keys, 3 runs)")
for name, cmd in (("direct", ECHO), ("proxied", None)):
    for i in range(3):
        med, p99, worst = latency(cmd or wrap(ECHO, 10 + i))
        print(f"  {name:8} median {med:.3f}  p99 {p99:.3f}  max {worst:.3f}", flush=True)
