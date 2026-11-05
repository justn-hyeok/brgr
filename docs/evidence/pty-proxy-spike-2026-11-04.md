# PTY proxy feasibility spike — 2026-11-04

Issue #228 asks whether brgr can wrap a worker's TUI in its own PTY proxy, without changing how the TUI behaves, and read its screen and state itself. This is what a brgr TUI on a plain terminal would rest on. The test harness is in [`spikes/pty-proxy/`](../../spikes/pty-proxy/), outside the root Cargo workspace. No file in `crates/` changed, no credential file was read, and no harness was registered.

## Decision: go, on macOS

A transparent proxy works for Claude Code, Codex and Cursor. No difference caused by the proxy was found in terminal behaviour, and silence on the output stream never called a working agent idle.

This decides feasibility, not scope. Linux and a plain terminal emulator were not tested, and they must be tested first in any implementation (see [Not tested](#not-tested)). Whether this work goes into v3 is the owner's decision (#227).

An earlier draft of this report said no-go. It rested on a proxy with no raw mode and on no real TUI run, so it was discarded and everything below was measured again.

| Criterion, set after the first measurements | Result | Met |
| --- | --- | --- |
| Every terminal reply, key and signal is byte-identical, direct against proxied, on two hosts | Herdr: 18 of 20 fields, and the other two differ only because the panes share a resize. tmux: 20 of 20 | yes |
| All three TUIs start, take input, run a tool, get interrupted, resize and quit through the proxy | All three, with the same outcome as the direct pane | yes |
| The proxy ends with its child and with its terminal | 0 hung of 90 runs | yes |
| Keypress overhead stays under 5 ms at p99 | p99 0.08 to 0.54 ms | yes |
| Throughput stays above what a TUI produces (over 10 MiB/s) | About 95 MiB/s | yes |
| The real screen rules decide the same on emulator output as on the captured text | 6 of 6 fixtures with both emulators | yes |
| The emulator's screen matches Herdr's in live sessions | 21 of 21 snapshots with both emulators, before any resize | yes |
| Zero samples of "idle" while Herdr says `working` | 0 of 764 | yes |

## Setup

- macOS on Apple Silicon. Herdr 0.9.0 and tmux 3.6a as outer terminals.
- Claude Code 2.1.293 (`--model haiku`), Codex CLI 0.166.0 (`-m gpt-6-luna --yolo`), Cursor Agent 2026.10.22-27dcf5f (`--model cursor-grok-4.6-high --force`).
- vt100 0.16 and alacritty_terminal 0.25. wezterm-term was not tried, because the other two left nothing for it to settle.
- Each harness ran direct and proxied side by side in one Herdr workspace, in a scratch git repository, with about 10 prompts per harness. Workspaces were closed afterwards.

## Terminal behaviour

`measure/term_probe.py` ran direct and proxied in the same host and recorded what the terminal answered and what reached the program.

| Scenario | Herdr | tmux |
| --- | --- | --- |
| DA1, DA2, XTVERSION, cursor position | same | same |
| OSC 10 and 11 colour queries | same | same (tmux gave no reply to either) |
| OSC 52 clipboard query, XTGETTCAP | same (no reply to either) | same (no reply to either) |
| DECRPM for paste, mouse and synchronized output | same | same |
| Kitty keyboard query | same | same (no reply to either) |
| Multi-line paste | same | same |
| Keys, including Shift+Enter (`\x1b[13;2u`), Ctrl+Enter, Alt+x | same | same, without Shift+Enter and Ctrl+Enter, which `send-keys` was not given |
| SGR mouse event bytes as input | not sent | same |
| Truecolor, alt screen, mouse modes on and off | same visible screen and exit | same visible screen and exit |
| Ctrl+C | reaches the child once; same | same |
| Resize | the child gets SIGWINCH | same, including final size |
| `stty` after exit | differs only in the row count | same |
| Exit code | 7 in both | 7 in both |

The two Herdr panes share their resize, so one ended at 4 rows and the other at 52. That accounts for the two differing Herdr fields and the `stty` difference.

## Real TUIs

| Phase | Claude Code | Codex | Cursor |
| --- | --- | --- | --- |
| Trust prompt, answered by key | same | same | same |
| Bracketed paste, then cleared | same | same | same |
| Silent 25-second shell command | ran | ran | ran |
| Ctrl+C during a command | idle in both | "Task is still running" menu in both | cancelled in both |
| Resize | redrawn | redrawn | redrawn |
| Quit, shell prompt restored | yes | yes, in the round with no interrupt | yes |

Direct and proxied screens are the same in the phases before a prompt, apart from Cursor's random tip line. They then differ only in the model's own wording, turn timing and random words such as "Baked" and "Crunched". After a resize the two panes have different widths, so they can't be compared with each other. There the proxy's emulator was compared with Herdr's screen instead, and they matched.

Codex's "Task is still running" menu stayed up through further Ctrl+C presses in both panes, and Herdr reported it as `working` in both, because the command under it was still running. So Codex did not quit in the interrupt round, and closing the workspace left its 120-second `ping` behind, which was stopped by hand. Which pane it came from was not recorded. Codex quit normally to the shell prompt in the earlier round, which had no interrupt.

## Throughput and latency

From an outer PTY, 16 MiB with 5 runs and 500 keypresses with 3 runs:

| | Direct | Proxied |
| --- | --- | --- |
| 16 MiB, median | 0.112 s | 0.169 s (about 95 MiB/s) |
| Keypress to echo, median | 0.035 to 0.060 ms | 0.033 to 0.074 ms |
| Keypress to echo, p99 | 0.128 to 0.279 ms | 0.079 to 0.369 ms |
| Keypress to echo, worst | 2.3 ms | 1.7 ms |

An earlier run of the same proxy, without the raw log, gave p99 up to 0.54 ms and a worst case of 12.6 ms, which is the figure to plan on. The two paths are close enough that the keypress runs overlap.

## Reading the screen

The real `classify` and `codex_trust` from `pane_adapter/screens.rs` ran on the emulator output through a temporary test, which is not in this change. All six files in `testdata/fixtures/screens/` got the same decision as the captured text, with both emulators.

For live sessions, the raw stream was replayed up to each snapshot and compared with `herdr pane read`, ignoring blank lines. All 21 snapshots (7 phases for each of 3 harnesses) matched exactly with both emulators. These snapshots are all from before the resize, because a replay at one fixed size can't follow one. After a resize, the proxy's live vt100 matched Herdr's screen at 1.0 for all three harnesses.

**Recommendation: vt100.** Both emulators gave the same text, and vt100 has about 21 lines in its dependency tree against 55 for alacritty_terminal.

## Working or idle

The proxy's process name matters. Herdr reported `unknown` for a proxy named `pty-proxy-spike` and reported normally for the same binary named after the harness, so it appears to recognize an agent by its foreground process name. This made Herdr's status available as the truth for the same session.

The rule tested is: the agent is idle if the output stream has been silent for a given time. It was sampled about three times a second in the last round, 894 samples across all three harnesses.

| Silent for | Agreement with Herdr | Said idle while Herdr said `working` (the dangerous case) | Said working while Herdr said idle or done |
| --- | --- | --- | --- |
| 500 ms | 867 of 894 (97.0%) | 0 | 27 |
| 1 s | 840 of 894 (94.0%) | 0 | 54 |
| 2 s | 799 of 894 (89.4%) | 0 | 95 |
| 3 s | 770 of 894 (86.1%) | 0 | 124 |
| 5 s | 731 of 894 (81.8%) | 0 | 163 |

The longest silence seen while `working` was 337 ms across both rounds, 764 `working` samples, including silent 25-second shell commands: all three TUIs animate a spinner throughout. A longer threshold only adds delay after a turn ends. The disagreements that remain are of the safe kind, since brgr presses nothing while it thinks an agent is working.

The other signals are weaker:

- **Window title:** it separates working from idle on Claude Code (`◐◑` against `✳`) and Codex (braille spinner against none), but not on Cursor.
- **Busy text on screen:** no false positives, but it misses most of Claude Code's `working` samples (3 of 134 found), against Codex 133 of 217 and Cursor 143 of 147.
- **`blocked`:** silence can't tell a waiting menu from idle. That needs the screen rules, as now. All 19 `blocked` samples were Codex's trust menu, which a rule answers.

## A bug the spike found

The proxy hung within a few runs when its outer terminal closed: the output thread ended on the write error, the main loop stopped, and the proxy then waited for a child that it no longer signalled. It also ignored SIGTERM in that state. This stalled a first measurement for four hours.

A real implementation must follow what the spike now does, and `measure/hang.py` tests it: the child's exit ends the run, not the reader. When the terminal goes, the proxy keeps draining output and sends SIGHUP to the child's process group. After the child exits, it waits a short time for remaining output, because a grandchild can hold the PTY open.

## Not tested

- **Linux.** Everything above is macOS. #229 and #289 both came from Linux differences, so this is the first thing to run.
- **A plain terminal emulator.** The outer terminal was Herdr or tmux, because those can be driven by a script. The proxy passes replies through from whatever host it has, but Ghostty, iTerm2 and Terminal.app were not driven directly.
- **Other harnesses.** Copilot, OpenCode, Gemini and others were not run.
- **Long or stalled turns.** Turns lasted under a minute. Whether a spinner keeps moving through a network stall is not known, so the silence rule needs watching on long runs. A spinner that froze would produce the dangerous case.
- **Mouse in a real TUI.** Mouse bytes passed through in the probe; no TUI was clicked.
- **Update and other notice screens live.** They were covered only by the fixture and rule tests.
- **The rule-driven key press itself.** No screen was answered by brgr through the proxy. The harness pressed the keys.

Herdr's status is used as the truth here, and it can itself be wrong.

## What an implementation should do

1. Run these same measurements on Linux before building anything.
2. Use the proxy only where no controlling host exists. Under Herdr, keep today's adapter: wrapping adds nothing there and costs Herdr's recognition unless the name trick is used.
3. Use vt100 for the screen, the existing `screens.rs` rules for dialogs, and at least 1 second of silence for idle. Press nothing unless both agree.
4. Treat `measure/hang.py` as the first regression test.

If Linux or a plain terminal fails, the issue's fallback still stands: controlled hosts such as tmux and cmux get the TUI, and a plain terminal gets `--headless` only.
