# Verification debt record — 2026-11-01

Tracks issue #225. Everything here was run on one Apple Silicon Mac with the
installed brgr 2.15.12 (release binary, checksum verified), Herdr 0.9.0, Claude
Code and Codex CLI 0.162.1 as installed on that Mac, and a scratch `BRGR_HOME`.
Nothing was run on Linux or on a second machine.

## Verified

### `brgr doctor` prints `ok` on a healthy install

Scratch `BRGR_HOME`, `CODEX_HOME` and `CLAUDE_CONFIG_DIR`; one fixture harness
(`testdata/fixtures/gjc`) registered with `brgr harness add`; `brgr integrate
codex install` and `brgr integrate claude install`. `brgr doctor` printed
`"status": "ok"` with `"problems": []` and the harness `"health": "healthy"`,
and exited 0 both piped and on a PTY. On the PTY the first line was `ok`,
followed by the JSON.

### Claude Code head handoff with `--exit-old`, in real panes

A disposable Claude Code pane acted as the outgoing head against a scratch
home. It ran one real `local.cursor-cli` worker (`cursor-grok-4.6-high`, prompt
"Reply with the single word OK."), then `brgr head handoff TASK --kind claude
--goal ... --exit-old` without deciding the result. Observed:

- a new head pane opened and received brgr's incoming prompt;
- the incoming head ran `head take`, read the result, accepted it, and ran
  `head retire`;
- the takeover row ended `retired` with `from_epoch` 1 and `committed_epoch` 2;
  `status --tree` showed the decision `accepted`;
- the outgoing head's pane was closed after the retire, the worker pane had
  been closed by the decision, and the pane that ran the test (a different
  Claude Code session) was untouched;
- a command run with the outgoing owner's identity was then refused with
  `owner ... is bound to another session`, so the command actually moved.

This is one run, Claude to Claude, with one worker.

### Workflow `retries`: a lost step is not retried

A live workflow step (real Cursor worker in a pane, `retries: 1`) was made to
end by closing its pane, and in a second run by terminating the agent process.
Both ended with outcome `lost`, `attempts: 1`, and no `KEY~retry1` step was
created. That is what the reference says: only `failed` is retried.

## Not verified

- **A step that ends `failed` in a live pane, retried and stopped at the limit.**
  Not reached: an unknown model is refused before the run starts, and closing
  the pane or killing the agent ends `lost`. The retry path for `failed` is
  covered by `workflow_e2e` with fake harnesses only.
- **Codex as the head.** Blocked: a Codex pane sharing a Codex app-server daemon
  that carries a worker's `BRGR_PARENT_*` variables cannot run `brgr` at all
  (issue #240). The handoff itself was not reached.
- **Cursor as the head.** Not attempted.
- **Conditional handoff (`set-handoff-condition --auto`) with a real usage
  signal.** Not run: the Claude Code signal needs `brgr integrate claude
  install --context`, which edits the real Claude status line, and a made-up
  signal file would not test the harness side.
- **`--exit-old` closing the outgoing pane when the outgoing head is Codex or
  Cursor.**
