//! The published JSON schemas must describe the bytes brgr actually writes.
//!
//! `schemas/` is the documented v1 wire contract, but nothing linked it to the
//! Rust types, so either side could drift silently. This checks both
//! directions: every `required` property is present, and — where the schema
//! closes itself with `additionalProperties: false` — every serialized key is
//! described.
//!
//! This is a structural check, not a JSON Schema validator. It resolves local
//! `$ref`s and honors `required`, `properties`, `additionalProperties`, `items`,
//! `type`, `const`, `enum`, `oneOf` and `anyOf`. It does not evaluate value
//! constraints such as `minLength`, `minimum`, `pattern` or `format`, so a green
//! run means the field names and shapes agree, not that every constraint holds.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use brgr_protocol::{
    ArtifactContract, ArtifactRef, AttemptBudget, AttemptId, EvidenceSpec, ObservationSource,
    OwnerId, ResultEnvelope, ResultId, Route, RouteObservation, SCHEMA_V1, TaskId,
    TaskInstructions, TaskSpec, TerminalOutcome,
};
use serde_json::{Value, json};
use tempfile::TempDir;

#[test]
fn task_schema_describes_a_serialized_task_spec() {
    let task = TaskSpec {
        schema: SCHEMA_V1.to_owned(),
        task_id: TaskId::new(),
        revision: 3,
        create_request_id: "request-1".to_owned(),
        owner_id: OwnerId::new("codex:thread-1").unwrap(),
        objective: "Review the change".to_owned(),
        workspace: "/tmp/worktree".to_owned(),
        route: Route {
            harness_id: "local.gjc".to_owned(),
            // Populated so the optional route fields are exercised too.
            requested_model: Some("openai-codex/gpt-5.6-luna".to_owned()),
            requested_effort: Some("minimal".to_owned()),
        },
        required_capabilities: vec!["completion".to_owned()],
        artifact_contract: ArtifactContract {
            media_type: "text/plain".to_owned(),
            max_bytes: 1_048_576,
        },
        acceptance_criteria: vec!["report cites changed lines".to_owned()],
        budget: AttemptBudget {
            deadline_seconds: 3_600,
            max_attempts: 2,
        },
        // Populated rather than defaulted: these skip serialization when empty,
        // so leaving them out would not exercise the schema that describes them.
        instructions: TaskInstructions {
            scope: vec!["crates/brgr-store".to_owned()],
            role: vec!["reviewer".to_owned()],
            forward_criteria: true,
        },
        evidence: EvidenceSpec {
            capture_diff: true,
            capture_logs: true,
            files: vec!["README.md".to_owned()],
            base_commit: Some("a".repeat(40)),
            base_tree: Some("b".repeat(40)),
        },
        max_concurrent_children: Some(2),
        permission: None,
    };
    assert_matches_schema("task-v1.json", &serde_json::to_value(&task).unwrap());
}

#[test]
fn result_schema_describes_a_serialized_result_envelope() {
    assert_matches_schema("result-v1.json", &serde_json::to_value(result()).unwrap());
}

/// An unreleased intermediate v1 build embedded its route observation in the
/// envelope, and brgr still reads and re-serializes those bytes.
#[test]
fn result_schema_describes_a_legacy_embedded_route_observation() {
    let mut envelope = result();
    envelope.legacy_embedded_route_observation = Some(RouteObservation {
        model: Some("openai-codex/gpt-5.6-luna".to_owned()),
        model_source: ObservationSource::HarnessJsonl,
        effort: None,
        effort_source: ObservationSource::Unavailable,
    });
    let encoded = serde_json::to_value(&envelope).unwrap();
    assert!(
        encoded.get("route_observation").is_some(),
        "legacy field stopped serializing; this test no longer covers those bytes"
    );
    assert_matches_schema("result-v1.json", &encoded);
}

#[test]
fn harness_schema_describes_a_drafted_manifest() {
    // An isolated home: without `--home` this resolves the developer's real
    // control home, creates and chmods it, and opens their real registry.
    let temp = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_brgr"))
        .arg("--home")
        .arg(temp.path().join("home"))
        .arg("--json")
        .args(["harness", "draft"])
        .arg(fixture())
        .env_remove("BRGR_HOME")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "harness draft failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let manifest: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_matches_schema("harness-v1.json", &manifest);
}

/// The drift check is only worth its name if it fails on a real mismatch.
#[test]
fn the_drift_check_reports_missing_required_and_undescribed_properties() {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["kept", "missing"],
        "properties": {
            "kept": { "type": "string" },
            "missing": { "type": "string" },
            "nested": { "$ref": "#/$defs/leaf" },
        },
        "$defs": { "leaf": { "type": "object", "required": ["needed"], "properties": {} } },
    });
    let mut findings = Vec::new();
    compare(
        &schema,
        &schema,
        &json!({ "kept": "yes", "extra": 1, "nested": {} }),
        "$",
        &mut findings,
    );
    let report = findings.join("\n");
    assert!(report.contains("$.missing"), "{report}");
    assert!(report.contains("$.extra"), "{report}");
    assert!(
        report.contains("$.nested.needed"),
        "a $ref was not resolved: {report}"
    );

    // A required property on a schema with no `properties` block — the shape
    // `harness-v1.json` uses for `capabilities` — must still be checked.
    let map_schema = json!({
        "type": "object",
        "required": ["completion"],
        "additionalProperties": { "type": "object" },
    });
    let mut findings = Vec::new();
    compare(
        &map_schema,
        &map_schema,
        &json!({ "cancel": {} }),
        "$",
        &mut findings,
    );
    assert!(
        findings
            .iter()
            .any(|finding| finding.contains("completion")),
        "{findings:?}"
    );

    // A type mismatch must be reported, not silently skipped.
    let typed = json!({ "type": "object", "properties": {} });
    let mut findings = Vec::new();
    compare(&typed, &typed, &json!("scalar"), "$", &mut findings);
    assert!(!findings.is_empty(), "a scalar passed an object schema");
}

fn result() -> ResultEnvelope {
    ResultEnvelope {
        schema: SCHEMA_V1.to_owned(),
        task_id: TaskId::new(),
        revision: 1,
        attempt_id: AttemptId::new(),
        result_id: ResultId::new(),
        outcome: TerminalOutcome::Candidate,
        artifacts: vec![ArtifactRef {
            digest: format!("sha256:{}", "a".repeat(64)),
            bytes: 17,
            media_type: "text/plain".to_owned(),
            store_relative_path: "artifacts/sha256/aa/aa".to_owned(),
        }],
        error: None,
        legacy_embedded_route_observation: None,
        route_observation: None,
        unresolved_effects: vec![],
    }
}

fn assert_matches_schema(name: &str, value: &Value) {
    let schema = load_schema(name);
    let mut findings = Vec::new();
    compare(&schema, &schema, value, "$", &mut findings);
    assert!(
        findings.is_empty(),
        "schemas/{name} drifted from the serialized type:\n  {}",
        findings.join("\n  ")
    );
}

fn compare(root: &Value, schema: &Value, value: &Value, path: &str, findings: &mut Vec<String>) {
    // A local reference replaces the schema at this position.
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        match resolve(root, reference) {
            Some(target) => compare(root, target, value, path, findings),
            None => findings.push(format!("{path}: unresolved $ref {reference}")),
        }
        return;
    }

    if let Some(expected) = schema.get("const")
        && expected != value
    {
        findings.push(format!(
            "{path}: expected const {expected}, serialized {value}"
        ));
    }
    if let Some(Value::Array(members)) = schema.get("enum")
        && !members.contains(value)
    {
        findings.push(format!("{path}: {value} is not one of {members:?}"));
    }
    for keyword in ["oneOf", "anyOf"] {
        if let Some(options) = schema.get(keyword).and_then(Value::as_array) {
            let matched = options.iter().any(|option| {
                let mut probe = Vec::new();
                compare(root, option, value, path, &mut probe);
                probe.is_empty()
            });
            if !matched {
                findings.push(format!("{path}: {value} matches no {keyword} alternative"));
            }
        }
    }
    if let Some(expected) = schema.get("type")
        && !type_matches(expected, value)
    {
        findings.push(format!(
            "{path}: serialized {} where the schema declares {expected}",
            kind(value)
        ));
        return;
    }

    // `required` is independent of `properties`: a map-valued object declares
    // its mandatory keys without listing every one of them.
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        match value.as_object() {
            Some(object) => {
                for name in required.iter().filter_map(Value::as_str) {
                    if !object.contains_key(name) {
                        findings.push(format!(
                            "{path}.{name}: required by the schema, not serialized"
                        ));
                    }
                }
            }
            None => findings.push(format!(
                "{path}: the schema requires properties but {} was serialized",
                kind(value)
            )),
        }
    }

    if let Some(object) = value.as_object() {
        let properties = schema.get("properties").and_then(Value::as_object);
        let additional = schema.get("additionalProperties");
        let closed = additional == Some(&json!(false));
        for (name, child) in object {
            let child_path = format!("{path}.{name}");
            if let Some(child_schema) = properties.and_then(|properties| properties.get(name)) {
                compare(root, child_schema, child, &child_path, findings);
            } else if let Some(child_schema) = additional.filter(|schema| schema.is_object()) {
                compare(root, child_schema, child, &child_path, findings);
            } else if closed {
                findings.push(format!(
                    "{child_path}: serialized but absent from a closed schema"
                ));
            }
        }
    }

    if let Some(items) = schema.get("items")
        && let Some(entries) = value.as_array()
    {
        for (index, entry) in entries.iter().enumerate() {
            compare(root, items, entry, &format!("{path}[{index}]"), findings);
        }
    }
}

/// Resolves a same-document JSON pointer such as `#/$defs/capability`.
fn resolve<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix("#/")?;
    let mut node = root;
    for segment in pointer.split('/') {
        node = node.get(segment)?;
    }
    Some(node)
}

fn type_matches(expected: &Value, value: &Value) -> bool {
    match expected {
        Value::String(name) => matches_type_name(name, value),
        Value::Array(names) => names
            .iter()
            .filter_map(Value::as_str)
            .any(|name| matches_type_name(name, value)),
        _ => true,
    }
}

fn matches_type_name(name: &str, value: &Value) -> bool {
    match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn load_schema(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../schemas")
        .join(name);
    let bytes =
        std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_slice(&bytes).unwrap()
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/bench/gjc")
        .canonicalize()
        .unwrap()
}
