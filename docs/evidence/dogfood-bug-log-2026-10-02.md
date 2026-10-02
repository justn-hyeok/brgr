# Dogfood bug log, 2026-10-02

Bugs found by using brgr 2.9.3 (installed at `~/.local/bin/brgr`) from a Claude Code
session inside Herdr. Each entry is appended when found, with how it was observed.
Contract being tested: [Operating Contract](../../AGENTS.md#operating-contract).

Levels: **observed** = reproduced in a live run or read from the real store;
**code** = read from source, not run; **hypothesis** = inferred.

## Seed: found by reading code and the real store before dogfooding

| ID | Contract | Defect | Level |
|---|---|---|---|
| S1 | trust | Trust/Continue screens during startup are handed to the owner and the run waits indefinitely (`pane_adapter.rs` `agent_not_ready`, unclassified `timeout`, `menu_on_screen`). Real store: all 6 messages are these "waiting for an approval" questions. | observed |
| S2 | trust | An unclassified screen or menu that appears *while the agent works* is ignored: `drive` matches `_ => {}`, and `menu_on_screen` runs only at startup. | code |
| S3 | trust | `wait_ready` and the menu wait loop have no bound; only the attempt deadline ends them. | code |
| S4 | trust | Interactive Claude Code with `bypassPermissions` may show a first-run warning screen. | hypothesis |
| S5 | yolo | Custom manifests containing `--yolo`, `--dangerously-skip-permissions`, `--force`, `--sandbox`, ... are rejected (`registry/lib.rs:832`); the generic recipe has no permission flags. | code |
| S6 | yolo | `PermissionLevel` caps (`config set-max-permission`, parent bound in `admission.rs`) and gaps in recipes (`gjc` full is empty; `cline`/`opencode` edits is `None`) turn into `UnsupportedPermission`. | code |
| S7 | messages | Owner-to-worker replies are never pushed; 2 replies in the real store stayed unacked. | observed |
| S8 | messages | Completion push needs an idle `codex` owner. 131 notifications: 2 delivered, 102 never attempted, 15 retried 51-1075 times on "parent agent is not idle". | observed |
| S9 | messages | The Codex hook injects pending results but not pending worker questions. | code |
| S10 | messages | `question_notices` has 0 rows for 6 questions: no question push ever succeeded. | observed |
| S11 | messages | Non-`codex` owners (for example Claude Code) never receive pushes (`prompt_owner`, `register_current_surface`). | code |
| S12 | messages | Question withdraw depends on `BRGR_PARENT_ATTEMPT_ID` and reports failure only on an unread stderr. | code |
| S13 | reporting | A `lost` result's `error` can hold a dump of the pane runner's stdout instead of the cause. | observed |
| S14 | panes | 5 `lost` results from `pane_not_found` (caller pane gone), 2 from "Herdr-backed OMP wrapper did not provide a valid final result". | observed |

## Dogfood findings

Appended below as they are found. Format: `D<n>` · what was run · expected · actual ·
evidence · level.

### D1 · The binary under test is not built from any committed source

- Run: `brgr --help`, `strings ~/.local/bin/brgr`, `git worktree list`, `git status` in each worktree.
- Expected: `~/.local/bin/brgr` (reports `2.9.3`, built 2026-10-02 10:24) matches tag `v2.9.3` / `main@f646d0f`.
- Actual: it has commands the tag lacks (`debate`, `input`, `report`, default native TUI, `native-launch.json` run files). Their source exists only as **uncommitted changes** in `the friction worktree` (37 files, +2109/-1055, `debate.rs` untracked). The Herdr plugin source (`~/.config/herdr/plugins/github/brgr-9c4d4754689d`) is the clean `f646d0f`, so the plugin and the CLI on `PATH` are different programs under the same version string.
- Consequence: a same-version bug report cannot be reproduced from any SHA. Seed entries S1-S14 describe `main@v2.9.3`, not the binary run here; each must be re-checked against the binary before being treated as live.
- Level: observed.

### D2 · OpenCode stuck on an in-app update dialog and an MCP auth modal; brgr says `starting` forever and tells nobody

- Run: `brgr run "Create hello.py ..." --harness local.opencode --capture-diff` from a Claude Code pane, scratch git repo, task `38d33b4e`.
- Expected: the run reaches `running`, or the owner is told what blocks it.
- Actual, after 65+ s: the pane `w5G:p33` shows OpenCode's own "Update Complete ... Please restart the application" dialog and an "MCP Authentication Required" modal. Herdr reports the pane `idle` and gives it no agent name (`agent get brgr-38d33b4e` is `agent_not_found`). `brgr status` stays `phase: starting`, and `brgr message list --for owner` is empty, so no notice was sent.
- Two sub-defects: (a) an agent that updates **itself** and asks for a restart is not an "update offer" the Skip rule can answer, so the driver needs a restart rule; (b) "MCP Authentication Required" is **probably a corner toast, not a blocker**: the input box ("Ask anything...") is visible under it and only the centred "Update Complete" dialog is modal. Treating it as fatal would fail every OpenCode run for a user with an OAuth MCP server, so it is not a rule (hypothesis).
- Level: observed.

### D3 · Dismissing the update dialog quits OpenCode; the run ends `lost` with a cause that hides the update

- Run: `brgr input 38d33b4e --key enter` on the "Update Complete ... restart" dialog from D2.
- Expected: the dialog closes and the run continues, or brgr relaunches the agent.
- Actual: OpenCode exited, Herdr closed the pane (`pane_not_found`), and the task became `terminal` / `lost` with `error: "the Herdr-backed worker did not provide a valid final result: task pane no longer exists"`. The objective was never delivered (scratch repo still holds only `README.md`). `unresolved_effects` claims "the worker may still be running", although the pane and its process are gone.
- Defects: (a) no restart-after-self-update path, so one auto-update kills the task; (b) the error does not say the agent exited after an update; (c) `unresolved_effects` is wrong when the pane is provably gone.
- Level: observed.

### D4 · Claude's trust prompt is not accepted when the workspace path wraps in a narrow pane

- Run: `target/release/brgr run ... --harness local.claude-code` from this pane, task `915c0be5`, worktree under `~/Library/Application Support/...`.
- Expected: the folder-trust prompt for the task's own worktree is accepted automatically.
- Actual: the prompt stayed on screen. The path wrapped at "Application / Support" and the wrap took the space, so `trust_workspace_matches` (rows joined with no separator) compared `...ApplicationSupport...` with `...Application Support...` and refused. Nothing pressed the key; with the new deadline the run failed after 180 s with the screen text. Present in the worktree's pre-existing matcher, not only in the new code.
- Fix: the matcher walks the workspace path through the rows and accepts one dropped space at each row break; longer or different paths still fail (`trust_path_wrapped_at_a_space_still_matches`).
- Level: observed live; fix covered by unit test. A later live run completed, but **no `screens.log` entry exists for it, so the live press of the fixed matcher is not shown** (the prompt may not have appeared).

### D5 · The "unknown screen" notice quoted the shell line that launched the agent

- Run: the same stalled task, `brgr message list --for owner`.
- Actual: the notice listed the first rows of the pane, which were the `exec ... __tui-host ...` echo, not the dialog below it.
- Fix: quote the last 8 non-empty rows, where a dialog sits.
- Level: observed; fixed.

### D6 · brgr's own key presses left no record

- `eprintln!` from the pane runner goes nowhere a person reads, so an auto-press could not be audited.
- Fix: every press is appended to `runs/<task>-r<rev>.screens.log` (`resolve_presses_the_rule_keys_on_the_pane`).
- Level: observed; fixed.

### D7 · The trust prompt is not recognized in a very narrow worker pane

- Run: Claude tasks started while earlier dogfood panes were still open (their results were not yet decided, so their panes were not closed and every new pane got half of what was left).
- Actual: the pane was a few columns wide, so the header, the path and even the option text wrapped word by word ("Yes, I / trust / this / folder"). `screen.contains("Yes, I trust this folder")` failed, nothing was pressed, and the run ended `lost` ("task pane no longer exists") or after the 180 s deadline.
- Fix: detect the prompt with whitespace removed, and match the path either row by row or, when the header wraps too, with spaces removed and the prompt's next sentence required right after the path (`trust_prompt_in_a_very_narrow_pane_still_matches`).
- Related, not changed: pane clutter shrinks later panes, because a pane is only closed after its owner decides the result.
- Level: observed live; fixed.

### D8 · Review finding on the first version: mid-run handling of `unknown` status

- Found in review before release, not live. The first mid-run branch acted on `unknown` as well as `blocked`. Herdr never classifies GJC or Command Code, so they report `unknown` for their whole life and their own output (a `> 1.` Markdown quote, say) could be pressed or fail the run after 180 s, and the owner note was re-sent every poll.
- Fix: only `blocked` is acted on while the agent works. Test `menu_like_output_from_an_unclassified_agent_is_not_pressed_mid_run` fails with the old arm (two `send-keys` instead of one) and passes with the fix.
- Level: reproduced in a fixture; not seen live.

### D9 · `brgr status` showed `phase: starting` for the whole run

- Observed live on every native run: the phase stayed `starting` until `finished`, while the agent was working.
- Fix: the pane runner sets `working` after the prompt is delivered and `awaiting_input` while blocked, and back to `working` when the screen is answered. Display only. Seen live: `starting` at 4 s, `working` at 8 s, `finished` at the end.
- Level: observed; fixed.

### D10 · An agent that answers in chat and never writes its report hangs the run

- Run: OpenCode, "Run `sleep 30`, then write a one-line final answer" (task `63abbf03`).
- Actual: OpenCode printed "Done." in the TUI and went idle; no `report.md` was written. The native drive loop had no idle handling, so the run stayed `working` for minutes with nothing to tell the owner (the pre-rework loop had a reminder; the rework dropped it).
- Fix: an idle agent past a grace period with no sealed report gets, in order: a report file it wrote but did not seal is sealed for it; otherwise one reminder prompt; otherwise the run fails with `the agent finished without writing its report to <path>`. Waits are 15 s after seen work and 60 s if work was never seen; a debug-build variable shortens them for the test `an_agent_that_never_writes_its_report_fails_with_the_cause`.
- Level: observed live; fixed and covered by a fake-Herdr test. A later live OpenCode run wrote its report normally, so the reminder was not exercised live.

### D11 · Overwriting the installed binary with `cp` kills it with exit 137

- Run: `cp target/release/brgr ~/.local/bin/brgr` over the existing file (second install of the session).
- Actual: every invocation, even `--version`, exited 137 with no output, until the file was replaced through a new file and `mv`. The first overwrite had worked. Each new build also drifts the Codex hook digest, so `brgr integrate codex install` is needed afterwards (`brgr doctor` reports `drifted` until then).
- Installing by new file and `mv` is now the procedure (recorded in memory); the packaging scripts were not checked.
- Level: observed.

### Messaging, checked live with the installed build (owner = this Claude Code session)

- Owner to worker: a note sent mid-run (worker running `sleep 45`) was delivered into the worker's TUI and the worker put the requested word in its final answer.
- Worker to owner and back: the worker sent a `question` through the brgr message tooling, the owner saw it with `brgr message list --for owner`, replied with `--reply-to`, and the worker's final answer used the reply.
### D12 · A Claude Code owner never received a worker question or a completion notice

- Cause: the dispatcher registered an owner surface only for `codex:` and `worker:` owners and checked the pane against a Codex session id. A Claude Code owner has no hook binding and Herdr keeps no agent session for it, so nothing was pushed; the owner had to poll.
- Change: an owner with no Codex or explicit brgr session is identified as `claude:<CLAUDE_CODE_SESSION_ID>`. `brgr run` records the owner's pane at admission, from the owner's own shell where the pane can be proven (the detached dispatcher cannot prove it), and delivery checks that the pane still hosts a `claude` agent and is idle. Without a Herdr session id, the pane and agent kind are the whole identity, so another Claude Code started in the same pane would receive the notice.
- Tests: `idle_claude_owner_is_told_when_a_worker_asks` (fake Herdr: held while busy, sent once when idle).
- Live: with the derived identity, `owner_surfaces` holds `claude:b54b7525-...` -> `w5G:p1`, and the completion notice for task `10716914` reached the identity check and waited on "parent agent is not idle" (17 attempts while this session was working). **Confirmed live afterwards:** once this session went idle, the notice for task `10716914` arrived as a new prompt (`FROM BRGR {"type":"brgr_completion",...}`) and `completion_notifications` shows `delivered=1` after 398 attempts (the dispatcher polls every 500 ms while the owner is busy; there is no backoff).
- Level: observed live up to the idle wait; fixed.

### Every way to lower a worker's permission was removed

- Gap: a default of full permissions was not "always": `--permission`, a `permission` config value, `config set-max-permission`, a parent task's level or a stored task could lower a worker, which then waited on approvals nobody answers. Read-only requests also forced headless mode.
- Change: workers always launch with the harness's full-permission arguments (`permission_arguments` ignores the requested level). `--permission` and `config set permission` are accepted for old scripts and ignored with a note; `set-max-permission` and `clear-max-permission` are gone; new tasks store no level; a read-only request no longer turns the TUI off. The schema and config fields stay so stored data loads.
- Tests: registry recipes at every level give the full arguments, `a_requested_permission_never_lowers_a_worker`, `a_read_only_request_still_runs_the_tui_with_full_permissions`. Live: `--permission read-only` printed the ignored note, the receipt said `full`, Claude launched with `bypassPermissions` and finished.

### Independent review of the first PR (`/code-review`, 2026-10-03) and what was done

Real code defects, fixed with tests: (1) issue text could carry a quote of the pane: the unknown-screen error ends with the screen rows, and the class, title and sample kept the first row. Class and sample are now cut before the quote (`a_quoted_pane_screen_never_reaches_a_class_a_sample_or_an_issue`). (2) A "press Enter to continue" line was matched before an update menu, so Enter could land on "Update now"; the update menu is now checked first (`an_update_menu_with_a_continue_line_is_skipped_not_confirmed`). (3) Only the success path of `supervise()` reached the memo; recovered lost runs and runs that never started now do too. (4) One failing `gh` call dropped issue addresses already found in the same pass; progress is now kept and the error reported afterwards (`one_failing_gh_call_keeps_the_issue_addresses_already_found`). (5) A failed write to `screens.log` was swallowed; it is now reported.

Documentation that overclaimed, corrected in `AGENTS.md` rather than in code: the permission rule (an unset level is the harness's full level; explicit `--permission` and the cap remain opt-in), the rule table (no rule exists for MCP trust or bypass-mode warnings; they are reported and fail after three minutes), `self_update_env` (OpenCode and Claude only), the log (prompt submission is not logged), the failure note (bound owners only, three listed), pending messages (`brgr status TASK --tree`), the reviewer-suppression sentence (narrowed to tool permissions; owner decisions unaffected), and precedence against the parent workspace policy. Not changed: main's installed skill text still tells agents to cap permissions and to ask before approving prompts; the branch rewrites it, so the contradiction ends when the branch merges.

### Feature: failures reach the owner immediately

- Gap: a failed or lost run reached the owner only as a generic "verify this result" push, and only when the owner pane was idle and registered; otherwise nothing told it.
- Change: (1) the completion push for a failed or lost run is a `brgr_failure` notice with the outcome and reason; (2) every brgr command the owner runs ends with a stderr note listing each unacknowledged failed or lost run, until `brgr result TASK --ack`, so any agent that runs brgr sees it in its own tool output. Cancelled runs, candidates and other owners are not mentioned.
- Tests: unit tests for the note text, and two fake-Herdr/CLI tests (`a_failed_run_is_reported_on_every_owner_command_until_acknowledged`, `idle_codex_parent_is_told_a_run_failed_with_its_reason`). Live: after a forced lost run, `brgr doctor` printed the note on stderr.
- Live, real panes: the `brgr_failure` notice reached an idle Claude Code pane (task `5cfe0273`, delivered after this session went idle) and a real Codex pane (see D14 for whose). Not verified: the Codex Stop hook with a failure pending (no Codex pane could own a task, see D14), and the note's effect on an agent that ignores stderr.

### D13 · A real Codex pane stopped at its own folder-trust screen

- Observed: Codex 0.160 on a new folder shows "Trust this folder? ... > 1. Trust and continue / 2. Back to Agent Command Center". Added as rule `codex-trust` (accepted only for the task's workspace, moves to "Trust and continue" whichever option the cursor is on); covered by tests on the verbatim screen, not yet by a brgr-launched Codex worker.

### D14 · Cross-session leak: Herdr's record put a new Codex session on another Codex's pane

- Run: a Codex pane started by hand (`herdr agent start --kind codex`) while another Codex session (`ap-codex`, pane `w6G:p1`) was running; the new Codex ran `brgr run`.
- **Corrected diagnosis (the first version of this entry was wrong).** The new Codex did not read the other session's environment: the session id it passed (`01a0fb33...`) was its own, as its rollout file (`~/.codex/sessions/.../rollout-...-01a0fb33...jsonl`, cwd = the scratch repo) shows. The wrong value was the **pane**. Herdr's Codex hook script (`~/.codex/herdr-agent-state.sh`) reports `pane_id` from the hook's environment, and Codex runs hooks in the shared daemon, whose `HERDR_PANE_ID` is the first session's pane. So Herdr recorded the new session's id as the agent session of `w6G:p1`. brgr's `caller_pane::for_session` treats "Herdr says this pane's agent_session equals the session" as proof, so brgr's own hook told the model `--source-pane w6G:p1`, the model used it, and the task, the worker pane and the `brgr_failure` notice went to the other session's pane. brgr also writes the same kind of record itself (`report-agent-session` in `notification.rs`).
- Consequence: any code path that trusts Herdr's `agent_session` as pane proof for Codex is exploitable whenever two Codex panes exist; a nonce or token alone would not help while those paths remain.
- Level: observed live (independent review traced it through the rollout file, the hook script and the daemon's environment; the rollout cwd and the hook script's pane source were re-checked by hand); this test caused the injection into the other session.

### D15 · A deadline-elapsed pane run reported its progress line, including the task objective

- Run: `brgr run ... --deadline-seconds 20` on OpenCode.
- Actual: the lost result's error ended with the adapter's last stderr line, `brgr pane mode · prompted: <first line of the task objective>`. That says nothing about why it stopped and put the objective into the result error, into the error ledger's class and sample, and so into the memo and the issue text the memo feeds.
- Fix: a run stopped at its deadline reports `the attempt deadline elapsed`; the adapter no longer prints the objective; the error class and redaction cut at the adapter's progress marker; the one local ledger entry that already held an objective was removed. Tests: `delegated_lost_reason` at the deadline, `the_adapter_progress_line_never_reaches_a_class_or_a_sample`.
- Level: observed live; fixed.

### Feature: error memo and automatic issues

- `brgr_error_issue_memo.md` in the brgr home, fed by every failed or lost run; `brgr errors [preview|file]`; issue filing is off until `brgr config set-issue-reporting OWNER/NAME`. Tests (8, fake `gh`): same failure on another task lands on one entry, redaction of home paths, ids, addresses and tokens, one issue created at the threshold and none afterwards. Seen live: a forced lost run (pane closed) created the memo entry. No issue was filed on GitHub; the preview was read instead.

## Verification state of the screen-rule work (friction worktree, uncommitted)

- Checks: `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -D warnings`, `cargo test --workspace --locked` = 383 passed, 0 failed (baseline before the change: 345 passed).
- Live panes: OpenCode and Claude tasks reached `candidate` with sealed output. In fresh scratch repos whose path contains a space, Claude's trust prompt appeared and brgr pressed it: `screens.log` shows `workspace-trust down enter` for `f9ffa2c0` (twice, before the redraw delay) and `38f4248d` (once).
- Installed: `~/.local/bin/brgr` is this build (installed by new file and `mv`); earlier binaries are in the scratchpad as `brgr-installed-before`, `-2`, `-3`. `brgr integrate codex install` was run and `brgr doctor` says `ok`.
- Checked separately: Claude Code's docs name `DISABLE_AUTOUPDATER` (also in `claude doctor` output) as the env switch for background updates. Herdr's key names reach a real terminal as the bytes the rules rely on (`down` = `ESC [ B`, `up` = `ESC [ A`, `esc` = `ESC`, `enter` = newline), checked with a raw-mode reader in a live pane.
- **Not verified live**: the `update-applied` Esc rule (OpenCode was already updated and `OPENCODE_DISABLE_AUTOUPDATE=1` now prevents the dialog), the update-offer Skip and continue-notice rules (synthetic screens only), and the report reminder. The push to a Claude Code owner is verified live; a question push to a Claude owner is covered only by the fake-Herdr test.
- The Herdr plugin still installs the clean GitHub `v2.9.3`, so plugin-launched runs do not get these changes until this work is committed and released.
