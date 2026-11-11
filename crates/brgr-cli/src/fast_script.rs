//! Test fixture: a `#!/bin/sh` executable without macOS's first-exec cost.
//!
//! The first execution of every new executable file is serialized by macOS
//! (~200 ms each), so a test process that writes a fresh script per case waits
//! in one queue behind every other test's scripts (#224). One trampoline per
//! source, shared by every test process and named for the source's SHA-256,
//! is made executable and run once; [`write_fast_script`] hard-links
//! that warmed inode to `path` and writes `body` beside it as `path` with a
//! `.body` suffix (mode `0644`, not executable). The trampoline runs `/bin/sh`
//! on that file, so this is only for a body whose first line is `#!/bin/sh`,
//! and `$0` inside the body is `path.body`: a fixture that names its side
//! files from `$0` must strip the suffix (`${0%.body}`) to keep the same name.
//!
//! A test that later overwrites, digests or chmods the executable bytes must
//! keep a unique inode (ordinary `fs::write` and `chmod`) so it does not share
//! this trampoline.

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
        shared_trampoline_in(&dir)
    })
}

/// The one trampoline every test process shares, named for the first 16 hex
/// chars of the source's SHA-256 so an older source's file is never reused.
/// An existing file is used only when it is a regular file with mode `0555`
/// and exactly this source; anything else is replaced by writing a pid-named
/// sibling, warming it, and renaming it over the shared name, so a racing
/// process finds either a complete `0555` file or keeps the links it already
/// made, never a half-written or writable trampoline.
fn shared_trampoline_in(dir: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let digest = Sha256::digest(TRAMPOLINE_SOURCE.as_bytes());
    let mut hex = String::with_capacity(16);
    for byte in &digest[..8] {
        write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    let trampoline = dir.join(format!("trampoline-{hex}"));
    let valid = fs::metadata(&trampoline)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o777 == 0o555)
        && fs::read(&trampoline).is_ok_and(|bytes| bytes == TRAMPOLINE_SOURCE.as_bytes());
    if valid {
        return trampoline;
    }
    let mut temporary = trampoline.file_name().unwrap().to_os_string();
    temporary.push(format!(".{}", std::process::id()));
    let temporary = trampoline.with_file_name(temporary);
    fs::write(&temporary, TRAMPOLINE_SOURCE).unwrap();
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o755)).unwrap();
    warm_executable(&temporary);
    // Drop write bits so a later `fs::write` through a hard link cannot
    // clobber every trampoline in this process.
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o555)).unwrap();
    fs::rename(&temporary, &trampoline).unwrap();
    trampoline
}
