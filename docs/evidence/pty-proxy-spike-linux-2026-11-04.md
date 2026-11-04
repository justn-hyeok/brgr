# PTY proxy spike on Linux — 2026-11-04

This repeats the [macOS PTY proxy spike](pty-proxy-spike-2026-11-04.md) for #228 on Linux, the first item that report left untested. No file in `crates/` changed, and no credential file was read. The owner logged the three harnesses in on the VM.

## Decision: go on Linux too

The proxy behaves on Linux as it does on macOS. Every criterion from the macOS report was met with the same thresholds. This covers x86_64 only, and a plain terminal emulator is still untested.

| Criterion | Result | Met |
| --- | --- | --- |
| Every terminal reply, key and signal is byte-identical, direct against proxied, on two hosts | Herdr: 18 of 20 fields, and the other two are pane sizes. tmux: 20 of 20 | yes |
| All three TUIs start, take input, run a tool, get interrupted and resize through the proxy, with the direct pane's outcome | All three | yes |
| TUIs quit through the proxy | Claude Code and Cursor. Codex was not seen to quit (see below) | partly |
| The proxy ends with its child and with its terminal | 0 hung of 90 runs | yes |
| Keypress overhead stays under 5 ms at p99 | p99 0.50 to 1.09 ms | yes |
| Throughput stays above 10 MiB/s | About 36 MiB/s | yes |
| The real screen rules decide the same on emulator output | Emulator text is byte-identical to macOS for all 6 fixtures, so the macOS result carries over | yes |
| The emulator's screen matches Herdr's in live sessions | 20 of 21 snapshots exact with both emulators; the other is explained below | yes |
| Zero samples of "idle" while Herdr says `working` | 0 of 1074 | yes |

## Setup

- Ubuntu 24.04.5 on x86_64, Linux 6.8, in a Proxmox VM with 2 vCPUs and 2 GiB. The VM's CPU type was set to `host`, because Claude Code's runtime needs SSE4.2.
- Herdr 0.9.5, run headless, and tmux 3.4 as outer terminals. For the real TUIs a Herdr client was attached inside a 200x60 tmux window, which gave both panes 57 rows and 84 columns.
- Claude Code 2.1.318 (`--model haiku`), Codex CLI 0.166.0 (`-m gpt-6-luna --yolo`), Cursor Agent 2026.10.22-27dcf5f (`--model cursor-grok-4.6-high --force`).
- Two rounds of real TUIs. The first had panes of unequal size and a Claude Code menu that took the prompts, so only its idle-rule samples are counted. The second is the result below.

## Terminal behaviour

Direct and proxied gave the same replies, keys, signals and exit code on both hosts, including the mouse bytes on tmux. On Herdr, the two differing fields are the pane sizes, 40 by 119 against 18 by 117, because the panes differed from the start. `stty` differs only in that size on Herdr and is identical on tmux.

## Real TUIs

| Phase | Claude Code | Codex | Cursor |
| --- | --- | --- | --- |
| Silent 25-second shell command | ran in both | ran in both | ran in both |
| Ctrl+C during a command | interrupted in both | "Task is still running" menu in both | cancelled in both |
| Resize | redrawn | redrawn | redrawn |
| Quit to the shell prompt | yes, exit 0 in both | not reached | yes, exit 0 in both |

Codex behaves as it did on macOS: its menu stays up through further Ctrl+C presses, so it did not quit in the interrupt phase in either pane. Unlike macOS, no round on Linux had Codex quit without that menu first, so its exit through the proxy was not seen on Linux. The proxy's own exit was covered by the 90 runs of `measure/hang.py`.

Screens before a prompt were the same apart from Cursor's and Claude Code's rotating tips and notices. Later they differ in the model's wording and timing.

## Throughput and latency

| | Direct | Proxied |
| --- | --- | --- |
| 16 MiB, median | 0.138 s | 0.442 s (about 36 MiB/s) |
| Keypress to echo, median | 0.104 to 0.126 ms | 0.165 to 0.181 ms |
| Keypress to echo, p99 | 0.251 to 0.307 ms | 0.497 to 1.087 ms |
| Keypress to echo, worst | 2.1 ms | 3.3 ms |

The proxy costs more here than on the Mac, but this was a 2 vCPU VM, so it is not a direct comparison.

## Reading the screen

`read-screen` gave byte-identical text on Linux and macOS for all 12 fixture outputs (6 fixtures, 2 emulators). The real `classify` already passed on that text on macOS, and it was not rerun on Linux.

In the second round, the raw stream was replayed up to each snapshot and compared with `herdr pane read`. 20 of 21 snapshots matched exactly with both emulators. The other is Cursor's `cleared` phase, where Herdr's text still had "Press Ctrl+C again to exit" and the replay did not. That message times out at about the moment of the snapshot, so this is probably the two reads landing either side of it, though that was not confirmed. The proxy's live vt100 matched Herdr at 1.0 at the end of that phase.

After a resize, the live vt100 matched Herdr's screen at 1.0 for all three harnesses.

## Working or idle

From the second round, 1122 samples at about three a second, with Herdr's status for the same session as the truth:

| Silent for | Agreement with Herdr | Said idle while Herdr said `working` | Said working while Herdr said idle or done |
| --- | --- | --- | --- |
| 500 ms | 1081 of 1122 (96.3%) | 0 | 41 |
| 1 s | 1044 of 1122 (93.0%) | 0 | 78 |
| 2 s | 983 of 1122 (87.6%) | 0 | 139 |
| 3 s | 942 of 1122 (84.0%) | 0 | 180 |
| 5 s | 895 of 1122 (79.8%) | 0 | 227 |

Across both rounds there were 1074 `working` samples, none called idle at any threshold, and the longest silence while `working` was 427 ms. That is higher than macOS's 337 ms and still well inside the 1 second the macOS report suggests.

The other signals look as they did on macOS, with one change: busy text found 156 of 157 Claude Code `working` samples here, against 3 of 134 on macOS, in a newer version at a different width. So it should not be relied on for Claude Code.

## Other findings

- **A brgr gap, not a proxy problem.** On its first run Claude Code 2.1.318 showed "Try the new fullscreen renderer?", with `Yes, try it` selected and `Not now` below it. The real `classify` returns `Unknown` for the captured screen, so a worker on a fresh install would stop there until the three-minute deadline. Filed as #303.
- **Name of the proxy.** Herdr 0.9.5 recognized all three harnesses with the proxy named after them. The proxy under its own name was not tried here.
- **VM setup.** DNS lookups failed for the first minutes of this VM's life, and cloud-init did not install the guest agent, probably for the same reason. Neither relates to the proxy.

## Not tested

- aarch64 Linux, and distributions other than Ubuntu.
- A plain terminal emulator, on either platform.
- Codex quitting through the proxy on Linux, as above.
- The other items in the [macOS report's list](pty-proxy-spike-2026-11-04.md#not-tested): other harnesses, long or stalled turns, a click in a real TUI, live notice screens, and brgr pressing a rule's key through the proxy.
