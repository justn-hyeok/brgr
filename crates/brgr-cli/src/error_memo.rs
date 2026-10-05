//! Tracks the failures brgr sees.
//!
//! Every failed or lost run is folded into a ledger by fingerprint and written
//! out as `brgr_error_issue_memo.md`. When issue reporting is switched on, one
//! GitHub issue is filed per distinct failure and later sightings only bump its
//! count. Issue text carries the redacted error class and counts, never a
//! task's objective, prompt, report or files.

use std::{
    collections::BTreeSet,
    fmt::Write as _,
    fs::{self, OpenOptions},
    io::Write as _,
    os::unix::fs::OpenOptionsExt as _,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{Paths, config::Config};

/// The pane adapter's progress line, which older runs left as the error's tail.
const PROGRESS_MARK: &str = "brgr pane mode";
/// An unknown-screen failure ends with a quote of what the pane showed, which
/// can hold anything the worker displayed. The text before the quote names the
/// failure; the quote never leaves the machine.
const SCREEN_MARK: &str = "no brgr rule answers";
/// Applies every cut that keeps free text out of an error class or sample.
fn without_free_text(error: &str) -> &str {
    let error = error.split(PROGRESS_MARK).next().unwrap_or(error);
    match error.find(SCREEN_MARK) {
        Some(at) => {
            let rest = &error[at..];
            let end = rest.find(": ").unwrap_or(rest.len());
            &error[..at + end]
        }
        None => error,
    }
}
const LEDGER: &str = "error-ledger.json";
const MEMO: &str = "brgr_error_issue_memo.md";
const SAMPLE_LIMIT: usize = 300;
const CLASS_LIMIT: usize = 140;
/// Counts at which an already filed issue gets a short "seen again" comment.
const COMMENT_AT: [u64; 3] = [5, 25, 100];

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct Ledger {
    entries: Vec<Entry>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct Entry {
    fingerprint: String,
    class: String,
    count: u64,
    first_seen: u64,
    last_seen: u64,
    outcomes: BTreeSet<String>,
    harnesses: BTreeSet<String>,
    versions: BTreeSet<String>,
    sample: String,
    issue_url: Option<String>,
    commented_at: u64,
}

/// What one failure looked like when it happened.
pub(crate) struct Failure<'a> {
    pub(crate) outcome: &'a str,
    pub(crate) harness: &'a str,
    pub(crate) error: &'a str,
}

/// Records a sealed failed or lost result; other outcomes are not failures.
pub(crate) fn record_result(paths: &Paths, harness: &str, result: &brgr_protocol::ResultEnvelope) {
    use brgr_protocol::TerminalOutcome;
    let outcome = match result.outcome {
        TerminalOutcome::Lost => "lost",
        TerminalOutcome::Failed => "failed",
        TerminalOutcome::Candidate | TerminalOutcome::Cancelled => return,
    };
    record(
        paths,
        &Failure {
            outcome,
            harness,
            error: result.error.as_deref().unwrap_or("no error text"),
        },
    );
}

/// Records a failure and, when reporting is on, starts the issue filer.
/// Never fails the caller: a broken ledger must not break a run.
pub(crate) fn record(paths: &Paths, failure: &Failure<'_>) {
    if let Err(error) = record_inner(paths, failure) {
        eprintln!("brgr error memo could not be updated: {error}");
    }
}

fn record_inner(paths: &Paths, failure: &Failure<'_>) -> Result<()> {
    let class = classify(failure.error);
    let fingerprint = fingerprint(&class);
    let now = now();
    let version = env!("CARGO_PKG_VERSION").to_owned();
    {
        let _lock = lock(paths)?;
        let mut ledger = load(paths);
        let position = ledger
            .entries
            .iter()
            .position(|entry| entry.fingerprint == fingerprint);
        let entry = if let Some(position) = position {
            &mut ledger.entries[position]
        } else {
            ledger.entries.push(Entry {
                fingerprint,
                class,
                first_seen: now,
                sample: redact(failure.error, SAMPLE_LIMIT),
                ..Entry::default()
            });
            ledger.entries.last_mut().context("ledger is empty")?
        };
        entry.count += 1;
        entry.last_seen = now;
        entry.outcomes.insert(failure.outcome.to_owned());
        entry.harnesses.insert(failure.harness.to_owned());
        entry.versions.insert(version);
        save(paths, &ledger)?;
        fs::write(paths.home.join(MEMO), memo(&ledger))?;
    }
    if Config::load(&paths.config)?.issues.auto_file {
        spawn_filer(paths);
    }
    Ok(())
}

pub(crate) fn run(paths: &Paths, command: Option<&crate::cli::ErrorsCommand>) -> Result<()> {
    use crate::cli::ErrorsCommand;
    match command.unwrap_or(&ErrorsCommand::List) {
        ErrorsCommand::List => show(paths, false),
        ErrorsCommand::Preview => show(paths, true),
        ErrorsCommand::File => file_issues(paths),
    }
}

/// Prints the ledger, or with `preview` the exact issue text that would be sent.
pub(crate) fn show(paths: &Paths, preview: bool) -> Result<()> {
    let ledger = load(paths);
    if !preview {
        println!("{}", serde_json::to_string_pretty(&ledger.entries)?);
        return Ok(());
    }
    for entry in &ledger.entries {
        let (title, body) = issue_text(entry);
        println!("{title}\n\n{body}\n---");
    }
    Ok(())
}

/// Files an issue for each failure that reached the configured count, and
/// comments on a filed one at a few milestones. A no-op when reporting is off.
pub(crate) fn file_issues(paths: &Paths) -> Result<()> {
    let config = Config::load(&paths.config)?;
    if !config.issues.auto_file {
        return Ok(());
    }
    let repo = config
        .issues
        .repo
        .clone()
        .context("issue reporting has no repository; run `brgr config set-issue-reporting`")?;
    let guard = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(paths.home.join("error-report.lock"))?;
    if guard.try_lock().is_err() {
        return Ok(());
    }
    let mut ledger = load(paths);
    let mut first_error = None;
    for entry in &mut ledger.entries {
        let step = if entry.issue_url.is_none() && entry.count >= config.issues.min_count {
            find_or_create(&repo, entry).map(|url| {
                entry.issue_url = Some(url);
                entry.commented_at = entry.count;
            })
        } else if let Some(url) = entry.issue_url.clone()
            && COMMENT_AT
                .iter()
                .any(|at| entry.count >= *at && entry.commented_at < *at)
        {
            comment(&repo, &url, entry).map(|()| entry.commented_at = entry.count)
        } else {
            Ok(())
        };
        // One failing call must not drop the issue addresses already found.
        if let Err(error) = step {
            first_error.get_or_insert(error);
        }
    }
    // Merge counts recorded while the filer ran instead of overwriting them.
    let _lock = lock(paths)?;
    let mut latest = load(paths);
    for entry in &ledger.entries {
        if let Some(current) = latest
            .entries
            .iter_mut()
            .find(|item| item.fingerprint == entry.fingerprint)
        {
            current.issue_url.clone_from(&entry.issue_url);
            current.commented_at = entry.commented_at;
        }
    }
    save(paths, &latest)?;
    fs::write(paths.home.join(MEMO), memo(&latest))?;
    first_error.map_or(Ok(()), Err)
}

fn find_or_create(repo: &str, entry: &Entry) -> Result<String> {
    let marker = marker(&entry.fingerprint);
    let found = gh(
        &[
            "issue",
            "list",
            "--repo",
            repo,
            "--state",
            "all",
            "--search",
            &format!("{marker} in:body"),
            "--json",
            "url",
            "--limit",
            "1",
        ],
        None,
    )?;
    if let Some(url) = serde_json::from_str::<serde_json::Value>(&found)
        .ok()
        .and_then(|value| value.pointer("/0/url")?.as_str().map(str::to_owned))
    {
        return Ok(url);
    }
    let (title, body) = issue_text(entry);
    let created = gh(
        &[
            "issue",
            "create",
            "--repo",
            repo,
            "--title",
            &title,
            "--body-file",
            "-",
        ],
        Some(&body),
    )?;
    created
        .lines()
        .rev()
        .find(|line| line.starts_with("https://"))
        .map(str::to_owned)
        .context("gh did not print the new issue address")
}

fn comment(repo: &str, url: &str, entry: &Entry) -> Result<()> {
    let body = format!(
        "Seen again: {} times in total, last on {}.",
        entry.count,
        date(entry.last_seen)
    );
    gh(
        &["issue", "comment", url, "--repo", repo, "--body-file", "-"],
        Some(&body),
    )?;
    Ok(())
}

#[cfg(test)]
thread_local! {
    static GH_PROGRAM: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

fn gh_program() -> std::path::PathBuf {
    #[cfg(test)]
    if let Some(path) = GH_PROGRAM.with(|program| program.borrow().clone()) {
        return path;
    }
    std::path::PathBuf::from("gh")
}

/// Runs `gh` as an argv array, never through a shell.
fn gh(args: &[&str], stdin: Option<&str>) -> Result<String> {
    let mut command = Command::new(gh_program());
    command
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("gh could not start")?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(text.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "gh failed: {}",
            redact(&String::from_utf8_lossy(&output.stderr), SAMPLE_LIMIT)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn spawn_filer(paths: &Paths) {
    use std::os::unix::process::CommandExt as _;
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let _ = Command::new(executable)
        .arg("--home")
        .arg(&paths.home)
        .args(["errors", "file"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
}

fn marker(fingerprint: &str) -> String {
    format!("brgr-fp:{fingerprint}")
}

fn issue_text(entry: &Entry) -> (String, String) {
    let title = format!("[brgr auto] {}", title_of(&entry.class));
    let mut body = String::new();
    let _ = write!(
        body,
        "Filed automatically by brgr {}.\n\n\
         **Failure:** `{}`\n\
         **Outcome:** {} · **Harness:** {}\n\
         **Seen:** {} time(s), {} to {}\n\n\
         Redacted sample:\n\n```\n{}\n```\n\n\
         This text holds only the error above, with home paths, ids, addresses and \
         tokens masked. It holds no task objective, prompt, report or file content.\n\n\
         {}",
        entry
            .versions
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", "),
        redact(&entry.class, 300),
        join(&entry.outcomes),
        join(&entry.harnesses),
        entry.count,
        date(entry.first_seen),
        date(entry.last_seen),
        entry.sample,
        marker(&entry.fingerprint),
    );
    (title, body)
}

/// The class cut at a word boundary, so a long one does not end mid-word.
fn title_of(class: &str) -> String {
    let text = redact(class, 200);
    if text.chars().count() <= 80 {
        return text;
    }
    let cut: String = text.chars().take(80).collect();
    let words = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
    format!("{words}…")
}

fn memo(ledger: &Ledger) -> String {
    let mut out = String::from(
        "# brgr error and issue memo\n\n\
         Generated by brgr from `error-ledger.json`; edits are overwritten. Each failed or lost \
         run is folded into one entry by fingerprint. Entries hold redacted error text only.\n\n",
    );
    let mut entries: Vec<&Entry> = ledger.entries.iter().collect();
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.last_seen));
    if entries.is_empty() {
        out.push_str("No failures recorded yet.\n");
    }
    for entry in entries {
        let _ = write!(
            out,
            "## `{}` · {}\n\n\
             - Seen: {} time(s), {} to {}\n\
             - Outcome: {} · Harness: {} · brgr: {}\n\
             - Issue: {}\n\n\
             ```\n{}\n```\n\n",
            entry.fingerprint,
            entry.class,
            entry.count,
            date(entry.first_seen),
            date(entry.last_seen),
            join(&entry.outcomes),
            join(&entry.harnesses),
            join(&entry.versions),
            entry.issue_url.as_deref().unwrap_or("not filed"),
            entry.sample,
        );
    }
    out
}

fn join(values: &BTreeSet<String>) -> String {
    values.iter().cloned().collect::<Vec<_>>().join(", ")
}

/// The error with everything that varies per run removed, so the same failure
/// on another task or machine lands on one entry.
fn classify(error: &str) -> String {
    // A screen quote after " | " differs on every run of the same failure.
    // Older runs also ended with the adapter's progress line, which quoted the
    // task's objective; it is never part of an error class.
    let head = error.split(" | ").next().unwrap_or(error);
    let head = without_free_text(head);
    let mut out = Vec::new();
    for token in head.split_whitespace() {
        out.push(normalize_token(token));
    }
    let text = out.join(" ");
    text.chars().take(CLASS_LIMIT).collect()
}

fn normalize_token(token: &str) -> String {
    let core = token.trim_matches(|c: char| ",.;:'\"()[]{}<>".contains(c));
    let replacement = if is_uuid(core) {
        Some("<id>")
    } else if core.starts_with('/') || core.starts_with("~/") {
        Some("<path>")
    } else if is_pane_id(core) {
        Some("<pane>")
    } else if core.len() >= 12 && core.chars().all(|c| c.is_ascii_hexdigit()) {
        Some("<hex>")
    } else if !core.is_empty() && core.chars().all(|c| c.is_ascii_digit()) {
        Some("<n>")
    } else {
        None
    };
    if let Some(new) = replacement {
        return token.replace(core, new);
    }
    // A count with a short unit, such as `180s` or `3m`.
    let digits = core.chars().take_while(char::is_ascii_digit).count();
    let unit = &core[digits..];
    if digits > 0 && unit.len() <= 2 && unit.chars().all(|c| c.is_ascii_alphabetic()) {
        return token.replace(core, &format!("<n>{unit}"));
    }
    token.to_owned()
}

fn is_uuid(text: &str) -> bool {
    let parts: Vec<&str> = text.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(len, part)| part.len() == *len && part.chars().all(|c| c.is_ascii_hexdigit()))
}

fn is_pane_id(text: &str) -> bool {
    text.split_once(":p").is_some_and(|(workspace, pane)| {
        workspace.starts_with('w')
            && workspace.len() <= 4
            && !pane.is_empty()
            && pane.len() <= 4
            && pane.chars().all(|c| c.is_ascii_alphanumeric())
    })
}

/// Masks what must not leave the machine: home directories, ids, addresses,
/// tokens, then cuts the text to `limit` characters. Each word is also split at
/// `=`, `,`, `;`, quotes and brackets, so `key=ghp_...` or `(bob@example.com)`
/// is caught inside a longer token.
pub(crate) fn redact(text: &str, limit: usize) -> String {
    let text = without_free_text(text);
    let home = std::env::var("HOME").unwrap_or_default();
    let mut words = Vec::new();
    for word in text.split_whitespace() {
        let word = if home.len() > 1 {
            word.replace(&home, "~")
        } else {
            word.to_owned()
        };
        words.push(mask_pieces(&word));
    }
    words.join(" ").chars().take(limit).collect()
}

fn mask_pieces(word: &str) -> String {
    const DELIMITERS: &str = "=,;:'\"()[]{}<>|";
    let mut out = String::new();
    let mut piece = String::new();
    for character in word.chars() {
        if DELIMITERS.contains(character) {
            out.push_str(&mask_piece(&piece));
            piece.clear();
            out.push(character);
        } else {
            piece.push(character);
        }
    }
    out.push_str(&mask_piece(&piece));
    out
}

fn mask_piece(piece: &str) -> String {
    let core = piece.trim_matches(|c: char| ".:".contains(c));
    if core.is_empty() {
        return piece.to_owned();
    }
    let replacement = if is_secret(core) {
        "<secret>".to_owned()
    } else if core.contains('@') && core.contains('.') {
        "<email>".to_owned()
    } else if is_uuid(core) {
        "<id>".to_owned()
    } else if let Some(masked) = mask_home(core) {
        masked
    } else {
        return piece.to_owned();
    };
    piece.replace(core, &replacement)
}

/// `/Users/<name>/...` and `/home/<name>/...` with the name masked.
fn mask_home(path: &str) -> Option<String> {
    for root in ["/Users/", "/home/"] {
        if let Some(rest) = path.strip_prefix(root) {
            let after = rest.split_once('/').map_or("", |(_, tail)| tail);
            return Some(if after.is_empty() {
                format!("{root}<user>")
            } else {
                format!("{root}<user>/{after}")
            });
        }
    }
    None
}

fn is_secret(token: &str) -> bool {
    [
        "ghp_",
        "gho_",
        "ghs_",
        "github_pat_",
        "sk-",
        "xoxb-",
        "xoxp-",
        "AKIA",
        "eyJ",
    ]
    .iter()
    .any(|prefix| token.starts_with(prefix))
        || token.len() >= 32 && token.chars().all(|c| c.is_ascii_alphanumeric()) && {
            let digits = token.chars().filter(char::is_ascii_digit).count();
            digits > 0 && digits < token.len()
        }
}

fn fingerprint(class: &str) -> String {
    let digest = Sha256::digest(class.as_bytes());
    digest.iter().take(6).fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// `YYYY-MM-DD` (UTC) from a unix time.
fn date(seconds: u64) -> String {
    let days = i64::try_from(seconds / 86_400).unwrap_or(0);
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = if month_part < 10 {
        month_part + 3
    } else {
        month_part - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

fn load(paths: &Paths) -> Ledger {
    fs::read(paths.home.join(LEDGER))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save(paths: &Paths, ledger: &Ledger) -> Result<()> {
    crate::write_json_atomic(&paths.home.join(LEDGER), ledger)
}

fn lock(paths: &Paths) -> Result<fs::File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(paths.home.join("error-ledger.lock"))?;
    file.lock()?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_failure_on_another_task_has_one_fingerprint() {
        let a = classify(
            "the claude agent stayed on a screen no brgr rule answers for 180s: exec '/Users/a/x' | 915c0be5 on main",
        );
        let b = classify(
            "the claude agent stayed on a screen no brgr rule answers for 90s: exec '/Users/b/y' | 22aa11bb on dev",
        );
        assert_eq!(fingerprint(&a), fingerprint(&b));
        // The quoted screen is cut: it is free text from the pane.
        assert_eq!(
            a,
            "the claude agent stayed on a screen no brgr rule answers for <n>s"
        );
    }

    #[test]
    fn ids_panes_and_paths_do_not_split_one_failure() {
        let a = classify(
            "the Herdr-backed worker did not provide a valid final result: pane w5G:p3C not found",
        );
        let b = classify(
            "the Herdr-backed worker did not provide a valid final result: pane w1:p2Z not found",
        );
        assert_eq!(a, b);
        assert_ne!(
            fingerprint(&a),
            fingerprint(&classify("original runner state could not be established"))
        );
    }

    #[test]
    fn redaction_masks_homes_ids_addresses_and_tokens() {
        let text = "failed in /Users/alice/dev/brgr task 38d33b4e-7368-4f7a-a050-822378f2b12a for someone@example.com with ghp_abcdefghijklmnopqrstuvwxyz0123456789 done";
        let out = redact(text, 400);
        assert!(!out.contains("alice"), "{out}");
        assert!(!out.contains("someone@"), "{out}");
        assert!(!out.contains("ghp_"), "{out}");
        assert!(!out.contains("38d33b4e"), "{out}");
        assert!(out.contains("<id>") && out.contains("<email>") && out.contains("<secret>"));
        // Another user's home is masked even when it is not this machine's.
        let other = redact("failed in /Users/someone/dev/x", 100);
        assert_eq!(other, "failed in /Users/<user>/dev/x");
    }

    #[test]
    fn the_adapter_progress_line_never_reaches_a_class_or_a_sample() {
        let error = "the Herdr-backed worker did not provide a valid final result: brgr pane mode · prompted: Rotate the production API key for acme";
        assert!(!classify(error).contains("Rotate"));
        assert!(!redact(error, 300).contains("Rotate"));
    }

    #[test]
    fn a_quoted_pane_screen_never_reaches_a_class_a_sample_or_an_issue() {
        let error = "the claude agent stayed on a screen no brgr rule answers for 180s: Rotate the production key | export TOKEN=abc | more";
        let class = classify(error);
        assert_eq!(
            class,
            "the claude agent stayed on a screen no brgr rule answers for <n>s"
        );
        let sample = redact(error, 300);
        assert!(
            !sample.contains("Rotate") && !sample.contains("TOKEN"),
            "{sample}"
        );
        let entry = Entry {
            fingerprint: "abc".to_owned(),
            class,
            sample,
            count: 1,
            ..Entry::default()
        };
        let (title, body) = issue_text(&entry);
        assert!(!title.contains("Rotate") && !body.contains("Rotate") && !body.contains("TOKEN"));
    }

    #[test]
    fn a_long_issue_title_is_cut_at_a_word_boundary() {
        let class = "the Herdr-backed worker did not provide a valid final result: task pane no longer exists";
        let title = title_of(class);
        assert!(
            title.ends_with('…') && title.chars().count() <= 81,
            "{title}"
        );
        assert!(class.starts_with(title.trim_end_matches('…')), "{title}");
        assert!(!title.contains("longe"), "{title}");
        assert_eq!(title_of("short class"), "short class");
    }

    #[test]
    fn secrets_addresses_and_homes_inside_a_longer_token_are_masked() {
        let error = "Herdr could not open a pane: BRGR_HOME=/Users/alice/Library/brgr contact=bob@example.com key=ghp_abcdefghijklmnopqrstuvwxyz0123456789 (path:/home/carol/x)";
        let out = redact(error, 400);
        for leaked in ["alice", "bob@", "ghp_", "carol"] {
            assert!(!out.contains(leaked), "{leaked} leaked: {out}");
        }
        let entry = Entry {
            fingerprint: "abc".to_owned(),
            class: error.to_owned(),
            sample: out,
            count: 1,
            ..Entry::default()
        };
        let (title, body) = issue_text(&entry);
        for leaked in ["alice", "bob@", "ghp_", "carol"] {
            assert!(
                !title.contains(leaked) && !body.contains(leaked),
                "{leaked} leaked"
            );
        }
    }

    #[test]
    fn redaction_truncates_long_text() {
        assert_eq!(redact(&"word ".repeat(200), 50).chars().count(), 50);
    }

    #[test]
    fn dates_are_utc_calendar_days() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_790_908_872), "2026-10-02");
    }

    #[test]
    fn the_issue_text_has_a_dedupe_marker_and_no_task_content() {
        let entry = Entry {
            fingerprint: "abc123def456".to_owned(),
            class: "agent finished without writing its report to <path>".to_owned(),
            count: 3,
            first_seen: 0,
            last_seen: 86_400,
            sample: "agent finished without writing its report to <path>".to_owned(),
            ..Entry::default()
        };
        let (title, body) = issue_text(&entry);
        assert!(title.starts_with("[brgr auto] "));
        assert!(body.contains("brgr-fp:abc123def456"));
        assert!(body.contains("3 time(s), 1970-01-01 to 1970-01-02"));
        assert!(body.contains("no task objective, prompt, report or file content"));
    }

    #[test]
    fn a_failure_is_filed_once_at_the_threshold_and_not_filed_again() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(temp.path().join("home"))).unwrap();
        let gh = temp.path().join("gh");
        fs::write(
            &gh,
            "#!/bin/sh\necho \"$@\" >> \"$0.calls\"\ncase \"$2\" in\n list) echo '[]';;\n create) /bin/cat >> \"$0.body\"; echo https://github.com/o/r/issues/7;;\nesac\n",
        )
        .unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o700)).unwrap();
        GH_PROGRAM.with(|program| *program.borrow_mut() = Some(gh.clone()));
        let mut config = Config::load(&paths.config).unwrap();
        config.issues.auto_file = true;
        config.issues.repo = Some("o/r".to_owned());
        config.issues.min_count = 2;
        config.save(&paths.config).unwrap();
        let failure = Failure {
            outcome: "lost",
            harness: "local.opencode",
            error: "the agent finished without writing its report to /Users/alice/x/report.md",
        };
        record(&paths, &failure);
        file_issues(&paths).unwrap();
        assert!(!gh.with_extension("calls").exists() && !temp.path().join("gh.calls").exists());
        record(&paths, &failure);
        file_issues(&paths).unwrap();
        file_issues(&paths).unwrap();
        let calls = fs::read_to_string(temp.path().join("gh.calls")).unwrap();
        assert_eq!(calls.matches("issue create").count(), 1, "{calls}");
        let body = fs::read_to_string(temp.path().join("gh.body")).unwrap();
        assert!(
            body.contains("brgr-fp:") && !body.contains("alice"),
            "{body}"
        );
        let ledger = load(&paths);
        assert_eq!(
            ledger.entries[0].issue_url.as_deref(),
            Some("https://github.com/o/r/issues/7")
        );
        let memo = fs::read_to_string(paths.home.join(MEMO)).unwrap();
        assert!(memo.contains("issues/7"), "{memo}");
    }

    #[test]
    fn one_failing_gh_call_keeps_the_issue_addresses_already_found() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(temp.path().join("home"))).unwrap();
        let gh = temp.path().join("gh");
        // The first create succeeds; every later one fails.
        fs::write(
            &gh,
            "#!/bin/sh\ncase \"$2\" in\n list) echo '[]';;\n create) if [ -e \"$0.made\" ]; then echo boom >&2; exit 1; fi; : > \"$0.made\"; /bin/cat >/dev/null; echo https://github.com/o/r/issues/1;;\nesac\n",
        )
        .unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o700)).unwrap();
        GH_PROGRAM.with(|program| *program.borrow_mut() = Some(gh));
        let mut config = Config::load(&paths.config).unwrap();
        config.issues.auto_file = true;
        config.issues.repo = Some("o/r".to_owned());
        config.issues.min_count = 1;
        config.save(&paths.config).unwrap();
        for error in ["first distinct failure", "second distinct failure"] {
            record(
                &paths,
                &Failure {
                    outcome: "failed",
                    harness: "local.x",
                    error,
                },
            );
        }
        assert!(file_issues(&paths).is_err());
        let urls: Vec<_> = load(&paths)
            .entries
            .iter()
            .filter_map(|entry| entry.issue_url.clone())
            .collect();
        assert_eq!(urls, ["https://github.com/o/r/issues/1"]);
    }

    #[test]
    fn recording_twice_keeps_one_entry_and_writes_the_memo() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(temp.path().join("home"))).unwrap();
        for _ in 0..2 {
            record(
                &paths,
                &Failure {
                    outcome: "lost",
                    harness: "local.claude-code",
                    error: "pane w1:p2 not found",
                },
            );
        }
        let ledger = load(&paths);
        assert_eq!(ledger.entries.len(), 1);
        assert_eq!(ledger.entries[0].count, 2);
        let memo = fs::read_to_string(paths.home.join(MEMO)).unwrap();
        assert!(
            memo.contains("2 time(s)") && memo.contains("not filed"),
            "{memo}"
        );
    }
}
