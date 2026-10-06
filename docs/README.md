# Documentation

Four directories, split by how each kind of document ages.

| Directory | Holds | Changes |
| --- | --- | --- |
| [`guides/`](guides/) | how brgr works and how to work with it | edited as the code changes |
| [`releases/`](releases/) | what shipped in each version | written once per release |
| [`evidence/`](evidence/) | dated records of what was observed | never edited after the date in the name |
| [`readiness/`](readiness/) | the checklist the project is held to | edited as gates open and close |

The split matters because the three kinds cannot be read the same way. A guide
that disagrees with the code is a bug; an evidence record that disagrees with
the code is simply older than it, and rewriting one destroys the thing it was
for. Every relative link below is resolved by
[`crates/brgr-cli/tests/docs_links.rs`](../crates/brgr-cli/tests/docs_links.rs),
so moving a file breaks a test rather than a reader.

## Guides

- [Reference](guides/reference.md) — the detailed contract moved out of the
  README: plugin details, routes, pane mode, recursive workers, the completion
  loop, worktree cleanup, and development.
- [Architecture](guides/architecture.md) — the managed-run contract: task
  identity, attempts, sealed results, and who may decide.
- [Agent-authored process manifests](guides/custom-harness-registration.md) —
  registering a new CLI harness, and the evidence that registration requires.
- [v2 Herdr plugin contract](guides/v2-herdr-plugin.md) — ownership, context,
  recovery, and the boundaries the plugin may not cross.
- [Recursive worker bridge](guides/recursive-workers.md) — delegation between
  workers, and the verified scope of it.
- [Completion loop plan](guides/completion-loop-plan.md) — how a run reaches a
  terminal state and what reconciles one that did not.
- [Orchestration](guides/orchestration.md) — native TUI defaults, messages,
  debate, settings, and verification limits.
- [Orchestration rework plan](guides/orchestration-rework-plan.md) — the accepted
  implementation scope and its verification criteria.
- [Unsigned macOS distribution](guides/unsigned-distribution.md) — what a
  release archive is and is not, and what to check before running one.

## Readiness

- [v1 readiness checklist](readiness/v1-readiness-checklist-2026-09-14.md) — the
  gates, their current state, and the dated records of each review round.

## Releases

Newest first. [v2.13.2](releases/v2.13.2.md) · [v2.13.1](releases/v2.13.1.md) · [v2.13.0](releases/v2.13.0.md) · [v2.12.16](releases/v2.12.16.md) · [v2.12.15](releases/v2.12.15.md) · [v2.12.14](releases/v2.12.14.md) · [v2.12.13](releases/v2.12.13.md) · [v2.12.12](releases/v2.12.12.md) · [v2.12.11](releases/v2.12.11.md) · [v2.12.10](releases/v2.12.10.md) · [v2.12.9](releases/v2.12.9.md) · [v2.12.8](releases/v2.12.8.md) · [v2.12.7](releases/v2.12.7.md) · [v2.12.6](releases/v2.12.6.md) · [v2.12.5](releases/v2.12.5.md) · [v2.12.4](releases/v2.12.4.md) · [v2.12.3](releases/v2.12.3.md) · [v2.12.2](releases/v2.12.2.md) · [v2.12.1](releases/v2.12.1.md) · [v2.12.0](releases/v2.12.0.md) · [v2.11.0](releases/v2.11.0.md) · [v2.10.6](releases/v2.10.6.md) · [v2.10.5](releases/v2.10.5.md) · [v2.10.4](releases/v2.10.4.md) · [v2.10.3](releases/v2.10.3.md) · [v2.10.2](releases/v2.10.2.md) · [v2.10.1](releases/v2.10.1.md) · [v2.10.0](releases/v2.10.0.md) · [v2.9.3](releases/v2.9.3.md) · [v2.9.2](releases/v2.9.2.md) · [v2.9.1](releases/v2.9.1.md) · [v2.9.0](releases/v2.9.0.md) · [v2.8.0](releases/v2.8.0.md) · [v2.7.1](releases/v2.7.1.md) · [v2.7.0](releases/v2.7.0.md) · [v2.6.1](releases/v2.6.1.md) · [v2.6.0](releases/v2.6.0.md) · [v2.5.0](releases/v2.5.0.md) · [v2.4.0](releases/v2.4.0.md) · [v2.3.1](releases/v2.3.1.md) ·
[v2.3.0](releases/v2.3.0.md) · [v2.2.2](releases/v2.2.2.md) ·
[v2.2.1](releases/v2.2.1.md) · [v2.2.0](releases/v2.2.0.md) ·
[v2.1.0](releases/v2.1.0.md) · [v2.0.2](releases/v2.0.2.md) ·
[v2.0.1](releases/v2.0.1.md) · [v2.0.0](releases/v2.0.0.md) ·
[v1.0.9](releases/v1.0.9.md) · [v1.0.8](releases/v1.0.8.md) ·
[v1.0.7](releases/v1.0.7.md) · [v1.0.6](releases/v1.0.6.md) ·
[v1.0.5](releases/v1.0.5.md) · [v1.0.4](releases/v1.0.4.md) ·
[v1.0.3](releases/v1.0.3.md) · [v1.0.2](releases/v1.0.2.md) ·
[v1.0.1](releases/v1.0.1.md) · [v1.0.0](releases/v1.0.0.md)

The running log is [`CHANGELOG.md`](../CHANGELOG.md); these are the per-version
notes that accompanied each tag.

## Evidence

Dated records of runs that were actually performed. The date in the filename is
when it was observed, and none of these is updated afterwards.

**Live harness runs.**
[Four real-harness Luna runs](evidence/live-four-harness-luna-evidence-2026-09-14.md) ·
[minimum effort](evidence/live-four-harness-luna-min-2026-09-14.md) ·
[v1.0.4](evidence/live-four-harness-luna-v1.0.4-evidence-2026-09-14.md) ·
[v1.0.6 process-harness matrix](evidence/live-v1.0.6-luna-matrix-2026-09-14.md) ·
[Devin CLI v2.0.2](evidence/live-devin-cli-v2.0.2-2026-09-15.md)

**Route observation and OMP.**
[WorkBuddy OMP process](evidence/live-workbuddy-omp-process-2026-09-14.md) ·
[native route and downgrade](evidence/live-workbuddy-route-observation-2026-09-14.md) ·
[callback migration audit](evidence/omp-callback-migration-audit-2026-09-14.md) ·
[callback 3-1 reconciliation](evidence/omp-callback-reconciliation-2026-09-21.md)

**Herdr plugin and bridge.**
[Ordinary pane to worker](evidence/live-ordinary-herdr-pane-2026-09-23.md) ·
[recursive bridge](evidence/live-recursive-bridge-2026-09-23.md) ·
[bidirectional bridge](evidence/live-bidirectional-bridge-2026-09-23.md) ·
[report replay regression](evidence/herdr-revision-replay-evidence-2026-09-14.md) ·
[pane-close criterion](evidence/pane-close-criterion-decision-2026-09-21.md)

**Gates.**
[2-3 public binary rerun](evidence/gates-public-binary-rerun-2026-09-21.md) ·
[2-4 clean-environment procedure](evidence/gate-24-cleanmac-procedure-2026-09-21.md) ·
[2-4 clean-environment evidence](evidence/gate-24-cleanmac-evidence-2026-09-21.md) ·
[2-5 review packet](evidence/gate-25-review-packet-2026-09-21.md) ·
[§1 natural-language bundle](evidence/gates-section1-copilot-bundle-2026-09-21.md)

**Durability.**
[Detached crash window](evidence/crash-window-evidence-2026-09-14.md) ·
[four-stage parallel verification](evidence/four-stage-parallel-evidence-2026-09-14.md)
