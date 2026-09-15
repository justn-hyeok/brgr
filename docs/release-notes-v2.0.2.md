# brgr v2.0.2

This patch adds Devin CLI as the generated `local.devin` process harness.
Registration observes the installed executable's version and help, performs an
authorized scratch run, and pins the resulting recipe and executable digest.
Managed runs use documented prompt-file print mode, smart permissions, and the
non-interactive workspace-trust override. Successful stdout follows the same
sealed artifact, durable inbox, and explicit Codex accept/reject contract as
the existing process harnesses.

The route uses the model already configured in Devin CLI. Devin's JSON model
catalog currently exceeds brgr's bounded probe limit, so model and effort flags
are unsupported for `local.devin`; explicit selector requests fail before task
admission. This release does not guess a model variant or claim native model
identity from plain stdout.

The installed Devin CLI 3000.10.27 path completed both scratch activation and a
fresh managed result on macOS. Deterministic fixtures cover exact argv,
catalog/selector rejection, health, sealing, owner inbox, and acceptance. See
the [live receipt](https://github.com/justn-hyeok/brgr/blob/v2.0.2/docs/live-devin-cli-v2.0.2-2026-09-15.md).

Install the Herdr plugin with:

```sh
herdr plugin install justn-hyeok/brgr --ref v2.0.2
```

The downloadable CLI archive remains unsigned and not notarized and targets
Apple Silicon macOS 15 or newer. No task wire format, store migration, implicit
fallback, or automatic acceptance changes.
