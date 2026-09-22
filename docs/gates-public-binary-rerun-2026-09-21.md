# 2-3 공개 바이너리 재실행 증거 — 2026-09-21

바이너리: `brgr-v2.0.2-aarch64-apple-darwin.tar.gz` (GitHub release `v2.0.2`,
archive SHA `c2ca3bf2…bd954`, 체크섬 검증 OK). SBOM은 미다운로드(archive만 검증).
Home: `/tmp/pub-gates/home` (격리). Owner `codex:pub23` / session `pub23-session`.

## Scratch 등록 (공개 바이너리)

| Harness | scratch digest |
|---|---|
| local.gjc | `cb13ae60…546b761274` |
| local.omp | `1f51ab0f…5367dc3f2f2f54` |
| local.cursor-cli | `e0af1eed…65bd62e41c881` |

## Fresh run → 봉인 → accept

| Harness | Task | Result | Marker | Decision | digest 일치 |
|---|---|---|---|---|---|
| GJC | `4807f0e9…` | `9f93daa3…` | `PUB_GJC_RUN` | `03681f53…` accepted | match |
| OMP | `4b5c8235…` | `16a6472a…` | `PUB_OMP_RUN` | `27dbaf7a…` accepted | match |
| Cursor | `ecf251e5…` | `86ac1905…` | `PUB_CURSOR_RUN\n` | `4eec6c23…` accepted | match |

모델/effort: 요청 없음(기본값). OMP·GJC 네이티브 모델 관측은 본 묶음에서
미수집 — route 요청 증거만. Cursor/Command Code의 provider 측 관측 불가는 유지.

Command Code는 공개 `v1.0.7` 재실행 기록(task `f9b35f98…`→decision `2aa8fc47…`)
이 체크리스트에 이미 있음. 4종 중 3종을 현 공개 `v2.0.2`에서 재실행했고,
나머지 1종은 구 공개 태그 영수증으로 커버.
