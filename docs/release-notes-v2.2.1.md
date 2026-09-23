# brgr v2.2.1

Detached managed runs started from an ordinary Herdr pane can open a brgr
worker pane beside that exact caller after
`brgr config set-auto-worker-pane true`. This option defaults off for
standalone users without the Herdr plugin. The existing
`brgr config set-worker-placement tab` setting opens the worker in a separate
tab instead. brgr keeps task identity, results, messages, and explicit owner
decisions; Herdr remains the presentation surface.

The installed brgr Codex skill now directs bounded work through this path and
explains how parent workers settle child results and exchange messages. A
[deterministic ordinary-pane fixture](live-ordinary-herdr-pane-2026-09-23.md)
reached a sealed candidate and explicit acceptance; the earlier
[real OMP → GJC → GJC run](live-bidirectional-bridge-2026-09-23.md) remains
the evidence for recursive model execution.

Install the Herdr plugin on Apple Silicon macOS 15 or newer:

```sh
herdr plugin install justn-hyeok/brgr --ref v2.2.1
```

The GitHub CLI archive remains unsigned and not notarized. Interactive TUI
sessions and an explicitly requested foreground run keep their own execution
paths.
