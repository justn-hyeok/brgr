//! What brgr presses so a worker is never left for a person to click.
//!
//! Orchestrating through brgr is an unconditional-trust contract, so a trust
//! prompt, a "continue" notice, an update offer, or an update that has already
//! been applied is answered here. A screen no rule covers is reported with its
//! text and given a deadline instead of waiting forever.

use std::{
    fs::OpenOptions,
    io::Write as _,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, bail};

use super::{
    Herdr,
    native::{squash, trust_workspace_matches},
};
use crate::{Paths, cli::PaneRunArgs};

/// How long the agent gets to redraw after a key press before the screen is
/// read again. Without it the same prompt is answered twice.
const REDRAW: Duration = Duration::from_millis(1500);

/// How long a screen no rule covers may stand before the run fails.
pub(super) const SCREEN_DEADLINE: Duration = Duration::from_mins(3);

/// The unknown-screen deadline, shortened for tests only.
fn screen_deadline() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(millis) = std::env::var("BRGR_TEST_SCREEN_DEADLINE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_millis(millis);
    }
    SCREEN_DEADLINE
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum Screen {
    /// No dialog brgr recognizes.
    Clear,
    /// A dialog a rule answers with these keys.
    Resolve {
        rule: &'static str,
        keys: Vec<&'static str>,
    },
    /// A menu or dialog no rule answers; carries its first lines.
    Unknown(String),
}

/// Classifies a pane's visible text. Pure: it never reads or presses anything.
pub(super) fn classify(screen: &str, workspace: &Path) -> Screen {
    if trust_workspace_matches(screen, workspace) {
        let options = menu_options(screen);
        let yes = options
            .iter()
            .position(|option| option.text.starts_with("Yes, I trust this folder"));
        let keys = if let (Some(selected), Some(yes)) =
            (options.iter().position(|option| option.selected), yes)
        {
            // A numbered menu: move to "Yes" from wherever the cursor is.
            let mut keys = keys_between(selected, yes);
            keys.push("enter");
            keys
        } else {
            let flat = squash(screen);
            let selected_yes = ["❯", "›", ">"]
                .iter()
                .any(|cursor| flat.contains(&format!("{cursor}Yes,Itrustthisfolder")));
            if selected_yes {
                vec!["enter"]
            } else {
                vec!["down", "enter"]
            }
        };
        return Screen::Resolve {
            rule: "workspace-trust",
            keys,
        };
    }
    if let Some(resolution) = codex_trust(screen, workspace) {
        return resolution;
    }
    let lower = screen.to_lowercase();
    // A trust prompt for some other folder is not the task's to accept.
    if squash(&lower).contains("trustthisfolder") {
        return Screen::Unknown(head(screen));
    }
    // The harness updated itself and wants a restart. The dialog carries an
    // `esc` affordance; Enter ("ok") is what quit OpenCode.
    if lower.contains("update complete")
        && lower.contains("restart")
        && screen
            .lines()
            .any(|line| matches!(line.trim().to_lowercase().as_str(), "ok" | "esc"))
    {
        return Screen::Resolve {
            rule: "update-applied",
            keys: vec!["esc"],
        };
    }
    let options = menu_options(screen);
    let selected = options.iter().position(|option| option.selected);
    if let (Some(selected), Some((rule, target))) = (selected, accepting_option(&lower, &options)) {
        let mut keys = keys_between(selected, target);
        keys.push("enter");
        return Screen::Resolve { rule, keys };
    }
    // An update menu comes first: its highlighted option is "Update now", so a
    // stray "press Enter" line on the same screen must not become Enter.
    if let Some(selected) = selected
        && is_update_offer(&lower)
        && let Some(skip) = options.iter().position(|option| is_skip(&option.text))
    {
        let mut keys = keys_between(selected, skip);
        keys.push("enter");
        return Screen::Resolve {
            rule: "update-offer-skip",
            keys,
        };
    }
    if selected.is_none() {
        return if has_continue_notice(screen) {
            Screen::Resolve {
                rule: "continue-notice",
                keys: vec!["enter"],
            }
        } else {
            Screen::Clear
        };
    }
    Screen::Unknown(head(screen))
}

/// A harness offering to update itself, not an agent asking about updating
/// something: "update" or "upgrade" with an offer's wording or a version.
fn is_update_offer(lower: &str) -> bool {
    (lower.contains("update") || lower.contains("upgrade"))
        && (lower.contains("available")
            || lower.contains("new version")
            || lower.contains("newer version")
            || has_version_number(lower))
}

/// A dotted version such as `0.2.0` or `v2.0.15`.
fn has_version_number(text: &str) -> bool {
    text.split(|character: char| !(character.is_ascii_digit() || character == '.'))
        .any(|token| {
            let parts: Vec<&str> = token.trim_matches('.').split('.').collect();
            parts.len() >= 3 && parts.iter().all(|part| !part.is_empty())
        })
}

/// The cursor presses that move from option `from` to option `to`.
fn keys_between(from: usize, to: usize) -> Vec<&'static str> {
    let key = if to > from { "down" } else { "up" };
    std::iter::repeat_n(key, to.abs_diff(from)).collect()
}

/// Claude Code's own gates in front of a run, answered by accepting: the
/// Bypass Permissions warning and a newly found MCP server.
fn accepting_option(lower: &str, options: &[MenuOption]) -> Option<(&'static str, usize)> {
    let position = |prefix: &str| {
        options
            .iter()
            .position(|option| option.text.to_lowercase().starts_with(prefix))
    };
    if lower.contains("bypass permissions mode") {
        return position("yes, i accept").map(|at| ("bypass-warning", at));
    }
    if lower.contains("mcp server") {
        // The single-server choice first; "use this and all future" only if it
        // is all there is.
        return position("use this mcp server")
            .or_else(|| position("use this"))
            .map(|at| ("mcp-trust", at));
    }
    None
}

fn has_continue_notice(screen: &str) -> bool {
    screen.lines().any(|line| {
        let line = line
            .trim()
            .trim_matches(['│', '┃', '║'])
            .trim()
            .to_lowercase();
        [
            "press enter to continue",
            "press return to continue",
            "press any key to continue",
        ]
        .iter()
        .any(|notice| line.contains(notice))
    })
}

/// Codex's own folder prompt ("Trust this folder? ... 1. Trust and continue").
/// Accepted only for the task's workspace, like Claude's.
fn codex_trust(screen: &str, workspace: &Path) -> Option<Screen> {
    let flat = squash(screen);
    if !flat.contains("Trustthisfolder?") || !flat.contains("Trustandcontinue") {
        return None;
    }
    let workspace = squash(&workspace.to_string_lossy());
    // The path must end where the prompt's own sentence begins, so a longer
    // path ("/repo/task-evil", "/repo/task/sub") is not the task's workspace.
    let shown = flat.split_once("Folderaccess").is_some_and(|(_, after)| {
        after
            .strip_prefix(workspace.trim_end_matches('/'))
            .is_some_and(|rest| rest.trim_start_matches('/').starts_with("Trustthisfolder"))
    });
    let options = menu_options(screen);
    let target = options
        .iter()
        .position(|option| option.text.starts_with("Trust and continue"));
    let (true, Some(target)) = (shown, target) else {
        return Some(Screen::Unknown(head(screen)));
    };
    let selected = options
        .iter()
        .position(|option| option.selected)
        .unwrap_or(target);
    let mut keys = Vec::new();
    keys.extend(std::iter::repeat_n(
        if target > selected { "down" } else { "up" },
        target.abs_diff(selected),
    ));
    keys.push("enter");
    Some(Screen::Resolve {
        rule: "codex-trust",
        keys,
    })
}

/// Presses what the rule table says for this pane, unless the agent is working.
///
/// A working agent's own output can contain `› 1.`-style lines, so nothing is
/// pressed while Herdr reports `working`.
pub(super) fn resolve(
    herdr: &Herdr,
    pane: &str,
    run: &PaneRunArgs,
    paths: &Paths,
    status: Option<&str>,
) -> Result<Screen> {
    let workspace = run.workspace.as_path();
    if status == Some("working") {
        return Ok(Screen::Clear);
    }
    let Some(screen) = herdr.screen(pane) else {
        return Ok(Screen::Clear);
    };
    let outcome = classify(&screen, workspace);
    if let Screen::Resolve { rule, keys } = &outcome {
        let mut call = vec!["pane", "send-keys", pane];
        call.extend(keys.iter().copied());
        herdr.call(&call).map_err(|failure| {
            anyhow::anyhow!("brgr could not answer the {rule} screen: {failure}")
        })?;
        eprintln!("brgr pane mode · {rule}: pressed {}", keys.join(" "));
        record(&log_path(paths, run), pane, rule, keys);
        std::thread::sleep(REDRAW);
    }
    Ok(outcome)
}

/// The per-attempt record of every key brgr pressed on its own.
pub(super) fn log_path(paths: &Paths, run: &PaneRunArgs) -> PathBuf {
    paths
        .runs
        .join(format!("{}-r{}.screens.log", run.task, run.revision))
}

fn record(path: &Path, pane: &str, rule: &str, keys: &[&str]) {
    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let written = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| writeln!(file, "{at} {pane} {rule} {}", keys.join(" ")));
    if let Err(error) = written {
        eprintln!("brgr pane mode · could not log the {rule} key press: {error}");
    }
}

/// Bounds how long a screen no rule covers may stand.
pub(super) struct ScreenWatch {
    kind: String,
    limit: Duration,
    since: Option<Instant>,
    presses: Vec<(&'static str, u32)>,
}

/// How many times one rule may answer the same standing screen before the run
/// fails: a screen a key does not clear would otherwise be pressed forever.
const PRESS_BUDGET: u32 = 5;

impl ScreenWatch {
    pub(super) fn new(kind: &str) -> Self {
        Self::with_limit(kind, screen_deadline())
    }

    pub(super) fn with_limit(kind: &str, limit: Duration) -> Self {
        Self {
            kind: kind.to_owned(),
            limit,
            since: None,
            presses: Vec::new(),
        }
    }

    /// Records a key press for `rule`. Fails when it keeps being needed.
    pub(super) fn pressed(&mut self, rule: &'static str) -> Result<()> {
        let count =
            if let Some((_, count)) = self.presses.iter_mut().find(|(name, _)| *name == rule) {
                *count += 1;
                *count
            } else {
                self.presses.push((rule, 1));
                1
            };
        if count > PRESS_BUDGET {
            bail!(
                "the {} agent kept showing a screen brgr answers ({rule}); {PRESS_BUDGET} presses did not clear it",
                self.kind
            );
        }
        Ok(())
    }

    /// Records a poll. Fails once an unknown screen has stood past the limit.
    pub(super) fn observe(&mut self, screen: &Screen) -> Result<()> {
        match screen {
            Screen::Unknown(text) => self.stuck(text),
            Screen::Clear => {
                self.since = None;
                self.presses.clear();
                Ok(())
            }
            Screen::Resolve { .. } => {
                self.since = None;
                Ok(())
            }
        }
    }

    /// Records a poll where Herdr reports `blocked` but no rule answered.
    pub(super) fn observe_blocked(&mut self, screen: Option<&str>) -> Result<()> {
        self.stuck(&screen.map(head).unwrap_or_default())
    }

    pub(super) fn reset(&mut self) {
        self.since = None;
        self.presses.clear();
    }

    /// Stops the unknown-screen clock but keeps the press counts, for an
    /// agent whose screen moved without Herdr saying it works.
    pub(super) fn restart_clock(&mut self) {
        self.since = None;
    }

    fn stuck(&mut self, text: &str) -> Result<()> {
        let since = *self.since.get_or_insert_with(Instant::now);
        if since.elapsed() >= self.limit {
            bail!(
                "the {} agent stayed on a screen no brgr rule answers for {}s: {text}",
                self.kind,
                self.limit.as_secs()
            );
        }
        Ok(())
    }
}

struct MenuOption {
    selected: bool,
    text: String,
}

/// The numbered options on screen, in order. `selected` marks the one a
/// cursor glyph points at.
fn menu_options(screen: &str) -> Vec<MenuOption> {
    screen
        .lines()
        .filter_map(|line| {
            let line = line.trim().trim_matches(['│', '┃', '║']).trim();
            let (selected, rest) = ["›", "❯", ">", "▸"]
                .iter()
                .find_map(|cursor| line.strip_prefix(cursor))
                .map_or((false, line), |rest| (true, rest.trim_start()));
            let digits = rest.chars().take_while(char::is_ascii_digit).count();
            let after = rest.get(digits..)?;
            (digits > 0 && (after.starts_with('.') || after.starts_with(')'))).then(|| MenuOption {
                selected,
                text: after[1..].trim().to_owned(),
            })
        })
        .collect()
}

fn is_skip(option: &str) -> bool {
    let option = option.to_lowercase();
    !option.contains("update now")
        && !option.contains("install")
        && [
            "skip",
            "not now",
            "later",
            "no thanks",
            "dismiss",
            "remind me",
        ]
        .iter()
        .any(|word| option.contains(word))
}

/// Marks the quoted screen in an error; nothing after it leaves the machine.
pub(crate) const LAST_SCREEN: &str = "last screen:";

/// The last rows of the screen, for an error message.
pub(super) fn tail(screen: &str) -> String {
    head(screen)
}

/// The last rows of the screen. A dialog sits at the bottom, below whatever
/// scrolled past (the shell line that launched the agent, for one).
fn head(screen: &str) -> String {
    let rows: Vec<&str> = screen
        .lines()
        .map(str::trim)
        .filter(|row| !row.is_empty())
        .collect();
    rows[rows.len().saturating_sub(8)..].join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The screen of the `OpenCode` pane that stalled task `38d33b4e` (D2 in the
    /// dogfood log), kept verbatim apart from trailing blank rows.
    const OPENCODE_UPDATED: &str = "\
                                                                                 ┃                               ┃
                                                                                 ┃  MCP Authentication Required  ┃
                                                                                 ┃                               ┃
                                                                                 ┃  Updating to v2.0.15…         ┃
                                                                                 ┃                               ┃

                              Update Complete                                      esc

                              Successfully updated to OpenCode v2.0.15. Please
                              restart the application.


                                                                                 ok

                     ┃
                     ┃  Ask anything… \"Fix broken tests\"
                     ┃
                     ┃  Build auto · Big Pickle OpenCode Zen
                     ╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀
                                                                     tab agents  ctrl+p commands
";

    fn work() -> &'static Path {
        Path::new("/repo/task")
    }

    #[test]
    fn an_applied_self_update_is_dismissed_with_esc_not_enter() {
        assert_eq!(
            classify(OPENCODE_UPDATED, work()),
            Screen::Resolve {
                rule: "update-applied",
                keys: vec!["esc"],
            }
        );
    }

    #[test]
    fn an_mcp_auth_toast_alone_does_not_block_the_run() {
        let toast = OPENCODE_UPDATED
            .lines()
            .filter(|line| !line.contains("Update Complete") && !line.contains("restart"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(classify(&toast, work()), Screen::Clear);
    }

    #[test]
    fn the_claude_trust_prompt_for_the_task_workspace_is_accepted() {
        let screen =
            "Accessing workspace:\n\n/repo/task\n\n❯ No, exit\n  Yes, I trust this folder\n";
        assert_eq!(
            classify(screen, work()),
            Screen::Resolve {
                rule: "workspace-trust",
                keys: vec!["down", "enter"],
            }
        );
        let selected =
            "Accessing workspace:\n\n/repo/task\n\n  No, exit\n❯ Yes, I trust this folder\n";
        assert_eq!(
            classify(selected, work()),
            Screen::Resolve {
                rule: "workspace-trust",
                keys: vec!["enter"],
            }
        );
    }

    /// Verbatim from a real Codex 0.160 pane (dogfood D13), path wrapped as shown.
    const CODEX_TRUST: &str = "\
  Folder access
  /private/tmp/scratchpad/cod
  ex owner repo
  Trust this folder? Codex can read, edit, and run files here, subject to your permission settings.
  Folder settings can run code automatically, even without a model request. Continue only if you
  trust these files. Your trust decision will be saved.
› 1. Trust and continue
  2. Back to Agent Command Center
  enter continue · esc back
";

    #[test]
    fn the_codex_trust_prompt_for_the_task_workspace_is_accepted() {
        assert_eq!(
            classify(
                CODEX_TRUST,
                Path::new("/private/tmp/scratchpad/codex owner repo")
            ),
            Screen::Resolve {
                rule: "codex-trust",
                keys: vec!["enter"],
            }
        );
        // The cursor on the other option still ends on "Trust and continue".
        let moved = CODEX_TRUST
            .replace("› 1. Trust and continue", "  1. Trust and continue")
            .replace("  2. Back", "› 2. Back");
        assert_eq!(
            classify(
                &moved,
                Path::new("/private/tmp/scratchpad/codex owner repo")
            ),
            Screen::Resolve {
                rule: "codex-trust",
                keys: vec!["up", "enter"],
            }
        );
    }

    #[test]
    fn the_codex_trust_prompt_for_another_folder_is_not_accepted() {
        assert!(matches!(
            classify(CODEX_TRUST, Path::new("/elsewhere")),
            Screen::Unknown(_)
        ));
    }

    #[test]
    fn a_trust_prompt_for_another_folder_is_not_accepted() {
        let screen =
            "Accessing workspace:\n\n/elsewhere\n\n❯ No, exit\n  Yes, I trust this folder\n";
        assert!(matches!(classify(screen, work()), Screen::Unknown(_)));
    }

    /// Synthetic: modelled on the Codex update offer described in v2.9.3.
    #[test]
    fn an_update_offer_is_skipped_by_moving_to_the_skip_option() {
        let screen = "\
 Update available! 0.1.0 -> 0.2.0

 › 1. Update now
   2. Skip
   3. Skip until next version
";
        assert_eq!(
            classify(screen, work()),
            Screen::Resolve {
                rule: "update-offer-skip",
                keys: vec!["down", "enter"],
            }
        );
    }

    /// Synthetic.
    #[test]
    fn an_update_offer_with_the_cursor_on_skip_only_presses_enter() {
        let screen = "Update available\n  1. Update now\n❯ 2. Not now\n";
        assert_eq!(
            classify(screen, work()),
            Screen::Resolve {
                rule: "update-offer-skip",
                keys: vec!["enter"],
            }
        );
    }

    /// Synthetic.
    #[test]
    fn an_agent_question_about_updating_something_is_not_an_update_offer() {
        for screen in [
            "Update the schema now?\n› 1. Yes\n  2. Later\n",
            "Upgrade the dependencies too?\n› 1. Yes\n  2. Skip\n",
            "> 1. update the config, then skip later steps\n  2. next\n",
        ] {
            assert!(
                matches!(classify(screen, work()), Screen::Unknown(_)),
                "{screen}"
            );
        }
    }

    #[test]
    fn an_update_offer_is_known_by_its_wording_or_version() {
        assert!(is_update_offer("a new version of codex is ready, update?"));
        assert!(is_update_offer("upgrade to v2.0.15"));
        assert!(!is_update_offer("update the 2.0 schema"));
        assert!(!is_update_offer("update readme.md and 1.2 notes"));
    }

    #[test]
    fn an_update_menu_without_a_skip_option_is_not_guessed() {
        let screen = "Update available\n› 1. Update now\n  2. Show release notes\n";
        assert!(matches!(classify(screen, work()), Screen::Unknown(_)));
    }

    /// Synthetic: Claude Code's documented Bypass Permissions warning, not
    /// captured from a pane here.
    #[test]
    fn the_bypass_permissions_warning_is_accepted() {
        let screen = "WARNING: Claude Code running in Bypass Permissions mode\n\nIn Bypass Permissions mode, Claude Code will not ask for your approval.\n\n❯ 1. No, exit\n  2. Yes, I accept\n";
        assert_eq!(
            classify(screen, work()),
            Screen::Resolve {
                rule: "bypass-warning",
                keys: vec!["down", "enter"],
            }
        );
    }

    /// Synthetic: Claude Code's documented new-MCP-server prompt.
    #[test]
    fn a_new_mcp_server_prompt_is_trusted() {
        let screen = "New MCP server found in .mcp.json: docs\n\n❯ 1. Use this and all future MCP servers in this project\n  2. Use this MCP server\n  3. Continue without using this MCP server\n";
        assert_eq!(
            classify(screen, work()),
            Screen::Resolve {
                rule: "mcp-trust",
                keys: vec!["down", "enter"],
            }
        );
    }

    #[test]
    fn a_numbered_claude_trust_menu_is_answered_by_moving_to_yes() {
        let screen = "Accessing workspace:\n\n/repo/task\n\nQuick safety check\n\n❯ 1. Yes, I trust this folder\n  2. No, exit\n";
        assert_eq!(
            classify(screen, work()),
            Screen::Resolve {
                rule: "workspace-trust",
                keys: vec!["enter"],
            }
        );
        let other = screen
            .replace("❯ 1. Yes", "  1. Yes")
            .replace("  2. No, exit", "❯ 2. No, exit");
        assert_eq!(
            classify(&other, work()),
            Screen::Resolve {
                rule: "workspace-trust",
                keys: vec!["up", "enter"],
            }
        );
    }

    #[test]
    fn the_codex_trust_prompt_for_a_longer_path_is_not_accepted() {
        for longer in [
            "/private/tmp/scratchpad/codex owner repo-evil",
            "/private/tmp/scratchpad/codex owner repo/sub",
        ] {
            let screen =
                CODEX_TRUST.replace("/private/tmp/scratchpad/cod\n  ex owner repo", longer);
            assert!(
                matches!(
                    classify(
                        &screen,
                        Path::new("/private/tmp/scratchpad/codex owner repo")
                    ),
                    Screen::Unknown(_)
                ),
                "{longer}"
            );
        }
    }

    #[test]
    fn agent_prose_about_an_update_is_not_the_applied_update_dialog() {
        let prose = "The build finished. Update complete, please restart the dev server.\n";
        assert_eq!(classify(prose, work()), Screen::Clear);
    }

    #[test]
    fn a_screen_that_a_key_does_not_clear_fails_the_run_after_the_budget() {
        let mut watch = ScreenWatch::new("claude");
        for _ in 0..PRESS_BUDGET {
            watch.pressed("continue-notice").unwrap();
        }
        let error = watch.pressed("continue-notice").unwrap_err().to_string();
        assert!(
            error.contains("continue-notice") && error.contains("did not clear"),
            "{error}"
        );
        // A clear screen is progress: the count starts over.
        watch.observe(&Screen::Clear).unwrap();
        watch.pressed("continue-notice").unwrap();
    }

    #[test]
    fn a_new_mcp_server_prompt_prefers_the_single_server_choice() {
        let screen = "New MCP server found in .mcp.json: docs\n\n❯ 1. Use this and all future MCP servers in this project\n  2. Use this MCP server\n  3. Continue without using this MCP server\n";
        assert_eq!(
            classify(screen, work()),
            Screen::Resolve {
                rule: "mcp-trust",
                keys: vec!["down", "enter"],
            }
        );
    }

    /// Synthetic: a stray notice line must not turn an update menu into Enter.
    #[test]
    fn an_update_menu_with_a_continue_line_is_skipped_not_confirmed() {
        let screen = "Update available\n› 1. Update now\n  2. Skip\nPress Enter to continue\n";
        assert_eq!(
            classify(screen, work()),
            Screen::Resolve {
                rule: "update-offer-skip",
                keys: vec!["down", "enter"],
            }
        );
    }

    /// Synthetic.
    #[test]
    fn a_continue_notice_is_dismissed_with_enter() {
        assert_eq!(
            classify(
                "Welcome to the new release\n\n  Press Enter to continue\n",
                work()
            ),
            Screen::Resolve {
                rule: "continue-notice",
                keys: vec!["enter"],
            }
        );
    }

    /// Synthetic.
    #[test]
    fn a_menu_no_rule_covers_is_unknown_and_never_pressed() {
        let screen = "Choose a theme\n› 1. Dark\n  2. Light\n";
        assert_eq!(
            classify(screen, work()),
            Screen::Unknown("Choose a theme | › 1. Dark | 2. Light".to_owned())
        );
    }

    #[test]
    fn an_ordinary_agent_screen_is_clear() {
        assert_eq!(classify("some output\n❯ \n", work()), Screen::Clear);
    }

    fn fake_herdr(screen: &str) -> (tempfile::TempDir, Herdr) {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("herdr");
        std::fs::write(temp.path().join("screen"), screen).unwrap();
        std::fs::write(
            &binary,
            r#"#!/bin/sh
d=$(/usr/bin/dirname "$0")
case "$1 $2" in
 'pane read') /bin/cat "$d/screen";;
 'pane send-keys') echo "$@" >> "$d/keys";;
 *) exit 8;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let herdr = Herdr {
            binary: binary.into_os_string(),
            session: None,
        };
        (temp, herdr)
    }

    fn run_args(temp: &tempfile::TempDir) -> (Paths, PaneRunArgs) {
        let paths = Paths::new(Some(temp.path().join("home"))).unwrap();
        let run = PaneRunArgs {
            caller: None,
            native_executable: None,
            prompt_file: PathBuf::new(),
            workspace: PathBuf::from("/repo/task"),
            task: brgr_protocol::TaskId::new(),
            revision: 1,
            kind: "opencode".to_owned(),
            agent_args: Vec::new(),
            keep_pane: false,
        };
        (paths, run)
    }

    #[test]
    fn resolve_presses_the_rule_keys_on_the_pane() {
        let (temp, herdr) = fake_herdr(OPENCODE_UPDATED);
        let (paths, run) = run_args(&temp);
        let outcome = resolve(&herdr, "w1:p2", &run, &paths, Some("idle")).unwrap();
        assert!(matches!(outcome, Screen::Resolve { .. }));
        let keys = std::fs::read_to_string(temp.path().join("keys")).unwrap();
        assert_eq!(keys.trim(), "pane send-keys w1:p2 esc");
        let log = std::fs::read_to_string(log_path(&paths, &run)).unwrap();
        assert!(
            log.trim_end().ends_with("w1:p2 update-applied esc"),
            "{log}"
        );
    }

    #[test]
    fn resolve_presses_nothing_while_the_agent_is_working() {
        let screen = "agent output\n› 1. Update now\n  2. Skip\n";
        let (temp, herdr) = fake_herdr(screen);
        let (paths, run) = run_args(&temp);
        assert_eq!(
            resolve(&herdr, "w1:p2", &run, &paths, Some("working")).unwrap(),
            Screen::Clear
        );
        assert!(!temp.path().join("keys").exists());
        assert!(!log_path(&paths, &run).exists());
    }

    #[test]
    fn an_unknown_screen_fails_the_run_with_its_text_after_the_deadline() {
        let mut watch = ScreenWatch::with_limit("opencode", Duration::ZERO);
        let error = watch
            .observe(&Screen::Unknown("Choose a theme".to_owned()))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("opencode") && error.contains("Choose a theme"),
            "{error}"
        );
    }

    #[test]
    fn a_resolved_screen_resets_the_deadline() {
        let mut watch = ScreenWatch::with_limit("claude", Duration::from_hours(1));
        watch.observe(&Screen::Unknown("x".to_owned())).unwrap();
        watch.observe(&Screen::Clear).unwrap();
        assert!(watch.since.is_none());
    }
}
