# brgr v1.0.3

Owner decisions now carry the bound Codex session and epoch. An unbound task
cannot be read, acknowledged, or decided through the CLI. Explicit
`brgr bind TASK` transfers a task to the current session while preserving its
task ID, sealed results, inbox entries, and earlier decisions. Old-session
decisions and acknowledgments fail after the transfer. A late SessionStart
hook cannot silently reverse it. Pending inbox items from the transferred
owner appear in the new session's hook hints.

**Migration:** Existing tasks and database files remain in place. If a task was
created from a bare shell without a Codex session, run
`brgr bind TASK --session SESSION` once, then use that session for `result`,
`accept`, `reject`, or `--ack`. Inside Codex, `brgr bind TASK` uses the current
session. Rerun `brgr integrate codex install` to update the owned skill text;
foreign hooks are preserved. Bare CLI fixtures can set both
`BRGR_OWNER_ID=codex:example` and `BRGR_SESSION_ID=example-session` on run and
follow-up commands. A bind does not accept a result or delete a worktree.

The archive remains unsigned and not notarized for Apple Silicon macOS 15+.
The same four bounded one-shot harness routes and declarative custom-process
registration remain available. Provider-side undo, hostile same-user isolation,
and atomic Herdr pane close are not claimed. Download the archive, checksums,
and SPDX SBOM together; follow the
[distribution guide](https://github.com/justn-hyeok/brgr/blob/v1.0.3/docs/unsigned-distribution.md).
