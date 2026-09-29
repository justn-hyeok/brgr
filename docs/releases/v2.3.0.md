# brgr v2.3.0

This release connects delegated work to an explicit owner decision:

- Herdr Codex owners receive a durable completion notification at their exact
  recorded pane when they become idle. Pending notifications survive a busy
  parent and session transfer; delivery never accepts a result automatically.
- Tasks can pass acceptance criteria, scope, and role instructions to workers.
  Selected dirty files and tracked deletions are copied into an isolated task
  worktree while the source remains unchanged.
- Required write, browser, MCP, and named capabilities are checked against the
  registered harness before task admission. Unsupported requirements fail
  closed without a model call.
- Bounded Git diff, logs, and requested files can be sealed as evidence. The
  requested logs are retained for failed and cancelled runs. The owner reviews
  and accepts a result separately from conflict-checked patch application.
  Task trees expose waits, remaining time, child limits, and
  subtree cancellation.

The all-features Rust suite and deterministic fake-Herdr and Git fixtures cover
these contracts, including session transfer, retry, CRLF snapshots, worker
commits, integration conflicts, and crash recovery. A live completion prompt
to a separate Codex pane has not been run for this release. Untracked worker
files require explicit evidence export or Git staging before diff capture.
There is no implicit harness or model fallback and no implicit result approval.

Install the Herdr plugin on Apple Silicon macOS 15 or newer:

```sh
herdr plugin install justn-hyeok/brgr --ref v2.3.0
```

The CLI archive remains unsigned and not notarized.
