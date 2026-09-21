# 2-4 깨끗한 환경 검증 증거 — 2026-09-21

별도 물리 Mac 대신 동일 머신 격리 디렉토리(`/tmp/pub24`, fresh home)에서 수행.
-release.yml의 릴리스 smoke와 동일한 절차.

## 다운로드·검증

- `gh release download v2.0.2` → `brgr-v2.0.2-aarch64-apple-darwin.tar.gz`,
  `checksums.txt`, `brgr.spdx.json`
- `shasum -a 256 -c checksums.txt` → archive OK, SBOM OK

## Gatekeeper

- `xattr -l`: `com.apple.provenance`만, quarantine 없음(tar 경유)
- `spctl -a`: `rejected` (무서명, 예상대로)
- `brgr --version` → `brgr 2.0.2` 정상 실행 (차단 없이 실행됨)

## 설치·fixture

- fresh home `/tmp/brgr-cleanmac.ZhKis5/home`, Codex 미연동 상태에서
  `doctor` → `needs_attention` (하네스 없음, 정상)
- `harness add testdata/fixtures/gjc` → contract 통과, scratch digest 기록
- foreground `run BRGR_FIXTURE_OK` → candidate, artifact
  `sha256:770fc671…` (`BRGR_FIXTURE_OK` 15바이트)
- `result` → `BRGR_FIXTURE_OK` 확인, `accept` → `accepted` (decision `ec82943b…`)
- Task `6653a6fe-395d-449b-a055-1c9516ffc821`

전역 보안 해제·quarantine 제거 없음.
