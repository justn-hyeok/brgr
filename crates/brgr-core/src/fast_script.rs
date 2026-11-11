//! Test fixture: a `#!/bin/sh` executable without macOS's first-exec cost.
//!
//! The first execution of every new executable file is serialized by macOS
//! (~200 ms each), so a test process that writes a fresh script per case waits
//! in one queue behind every other test's scripts (#224). One trampoline per
//! source, shared by every test process and named for the source's SHA-256,
//! is made executable and run once; [`write_fast_script`] hard-links
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
/// If the hard link fails with [`io::ErrorKind::CrossesDevices`] or
/// [`io::ErrorKind::TooManyLinks`], the body is written to `path` with mode
/// `0755` instead.
///
/// # Panics
///
/// Panics if `body` does not start with `#!/bin/sh`, if `path` has no file
/// name, or if creating, warming, or linking the trampoline fails for a
/// reason other than a cross-device or too-many-links hard link.
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
    place_script(warmed_trampoline(), path, body, |from, to| {
        fs::hard_link(from, to)
    });
}

/// Links `trampoline` to `path`, or writes `body` to `path` with mode `0755`
/// when the link fails because the filesystem does not allow it: a
/// cross-device link (`EXDEV`) or an inode at its hard-link limit (`EMLINK`,
/// reachable on Linux ext4 at ~65,000 links once leaked test temp dirs
/// accumulate). Any other link error panics.
fn place_script(
    trampoline: &Path,
    path: &Path,
    body: &str,
    link: impl FnOnce(&Path, &Path) -> io::Result<()>,
) {
    match link(trampoline, path) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::CrossesDevices | io::ErrorKind::TooManyLinks
            ) =>
        {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn shared_name() -> String {
        use sha2::{Digest, Sha256};
        use std::fmt::Write as _;
        let digest = Sha256::digest(TRAMPOLINE_SOURCE.as_bytes());
        let mut hex = String::with_capacity(16);
        for byte in &digest[..8] {
            write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
        }
        format!("trampoline-{hex}")
    }

    fn entry_count(dir: &Path) -> usize {
        fs::read_dir(dir).unwrap().count()
    }

    #[test]
    fn valid_shared_trampoline_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let first = shared_trampoline_in(dir.path());
        let inode = fs::metadata(&first).unwrap().ino();
        let second = shared_trampoline_in(dir.path());
        assert_eq!(first, second);
        assert_eq!(fs::metadata(&second).unwrap().ino(), inode);
        assert_eq!(entry_count(dir.path()), 1);
    }

    #[test]
    fn shared_trampoline_with_wrong_bytes_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let trampoline = dir.path().join(shared_name());
        fs::write(&trampoline, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&trampoline, fs::Permissions::from_mode(0o555)).unwrap();
        let stale_inode = fs::metadata(&trampoline).unwrap().ino();
        let got = shared_trampoline_in(dir.path());
        assert_eq!(got, trampoline);
        assert_eq!(fs::read(&got).unwrap(), TRAMPOLINE_SOURCE.as_bytes());
        let meta = fs::metadata(&got).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o555);
        assert_ne!(meta.ino(), stale_inode);
        assert_eq!(entry_count(dir.path()), 1);
    }

    #[test]
    fn writable_shared_trampoline_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let trampoline = dir.path().join(shared_name());
        fs::write(&trampoline, TRAMPOLINE_SOURCE).unwrap();
        fs::set_permissions(&trampoline, fs::Permissions::from_mode(0o755)).unwrap();
        let stale_inode = fs::metadata(&trampoline).unwrap().ino();
        let got = shared_trampoline_in(dir.path());
        assert_eq!(got, trampoline);
        let meta = fs::metadata(&got).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o555);
        assert_ne!(meta.ino(), stale_inode);
        assert_eq!(entry_count(dir.path()), 1);
    }

    const OK_BODY: &str = "#!/bin/sh\necho ok\n";

    /// Runs a script this test just wrote. On Linux another test thread's
    /// `fork` can hold it open for writing until its child execs, so the
    /// first exec can fail with `ETXTBSY`; that alone is retried, with a hang
    /// guard a correct run does not reach.
    fn run_script(path: &Path) -> String {
        let started = std::time::Instant::now();
        let output = loop {
            match Command::new(path).output() {
                Ok(output) => break output,
                Err(error)
                    if error.kind() == io::ErrorKind::ExecutableFileBusy
                        && started.elapsed() < std::time::Duration::from_mins(1) =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("run {}: {error}", path.display()),
            }
        };
        assert!(
            output.status.success(),
            "{} failed: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn fallback_writes_executable_body(error: io::Error) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script");
        place_script(Path::new("no-such-trampoline"), &path, OK_BODY, |_, _| {
            Err(error)
        });
        assert_eq!(fs::read(&path).unwrap(), OK_BODY.as_bytes());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(run_script(&path).trim(), "ok");
    }

    #[test]
    fn too_many_links_writes_executable_body() {
        fallback_writes_executable_body(io::Error::from(io::ErrorKind::TooManyLinks));
    }

    #[test]
    fn cross_device_link_writes_executable_body() {
        fallback_writes_executable_body(io::Error::from(io::ErrorKind::CrossesDevices));
    }

    #[test]
    #[should_panic(expected = "hard-linking trampoline to")]
    fn other_link_error_panics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script");
        place_script(Path::new("no-such-trampoline"), &path, OK_BODY, |_, _| {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        });
    }

    /// Child of [`two_test_processes_share_one_trampoline`]: run only as a
    /// `--exact --ignored` single test in a spawned copy of this binary, with
    /// `TMPDIR` pointed at a fresh dir.
    #[test]
    #[ignore = "runs only as a spawned child of two_test_processes_share_one_trampoline"]
    fn child_writes_and_runs_script() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script");
        write_fast_script(&path, OK_BODY);
        assert_eq!(run_script(&path).trim(), "ok");
    }

    /// Two separate test processes sharing one `TMPDIR` must leave exactly
    /// one trampoline file (`trampoline-<sha256>`), not one per process.
    #[test]
    fn two_test_processes_share_one_trampoline() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = std::env::current_exe().unwrap();
        for _ in 0..2 {
            let output = Command::new(&exe)
                .args([
                    "--exact",
                    "fast_script::tests::child_writes_and_runs_script",
                    "--ignored",
                    "--test-threads=1",
                ])
                .env("TMPDIR", tmp.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "child failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert_eq!(entry_count(&tmp.path().join("brgr-fast-script")), 1);
    }
}
