# Detached crash-window evidence — 2026-09-14

The process-level regression
`detached_crash_windows_reconcile_without_duplicate_or_overlapping_attempts`
uses a disposable store/workspace and a deterministic fixture CLI. Debug-only
failpoints exit the detached supervisor; the foreground CLI never receives
those failpoints during harness health probing. Each stage is followed by
repeated status/result reads and a new attempt-claim check.

| Forced exit | Recovery oracle |
|---|---|
| After attempt claim | One `lost` inbox item, no artifact reference or replay |
| After launch intent | One `lost` inbox item, no artifact reference or replay |
| After process spawn, before child PID file | One `lost` item with unresolved external effects; no overlapping retry |
| After artifact seal, before terminal DB commit | One `lost` item; orphan bytes are not published as a candidate |
| After terminal DB commit, before CLI completion hint | Original candidate/result ID survives and its sealed bytes verify |

The companion test
`detached_supervisor_exit_before_claim_becomes_one_durable_lost_inbox_item`
covers admission before an attempt is claimed. `queued_cancel_settles_without_starting_the_harness`
covers cancellation in that window. Run locally with:

Store-level fault tests also force a failure at inbox insertion after the
result insert, and hold a concurrent SQLite write lock. Both leave no partial
inbox/terminal state; after the fault is cleared, the same result commits once.
`failed_ack_rolls_back_decision_insert` separately checks decision/ack atomicity.
The optional OMP report importer and generic file-result collector reject
symlinks, oversized files, and a changed opened file identity. The generic
collector also rejects a symlinked intermediate directory that escapes the
workspace. Their reads are capped at the manifest/task artifact limit.

```bash
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

This evidence proves the listed fixture crash windows, **not** that an
already-spawned external process had no side effects, that DB disk exhaustion
is recovered, or that Herdr pane close is atomic. `lost` expressly retains
unresolved-effects evidence and cannot be retried on the same revision.
