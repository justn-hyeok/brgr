# 2-4 깨끗한 별도 Mac 검증 절차서 — 2026-09-21

대상: Apple Silicon macOS 15+ 깨끗한 별도 Mac 1대. 전역 보안 해제·quarantine
자동 제거 금지. 소요 ~30분.

## 준비

1. GitHub release `v2.0.2`에서 `brgr-v2.0.2-aarch64-apple-darwin.tar.gz`,
   `checksums.txt`, `brgr.spdx.json` 다운로드.

## 검증

2. `shasum -a 256 -c checksums.txt` — archive·SBOM 모두 OK여야 함.
3. 압축 해제 후 `brgr`를 `PATH` 디렉토리로 이동.
4. `brgr --version` → macOS가 차단하면 경고문 확인 후 **개별 항목만**
   시스템 설정 > 개인정보 보호 및 보안 > `Open Anyway`로 허용.
   `xattr -d com.apple.quarantine` 같은 일괄 해제는 금지.
5. `brgr doctor` → store/registry ok, 하네스 미등록 상태 확인.
6. fixture 실행:
   ```sh
   scratch=$(mktemp -d "${TMPDIR}/brgr-cleanmac.XXXXXX")
   mkdir "$scratch/work"
   brgr --home "$scratch/home" harness add /path/to/any-approved-CLI \
     --workspace "$scratch/work" --prompt "small authorized probe"
   ```
   승인된 CLI가 없으면 repo 동봉 `testdata/fixtures/gjc` 사용 불가 —
   release.yml의 릴리스 smoke와 동일한 fixture 경로이므로, 로컬 checkout 없이
   검증하려면 승인된 설치 CLI(GJC/OMP/Cursor 중 택1)로 작은 유료 scratch 1회.
7. `brgr --home "$scratch/home" doctor` → 해당 하네스 `healthy`.

## 기록

task/revision/result/decision ID, 아티팩트 해시, Gatekeeper 경고문 스크린샷
또는 문구를 `docs/`에 영수증으로 추가 후 2-4를 close.

## 현재 상태

본 머신에서는 수행 불가 — 별도 Mac 확보 후 위 절차 실행 필요.
