# brgr v1.0.0

`brgr` is a local managed-task bridge for Codex on Apple Silicon macOS 15+.
Codex remains the natural-language owner: it reviews sealed results and records
accept or reject. The CLI is an explicit escape hatch.

- Bounded fresh runs through OMP, GJC, Cursor CLI, and Command Code; a generic
  process recipe can register another documented `--prompt-file` CLI without
  changing the orchestration core. Exact requested harness/model/effort support
  is checked before launch.
- SHA-256-sealed result artifacts, a durable SQLite owner inbox, idempotent
  decisions, and correction through a new immutable task revision.
- OMP's default process adapter works without Herdr. The separate
  `local.omp-herdr` adapter offers optional interactive presentation and
  owner-scoped best-effort pane cleanup.

The release archive is **unsigned and not notarized**. Download the archive,
`checksums.txt`, and SPDX SBOM from this release, verify them, then follow the
[unsigned distribution guide](https://github.com/justn-hyeok/brgr/blob/v1.0.0/docs/unsigned-distribution.md). No global Gatekeeper
disablement is required.

Limits: cancellation stops a supervised local process group; it does not undo
external actions or guarantee provider-side cancellation. Rich in-flight
steering and resume are not certified. Inbox pull remains durable when Codex is
offline; wake hints are not exactly-once delivery. Installed same-user
harnesses are assumed cooperative, not isolated from a hostile same-user
process. Herdr 0.9.0 lacks atomic conditional pane close, so optional pane
cleanup is best-effort and should be disabled with `--keep-pane` for shared
sessions.
