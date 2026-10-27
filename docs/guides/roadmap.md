# Execution Model Roadmap

상태: Phase 1 store/CLI는 v2.14.2, 버려진 Head 인계는 v2.15.0 · 작성 2026-10-14 · 버전 정책 2026-10-26

> 에이전트 세션이 아니라 brgr이 작업의 생명주기를 소유한다.

현재 동작의 기준은 [Architecture](architecture.md)와
[Reference](reference.md)다.

`brgr head request|prepare|show|commit|abort|retire|status`는 v2.14.2에 있다.
`brgr head claim TASK --abandoned --goal TEXT`는 v2.15.0에 있다: 나가는 Head가
brgr을 못 칠 때 들어오는 세션이 인계를 시작하고 지휘권을 가져간다.
`brgr head handoff|take|exit`는 v2.15.2에 있다: 현재 Head가 새 Herdr pane을 열어
새 Head를 시작하고, 새 Head가 `take`로 승계하며, `--exit-old`를 승인한 경우
검증된 `retire` 뒤에 이전 Head의 pane만 닫는다.
`config set-handoff-condition`은 v2.15.5에 있다: Head의 컨텍스트나 플랜 사용량이
한도를 넘으면 brgr이 Head에게 `head handoff` 명령을 알리고, `--auto`를 켜면 Head의
다음 brgr 명령 끝에서 같은 인계를 직접 실행한다. Claude Code, Codex, Cursor가
신호를 낸다(Cursor는 컨텍스트만). 이것으로 2.6까지 갖췄다. Workflow Runtime은
향후 계획이다.

모든 단계는 v2.x로 낸다. 메이저 버전은 올리지 않는다.

**마이너는 Head Migration, Dynamic Workflows, Unified Continuity처럼 대형 기능이
실제로 출시될 때만 올린다. 그 밖의 수정·호환·작은 명령·소유자 면 구멍은 전부
패치다.** 여러 작은 일이 같이 쌓였다고 마이너를 올리지 않는다. 문서만 바뀌면
릴리스를 만들지 않는다. 같은 날 추가 릴리스는 방금 낸 버전이 깨졌을 때만 한다.

## 방향과 유지할 계약

```text
현행 품질 개선
  -> Continuity Foundation
  -> Head Migration
  -> Dynamic Workflows
  -> Unified Continuity
```

기존 최소 실행 계약은 모든 단계에서 유지한다.

```text
bounded fresh run -> sealed result -> durable inbox -> owner decision
```

Head는 작업을 지휘하는 논리적인 owner 역할이다. 하네스 세션은 그 역할에 현재
연결된 실행 주체이며, 세션을 교체해도 작업·결과·결정의 정체성은 유지되어야 한다.
Workflow Runtime은 작업 실행 상태를 관리하고, Head는 결과를 검토해 명시적으로
accept/reject를 결정한다. 프로세스 종료, pane 상태, 메시지 수신이나 workflow
분기만으로 결과가 자동 승인되어서는 안 된다.

현재 이미 존재하는 기반은 다음과 같다.

- task revision과 attempt 식별자, sealed artifact와 durable inbox.
- 안정적인 owner ID와 세션 binding, 단조 증가하는 binding epoch.
- 명시적인 `brgr bind TASK`를 통한 owner의 세션 이전과 stale-session 명령 차단.
- owner의 세션 binding 변경 후에도 유지되는 기존 결과·결정과 미처리 완료 알림의
  재전달 경로.
- v2.14.2의 기록된 Head 인계: `request` → `prepare` → `commit` → 새 세션의
  owner write로 `resumed` → `retire`. 매니페스트는 참조만 담는다. 인계가 열려
  있으면 `bind`는 거부된다.

`bind TASK`는 task를 통해 owner를 찾는 진입점이다. 세션이 같으면 epoch를 올리지
않고(멱등), 다르면 epoch를 1 올려 저장하며, 미처리 완료 알림의 전달 표시를
초기화해 새 세션에 재전달한다. 저장된 binding은 task별이 아니라 owner별이므로
같은 owner의 작업들은 이 binding을 공유한다. `bind`는 묶이지 않았거나 버려진
owner용이다. 지휘권을 다른 세션으로 넘기는 것은 `brgr head`다. 근거는
[owner store](../../crates/brgr-store/src/owner.rs),
[CLI binding](../../crates/brgr-cli/src/task_commands.rs),
[Reference의 head 절](reference.md)에 있다.

## Phase 0. 현재 경험 완성

릴리스 방향: v2.x 패치 · 상태: 지속 진행

현재 사용자가 잘 쓰는 `brgr run`의 동작과 경험을 유지한다. 대형 기능을 개발하는
동안에도 하네스 호환성 문제와 실제 사용자 제보를 계속 해결한다. 이 단계의
산출물은 패치로 낸다.

- OpenCode 모델 지정처럼 하네스별 실행·모델·옵션의 불편함을 제거한다.
- 실제 TUI, 메시지 전달, 결과 검증, worktree와 pane 수명 관리의 품질을 유지한다.
- 등록 성공, 빌드 성공과 실제 하네스 실행 성공을 구분해 검증한다.
- 새로운 실행 엔진 때문에 기존 사용자에게 새 workflow 사용법을 강제하지 않는다.

이 단계는 이후 단계의 개발을 중단시키는 일괄 게이트가 아니다.

## Phase 1. Continuity Foundation

릴리스 방향: 내부 기반. store/CLI는 v2.14.2 패치로 출시 · 상태: 부분 출시

Head Migration과 Dynamic Workflows가 공통으로 사용할 상태 소유권과 인수인계
기반이다. 세 산출물 중 store에 기록되는 부분과 CLI는 v2.14.2에 있다.

| 산출물 | 기록할 내용 | 상태 |
| --- | --- | --- |
| Handoff Manifest | 목표, 결정 사항, 원본 세션 참조, 미완료 작업, worker 식별자, 대기 메시지·결과 참조 | v2.14.2. 참조만 담는다. attempt가 없는 revision은 매니페스트에 없고 `brgr status`로 본다 |
| Head Binding | 논리적인 Head ID, 안정적인 owner와 작업 집합의 관계, 현재 세션과 binding epoch | 기존 owner binding과 epoch를 재사용. 인계 `commit`이 epoch를 1 올린다 |
| Takeover Protocol | 인계 요청, 새 Head 준비, 지휘권 승계, 실제 작업 재개 확인, 이전 Head 종료 확인 | store 단계(`request`/`prepare`/`commit`/`resumed`/`retire`)는 v2.14.2. 죽은 Head 시작은 v2.15.0 `claim`. 새 Head pane 준비, 두 live pane 알림, 이전 Head 자동 종료는 없음 |

```text
Logical Head
  ├── owner: stable-owner-id
  ├── bindingEpoch: 3
  ├── activeSession: codex-session-B
  ├── previousSession: claude-session-A
  ├── tasks / workers: [task + revision + attempt references]
  ├── pendingResults / messages: [durable references]
  └── handoff: [manifest reference + takeover progress]
```

이 단계의 검증 기준. 앞의 네 줄은 v2.14.2에서 격리 홈과 store로 확인했다.
뒤의 두 줄 중 죽은 Head 시작은 v2.15.0 `claim`이다. 두 live pane 배달과 이전
Head 자동 종료는 남아 있다.

- 새 Head 준비와 지휘권 승계를 구분한다. 새 pane이 열렸다는 사실만으로 승계를
  완료 처리하지 않는다.
- 같은 owner의 작업은 기존 owner binding을 재사용한다. 여러 owner를 함께
  인계해야 한다면 대상 집합과 부분 실패 시 복구 동작을 먼저 명시한다.
- 인계 중 도착한 결과·질문·완료 알림이 사라지지 않고 현재 Head에 도달한다.
  (store 재큐는 v2.14.2. 두 live pane 배달은 아직 확인하지 않았다.)
- 중복 요청과 프로세스 재시작이 이중 승계·중복 결정·worker 재실행을 만들지 않는다.
- 인계 기록이 원본 세션과 sealed artifact를 참조한다. 요약문이 원본 결과나
  이미 기록된 결정을 덮어쓰지 않는다.
- cooperative same-user identity라는 현재 신뢰 모델을 유지하며, epoch를
  별도의 보안 인증 수단으로 설명하지 않는다.

범용 workflow 언어나 usage 자동 감지는 이 단계의 산출물에 넣지 않는다.

## Phase 2. Head Migration

릴리스 방향: v2.15.0 · 상태: 2.1~2.6 출시. 실사용 검증은 Claude·Codex·Cursor 각각이 Head인 경우

지휘권을 다른 세션으로 옮기고 기존 worker와 미처리 결과를 승계한다. 인계 성공은
새 Head가 실제 작업을 이어서 수행했을 때 입증한다. 특정 하네스 전용이 아니다.

| 순서 | 기능 | 완료 조건 |
| --- | --- | --- |
| 2.1 | 동일 하네스 이전 | Claude A -> Claude B가 작업과 맥락을 인계한다 |
| 2.2 | 크로스하네스 이전 | Claude -> Codex가 같은 작업 집합의 지휘권을 승계한다 |
| 2.3 | 기존 worker 승계 | 진행 중 worker와 미처리 결과·질문이 유지되고 새 Head로 전달된다 |
| 2.4 | 검증된 인계 | 새 Head가 결과를 읽고 명시적으로 판단하거나 기존 작업의 다음 행동을 수행한다 |
| 2.5 | 자율 퇴장 | 인계와 작업 재개 확인 후 기존 Head만 종료되고 worker는 유지된다 |
| 2.6 | 조건부 migration | 사용자가 정한 usage 등의 조건에 따라 같은 검증된 인계 절차를 실행한다 |

첫 마이너에서는 2.1~2.5를 검증한다. 2.6은 그 마이너 다음의 패치다. 먼저 아래
요청이 끝까지 동작해야 한다.

> 지금 진행 중인 작업과 worker를 모두 다른 세션으로 넘겨.
> 정상 인계가 끝나면 현재 세션을 종료해.

나가는 Head가 한도·중단으로 명령을 못 쳐도 들어오는 세션이 인계를 시작할 수
있어야 한다. 지금 `request`/`prepare`는 현재 Head만 할 수 있다.

여기서 옮기는 것은 Head의 지휘권이다. 기존 worker를 다른 하네스로 재실행하거나
worker의 모델을 바꾸라는 뜻이 아니다. 기존 Head의 종료도 이 요청처럼 승인된
범위 안에서만 수행한다.

### 첫 데모와 출시 게이트

1. Head A가 세 worker를 지휘하며 작업을 진행한다.
2. 목표·결정·세션 참조·worker·미처리 결과를 Handoff Manifest에 기록한다.
3. Head B가 준비된 뒤 같은 owner의 지휘권을 승계한다.
4. B가 기존 worker의 `FROM BRGR` 결과 또는 질문을 받고, 결과 판단이나 후속
   작업을 실제로 수행한다. pane 표시나 단순 수신 확인만으로 통과하지 않는다.
5. 작업 재개가 인계 기록에 확인된 뒤 A가 종료된다. worker와 실행 상태는 유지된다.

2.5의 "작업 재개 확인"은 새 Head의 자기 보고가 아니라 brgr이 기록한 사건, 예를
들어 새 Head 세션이 epoch로 낸 accept/reject나 후속 작업 생성이어야 한다. 기존
Head 종료는 되돌릴 수 없으므로 이 기록이 있기 전에는 실행하지 않는다.

성공 경로와 함께 새 Head 시작 실패, 인계 도중 Head 소실, 지연·중복 메시지,
stale Head의 결정 시도, 기존 Head 종료 실패, 나가는 Head가 침묵하는 경우를
재현한다. 종료 실패는 인계 성공과 별도로 보고하며, worker까지 종료하거나
승인되지 않은 새 실행으로 숨기지 않는다.

**가장 중요한 이정표는 Head Migration의 첫 실제 성공이다.**
새 Head가 기존 worker의 결과를 판단하고, 이전 Head가 검증된 인계 후 스스로
퇴장하는 장면을 첫 마이너의 기준으로 삼는다.

## Phase 3. Dynamic Workflows

릴리스 방향: Head Migration 다음의 별도 마이너 · 상태: 첫 버전 구현 (agent/parallel, 저널, 드라이버, SDK)

작은 Workflow Runtime을 추가하고 기존 brgr 위임 계약을 실행 백엔드로 사용한다.
`six-ddc/codex-dynamic-workflows`는 참고 후보이며 그대로 이식하기로 결정한
것은 아니다.

지금 있는 것: 단계 저널, `brgr workflow create`, `brgr workflow status RUN`,
`brgr workflow set-state RUN blocked`, `brgr workflow run SCRIPT`,
`brgr workflow resume RUN`, `brgr workflow cancel RUN`, `brgr workflow init`,
`brgr run OBJECTIVE --workflow RUN --step KEY` (병렬 한도에 걸리면 종료 코드 75),
분리된 node 드라이버, 런 상태 변화당 owner 알림 하나, 그리고 의존성 없는
JavaScript SDK (`agent()`, `parallel()`). SDK는 accept/reject/apply/land를
노출하지 않는다. 완료 조건은 실행 도중의 **관찰한** 결과를 바탕으로 처음에는
존재하지 않았던 작업을 새로 생성하는 것이다. SDK가 돌려준 값은 승인된 변경이
아니다.

아직 없는 것: `pipeline()`, 재시도 정책, 단계별 기한, TypeScript 도구,
실행 중인 런의 Head 이전.

첫 버전의 실행 표현은 네 가지로 제한한다.

| 표현 | 역할 | 첫 버전 |
| --- | --- | --- |
| `agent()` | 기존 brgr 경로로 bounded 작업을 위임한다 | 있음 |
| `parallel()` | 독립 작업을 병렬 실행하고 각각의 결과를 회수한다 | 있음 |
| `pipeline()` | 앞 단계의 기록된 결과를 다음 단계 입력으로 연결한다 | 없음 |
| 일반 JS 조건문·반복문 | 관찰한 결과에 따라 동적으로 분기하고 새 작업을 생성한다 | 있음 |

아래는 현재 `agent()` / `parallel()` 형태다. 반환값의 수신과 owner의
accept/reject는 별도 절차다.

```javascript
import { agent, parallel } from "./brgr-workflow.mjs";

const reviews = await parallel([
  () => agent({ task: "Review security" }),
  () => agent({ task: "Review architecture" }),
]);

if (reviews.some((review) => review.report.includes("critical"))) {
  await agent({ task: "Reproduce and fix verified issues" });
}
```

완료 조건은 실행 도중의 결과를 바탕으로 **처음에는 존재하지 않았던 작업을 새로
생성하는 것**이다. 미리 작성된 DAG를 병렬 실행하는 것만으로 완료 처리하지 않는다.

- 실제 TUI, worktree, 메시지 전달, 결과 봉인·검증과 owner inbox를 재구현하지 않는다.
- 분기 입력은 정확한 task/revision/result를 참조하고, 검토 결과의 의미와 유효성을
  확인한다. 임의의 worker 텍스트를 승인된 변경으로 취급하지 않는다.
- 실행 상태·단계 입력·결과·후속 작업 관계를 영속 기록으로 남긴다. JS/TS 함수를
  다시 호출하는 것만으로 중단된 실행이 안전하게 재개된다고 가정하지 않는다.
- 각 단계에는 완료 조건, 예산·기한, 재시도 한도와 실패 시 돌아갈 상태를 둔다.
- 재시도는 기록된 정책과 명시적인 권한을 따른다. 현재의 불확실한 실행을 `lost`로
  처리하는 계약을 유지하고, 결과를 모른다는 이유로 worker를 자동 재실행하지 않는다.
- runtime 재시작으로 완료된 작업이나 명시적 owner 결정을 중복 실행하지 않는다.

## Phase 4. Unified Continuity

릴리스 방향: Phase 2와 3 다음의 별도 마이너 · 상태: 각각을 실사용한 뒤

실행 중인 동적 workflow의 Head를 교체해도 작업이 계속되게 한다.

> 이 저장소를 조사해서 필요한 작업을 알아서 구성하고 실행해.
> 지휘자의 사용량이 부족해지면 다른 세션으로 Head를 옮겨서 계속 진행해.

| 상태 | 관리 주체 |
| --- | --- |
| workflow 실행·분기·후속 작업 상태 | Workflow Runtime |
| 현재 지휘권과 epoch | Head Binding |
| 하네스의 대화·실행 세션 | 각 하네스 adapter |
| worker 실행, sealed result, inbox, 명시적 결정 | 기존 brgr 계약 |
| 원본 맥락과 인수인계 근거 | 이전 세션 기록과 Handoff Manifest |

출시 게이트는 실행 중인 worker, 대기 결과·질문, 이미 기록된 분기와 후속 작업을
유지하면서 새 Head가 workflow를 계속 진행하는 것이다. 인계와 workflow 재개를
각각 검증한 뒤 결합해, 실패가 어느 경계에서 발생했는지 확인할 수 있어야 한다.

## 투자 우선순위

점수는 현재 구조를 바탕으로 한 상대적 제품 판단이다. 검증된 공수나 일정을 뜻하지 않는다.
구현 순서는 Phase 0의 지속 개선 -> 2.5/2.6 -> 3 -> 4다.

| 투자 대상 | 효과 / 5 | 공수 / 5 | 판단 |
| --- | --- | --- | --- |
| 공통 Continuity 기반 | 5 | 3 | store/CLI는 v2.14.2. `claim`은 v2.15.0 |
| Head Migration | 5 | 4 | 수동 인계, 자동 퇴장, 사용량 조건부 인계(알림과 `--auto`)까지 출시 |
| Dynamic Workflows | 5 | 5 | 그다음 마이너. 실행 모델을 확장하므로 따로 낸다 |
| 하네스 호환성 | 4 | 2 | 매일의 경험이며 전부 패치다 |
| 계층 탭·위성 UI | 2 | 3 | 현재 로드맵에서 제외한다 |

## 릴리스 전략과 제품 범위

| 릴리스 | 내는 때 |
| --- | --- |
| v2.x 패치 | 대형 기능이 아닌 모든 것. 현행 경험, 호환, 버그, 작은 명령, 소유자 알림 구멍 |
| v2.x 마이너 | Head Migration, Dynamic Workflows, Unified Continuity가 각각의 출시 게이트를 통과했을 때뿐 |

대형 기능은 실제 사용과 복구 게이트를 충족했을 때 마이너로 낸다. 그 전에 쌓인
작은 수정은 패치로 먼저 낸다.

제품의 중심은 Herdr에서 사용 중인 실제 CLI 에이전트가 작업을 위임하고, 결과를
주고받고, 지휘권까지 이전하는 경험이다. Herdr와 CLI의 통합을 깊게 만들되,
core·store와 generic process 실행에 Herdr 의존성을 강제하지 않는다. Herdr는
작업 정체성이나 완료 여부를 대신 판단하지 않는다.

자체 IDE, 자체 채팅 UI, 범용 모델 게이트웨이, 계층 탭·위성 UI는 이번 로드맵의
범위에 넣지 않는다. 현재 자동 pane 배치를 활용하고 핵심 실행 모델에 투자한다.
별도 제품인 `me`와의 역할 분담이나 UI 확장은 필요할 때 따로 결정한다.

목표 경험: **지휘자가 바뀌어도 개발 작업은 계속 굴러간다.**
