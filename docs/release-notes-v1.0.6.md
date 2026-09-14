# brgr v1.0.6

This patch requires an authorized paid scratch run before a named process harness is activated. Existing help/version-only activations fail closed for new tasks; re-add an approved executable with a disposable workspace, explicit scratch prompt, and exact model selector. New model requests use a bounded native catalog preflight before creating a task worktree or spending a model request. An unavailable selector is rejected rather than silently routed elsewhere.

OMP JSONL results now record the observed native provider/model separately from the sealed v1 result envelope. A mismatched or missing model cannot become a candidate. OMP effort remains unverified. GJC's JSONL route can report its observed model; Cursor CLI and Command Code still report native route observation unavailable. An older v1.0.5 binary can read and decide new v1 results, but cannot load newly activated manifests with model catalogs; preserve a registry snapshot and stop new admissions before rollback.

The [live process-harness matrix](https://github.com/justn-hyeok/brgr/blob/v1.0.6/docs/live-v1.0.6-luna-matrix-2026-09-14.md) records GJC, Cursor CLI, and Command Code fresh Luna runs with sealed artifacts, durable inbox items, and explicit decisions; the linked OMP WorkBuddy receipt used an earlier source revision. This is not a v1-wide completion claim.

The release includes a package-focused SPDX SBOM with declared licenses. The archive remains unsigned and not notarized for Apple Silicon macOS 15+. See the [distribution guide](https://github.com/justn-hyeok/brgr/blob/v1.0.6/docs/unsigned-distribution.md). Optional Herdr pane cleanup is owner-scoped best effort, not atomic compare-and-close. No hostile same-user isolation, provider-side undo, or verified model effort is claimed.
