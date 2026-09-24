# Completion loop implementation plan

Target: `Codex ↔ OMP ↔ GJC ↔ GJC` and other registered process harnesses.
brgr owns task identity, durable artifacts, and explicit decisions. Herdr
provides an optional exact-pane presentation and wake path.

1. **Completion delivery.** Commit a stable notification ID with every
   terminal inbox item. Capture the exact Codex owner session and Herdr pane.
   A detached, bounded dispatcher retries pending items while the parent is
   busy, delivers only after checking pane and session identity, and retains
   undelivered items through restart or session transfer. The callback carries
   the ID and task handle; it never accepts a result.
2. **Worker instructions.** Render objective, acceptance criteria, scope, and
   role instructions into the worker prompt from the recorded task contract.
   The worker sees the same criteria the owner will verify.
3. **Selected dirty snapshot.** Require explicit relative paths. Capture only
   selected tracked changes and untracked regular files under strict size and
   path bounds into a new task worktree; preserve source bytes. Record the
   snapshot manifest and digests.
4. **Capability preflight.** Make required write, MCP, browser, or other
   declared capabilities explicit in task admission. Check the activated
   harness before task worktree creation or model execution; reject unsupported
   requests with the missing capability named.
5. **Evidence and integration.** Seal bounded report, diff, screenshot, and
   log artifacts. Keep `accept` separate from an explicit integration command
   that checks patch conflicts against the target worktree before applying.
6. **Tree control and status.** Add bounded concurrent-child admission,
   cancellation over the recorded subtree, and a tree view of time remaining,
   question/approval waits, results, and decisions. Prevent new children after
   parent cancellation begins.

Verification at every stage uses deterministic fixture executables without
Herdr for the core contract. Exact fake-Herdr tests cover parent wake and
session transfer. The live Herdr CLI's current Codex pane identity and status
response were checked read-only; a live completion prompt to another Codex
pane remains a separate presentation smoke. No task or result is accepted
automatically.
