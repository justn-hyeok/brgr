# brgr v1.0.1

This patch rejects a candidate with no sealed artifact, an invalid artifact
reference, or bytes that fail its digest/size check **before** creating the
durable owner inbox item. Tests cover the missing and forged cases. The four
Luna-backed harness paths and public unsigned macOS arm64 distribution remain
as in [v1.0.0](https://github.com/justn-hyeok/brgr/releases/tag/v1.0.0).

The archive is unsigned and not notarized. Download its archive, checksums,
and SPDX SBOM together; verify the hashes and follow the
[distribution guide](https://github.com/justn-hyeok/brgr/blob/v1.0.1/docs/unsigned-distribution.md).
The same cooperative-local, local-cancellation, and best-effort Herdr cleanup
limits apply. This patch does not claim adversarial worker isolation or
provider-side cancellation.
