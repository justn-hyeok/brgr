//! Contract for [`write_fast_script`]: a warmed trampoline hard-link.

mod common;

use std::{fs, os::unix::fs::PermissionsExt, process::Command};

use common::write_fast_script;
use tempfile::TempDir;

#[test]
fn write_fast_script_runs_the_body_with_arguments_and_its_exit_code() {
    let temp = TempDir::new().unwrap();
    let script = temp.path().join("echo-status");
    write_fast_script(
        &script,
        "#!/bin/sh\nprintf '%s\\n' \"$1\" \"$2\"\nexit 17\n",
    );
    let mode = fs::metadata(&script).unwrap().permissions().mode();
    assert_ne!(mode & 0o111, 0, "script is not executable: {mode:#o}");
    let body = temp.path().join("echo-status.body");
    let body_mode = fs::metadata(&body).unwrap().permissions().mode();
    assert_eq!(body_mode & 0o111, 0, "body is executable: {body_mode:#o}");
    let output = Command::new(&script)
        .args(["alpha", "beta"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(17));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "alpha\nbeta\n");
}

#[test]
#[should_panic(expected = "#!/bin/sh")]
fn write_fast_script_rejects_a_non_sh_shebang() {
    let temp = TempDir::new().unwrap();
    write_fast_script(&temp.path().join("bash-script"), "#!/bin/bash\nexit 0\n");
}

/// `ETXTBSY` is what a file open for writing gives `exec` on Linux, and what a
/// `fork` in another test thread can briefly cause.
#[cfg(target_os = "linux")]
#[test]
fn warming_waits_out_a_file_another_process_holds_open_for_writing() {
    let directory = TempDir::new().unwrap();
    let script = directory.path().join("busy");
    let writer = fs::File::create(&script).unwrap();
    std::io::Write::write_all(&mut &writer, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        Command::new(&script).status().unwrap_err().kind(),
        std::io::ErrorKind::ExecutableFileBusy
    );
    let warming = {
        let script = script.clone();
        std::thread::spawn(move || common::warm_executable(&script))
    };
    // A pause only gives the retry something to wait out; the test passes
    // however the two threads interleave once the writer has gone.
    std::thread::sleep(std::time::Duration::from_millis(100));
    drop(writer);
    warming.join().unwrap();
}
