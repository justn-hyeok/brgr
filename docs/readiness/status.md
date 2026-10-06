# Readiness status

**v2 is the current release line.** Two gates below are still open:
multi-process concurrency (2-4) and a full non-author review (2-5).

The rest of this page moved from the top of the README on 2026-10-05,
unchanged. The [v1 readiness checklist](v1-readiness-checklist-2026-09-14.md)
holds the full record.

v2 packages brgr as a Herdr 0.9+ plugin for Apple Silicon macOS 15 or newer.
Herdr hosts a read-only task board and a Codex pane; brgr still owns task
identity, sealed results, and the durable inbox, while Codex alone decides
accept or reject. The v1 CLI and stored results remain usable without Herdr.
Release archives are
unsigned and not notarized. Read [the unsigned distribution guide](../guides/unsigned-distribution.md)
before sharing or running a downloaded binary.

The v1 managed-run contract is retained. The v2.1.0 release closed the
[v1 readiness checklist](../readiness/v1-readiness-checklist-2026-09-14.md) under its
recorded personal-use scope and evidence limits, and v2.2.0 adds recursive worker
delegation and an attempt-scoped message mailbox; its verified scope and open
limits are described below.

That gate closure did not hold. Re-measuring the same code on 2026-09-28 found a
concurrency defect that failed one in four concurrent admissions with a raw store
error, a board projection that cost 3.26s at 6,400 stored tasks against a
two-second refresh, and task worktrees that were never reclaimed. Those are fixed
and pinned by regression gates, but gates 2-4, 2-5 and the final GO verdict are
reopened and the public v1 verdict is NO-GO until they close. The personal-use GO
stands, because it was scoped to observed single-session paths.

One limitation is accepted rather than pending: brgr has no human reviewer other
than its author. The review gate is therefore defined as a procedure — a
non-author review of the whole crate, every finding disposed of in writing, each
load-bearing claim reproduced independently, and every performance claim backed
by a bench in this repository — and the absence of a second person is recorded
here instead of being waived.

