# OMP callback migration audit — 2026-09-14

Read-only snapshot, not a claim that all old callback deliveries are drained.
New brgr `local.omp` uses a one-shot process recipe without Herdr. The optional
`local.omp-herdr` launcher starts without an initial Codex prompt, so its
launcher receipt has `codex_prompt_marked=false`. The installed
`omp-role-parent-notify.ts` extension's `enabled()` requires the corresponding
`OMP_ROLE_CALLBACK_CODEX_MARKED=1`; new brgr-managed Herdr turns therefore do
not have that legacy parent-notify authority. Brgr still requires its own
sealed result, durable owner inbox, and explicit accept/reject.

The local contract directory contained 227 OMP callback contracts, 12 with
brgr-related task names. Of those 12 captured child pane IDs, only `w2K:pD`
(`brgr_offline_v1`) and `w2K:pJ` (`brgr_recovery_pr8_review`) were live in the
workspace inventory; both were idle at audit time. The other ten exact pane
IDs were absent. These are mixed historical brgr-managed and Codex review
workers, not twelve pending brgr task results. No pane or contract was closed
or deleted by this audit.

The callback helper's `--gc` **dry run** reported 247 retained outbox files:
110 `pending`, 119 `recent`, and 18 `unverified_report`; zero deletion-eligible.
Those totals are global OMP state, not brgr-only state. Pending or ambiguous
records must not be globally replayed, acknowledged, or deleted merely to
make the count zero. Before any old-generation drain, correlate each exact
run ID with its contract, immutable child session, report descriptor, and
Codex parent delivery/acceptance receipt. Unknown delivery stays quarantined;
existing brgr result/decision rows remain authoritative for managed tasks.

This audit does not prove that every old parent callback was delivered once,
nor does it establish an atomic Herdr pane compare-and-close operation.
