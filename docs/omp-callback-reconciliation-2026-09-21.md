# OMP 콜백 3-1 대조 기록 — 2026-09-21

감사 범위: 2026-09-14 스냅샷(`omp-callback-migration-audit-2026-09-14.md`)의
brgr 관련 12건 중 잔류분 + 이후 v2 QA 추가분. 전역 일괄 삭제·재생 없음.
읽기전용 조회만 수행, 어떤 pane·계약·outbox도 닫거나 지우지 않음.

## 현재 상태 (2026-09-21 실측)

- 콜백 계약 풀: 150개 파일·77 task. brgr task명 계약은 **0건** (9/14의 12건은 소멸).
- outbox: 159개 계약 디렉토리. 그중 brgr 언급 **9개 계약 디렉토리**.
- 9개 계약의 기록 pane 9개(`w2K:pD`, `w2K:pJ`, `w2K:p7`, `w4T:p3~p5`,
  `w57:p1`, `w56:p1`, `w59:p1`)는 **현재 Herdr pane 20개 중 전부 ABSENT**.
  좀비 pane 없음.

## 계약별 대조

| 계약 | child / task | 상태 | 리포트 | 판정 |
|---|---|---|---|---|
| `0dd7f4ba…` | brgr_plugin_lifecycle / brgr-v2-plugin-lifecycle | cross_workspace_rejected, 리포트 없음 | 없음 | 격리됨. 실패가 실패로 기록, 재전달 없음 |
| `1fe3bb9a…` | brgr_offline_v1 / brgr-offline-pull (`w2K:pD`) | delivered ×3 completion | `/private/tmp/…/p0-review.md` FILE-GONE (tmp 정리) | 전달 완료 기록. tmp 소실은 증거 보존 이슈, store 오염 아님 |
| `38f751e4…` | brgr_board_store / brgr-v2-board-store | delivered 2 + unverified 1 | `/private/tmp/brgr-v2-qa/…` FILE-GONE | 상동 |
| `5773b38d…` | brgr_bridge_hardening / brgr-v2-bridge-hardening | unverified_report + cross_workspace_rejected | 없음 | 격리됨. 미검증은 미검증으로 남음 |
| `9510587f…` | brgr_harness_health / brgr-v2-harness-health | cross_workspace_rejected ×3 | 없음 | 격리됨 |
| `9f6a5020…` | brgr_astra_completion / brgr-v1-completion-criteria (`w2K:p7`) | delivered | worktree 내 `v1-completion-checklist.md` **SHA MATCH** (30752B) | 정상 전달, 바이트 검증됨 |
| `9fd34dcd…` | brgr_worktree_recovery / brgr-v2-worktree-recovery | delivered 2 + unverified 3 | `/private/tmp/brgr-v2-qa/…` FILE-GONE | delivered분은 기록상 완료, tmp 소실은 증거 이슈 |
| `cc5f2e77…` | brgr_recovery_pr8_review / brgr-recovery-pr8-review (`w2K:pJ`) | delivered ×11 completion | worktree 내 `pr13-probe-followup.md` **SHA MATCH** (7038B) | 정상 전달, 바이트 검증됨 |
| `fb6cb35a…` | callback_smoke / brgr-v2-callback-smoke | delivered | `/private/tmp/brgr-v2-qa/…` FILE-GONE | 기록상 완료, tmp 소실은 증거 이슈 |

동일 계약 내 복수 completion(`:1`, `:2`…)은 OMP 중복 turn 알림 형태이며,
brgr result/digest와 별개 레이어다. 아래 store 대조에서 오염 없음을 확인.

## brgr store 대조 (개인 store, 읽기전용)

- tasks/results/decisions/inbox 각 20행.
- spec에 callback/omp-herdr 흔적: 1건 — task `edcb8bb8…` (route `local.omp-herdr`,
  정상 관리 task, result `641751a2…` digest 일치 decision `6063c76b…` 보유).
- 구 OMP 콜백이 brgr result·decision 행으로 유입된 흔적 없음.
- 기존 brgr result/decision 행이 authoritative하게 유지됨.

## 잔여 이슈 (게이트 관점)

1. `/private/tmp` 리포트 6건 FILE-GONE: tmp 휘발성 탓. 콜백 증거가 tmp에 묶인
   운용이 문제 — 재발 방지는 tmp가 아닌 worktree/store 경로 계약이 필요.
   단, brgr 봉인 아티팩트는 store에 별도 보관되므로 관리 결과 무결성과는 무관.
2. 중복 turn 알림(`cc5f2e77…` 11 completion)은 brgr와 무관한 OMP 레이어 현상.
   3-2(단일 완료주체 재현)의 카오스 실험 대상이지, 3-1 대조 대상이 아님.
3. `cross_workspace_rejected`·`unverified_report`는 격리 상태 그대로 보존.
   재생·삭제 금지 원칙 유지.

## 결론

3-1의 "개별 대조·격리·재전달 금지" 요구는 **9개 잔류 계약에 대해 충족**:
어느 것도 새 brgr 결과로 재전달·자동승인되지 않았고, store 오염이 없으며,
살아있는 좀비 pane이 없다. 9/14 스냅샷 이후 소멸한 3건을 포함한 원 12건 중
현재 추적 가능한 잔류분은 전부 대조됨.
