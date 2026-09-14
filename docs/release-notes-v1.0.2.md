# brgr v1.0.2

This release lets Codex register an approved unfamiliar one-shot CLI through an
agent-authored `process/v1` manifest. The workflow is bounded probe → contract
test → authorized scratch run → activation → health check. An unknown synthetic
CLI with positional prompt syntax reaches a sealed result, durable owner inbox,
and explicit acceptance without a core harness branch. See the
[registration guide](https://github.com/justn-hyeok/brgr/blob/v1.0.2/docs/custom-harness-registration.md).

Detached task admission now survives a supervisor exit before the first
attempt. Reconciliation records one `lost` result, or `cancelled` if the owner
cancelled before execution; it never silently reruns an uncertain task. A
second attempt requires a durable grant for a transient pre-spawn failure.
Control-home/workspace overlap is rejected, and artifact reads compare the
opened file identity with the checked path before importing bytes.

The four named harnesses still use their v1.0.0 bounded fresh-run contracts.
This release does not add provider-side undo, hostile-worker isolation, Apple
signing/notarization, or atomic Herdr pane close. Optional Herdr cleanup is
owner-scoped best effort; use `--keep-pane` for shared panes. Download the
archive, checksums, and SPDX SBOM together and follow the
[unsigned distribution guide](https://github.com/justn-hyeok/brgr/blob/v1.0.2/docs/unsigned-distribution.md).
