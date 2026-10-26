//! `brgr integrate claude install --context` and `integrate cursor install`,
//! against scratch configs.

use std::{
    fs,
    io::Write as _,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde_json::Value;
use tempfile::TempDir;

const CLAUDE: &str = include_str!("../../../testdata/fixtures/statusline/claude-2.1.293.json");

struct Setup {
    _temp: TempDir,
    /// Deliberately has a space in its name: it ends up in a shell command.
    home: PathBuf,
    config: PathBuf,
}

fn setup() -> Setup {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("brgr home");
    let config = temp.path().join("claude config");
    fs::create_dir_all(&config).unwrap();
    Setup {
        home,
        config,
        _temp: temp,
    }
}

fn settings(setup: &Setup) -> PathBuf {
    setup.config.join("settings.json")
}

fn brgr(setup: &Setup, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_brgr"))
        .arg("--home")
        .arg(&setup.home)
        .arg("--json")
        .args(["integrate", "claude"])
        .args(args)
        .env("CLAUDE_CONFIG_DIR", &setup.config)
        .output()
        .unwrap()
}

fn json(output: &std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn status_line_command(path: &Path) -> String {
    let value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    value["statusLine"]["command"].as_str().unwrap().to_owned()
}

const ORIGINAL: &str = "{\n  \"model\": \"opus\",\n  \"statusLine\": {\n    \"type\": \"command\",\n    \"command\": \"bash CHAIN\"\n  },\n  \"hooks\": {\"Stop\": []}\n}\n";

#[test]
fn install_chains_to_the_existing_status_line_and_uninstall_restores_it_exactly() {
    let setup = setup();
    let chain = setup.config.join("status line.sh");
    let seen = setup.config.join("seen.json");
    fs::write(
        &chain,
        format!("#!/bin/bash\ncat > '{}'\necho chained-ok\n", seen.display()),
    )
    .unwrap();
    // The existing command, with a path that needs quoting in the shell.
    let original = ORIGINAL.replace("bash CHAIN", &format!("bash '{}'", chain.display()));
    fs::write(settings(&setup), &original).unwrap();

    let installed = json(&brgr(&setup, &["install", "--context"]));
    assert_eq!(installed["status_line"]["status"], "installed");
    assert_eq!(installed["status_line"]["chained"], true);
    let command = status_line_command(&settings(&setup));
    assert!(command.contains(" __context -- bash "), "{command}");
    // The backup holds the file as it was.
    let backup = installed["status_line"]["backup"].as_str().unwrap();
    assert_eq!(fs::read_to_string(backup).unwrap(), original);

    // Run the command as Claude Code's shell would: the status line still draws,
    // and brgr recorded the session.
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(CLAUDE.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "chained-ok");
    assert_eq!(fs::read_to_string(&seen).unwrap(), CLAUDE);
    let recorded = setup
        .home
        .join("context/0a91ffd2-1a5e-4c75-b65a-54cc4b3b1fa0.json");
    assert!(recorded.exists());

    let status = json(&brgr(&setup, &["status"]));
    assert_eq!(status["status_line"]["status"], "installed");
    let removed = json(&brgr(&setup, &["uninstall"]));
    assert_eq!(removed["status_line"]["status_line_restored"], true);
    assert_eq!(fs::read_to_string(settings(&setup)).unwrap(), original);
}

#[test]
fn without_an_existing_status_line_install_adds_one_and_uninstall_removes_it() {
    let setup = setup();
    let original = "{\n  \"model\": \"opus\",\n  \"hooks\": {}\n}\n";
    fs::write(settings(&setup), original).unwrap();
    let installed = json(&brgr(&setup, &["install", "--context"]));
    assert_eq!(installed["status_line"]["chained"], false);
    assert!(status_line_command(&settings(&setup)).ends_with(" __context"));
    json(&brgr(&setup, &["uninstall"]));
    assert_eq!(fs::read_to_string(settings(&setup)).unwrap(), original);
}

#[test]
fn a_symlinked_settings_file_is_edited_at_its_target_and_stays_a_link() {
    let setup = setup();
    let managed = setup.config.join("managed");
    fs::create_dir_all(&managed).unwrap();
    let target = managed.join("settings.json");
    fs::write(&target, ORIGINAL.replace("bash CHAIN", "bash plain.sh")).unwrap();
    symlink(&target, settings(&setup)).unwrap();
    let installed = json(&brgr(&setup, &["install", "--context"]));
    assert_eq!(installed["status_line"]["symlink"], true);
    assert!(fs::symlink_metadata(settings(&setup)).unwrap().is_symlink());
    assert!(status_line_command(&target).contains("__context"));
    json(&brgr(&setup, &["uninstall"]));
    assert!(fs::symlink_metadata(settings(&setup)).unwrap().is_symlink());
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        ORIGINAL.replace("bash CHAIN", "bash plain.sh")
    );
}

#[test]
fn a_status_line_with_shell_operators_or_invalid_settings_is_refused_untouched() {
    let setup = setup();
    let piped = ORIGINAL.replace("bash CHAIN", "bash a.sh | head -1");
    fs::write(settings(&setup), &piped).unwrap();
    let refused = brgr(&setup, &["install", "--context"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("shell operators"));
    assert_eq!(fs::read_to_string(settings(&setup)).unwrap(), piped);
    assert!(!setup.home.join("claude-statusline.json").exists());

    fs::write(settings(&setup), "{ not json").unwrap();
    assert!(!brgr(&setup, &["install", "--context"]).status.success());
    assert_eq!(fs::read_to_string(settings(&setup)).unwrap(), "{ not json");
}

#[test]
fn installing_twice_changes_nothing_the_second_time() {
    let setup = setup();
    fs::write(
        settings(&setup),
        ORIGINAL.replace("bash CHAIN", "bash x.sh"),
    )
    .unwrap();
    json(&brgr(&setup, &["install", "--context"]));
    let once = fs::read_to_string(settings(&setup)).unwrap();
    let again = json(&brgr(&setup, &["install", "--context"]));
    assert_eq!(again["status_line"]["status"], "already_installed");
    assert_eq!(fs::read_to_string(settings(&setup)).unwrap(), once);
}

#[test]
fn a_status_line_the_user_changed_since_install_is_left_alone_by_uninstall() {
    let setup = setup();
    fs::write(
        settings(&setup),
        ORIGINAL.replace("bash CHAIN", "bash x.sh"),
    )
    .unwrap();
    json(&brgr(&setup, &["install", "--context"]));
    let mine = ORIGINAL.replace("bash CHAIN", "bash mine.sh");
    fs::write(settings(&setup), &mine).unwrap();
    assert_eq!(
        json(&brgr(&setup, &["status"]))["status_line"]["status"],
        "drifted"
    );
    let removed = json(&brgr(&setup, &["uninstall"]));
    assert_eq!(removed["status_line"]["status_line_restored"], false);
    assert_eq!(fs::read_to_string(settings(&setup)).unwrap(), mine);
}

#[test]
fn a_plain_install_does_not_touch_the_status_line() {
    let setup = setup();
    let original = ORIGINAL.replace("bash CHAIN", "bash x.sh");
    fs::write(settings(&setup), &original).unwrap();
    let installed = json(&brgr(&setup, &["install"]));
    assert!(installed.get("status_line").is_none());
    assert_eq!(fs::read_to_string(settings(&setup)).unwrap(), original);
}

fn cursor(setup: &Setup, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_brgr"))
        .arg("--home")
        .arg(&setup.home)
        .arg("--json")
        .args(["integrate", "cursor"])
        .args(args)
        .env("CURSOR_CONFIG_DIR", &setup.config)
        .output()
        .unwrap()
}

const CURSOR_CONFIG: &str = "{\n  \"permissions\": {\n    \"allow\": [\"Shell(ls)\"],\n    \"deny\": []\n  },\n  \"version\": 1\n}\n";

#[test]
fn cursor_install_adds_a_recorder_tagged_as_cursor_and_uninstall_restores_the_file() {
    let setup = setup();
    let path = setup.config.join("cli-config.json");
    fs::write(&path, CURSOR_CONFIG).unwrap();
    let installed = json(&cursor(&setup, &["install"]));
    assert_eq!(installed["status"], "installed");
    let command = status_line_command(&path);
    assert!(command.ends_with(" __context --source cursor"), "{command}");
    // The other settings are untouched.
    let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(value["permissions"]["allow"][0], "Shell(ls)");
    assert_eq!(json(&cursor(&setup, &["status"]))["status"], "installed");
    json(&cursor(&setup, &["uninstall"]));
    assert_eq!(fs::read_to_string(&path).unwrap(), CURSOR_CONFIG);
}

#[test]
fn cursor_keeps_the_status_line_it_had_and_its_other_fields() {
    let setup = setup();
    let path = setup.config.join("cli-config.json");
    let original = "{\n  \"statusLine\": {\n    \"type\": \"command\",\n    \"command\": \"~/.cursor/statusline.sh\",\n    \"padding\": 2\n  },\n  \"version\": 1\n}\n";
    fs::write(&path, original).unwrap();
    json(&cursor(&setup, &["install"]));
    let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(value["statusLine"]["padding"], 2);
    assert!(
        value["statusLine"]["command"]
            .as_str()
            .unwrap()
            .ends_with(" __context --source cursor -- ~/.cursor/statusline.sh")
    );
    json(&cursor(&setup, &["uninstall"]));
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
}

#[test]
fn a_chained_tilde_path_is_expanded_by_brgr_when_the_harness_does_not() {
    let setup = setup();
    let script = setup.config.join("bin/status.sh");
    fs::create_dir_all(script.parent().unwrap()).unwrap();
    fs::write(&script, "#!/bin/bash\ncat >/dev/null\necho from-home\n").unwrap();
    fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_brgr"))
        .arg("--home")
        .arg(&setup.home)
        .args(["__context", "--source", "cursor", "--", "~/bin/status.sh"])
        .env("HOME", &setup.config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(CLAUDE.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "from-home");
    let signal: Value = serde_json::from_slice(
        &fs::read(
            setup
                .home
                .join("context/0a91ffd2-1a5e-4c75-b65a-54cc4b3b1fa0.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(signal["source"], "cursor");
}
