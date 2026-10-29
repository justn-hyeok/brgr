//! Write the JavaScript SDK, TypeScript declarations, and a short example.

use std::{fs, path::Path};

use anyhow::{Context as _, Result, bail};
use serde_json::json;

use crate::print_value;

const SDK: &str = include_str!("../../assets/brgr-workflow.mjs");
const DTS: &str = include_str!("../../assets/brgr-workflow.d.mts");
const EXAMPLE: &str = r#"import { agent, pipeline } from "./brgr-workflow.mjs";

const { results, stopped_at } = await pipeline([
  () => agent({ task: "Review security", deadline: 600 }),
  (prev) => agent({ task: `Fix issues from: ${prev.report}` }),
]);
"#;

pub(super) fn write_sdk(dir: Option<&Path>, force: bool, json_output: bool) -> Result<()> {
    let dir = match dir {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir()?,
    };
    fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    let sdk_path = dir.join("brgr-workflow.mjs");
    let dts_path = dir.join("brgr-workflow.d.mts");
    let example_path = dir.join("example.mjs");
    write_if_allowed(&sdk_path, SDK, force)?;
    write_if_allowed(&dts_path, DTS, force)?;
    write_if_allowed(&example_path, EXAMPLE, force)?;
    print_value(
        &json!({
            "sdk": sdk_path,
            "dts": dts_path,
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

#[cfg(test)]
mod tests {
    use super::{DTS, SDK};
    use std::collections::BTreeSet;

    fn exported_functions(source: &str) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        for line in source.lines() {
            let line = line.trim();
            let rest = line
                .strip_prefix("export async function ")
                .or_else(|| line.strip_prefix("export declare function "))
                .or_else(|| line.strip_prefix("export function "));
            let Some(rest) = rest else {
                continue;
            };
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                names.insert(name);
            }
        }
        names
    }

    #[test]
    fn the_declaration_file_exports_the_same_functions_as_the_sdk() {
        let sdk = exported_functions(SDK);
        let dts = exported_functions(DTS);
        assert_eq!(sdk, dts, "mjs exports {sdk:?} dts exports {dts:?}");
        assert!(sdk.contains("agent"), "{sdk:?}");
        assert!(sdk.contains("parallel"), "{sdk:?}");
        assert!(sdk.contains("pipeline"), "{sdk:?}");
    }
}
