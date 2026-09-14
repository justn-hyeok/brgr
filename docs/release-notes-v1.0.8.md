# brgr v1.0.8

This public unsigned patch ships the [current v1 readiness checklist](https://github.com/justn-hyeok/brgr/blob/v1.0.8/docs/v1-readiness-checklist-2026-09-14.md) and links it from the README. It preserves the existing `v1.x` version series and all previously published tags and releases. No managed-run protocol, harness recipe, or worker behavior changed from v1.0.7.

The bounded fresh-run → sealed result → durable owner inbox → explicit accept/reject path has live evidence for OMP, GJC, Cursor CLI, and Command Code, with the lowest available Luna setting used in those tests. The checklist clearly separates that evidence from the still-open natural-language end-to-end, failure-race, legacy-callback, and clean-host gates. This release is usable as a public preview; it is **not** a declaration that every v1 practical-use gate passed.

The archive targets Apple Silicon macOS 15+ and remains unsigned and not notarized. Download the archive, checksums, and SPDX SBOM together, and follow the [distribution guide](https://github.com/justn-hyeok/brgr/blob/v1.0.8/docs/unsigned-distribution.md). Optional Herdr presentation and owner-scoped pane cleanup remain best effort; provider-side undo, verified effort, and hostile same-user isolation are not claimed.
