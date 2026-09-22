# Pane-close 완료 기준 결정 — 2026-09-21

## 결정

공개 v1의 pane 정리 완료 기준은 **협력적 로컬 best-effort**다. 원자적
compare-and-close 보장은 v1 필수 조건에서 제외한다. Owner 승인 2026-09-21.

## 근거

- Herdr 0.9.0의 `pane.close(pane_id)`에는 조건부 identity 필드가 없다. 최종 live
  확인과 close 호출은 분리된 연산이며, 그 사이 다른 actor가 pane을 바꾸는
  race는 brgr 단독으로 제거할 수 없다 (`docs/architecture.md`, `pane_cleanup.rs::close_if_eligible` 문서화됨).
- Herdr API 변경은 brgr 범위 밖이다. brgr가 바꿀 수 없는 외부 API 보장을 v1
  필수 P0로 두는 것은 완료 기준 오류였다 (2026-09-13 초안의 문제).
- Pane 정리는 선택적 `local.omp-herdr` 어댑터의 presentation 영역이다. 관리 실행
  핵심 계약(fresh run → sealed artifact → durable inbox → 명시적 결정)은 pane
  정리 없이 완전히 성립한다.
- 안전장치는 이미 구현되어 있다: brgr launcher 영수증이 있는 pane만 대상,
  owner 결정+inbox ack 후 idle·신원·보호탭 재확인, working/blocked/불일치/`--keep-pane`
  pane은 닫지 않음, worktree 삭제 없음, 실패한 close는 bounded retry로 pending 유지.

## 승인된 v1 한계

- 불확실한 pane은 남긴다. 공유 세션은 `--keep-pane`을 사용한다.
- Check-then-close race window는 문서화된 잔여 위험으로 유지하며, 적대적
  동일 사용자에 대한 원자성·격리는 주장하지 않는다 (v1 신뢰 모델과 일치).

## 재심의 조건

Herdr가 조건부 close(compare-and-close) API를 제공하면 이 결정을 재심의한다.
그때까지 관련 코드는 best-effort 경로만 유지한다.

## 게이트 영향

- 4-2(기준 충돌 해소): 본 결정으로 close.
- 4-1(pane 정리 실물 검사 매트릭스), 4-3(외부 worker `lost` 검증)은 best-effort
  구현을 대상으로 하는 테스트 작업으로 계속 open.
