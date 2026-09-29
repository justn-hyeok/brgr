//! Concurrent admission: does bounded execution actually overlap, and does the
//! store survive the overlap?
//!
//! Two properties are reported together because they trade against each other.
//! `eff` is how much of the ideal speedup survives, and `failed` is how many of
//! those runs never reached a sealed result because the CLI exited nonzero. A
//! fast sweep with failures is not a passing sweep.
//!
//! The workspaces are real Git repositories. A non-Git workspace short-circuits
//! `acquire_admission_lock` and `prepare_workspace`, so a sweep over plain
//! directories takes no admission lock and creates no worktree — it would
//! measure neither of the things named above. Two shapes are reported: one
//! shared repository, where admissions serialize on the repository-wide lock
//! before execution overlaps, and one repository per run, where they do not.
//!
//! ```text
//! cargo bench -p brgr-cli --bench concurrent_admission
//! ```

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// Matches the `sleep` in `testdata/fixtures/bench/gjc`.
const HARNESS_SECONDS: f64 = 1.0;
const WIDTHS: &[usize] = &[1, 2, 4, 8];

fn main() {
    let temp = TempDir::new().expect("bench temp dir");
    let home = temp.path().join("home");
    let scratch = temp.path().join("scratch");
    fs::create_dir_all(&scratch).expect("scratch dir");
    register_fixture(&home, &scratch);

    println!("concurrent admission of a {HARNESS_SECONDS:.0}s harness");
    println!("  eff 100% = fully parallel, eff ~1/n = fully serialized");

    for shared in [true, false] {
        println!();
        println!(
            "  {} repository",
            if shared { "one shared" } else { "one per run" }
        );
        println!("   width      wall   speedup     eff   failed   distinct errors");
        println!("  ------   -------   -------   -----   ------   ---------------");
        for &width in WIDTHS {
            let sweep = sweep(temp.path(), &home, width, shared);
            let n = f64::from(u32::try_from(width).unwrap_or(1));
            let speedup = HARNESS_SECONDS * n / sweep.wall.as_secs_f64();
            let errors = if sweep.errors.is_empty() {
                "-".to_owned()
            } else {
                sweep.errors.iter().cloned().collect::<Vec<_>>().join("; ")
            };
            println!(
                "  {width:>6}   {:>7.2?}   {speedup:>6.2}x   {:>4.0}%   {:>3}/{width:<2}   {errors}",
                sweep.wall,
                speedup / n * 100.0,
                sweep.failed,
            );
        }
    }

    println!();
    println!("Any nonzero `failed` column is a correctness result, not a slow one:");
    println!("the harness already ran, so a failed admission discards work a paid");
    println!("route would have charged for. Reconciliation recovers the sealed bytes");
    println!("on the next `brgr status`, but the caller saw a hard error.");
}

struct Sweep {
    wall: Duration,
    failed: usize,
    errors: BTreeSet<String>,
}

fn sweep(base: &Path, home: &Path, width: usize, shared: bool) -> Sweep {
    let label = if shared { "shared" } else { "split" };
    let logs = base.join(format!("logs-{label}-{width}"));
    fs::create_dir_all(&logs).expect("log dir");
    let repositories: Vec<PathBuf> = if shared {
        let repository = seed_repository(base, &format!("{label}-{width}"));
        vec![repository; width]
    } else {
        (0..width)
            .map(|index| seed_repository(base, &format!("{label}-{width}-{index}")))
            .collect()
    };

    let start = Instant::now();
    let children: Vec<_> = repositories
        .iter()
        .enumerate()
        .map(|(index, repository)| {
            // Stderr to a file: children awaited in turn would deadlock on a
            // full pipe buffer if one produced a large backtrace.
            let log = logs.join(format!("{index}.stderr"));
            let child = brgr(home)
                .args(["--json", "run", "SLOW bench", "--workspace"])
                .arg(repository)
                .arg("--foreground")
                .stdout(Stdio::null())
                .stderr(Stdio::from(File::create(&log).expect("stderr log")))
                .spawn()
                .expect("spawn run");
            (child, log)
        })
        .collect();

    let mut failed = 0;
    let mut errors = BTreeSet::new();
    for (mut child, log) in children {
        if !child.wait().expect("await run").success() {
            failed += 1;
            if let Ok(text) = fs::read_to_string(&log)
                && let Some(line) = text.lines().find(|line| !line.trim().is_empty())
            {
                errors.insert(line.trim().to_owned());
            }
        }
    }

    Sweep {
        wall: start.elapsed(),
        failed,
        errors,
    }
}

fn seed_repository(base: &Path, name: &str) -> PathBuf {
    let repository = base.join(format!("repo-{name}"));
    if repository.is_dir() {
        return repository;
    }
    fs::create_dir_all(&repository).expect("repository dir");
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .arg("-C")
            .arg(&repository)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.name", "Bench"]);
    git(&["config", "user.email", "bench@example.invalid"]);
    fs::write(repository.join("README"), b"seed\n").expect("seed file");
    git(&["add", "README"]);
    git(&["commit", "-m", "seed"]);
    repository
}

fn register_fixture(home: &Path, scratch: &Path) {
    let status = brgr(home)
        .arg("harness")
        .arg("add")
        .arg(bench_fixture())
        .arg("--workspace")
        .arg(scratch)
        .args(["--prompt", "BRGR_FIXTURE_OK"])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .expect("register bench fixture");
    assert!(status.success(), "bench fixture registration failed");
}

fn brgr(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_brgr"));
    command
        .arg("--home")
        .arg(home)
        .env_remove("BRGR_HOME")
        .env("BRGR_OWNER_ID", "codex:bench-owner")
        .env("BRGR_SESSION_ID", "bench-session");
    command
}

fn bench_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/bench/gjc")
        .canonicalize()
        .expect("bench fixture path")
}
