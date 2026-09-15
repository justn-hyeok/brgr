# Changelog

## 2.0.2 — 2026-09-15

Add a generated `local.devin` process recipe for Devin CLI. It uses documented
prompt-file print mode, smart permissions, and an explicit non-interactive
workspace-trust override, then seals nonempty stdout through the existing v1
result and Codex decision contract. Devin model and effort selection remain
unsupported through brgr; the recipe uses Devin's configured default and fails
instead of probing its current 64 KiB-plus model catalog or guessing a variant.

The installed Devin 3000.10.27 path completed an authorized scratch run and a
fresh managed run through sealed artifact, durable owner inbox, and explicit
acceptance. This adds no shell execution, implicit model fallback, automatic
acceptance, or store/wire migration.

## 2.0.1 — 2026-09-15

Harden the v2 personal-use path around the Herdr host bridge, Git worktree
admission, harness health checks, SQLite writer contention, and the read-only
board. The plugin Codex pane now exposes only an ephemeral bridge directory;
the host pins accepted brgr commands to the configured control home and
selected workspace and rejects registry or integration mutation. Bridge
timeouts and output overflow stop the spawned process group, and encoded
responses are bounded before publication.

Concurrent admissions share a repository-wide lock across linked worktrees,
preserve collisions, and release the lock before foreground execution. Harness
health distinguishes executable, spawn, timeout, exit, and probe-evidence
failures and detects current Codex hook or skill drift. The board reads an
existing store without schema mutation and validates relational task, result,
and decision identities before projecting them. No wire format, automatic
acceptance, implicit fallback, or public v1 readiness claim changes.

## 2.0.0 — 2026-09-15

Package brgr as a Herdr 0.9+ plugin on macOS. Workspace actions open a
read-only task board, launch a worktree-bound Codex pane, and run the existing
health check. The Codex pane installs the brgr-owned integration and uses the
plugin-built binary. A private, pane-lifetime bridge runs brgr commands in the
Herdr host while Codex keeps its command sandbox. Herdr supplies execution and
status presentation; Codex retains the explicit final accept/reject decision.

The v1 managed-run CLI, store, result envelope, and decision contract remain
available without Herdr. This release adds no implicit harness or model
fallback, automatic acceptance, store migration, or worker cancellation claim.
The optional legacy OMP-through-Herdr adapter keeps its documented cleanup
limit. See `docs/v2-herdr-plugin.md` for the plugin contract and evidence gates.

## 1.0.9 — 2026-09-14

Harden the local managed-run path for personal use. `brgr doctor` now checks
every registered harness and the Codex integration, returns `needs_attention`
for missing or drifted paths, and exits unsuccessfully until they are healthy.
This changes the exit status of an unhealthy doctor check.

JSONL process capture now discards repeated update events while retaining
assistant completion and model evidence. It bounds raw transport to 64 MiB,
keeps the final artifact limit, and stops the process group promptly on
overflow. A fresh Codex session and the installed GJC and OMP process routes
completed sealed-result, owner-inbox, and explicit-decision checks. No wire or
store schema, implicit model fallback, or automatic acceptance changed.

This is the last planned `1.0.x` personal-use stabilization patch. The separate
public v1 completion checklist remains `NO-GO`.

## 1.0.8 — 2026-09-14

Publish the current v1 readiness checklist with linked implementation, live
harness, crash-window, and unsigned-release evidence. The README now points
to the open practical-use gates. No execution contract or adapter behavior
changed; this is a public documentation and version-alignment patch, not a
claim that every v1 acceptance gate has passed.

## 1.0.7 — 2026-09-14

Command Code registration now enables `--effort` only when its installed help
documents that option. A requested effort is passed as a separate argv value;
older executables without the flag remain explicitly unsupported. Live
minimum-effort Luna runs across OMP, GJC, Cursor CLI, and Command Code are
recorded in `docs/live-four-harness-luna-min-2026-09-14.md`. Cursor's `none`
level is selected through its exact model variant, not an invented effort flag.
Native effort remains unobserved; accepting a response does not prove that a
provider honored an effort setting.

## 1.0.6 — 2026-09-14

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
readable without rewriting their stored bytes or decisions. Replayed decisions
return the persisted decision ID, not a newly generated uncommitted one.

Exact model requests now run a bounded, declarative native catalog preflight
before task worktree creation and before paid scratch. OMP, GJC, Cursor CLI,
and Command Code packages supply their respective catalog shapes; agent-authored
packages may declare the same generic formats. Missing or unknown selectors
fail closed. The optional Herdr adapter also checks OMP's catalog before brgr
admission. Native probes retain original file descriptors and immediately
stop their process group after a 64 KiB output threshold (sampled every 1 ms) or a
finite deadline; trusted executables can briefly overshoot that disk threshold
between samples. New activation manifests contain `model_catalog`; `v1.0.5`
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
