//! Test fixture: a `#!/bin/sh` executable without macOS's first-exec cost.
//!
//! The first execution of every new executable file is serialized by macOS
//! (~200 ms each), so a test process that writes a fresh script per case waits
//! in one queue behind every other test's scripts (#224). One process-global
//! trampoline is made executable and run once; [`write_fast_script`] hard-links
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
