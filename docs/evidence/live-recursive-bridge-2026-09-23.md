# Live recursive bridge evidence — 2026-09-23

## Target and isolation

- Source: `feat/recursive-bridge-20260923`, based on
  `5f190cb4c86c5585baf141927a4caaa811bb4af6`, with uncommitted changes.
- Locally built `target/release/brgr` SHA-256:
  `ad857b5198f089e734257088117399b0c69f2f8c8c2e9d40ed66658617ce2785`.
  `herdr-plugin.toml` SHA-256:
  `f09cf366302f788fdcd38647bef342a2599515f759a9bf98dcda5b02cb8007d3`.
  At test time this was an unreleased development binary carrying a v2.1.0
  package version; the v2.2.0 release candidate was built afterward.
- Herdr 0.9.0 headless named session: `brgr-evidence-20260923`.
  Scratch work: `/tmp/brgr-recursive-evidence.IKd1pB/work`; isolated brgr home:
  `/tmp/brgr-recursive-evidence.IKd1pB/control`. Only `input.txt` was in the
  work directory; its text was `BRGR_LIVE_EVIDENCE_OK` and its SHA-256 was
  `3d29743fe25d07eb8c8257c6d1be19ba815d33c45bb6569ec0255ee43361992f`.
- The local plugin was linked for the test. Herdr's plugin registry proved to
  be shared across named sessions. After the test, the preexisting GitHub
  `v2.0.2` plugin was reinstalled at commit
  `d612a34868759ab385c3800cabb1972adb1ea589`; `brgr doctor --json`
  again reported `ok` for the user's installed CLI and four registered
  harnesses. The named test session remains for inspection with only its
  original shell pane; the scratch store and sealed artifacts are retained.
  No task worktree or source file was deleted.

## Harness identity

Both installed CLIs were activated in the isolated brgr home after bounded
real-model scratch requests. OMP 18.2.7 realpath was
`/Users/justn/.local/bin/omp`, executable digest
`2b2ff2084a34f61a018634c03929aaa9117009d107dbef553d8fc51eb5c28b14`,
scratch result digest
`89b4a17af09f34fd2fc83154761cae0c775c38ced829a4592006b8800a3cb1f6`.
GJC 0.17.2 realpath was `/Users/justn/.local/bin/gjc`, executable digest
`8e049e5eb182d902845dccee294f5597b6e1a343b47b87bb064f8eb480fbc23e`,
scratch result digest
`742672fb35c1b55ab6710cec67927ac55c4da5b4520b408218e9c69bb400abae`.

## Live Herdr pane and result path

A one-level OMP request opened adjacent worker pane `w1:p3` in tab `w1:t1`
with terminal `term_65c1cb0d5e9673`. Task
`601f1eaa-3b2d-4f0b-a92a-09703b0527df` produced result
`f9ba7251-3c26-4135-82c7-07a0fabd567a`, sealed text exactly
`BRGR_LIVE_EVIDENCE_OK`, artifact SHA-256
`1e09a1e575ff62ac487cffca2757b8585e36ac8d32f6067417d6295b835997db`,
and explicit Codex-owner decision `5305d2e2-f88f-46ff-81cf-a62aa9eec0ba`
(`accepted`). The OMP JSONL reported native model
`commandcode/meta/muse-spark-1.3-contributor`; effort observation was
unavailable.

The full live chain then ran in the same named Herdr session:

| Depth | Harness | Task | Herdr pane / tab | Result | Decision |
| --- | --- | --- | --- | --- | --- |
| 0 | OMP | `c9ca6031-2030-4d5e-b714-91f46ca7dcc0` | `w1:p5` / `w1:t1` | `9a587704-e578-4a9d-80d1-3e20a7742f99` | `7f0d361f-e332-4dcb-a2d1-54a5f29d2e22` accepted |
| 1 | GJC | `ab67111b-bb61-430f-9317-28f2c9490bda` | `w1:p6` / `w1:t1` | `56bd85c1-6f1a-42f5-9a74-20ae8eeadc21` | `9e03aeb8-d0b4-4746-a0ad-d434b2ff1cc2` accepted |
| 2 | GJC | `927efa66-470e-46c0-965b-1a8b9e4326bd` | `w1:p7` / `w1:t1` | `b9218886-9af4-4e52-9c0e-1ca4ba40ab7b` | `a91c8cf1-133a-49db-b8b0-19066c0a7e00` accepted |

The store's `delegation_edges` rows tied depth 1 to root attempt
`7b8984bc-c3d4-44f3-beea-1954a4f04fea` and depth 2 to GJC attempt
`3ddba294-369f-4236-a821-a303177efe84`. For all three tasks,
`decision.result_digest = results.result_digest` and the decision verdict was
`accepted`. The leaf artifact text was exactly `BRGR_LIVE_EVIDENCE_OK` (SHA-256
`1e09a1e575ff62ac487cffca2757b8585e36ac8d32f6067417d6295b835997db`).
The middle artifact contained `GJC_PARENT_OK`, the leaf task ID, and its
accepted decision ID. The root artifact contained `OMP_CHAIN_OK`, both child
task IDs, and both accepted decision IDs. Its native observed model was
`cursor/cursor-grok-4.6`; both GJC results observed
`commandcode-goat/z-ai/glm-5.3-flash`. Native effort was unavailable.

While all three workers ran, `herdr pane layout --pane w1:p6` showed one tab
with parent pane `w1:p1` still focused and split workers beside it. At depth 2,
the 120-column tab had pane widths 60/30/15/15. This proves adjacency but
also shows the current fixed-right split becomes too narrow for deep chains.
All worker panes closed automatically when their plugin commands exited; the
task, result, decision, and pane-launch receipts remained in brgr's store.

## Placement toggle

After `brgr config set-worker-placement tab`, one GJC run opened pane `w1:p8`
in separate tab `w1:t2` (terminal `term_65c1cce4731858`). Task
`97ecb215-0a2e-47b1-8933-e3e49cab8a72` sealed text `BRGR_TAB_OK`, artifact
SHA-256 `bdb238bf8ce925f2acb487813740330acc62b7865a29698ee15c55c579ef41d8`,
and was accepted with decision `b21791cc-e2cf-4138-882c-b20a06fba4c2`.
The isolated brgr config was returned to `adjacent` afterward.

## Corrections and limits revealed by the live test

- Herdr 0.9.0 rejects a split plugin-pane request that supplies both
  `--target-pane` and `--workspace`. The split path now sends only the exact
  parent pane; the tab path sends only the workspace.
- Nested workers must carry `HERDR_SESSION` and `HERDR_SOCKET_PATH`. Without
  those fields the child targeted the default Herdr session and failed with
  `pane_not_found`. The failed child was recorded `lost`, and its root was
  explicitly cancelled; those records were retained, not retried in place.
- This proves one real OMP → GJC → GJC process-worker chain and brgr's
  parent decisions. It does not prove interactive GJC follow-up, arbitrary
  cross-machine routing, dirty-worktree inheritance, or user-facing visual QA.
