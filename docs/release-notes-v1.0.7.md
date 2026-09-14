# brgr v1.0.7

This patch lets the Command Code process recipe pass an explicit reasoning
effort when the installed executable advertises `--effort`. Older Command
Code executables without that flag remain unsupported for effort selection;
re-register after upgrading the executable and run its authorized scratch
activation before admission. No core harness switch or implicit model fallback
was added.

The [four-harness live Luna receipt](https://github.com/justn-hyeok/brgr/blob/v1.0.7/docs/live-four-harness-luna-min-2026-09-14.md)
records OMP at `low`, GJC at `minimal`, Cursor CLI's `none` model variant, and
Command Code at `low`, each through a fresh sealed result, durable owner inbox,
and explicit decision. The receipt used a local pre-version-bump debug binary;
the tagged release separately runs deterministic CI and unsigned-package
smoke tests. Native effort is not attested by any of these CLIs, and Cursor
CLI/Command Code do not supply native model identity in the captured output.

The Apple Silicon macOS 15+ archive remains unsigned and not notarized. Use
the [distribution guide](https://github.com/justn-hyeok/brgr/blob/v1.0.7/docs/unsigned-distribution.md)
and verify the archive and SPDX SBOM against the shipped checksums. Optional
Herdr cleanup remains owner-scoped best effort, not atomic compare-and-close.
This patch is not a claim that the full v1 completion checklist is closed.
