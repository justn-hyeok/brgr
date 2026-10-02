# brgr 오케스트레이션 재정비 계획

상태: 구현·로컬 검사·설치 완료, 전체 하네스 live 검증은 일부 제한 · 갱신 2026-10-02 · 기준 코드 `f646d0f` / v2.9.3

현재 결과와 검증 범위는 [사용 가이드](orchestration.md)와
[실행 증거](../evidence/orchestration-rework-2026-10-01.md)에 기록한다.
Claude의 기본 TUI, trust 자동 해제, 후속 메시지, read-only 결과 채널,
명시적 debate, 결과 처리 후 정리를 실제 실행으로 확인했다.
하네스별 등록·TUI 진입점과 실제 provider 완료 검증은 별개의 증거다.
GJC·OMP는 실제 TUI에 지시를 전달했지만 설정된 provider의 크레딧 오류로
완료 검증이 막혔다. 나머지 하네스의 live 완료와 혼합 하네스 대화는
미검증으로 남긴다. 이 제한을 포함해 전체 실사용 검증을 100%로 표시하지 않는다.
최종 리비전은 테스트 345개, fmt, strict clippy, release build를 통과했다.
설치본과 release 빌드 digest가 같고 Codex 훅 3개와 설치 스킬도 최신 상태다.
실제 QA 작업 8건의 terminal·pane 정리 완료 상태를 설치본으로 재확인했다.

이해한 요청: brgr를 기본 TUI·YOLO 실행과 하네스 간 소통을 제공하는 오케스트레이션 도구로 정리하고, 실행·완료 전달·정리를 복구한다. 사용자가 지정한 `debate`에서만 형제 작업자의 직접 대화를 활성화하며, 사용자 지침 파일과 brgr 호출 설정을 추가한다.

## 1. 확정된 제품 동작

| 항목 | 동작 |
| --- | --- |
| 기본 실행 | Herdr pane에서 하네스의 실제 TUI 실행 |
| TUI의 목적 | 하네스들이 질문·답변·후속 지시를 주고받는 실행 세션 |
| 기본 도구 승인 모드 | 각 하네스의 YOLO/full에 해당하는 네이티브 옵션 |
| headless | 해당 호출에 `--headless`를 명시했을 때만 실행 |
| 기본 소통 | 부모 ↔ 작업자 |
| 형제 간 소통 | `debate`를 명시한 참여 작업자 사이에서만 직접 대화 |
| 완료 | 결과 회수·봉인 → 부모 전달 → 부모의 검토·결정/통합 → 소유 pane 정리 |
| 사용자 지침 | `$BRGR_HOME/BRGR.md` |
| 호출 설정 | `$BRGR_HOME/config.toml`, 기존 `brgr config` 확장 |
| 스킬 | 명령·옵션·입출력·오케스트레이션 사용법 설명 |

사용자가 모델·effort·권한·배치를 지정하면 그 값을 호출에 전달한다. 모델의 답변 품질은 부모가 완료 기준과 대조한다. brgr는 자체적인 작업 제한이나 일괄적인 사용자 재승인 방침을 스킬에 추가하지 않는다.

읽기 전용 검토라는 작업 범위와 TUI/headless 선택을 분리한다. 검토 작업이라는 이유로 native plan 모드를 자동 선택하거나 TUI를 제외하지 않는다. 명시적으로 지정한 도구 권한은 별도로 존중하며, 결과 출력 채널은 소스 수정 범위와 구분한다.

## 2. 현재 확인한 결함과 불확실성

| 관측 | 수정 대상 |
| --- | --- |
| 일반 Codex 도구 호출은 pane 검증 실패 시 headless로 전환 | 호출 세션과 Herdr pane을 연결하는 경로 |
| read-only 및 일부 effort 요청도 자동 headless | 실행 방식과 권한·지원 능력의 분리 |
| 모든 네이티브 승인 화면을 사용자에게 넘기라는 내장 스킬 문구 | 내장 스킬과 설치된 관련 안내의 정합성 |
| `brgr message reply`가 네이티브 승인 화면을 해제하지 않음 | 일반 대화와 네이티브 입력 대기의 구분 및 전달 |
| 감독 프로세스 소실 후 작업자는 계속 실행하며 보고서를 작성 | TUI 세션의 수명 관리와 결과 회수 |
| 실제 보고서는 존재하지만 sealed result는 `lost`, artifacts 0 | 복구 중인 세션과 최종 결과의 구분 |
| native pane과 OMP pane의 정리 기록·파일명이 서로 다름 | 공통 pane/session 기록과 정리 큐 |
| 닫기 실패에도 native pane 소유 기록을 삭제 | 닫기 확인 후 상태 갱신, 실패 재시도 |
| native pane은 오른쪽 split으로 고정 | 설정된 배치의 공통 적용 |
| fixture가 승인 대기를 스스로 해제 | 자동 회복을 가정하지 않는 검증 |

사례 task `5061cc33`에서 감독 프로세스가 사라진 최초 원인은 아직 확정되지 않았다. 이 원인은 첫 단계에서 실행 기록과 격리 재현으로 구분한다. 보고서 회수와 pane 정리 경로의 공백은 코드와 실제 기록에서 각각 확인했다.

## 3. 구현 구조

### 작업과 실행 세션

기존 task/revision/attempt, 봉인 결과, inbox, 명시적 부모 결정은 유지한다. 여기에 실행 세션의 관측을 연결한다.

- 공통 세션 기록: task·attempt, 하네스, 원본 호출 세션, 실행 방식, 실제 적용 옵션, worker pane·terminal, 하네스가 제공하는 native session, 보고서 위치, 정리 상태.
- 실제 하네스의 세션 형식은 각 adapter가 유지한다. native session을 제공하지 않는 하네스는 brgr의 실행 식별자와 제공하지 않는 항목을 구분해서 기록한다.
- `starting`, `working`, `awaiting_input`, `recovering`, `finished` 등은 작업자의 관측 상태다. 프로세스 wrapper가 살아 있다는 사실과 구분해서 출력한다.
- 작업 관리 프로세스와 수집기는 호스트에서 실행하고 부모 pane의 수명에 묶지 않는다. attempt별 로그와 종료 원인을 남기며, 복구 수집 권한은 store에서 하나의 관측자만 획득한다.
- 부모 세션이 잠시 없거나 supervisor가 끊겼어도 작업자 세션·보고서를 다시 관측한다. 실행을 새로 시작하는 재시도와 기존 실행을 회수하는 복구를 구분한다.
- 이미 봉인된 `lost`나 기존 결정은 덮어쓰지 않는다. 과거 실행에서 찾은 출력은 별도의 복구 증거로 연결한다. 새 실행은 결과가 확정되기 전에 회수할 수 있게 상태 흐름을 고친다.

### 하네스 adapter와 Herdr

하네스 adapter는 native argv, TUI 시작, 입력 가능 상태, 메시지 전달, 결과 출력을 선언한다. 하네스별 분기는 registry/adapter에 두고 core에 CLI 이름별 switch를 추가하지 않는다.

- Herdr가 인식하는 하네스는 native agent 기능을 사용한다.
- GJC·Command Code처럼 현재 Herdr의 agent kind에 없는 경로는 brgr worker pane 안에서 native interactive argv를 실행하고, adapter가 입력·상태·결과를 연결한다.
- Herdr가 특정 하네스를 인식하지 않는다고 print-mode 실행을 TUI 지원으로 간주하지 않는다.
- 해당 하네스에 실제 interactive 진입점이 없는 경우 지원 부족을 명확히 출력한다. 사용자 요청 없이 headless로 바꾸지 않는다.
- TUI, YOLO, 모델, effort 지원 여부와 실제 적용값을 별도로 기록한다. TUI에서 특정 옵션을 사용할 수 없으면 시작 전에 구체적인 충돌을 출력한다.

### 대화와 완료 전달

메시지는 store에 먼저 기록하고, 수신자의 실제 TUI 세션에 전달한 영수증을 별도로 남긴다. 전송·수신·답변·결과 결정은 서로 다른 상태다.

- 부모 ↔ 작업자 메시지와 후속 요청을 adapter가 TUI에 전달한다.
- 작업자가 바쁘거나 네이티브 입력 화면에 있으면 대화 메시지를 보관하고 입력 가능한 시점에 전달한다.
- 네이티브 도구 승인/시작 화면과 작업자가 작성한 질문을 구분한다. 전자는 하네스별 실행 설정과 입력 adapter가 다루고, 후자는 실제 대화 메시지로 전달한다.
- 기본 YOLO 옵션을 실제 실행 argv에서 확인한다. brgr는 모든 네이티브 입력 대기를 일괄적으로 사용자에게 넘기라는 방침을 생성하지 않는다.
- 완료 알림을 받는 부모가 바쁘거나 재접속하면 결과는 durable inbox에 남고, 정확한 세션에 한 번 전달한다.

## 4. 사용자 지침과 CLI 설정

### 파일 위치와 적용

지침의 canonical 위치는 `$BRGR_HOME/BRGR.md`다. 현재 Mac의 기본 위치는 `~/Library/Application Support/brgr/BRGR.md`다. 기본 파일은 오케스트레이션 설명과 작성 예시만 제공한다.

지침은 실행 시 읽어 부모와 작업자에게 필요한 내용으로 전달한다. 적용한 경로·내용 digest를 실행 영수증에 기록하여, 파일이 나중에 바뀌어도 해당 실행에 사용한 지침을 확인할 수 있게 한다. 파일이 없으면 추가 지침 없이 실행한다.

호출 옵션의 우선순위는 **명시한 CLI 옵션 → 하네스별 사용자 설정 → 전역 사용자 설정 → 제품 기본값**이다. Markdown은 사용자 지침이고, CLI 옵션을 조용히 다시 쓰는 설정 parser로 사용하지 않는다. 각 하네스의 네이티브 지침 파일과 인증 상태도 그대로 유지한다.

### CLI 표면

아래 명령 표면은 개발 빌드에 구현하여 로컬 설치본에 반영했다.

| 명령 | 역할 |
| --- | --- |
| `brgr config init` | 설정과 `BRGR.md` 작성 예시를 만들고 실제 경로 표시; 기존 파일 보존 |
| `brgr config show` | 전역·하네스별 설정과 적용 출처 표시 |
| `brgr config set ...` | 기본 하네스·모델·effort·호출 옵션·배치·시간 제한 설정 |
| `brgr config check` | 현재 등록 하네스와 설정의 호환성 확인; 유료 작업 실행 없음 |
| `brgr run ... --headless` | 명시적 headless 실행 |
| `brgr run/revise ...` | 지정값 및 사용자 설정을 해석해 기본 TUI·YOLO 실행 |
| `brgr status TASK` | 세션·pane·실제 옵션·대기 원인·결과·전달·정리 상태를 함께 표시 |
| `brgr message ...` | 부모 ↔ 작업자 대화와 전달 상태 조회 |
| `brgr debate start TASK...` | 명시한 형제 작업자들을 직접 대화 그룹으로 연결 |
| `brgr debate status/stop ...` | 참여자·전달 상태 조회와 직접 대화 종료 |

등록은 기존 `brgr harness` 기능을 사용한다. native 호출용 추가 옵션은 문자열 shell 명령이 아닌 argv 배열로 설정한다. 설정된 옵션과 brgr의 모델·권한 옵션이 중복되면 적용 결과를 숨기지 않고 설정 검사에서 드러낸다.

하네스의 개인 config·인증·로그인을 자동 변경하는 기능은 추가하지 않는다. 구형 `set-pane-mode`/`prefer_print_mode` 및 permission cap은 마이그레이션 안내와 실제 적용 출처를 제공한다. 과거 설정 때문에 새 기본 호출이 조용히 headless나 비-YOLO가 되는 상태는 제거한다.

기존 사용자가 명시한 설정은 보존하고 적용 출처를 표시한다. 스킬이 임의로 낮춘 권한과 제품의 숨은 기본값을 제거하는 것이며, 사용자가 선택한 권한을 몰래 YOLO로 올리는 마이그레이션은 하지 않는다. 자연어에서 `debate`를 지정하면 부모가 위 CLI로 참여 작업자를 연결한다. 사용자가 task ID를 복사해 입력하는 절차를 기본 사용 흐름으로 만들지 않는다.

## 5. 실행 순서와 단계별 완료 조건

| 단계 | 작업 / 주요 책임 파일 | 산출물 | 완료 조건 |
| --- | --- | --- | --- |
| 0. 실패 고정 | `cli/tests`, `testdata/fixtures`, 실행 기록, 설치 하네스 도움말 | 재현 시나리오·원인 기록·native TUI 지원 표 | supervisor 소실·살아 있는 작업자·뒤늦은 보고서·닫기 실패를 각각 재현하고 관측; 각 adapter의 실제 진입점과 입력·결과 채널 확인 |
| 1. 기본 호출 통합 | `cli/admission`, `caller_pane`, `plugin_bridge`, `registry/recipe`, `runner/manifest` | 실행 설정 해석 및 원본 세션 연결 | 일반 Codex/플러그인/worker 호출에서 실제 TUI·YOLO·모델·effort·배치가 요청과 일치; 자동 headless 0건 |
| 2. 세션·결과 복구 | `protocol`, `core`, `store`, `cli/supervision`, `pane_adapter` | 공통 세션 기록과 회수 상태 흐름 | supervisor 또는 부모 연결이 끊긴 실행을 재호출 없이 회수; 결과 봉인·inbox 생성 1회 |
| 3. 대화·완료 전달 | `store/message`, `runner/prompt`, `cli/message`, `notification` | native TUI 전달 adapter와 영수증 | 부모 ↔ 작업자가 질문·답변·후속 요청을 실제 세션에서 받음; busy/재접속 뒤 전달, 중복 없음 |
| 4. 정리 통합 | `cli/pane_cleanup`, `pane_adapter`, `supervision`, Herdr 호출 | 하나의 소유 기록·정리 큐 | native/OMP 경로 모두 결과 처리 뒤 소유 pane 정리; 실패·재시작 후 재시도; `--keep-pane` 유지 |
| 5. debate | `protocol`, `store`, `cli/message`, `runner/prompt` | 명시적 대화 그룹과 peer 주소 | 지정한 형제들이 직접 대화; 기본 호출에는 peer 대화 경로 없음; 그룹 종료와 참여자 종료가 반영 |
| 6. 사용자 설정 | `cli/config`, `cli/cli`, `admission`, `registry` | `BRGR.md` 로딩, CLI 설정·검사·해석 | 설정 우선순위와 적용값 확인; 기존 파일 보존; 새 세션에도 같은 설정 반영 |
| 7. 스킬·문서 및 실사용 | `codex_integration`, 관련 설치 스킬, `docs/guides`, `README` | 사용법 중심 스킬, 하네스별 실사용 증거 | 재승인·자동 headless 안내 제거; 실제 하네스에서 전체 흐름 완료 |

단계 1과 6은 설정 해석 코드를 공유한다. 기본 동작과 CLI 형태를 먼저 정하고, 설정 작성 기능은 핵심 실행 경로가 통과한 뒤 마무리한다. 단계 5는 단계 3의 검증된 메시지 전달을 그대로 사용한다.

각 단계는 앞 단계의 기록된 산출물과 통과 근거를 입력으로 사용한다. 같은 재현/수정 시도가 두 번 실패하면 실행 경계나 최소 fixture를 바꿔 원인을 좁힌다. 불확실한 유료 실행은 복구 상태를 확인하며 중복 dispatch로 해결하지 않는다. 미완료 단계는 통과로 기록하지 않는다.

## 6. 검증 계획

대상은 사용자 소유 Rust 1.95 CLI workspace, Cargo, SQLite, Herdr/TUI adapter, macOS 15 CI다. 기존 `assert_cmd`·fixture·store 계약 검사와 실제 하네스 검증을 함께 사용한다.

### 결정적 검사

1. 기본 호출은 native TUI·YOLO. read-only 작업 범위, effort, 호출 출처를 이유로 headless 전환 없음.
2. `--headless`를 명시한 호출만 headless. 실행 영수증의 requested/actual 값 일치.
3. 승인 화면 fixture는 외부 입력이 없으면 계속 막힌 상태로 유지. 실제 설정·입력 adapter가 해제해야 다음 단계 진행.
4. supervisor 중단 전·중·후에 작업자 세션과 완료 보고서를 관측하고, 재실행 없이 복구. 결과·inbox·완료 전달 중복 0건.
5. 잘못된/미완성/다른 attempt의 보고서는 해당 실행의 완료 결과로 사용하지 않음.
6. pane 닫기 실패 후 소유 기록이 남음. 재시작과 ack 뒤 정리 재시도. closed 확인 뒤에만 정리 완료 기록.
7. native와 OMP adapter가 같은 정리 계약을 충족. 지정한 adjacent/tab 배치와 `--keep-pane` 확인.
8. 부모·작업자의 busy/재접속, 메시지 재전송, 네이티브 입력 대기에서 전달 및 처리 횟수 확인.
9. `debate`를 지정한 형제 간 왕복 대화, 참여자 종료, group stop 검증. 기본 호출에서 peer 전달이 활성화되지 않음.
10. `BRGR.md` 없음/변경, 설정 우선순위, 오래된 config, 잘못된 argv·지원하지 않는 옵션 검사.
11. 다중 프로세스 취소·복구·정리 경합에서 겹친 실행, 결과 중복, 잘못된 pane 정리 없음.

### 실제 하네스 검사

등록된 각 하네스에 대해 **실제 interactive 진입점, YOLO 적용, 초기 입력, 후속 메시지, 결과 회수, pane 정리**의 지원 표를 만든다. 하네스별 명령과 native 세션 형식은 설치본의 도움말과 실제 실행으로 확인한다. fixture 결과를 live 통과로 표시하지 않는다.

첫 실사용 기준은 문제를 관측한 Claude Code 경로다. 부모와 다른 종류의 하네스 사이 왕복 소통, 두 형제 하네스의 `debate`, 부모/감독 연결 중단 뒤 회수까지 검증하고 이후 등록 하네스로 넓힌다. 막힘 없는 짧은 응답 외에 파일을 읽고 결과를 작성하는 작은 실제 작업도 포함한다.

기능을 채택할 하네스는 이 표의 적용 항목을 통과해야 한다. interactive 진입점이나 후속 입력을 제공하지 않는 경로는 미지원으로 기록하고, headless 실행으로 TUI 검증을 대신하지 않는다. 실사용 실패는 해당 단계의 fixture 재현으로 되돌려 수정한다.

완료 기준은 사용자가 task ID를 복사하거나 pane을 수동 이동·승인·회수·닫기 하지 않아도 **호출 → 소통 → 결과 전달 → 검토·처리 → 정리**가 끝나는 것이다. 검토 작업은 답변이 존재하는 것뿐 아니라 요청한 검토 범위와 보고된 범위를 대조한다.

### 코드 검사 명령

```sh
cargo fmt --all -- --check
cargo test --workspace --all-features --locked
cargo test --workspace --doc --locked
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace --release --locked
```

관련 단계의 집중 검사를 먼저 실행하고, 계약을 변경한 최종 리비전에서 전체 검사를 실행한다. 독립 리뷰는 `codex review`로 수행하고, 지적 사항의 근거를 재현한 뒤 처분한다. 스킬 설치 변경 후에는 native 통합 상태와 `codex-self-check`를 확인한다.

## 7. 호환성과 완료 보고

기존 sealed result·결정·inbox의 바이트와 digest는 유지한다. 새로운 세션·대화·복구 정보는 명시적인 schema migration과 버전 표시를 사용하며, 기존 v1 task/result fixture를 함께 검증한다. 지원되지 않는 새 schema는 설명 가능한 오류로 처리한다.

현재 설치본과 새 빌드의 증거를 구분한다. 최종 보고에는 적용 리비전, 하네스별 TUI 지원 표, 실제 호출 옵션, 메시지 전달, sealed result/inbox, 복구와 정리 결과를 적는다. 실행하지 못한 live 경로는 미검증으로 남긴다.

계획 등록 당시에는 문서만 작성했다. 이후 사용자 진행 승인으로 코드 구현과
로컬 CLI·관련 스킬 설치를 수행했다. 다른 작업의 회수, commit/push/merge,
worktree 삭제 및 릴리스는 수행하지 않았다. 원래 완료 조건과 실제 검증의
차이는 실행 증거에 명시한다.
