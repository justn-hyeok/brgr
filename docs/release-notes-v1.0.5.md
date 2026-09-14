# brgr v1.0.5

This patch fixes a serious optional Herdr-backed OMP completion bug. In
v1.0.4, a rejected task's next revision could reuse the prior report file
and treat agent startup idle as a fresh completed turn. Brgr now allocates a
different agent/report path per revision, rejects a pre-existing report,
checks the exact prompted target, and requires a new lifecycle sequence from
the same pane, terminal, and agent session before sealing a candidate. Its
first observation must match brgr's persisted spawn-ownership receipt.

The [R1→R2→R3 reproduction and corrected live run](https://github.com/justn-hyeok/brgr/blob/v1.0.5/docs/herdr-revision-replay-evidence-2026-09-14.md)
records two rejected stale/unsatisfied results and one accepted fresh report
using `workbuddy/deepseek-v4.1-flash`. A deterministic offline-owner fixture
also verifies that a successful detached result remains in the durable inbox
until an explicit decision and acknowledgment. Recovery defers to a live
task-bound supervisor during the brief startup
identity window and treats a stale recovery snapshot as concurrent progress,
without replacing the runner's result. A replacement supervisor reconciles
old attempts before publishing its own receipt; process-level tests exercise
the live startup and dead-predecessor paths. Older stored results and
decisions are not rewritten; already accepted historical results should be
re-evaluated against their original criteria if they came from a revised
Herdr-backed OMP task.

The archive is unsigned and not notarized for Apple Silicon macOS 15+. The
one-shot OMP process route does not depend on Herdr. Optional Herdr pane
cleanup remains owner-scoped best effort, not atomic conditional close;
provider-side undo and hostile same-user isolation are not claimed. Download
the archive, checksums, and SPDX SBOM together and follow the
[distribution guide](https://github.com/justn-hyeok/brgr/blob/v1.0.5/docs/unsigned-distribution.md).
