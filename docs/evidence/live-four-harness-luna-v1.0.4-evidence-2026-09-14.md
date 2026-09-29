# v1.0.4 four-harness Luna evidence — 2026-09-14

The **public, unsigned v1.0.4 Apple Silicon binary** was downloaded without
GitHub authentication, checksum-verified, extracted, and used for four fresh
managed model runs. Its SHA-256 was
`4d093187fa16ea4eb4c000bd5cdfd8dffbcc7e68d205e0d67169aa62d21eb58b`;
the locally installed binary matched. Tag `v1.0.4` peels to main commit
`518250a6260b629bcd1769d4788717fa14983d56`.
[PR #5 CI](https://github.com/justn-hyeok/brgr/actions/runs/34800673439),
[main CI](https://github.com/justn-hyeok/brgr/actions/runs/34800836568), and
[tag Release workflow](https://github.com/justn-hyeok/brgr/actions/runs/34800996580)
all passed for their recorded target SHAs.

The isolated supervisor home was `/tmp/brgr-live-v104.faOCf6/home`; the
workspace was a disposable non-Git directory. The owner was
`codex:live-v104`, bound to `live-v104` at epoch 1. No repo worktree or pane
was created for these four runs. OMP's process route was invoked with
`HERDR_ENV=0` and no `HERDR_PANE_ID`. Cursor used read-only `ask` and Command
Code used `plan` according to their activated recipes. OMP, Cursor, and
Command Code passed paid Luna scratch activation first; GJC's known recipe
activated without a scratch request. All four activations finished with
`process-contract/v1` and later reported `healthy`.

| Harness | Exact requested model / effort | Task ID / result ID | Exact sealed text | Artifact SHA-256 |
|---|---|---|---|---|
| GJC | `gpt-5.6-luna` / `low` | `5211f79e-b00d-4225-9b0e-bcf6bac18582` / `f4ecb2c4-70db-4a33-9213-89b42ebbeda7` | `BRGR_GJC_V104_LUNA_39F2` | `234bbb0b643c9dc890714bb60f129f0820b44cfc0ecb2f8948c07087097aafcc` |
| OMP process | `openai-codex/gpt-5.6-luna` / `low` | `ee671f87-110d-40f3-aced-bc748d7d2cc9` / `d6e5dafb-1c4e-42ce-9d91-1f4068dc64a5` | `BRGR_OMP_V104_LUNA_NO_HERDR_5C1B` | `7b1beb43dbcd40b73e355647a6e0ffdde9eb6f8b595878b283cfb2bf1af973ad` |
| Cursor CLI | `gpt-5.6-luna-low-fast` / unavailable | `ca855280-7fd7-483a-ba12-1862eaba1fe5` / `7fe72b3b-b4d4-4b0a-a1d5-d3f0b4264461` | `BRGR_CURSOR_V104_LUNA_83AC` | `1c7722727087828648cc23edd5119bf42900365ccc57ef3ea3732e37f38f2ea8` |
| Command Code | `gpt-5.6-luna` / unavailable | `c8000305-84bc-425a-a236-e7df861ecbc3` / `b8f494c2-a045-4a83-8583-1ac1993f9cac` | `BRGR_COMMAND_V104_LUNA_4D91` | `2258f5b1735c6f53b6089d0d07a82222ccfba1f05d3886cef79454407f53ba5b` |

For each candidate, `brgr result TASK` returned the exact text above. A
separate `jq -rj .artifacts[0].text | shasum -a 256` matched its artifact
reference. Codex then recorded an explicit `accepted` decision. An independent
SQLite join found **4 results, 4 accepted decisions, 4 acknowledged inbox
items, 4 matching result/decision digests, and binding epoch 1**. The task
route JSON retained the exact model/effort selectors shown above; no model
substitution was requested by brgr.

This proves the four bounded happy paths on the released binary, not native
provider-side model identity or absence of an undocumented provider fallback.
It also does not prove provider-side cancellation/undo, a natural-language
new-session correction journey, hostile same-user isolation, or atomic Herdr
pane close. The scratch home and its activation receipts are retained locally;
the table and immutable release/CI links preserve the essential evidence if
temporary files are later removed.
