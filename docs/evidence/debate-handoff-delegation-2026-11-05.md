# Debate, handoff and delegation in real panes — 2026-11-05

Tracks #301, #302 and #225. Everything ran on the Linux VM described in [Real agents on Linux](linux-real-agents-2026-11-05.md), with real logged-in agents. The checks read the store, the pane list and `ps`. Nothing here ran on macOS.

## Debate

| Members | Situation | Time from question to reply | Result |
| --- | --- | --- | --- |
| Cursor, Devin | Plain debate | not recorded, because the message timestamp was broken in that build | answered; both sealed `candidate` |
| Codex, Claude Code | Claude Code holding a child, blocked in `message wait` when the debate started | 4 s | answered |
| Claude Code, Codex | Codex holding a child, blocked in `message wait` when the debate started | 13 s | answered |

The reported delay in #301 was about 8 minutes. Three more defects appeared only in these runs, and are fixed:

- A wait that began before the debate never noticed it, because membership was read once.
- A member waiting in `debate wait` never learned the group, because a busy pane cannot show the notice.
- A Codex worker had no identity of its own, because its commands run in Codex's shared daemon. It could not start a child: `brgr control home and the source workspace must not overlap`. A task directory now names the worker and its pane.

In both blocked runs the member that had a child ended `failed`, because it left its child undecided. Fixed for a child that its parent cancelled in #321; those runs were not repeated with that fix.

## Delegation

| Check | Result |
| --- | --- |
| A Claude Code worker without `--enable-delegation` runs `brgr run` | Refused, no child task in the store |
| `status --brief` while a child runs | Lists it with its parent, harness and model |
| `cancel` on a Codex parent with a Cursor child running `ping` | Receipt names both, both panes closed, `ping` gone within 3 seconds |

`ping` outlived the pane until the host's descendants were signalled. A tool that a Codex worker runs lives in Codex's shared daemon, so this cannot reach it. In one of four runs, a Codex parent had not started its child after 180 seconds; the cause was not found.

## Head handoff

Each round: a head started a Cursor worker running a 60-second command, then ran `brgr head handoff`. The incoming head took over, and decided and applied the worker's result.

| From | To | Takeover | Decision by the new head | Applied | Seconds | Push notice |
| --- | --- | --- | --- | --- | --- | --- |
| Claude Code | Codex | `resumed` | accepted | yes | 130 | not delivered, head busy in `brgr watch` |
| Codex | Claude Code | `resumed` | accepted | yes | 126 | delivered to the new head's pane |
| Cursor | Claude Code | `resumed` | accepted | yes | 127 | delivered to the new head's pane |
| Claude Code | Cursor, with `--exit-old` | `resumed` | accepted | yes | 121 | not delivered, head busy |

With `--exit-old`, the old head's pane closed. Codex to Cursor and Cursor to Codex were not run. Conditional handoff (`--auto`) was not run.

## Workflow retries

A `retries` step was not seen retrying live. A retry needs the outcome `failed`. A pane worker that was killed ended `lost` with `task pane no longer exists`, and one that wrote no report ended `lost` with `the agent finished without writing its report`. Neither retried, as documented. So in a pane workflow `retries` may rarely fire.
