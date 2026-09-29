# Herdr report replay regression — 2026-09-14

This is a **v1.0.4 defect reproduction and an untagged fix candidate**, not
proof that the public release already contains the fix. The actual model was
`workbuddy/deepseek-v4.1-flash` at high effort through `local.omp-herdr`.
The test owner was `codex:herdr-v104`, bound to session `herdr-v104`. The
coordinator explicitly used a clean-HEAD snapshot of the dirty task worktree;
uncommitted files were not copied into model worktrees.

Task `4caa19ed-d478-4cf3-9f7c-a4e011493121` produced:

| Revision | Binary | Result | Artifact digest | Codex decision |
|---|---|---|---|---|
| R1 | public v1.0.4 | `131cd3b5-db0e-46f7-8866-db4d2a488213` | `sha256:1311ec659982496dbadaf04c08cea14830bbba42b40c872827f498a12055e363` | Rejected: report was not exactly the required marker |
| R2 | public v1.0.4 | `df9fb59e-7a2c-40cf-b7f4-0459c35d5c07` | **the same digest as R1** | Rejected: the new marker was absent |
| R3 | patched local debug binary | `cc1d5e64-66cb-4457-9208-ba6c2af93d30` | `sha256:1ceea621c5425b4d21a53010f22571bf382ac943d9433645d756517c906ab4fd` | Accepted: new marker present, bytes verified |

R1 and R2 shared `runs/<task>.omp-report.md`. The R2 wrapper observed the
agent's startup idle and reused the already-existing R1 file as a new
candidate. A successful process exit and a new result ID therefore did **not**
prove a fresh model turn. The corrective code gives each revision a distinct
agent name and report path, rejects any pre-existing report at that path,
pins the wrapper to the admitted revision, checks prompt target, and requires
an increased Herdr lifecycle sequence with unchanged pane, terminal, and
agent-session identity before collecting a terminal report. The first agent
observation must also match the pane, terminal, and session already persisted
in brgr's spawn-ownership receipt, closing a replacement window before the
new lifecycle baseline is recorded.

The patched R3 report was written to
`runs/4caa19ed-d478-4cf3-9f7c-a4e011493121-r3.omp-report.md`.
Independent SHA-256 of its returned text matched the artifact reference. A
SQLite join found R1/R2 `rejected`, R3 `accepted`, all three inbox items
acknowledged, and every decision digest equal to its stored result digest.
Brgr's R3 pane receipt identified child `w2K:pG` and recorded `closed` after
the decision; the parent pane and worktree were retained. The task worktree
had no tracked changes (`.omp-role/` runtime metadata was untracked).

Deterministic tests cover revision-scoped report paths, refusal to reuse
existing bytes, startup idle versus a new lifecycle transition, and changed
session/pane/blocked states, including the spawn-receipt-to-first-read boundary.
The real R3 run shows the fix works for this
observed path. Final CI, an independent review, and a newly tagged unsigned
release are still required before claiming a public fix. Herdr 0.9 still
offers only ID-based pane close, so owner-scoped cleanup remains best effort.

An independent read-only WorkBuddy review found the earlier gap between the
persisted spawn receipt and the first lifecycle observation. After the
`SpawnIdentity` comparison was added, the reviewer found no further verified
P0/P1 within that focused admission scope. The review report's SHA-256 is
`ee3d2d2b647c14fdc10447d53c8ce757c8e0c349e7f7699b8a6157c1405f9e9c`;
it is retained at `/private/tmp/brgr-workbuddy-offline.wqzCcY/p0-review.md`.

The additional **post-review positive run** used the patched local v1.0.5
debug binary, not the public v1.0.4 binary. New task
`d6fd32e7-70c0-48dd-99db-9980ade9a737` requested the same WorkBuddy
model/high effort and produced candidate result
`d39341c9-36b0-42d3-bf91-27ff963019a2`, with artifact digest
`sha256:4c4a9bc17a722288db09ef3a85b6b0d5f815d7d444f89d71ed8440251db1e474`.
Its fresh report was under `runs/<task>-r1.omp-report.md`, contained the
required marker, and the task worktree had no tracked changes. Codex
independently rehashed the returned bytes, accepted the result, and verified
one acknowledged inbox item with matching decision/result digest. The
recorded brgr-owned child pane `w2K:pH` was `closed` and a subsequent Herdr
lookup returned `pane_not_found`; its parent and worktree root remained.

This confirms that the strengthened recorded-spawn comparison still permits
a real positive OMP+Herdr path. A forced live session replacement between
the two reads remains a negative-case test gap; the synthetic mismatch test
and fail-closed code path cover that boundary without mutating another user's
agent session.
