# Four-harness minimum-effort Luna run — 2026-09-14

This opt-in live test used a local `brgr 1.0.6` debug binary with SHA-256
`473bd0d2f96a7d7623995e80a3f403d05fe12738f3265f571ca10e1524475338`,
built from main `f10c1dc64766b15164983c5f58eed8ab2907458e` plus the
Command Code `--effort` manifest change in this evidence commit. It is not a
test of the public v1.0.6 binary. Four separate non-Git workspaces and
supervisor homes are retained at `/private/tmp/brgr-luna-min.93yuiB/`.

Installed CLI evidence selected the lowest exposed Luna setting for each
harness: OMP's native catalog lists `low` as its lowest thinking level; GJC
offers `minimal` and a paid scratch run succeeded; Cursor CLI has no separate
effort flag, but its native catalog offers `gpt-5.6-luna-none`; Command Code
rejected `--effort none` with `Supported: low, medium, high, xhigh, max`, then
succeeded with `low`. This is not a claim that providers attested the effort.

Each row passed authorized paid scratch activation and a fresh bounded run.
`brgr result` returned the exact marker below (Cursor and Command Code include
one trailing newline). A separate `shasum -a 256` over the stored artifact
matched the reference. The owner then explicitly accepted. Read-only SQLite
joins showed one matching result/decision digest and `acknowledged=1` in each
home, binding epoch 1.

| Harness | Requested Luna / minimum effort | Task / result / decision IDs | Exact marker | Artifact SHA-256 |
|---|---|---|---|---|
| OMP process, no Herdr | `openai-codex/gpt-5.6-luna` / `low` | `07c6c094-78b1-4c3a-896c-ed63ea037af1` / `be690aa5-6d7a-45df-82f4-6961d4b140e4` / `da5fd0c3-910d-445a-826c-f1d9df092e86` | `BRGR_OMP_LUNA_MIN_9E4B` | `7aed9564eb1a009aa47b2ae6630956fe7fc715252cc75b982c1621462281f2c4` |
| GJC | `openai-codex/gpt-5.6-luna` / `minimal` | `b3856e24-d107-4ea7-9a22-c654c3889356` / `fdb010fd-11e6-41a0-9473-bf731213fb24` / `f8bfa7cd-7ae7-4d55-8148-773f6f5c8b65` | `BRGR_GJC_LUNA_MIN_7C21` | `fa332814ba6a9d9606cd2d0fcb2c8f260695a9d4394affc0645a1d480a74fc34` |
| Cursor CLI | `gpt-5.6-luna-none` / encoded `none` | `89de126d-d2cd-4e63-aaba-145743174a0e` / `d81f1613-865e-41b5-a460-bc30b218758b` / `6a834c24-00b2-4a62-96a0-8f2ffca77682` | `BRGR_CURSOR_LUNA_MIN_52D8` | `f4cbc43fc0639f66cbb3bbed1f04d2827b8fbdc71e13ecd4991ea66ac18022f4` |
| Command Code | `gpt-5.6-luna` / `low` | `188afe3d-bd34-4fee-b9a8-985b381f9696` / `f0480794-92f7-4ba5-b0d4-868971f5cf23` / `199a4de2-373a-4d29-b2bf-abe9925089e1` | `BRGR_COMMAND_LUNA_MIN_4F63` | `4ab933fad363b4c7be8c39e918f84644e33692d272d98c385216af89f1f19ecd` |

OMP and GJC JSONL separately reported the requested native model
`openai-codex/gpt-5.6-luna`; their native effort remained unavailable. Cursor
CLI and Command Code returned stdout only, so their native model and effort
identity remain unavailable beyond the catalog, argv route, and response. No
silent substitution was requested by brgr. This proves the narrow four-stage
happy path, not the full v1 completion checklist or provider-side fallback
exclusion for stdout-only harnesses.
