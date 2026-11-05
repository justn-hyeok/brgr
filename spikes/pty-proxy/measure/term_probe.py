#!/usr/bin/env python3
"""Runs inside a terminal: records what the terminal answers and what reaches the
program. Usage: term_probe.py OUT.json. It stops when OUT.stop appears."""
import json, os, select, signal, struct, fcntl, sys, termios, time, tty
out = sys.argv[1]
result, winch, sigint = {}, [], []
def size():
    rows, cols, _, _ = struct.unpack("hhhh", fcntl.ioctl(0, termios.TIOCGWINSZ, b"\0" * 8))
    return [rows, cols]
signal.signal(signal.SIGWINCH, lambda *_: winch.append(size()))
signal.signal(signal.SIGINT, lambda *_: sigint.append(time.time()))
saved = termios.tcgetattr(0)
def ask(query, wait=0.8):
    while select.select([0], [], [], 0)[0]: os.read(0, 4096)
    os.write(1, query)
    data, deadline = b"", time.time() + wait
    while time.time() < deadline:
        if select.select([0], [], [], 0.05)[0]: data += os.read(0, 4096)
    return data.hex()
result["size"] = size()
result["term"] = os.environ.get("TERM"); result["colorterm"] = os.environ.get("COLORTERM")
result["termprog"] = os.environ.get("TERM_PROGRAM")
tty.setraw(0)
for name, query in {
    "da1": b"\x1b[c", "da2": b"\x1b[>c", "xtversion": b"\x1b[>0q",
    "osc10": b"\x1b]10;?\x07", "osc11": b"\x1b]11;?\x07", "osc52": b"\x1b]52;c;?\x07",
    "kitty_query": b"\x1b[?u", "cursor_pos": b"\x1b[5;7H\x1b[6n",
    "decrpm_paste": b"\x1b[?2004$p", "decrpm_mouse": b"\x1b[?1006$p", "decrpm_sync": b"\x1b[?2026$p",
    "xtgettcap_RGB": b"\x1bP+q524742\x1b\\",
}.items():
    result[name] = ask(query)
def checkpoint(): json.dump(result, open(out + ".tmp", "w"), indent=1); os.replace(out + ".tmp", out)
os.write(1, b"\x1b[2J\x1b[H\x1b[?2004h\x1b[>1u\x1b[38;2;12;34;56mtruecolor\x1b[0m READY1\r\n")
result["ready1"] = time.time(); checkpoint()
keys = b""; deadline = time.time() + 25
while time.time() < deadline and b"KEYSEND" not in keys:
    if select.select([0], [], [], 0.05)[0]: keys += os.read(0, 4096)
result["keys_hex"] = keys.hex()
os.write(1, b"\x1b[<u\x1b[?2004l\x1b[?1049h\x1b[Halt-screen\x1b[?1049l\x1b[?1000h\x1b[?1006h\x1b[?1000l\x1b[?1006l")
termios.tcsetattr(0, termios.TCSADRAIN, saved)
os.write(1, b"READY2\n")
result["ready2"] = time.time(); checkpoint()
while not os.path.exists(out + ".stop"):
    time.sleep(0.05)
result["winch"] = winch; result["sigint"] = len(sigint); result["size_end"] = size()
checkpoint()
sys.exit(7)
