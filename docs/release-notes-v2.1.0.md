# brgr v2.1.0

This release closes every remaining public v1 readiness gate. The
[v1 checklist](https://github.com/justn-hyeok/brgr/blob/v2.1.0/docs/v1-readiness-checklist-2026-09-14.md)
has zero open items.

New regression coverage: ENOSPC seal/commit behavior, OMP identity-swap
fail-closed, duplicate-completion idempotent replay, delegated cancel/deadline
`lost` paths, and the pane-cleanup status matrix. A flaky bridge timing test
is fixed. No execution contract, adapter behavior, wire format, or store
migration changes.

Live evidence in this release cycle: a new agent-authored `local.copilot`
harness registration with a reject-to-accept revision bundle, public-binary
fresh runs across GJC/OMP/Cursor with matching digests, and a clean-environment
install with Gatekeeper and fixture verification. The pane-close criterion is
decided as cooperative-local best effort; external review is waived by the
owner for personal-use scope.

Install the plugin with:

```sh
herdr plugin install justn-hyeok/brgr --ref v2.1.0
```

The downloadable CLI archive remains unsigned and not notarized and targets
Apple Silicon macOS 15 or newer.
