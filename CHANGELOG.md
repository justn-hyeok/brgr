# Changelog

## Unreleased

Named process harnesses no longer activate through help/version probes alone.
`harness add` requires an explicitly authorized scratch workspace and prompt,
runs the observed contract and scratch task, then checks health. Scratch
workspaces overlapping the brgr control directory are rejected. Older process
activations without a scratch receipt require re-certification before new task
admission; stored results and decisions are unchanged. The optional Herdr
adapter is explicitly presentation-only and does not claim this certification.

Release SBOM generation now targets the `brgr-cli` Cargo package, records
metadata-derived dependency license declarations, and fails if any package
license is unasserted.

OMP JSONL process candidates now require native assistant `provider/model`
evidence to match an explicitly requested model; missing, mixed, or changed
models fail before artifact sealing. A separate atomic receipt records the
observed model and explicitly marks effort evidence unavailable; the v1
result envelope and decision digest remain compatible with older binaries.
Results written by an unreleased intermediate embedded-field build remain
readable without rewriting their stored bytes or decisions.
Replayed decisions return the persisted
decision ID, not a newly generated uncommitted one.

Exact model requests now run a bounded, declarative native catalog preflight
before task worktree creation and before paid scratch. OMP, GJC, Cursor CLI,
and Command Code packages supply their respective catalog shapes; agent-authored
packages may declare the same generic formats. Missing or unknown selectors
fail closed. New activation manifests contain `model_catalog`; `v1.0.5`
binaries reject those manifests on downgrade, so retain and restore a matching
registry snapshot only at an idle boundary. Result and decision bytes remain
compatible and must not be overwritten by an old store snapshot.

## 1.0.5 — 2026-09-14

Fix a Herdr-backed OMP revision replay: a new task revision could reuse the
previous revision's report while its agent was still at startup idle. The
wrapper now uses a fresh revision-scoped report and agent identity, refuses
existing report bytes, and requires an observed lifecycle transition from the
same pane, terminal, and immutable agent session bound to the spawn receipt
before publishing a
candidate. Tests and an actual WorkBuddy/DeepSeek R1→R2→R3 run cover the
negative replay and corrected positive path. A detached successful run also
has a durable offline-owner inbox regression test. Recovery now defers to a
live task-bound supervisor during the brief identity-recording window and
ignores a stale observation when a concurrent runner advances first. A
replacement supervisor reconciles old attempts before publishing its own
receipt, so it cannot adopt a dead predecessor. Process-level tests exercise
both sides of that race. All
previous unsigned and best-effort Herdr cleanup limits remain.

## 1.0.4 — 2026-09-14

Process-level crash fixtures now cover five detached supervisor windows,
including an interrupted artifact seal and a committed result before its CLI
hint. Store tests cover inbox-insert rollback and concurrent SQLite writer
contention. File-based results and optional OMP reports have bounded,
descriptor-checked reads; intermediate symlink escape is rejected. A failed
or interrupted separately launched Herdr-backed OMP worker is reported as
`lost` with unresolved effects, not falsely as a stopped one-shot process.
The unsigned cooperative-local platform boundary remains unchanged.

## 1.0.3 — 2026-09-14

Bind each owner to a Codex session epoch. Unbound or stale sessions can no
longer read, acknowledge, cancel, revise, or decide a task through the CLI.
`brgr bind TASK` explicitly transfers ownership without rewriting pending
results or decisions. Decision and acknowledgment transactions verify the
current epoch; a late old SessionStart hook cannot take it back. Existing
manual tasks require one explicit bind before follow-up. Hook inbox hints also
include pending items from owners transferred into the current session. The unsigned,
cooperative-local platform and optional best-effort Herdr cleanup are unchanged.

## 1.0.2 — 2026-09-14

Approved agents can activate an authored declarative process manifest after
bounded probes, contract tests, and an authorized scratch run. Admission is
durable before detached supervision; abandoned or pre-start-cancelled tasks
settle to one inbox result. Retries now require a durable pre-spawn failure
grant. The control home cannot overlap the source workspace, and artifact
imports verify the opened file identity. This remains an unsigned,
cooperative-local macOS arm64 release.

## 1.0.1 — 2026-09-14

Reject candidate results whose artifact is missing, forged, or fails sealed
byte verification before publishing them to an owner inbox. This closes a
store API path that could create a candidate the owner could not safely decide.
All other v1.0.0 feature and platform limits remain unchanged.

## 1.0.0 — 2026-09-14

First managed local release. Codex can start bounded fresh tasks through OMP,
GJC, Cursor CLI, and Command Code; brgr seals results, stores one durable owner
inbox item, and records explicit accept/reject decisions. Rejected candidates
can be corrected as immutable task revisions. A tested generic process recipe
supports future CLIs with a documented `--prompt-file` contract.

The macOS arm64 archive is unsigned and not notarized. See
[release notes](docs/release-notes-v1.md) and the
[distribution guide](docs/unsigned-distribution.md) for supported behavior and
limitations.
