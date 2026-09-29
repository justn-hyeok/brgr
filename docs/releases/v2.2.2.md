# brgr v2.2.2

This patch fixes two recursive bridge paths identified in the v2.2.1 code
review:

- Revising a rejected child task now retains its original parent edge and
  checks that the exact parent attempt is still active. A Git parent worker
  can revise a child using its own task worktree. A new revision cannot drop
  or replace an existing parent edge.
- A worker can read and acknowledge messages addressed to its exact attempt
  after that attempt becomes terminal. Sending a new message still requires
  an active attempt; another task or attempt cannot read that mailbox.

The deterministic CLI regression covers a Git parent → rejected child →
revision 2 → accepted child path and terminal-attempt message access. The
existing OMP → GJC → GJC execution and Herdr pane contracts remain as in
v2.2.0 and v2.2.1.

Install the Herdr plugin on Apple Silicon macOS 15 or newer:

```sh
herdr plugin install justn-hyeok/brgr --ref v2.2.2
```

The CLI archive remains unsigned and not notarized.
