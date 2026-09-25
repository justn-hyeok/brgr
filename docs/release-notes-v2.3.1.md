# brgr v2.3.1

This patch closes two local Herdr callback gaps from v2.3.0:

- The plugin Codex pane can use a configured absolute Codex executable when
  the Herdr server's `PATH` does not include a shell-managed installation.
  Use `brgr config set-codex-executable "$(command -v codex)"` outside the
  plugin Codex pane when needed.
- If Herdr has not reported a Codex session ID, brgr checks the exact bound
  Codex pane and reports that session before dispatching a completion prompt.
  Registration runs after task supervision starts. Slow Herdr lookups cannot
  make tasks appear unclaimed or delay Codex inbox and Stop hooks past their
  budget.

An isolated live Herdr pane received a brgr fixture completion prompt without
manual session reporting; Codex wrote the expected callback receipt. The
deterministic fixtures also cover a minimal `PATH`, absent session metadata,
and a delayed Herdr socket. The task owner still verifies and explicitly
decides every result.

Install the Herdr plugin on Apple Silicon macOS 15 or newer:

```sh
herdr plugin install justn-hyeok/brgr --ref v2.3.1
```

The CLI archive remains unsigned and not notarized.
