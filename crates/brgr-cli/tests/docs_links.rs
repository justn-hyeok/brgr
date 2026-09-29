//! Every relative link in the repository's prose resolves to a file.
//!
//! `docs/` was flat — forty-seven files, nineteen of them release notes, with no
//! index — and sorting it into directories meant rewriting sixty-seven links. A
//! reorganization that leaves a broken link behind is worse than the pile it
//! replaced, and the only way to know is to resolve every one of them. Left as a
//! test rather than a one-off script so the next move is cheap to verify.
//!
//! It lives under `brgr-cli` because the workspace has no repository-level test
//! crate and this is the crate every other one depends on last; nothing about
//! the check is CLI-specific.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// Files whose prose is checked: markdown anywhere, plus the Rust sources and
/// workflows, which name documentation paths in strings a link checker would
/// otherwise never see.
fn tracked_files(root: &Path) -> Vec<PathBuf> {
    let listing = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .output()
        .expect("git ls-files");
    assert!(listing.status.success(), "git ls-files failed");
    String::from_utf8_lossy(&listing.stdout)
        .split('\0')
        .filter(|path| !path.is_empty())
        .filter(|path| {
            Path::new(path).extension().is_some_and(|extension| {
                matches!(extension.as_encoded_bytes(), b"md" | b"rs" | b"yml")
            })
        })
        .map(|path| root.join(path))
        .collect()
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root is two levels above the crate")
        .to_path_buf()
}

/// Markdown links of the form `](target)`, with the anchor and title removed.
///
/// Deliberately not a markdown parser: the question is only whether a path that
/// looks like a link to a file in this repository resolves, and a parser would
/// bring a dependency for a job a split does.
fn markdown_link_targets(text: &str) -> Vec<String> {
    let mut targets = Vec::new();
    for tail in text.split("](").skip(1) {
        let Some(end) = tail.find(')') else { continue };
        let target = tail[..end].trim();
        // A title after the path, as in `](path "Title")`.
        let target = target.split_whitespace().next().unwrap_or(target);
        // An anchor alone points inside the same file.
        let target = target.split('#').next().unwrap_or(target);
        if target.is_empty() || target.contains("://") || target.starts_with("mailto:") {
            continue;
        }
        targets.push(target.to_owned());
    }
    targets
}

/// Bare `docs/...md` paths named in prose or in a string literal.
fn bare_docs_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for tail in text.split("docs/").skip(1) {
        let path: String = tail
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/'))
            .collect();
        if Path::new(&path).extension().is_some_and(|e| e == "md") {
            paths.push(format!("docs/{path}"));
        }
    }
    paths
}

#[test]
fn every_relative_link_in_the_repository_resolves() {
    let root = repository_root();
    let mut broken: BTreeSet<String> = BTreeSet::new();
    let mut checked = 0_u32;

    for file in tracked_files(&root) {
        // This file's own prose spells out the shapes it looks for — `](path
        // "Title")`, a bare `docs/...md` — and matching them would report four
        // links that were never meant to resolve.
        if file.ends_with("tests/docs_links.rs") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        let here = file.parent().expect("a tracked file has a parent");
        let shown = file.strip_prefix(&root).unwrap_or(&file).display();

        for target in markdown_link_targets(&text) {
            // Absolute-looking targets are site paths, not repository files.
            if target.starts_with('/') {
                continue;
            }
            checked += 1;
            if !here.join(&target).exists() {
                broken.insert(format!("{shown} -> {target}"));
            }
        }
        // A bare path in prose is repository-relative, not file-relative: it is
        // written for a reader standing at the repository root.
        for target in bare_docs_paths(&text) {
            checked += 1;
            if !root.join(&target).exists() {
                broken.insert(format!("{shown} -> {target}"));
            }
        }
    }

    assert!(
        broken.is_empty(),
        "{} of {checked} links do not resolve:\n  {}",
        broken.len(),
        broken.into_iter().collect::<Vec<_>>().join("\n  ")
    );
    // A checker that finds nothing to check passes for the wrong reason. The
    // repository had sixty-seven documentation links when this was written.
    assert!(
        checked > 50,
        "only {checked} links were examined; the extraction has stopped finding them"
    );
}
