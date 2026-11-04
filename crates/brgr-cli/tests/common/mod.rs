//! Shared fixtures for brgr-cli integration tests.
#![allow(dead_code)]

use std::{
    env, fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;

const TRAMPOLINE_SOURCE: &str = concat!(
    "#!/bin/sh\n",
    "[ -f \"$0.body\" ] || exit 0\n",
    "exec /bin/sh \"$0.body\" \"$@\"\n"
);

static WARMED_TRAMPOLINE: OnceLock<PathBuf> = OnceLock::new();
static HERDR_STUB_DIR: OnceLock<PathBuf> = OnceLock::new();

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

/// Runs a just-written trampoline once. On Linux another thread's `fork` can
/// hold the file open for writing until its child execs, so the first run can
/// fail with `ETXTBSY` for a moment. It is retried only for that, with a hang
/// guard a correct run does not reach.
pub fn warm_executable(path: &Path) {
    let started = std::time::Instant::now();
    loop {
        match Command::new(path).arg("--warm").status() {
            Ok(status) => {
                assert!(status.success(), "trampoline --warm failed: {status}");
                return;
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && started.elapsed() < std::time::Duration::from_mins(1) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => panic!("run trampoline --warm: {error}"),
        }
    }
}

/// Runs a command on a file this process just wrote, waiting out the moment a
/// `fork` in another test thread can still hold it open for writing on Linux.
/// The guard only stops a hang: a correct run does not reach it.
pub fn output_when_not_busy(command: &mut Command) -> std::process::Output {
    let started = std::time::Instant::now();
    loop {
        match command.output() {
            Err(error)
                if error.kind() == io::ErrorKind::ExecutableFileBusy
                    && started.elapsed() < HANG_GUARD =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            other => return other.unwrap(),
        }
    }
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
    warm_executable(&trampoline);
    // Drop write bits so a later `fs::write` through a hard link cannot
    // clobber every trampoline in this process.
    fs::set_permissions(&trampoline, fs::Permissions::from_mode(0o555)).unwrap();
    trampoline
}

/// How long a test waits for an event before it gives up.
///
/// This is a hang guard, not a measurement: a correct run reaches its event
/// long before it, and only a run that never produces the event hits it. It is
/// a minute because a loaded machine (#224) starts a detached brgr process,
/// runs a fake Herdr, or seals a result seconds late, and a shorter guard then
/// becomes the reason a correct run fails.
pub const HANG_GUARD: Duration = Duration::from_mins(1);

/// Polls `ready` every 50 ms until it is true, for up to [`HANG_GUARD`].
/// Returns whether it became true, so the caller names the event it waited for
/// in its own assertion.
pub fn wait_until(mut ready: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    loop {
        if ready() {
            return true;
        }
        if started.elapsed() >= HANG_GUARD {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Whether `pid` still exists, zombies included: `/bin/kill -0`.
pub fn process_exists(pid: &str) -> bool {
    Command::new("/bin/kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
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
    let mut command = Command::new(brgr_binary());
    command.env("BRGR_TEST_TRUST_HERDR_PANE", "1");
    isolate_test_command(&mut command);
    command
}

/// Removes inherited worker/Herdr identity and supplies the isolated test PATH
/// for any intermediary process that may execute brgr.
pub fn isolate_test_command(command: &mut Command) {
    for name in INHERITED_WORKER_ENV {
        command.env_remove(name);
    }
    clear_inherited_herdr_env(command);
    // Harness children inherit PATH (but the runner removes HERDR_BIN_PATH),
    // so put a failing Herdr stub first while preserving every existing tool.
    command.env("PATH", test_path());
}

/// PATH used by intermediary test processes that may launch the brgr binary.
pub fn test_path() -> std::ffi::OsString {
    std::env::join_paths(
        std::iter::once(herdr_stub_dir().to_path_buf())
            .chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
    )
    .unwrap()
}

fn herdr_stub_dir() -> &'static Path {
    HERDR_STUB_DIR.get_or_init(|| {
        let directory =
            env::temp_dir().join(format!("brgr-test-herdr-stub-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        write_fast_script(&directory.join("herdr"), "#!/bin/sh\nexit 1\n");
        directory
    })
}

/// Removes inherited Herdr environment variables from this process.
/// Intermediary processes use this when they may run brgr later.
pub fn clear_inherited_herdr_env(command: &mut Command) {
    for (name, _) in env::vars_os() {
        if name.to_string_lossy().starts_with("HERDR_") {
            command.env_remove(name);
        }
    }
}

/// Path to the binary under test for places that need to name it as data.
pub fn brgr_binary() -> &'static str {
    env!("CARGO_BIN_EXE_brgr")
}

fn contains_binary_constructor(source: &str) -> bool {
    let constructor = concat!("Command::new(env!(\"CARGO_BIN_EXE_", "brgr\"))");
    let binary = concat!("env!(\"CARGO_BIN_EXE_", "brgr\")");
    source.contains(constructor) || source.contains(binary)
}

pub fn brgr_constructor_offenders(root: &Path) -> Vec<PathBuf> {
    fn scan(directory: &Path, allowed: &Path, offenders: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                scan(&path, allowed, offenders);
            } else if path.extension().is_some_and(|ext| ext == "rs") && path != allowed {
                let source = fs::read_to_string(&path).unwrap();
                if contains_binary_constructor(&source) {
                    offenders.push(path);
                }
            }
        }
    }

    let allowed = root.join("tests/common/mod.rs");
    let mut offenders = Vec::new();
    for directory in [root.join("tests"), root.join("benches")] {
        scan(&directory, &allowed, &mut offenders);
    }
    offenders.sort();
    offenders
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
