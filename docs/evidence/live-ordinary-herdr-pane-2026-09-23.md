# Ordinary Herdr pane → brgr worker receipt — 2026-09-23

This is a local fixture proof from the `feat/brgr-herdr-default-20260923`
development worktree. The caller was an ordinary Herdr shell pane `w64:p1`,
not a brgr plugin Codex pane. It ran the branch's `target/debug/brgr` with an
isolated control home and the registered deterministic `local.gjc` fixture.

The `brgr run` receipt returned task
`953ef7cf-f59c-4ad2-bb11-4e36ca896777`, `worker_placement: adjacent`, and
worker pane `w64:p2`. The Herdr pane-open receipt placed `w64:p2` unfocused in
the caller's tab `w64:t1`. The fixture produced a sealed candidate with text
`BRGR_FIXTURE_OK`, artifact digest
`sha256:770fc6713b7be966375c363b51c1fe2ccab11612c89fb9e987089fd013f57504`,
then the owner explicitly accepted it with decision
`dbbcbb5f-dc6b-43e1-811b-8f0822009f08`. The worker pane closed after its
process exited; the original pane remained. The isolated store and pane
receipt remain under `/tmp/brgr-ordinary-herdr-proof.PCJBFk/control`.

This proves pane placement and result ownership for an ordinary Herdr caller
using a fixture. The separate [live recursive receipt](live-bidirectional-bridge-2026-09-23.md)
covers real OMP and GJC model processes and two-way nested delegation.

After adding the v2.2.1 `herdr.auto_worker_pane` switch, the isolated control
home was set to `true` with `worker_placement = "adjacent"`. A second command
from the same ordinary pane returned task
`ef6f34b9-a403-47a2-8230-437136e649ce` and worker pane `w64:p3`, again
unfocused in `w64:t1`. The sealed text was `BRGR_FIXTURE_OK`; owner decision
`9bd8a27d-ae74-44c9-b3de-8a129cef035c` was `accepted`. This second run
checks the final opt-in implementation rather than only the earlier
always-on development variant.
