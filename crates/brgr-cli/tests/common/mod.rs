//! Shared fixtures for brgr-cli integration tests.
#![allow(dead_code)]

use std::{fs, path::Path};

/// A Herdr that records every call and answers the few pane-mode and handoff
/// tests make. `status-<pane>` files choose a pane's agent state (default
/// `idle`). `list.json` and per-pane screen files, when present, feed
/// `agent list` and `pane read`.
pub fn fake_herdr(dir: &Path) -> std::path::PathBuf {
    let script = dir.join("herdr");
    fs::write(
        &script,
        r#"#!/bin/bash
d="${FAKE_HERDR_DIR:-$(/usr/bin/dirname "$0")}"
echo "$@" >> "$d/calls.log"
if [ "$1" = "--session" ]; then shift 2; fi
case "$1 $2" in
  "pane split")
    echo '{"result":{"pane":{"pane_id":"p-new"}}}' ;;
  "pane get")
    echo "{\"result\":{\"pane\":{\"terminal_id\":\"term-$3\"}}}" ;;
  "agent list")
    if [ -f "$d/list.json" ]; then /bin/cat "$d/list.json"
    else echo '{"result":{"agents":[]}}'; fi ;;
  "agent get")
    if [ -f "$d/missing-$3" ]; then
      echo '{"error":{"code":"pane_not_found"}}' >&2
      exit 1
    fi
    s=idle
    [ -f "$d/status-$3" ] && s=$(/bin/cat "$d/status-$3")
    kind=claude
    [ -f "$d/kind-$3" ] && kind=$(/bin/cat "$d/kind-$3")
    echo "{\"result\":{\"agent\":{\"agent\":\"$kind\",\"pane_id\":\"$3\",\"agent_status\":\"$s\"}}}" ;;
  "pane layout") exit 1 ;;
  "pane read")
    f=$(printf %s "$3" | /usr/bin/sed 's/:/_/')
    if [ -f "$d/$f" ]; then /bin/cat "$d/$f"; fi ;;
  *) ;;
esac
exit 0
"#,
    )
    .unwrap();
    fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    script
}

pub fn herdr_env<'a>(fake: &'a Path, dir: &'a Path, pane: &'a str) -> Vec<(&'static str, String)> {
    vec![
        ("HERDR_ENV", "1".to_owned()),
        ("HERDR_PANE_ID", pane.to_owned()),
        ("HERDR_BIN_PATH", fake.to_str().unwrap().to_owned()),
        ("FAKE_HERDR_DIR", dir.to_str().unwrap().to_owned()),
    ]
}

pub fn calls(dir: &Path) -> String {
    fs::read_to_string(dir.join("calls.log")).unwrap_or_default()
}
