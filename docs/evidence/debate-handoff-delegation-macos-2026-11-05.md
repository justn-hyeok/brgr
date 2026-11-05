# Debate, handoff and delegation on macOS — 2026-11-05

Tracks #225, #301, #302 and #306. Same rounds as [the Linux record](debate-handoff-delegation-2026-11-05.md), on this Mac (macOS, arm64) with real logged-in agents: Claude Code with Haiku, Codex with `gpt-6-luna`, Devin. Everything ran in a scratch control home and in separate Herdr workspaces. The binary was `main` plus each fix below as it was made.

Cursor was left out, because its plan was at 96% with a reset on 2026-11-07. Cursor heads and Codex to Cursor handoffs were therefore not run.

## Defects these rounds found

All are fixed.

| Symptom | Cause | Fix |
| --- | --- | --- |
| A Codex worker's child and debate commands went to the default home, invisible to the owner and outside `cancel` | Codex's shared daemon lost `BRGR_HOME` | #326, a worktree's path names its home |
| `CODEX_THREAD_ID and BRGR_SESSION_ID disagree` on a Codex worker's `brgr run`, and `report completion differs from the current worker session` on its seal | The daemon inherited another session's identity | #327 |
| A Devin child's `ping` still running 60 seconds after `cancel` | Its tools sit in their own sessions, and the process that closed the pane first could leave them | #328 |
| An automatic handoff failed with a startup timeout: `zsh: command not found: odex` | oh-my-zsh's update offer took the command's first letter | #329 |

The Linux record notes a Codex parent that had not started its child after 180 seconds, in one of four runs. The second defect above could explain it. That was not confirmed.

## Debate

| Members | Situation | Question to reply | Result |
| --- | --- | --- | --- |
| Claude Code, Devin | Plain debate | 2 s | Both `candidate` |
| Codex, Claude Code | Claude Code holding a Devin child, blocked in `message wait` when the debate started | 5 s | Both `candidate` |
| Claude Code, Codex | Codex holding a Devin child, blocked in `message wait` when the debate started | 12 s | Both `candidate` |

The two blocked runs repeat the ones that ended `failed` on Linux before #321. In the Codex, Claude Code run, Codex took 115 seconds after the debate started to send its question. The cause was not found.

## Delegation

| Check | Result |
| --- | --- |
| `status --brief` while a Codex parent's Devin child runs | Lists both, with the parent |
| `cancel` on that parent | Receipt names both, panes closed, `ping` gone within 3 seconds |
| The same with a Claude Code child | The same |

A worker refused without `--enable-delegation` was not repeated here; it is covered by tests and the Linux record.

## Head handoff

| From | To | Takeover | Decision by the new head | Seconds | Notes |
| --- | --- | --- | --- | --- | --- |
| Claude Code | Codex | `resumed` | accepted, applied | 158 | Notice delivered to the new head's pane |
| Codex | Claude Code | `resumed` | accepted, applied | 122 | The Codex head passed `--home` on its commands |
| Claude Code | Codex, with `--exit-old` | `resumed` | accepted, applied | 158 | Old pane closed about 10 seconds after the apply |

## Conditional handoff

With `set-handoff-condition --kind codex --quota-percent 70 --auto`, a Claude Code head's next `brgr status --brief` opened a Codex head, which took over and accepted the worker's result in 87 seconds. It did not apply the result, as the automatic goal does not ask it to.

The usage figure, 90%, was written to the head's signal file by the test. A signal from a real plan was not tried. The oh-my-zsh offer had been answered outside the test before this passing run, so #329's key press has only its unit test.

## Workflow retries

A `retries: 1` step with a Claude Code worker on a model that does not exist ran `die` and `die~retry1`, both sealed `failed`, and stopped at the limit (after #325). A worker that disappears still ends `lost` and is not retried; that is the Linux record.

## Test setup that went wrong

Several rounds were invalid and are not counted above. A Codex head without `--home` and the Codex defects reached the default home, leaving three test tasks there. An in-place `cp` over the running binary got it killed (exit 137). A full stop after the command in a prompt reached `head handoff`, and a command over about 1000 characters was cut at the terminal's input limit.
