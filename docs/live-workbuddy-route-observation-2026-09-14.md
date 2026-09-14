# Native OMP route and downgrade evidence — 2026-09-14

Scoped live proof for PR #12 source `ca31c66545fbfccd75b1d1bfc1b7f2f5f4ce4701`
(tree `f210fa19883079b9eb598fc4a5731678a2ec4582`). The tested local debug
binary SHA-256 was `dcf3cb317fd20ef53edf4c1f231bccfb6241261572a8802f46839f167b17db03`.
The previously published `v1.0.5` local binary SHA-256 was
`fc35612c1af622623304cfc441c52b8195f342234c54fdbafa5c1efd11bd4570`.
This is not a public-release or v1-wide completion proof.

An OMP `--mode=json` diagnostic run with the requested selector
`workbuddy/deepseek-v4.1-flash` produced a native assistant `message_end`
whose `provider` was `workbuddy` and `model` was `deepseek-v4.1-flash`. The
event contained no verified thinking/effort field. Brgr now compares every
assistant event to the requested full selector before candidate sealing;
`omp_fallback_model_cannot_be_sealed_as_requested_model` proves a different
`other/fallback` model becomes `failed` with no artifact, while a matching
model is reported through `brgr result`.

The patched binary performed a fresh real-model run, task
`3c5f4a06-9397-4f24-8a92-aebc17f19fed`, result
`23e11667-945c-4e17-aac7-610e92a1ea60`. Its sealed text was exactly
`BRGR_ROLLBACK_COMPAT_OK` (23 bytes), independently rehashed to
`sha256:d469f4ad7bf93beab09e9e705bd62bc09a11ee3273469b601893de646cd13588`.
The separately committed `brgr result` receipt recorded
`model=workbuddy/deepseek-v4.1-flash`, `model_source=harness_jsonl`, and
`effort_source=unavailable`; observation digest in SQLite was
`sha256:b8e3be9eb6632b0a55eba6e355495637eec6ccc3075b909daf3e808e4e41acff`.
The sealed result itself still validated against the unchanged strict
`schemas/result-v1.json`.

The installed **older** `v1.0.5` binary accepted that new candidate after
review, producing decision `4aff11db-980f-4cab-9219-bd65b07133b9` and
result digest
`sha256:f18c87ff914080bcc7cb03bced8ab588ee1955d44c0907c730f9ad72e47a7903`.
A read-only DB query found exactly one decision and `acknowledged=1`; retrying
the same decision through the patched binary returned the persisted decision
ID. This falsifies the earlier PR head's rollback P1, which had placed the
observation inside the versioned result JSON and made the older binary reject
the new result with a digest conflict.

The receipt binds native-model bytes to a result ID and is inserted in the
same SQLite transaction as the result and inbox. It is not an attestation by
the provider, and OMP's effort remains explicitly unobserved. The public
`v1.0.5` Release artifacts were not changed by this test.

## Intermediate-result preservation follow-up

At source `aec4ff844648118b110d9c0be0a7ddac300fb08a` (tree
`a30d25c78877eb63354ebd3e62215ba389ca8b81`), local debug binary
SHA-256 `f6846aa90649eed0558570ab8b1bfb388c5a6e9f64803596ebc58c999d10100d`,
the earlier unreleased **embedded-field** task
`3e63a022-9f45-4741-8170-853ac15052e6` was reopened without altering its
sealed result. `brgr result` returned its original native model both from the
legacy embedded field and the compatibility top-level view. Repeating its
existing accept reason returned the original decision
`1af23884-4716-4093-8060-29cd4ecb3cad` and original result digest
`sha256:4b7b3ef69f2c8594b2906d51d0e4b59ad49026865f8027bd66958726d4a2cc0b`.
The `unreleased_embedded_route_result_retains_its_original_decision_digest`
Store regression covers the same byte-preservation path. An intermediate
envelope is historical test data and does not validate against the strict
released v1 result schema; newly written envelopes do.
