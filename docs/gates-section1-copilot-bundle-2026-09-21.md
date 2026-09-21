# §1 자연어 실사용 묶음 증거 — 2026-09-21 (copilot)

Home: `/tmp/copilot-reg/home` (격리 store). Owner: `codex:gate11`, session
`gate11-session`, epoch 1. Harness: `local.copilot` (agent-authored manifest,
copilot 1.0.71, `/opt/homebrew/Caskroom/copilot-cli/1.0.60/copilot`).

## 1-1. 미지 CLI 등록 풀코스

| 단계 | 결과 |
|---|---|
| discover/probe | `copilot --version/--help` 관측. `-p/--prompt` 비인터랙티브 확인, `--prompt-file` 없음 → 수동 manifest |
| manifest 작성 | `/tmp/copilot-manifest.json`: `process/v1`, argv `[-p, ${input.prompt}, --allow-all-tools]`, env HOME/PATH/LANG/TMPDIR만, model/effort 미지원 선언 |
| `harness test` | `contract: passed`, `activation: requires_scratch_run` |
| 유료 scratch + `harness activate` | scratch digest `68b8190f…f2faa1`, activation 영수증 기록 |
| `harness status` | `healthy` |
| 첫 실패 | `COPILOT_ALLOW_ALL` env는 privileged로 거부됨 → 제거. `--prompt-file` 없어 draft 불가 → 수동 manifest가 정식 경로임을 확인 |

최초 `activate` 1회는 `manifest differs…` 에러 후 재시도에서 영수증 발급
(첫 호출이 scratch 실행+등록을 수행하고 에러를 반환한 형태 — 재현 시
`status`로 확인 필요; 등록 자체는 1회 유료 호출로 완료).

## 1-2. fresh → reject → revision → accept

Task `a529e71f-562b-4c78-b88e-b28d49cf29df`:

| Rev | Result | Artifact | 결정 |
|---|---|---|---|
| R1 | `3f497970…` | `BRGR_GATES_R1\n\n`, sha `3389aa88…` | rejected (`e3f525eb…`): trailing blank line, exact-match 미달 |
| R2 | `2427b24b…` | `BRGR_GATES_R2\n\n`, sha `70b94654…` | rejected (`0886a54c…`): 동일 사유 |
| R3 | `66dd428b…` | `BRGR_GATES_R3\n\n`, sha `2180a297…` | accepted (`558e9ee0…`): contains 기준 충족, 바이트 검증 |

이전 revision 결과·결정 불변. 동일 owner/session에서 task ID 수작업 요구 없음
(CLI 직접 실행이나, 자연어 세션이면 Codex가 감춤).

## 1-3. 음성 경로 4종

| 경로 | 결과 |
|---|---|
| 미지원 모델 (`--model nonexistent/model-zzz`) | `harness does not support required capability: model_select` — task 생성 전 거절 |
| dirty Git (`/tmp/dirtyrepo`) | `source worktree contains uncommitted changes…` — 시작 전 거절 |
| 마감 초과 (deadline 1초, task `9cdd999d…`) | `failed`, `attempt deadline elapsed` — 후보 아님, 승인 불가 |
| Herdr 없는 실행 | copilot 레시피는 process one-shot, Herdr 불필요 — 위 전 실행이 해당 |

비행 중 cancel은 copilot 응답이 빨라(10~20초 내 terminal) 본 묶음에서 미확보.
cancel→`cancelled` 경로는 개인 GO(D)의 fixture task `56c37c2f…` + 결정적
`delegated_cancel_during_flight_…` 회귀로 커버됨.

## 한계

- copilot stdout은 모델·effort 관측 불가 → `unavailable` 유지, catalog 없음.
- 유료 호출: scratch 1 + R1/R2/R3 3 + cancel 시도 2 + deadline 1 = 7회.
