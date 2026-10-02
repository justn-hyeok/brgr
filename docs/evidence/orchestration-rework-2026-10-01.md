# Orchestration rework evidence — 2026-10-01

Snapshot: task worktree based on `f646d0f`, with uncommitted implementation.
This records local development evidence, not a published release.
Updated: 2026-10-02. Target worktree:
`/Users/justn/dev/.worktrees/brgr-automation-friction-20261001`.

## Live Claude Code

- Task `917f977f-1ff3-4d09-bd21-fedb18a5eba7`: owned native TUI, full permission,
  automatic workspace trust, `NATIVE_QA_OK` report, owner decision, and automatic
  owned pane close. Its report-only source diff exposed the exclusion bug;
  that diff was inspected and not applied.
- Task `76545faa-d393-4e45-8695-998092ea12a1`: native first turn returned
  `READY_FOR_NATIVE_MESSAGE`. A later owner note was delivered to the TUI,
  acknowledged by the worker, and produced `NATIVE_MESSAGE_OK` with an empty
  sealed source diff. Native delivery state was `delivered`, message ack was
  true, owner accepted, and cleanup recorded `closed`.
- Task `50ab8ad0-85cc-4d8e-810e-f9421dcbacde`: explicit read-only permission
  remained native plan mode. The response hook captured `READONLY_NATIVE_OK`
  without a worker report-file write. Its source diff was empty; after owner
  acceptance the pane closed, the report was archived, and Git status was clean.
- Debate group `cf7bee41-df35-4aef-825c-54cd2517674a`: participants
  `0d698929-4ed6-4a6d-aa1e-bc67d91b2b4d` and
  `f8d6c7d0-3a9b-449d-897e-350cd945734d` sent each other native notes directly.
  Both peer deliveries were `delivered` and acknowledged. Each returned the
  other participant's exact token with an empty source diff; both were accepted,
  closed automatically, and their reports archived. This was two Claude TUIs,
  not a live mixed-harness chain.
- Earlier failed probes exposed empty Herdr success responses, premature shell
  readiness, startup detection delay, and the idle-report reminder. Those
  attempts retain their original failed/lost results. Owned probe panes were
  closed; test notifications were disabled in the isolated test home.

## Live GJC

Task `8f8d1261-b14f-41ff-b313-4ed15c4a59bb` opened its actual GJC TUI and received
the objective. The configured model returned an insufficient-credits error.
No final report was produced, so this is not a successful completion check.
brgr did not switch the harness/model to make the check pass.

## Live OMP

Task `162e70fc-80a3-4cce-af6a-51fe0564f9e9` opened the actual OMP TUI,
received the objective, and used the native `--approval-mode=yolo` argument.
The configured Laguna S 2.1 Free route returned insufficient credits and a
retry-delay error. The original 90-second deadline ended the attempt without
a final report; the lost result was acknowledged and its owned pane closed.
The earlier startup-classification probe retains its original failed result.
Neither attempt is a successful provider completion check.

## Native execution matrix

These are activated recipe declarations, separated from live completion proof.
Every row launches the registered executable interactively in the task cwd;
ordinary input uses the native prompt/Herdr transport. A completed file report
requires `__seal-report`; idle or a partial file does not certify completion.

| Harness | Native TUI entrypoint and full permission argv | Result channel | Live evidence |
| --- | --- | --- | --- |
| Claude Code | `claude --permission-mode bypassPermissions` | Sealed report; read-only native Stop hook | Trust, owner follow-up, read-only result, two-Claude debate, handling and cleanup passed |
| GJC | `gjc`; full recipe adds no permission flag | Sealed report | TUI and initial prompt reached; configured provider credits blocked completion |
| OMP | `omp --approval-mode=yolo` | Sealed report | TUI and initial prompt reached; configured provider credits blocked completion |
| Cursor CLI | `cursor-agent --trust --force` | Sealed report; read-only response hook | Recipe/fixtures checked; live completion not run |
| Command Code | `command-code --trust --no-auto-update --skip-onboarding --permission-mode yolo` | Sealed report | Recipe/fixtures checked; live completion not run |
| Devin | `devin --respect-workspace-trust false --permission-mode dangerous` | Sealed report | Recipe/fixtures checked; live completion not run |
| Cline | `cline --auto-approve true` | Sealed report | Recipe/fixtures checked; live completion not run |
| OpenCode | `opencode --auto` | Sealed report | Recipe/fixtures checked; live completion not run; requested TUI effort fails explicitly when unsupported |

Native authentication and configured model choices were retained. The live QA
home had notifications disabled and tab placement selected. No test completion
callback is claimed as proof of actual owner-notice delivery.

## Local oracle

Tests cover typed ownership and attempt relationships, immutable terminal
results, peer opt-in and replay conflicts, native message claims, CLI settings,
instruction snapshots, explicit execution shape, and native pane transport.
Recovery fixtures distinguish an unfinished report from a completed digest,
recover after a pane disappears, preserve one inbox item, retain the original
deadline, and retry close without deleting the report.

Independent serial `codex review` findings were reproduced and fixed, including
partial reports after Herdr lookup failure, unclassified native parent-notice
delivery and cleanup, and additional argv overriding a manifest effort selector
(OpenCode `--variant`). Other regressions cover report FIFOs, read-only output,
question acknowledgment versus answer, disabled notifications, empty diffs, and
tracked/generated report exclusions. A final live-receipt audit also exposed an
already absent pane left in pending cleanup: only Herdr's typed `pane_not_found`
response now settles that receipt, while transport failures keep retry state.

The final local oracle passed: **345 tests, zero failures/ignored**, including
workspace/all-feature tests and doctests, formatting, strict
workspace/all-target/all-feature clippy, and a locked release build.
Logs are under `/tmp/brgr-final-native-*.log` and
`/tmp/brgr-final-cleanup-*.log`; live audit is
`/tmp/brgr-final-herdr-audit-20261002.log`.

## Local installation

The release executable is installed at `/Users/justn/.local/bin/brgr`. The
previous executable was retained in a temporary local backup. Codex integration
reports three expected/present hooks with current executable and skill digests.
`config check` reports `tui`, `full`, and `ok`; it checks configuration, not
provider credit availability. Existing user configuration was preserved.
`config init` created the missing instruction example at
`/Users/justn/Library/Application Support/brgr/BRGR.md`.
Installed and release executable SHA-256 both match
`aee307322a1201d400300c95e8a066a62e19e1bc284aea2b119da9941eb2753c`.
The installed executable rechecked all eight recorded final QA attempts:
each was terminal with cleanup `closed` and no cleanup error, including the
previously stale absent-pane receipt. The installed read-only task's diff stat
was an empty file list with zero patch bytes. The live Herdr audit contained
none of the QA receipt pane IDs. The installation receipt is
`/tmp/brgr-final-install-20261002.json`.

The brgr, model-orchestration, and Herdr skill changes passed their skill
validators. Guidance now covers orchestration operations and existing
authorization rather than imposing a blanket native-approval question.
No commit, push, merge, worktree deletion, or release was performed.

Live nested cross-harness conversation, every provider's native completion,
and provider login/credit availability have not all been verified by this
receipt. Their absence is a recorded limitation, not a pass.
