# Real agents on Linux — 2026-11-05

Tracks #284. This records real heads and real workers running brgr end to end on Linux, which [the feasibility report](linux-feasibility-2026-11-03.md) left open.

## Setup

- Ubuntu 24.04.5 on x86_64, Linux 6.8, a Proxmox VM with 2 vCPUs, 2 GiB and 4 GiB of swap. The CPU type had to be `host`, because Claude Code's runtime needs SSE4.2.
- brgr built from `main`, installed and registered by the README's steps with a default home.
- Herdr 0.9.5 with a client attached inside a tmux window.
- Claude Code 2.1.318 (`haiku`), Codex CLI 0.166.0 (`gpt-6-luna`), Cursor Agent 2026.10.22-27dcf5f (`cursor-grok-4.6-high`), Devin CLI 3000.11.234 (its default model). The owner logged each one in on the VM.

`brgr harness add` activated all four, and `brgr doctor` reported `ok`.

## Rounds

Each round: the head got one prompt, started a worker that writes a file, and ended its turn. The result was sealed, the head read it, accepted it and applied it. Then the worker's pane closed and its worktree went away. The checks read the store and the pane list, not what the agents said.

| Head | Worker | Seconds | Sealed | Decision | Applied | Notice reached the head's pane | Pane and worktree gone |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Claude Code | Claude Code | 30 | candidate | accepted | yes | yes | yes |
| Claude Code | Codex (first try) | 481 | candidate | rejected | no | yes | see below |
| Claude Code | Codex (after #315) | 60 | candidate | accepted | yes | yes | yes |
| Claude Code | Cursor | 51 | candidate | accepted | yes | yes | yes |
| Claude Code | Devin | 30 | candidate | accepted | yes | yes | yes |
| Codex | Cursor | 68 | candidate | accepted | yes | yes | yes |
| Cursor | Claude Code | 37 | candidate | accepted | yes | yes | yes |

## Handoff

A Claude Code head started a Cursor worker, then ran `brgr head handoff TASK --kind codex`. The new Codex pane opened and took over. The takeover ended `resumed`, the decision was recorded from the Codex session, and the file was applied. It took 130 seconds.

The push notice did not land: it recorded 7 attempts and `parent agent is not idle`, because the new head was running `brgr watch` and stayed busy. That is what the skill allows, and the head got the result from `watch`.

## What this found

- **Codex's hook review blocked a worker.** After `brgr integrate codex install`, Codex's first run shows "Hooks need review", and no rule answered it. The head stepped in with `brgr input`, the worker's first prompt was lost, and the worker sealed a report saying it had no objective. The head read that and rejected it. The rule was added in #315. The lost prompt is #316.
- **A Codex head meets the same screen.** An owner starting Codex for the first time after `integrate codex install` has to answer it. This run's driver pressed the key.
- **Claude Code's renderer offer.** It took the prompts of an earlier run. Fixed in #305. That rule was not exercised here, because the offer appears only once.

## Not tested

- aarch64, and distributions other than Ubuntu 24.04.
- Heads and workers beyond the table. Devin was run only as a worker, since it cannot be an owner.
- Handoff in any other direction, a handoff with `--exit-old`, and an abandoned head (`head claim`).
- Tier 2 and tier 3 harnesses on Linux.
- A long turn, or a real model question from a worker.
