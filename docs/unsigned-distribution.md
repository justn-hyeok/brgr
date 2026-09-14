# Unsigned macOS Distribution

The first public `brgr` archives are built for Apple Silicon on macOS 15 or
newer. They are not signed with Apple Developer ID and are not notarized.
macOS therefore cannot verify the publisher or confirm that Apple checked the
download for malware.

Before running a downloaded archive:

1. Download the `.tar.gz`, `brgr.spdx.json`, and `checksums.txt` from the same GitHub release.
2. Verify it with `shasum -a 256 -c checksums.txt` from that directory.
3. Extract the archive and move `brgr` to a directory on your `PATH`.
4. Try `brgr --version`. If macOS blocks it, review the warning and use the
   per-item **Open Anyway** control in System Settings > Privacy & Security only
   if you trust the repository and verified the checksum.

The project never asks users to disable Gatekeeper globally and never removes
quarantine metadata automatically. See Apple's guidance on safely opening
software from an unidentified developer:
<https://support.apple.com/en-gb/102445>.
