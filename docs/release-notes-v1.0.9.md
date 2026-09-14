# brgr v1.0.9

This is the last planned `1.0.x` personal-use stabilization patch. It makes
registered harness drift visible in `brgr doctor` and prevents repeated JSONL
update events from exhausting the final-result capture limit. A changed or
incomplete harness now produces `needs_attention` and a nonzero doctor exit;
re-certify it with an authorized scratch run before new work. JSONL transport
remains bounded to 64 MiB, sealed artifacts keep their manifest byte limits,
and an over-limit process group is stopped without publishing a candidate.

The installed GJC and OMP process routes completed fresh-run → sealed result →
owner inbox → explicit decision checks on the personal macOS setup. A separate
fresh Codex session reached an accepted GJC result without asking the user to
handle task IDs. The [readiness checklist](https://github.com/justn-hyeok/brgr/blob/v1.0.9/docs/v1-readiness-checklist-2026-09-14.md)
records the local scope and evidence. Native effort remains unobserved; the
requested model was observed for GJC and OMP. No result wire format or store
schema changed, and existing sealed results and decisions are preserved.

This release does **not** declare the broader public v1 checklist complete.
Unknown CLI onboarding, the full failure matrix, legacy OMP callback migration,
and clean-host validation remain separate open gates. Optional Herdr pane
cleanup is still best effort, and its worker stop behavior is not certified.

The archive targets Apple Silicon macOS 15+, is unsigned and not notarized,
and ships with checksums and an SPDX SBOM. Follow the
[unsigned distribution guide](https://github.com/justn-hyeok/brgr/blob/v1.0.9/docs/unsigned-distribution.md).
