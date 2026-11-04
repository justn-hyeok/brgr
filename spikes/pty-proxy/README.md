# PTY proxy feasibility spike

Issue #228: can a brgr-owned PTY wrap a worker's TUI without changing how it behaves, and can brgr then read its screen and tell when it is working? The results and the decision are in [`docs/evidence/pty-proxy-spike-2026-11-04.md`](../../docs/evidence/pty-proxy-spike-2026-11-04.md).

This is a test harness, not product code. Its empty `[workspace]` keeps it out of the root Cargo workspace, and nothing in `crates/` uses it.

## Build

```sh
cd spikes/pty-proxy
cargo build --release
```

- `pty-proxy-spike PREFIX -- COMMAND...` runs a command in a new PTY and passes bytes both ways unchanged. It writes `PREFIX.raw` (the output stream), `PREFIX.screen` (its emulator's screen) and `PREFIX.state` (activity and window title).
- `read-screen ROWS COLS OUT_PREFIX [--text] < STREAM` renders a stream through vt100 and alacritty_terminal.
- `throughput BYTES` writes that many bytes to stdout.

## Rerun the measurements

Run these from `spikes/pty-proxy`. Each one that uses Herdr must run inside a Herdr pane; it creates its own workspace and closes it. `tui.py` starts real harnesses, which spend a few cheap prompts per run.

```sh
P=target/release/pty-proxy-spike
python3 measure/hang.py $P 30
python3 measure/perf.py $P target/release/throughput
python3 measure/fidelity.py $P OUTDIR         # Herdr as the outer terminal
python3 measure/fidelity_tmux.py $P OUTDIR    # tmux as the outer terminal
python3 measure/tui.py claude $P OUTDIR SCRATCH_REPO claude --model haiku
python3 measure/tui.py codex  $P OUTDIR SCRATCH_REPO codex -m gpt-6-luna --yolo
python3 measure/tui.py cursor $P OUTDIR SCRATCH_REPO cursor-agent --model cursor-grok-4.6-high --force
python3 measure/analyze.py OUTDIR target/release/read-screen ROWS COLS
```

`tui.py` starts the proxy from a copy named after the harness, because Herdr reports `unknown` for a proxy under its own name. That lets Herdr's status for the proxied pane serve as the truth for the same session. `ROWS` and `COLS` are the size of a proxied pane before any resize.
