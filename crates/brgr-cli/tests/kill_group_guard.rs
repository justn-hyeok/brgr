//! Linux procps `kill` reads `-<pgid>` as an option and ends up as `kill(-1)`,
//! which signals every process of the user. A group is signalled with
//! `kill SIGNAL -- -<pgid>`; macOS's `kill` accepts the same form.

use std::fs;
use std::path::{Path, PathBuf};

const WINDOW_LINES: usize = 8;

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name != "target") {
                rust_files(&path, out);
            }
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn unseparated_group_kills(source: &str) -> Vec<usize> {
    let lines: Vec<&str> = source.lines().collect();
    let command = ["\"/bin/", "kill\""].concat();
    let negative = ["format!(\"-", "{"].concat();
    let mut found = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if !line.contains(&command) {
            continue;
        }
        let window = &lines[index..lines.len().min(index + WINDOW_LINES)];
        let end = window
            .iter()
            .position(|line| line.contains(';') || line.trim_end().ends_with(".await"))
            .map_or(window.len(), |end| end + 1);
        let call = window[..end].join("\n");
        if call.contains(&negative) && !call.contains("\"--\"") {
            found.push(index + 1);
        }
    }
    found
}

#[test]
fn a_detector_flags_a_group_kill_without_the_separator() {
    let bad = [
        "Command::new(\"/bin/",
        "kill\")\n    .args([\"-KILL\", &format!(\"-",
        "{pid}\")])\n    .status();",
    ]
    .concat();
    let good = [
        "Command::new(\"/bin/",
        "kill\")\n    .args([\"-KILL\", \"--\", &format!(\"-",
        "{pid}\")])\n    .status();",
    ]
    .concat();
    assert_eq!(unseparated_group_kills(&bad), vec![1]);
    assert!(unseparated_group_kills(&good).is_empty());
}

#[test]
fn every_group_kill_separates_the_negative_pid_with_double_dash() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    let me = Path::new(file!()).file_name().unwrap().to_owned();
    let mut offenders = Vec::new();
    for file in files {
        if file.file_name().is_some_and(|name| *name == *me) {
            continue;
        }
        for line in unseparated_group_kills(&fs::read_to_string(&file).unwrap()) {
            offenders.push(format!("{}:{line}", file.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "group kill without `--` before the negative pid: {offenders:#?}"
    );
}
