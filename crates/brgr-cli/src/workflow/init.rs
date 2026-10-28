//! Write the JavaScript SDK and a short example into a directory.

use std::{fs, path::Path};

use anyhow::{Context as _, Result, bail};
use serde_json::json;

use crate::print_value;

const SDK: &str = include_str!("../../assets/brgr-workflow.mjs");
const EXAMPLE: &str = r#"import { agent, parallel } from "./brgr-workflow.mjs";

const reviews = await parallel([
  () => agent({ task: "Review security", deadline: 600 }),
  () => agent({ task: "Review architecture" }),
]);

if (reviews.some((review) => review.report.includes("critical"))) {
  await agent({ task: "Reproduce and fix verified issues" });
}
"#;

pub(super) fn write_sdk(dir: Option<&Path>, force: bool, json_output: bool) -> Result<()> {
    let dir = match dir {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir()?,
    };
    fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    let sdk_path = dir.join("brgr-workflow.mjs");
    let example_path = dir.join("example.mjs");
    write_if_allowed(&sdk_path, SDK, force)?;
    write_if_allowed(&example_path, EXAMPLE, force)?;
    print_value(
        &json!({
            "sdk": sdk_path,
            "example": example_path,
        }),
        json_output,
    );
    Ok(())
}

fn write_if_allowed(path: &Path, contents: &str, force: bool) -> Result<()> {
    if path.exists() {
        refuse_different(path, contents, force)?;
        if !force {
            return Ok(());
        }
    }
    fs::write(path, contents).with_context(|| format!("could not write {}", path.display()))?;
    Ok(())
}

fn refuse_different(path: &Path, contents: &str, force: bool) -> Result<()> {
    let existing = fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    if existing == contents.as_bytes() || force {
        return Ok(());
    }
    bail!(
        "{} already exists and differs; pass --force to overwrite",
        path.display()
    )
}
