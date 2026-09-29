# WorkBuddy OMP process evidence — 2026-09-14

This is a bounded live proof for the **non-Herdr OMP process adapter**, not a
v1-wide completion claim. The checked-out source was main merge
`3db072cd91cbe14f7f47f309706311691c2b815f` (tree
`4c1b31d9daf6392b73bd58923671a50e731e1da4`). The locally built debug
`brgr` binary SHA-256 was
`3dc4034b672e1dbabfca505c76c6f2537a0ff4d1933d3448c8ee4fedc7884da0`.
Main CI run `34811117447` and tag-free Release run `34811124002` both succeeded
at that merge SHA; the live task itself used the local binary, not a Release
download.

- An isolated non-Git scratch and control home were created under
  `/private/tmp/brgr-workbuddy-omp-process.GiweBW`. `harness add` ran an
  authorized scratch prompt with requested
  `workbuddy/deepseek-v4.1-flash` / `high`. The activation recorded
  `scratch_result_digest=46a2d3136829b5a0f1c0fcdeefe50357b09a05a2a34f3223ac69d8015370062f`;
  `harness status local.omp` returned `healthy`. Its manifest uses `process/v1`
  with argv `-p --mode=json --no-session --no-prewalk --no-extensions --no-title`
  and does not allow `HERDR_ENV` into the child.
- Detached task `3947a85d-785c-495b-8a54-8c7516fa1dec`, revision 1,
  requested the same model/effort with a 120-second deadline and criterion
  “sealed text is exactly `BRGR_WORKBUDDY_OMP_PROCESS_OK`”. It produced candidate
  result `917e2484-9378-46f6-8d5b-eaa7097e9f10`, attempt
  `14679c79-ce3b-4be7-9ab9-e54c7791fdaf`.
- `brgr result` returned exactly `BRGR_WORKBUDDY_OMP_PROCESS_OK` (29 bytes).
  Independently hashing the stored artifact matched
  `sha256:7bb8f900c0f41c66573843f6da8730c2a2bd42347bd52e117c2890d618e5d027`.
  A read-only SQLite query found one owner inbox row with `acknowledged=0`.
  After explicit Codex `accept`, the same row had `acknowledged=1` and the
  decision was `accepted` with result digest
  `sha256:317e8bdf0f12e426612e92ba08b3f03da7ad88ef59226f2ac765b784c11c1ed8`.

The task receipt proves the requested selector was passed through the
activated recipe and a real response met the criterion. It does **not** prove
the model OMP actually selected internally or that OMP did not fall back;
there is no native observed-model field in this brgr result. The activation
scratch stores a digest and success status, not its reviewable text. Those
remain separate certification gaps.
