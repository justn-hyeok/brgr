# brgr v1.0.4

This patch hardens the managed-run failure boundary. Debug-only process
failpoints and deterministic fixtures cover task claim, launch intent,
process spawn, artifact seal, and terminal commit. Reconciliation retains one
durable result and forbids an overlapping retry. SQLite fault tests verify
that a busy writer or failed inbox insert cannot publish a partial terminal
result. See the [crash-window evidence](https://github.com/justn-hyeok/brgr/blob/v1.0.4/docs/crash-window-evidence-2026-09-14.md).

Generic file results are capped, descriptor-checked, and cannot traverse an
intermediate symlink outside the workspace. Optional OMP report imports are
also size-bounded before collection. A separately launched Herdr-backed OMP
worker is now typed as externally delegated: if its wrapper times out,
stops, or exits without a valid final result, brgr records `lost` with
unresolved external effects rather than claiming the worker stopped. Custom
manifests remain one-shot only.

The four baseline harnesses, session-bound decisions, and custom declarative
registration remain as in v1.0.3. This does not claim provider-side undo,
hostile same-user isolation, Apple signing/notarization, or atomic Herdr pane
close. Download the archive, checksums, and SPDX SBOM together and follow the
[unsigned distribution guide](https://github.com/justn-hyeok/brgr/blob/v1.0.4/docs/unsigned-distribution.md).
