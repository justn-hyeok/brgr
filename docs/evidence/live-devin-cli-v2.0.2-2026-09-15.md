# Devin CLI v2.0.2 live evidence — 2026-09-15

Scope: the new `local.devin` one-shot process recipe on this macOS user account.
This is a live personal-use receipt, not proof for every Devin account, model,
or future CLI version.

- Installed executable: Devin CLI `3000.10.27` (`bcbe88c7`), canonical path
  `/Users/justn/.local/share/devin/cli/_versions/3000.10.27/bin/devin`, SHA-256
  `f3fb3868c38c83826951ce71b803d5ad7d7d402eaa56f6dc9bca7a14cbfa5de7`.
- Authentication and `devin doctor --json` succeeded. A stale configured
  `swe-2-high` default initially disagreed with the backend's available model.
  Selecting **SWE-1.6** once in the interactive Devin model picker established
  a working account mapping; a fresh prompt-file print process then returned
  the exact requested marker with empty stderr.
- `devin models list --format json` returned 176,077 bytes on this account,
  above brgr's 64 KiB model-probe bound. The recipe therefore uses Devin's
  configured model and declares brgr model/effort selection unsupported.
- The final local `brgr 2.0.2` release candidate had SHA-256
  `4684db837da2ee9865a107a189776d4152fb1073c4bf11c012d7f6eda35b0341`.
  Scratch activation in `/private/tmp/brgr-devin-v202-final.kI4S0b` produced nonempty
  scratch digest
  `8295656d0ee1882701720ae6bd0a66c2270333a35af537036af930695f76e184`;
  `brgr harness status local.devin` returned `healthy`.
- Fresh managed task `21cdc70c-2e6d-46ca-94bf-90ab17e11c0f` produced result
  `ddbb2da4-e036-4841-b603-74ce405964ba`. Its sealed 23-byte artifact was
  exactly `BRGR_DEVIN_V202_RUN_OK\n`, SHA-256
  `0d64aeb0608a493addd2fbe04617d36d0044a5df6f2d96da676d219edd965bf8`.
- Codex independently compared the sealed bytes and recorded accepted decision
  `eb8ab0ee-211e-412e-be60-bec0a5bbc6b5`. The result digest was
  `e75905661283c4f3cf3d6afadc03fb5c3a1c856756b6ca82f9e2401cefc727d9`.

Devin stdout does not expose a native model receipt, so the stored
`route_observation` correctly reports model and effort as `unavailable`. The
interactive SWE-1.6 label proves the local setup choice, not provider-side
model identity for the managed result.
