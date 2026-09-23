# Live bidirectional bridge evidence — 2026-09-23

## Scope

This receipt covers the development branch `feat/recursive-bridge-20260923`
in `/Users/justn/dev/.worktrees/brgr-recursive-bridge-20260923`, not the
installed public brgr binary. The local release binary used for the nested
message run had SHA-256
`1780b8e81bcd3c6660625cfeb6343802396eb1bcd4a11af48f9a5a4e48c07aa0`.
Herdr 0.9.0 ran the named `brgr-evidence-20260923` session with real OMP
18.2.7 and GJC 0.17.2 processes. The isolated task store is
`/tmp/brgr-recursive-evidence.IKd1pB/control/store/brgr.sqlite3`; the scratch
work and sealed artifacts remain under the same scratch root.

## Accepted single-chain rerun

A subsequent run used `accepted-bidi-protocol.txt` in the scratch work
directory and gave the root, middle, and leaf budgets of 1500, 900, and 360
seconds respectively. It used the same development binary digest above and
produced **one fully accepted Codex ↔ OMP ↔ GJC ↔ GJC chain**:

| Depth | Harness | Task | Parent decision | Sealed artifact SHA-256 |
| --- | --- | --- | --- | --- |
| 0 | OMP | `97f1467c-cb5b-449d-9124-b0e2737cb61e` | Codex `7c5f81c6-c616-47e8-8bfd-6b76c7bdaf6d` accepted | `af63bb9ee23c246d87a007011d2b3e27c95c34812211cbe1aff5ce3d851b0553` |
| 1 | GJC | `4595f52e-0793-45d5-9f27-ad90aeb2fb55` | OMP `62e019d0-ad17-4aa7-8fcd-34c865c631c4` accepted | `affd818d3aef9fb9985590f46c2ab12ac57e0f145092bd07462492321a501e60` |
| 2 | GJC | `4d709823-2420-41c4-b4bc-4b6005963cad` | GJC `d2b2f9ca-186d-4b3a-91e9-2f707caa83bd` accepted | `cf0a6cb166a1965311f672af9cc1452a685dbaa968a9f268c7d8ef8562b62a1a` |

The store has exactly two descendant edges from the root: depth 1 binds to
root attempt `bbb8e87c-3bc0-4af3-8838-28b07d00c547`, and depth 2 binds to
middle attempt `104ac520-9cbd-41c7-af34-aad6795cd6b0`. Each task has four
messages: two questions and two opposite-direction replies linked to their
question on the same attempt. All twelve messages were acknowledged. All
three results are `candidate` with `accepted` decisions whose result digests
exactly match their sealed results. The leaf and middle artifact texts contain
`LEAF_BIDI_OK` and `MIDDLE_BIDI_OK`; the root artifact contains
`ROOT_BIDI_OK`, both child task IDs, the middle decision ID, and the input
token `BRGR_LIVE_EVIDENCE_OK`. Independently hashing all three stored files
matched their artifact references.

The middle and leaf opened unfocused adjacent Herdr panes `w1:pJ` and `w1:pK`
in tab `w1:t1`. This rerun launched the root OMP as a detached brgr process
from the Codex owner shell; it did not put that root process in its own Herdr
pane. Root OMP pane placement was demonstrated by the earlier
[recursive result receipt](live-recursive-bridge-2026-09-23.md), so the two
pane observations are separate evidence. After the run the named session
again had only its original shell pane `w1:p1`.

## Earlier partial bidirectional runs

Each row below is one exact task attempt. For each boundary, the child sent
a question to its owner, the owner replied, the owner sent a question to the
child, and the child replied. The four stored messages are on the same attempt,
both replies reference the opposite-direction question, and all four messages
have `acknowledged = 1` in the SQLite store.

| Boundary | Task | Child question → owner reply | Owner question → child reply |
| --- | --- | --- | --- |
| Codex ↔ OMP | `8baa69f2-1a7a-4c4b-9137-c439892143e3` | `ROOT_ASK_TOKEN` → `TOKEN_FROM_CODEX` | `ROOT_OWNER_QUESTION` → `ROOT_REPLY_ACK` |
| OMP ↔ GJC | `9f4fcca0-812e-4443-862f-c3332b9c7080` | `LEVEL1_CHILD_ASK` → `LEVEL1_OWNER_REPLY` | `LEVEL1_OWNER_ASK` → `LEVEL1_CHILD_REPLY` |
| GJC ↔ GJC | `4488c7d4-71a4-407a-9537-83f6b7d4e8b1` | `LEVEL2_CHILD_ASK` → `LEVEL2_OWNER_REPLY` | `LEVEL2_OWNER_ASK` → `LEVEL2_CHILD_REPLY` |

The standalone Codex ↔ OMP task produced sealed candidate
`65d76792-ea8e-4f62-ab5e-b6db0d26d06b` with text including
`ROOT_TWO_WAY_OK TOKEN_FROM_CODEX`, artifact SHA-256
`6a111313d4f95f69411c07b5144639a3c447a17ac8c84a16c7657f2fac1af02d`,
and explicit accepted decision `80a28413-f3b4-42a9-bf5a-6dd4b3540f20`.

The nested run opened adjacent panes `w1:pE` (root OMP), `w1:pF` (middle
GJC), and `w1:pG` (leaf GJC). The leaf produced sealed candidate
`f68f249c-25ad-4876-a2b8-02635142f784`, whose exact text was
`LEAF_BIDI_OK BRGR_LIVE_EVIDENCE_OK` and artifact SHA-256 was
`2b4a3d3852908b17a76bcccd3b6f56c82522a072b500550f5ebc2da6065844f3`.
The middle GJC explicitly accepted it with decision
`4e9cead3-7e9a-45a4-a1bb-b9cf5f954627`.

## Limits revealed by the run

In the earlier nested run, the middle GJC hit its 300-second attempt deadline after approving the
leaf. The root OMP launched another depth-one sibling, and that task and the
root were explicitly cancelled. Its traffic proves live two-way exchange at
the nested boundaries but did not complete the root. The accepted rerun above
resolved that specific proof gap with longer parent budgets.

An earlier bidirectional run also exchanged four acknowledged messages at
both nested boundaries, but its leaf launched an unintended depth-three
child. The active descendants were cancelled and their records retained. The
later run used a leaf-specific objective and created no extra leaf descendant.
These observations leave bounded delegation behavior and automatic parent time
budgeting as product work. The successful rerun set those budgets explicitly.

After testing, Herdr's shared plugin registry was restored to
`github:justn-hyeok/brgr@d612a34868759ab385c3800cabb1972adb1ea589`
(v2.0.2). The installed `brgr doctor --json` returned
`status: ok` with four healthy harnesses. The named session now has only its
original shell pane `w1:p1`; test task records and artifacts remain available.
