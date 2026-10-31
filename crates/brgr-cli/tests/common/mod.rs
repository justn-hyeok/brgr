//! Shared fixtures for brgr-cli integration tests.
#![allow(dead_code)]

use std::{
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

use serde_json::Value;

const TRAMPOLINE_SOURCE: &str = concat!(
    "#!/bin/sh\n",
    "[ -f \"$0.body\" ] || exit 0\n",
    "exec /bin/sh \"$0.body\" \"$@\"\n"
);

static WARMED_TRAMPOLINE: OnceLock<PathBuf> = OnceLock::new();

/// Writes a `#!/bin/sh` test executable without macOS's first-exec cost.
///
/// One process-global trampoline under [`std::env::temp_dir`] is made
/// executable and run once. [`write_fast_script`] then hard-links that
/// warmed inode to `path` and writes `body` beside it as `path` with a
/// `.body` suffix (mode `0644`, not executable). The trampoline runs
/// `/bin/sh` on that file, so this is only for a body whose first line is
/// `#!/bin/sh`. If the hard link fails with [`io::ErrorKind::CrossesDevices`],
/// the body is written to `path` with mode `0755` instead. Tests that later
/// overwrite or digest the executable bytes must keep a unique inode (ordinary
/// `fs::write` and `chmod`) so they do not share this trampoline.
///
/// # Panics
///
/// Panics if `body` does not start with `#!/bin/sh`, if `path` has no file
/// name, or if creating, warming, or linking the trampoline fails for a
/// reason other than a cross-device hard link.
pub fn write_fast_script(path: &Path, body: &str) {
    let first_line = body.lines().next();
    assert_eq!(
        first_line,
        Some("#!/bin/sh"),
        "write_fast_script requires a #!/bin/sh first line, got {first_line:?}"
    );
    let body_path = script_body_path(path);
    fs::write(&body_path, body).unwrap();
    fs::set_permissions(&body_path, fs::Permissions::from_mode(0o644)).unwrap();
    if path.exists() {
        fs::remove_file(path).unwrap();
    }
    match fs::hard_link(warmed_trampoline(), path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
            write_direct_script(path, body);
        }
        Err(error) => panic!("hard-linking trampoline to {}: {error}", path.display()),
    }
}

fn script_body_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .expect("write_fast_script path has a file name")
        .to_os_string();
    name.push(".body");
    path.with_file_name(name)
}

fn write_direct_script(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn warmed_trampoline() -> &'static Path {
    WARMED_TRAMPOLINE.get_or_init(create_warmed_trampoline)
}

fn create_warmed_trampoline() -> PathBuf {
    let dir = std::env::temp_dir().join("brgr-fast-script");
    fs::create_dir_all(&dir).unwrap();
    let trampoline = dir.join(format!("trampoline-{}", std::process::id()));
    if trampoline.exists() {
        fs::remove_file(&trampoline).unwrap();
    }
    fs::write(&trampoline, TRAMPOLINE_SOURCE).unwrap();
    fs::set_permissions(&trampoline, fs::Permissions::from_mode(0o755)).unwrap();
    let status = Command::new(&trampoline)
        .arg("--warm")
        .status()
        .expect("run trampoline --warm");
    assert!(status.success(), "trampoline --warm failed: {status}");
    // Drop write bits so a later `fs::write` through a hard link cannot
    // clobber every trampoline in this process.
    fs::set_permissions(&trampoline, fs::Permissions::from_mode(0o555)).unwrap();
    trampoline
}

/// A Herdr that records every call and answers the few pane-mode and handoff
/// tests make. `status-<pane>` files choose a pane's agent state (default
/// `idle`). `list.json` and per-pane screen files, when present, feed
/// `agent list` and `pane read`.
pub fn fake_herdr(dir: &Path) -> std::path::PathBuf {
    let script = dir.join("herdr");
    write_fast_script(
        &script,
        r#"#!/bin/sh
d="${FAKE_HERDR_DIR:-$(/usr/bin/dirname "$0")}"
echo "$@" >> "$d/calls.log"
if [ "$1" = "--session" ]; then shift 2; fi
case "$1 $2" in
  "pane split")
    echo '{"result":{"pane":{"pane_id":"p-new"}}}' ;;
  "pane get")
    echo "{\"result\":{\"pane\":{\"terminal_id\":\"term-$3\"}}}" ;;
  "agent list")
    if [ -f "$d/list.json" ]; then /bin/cat "$d/list.json"
    else echo '{"result":{"agents":[]}}'; fi ;;
  "agent get")
    if [ -f "$d/missing-$3" ]; then
      echo '{"error":{"code":"pane_not_found"}}' >&2
      exit 1
    fi
    s=idle
    [ -f "$d/status-$3" ] && s=$(/bin/cat "$d/status-$3")
    kind=claude
    [ -f "$d/kind-$3" ] && kind=$(/bin/cat "$d/kind-$3")
    echo "{\"result\":{\"agent\":{\"agent\":\"$kind\",\"pane_id\":\"$3\",\"agent_status\":\"$s\"}}}" ;;
  "pane layout") exit 1 ;;
  "pane read")
    f=$(printf %s "$3" | /usr/bin/sed 's/:/_/')
    if [ -f "$d/$f" ]; then /bin/cat "$d/$f"; fi ;;
  *) ;;
esac
exit 0
"#,
    );
    script
}

pub fn herdr_env<'a>(fake: &'a Path, dir: &'a Path, pane: &'a str) -> Vec<(&'static str, String)> {
    vec![
        ("HERDR_ENV", "1".to_owned()),
        ("HERDR_PANE_ID", pane.to_owned()),
        ("HERDR_BIN_PATH", fake.to_str().unwrap().to_owned()),
        ("FAKE_HERDR_DIR", dir.to_str().unwrap().to_owned()),
    ]
}

pub fn calls(dir: &Path) -> String {
    fs::read_to_string(dir.join("calls.log")).unwrap_or_default()
}

/// A worker's own identity. `cargo test` inside a worker inherits it, and a
/// fixture command must not: a stale parent attempt or a session that
/// disagrees with `CODEX_THREAD_ID` fails the command before it runs.
pub const INHERITED_WORKER_ENV: &[&str] = &[
    "BRGR_HOME",
    "BRGR_BIN",
    "BRGR_OWNER_ID",
    "BRGR_SESSION_ID",
    "BRGR_PARENT_TASK_ID",
    "BRGR_PARENT_ATTEMPT_ID",
    "BRGR_WORKER_HERDR_CONTEXT",
    "CODEX_THREAD_ID",
    "CLAUDE_CODE_SESSION_ID",
    "CURSOR_CONVERSATION_ID",
];

/// The brgr binary under test, with the worker identity removed. Debug builds
/// trust a faked `HERDR_PANE_ID` only with `BRGR_TEST_TRUST_HERDR_PANE`.
pub fn brgr_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_brgr"));
    command.env("BRGR_TEST_TRUST_HERDR_PANE", "1");
    for name in INHERITED_WORKER_ENV {
        command.env_remove(name);
    }
    command
}

/// Runs `brgr --home HOME ARGS` with a clean Herdr/worker environment and a
/// default `BRGR_SESSION_ID`. `--json` is part of `args` so a test can compare
/// the compact, pretty, and human forms of one command. `run`/`revise` get
/// `--headless` unless the test opts into pane fixtures.
pub fn run(home: &Path, args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
    let mut command = brgr_command();
    command.arg("--home").arg(home).args(args);
    let subcommand = args.iter().find(|arg| !arg.starts_with('-'));
    if matches!(subcommand.copied(), Some("run" | "revise")) {
        command.arg("--headless");
    }
    for name in [
        "HERDR_ENV",
        "HERDR_PANE_ID",
        "HERDR_TAB_ID",
        "HERDR_WORKSPACE_ID",
        "HERDR_BIN_PATH",
        "HERDR_SOCKET_PATH",
        "HERDR_SESSION",
        "HERDR_PLUGIN_ID",
    ] {
        command.env_remove(name);
    }
    if !envs
        .iter()
        .any(|(name, _)| matches!(*name, "CLAUDE_CODE_SESSION_ID" | "BRGR_SESSION_ID"))
    {
        command.env("BRGR_SESSION_ID", "fixture-session");
    }
    for (name, value) in envs {
        command.env(name, value);
    }
    command.output().unwrap()
}

/// Parses a successful command's stdout as JSON, failing with stderr shown.
pub fn json_output(output: &std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// The gjc fixture harness every contract test registers.
pub fn gjc_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap()
}

/// Certifies the gjc fixture against `scratch` so `local.gjc` can be run.
pub fn add_fixture(home: &Path, fixture: &Path, scratch: &Path) {
    fs::create_dir_all(scratch).unwrap();
    let receipt = json_output(&run(
        home,
        &[
            "--json",
            "harness",
            "add",
            fixture.to_str().unwrap(),
            "--workspace",
            scratch.to_str().unwrap(),
            "--prompt",
            "BRGR_FIXTURE_OK",
        ],
        &[],
    ));
    assert!(receipt["scratch_result_digest"].as_str().is_some());
}
