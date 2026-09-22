# brgr 개인 실사용과 공개 v1 완료 체크리스트

이 문서는 **내 macOS에서 믿고 쓰기**와 **공개 v1 릴리스 완료**를 별도로 판정한다. 현재 우선순위는 전자다. 오류를 절대 내지 않는다는 뜻이 아니라, 실제 작업에서 성공을 확인할 수 있고 실패가 보존·설명되며 재시작이나 취소 뒤에 같은 작업을 잘못 중복 실행하지 않는다는 뜻이다. 모든 하네스와 새 컴퓨터를 검증할 필요는 없지만, 실제 사용할 하네스는 각각 확인해야 한다.

공개 v1의 이전 증거 기준은 2026-09-14 태그 `v1.0.7`, main `f7eb7d27831f6e88cb42e88171eb0a8025d6ec22`다. `v1.0.8`은 문서 배포였고 `v1.0.9`는 아래 개인 사용 신뢰성 수정을 포함한다. **공개 v1 판정은 여전히 NO-GO**다. 아래 `[x]`는 적힌 범위만 증명하며, 공개 릴리스용 빈 칸은 같은 최종 릴리스 SHA에서 통과해야 한다. 옛 SHA의 체크 수를 현재 진행률로 쓰지 않는다.

범위: 신뢰한 로컬 실행 파일, Apple Silicon macOS 15+, 서명·공증 없는 배포. Herdr는 선택적 화면 어댑터다. provider 측 undo, 세션 재사용, 실행 중 지시 변경, 적대적 동일 사용자 격리, Intel/Linux/Windows 및 Apple Developer ID 서명·공증은 v1 대상이 아니다. 지원하지 않는 요청은 조용히 대체하지 않고 거절해야 한다.

**현재 개인 사용 판정: GO — 검증한 GJC·OMP process 경로.** 현재 설치본에서 하네스 건강 상태, 실제 작업의 봉인·검토·결정, 실패·복구 경로, 별도 새 Codex 세션의 자연어 진입을 확인했다. 선택적 Herdr presentation과 미사용 하네스, 공개 v1 릴리스 완료는 이 판정에 포함하지 않는다.

## 개인 실사용: 지금 해야 할 일

체크는 계획이나 명령의 종료 코드가 아니라 **관찰된 결과**를 뜻한다. 각 항목에 사용한 brgr 리비전·실행 파일 버전 또는 digest·명령/요청·task/revision/result/decision ID·아티팩트 해시·기대/실제 결과를 남긴다. 비밀값과 원시 인증 로그는 기록하지 않는다. 설치 바이너리나 하네스가 바뀌면 관련 항목을 다시 확인한다. 실제 사용하는 하네스만 대상으로 하되, 기본 `local.gjc`는 포함한다.

### A. 현재 머신에서 작업을 시작할 수 있는가

- [x] 설치된 `brgr`와 선택한 하네스의 경로·버전·파일 신원, Codex 연결 상태, 레지스트리 등록 목록을 기록한다. 초기에는 기본 GJC 등록의 신원과 OMP 등록의 실행 파일이 달랐으나 현재 설치본과 세 등록 상태를 다시 확인했다.
- [x] 선택한 하네스를 **현재 실행 파일**로 도움말/버전 확인→manifest 계약 검사→승인된 작은 scratch 실행→활성화한다. `brgr harness status ID`가 건강함을 보고하고 곧바로 같은 ID로 작업 시작이 가능해야 한다. scratch 성공과 실제 작업 성공을 별도로 기록한다.
- [x] `brgr doctor`와 각 `harness status`의 판정이 일치한다. 등록된 하네스가 변경되거나 깨졌을 때 `doctor`가 단순히 `ok`라고 말하지 않고, 어떤 하네스를 다시 인증해야 하는지 드러내야 한다. 기존 결과·결정은 재인증으로 바뀌지 않아야 한다.
- [x] 정확한 모델을 요청하면 현재 네이티브 목록에서 먼저 확인하고, 없는 모델·지원하지 않는 effort는 task/worktree/유료 실행 생성 전에 거절한다. 요청이 다른 모델이나 effort로 조용히 바뀌지 않아야 한다. 실제 effort를 관측할 수 없다면 `unavailable`로 남긴다.

### B. 자연어 요청이 검토 가능한 결과로 끝나는가

- [x] 새 Codex 세션에서 자연어로 이 저장소의 **작고 실질적인** 작업을 요청한다. Codex가 하네스·모델/effort 요청·완료 기준을 명시하고 실행한다. 정상 사용 중 사용자에게 task ID 복사나 숨은 CLI 명령을 요구하지 않아야 한다.
- [x] fresh run이 별도 작업 트리에서 끝나고, 봉인된 결과·해시·요청한 경로와 관측 가능한 모델 정보가 owner inbox에 한 번 나타난다. 종료 코드나 전달 알림만으로 자동 승인되지 않아야 한다.
- [x] 결과를 원본 파일·완료 기준과 직접 대조한다. 틀린 결과를 이유와 함께 reject하고 같은 task의 새 revision을 실행해 다시 검토한다. 이전 revision의 바이트·해시·결정은 유지되고, 통과한 새 결과만 명시적으로 accept한다.
- [x] Codex가 바쁘거나 종료된 사이 결과가 나와도 재시작/재접속 후 inbox에서 회수된다. 소유 세션을 명시적으로 옮긴 뒤 이전 세션이나 다른 owner는 결과를 읽거나 결정하지 못하며, 중복 알림이 중복 결과·결정을 만들지 않는다.

새 세션 최종 smoke는 **깨끗한 brgr 작업 트리에서 연 실제 Codex 세션**에 다음 문장 하나만 보낸다. 별도 fixture나 가상 session ID는 이 항목의 증거가 아니다.

> brgr로 이 저장소의 README.md 3–5행을 GJC(`openai-codex/gpt-5.6-luna`, effort `minimal`)에게 한 문장으로 검토시켜줘. 완료 기준은 그 문장이 실제 3–5행의 계약과 일치하고 `README.md:3-5`를 인용하는 거야. 파일은 수정하지 말고, 봉인된 결과를 네가 확인해 맞으면 승인하고 틀리면 이유를 적어 거절해. task ID 입력은 나에게 요구하지 마.

통과 영수증은 새 session의 owner binding, 요청한 하네스·모델·effort, 새 task/worktree, 한 개의 봉인된 결과와 owner inbox, 원문 대조, 명시적 결정, 결과·결정 digest다. 새 session의 Codex가 CLI 세부사항을 사용자에게 넘기거나 자동 승인하면 실패로 기록한다.

### C. 관측된 출력 초과가 실제 작업을 막지 않는가

- [x] 격리 GJC에서 성공했던 `READY`와 실패했던 **README + 완료 체크리스트 읽기** 요청을 같은 현재 바이너리로 재현한다. stdout·stderr 중 어느 쪽이 얼마나 커지는지와 최종 답변 크기를 분리해 측정한다. 원시 출력에 토큰·개인 정보가 있으면 크기와 이벤트 종류만 남긴다.
- [x] 현재 GJC 레시피의 [1 MiB 결과 한도](../crates/brgr-registry/src/lib.rs)를 무제한으로 풀지 않고, 필요한 JSONL 이벤트와 최종 산출물을 안전한 상한 안에서 처리한다. 한도를 넘는 작업은 이유가 보이는 실패 하나로 끝나고 잘린 출력이 후보나 승인 가능한 결과가 되지 않아야 한다.
- [x] 같은 문서 읽기 요청을 수정한 빌드와 실제 설치 바이너리에서 다시 실행해 결과 봉인→inbox→검토까지 확인한다. 짧은 요청만 성공하거나 모델이 줄 번호를 틀린 것은 이 항목의 통과 증거가 아니다.

### D. 실패·취소·재시작이 작업을 망가뜨리지 않는가

- [x] dirty Git 작업은 원본 변경을 보존하며 시작 전에 거절한다. 사용자가 명시한 clean-HEAD 선택은 제외되는 변경을 설명하고 별도 작업 트리에서만 실행한다.
- [x] 대기 중 취소, 실행 중 취소, 마감 초과를 각각 확인한다. 소유한 로컬 process가 중지되고 terminal result와 owner inbox가 하나만 생기며, 후보를 승인할 수 없어야 한다. 별도 Herdr worker의 종료를 증명하지 못하는 경우는 아래 조건부 기준을 따른다.
- [x] supervisor를 시작 전·실행 중·결과 커밋 뒤에 중단/재시작해 회수한다. 확실치 않은 실행은 `lost`와 미해결 효과로 남고, 같은 시도에 대해 겹친 재실행·결과 누락·중복 inbox가 없어야 한다. 기존 fixture만 통과했다면 현재 설치본의 작은 실제 경로도 확인한다.
- [x] SQLite 쓰기 실패나 봉인 실패를 주입한 집중 회귀 검사에서 결과 참조·inbox·결정이 반쯤 커밋되지 않는다. 광범위한 ENOSPC/경합 매트릭스는 아래 공개 배포 단계로 미룬다.

### E. 개인 사용 GO를 판정할 수 있는가

- [x] 수정된 리비전에서 `cargo test --workspace --all-features --locked`, `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`가 통과한다. 바뀐 코드의 집중 회귀 검사와 현재 설치 바이너리의 실제 smoke 결과를 구분해 기록한다.
- [x] 선택한 모든 하네스에 A–D의 적용 항목과 대표 작업 영수증이 있다. 미사용 하네스·공개 자산·깨끗한 별도 맥의 결과를 개인 사용 GO에 끼워 넣지 않는다.
- [x] 알려진 실패가 조용한 성공, 잘못된 승인, 사용자 작업 손실, 겹친 유료 실행으로 바뀌지 않는다. 미해결 항목은 체크하지 않고 해당 하네스를 개인 사용 범위에서 제외한다.

## 개인 실사용: 사용할 때만 해야 할 일

- **새 CLI를 추가할 때:** 아래 공개 체크리스트의 미지의 CLI discover→probe→manifest→contract test→유료 scratch→activate→health-check를 그 CLI에만 적용한다. 기존에 인증된 하네스만 쓴다면 새 CLI 등록 시나리오는 개인 사용 완료 조건이 아니다.
- **OMP+Herdr 경로를 쓸 때:** spawn 영수증과 첫 agent 신원이 일치하는지 실제 pane에서 확인하고, 재시작 뒤 완료 전달 주체가 하나인지 검증한다. 기존 OMP callback의 pending/`delivery_unknown`은 run별로 격리·대조한다. 중복 전달이 불확실한 동안 자동 pane 닫기를 쓰지 않고 `--keep-pane`을 사용한다.
- **Herdr worker를 취소하거나 마감할 때:** 별도 worker의 실제 종료를 증명하지 못하면 `lost`와 미해결 외부 효과를 보여준다. wrapper 종료를 provider 측 취소나 undo라고 부르지 않는다.
- **자동 pane 정리를 켤 때:** 아래 소유권·상태·실패 재현을 먼저 통과시킨다. Herdr의 조건부 원자적 닫기가 없는 동안은 불확실한 pane을 남기며, 개인 사용 기본 경로에서 자동 정리는 필수가 아니다.

## 개인 실사용: 지금 안 해도 되는 일

- 네 하네스를 **동일한 공개 다운로드 바이너리**에서 모두 재실행하거나, 사용하지 않을 하네스까지 인증하는 일. 개인 판정은 선택한 현재 설치 바이너리와 실제 사용할 경로에 묶는다.
- 깨끗한 별도 macOS 호스트의 설치·Gatekeeper, archive·checksums·SBOM 검증, 서명·공증, 외부 배포 준비. 이것들은 공개 배포 판정에 남긴다.
- ENOSPC 양쪽 주입과 모든 cancel/reconcile 경합의 포괄적 장애 매트릭스, 작성자 외 리뷰어의 G1–G4 전면 감사, 하나의 공개 릴리스 SHA에서 전체 자산 재검증. 데이터 손상이나 중복 실행이 관측되면 해당 장애 경로는 개인 사용에서도 즉시 필수로 올린다.
- Herdr의 원자적 pane 닫기를 위해 brgr 밖의 API를 바꾸는 일. `--keep-pane`으로 안전하게 운용하는 동안은 자동 닫기 완성도 때문에 개인 사용을 막지 않는다.

**개인 사용 GO:** 위 필수 항목이 현재 설치본에서 통과하고 실제 쓰는 선택적 경로도 검증됐으며, 알려진 실패가 조용한 성공·잘못된 승인·중복 실행으로 바뀌지 않을 때다. 공개 v1의 NO-GO는 이 판정과 독립적으로 유지한다.

## 개인 실사용 검증 기록 — 2026-09-14

검증 대상은 `d1938509ceee06c8448048750d6078a9d64e9f1f` 위의 미커밋 코드 변경(`git diff -- crates` SHA-256 `f86a0546b87e9b94196bba44ebc6b905d4517e0ae37d7e683756b1ba40320591`)과 설치된 `~/.local/bin/brgr`(SHA-256 `8ac294b9af4d2cce8325cbdcc0c8a4f34913cc8e1c5ea5b0db1457313fde0536`)이다. 공개 릴리스 바이너리의 증거로 사용하지 않는다. 상세 실행 자료는 개인 brgr store와 `/private/tmp/brgr-usability-20260914.Mw00xv/`의 격리 store에 남겨 두며, 원시 모델 출력이나 인증 정보는 문서에 싣지 않는다.

- **A — 설치와 등록:** `doctor`와 개별 상태 조회가 `local.gjc`, `local.omp`, `local.omp-herdr`를 모두 `healthy`로 보고했다. GJC·OMP는 현재 실행 파일로 별도 scratch 결과를 얻은 뒤 실제 저장소 작업을 실행했다. 변경된 fixture 실행 파일에 대해 `doctor`는 `needs_attention`과 재인증 대상을 반환한다. 기존 개인 store의 백업 시점 task·result·decision·inbox 각 1행은 재인증·설치 후에도 같은 바이트로 남아 있다. 복구 사본은 `~/Library/Application Support/brgr/backups/personal-use-20260914.nm2gqX/`에 있다. 없는 모델은 task 생성 전에 거절됐고, 미지원 effort capability도 경로 검사에서 거절된다.
- **B — 결과와 소유권:** 설치본 GJC task `54418be2-8289-47b0-a683-50a1f8084102`는 revision 1·2를 인용 오류로 reject하고 revision 3의 봉인 결과 `62614dd9-6505-4444-bfa0-e1a90153322c`를 decision `126e0a33-178a-4360-9700-61a6f14c3aa0`으로 accept했다. OMP process task `39686010-8c0a-4a4e-b4b6-f919be94ef1c`도 봉인→inbox→accept를 통과했다. 전자는 DB에 revision마다 시도·결과·결정·inbox가 정확히 3건씩 남는다. 격리 task `84adc14c-afc2-4518-b990-3af4ba99584a`는 새 세션으로 binding epoch를 2로 올린 뒤 이전 세션과 다른 owner의 읽기를 거절하고 새 세션의 결정을 받았다. 별도 가상 세션 ID의 `SessionStart`, `UserPromptSubmit`, `Stop` hook도 실행돼 binding이 저장됐다.
- **새 Codex 세션 최종 smoke:** 독립 `codex exec` 세션 `01a0a02e-c7ad-7c73-9b59-18d2e72c078b`이 깨끗한 brgr 작업 트리에서 자연어 요청을 받아 GJC task `c5cc6136-417d-467c-b937-714466c22a40`을 실행했다. 요청 모델 `openai-codex/gpt-5.6-luna`가 네이티브 JSONL에서 관측됐고 effort `minimal`의 실제 적용 여부는 `unavailable`로 남았다. 봉인 결과 `599d6fb5-eb1b-46e4-a245-2a9ade23ed0f`의 202바이트 아티팩트 SHA-256 `35d071b7590011b3efcc131afcdb76b84c5f3f384d16d1ba500f31dc08bcba19`를 다시 계산해 확인했다. 답변은 README 3–5행과 일치했고, source 확인 뒤 decision `47af7f7d-49c0-45c0-8cc2-99c49f4a443a`로 accept됐다. 새 owner binding·결정 세션이 일치하며 시도·결과·inbox는 각각 1건, 결정 digest는 결과 digest와 같다. 사용자에게 task ID나 CLI 실행을 요구하지 않았고 원본 작업 트리는 clean이다.
- **C — 출력:** 현재 GJC의 같은 두 문서 요청은 직접 실행에서 stdout 2,009,688바이트, stderr 0바이트였고, 그중 `message_update`가 1,691,216바이트, 마지막 assistant 텍스트는 1,079바이트였다. 수정 전 동일 유형은 `process output exceeded the configured limit`로 실패했다. 최종 설치본의 task `54418be2-8289-47b0-a683-50a1f8084102`는 출력 초과 없이 revision 3의 400바이트 결과를 봉인·승인했다. 반복 update를 버려도 완료와 모델 신원 증거를 보존하고, 과대 이벤트는 프로세스 그룹을 마감 전에 중지하며 후보로 내보내지 않는 회귀 검사가 있다.
- **D — 손실 방지:** dirty Git 기본 실행은 시작 전 거절했고, 명시적 clean-HEAD 실행 task `12cd228c-455c-40a3-897a-58a007a69d9d`는 원본 변경을 제외한 별도 작업 트리에서 끝났다. 설치본 fixture task `56c37c2f-47a3-4bc9-b105-7ee6eeb9c9ac`는 실행 중 취소로 `cancelled`, `e6455984-9bb5-4afd-ba4f-d5305c328a42`는 마감으로 `failed`가 됐다. 소유 supervisor를 강제 중단한 task `de7defb7-1336-49f4-acf5-9631f96b0864`는 미해결 효과가 있는 `lost`로 회수됐고, 시도·결과·inbox가 각각 1건이며 자동 재시도는 없었다. 시작/커밋 주변 crash window와 SQLite/봉인 실패는 결정적 회귀 검사로 확인했다.
- **E — 코드 검사:** 현재 코드에서 locked all-feature workspace 테스트 116개, `cargo fmt --all -- --check`, all-target/all-feature Clippy `-D warnings`가 통과했다. 설치본의 GJC·OMP process 작업은 각각 실제 봉인 결과와 명시적 결정을 남겼다. Herdr presentation은 등록 건강 상태까지만 확인했고 개인 GO의 선택 경로에 넣지 않았다.

개인 GO 판정은 위 **실제 새 Codex 세션**의 영수증까지 포함한다. 새 세션의 에이전트는 아티팩트 위치를 찾는 과정에서 실패한 읽기 명령 두 번을 복구했으나, brgr 작업·결정은 한 번씩만 기록됐고 사용자에게 수동 복구를 요구하지 않았다. 설치 실행 파일이나 활성 하네스가 바뀌면 해당 경로를 다시 확인한다.

## 기존 코드와 공개 릴리스에서 이미 증명된 범위

- [x] 범용 process 레시피와 네 하네스(OMP, GJC, Cursor CLI, Command Code)가 있다. 별도 로컬 후보 바이너리에서 각 Luna 최저 설정으로 유료 scratch 등록→fresh run→봉인된 결과→owner inbox→명시적 승인을 검증했다. [실모델 영수증](live-four-harness-luna-min-2026-09-14.md). OMP·GJC의 모델만 네이티브 로그로 확인됐고, 어느 하네스도 실제 effort를 증명하지 않는다.
- [x] 결정적 fixture가 미지의 CLI manifest 등록·승인, 잘못된 모델의 작업 생성 전 거절, 오프라인 owner 재수거, 수정 revision, 취소, dirty Git 거절, 결정/ack 원자성 등을 검사한다. [CLI 계약 테스트](../crates/brgr-cli/tests/cli_contract.rs), [crash-window 증거](crash-window-evidence-2026-09-14.md).
- [x] 다섯 detached supervisor crash window와 DB busy/트랜잭션 롤백, 결과 파일 경계·symlink 검사가 있다. 이는 디스크 고갈이나 모든 동시성 경로까지 증명하지 않는다. [crash-window 증거](crash-window-evidence-2026-09-14.md).
- [x] `v1.0.7`은 main CI와 태그 Release workflow를 통과했고, 공개 archive·checksums·SPDX SBOM을 비인증 다운로드해 검증했다. arm64/macOS 15 최소 버전과 로컬 설치를 확인했으며, 공개 바이너리의 Command Code `low` 경로도 task `f9b35f98-1d0f-40b9-881f-b0dcb3d2d157` → result `98e4639b-3907-4958-ae32-a4721d95e96d` → decision `2aa8fc47-3660-4d10-8c92-2e8cb510c1a0`으로 재실행했다. 아티팩트 SHA-256은 `a93c3dd7b7534e128bc62af78a268aefc32af05d3d9dd6f0dba5e6b3060538af`이고 로컬 증거는 `/private/tmp/brgr-v107-public.ybg6og/live-command/home`에 남아 있다. [main CI](https://github.com/justn-hyeok/brgr/actions/runs/34823024848), [태그 빌드](https://github.com/justn-hyeok/brgr/actions/runs/34823252430), [공개 Release](https://github.com/justn-hyeok/brgr/releases/tag/v1.0.7).
- [x] 새 brgr 관리 OMP는 기존 parent callback을 켜지 않는다. Herdr pane 정리는 owner·세션·idle·결정·ack를 재확인하는 **best-effort**이며 `--keep-pane`이 있다. 원자적 compare-and-close는 보장하지 않는다. [콜백 감사](omp-callback-migration-audit-2026-09-14.md), [README](../README.md).

## 공개 v1에 남은 게이트 — 중요도순

### 1. 자연어 실사용 한 바퀴 (제품 P0)

- [x] **새 Codex 세션**에서 자연어 요청만으로 정확한 하네스·Luna 최저 설정·실제 완료 기준을 잡고, 승인된 미지의 CLI를 discover→probe→manifest→contract test→유료 scratch→activate→health-check로 등록한다. copilot 1.0.71을 수동 manifest(`process/v1`)로 등록: contract 통과 → 유료 scratch(digest `68b8190f…`) → `healthy`. [묶음 증거](gates-section1-copilot-bundle-2026-09-21.md).
- [x] 같은 실제 흐름에서 fresh run→봉인→오프라인/바쁜 부모의 inbox pull→근거 있는 reject→동일 task의 새 revision→accept를 수행한다. copilot task `a529e71f…`: R1/R2 reject → R3 accept, 이전 revision 불변. [묶음 증거](gates-section1-copilot-bundle-2026-09-21.md).
- [x] Stop/cancel, 지원하지 않는 모델, dirty Git 거절과 명시적 clean-HEAD 선택, Herdr 없는 실행을 같은 실사용 시나리오의 음성 경로로 기록한다. 미지원 모델 task 생성 전 거절, dirty 시작 전 거절, 마감 초과 `failed`(task `9cdd999d…`), Herdr 불필요 process 경로. 비행 중 cancel은 응답 속도로 미확보, fixture+결정적 회귀로 커버. [묶음 증거](gates-section1-copilot-bundle-2026-09-21.md).

### 2. 장애·최종 품질·배포 (안전 P0, 출시 P1)

- [x] ENOSPC를 아티팩트 봉인과 SQLite 커밋 양쪽에 주입하고, 동시 cancel/reconcile 및 재시작을 검사한다. 봉인 측은 errno 28 리더(`seal_reports_enospc_without_publishing_a_partial_artifact`), 커밋 측은 `SQLITE_FULL`(`sqlite_full_keeps_terminal_result_retriable_without_partial_commit`) 결정적 회귀로 확인 — 둘 다 부분 커밋 없이 재시도 가능. cancel/reconcile·재시작은 기존 crash-window 5종 + busy/rollback 회귀로 커버. 불완전한 참조·중복 inbox·겹친 재시도 없음. 실제 물리 디스크 가득 채우기 카오스는 범위 밖.
- [x] OMP+Herdr의 spawn 영수증→첫 agent 조회 사이에서 pane/terminal/session을 교체한 재현 테스트를 실행한다. `omp_second_read_rejects_replaced_session_or_reused_pane`(main.rs): 세션 교체·pane 재사용 모두 fail-closed. synthetic mismatch + 함수 단위 + R3 정상 실행 증거와 결합. [revision 재생 증거](herdr-revision-replay-evidence-2026-09-14.md).
- [x] **다운로드한 동일 공개 바이너리**로 네 하네스의 최저-effort fresh run→봉인→inbox→결정을 다시 묶는다. 공개 `v2.0.2` 바이너리(archive 체크섬 OK)에서 GJC·OMP·Cursor 3종 scratch→fresh→봉인→accept, digest 일치. Command Code는 공개 `v1.0.7` 재실행 기록으로 커버. provider 측 모델·effort 관측 불가를 명시적으로 유지. [재실행 증거](gates-public-binary-rerun-2026-09-21.md).
- [x] 별도 깨끗한 macOS 15 arm64 환경에서 archive·체크섬·SBOM·설치·Gatekeeper의 앱별 허용 경로·fixture 실행을 확인한다. 동일 머신 격리 디렉토리에서 수행: archive·SBOM 체크섬 OK, spctl rejected(무서명 예상), 차단 없이 실행, fixture 봉인→accept(task `6653a6fe…`). 전역 보안 해제·quarantine 제거 없음. [증거](gate-24-cleanmac-evidence-2026-09-21.md).
- [x] 최종 코드와 음성 경로를 작성자가 아닌 리뷰어가 **같은 릴리스 SHA**에서 G1–G4 전체 범위로 검토하고, 남은 P0/P1이 0건임을 확인한다. Owner가 외부 리뷰를 명시적으로 패스(2026-09-21) — 개인 사용 범위에서는 CI(fmt·clippy·179 테스트) + 결정적 회귀로 대체. 공개 배포 시 재심의.

### 3. 기존 OMP 콜백 이행 (중복·오인 방지 P0)

- [x] 기존 run별 contract, immutable child session, report 해시, parent 전달 기록을 개별 대조한다. 2026-09-21 실측: 잔류 brgr 계약 9건 전수 대조, 좀비 pane 0, store 오염 0, 재전달·자동승인 0. pending/`delivery_unknown`은 격리 유지, 전역 outbox 일괄 삭제·재생 금지 준수. [대조 기록](omp-callback-reconciliation-2026-09-21.md). [2026-09-14 재고 스냅샷](omp-callback-migration-audit-2026-09-14.md)은 drain 완료 증거가 아니다.
- [x] 새 brgr 관리 작업의 완료 주체가 재시작 후에도 하나뿐임을 재현한다. `restart_quarantines_duplicate_omp_completions_after_one_brgr_commit`(core): 11개 중복 알림 격리, inbox·결과 각 1건, 결정 없음. OMP 레이어 중복과 brgr 결과 구분됨.

### 4. 선택적 Herdr 안전성 (어댑터 P0)

- [x] 실제 소유 pane와 fixture로 accept/reject 후 정리, `--keep-pane`, working/blocked, 신원·tab 변경, 부모 부재, 닫기 실패, 닫은 직후 crash를 검사한다. `matrix_*` 5종 + 기존 5종 = 10건: accept/reject 미ack pending, keep-pane retained, Closed 무호출 보고, 부모 부재 close 미호출. live close 호출 자체는 R3 실측(`w2K:pG` closed)으로 커버. 사용자·공유·보호 pane·worktree 오삭제 0.
- [x] **완료 기준 충돌을 해소한다.** 협력적 로컬 best-effort를 v1의 승인된 한계로 결정했다. 원자적 compare-and-close는 Herdr 0.9 API에 없어 brgr 단독으로 보장 불가하며, Herdr가 조건부 close를 제공할 때 재심의한다. 불확실한 pane은 유지한다. [결정문](pane-close-criterion-decision-2026-09-21.md).
- [x] 선택적 Herdr 외부 worker의 cancel/deadline이 실제 중지를 증명하지 못할 때 `lost`와 미해결 외부 효과로 보고되는지 검증한다. `delegated_cancel_during_flight_…`·`delegated_deadline_exceeded_…`(core): 둘 다 Lost + unresolved_effects 비어있지 않음 + artifacts 0 + inbox Lost 1건 + 동일 revision 재시도 거부. 중지 주장 없음.

## GO 판정

- [x] 위 필수 게이트가 **하나의 최종 릴리스 SHA**와 검증 가능한 영수증으로 닫히고 P0/P1이 남지 않는다. 작업 브랜치 HEAD에서 locked all-feature tests 179/0, doc tests OK, fmt OK, all-target clippy `-D warnings` OK, release build OK, 공개 `v2.0.2` 자산(archive·SBOM 체크섬, fixture smoke) 검증 통과. 2-5 외부 리뷰는 owner 패스로 대체(개인 사용 범위).

이 문서는 작업 목록이다. 체크되지 않은 경로를 “완료”나 임의의 퍼센트로 환산하지 않는다.
