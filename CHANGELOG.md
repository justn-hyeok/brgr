# Changelog

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
