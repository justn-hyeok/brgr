//! Test fixture: a `#!/bin/sh` executable without macOS's first-exec cost.
//!
//! The first execution of every new executable file is serialized by macOS
//! (~200 ms each), so a test process that writes a fresh script per case waits
//! in one queue behind every other test's scripts (#224). One process-global
//! trampoline is made executable and run once; [`write_fast_script`] hard-links
//! that warmed inode to `path` and writes `body` beside it as `path` with a
//! `.body` suffix (mode `0644`, not executable). The trampoline runs `/bin/sh`
//! on that file, so this is only for a body whose first line is `#!/bin/sh`.
//! The same helper lives in `brgr-cli`, `brgr-runner` and `brgr-registry`; a
//! test crate cannot import another crate's `#[cfg(test)]` module.

use std::{
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

const TRAMPOLINE_SOURCE: &str = concat!(
    "#!/bin/sh\n",
    "[ -f \"$0.body\" ] || exit 0\n",
    "exec /bin/sh \"$0.body\" \"$@\"\n"
);

static WARMED_TRAMPOLINE: OnceLock<PathBuf> = OnceLock::new();

/// Writes a `#!/bin/sh` test executable without macOS's first-exec cost.
///
/// If the hard link fails with [`io::ErrorKind::CrossesDevices`], the body is
/// written to `path` with mode `0755` instead.
///
/// # Panics
///
/// Panics if `body` does not start with `#!/bin/sh`, if `path` has no file
/// name, or if creating, warming, or linking the trampoline fails for a
/// reason other than a cross-device hard link.
pub(crate) fn write_fast_script(path: &Path, body: &str) {
    let first_line = body.lines().next();
    assert_eq!(
        first_line,
        Some("#!/bin/sh"),
        "write_fast_script requires a #!/bin/sh first line, got {first_line:?}"
    );
    let mut body_name = path
        .file_name()
        .expect("write_fast_script path has a file name")
        .to_os_string();
    body_name.push(".body");
    let body_path = path.with_file_name(body_name);
    fs::write(&body_path, body).unwrap();
    fs::set_permissions(&body_path, fs::Permissions::from_mode(0o644)).unwrap();
    if path.exists() {
        fs::remove_file(path).unwrap();
    }
    match fs::hard_link(warmed_trampoline(), path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
            fs::write(path, body).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        Err(error) => panic!("hard-linking trampoline to {}: {error}", path.display()),
    }
}

/// Runs a just-written trampoline once. On Linux another thread's `fork` can
/// hold the file open for writing until its child execs, so the first run can
/// fail with `ETXTBSY` for a moment. It is retried only for that, with a hang
/// guard a correct run does not reach.
fn warm_executable(path: &Path) {
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

fn warmed_trampoline() -> &'static Path {
    WARMED_TRAMPOLINE.get_or_init(|| {
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
    })
}
