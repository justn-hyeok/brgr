# brgr v1 실사용 완료 체크리스트

증거 기준: 2026-09-14 공개 태그 `v1.0.7`, main `f7eb7d27831f6e88cb42e88171eb0a8025d6ec22`. 이 문서를 포함하는 `v1.0.8`은 실행 계약을 바꾸지 않는 문서 배포다. **판정: 전체 v1 NO-GO.** 공개 패치가 존재한다는 사실과 자연어 기반 실사용 완료는 별개다. `[x]`는 적힌 범위만 증명하며, 빈 칸은 같은 최종 릴리스 SHA에서 통과해야 하는 게이트다. 2026-09-13의 옛 SHA 감사는 역사적 기록이며, 그때의 체크 수를 현재 진행률로 사용하지 않는다.

범위: 신뢰한 로컬 실행 파일, Apple Silicon macOS 15+, 서명·공증 없는 배포. Herdr는 선택적 화면 어댑터다. provider 측 undo, 세션 재사용, 실행 중 지시 변경, 적대적 동일 사용자 격리, Intel/Linux/Windows 및 Apple Developer ID 서명·공증은 v1 대상이 아니다. 지원하지 않는 요청은 조용히 대체하지 않고 거절해야 한다.

## 이미 증명된 범위

- [x] 범용 process 레시피와 네 하네스(OMP, GJC, Cursor CLI, Command Code)가 있다. 별도 로컬 후보 바이너리에서 각 Luna 최저 설정으로 유료 scratch 등록→fresh run→봉인된 결과→owner inbox→명시적 승인을 검증했다. [실모델 영수증](live-four-harness-luna-min-2026-09-14.md). OMP·GJC의 모델만 네이티브 로그로 확인됐고, 어느 하네스도 실제 effort를 증명하지 않는다.
- [x] 결정적 fixture가 미지의 CLI manifest 등록·승인, 잘못된 모델의 작업 생성 전 거절, 오프라인 owner 재수거, 수정 revision, 취소, dirty Git 거절, 결정/ack 원자성 등을 검사한다. [CLI 계약 테스트](../crates/brgr-cli/tests/cli_contract.rs), [crash-window 증거](crash-window-evidence-2026-09-14.md).
- [x] 다섯 detached supervisor crash window와 DB busy/트랜잭션 롤백, 결과 파일 경계·symlink 검사가 있다. 이는 디스크 고갈이나 모든 동시성 경로까지 증명하지 않는다. [crash-window 증거](crash-window-evidence-2026-09-14.md).
- [x] `v1.0.7`은 main CI와 태그 Release workflow를 통과했고, 공개 archive·checksums·SPDX SBOM을 비인증 다운로드해 검증했다. arm64/macOS 15 최소 버전과 로컬 설치를 확인했으며, 공개 바이너리의 Command Code `low` 경로도 task `f9b35f98-1d0f-40b9-881f-b0dcb3d2d157` → result `98e4639b-3907-4958-ae32-a4721d95e96d` → decision `2aa8fc47-3660-4d10-8c92-2e8cb510c1a0`으로 재실행했다. 아티팩트 SHA-256은 `a93c3dd7b7534e128bc62af78a268aefc32af05d3d9dd6f0dba5e6b3060538af`이고 로컬 증거는 `/private/tmp/brgr-v107-public.ybg6og/live-command/home`에 남아 있다. [main CI](https://github.com/justn-hyeok/brgr/actions/runs/34823024848), [태그 빌드](https://github.com/justn-hyeok/brgr/actions/runs/34823252430), [공개 Release](https://github.com/justn-hyeok/brgr/releases/tag/v1.0.7).
- [x] 새 brgr 관리 OMP는 기존 parent callback을 켜지 않는다. Herdr pane 정리는 owner·세션·idle·결정·ack를 재확인하는 **best-effort**이며 `--keep-pane`이 있다. 원자적 compare-and-close는 보장하지 않는다. [콜백 감사](omp-callback-migration-audit-2026-09-14.md), [README](../README.md).

## 남은 게이트 — 중요도순

### 1. 자연어 실사용 한 바퀴 (제품 P0)

- [ ] **새 Codex 세션**에서 자연어 요청만으로 정확한 하네스·Luna 최저 설정·실제 완료 기준을 잡고, 승인된 미지의 CLI를 discover→probe→manifest→contract test→유료 scratch→activate→health-check로 등록한다. 코어 수정이나 사용자에게 숨은 task ID 수작업을 요구하지 않는다.
- [ ] 같은 실제 흐름에서 fresh run→봉인→오프라인/바쁜 부모의 inbox pull→근거 있는 reject→동일 task의 새 revision→accept를 수행한다. 이전 revision의 결과·결정은 바뀌지 않고, owner가 다른 세션은 읽거나 승인하지 못한다.
- [ ] Stop/cancel, 지원하지 않는 모델, dirty Git 거절과 명시적 clean-HEAD 선택, Herdr 없는 실행을 같은 실사용 시나리오의 음성 경로로 기록한다. 대화·요청 모델/effort·task/revision/result ID·아티팩트 해시·결정·실패 관찰을 최종 SHA에 묶는다. 단위/fixture 테스트만으로 이 칸을 체크하지 않는다.

### 2. 장애·최종 품질·배포 (안전 P0, 출시 P1)

- [ ] ENOSPC를 아티팩트 봉인과 SQLite 커밋 양쪽에 주입하고, 동시 cancel/reconcile 및 재시작을 검사한다. 불완전한 참조·중복 inbox·불확실한 실행의 겹친 재시도가 없어야 한다.
- [ ] OMP+Herdr의 spawn 영수증→첫 agent 조회 사이에서 pane/terminal/session을 교체한 재현 테스트를 실행한다. mismatch는 prompt 전에 실패하고 같은 신원은 정상 진행해야 한다. 현재는 함수 단위 테스트와 실제 정상 실행 증거만 있다. [revision 재생 증거](herdr-revision-replay-evidence-2026-09-14.md).
- [ ] **다운로드한 동일 공개 바이너리**로 네 하네스의 최저-effort fresh run→봉인→inbox→결정을 다시 묶는다. 현재 4종 영수증은 로컬 후보 빌드이고, 공개 `v1.0.7`에서 재실행된 새 경로는 Command Code뿐이다. Cursor CLI/Command Code의 provider 측 모델·effort 관측 불가를 명시적으로 유지한다.
- [ ] 별도 깨끗한 macOS 15 arm64 환경에서 archive·체크섬·SBOM·설치·Gatekeeper의 앱별 허용 경로·fixture 실행을 확인한다. 전역 보안 해제나 자동 quarantine 제거를 요구하지 않는다. Apple Developer ID 서명·공증은 완료 조건이 아니다.
- [ ] 최종 코드와 음성 경로를 작성자가 아닌 리뷰어가 **같은 릴리스 SHA**에서 G1–G4 전체 범위로 검토하고, 남은 P0/P1이 0건임을 확인한다. 좁은 PR/수정별 리뷰나 초록색 CI만으로 대체하지 않는다.

### 3. 기존 OMP 콜백 이행 (중복·오인 방지 P0)

- [ ] 기존 run별 contract, immutable child session, report 해시, parent 전달 기록을 개별 대조한다. pending/`delivery_unknown`은 격리하고 새 brgr 결과를 재전달·자동 승인하지 않는다. 전역 outbox 일괄 삭제·재생은 금지한다. [2026-09-14 재고 스냅샷](omp-callback-migration-audit-2026-09-14.md)은 drain 완료 증거가 아니다.
- [ ] 새 brgr 관리 작업의 완료 주체가 재시작 후에도 하나뿐임을 재현한다. 오래된 OMP worker에서 같은 보고서가 여러 turn 완료 알림으로 도착하는 현상은 brgr task 결과와 구분하고, 중복 알림을 안전하게 종결한다.

### 4. 선택적 Herdr 안전성 (어댑터 P0)

- [ ] 실제 소유 pane와 fixture로 accept/reject 후 정리, `--keep-pane`, working/blocked, 신원·tab 변경, 부모 부재, 닫기 실패, 닫은 직후 crash를 검사한다. 사용자·공유·보호된 pane이나 worktree는 닫히거나 삭제되면 안 된다.
- [ ] **완료 기준 충돌을 해소한다.** 2026-09-13 초안은 원자적 pane 닫기를 필수 P0로 적었지만 현재 Herdr 0.9 API와 README는 brgr 단독 best-effort만 보장한다. 이를 조용히 `[x]`로 바꾸지 말고, 원자적 보장이 필요한지 또는 협력적 로컬 best-effort가 v1의 승인된 한계인지 최종 기준에 명시한다. 불확실한 pane은 유지한다.
- [ ] 선택적 Herdr 외부 worker의 cancel/deadline이 실제 중지를 증명하지 못할 때 `lost`와 미해결 외부 효과로 보고되는지 검증한다. 로컬 wrapper 종료를 provider 측 undo나 worker 종료로 표현하지 않는다.

## GO 판정

- [ ] 위 필수 게이트가 **하나의 최종 릴리스 SHA**와 검증 가능한 영수증으로 닫히고 P0/P1이 남지 않는다. 그 SHA의 locked all-feature tests, doc tests, fmt, all-target clippy, release build, 공개 자산 검증을 다시 통과한다. 성공한 이전 태그를 새 변경의 증거로 재사용하지 않는다.

이 문서는 작업 목록이다. 체크되지 않은 경로를 “완료”나 임의의 퍼센트로 환산하지 않는다.
