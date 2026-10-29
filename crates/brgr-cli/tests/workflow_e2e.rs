//! Black-box proof that a workflow script creates a later task from an observed result.

use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime},
};

use serde_json::Value;
use tempfile::TempDir;

fn brgr() -> &'static str {
    env!("CARGO_BIN_EXE_brgr")
}

const INHERITED: &[&str] = &[
    "BRGR_HOME",
    "BRGR_BIN",
    "BRGR_OWNER_ID",
    "BRGR_SESSION_ID",
    "BRGR_PARENT_TASK_ID",
    "BRGR_PARENT_ATTEMPT_ID",
    "BRGR_WORKER_HERDR_CONTEXT",
    "CODEX_THREAD_ID",
    "CLAUDE_CODE_SESSION_ID",
    "CURSOR_CONVERSATION_ID",
];

const HERDR: &[&str] = &[
    "HERDR_ENV",
    "HERDR_PANE_ID",
    "HERDR_TAB_ID",
    "HERDR_WORKSPACE_ID",
    "HERDR_BIN_PATH",
    "HERDR_SOCKET_PATH",
    "HERDR_SESSION",
    "HERDR_PLUGIN_ID",
];

const CRITERION_SCRIPT: &str = r#"
import { existsSync, writeFileSync } from "node:fs";
import { agent, parallel } from "./brgr-workflow.mjs";

const [reviewA, reviewB] = await parallel([
  () => agent({ task: "BRGR_FIXTURE_OK review-a", mode: "headless", key: "review-a" }),
  () => agent({ task: "BRGR_FIXTURE_OK review-b", mode: "headless", key: "review-b" }),
]);
writeFileSync(process.env.BRGR_HOME + "/ab-sealed", "1");
const pause = process.env.BRGR_HOME + "/pause-before-c";
while (existsSync(pause)) {
  await new Promise((resolve) => setTimeout(resolve, 50));
}
if (reviewA.report.includes("BRGR_FIXTURE_OK")) {
  await agent({ task: "BRGR_FIXTURE_OK follow-up", mode: "headless", key: "follow-up" });
}
"#;

const TYPEERROR_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const cases = [
  { task: undefined },
  { task: "BRGR_FIXTURE_OK", extra: NaN },
  { task: "BRGR_FIXTURE_OK", stamp: new Date() },
];
for (const options of cases) {
  try {
    await agent(options);
    throw new Error("expected TypeError");
  } catch (error) {
    if (!(error instanceof TypeError)) {
      throw error;
    }
  }
}
writeFileSync("typeerror-ok", "1");
"#;

const OVERLONG_TASK_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const limit = 262144;
const over = "x".repeat(limit + 1);
try {
  await agent({ task: over, mode: "headless", key: "overlong" });
  writeFileSync(process.env.BRGR_HOME + "/agent-error.txt", "agent() returned");
} catch (error) {
  if (!(error instanceof TypeError)) {
    throw error;
  }
  const message = String(error.message);
  if (message.includes(over) || message.includes("xxx")) {
    throw new Error("TypeError quoted the task: " + message.slice(0, 200));
  }
  if (!message.includes(String(limit))) {
    throw new Error("TypeError must name the limit: " + message);
  }
  writeFileSync(process.env.BRGR_HOME + "/agent-error.txt", message);
}
"#;

const EXACT_LIMIT_TASK_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const sealed = await agent({
  task: "x".repeat(262144),
  mode: "headless",
  key: "at-limit",
});
writeFileSync(process.env.BRGR_HOME + "/agent-result.json", JSON.stringify(sealed));
"#;

const FAILED_SCRIPT: &str = r#"
import { agent } from "./brgr-workflow.mjs";

const failed = await agent({ task: "FAIL", mode: "headless", key: "fail-step" });
if (failed.outcome !== "failed") {
  throw new Error("expected failed, got " + failed.outcome);
}
console.log("HANDLED_FAILED");
await agent({ task: "BRGR_FIXTURE_OK recovered", mode: "headless", key: "recovered" });
"#;

const QUESTION_SCRIPT: &str = r#"
import { agent } from "./brgr-workflow.mjs";
await agent({ task: "ASK which token", mode: "headless", key: "ask" });
"#;

const CAP_WHILE_BLOCKED_SCRIPT: &str = r#"
import { existsSync, writeFileSync } from "node:fs";
import { agent, parallel } from "./brgr-workflow.mjs";

await parallel([
  () => agent({ task: "ASK which token", mode: "headless", key: "ask" }),
  async () => {
    writeFileSync(process.env.BRGR_HOME + "/second-ready", "1");
    while (!existsSync(process.env.BRGR_HOME + "/release-second")) {
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    writeFileSync(process.env.BRGR_HOME + "/second-launching", "1");
    return agent({ task: "BRGR_FIXTURE_OK second", mode: "headless", key: "second" });
  },
]);
"#;

const DIVERGED_SCRIPT: &str = r#"
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { agent } from "./brgr-workflow.mjs";

const objective = readFileSync(
  join(dirname(fileURLToPath(import.meta.url)), "objective.txt"),
  "utf8",
).trim();
await agent({ task: objective, mode: "headless" });
await new Promise((resolve) => setTimeout(resolve, 60_000));
"#;

const DEADLINE_CANCEL_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const sealed = await agent({
  task: "SLOW keep going",
  mode: "headless",
  key: "slow",
  deadline: 2,
});
writeFileSync(process.env.BRGR_HOME + "/agent-result.json", JSON.stringify(sealed));
"#;

const DEADLINE_OK_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const sealed = await agent({
  task: "BRGR_FIXTURE_OK",
  mode: "headless",
  key: "fast",
  deadline: 60,
});
writeFileSync(process.env.BRGR_HOME + "/agent-result.json", JSON.stringify(sealed));
"#;

const INVALID_DEADLINE_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const cases = [0, -1, 1.5, "2", NaN];
for (const deadline of cases) {
  try {
    await agent({ task: "BRGR_FIXTURE_OK", mode: "headless", deadline });
    throw new Error("expected TypeError for " + String(deadline));
  } catch (error) {
    if (!(error instanceof TypeError)) {
      throw error;
    }
  }
}
writeFileSync("deadline-typeerror-ok", "1");
"#;

const DEADLINE_DIVERGED_SCRIPT: &str = r#"
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { agent } from "./brgr-workflow.mjs";

const deadline = Number(
  readFileSync(
    join(dirname(fileURLToPath(import.meta.url)), "deadline.txt"),
    "utf8",
  ).trim(),
);
await agent({ task: "BRGR_FIXTURE_OK", mode: "headless", deadline });
await new Promise((resolve) => setTimeout(resolve, 60_000));
"#;

const DEADLINE_RESUME_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const sealed = await agent({
  task: "HOLD until release",
  mode: "headless",
  key: "hold",
  deadline: 25,
});
writeFileSync(process.env.BRGR_HOME + "/agent-result.json", JSON.stringify(sealed));
"#;

const QUESTION_DEADLINE_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const sealed = await agent({
  task: "ASK which token",
  mode: "headless",
  key: "ask",
  deadline: 2,
});
writeFileSync(process.env.BRGR_HOME + "/agent-result.json", JSON.stringify(sealed));
"#;

const REVISE_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const sealed = await agent({ task: "BRGR_FIXTURE_OK first", mode: "headless", key: "revised" });
writeFileSync(process.env.BRGR_HOME + "/agent-result.json", JSON.stringify(sealed));
"#;

const DASH_TASK_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const sealed = await agent({
  task: "--workspace /etc",
  key: "--help",
  deadline: 60,
  mode: "headless",
});
writeFileSync(process.env.BRGR_HOME + "/agent-result.json", JSON.stringify(sealed));
"#;

const FUTURE_CLOCK_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const sealed = await agent({
  task: "BRGR_FIXTURE_OK",
  key: "future",
  deadline: 2,
  mode: "headless",
});
writeFileSync(process.env.BRGR_HOME + "/agent-result.json", JSON.stringify(sealed));
"#;

const ALREADY_CANCELLED_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const sealed = await agent({
  task: "HOLD until release",
  key: "late",
  deadline: 2,
  mode: "headless",
});
writeFileSync(process.env.BRGR_HOME + "/agent-result.json", JSON.stringify(sealed));
"#;

const OWNER_CANCEL_AFTER_DEADLINE_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const sealed = await agent({
  task: "HOLD until release",
  key: "late",
  deadline: 60,
  mode: "headless",
});
writeFileSync(process.env.BRGR_HOME + "/agent-result.json", JSON.stringify(sealed));
"#;

const DEADLINE_RETRY_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const sealed = await agent({
  task: "SLOW keep going",
  key: "slow",
  deadline: 2,
  retries: 2,
  mode: "headless",
});
writeFileSync(process.env.BRGR_HOME + "/agent-result.json", JSON.stringify(sealed));
"#;

const CANCELLED_RUN_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

try {
  await agent({ task: "queued", key: "queued", deadline: 2, mode: "headless" });
  writeFileSync(process.env.BRGR_HOME + "/agent-error.txt", "agent() returned");
} catch (error) {
  writeFileSync(process.env.BRGR_HOME + "/agent-error.txt", String(error && error.message));
}
"#;

const RETRY_ONCE_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const sealed = await agent({
  task: "retry once",
  mode: "headless",
  key: "retry-me",
  retries: 1,
});
writeFileSync(process.env.BRGR_HOME + "/agent-result.json", JSON.stringify(sealed));
"#;

const RETRY_EXHAUSTED_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const sealed = await agent({
  task: "FAIL always",
  mode: "headless",
  key: "always-fail",
  retries: 2,
});
writeFileSync(process.env.BRGR_HOME + "/agent-result.json", JSON.stringify(sealed));
"#;

const RETRY_ZERO_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const sealed = await agent({
  task: "FAIL",
  mode: "headless",
  key: "fail-step",
  retries: 0,
});
writeFileSync(process.env.BRGR_HOME + "/agent-result.json", JSON.stringify(sealed));
"#;

const RETRY_ABSENT_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const sealed = await agent({
  task: "FAIL",
  mode: "headless",
  key: "fail-step",
});
writeFileSync(process.env.BRGR_HOME + "/agent-result.json", JSON.stringify(sealed));
"#;

const RETRY_LOST_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const sealed = await agent({
  task: "lost step",
  mode: "headless",
  key: "lost-step",
  retries: 2,
});
writeFileSync(process.env.BRGR_HOME + "/agent-result.json", JSON.stringify(sealed));
"#;

const RETRY_CANCELLED_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const sealed = await agent({
  task: "cancelled step",
  mode: "headless",
  key: "cancelled-step",
  retries: 2,
});
writeFileSync(process.env.BRGR_HOME + "/agent-result.json", JSON.stringify(sealed));
"#;

const RETRY_DIVERGED_SCRIPT: &str = r#"
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { agent } from "./brgr-workflow.mjs";

const retries = Number(
  readFileSync(
    join(dirname(fileURLToPath(import.meta.url)), "retries.txt"),
    "utf8",
  ).trim(),
);
await agent({ task: "BRGR_FIXTURE_OK", mode: "headless", retries });
await new Promise((resolve) => setTimeout(resolve, 60_000));
"#;

const INVALID_RETRIES_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const cases = [4, -1, 1.5, "2", NaN];
for (const retries of cases) {
  try {
    await agent({ task: "BRGR_FIXTURE_OK", mode: "headless", retries });
    throw new Error("expected TypeError for " + String(retries));
  } catch (error) {
    if (!(error instanceof TypeError)) {
      throw error;
    }
  }
}
writeFileSync("retries-typeerror-ok", "1");
"#;

const RETRY_DASH_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const sealed = await agent({
  task: "--workspace /etc",
  key: "--help",
  retries: 1,
  mode: "headless",
});
writeFileSync(process.env.BRGR_HOME + "/agent-result.json", JSON.stringify(sealed));
"#;

const PIPELINE_PASS_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent, pipeline } from "./brgr-workflow.mjs";

const { results, stopped_at } = await pipeline([
  () => agent({ task: "BRGR_FIXTURE_OK first-stage", mode: "headless" }),
  (prev) => agent({ task: "BRGR_FIXTURE_OK uses " + prev.report, mode: "headless" }),
  (prev) => agent({ task: "BRGR_FIXTURE_OK third " + prev.report, mode: "headless" }),
]);
writeFileSync(
  process.env.BRGR_HOME + "/pipeline-result.json",
  JSON.stringify({ results, stopped_at }),
);
"#;

const PIPELINE_FAIL_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent, pipeline } from "./brgr-workflow.mjs";

const { results, stopped_at } = await pipeline([
  () => agent({ task: "BRGR_FIXTURE_OK first-stage", mode: "headless" }),
  () => agent({ task: "FAIL middle", mode: "headless" }),
  () => agent({ task: "BRGR_FIXTURE_OK third-stage", mode: "headless" }),
]);
writeFileSync(
  process.env.BRGR_HOME + "/pipeline-result.json",
  JSON.stringify({ results, stopped_at }),
);
"#;

const PIPELINE_RESUME_SCRIPT: &str = r#"
import { appendFileSync, existsSync, writeFileSync } from "node:fs";
import { agent, pipeline } from "./brgr-workflow.mjs";

const home = process.env.BRGR_HOME;
function note(stage) {
  appendFileSync(home + "/stage-calls", stage + "\n");
}

await pipeline([
  async () => {
    note("0");
    const sealed = await agent({ task: "BRGR_FIXTURE_OK first", mode: "headless" });
    writeFileSync(home + "/stage0-done", "1");
    return sealed;
  },
  async (prev) => {
    note("1");
    while (existsSync(home + "/pause-before-1")) {
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    return agent({ task: "BRGR_FIXTURE_OK second " + prev.report, mode: "headless" });
  },
  () => {
    note("2");
    return agent({ task: "BRGR_FIXTURE_OK third", mode: "headless" });
  },
]);
"#;

const PIPELINE_INVALID_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { pipeline } from "./brgr-workflow.mjs";

const tooMany = Array.from({ length: 65 }, () => () => {});
const cases = [null, "nope", [], [() => {}, "x"], tooMany, undefined];
for (const stages of cases) {
  try {
    await pipeline(stages);
    throw new Error("expected TypeError for " + String(stages));
  } catch (error) {
    if (!(error instanceof TypeError)) {
      throw error;
    }
  }
}
try {
  await pipeline([() => {}], "prefix");
  throw new Error("expected TypeError for options");
} catch (error) {
  if (!(error instanceof TypeError)) {
    throw error;
  }
}
writeFileSync("pipeline-typeerror-ok", "1");
"#;

const PIPELINE_THROW_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent, pipeline } from "./brgr-workflow.mjs";

try {
  await pipeline([
    () => agent({ task: "BRGR_FIXTURE_OK first", mode: "headless" }),
    () => {
      throw new Error("stage-boom");
    },
    () => agent({ task: "BRGR_FIXTURE_OK third", mode: "headless" }),
  ]);
  throw new Error("pipeline should have rejected");
} catch (error) {
  writeFileSync(
    process.env.BRGR_HOME + "/pipeline-throw.txt",
    String(error && error.message),
  );
  throw error;
}
"#;

const PIPELINE_PARALLEL_TYPEERROR_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent, parallel, pipeline } from "./brgr-workflow.mjs";

try {
  await parallel([
    () => pipeline([() => agent({ task: "BRGR_FIXTURE_OK", mode: "headless" })]),
  ]);
  throw new Error("expected TypeError");
} catch (error) {
  if (!(error instanceof TypeError)) {
    throw error;
  }
}
writeFileSync("pipeline-parallel-typeerror-ok", "1");
"#;

const PIPELINE_DASH_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent, pipeline } from "./brgr-workflow.mjs";

await pipeline([
  () => agent({ task: "first", mode: "headless" }),
  (prev) =>
    agent({
      task: prev.report,
      deadline: 60,
      retries: 1,
      mode: "headless",
    }),
]);
writeFileSync(process.env.BRGR_HOME + "/pipeline-dash-ok", "1");
"#;

const RESULT_FIELDS_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent } from "./brgr-workflow.mjs";

const sealed = await agent({ task: "BRGR_FIXTURE_OK fields", mode: "headless", key: "fields" });
writeFileSync(process.env.BRGR_HOME + "/agent-result.json", JSON.stringify(sealed));
"#;

const PIPELINE_PARALLEL_KEYS_SCRIPT: &str = r#"
import { writeFileSync } from "node:fs";
import { agent, parallel, pipeline } from "./brgr-workflow.mjs";

await parallel([
  () =>
    pipeline(
      [
        () => agent({ task: "BRGR_FIXTURE_OK left-0", mode: "headless" }),
        () => agent({ task: "BRGR_FIXTURE_OK left-1", mode: "headless" }),
      ],
      { key: "left" },
    ),
  () =>
    pipeline(
      [
        () => agent({ task: "BRGR_FIXTURE_OK right-0", mode: "headless" }),
        () => agent({ task: "BRGR_FIXTURE_OK right-1", mode: "headless" }),
      ],
      { key: "right" },
    ),
]);
writeFileSync(process.env.BRGR_HOME + "/pipeline-parallel-ok", "1");
"#;

const HOLD_REV2_GJC: &str = r#"#!/bin/sh
set -eu
case "${1:-}" in
  --version) printf 'gjc v-hold-rev2\n'; exit 0;;
  --help) printf '%s\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;
esac
prompt_file=
for argument in "$@"; do
  case "$argument" in @*) prompt_file=${argument#@};; esac
done
if [ -n "$prompt_file" ] && /usr/bin/grep -q REV2 "$prompt_file"; then
  n=0
  while [ ! -e "$BRGR_HOME/release-rev2" ] && [ "$n" -lt 600 ]; do
    /bin/sleep 0.1
    n=$((n + 1))
  done
  printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"REV2_SEALED"}]}}'
else
  printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"REV1_SEALED"}]}}'
fi
printf '%s\n' '{"type":"agent_end","stopReason":"completed"}'
"#;

const FAILING_GJC: &str = r#"#!/bin/sh
set -eu
case "${1:-}" in
  --version) printf 'gjc v-fail-fixture\n'; exit 0;;
  --help) printf '%s\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;
esac
prompt_file=
for argument in "$@"; do
  case "$argument" in @*) prompt_file=${argument#@};; esac
done
if [ -n "$prompt_file" ] && /usr/bin/grep -q BRGR_FIXTURE_OK "$prompt_file"; then
  printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"BRGR_FIXTURE_OK"}]}}'
  printf '%s\n' '{"type":"agent_end","stopReason":"completed"}'
  exit 0
fi
exit 7
"#;

const FAIL_ONCE_GJC: &str = r#"#!/bin/sh
set -eu
case "${1:-}" in
  --version) printf 'gjc v-fail-once\n'; exit 0;;
  --help) printf '%s\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;
esac
count_file="${BRGR_HOME:-}/step-attempts"
if [ -z "${BRGR_HOME:-}" ] || [ ! -f "$count_file" ]; then
  printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"BRGR_FIXTURE_OK"}]}}'
  printf '%s\n' '{"type":"agent_end","stopReason":"completed"}'
  exit 0
fi
n=$(cat "$count_file")
n=$((n + 1))
printf '%s\n' "$n" > "$count_file"
if [ "$n" -eq 1 ]; then
  exit 7
fi
if [ -f "${BRGR_HOME}/hold-retries" ]; then
  n=0
  while [ ! -e "${BRGR_HOME}/release-hold" ] && [ "$n" -lt 6000 ]; do
    /bin/sleep 0.1
    n=$((n + 1))
  done
fi
printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"BRGR_FIXTURE_OK"}]}}'
printf '%s\n' '{"type":"agent_end","stopReason":"completed"}'
"#;

const ASKING_GJC: &str = r#"#!/bin/sh
set -u
case "${1:-}" in
  --version) echo 'gjc v-asking-fixture'; exit 0;;
  --help) printf '%s\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;
esac
prompt_file=
for argument in "$@"; do
  case "$argument" in @*) prompt_file=${argument#@};; esac
done
if test -n "${BRGR_PARENT_ATTEMPT_ID:-}" && /usr/bin/grep -q ASK "$prompt_file"; then
  "$BRGR_BIN" --json message send "$BRGR_PARENT_TASK_ID" --to owner --kind question --body 'Which token?' >/dev/null
  reply=$("$BRGR_BIN" --json message wait "$BRGR_PARENT_TASK_ID" --for worker --timeout-seconds 60)
  reply_id=$(printf '%s\n' "$reply" | /usr/bin/sed -n 's/.*"message_id":"\([^"]*\)".*/\1/p')
  "$BRGR_BIN" --json message ack "$BRGR_PARENT_TASK_ID" "$reply_id" --for worker >/dev/null
  answer=$(printf '%s\n' "$reply" | /usr/bin/sed -n 's/.*"body":"\([^"]*\)".*/\1/p')
else
  answer=BRGR_FIXTURE_OK
fi
printf '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"%s"}]}}\n' "$answer"
printf '%s\n' '{"type":"agent_end","stopReason":"completed"}'
"#;

const HOLD_GJC: &str = r#"#!/bin/sh
set -eu
case "${1:-}" in
  --version) printf 'gjc v-hold-fixture\n'; exit 0;;
  --help) printf '%s\n' '-p, --print' '--mode=<value>' '--no-session' '--no-mcp' '--model' '--thinking'; exit 0;;
esac
prompt_file=
for argument in "$@"; do
  case "$argument" in @*) prompt_file=${argument#@};; esac
done
if [ -n "$prompt_file" ] && /usr/bin/grep -q HOLD "$prompt_file"; then
  n=0
  while [ ! -e "$BRGR_HOME/release-hold" ] && [ "$n" -lt 6000 ]; do
    /bin/sleep 0.1
    n=$((n + 1))
  done
fi
printf '%s\n' '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"BRGR_FIXTURE_OK"}]}}'
printf '%s\n' '{"type":"agent_end","stopReason":"completed"}'
"#;

fn require_node() {
    let output = Command::new("node")
        .arg("-v")
        .output()
        .expect("node must be executable on PATH");
    assert!(
        output.status.success(),
        "node -v failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn brgr_command() -> Command {
    let mut command = Command::new(brgr());
    command.env("BRGR_TEST_TRUST_HERDR_PANE", "1");
    for name in INHERITED {
        command.env_remove(name);
    }
    command
}

fn run(
    home: &Path,
    workspace: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
) -> std::process::Output {
    let mut command = brgr_command();
    command
        .current_dir(workspace)
        .arg("--home")
        .arg(home)
        .arg("--json")
        .args(args);
    if matches!(args.first(), Some(&"run" | &"revise")) {
        command.arg("--headless");
    }
    command.env_remove("CODEX_THREAD_ID");
    command.env_remove("CLAUDE_CODE_SESSION_ID");
    command.env_remove("CURSOR_CONVERSATION_ID");
    for name in HERDR {
        command.env_remove(name);
    }
    if !envs.iter().any(|(name, _)| {
        matches!(
            *name,
            "BRGR_SESSION_ID" | "CLAUDE_CODE_SESSION_ID" | "CURSOR_CONVERSATION_ID"
        )
    }) {
        command.env("BRGR_SESSION_ID", "fixture-session");
    }
    for (name, value) in envs {
        command.env(name, value);
    }
    command.output().unwrap()
}

fn json_output(output: &std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn seed_git_repo(repository: &Path) {
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.name", "Fixture"]);
    git(&["config", "user.email", "fixture@example.invalid"]);
    fs::write(repository.join("README"), b"seed\n").unwrap();
    git(&["add", "README"]);
    git(&["commit", "-m", "seed"]);
}

fn add_harness(home: &Path, executable: &Path, scratch: &Path) {
    fs::create_dir_all(scratch).unwrap();
    let receipt = json_output(&run(
        home,
        scratch,
        &[
            "harness",
            "add",
            executable.to_str().unwrap(),
            "--workspace",
            scratch.to_str().unwrap(),
            "--prompt",
            "BRGR_FIXTURE_OK",
        ],
        &[],
    ));
    assert!(receipt["scratch_result_digest"].as_str().is_some());
}

struct Fixture {
    _temp: TempDir,
    home: PathBuf,
    workspace: PathBuf,
    scripts: PathBuf,
}

fn fixture_from_gjc(executable: &Path) -> Fixture {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    let home = root.join("brgr");
    let workspace = root.join("repo");
    let scripts = root.join("scripts");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&scripts).unwrap();
    seed_git_repo(&workspace);
    add_harness(&home, executable, &root.join("scratch"));
    Fixture {
        _temp: temp,
        home,
        workspace,
        scripts,
    }
}

fn default_gjc() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures/gjc")
        .canonicalize()
        .unwrap()
}

fn fixture_home() -> Fixture {
    fixture_from_gjc(&default_gjc())
}

fn fixture_with_script(source: &str) -> Fixture {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    let home = root.join("brgr");
    let workspace = root.join("repo");
    let scripts = root.join("scripts");
    let executable = root.join("gjc");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&scripts).unwrap();
    seed_git_repo(&workspace);
    fs::write(&executable, source).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    add_harness(&home, &executable, &root.join("scratch"));
    Fixture {
        _temp: temp,
        home,
        workspace,
        scripts,
    }
}

fn owner_env() -> [(&'static str, &'static str); 2] {
    [
        ("BRGR_OWNER_ID", "codex:workflow-owner"),
        ("BRGR_SESSION_ID", "workflow-session"),
    ]
}

fn wait_until(timeout: Duration, ready: impl FnMut() -> bool) -> bool {
    wait_until_every(timeout, Duration::from_millis(100), ready)
}

fn wait_until_every(timeout: Duration, every: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if ready() {
            return true;
        }
        thread::sleep(every);
    }
    false
}

fn wait_run_state(fixture: &Fixture, run_id: &str, want: &str, timeout: Duration) -> Value {
    let started = Instant::now();
    let envs = owner_env();
    loop {
        let output = run(
            &fixture.home,
            &fixture.workspace,
            &["workflow", "status", run_id],
            &envs,
        );
        if output.status.success() {
            let value = serde_json::from_slice::<Value>(&output.stdout).unwrap();
            if value["state"].as_str() == Some(want) {
                return value;
            }
        }
        assert!(
            started.elapsed() <= timeout,
            "run {run_id} did not reach {want}: status={} stderr={} log={}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
            workflow_log(fixture, run_id)
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn init_sdk(fixture: &Fixture) {
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "init", fixture.scripts.to_str().unwrap()],
        &[],
    ));
    assert!(fixture.scripts.join("brgr-workflow.mjs").is_file());
}

fn write_script(fixture: &Fixture, name: &str, source: &str) -> PathBuf {
    let path = fixture.scripts.join(name);
    fs::write(&path, source).unwrap();
    path
}

fn start_workflow(fixture: &Fixture, script: &Path) -> String {
    start_workflow_with(fixture, script, &[])
}

fn start_workflow_with(fixture: &Fixture, script: &Path, extra: &[&str]) -> String {
    let script = script.to_str().unwrap();
    let mut args = vec!["workflow", "run", script];
    args.extend_from_slice(extra);
    json_output(&run(&fixture.home, &fixture.workspace, &args, &owner_env()))["run_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn wait_for_step_running(fixture: &Fixture, run_id: &str, task: &str) {
    assert!(
        wait_until(Duration::from_secs(90), || {
            let status = run(
                &fixture.home,
                &fixture.workspace,
                &["status", task],
                &owner_env(),
            );
            status.status.success()
                && serde_json::from_slice::<Value>(&status.stdout)
                    .ok()
                    .is_some_and(|value| value["state"].as_str() == Some("running"))
        }),
        "step {task} never started running: {}",
        dump_run(fixture, run_id)
    );
}

fn wait_for_first_step(fixture: &Fixture, run_id: &str) -> String {
    let mut task_id = String::new();
    assert!(
        wait_until(Duration::from_secs(90), || {
            let status = run(
                &fixture.home,
                &fixture.workspace,
                &["workflow", "status", run_id],
                &owner_env(),
            );
            let Some(value) = status
                .status
                .success()
                .then(|| serde_json::from_slice::<Value>(&status.stdout).ok())
                .flatten()
            else {
                return false;
            };
            let Some(task) = value["steps"]
                .as_array()
                .and_then(|steps| steps.first())
                .and_then(|step| step["task_id"].as_str())
            else {
                return false;
            };
            task.clone_into(&mut task_id);
            true
        }),
        "first step was never journaled: {}",
        dump_run(fixture, run_id)
    );
    task_id
}

fn create_workflow(fixture: &Fixture, extra: &[&str]) -> String {
    let mut args = vec!["workflow", "create"];
    args.extend_from_slice(extra);
    json_output(&run(&fixture.home, &fixture.workspace, &args, &owner_env()))["run_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn write_wait_gate(fixture: &Fixture) -> PathBuf {
    let path = fixture.scripts.join("wait-gate-brgr");
    fs::write(
        &path,
        format!(
            r#"#!/bin/sh
set -eu
real="{real}"
home="{home}"
count="$home/wait-count"
first_wait="$home/first-wait.json"
first_result="$home/first-result.json"
if [ "${{1:-}}" = "--json" ] && [ "${{2:-}}" = "wait" ]; then
  n=0
  if [ -f "$count" ]; then
    n=$(cat "$count")
  fi
  n=$((n + 1))
  printf '%s\n' "$n" > "$count"
  if [ "$n" -eq 1 ]; then
    "$real" "$@" > "$first_wait"
    task=""
    for argument in "$@"; do
      task="$argument"
    done
    "$real" --json result -- "$task" > "$first_result"
    printf '1\n' > "$home/wait-returned"
    while [ ! -e "$home/allow-wait-return" ]; do
      /bin/sleep 0.05
    done
    cat "$first_wait"
    exit 0
  fi
fi
if [ "${{1:-}}" = "--json" ] && [ "${{2:-}}" = "result" ]; then
  n=0
  if [ -f "$count" ]; then
    n=$(cat "$count")
  fi
  if [ "$n" -eq 1 ] && [ -f "$first_result" ]; then
    cat "$first_result"
    exit 0
  fi
fi
exec "$real" "$@"
"#,
            real = brgr(),
            home = fixture.home.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn spawn_agent_script(
    fixture: &Fixture,
    script: &Path,
    run_id: &str,
    brgr_bin: &Path,
) -> std::process::Child {
    let mut command = Command::new("node");
    command
        .arg(script)
        .current_dir(&fixture.workspace)
        .env("BRGR_BIN", brgr_bin)
        .env("BRGR_HOME", &fixture.home)
        .env("BRGR_WORKFLOW_RUN", run_id)
        .env("BRGR_OWNER_ID", "codex:workflow-owner")
        .env("BRGR_SESSION_ID", "workflow-session")
        .env_remove("BRGR_PARENT_TASK_ID")
        .env_remove("BRGR_PARENT_ATTEMPT_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CURSOR_CONVERSATION_ID")
        .env("BRGR_TEST_TRUST_HERDR_PANE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in HERDR {
        command.env_remove(name);
    }
    command.spawn().unwrap()
}

fn reject_and_revise(fixture: &Fixture, task: &str) {
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["reject", task, "--reason", "revise the step"],
        &owner_env(),
    ));
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &[
            "revise",
            task,
            "BRGR_FIXTURE_OK REV2",
            "--workspace",
            fixture.workspace.to_str().unwrap(),
        ],
        &owner_env(),
    ));
}

fn launch_workflow_step(
    fixture: &Fixture,
    run_id: &str,
    step: &str,
    objective: &str,
) -> std::process::Output {
    launch_workflow_step_with(fixture, run_id, step, objective, &[])
}

fn launch_workflow_step_with(
    fixture: &Fixture,
    run_id: &str,
    step: &str,
    objective: &str,
    extra: &[&str],
) -> std::process::Output {
    let workspace = fixture.workspace.to_str().unwrap();
    let mut args = vec![
        "run",
        objective,
        "--workspace",
        workspace,
        "--workflow",
        run_id,
        "--step",
        step,
    ];
    args.extend_from_slice(extra);
    run(&fixture.home, &fixture.workspace, &args, &owner_env())
}

fn wait_for_n_steps(fixture: &Fixture, run_id: &str, n: usize) -> Value {
    let mut listed = Value::Null;
    assert!(
        wait_until(Duration::from_secs(90), || {
            let status = run(
                &fixture.home,
                &fixture.workspace,
                &["workflow", "status", run_id],
                &owner_env(),
            );
            let Some(value) = status
                .status
                .success()
                .then(|| serde_json::from_slice::<Value>(&status.stdout).ok())
                .flatten()
            else {
                return false;
            };
            if value["steps"]
                .as_array()
                .is_some_and(|steps| steps.len() == n)
            {
                listed = value;
                return true;
            }
            false
        }),
        "did not reach {n} journaled steps: {}",
        dump_run(fixture, run_id)
    );
    listed
}

fn step_digest(fixture: &Fixture, run_id: &str, key: &str) -> String {
    let connection =
        rusqlite::Connection::open(fixture.home.join("store").join("brgr.sqlite3")).unwrap();
    connection
        .query_row(
            "SELECT input_digest FROM workflow_steps WHERE run_id = ?1 AND step_key = ?2",
            rusqlite::params![run_id, key],
            |row| row.get(0),
        )
        .unwrap()
}

fn enable_fail_once(fixture: &Fixture) {
    fs::write(fixture.home.join("step-attempts"), b"0").unwrap();
}

fn fake_terminal_body(outcome: &str) -> String {
    format!(
        r#"
task="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
rid="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
now=$(/bin/date +%s)
case " $* " in
  *" run "*) printf '{{"task_id":"%s"}}\n' "$task";;
  *" workflow status "*) printf '{{"state":"running"}}\n';;
  *" status "*) printf '{{"task":{{"revision":1}},"state":"terminal","created_at":%s}}\n' "$now";;
  *" wait "*) printf '{{"task_id":"%s","result_id":"%s","outcome":"{outcome}","revision":1}}\n' "$task" "$rid";;
  *" result "*) printf '{{"result":{{"outcome":"{outcome}","revision":1,"result_id":"%s"}},"artifacts":[]}}\n' "$rid";;
  *) printf '{{"ok":true}}\n';;
esac
"#
    )
}

fn agent_result(fixture: &Fixture) -> Value {
    json_home_file(fixture, "agent-result.json")
}

fn json_home_file(fixture: &Fixture, name: &str) -> Value {
    let path = fixture.home.join(name);
    wait_for_file(&path, Duration::from_secs(90));
    serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap()
}

fn task_objective(fixture: &Fixture, task: &str) -> String {
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["status", task],
        &owner_env(),
    ))["task"]["objective"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn declared_interface_fields(source: &str, name: &str) -> Vec<String> {
    let header = format!("export interface {name} {{");
    let start = source
        .find(&header)
        .unwrap_or_else(|| panic!("missing interface {name}"));
    let body = &source[start + header.len()..];
    let end = body.find('}').unwrap();
    body[..end]
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('/') || line.starts_with('*') {
                return None;
            }
            let field = line.split(':').next()?.trim();
            (!field.is_empty()).then(|| field.to_owned())
        })
        .collect()
}

fn release_revision_two(fixture: &Fixture) {
    fs::write(fixture.home.join("release-rev2"), b"1").unwrap();
}

fn step_keys(status: &Value) -> Vec<String> {
    status["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["step_key"].as_str().unwrap().to_owned())
        .collect()
}

fn notice_states(status: &Value) -> Vec<String> {
    status["notices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|notice| notice["state"].as_str().unwrap().to_owned())
        .collect()
}

fn assert_reviews_then_follow_up(status: &Value) {
    let keys = step_keys(status);
    assert_eq!(keys.len(), 3, "{status}");
    assert!(
        keys[..2].contains(&"review-a".to_owned()) && keys[..2].contains(&"review-b".to_owned()),
        "reviews were not journaled before follow-up: {status}"
    );
    assert_eq!(
        keys[2], "follow-up",
        "follow-up was not created after both reviews sealed: {status}"
    );
}

fn workflow_log(fixture: &Fixture, run_id: &str) -> String {
    fs::read_to_string(
        fixture
            .home
            .join("runs")
            .join(format!("{run_id}.workflow.log")),
    )
    .unwrap_or_default()
}

fn kill_driver(fixture: &Fixture, run_id: &str) {
    let pid_path = fixture
        .home
        .join("runs")
        .join(format!("{run_id}.workflow.pid"));
    assert!(
        wait_until(Duration::from_secs(5), || pid_path.is_file()),
        "driver pid file missing"
    );
    let pid: u32 = fs::read_to_string(&pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let killed = Command::new("/bin/kill")
        .args(["-KILL", &format!("-{pid}")])
        .status()
        .unwrap();
    assert!(killed.success(), "failed to kill driver group {pid}");
    assert!(
        wait_until(Duration::from_secs(5), || {
            Command::new("/bin/kill")
                .args(["-0", &pid.to_string()])
                .status()
                .is_ok_and(|status| !status.success())
        }),
        "driver {pid} still alive"
    );
}

fn wait_for_file(path: &Path, timeout: Duration) {
    assert!(
        wait_until(timeout, || path.is_file()),
        "{} was never written",
        path.display()
    );
}

fn dump_run(fixture: &Fixture, run_id: &str) -> String {
    let status = run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "status", run_id],
        &owner_env(),
    );
    format!(
        "status_ok={} stdout={} stderr={} log={}",
        status.status.success(),
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr),
        workflow_log(fixture, run_id)
    )
}

fn launch_file(fixture: &Fixture, task: &str) -> PathBuf {
    fixture.home.join("launches").join(format!("{task}.json"))
}

fn set_task_attempt_clocks(fixture: &Fixture, task: &str, started_at: i64) {
    let connection =
        rusqlite::Connection::open(fixture.home.join("store").join("brgr.sqlite3")).unwrap();
    let updated = connection
        .execute(
            "UPDATE attempt_clocks SET started_at = ?1
             WHERE attempt_id IN (SELECT attempt_id FROM attempts WHERE task_id = ?2)",
            rusqlite::params![started_at, task],
        )
        .unwrap();
    assert!(
        updated >= 1,
        "no attempt clock for {task} in {}",
        fixture.home.display()
    );
}

fn status_created_at(fixture: &Fixture, task: &str) -> Value {
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["status", task],
        &owner_env(),
    ))
}

fn wait_until_running(fixture: &Fixture, task: &str) {
    assert!(
        wait_until(Duration::from_secs(90), || {
            let output = run(
                &fixture.home,
                &fixture.workspace,
                &["status", task],
                &owner_env(),
            );
            output.status.success()
                && serde_json::from_slice::<Value>(&output.stdout)
                    .ok()
                    .is_some_and(|value| value["state"].as_str() == Some("running"))
        }),
        "task {task} never started running"
    );
}

#[test]
fn a_follow_up_task_is_created_only_from_an_observed_report() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let script = write_script(&fixture, "criterion.mjs", CRITERION_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(90));
    assert_reviews_then_follow_up(&status);
    let notices = notice_states(&status);
    assert_eq!(notices, vec!["completed"], "{status}");
}

#[test]
fn resume_after_the_driver_dies_still_creates_exactly_three_tasks() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    fs::write(fixture.home.join("pause-before-c"), b"1").unwrap();
    let script = write_script(&fixture, "criterion.mjs", CRITERION_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    wait_for_file(&fixture.home.join("ab-sealed"), Duration::from_secs(90));
    let before = json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "status", &run_id],
        &owner_env(),
    ));
    let before_keys = step_keys(&before);
    assert_eq!(before_keys.len(), 2, "{before}");
    assert!(
        before_keys.contains(&"review-a".to_owned())
            && before_keys.contains(&"review-b".to_owned()),
        "{before}"
    );
    kill_driver(&fixture, &run_id);
    fs::remove_file(fixture.home.join("pause-before-c")).unwrap();
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "resume", &run_id],
        &owner_env(),
    ));
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(90));
    assert_reviews_then_follow_up(&status);
}

#[test]
fn a_question_blocks_the_run_until_the_owner_replies() {
    require_node();
    let fixture = fixture_with_script(ASKING_GJC);
    init_sdk(&fixture);
    let script = write_script(&fixture, "ask.mjs", QUESTION_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    let blocked = wait_run_state(&fixture, &run_id, "blocked", Duration::from_secs(90));
    let blocked_notices = notice_states(&blocked)
        .into_iter()
        .filter(|state| state == "blocked")
        .count();
    assert_eq!(blocked_notices, 1, "{blocked}");
    let task = blocked["steps"][0]["task_id"].as_str().unwrap().to_owned();
    reply_to_worker_question(&fixture, &task, "token-42");
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(90));
    assert_eq!(
        notice_states(&status)
            .iter()
            .filter(|state| *state == "blocked")
            .count(),
        1,
        "{status}"
    );
}

#[test]
fn a_launch_that_hits_the_parallel_cap_while_blocked_keeps_waiting() {
    require_node();
    let fixture = fixture_with_script(ASKING_GJC);
    init_sdk(&fixture);
    let script = write_script(&fixture, "cap-blocked.mjs", CAP_WHILE_BLOCKED_SCRIPT);
    let run_id = start_workflow_with(&fixture, &script, &["--max-parallel", "1"]);
    let blocked = wait_run_state(&fixture, &run_id, "blocked", Duration::from_secs(90));
    wait_for_file(&fixture.home.join("second-ready"), Duration::from_secs(90));
    fs::write(fixture.home.join("release-second"), b"1").unwrap();
    wait_for_file(
        &fixture.home.join("second-launching"),
        Duration::from_secs(90),
    );
    let ask = blocked["steps"][0]["task_id"].as_str().unwrap().to_owned();
    assert_eq!(step_keys(&blocked), vec!["ask"], "{blocked}");
    reply_to_worker_question(&fixture, &ask, "token-42");
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(90));
    let mut keys = step_keys(&status);
    keys.sort();
    assert_eq!(keys, vec!["ask", "second"], "{status}");
}

#[test]
fn a_failed_worker_is_returned_and_the_script_branches_on_it() {
    require_node();
    let fixture = fixture_with_script(FAILING_GJC);
    init_sdk(&fixture);
    let script = write_script(&fixture, "fail.mjs", FAILED_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(90));
    assert_eq!(
        step_keys(&status),
        vec!["fail-step", "recovered"],
        "{status}"
    );
    let log = workflow_log(&fixture, &run_id);
    assert!(
        log.contains("HANDLED_FAILED"),
        "script did not branch: {log}"
    );
}

#[test]
fn invalid_agent_options_throw_before_launching_a_task() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let script = write_script(&fixture, "types.mjs", TYPEERROR_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(30));
    assert!(
        step_keys(&status).is_empty(),
        "invalid options launched a task: {status}"
    );
    assert!(
        fixture.workspace.join("typeerror-ok").is_file(),
        "script did not record TypeError coverage"
    );
}

#[test]
fn a_changed_objective_under_an_automatic_key_diverges_and_launches_nothing_new() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    fs::write(
        fixture.scripts.join("objective.txt"),
        "BRGR_FIXTURE_OK first\n",
    )
    .unwrap();
    let script = write_script(&fixture, "diverge.mjs", DIVERGED_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    assert!(
        wait_until(Duration::from_secs(90), || {
            let status = run(
                &fixture.home,
                &fixture.workspace,
                &["workflow", "status", &run_id],
                &owner_env(),
            );
            status.status.success()
                && serde_json::from_slice::<Value>(&status.stdout)
                    .ok()
                    .is_some_and(|value| {
                        value["steps"]
                            .as_array()
                            .is_some_and(|steps| !steps.is_empty())
                    })
        }),
        "first step was never journaled: {}",
        dump_run(&fixture, &run_id)
    );
    kill_driver(&fixture, &run_id);
    fs::write(
        fixture.scripts.join("objective.txt"),
        "BRGR_FIXTURE_OK changed\n",
    )
    .unwrap();
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "resume", &run_id],
        &owner_env(),
    ));
    let status = wait_run_state(&fixture, &run_id, "failed", Duration::from_secs(30));
    assert_eq!(
        status["steps"].as_array().map(Vec::len),
        Some(1),
        "{status}"
    );
    let log = workflow_log(&fixture, &run_id);
    assert!(log.contains("diverged"), "resume did not diverge: {log}");
}

#[test]
fn a_script_receives_the_newest_revision_when_the_owner_revises_a_sealed_step() {
    require_node();
    let fixture = fixture_with_script(HOLD_REV2_GJC);
    init_sdk(&fixture);
    let script = write_script(&fixture, "revise.mjs", REVISE_SCRIPT);
    let run_id = create_workflow(&fixture, &["--max-parallel", "1"]);
    let gate = write_wait_gate(&fixture);
    let mut child = spawn_agent_script(&fixture, &script, &run_id, &gate);
    wait_for_file(&fixture.home.join("wait-returned"), Duration::from_secs(90));
    let task = wait_for_first_step(&fixture, &run_id);
    reject_and_revise(&fixture, &task);

    let listed = json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "status", &run_id],
        &owner_env(),
    ));
    assert_eq!(listed["state"], "running", "{listed}");
    assert_eq!(listed["steps"][0]["revision"], 1, "{listed}");

    let probe = launch_workflow_step(&fixture, &run_id, "probe", "BRGR_FIXTURE_OK probe");
    assert_eq!(
        probe.status.code(),
        Some(75),
        "a revised step without a newest result freed its parallel slot: status={} stderr={}",
        probe.status,
        String::from_utf8_lossy(&probe.stderr)
    );
    assert!(
        !fixture.home.join("agent-result.json").is_file(),
        "the script received a sealed result before revision 2 finished"
    );

    fs::write(fixture.home.join("allow-wait-return"), b"1").unwrap();
    release_revision_two(&fixture);
    let sealed = agent_result(&fixture);
    let status = child.wait().unwrap();
    assert!(status.success(), "agent script failed: {status}");
    assert_eq!(sealed["revision"], 2, "{sealed}");
    assert_eq!(sealed["outcome"], "completed", "{sealed}");
    assert!(
        sealed["report"]
            .as_str()
            .is_some_and(|text| text.contains("REV2_SEALED")),
        "{sealed}"
    );
}

#[test]
fn resume_after_a_revise_returns_the_newest_revision_result() {
    require_node();
    let fixture = fixture_with_script(HOLD_REV2_GJC);
    init_sdk(&fixture);
    let script = write_script(&fixture, "revise-resume.mjs", REVISE_SCRIPT);
    let run_id = create_workflow(&fixture, &[]);
    let gate = write_wait_gate(&fixture);
    let mut first = spawn_agent_script(&fixture, &script, &run_id, &gate);
    wait_for_file(&fixture.home.join("wait-returned"), Duration::from_secs(90));
    let task = wait_for_first_step(&fixture, &run_id);
    reject_and_revise(&fixture, &task);
    assert!(
        !fixture.home.join("agent-result.json").is_file(),
        "the script received a result before resume"
    );
    let _ = first.kill();
    let _ = first.wait();
    let mut resumed = spawn_agent_script(&fixture, &script, &run_id, Path::new(brgr()));
    release_revision_two(&fixture);
    let sealed = agent_result(&fixture);
    let status = resumed.wait().unwrap();
    assert!(status.success(), "resumed script failed: {status}");
    let listed = json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "status", &run_id],
        &owner_env(),
    ));
    assert_eq!(step_keys(&listed), vec!["revised"], "{listed}");
    assert_eq!(sealed["revision"], 2, "{sealed}");
    assert!(
        sealed["report"]
            .as_str()
            .is_some_and(|text| text.contains("REV2_SEALED")),
        "{sealed}"
    );
}

#[test]
fn wait_for_sealed_keeps_waiting_when_status_shows_a_newer_revision() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let argv_log = fixture.scripts.join("fake-brgr.log");
    let wait_count = fixture.scripts.join("wait-count");
    let fake_bin = fixture.scripts.join("fake-brgr");
    fs::write(
        &fake_bin,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "{log}"
count_file="{count}"
task="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
rid1="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
rid2="cccccccc-cccc-4ccc-8ccc-cccccccccccc"
case " $* " in
  *" wait "*)
    n=0
    if [ -f "$count_file" ]; then
      n=$(cat "$count_file")
    fi
    n=$((n + 1))
    printf '%s\n' "$n" > "$count_file"
    if [ "$n" -eq 1 ]; then
      printf '%s\n' "{{\"task_id\":\"$task\",\"result_id\":\"$rid1\",\"outcome\":\"candidate\",\"revision\":1}}"
    else
      printf '%s\n' "{{\"task_id\":\"$task\",\"result_id\":\"$rid2\",\"outcome\":\"candidate\",\"revision\":2}}"
    fi
    ;;
  *" workflow "*)
    printf '%s\n' "{{\"state\":\"running\"}}"
    ;;
  *" status "*)
    printf '%s\n' "{{\"task\":{{\"revision\":2}},\"state\":\"running\"}}"
    ;;
  *" result "*)
    n=0
    if [ -f "$count_file" ]; then
      n=$(cat "$count_file")
    fi
    if [ "$n" -eq 1 ]; then
      printf '%s\n' "{{\"result\":{{\"outcome\":\"candidate\",\"revision\":1,\"result_id\":\"$rid1\"}},\"artifacts\":[{{\"text\":\"REV1\"}}]}}"
    else
      printf '%s\n' "{{\"result\":{{\"outcome\":\"candidate\",\"revision\":2,\"result_id\":\"$rid2\"}},\"artifacts\":[{{\"text\":\"REV2\"}}]}}"
    fi
    ;;
  *" run "*)
    printf '%s\n' "{{\"task_id\":\"$task\"}}"
    ;;
  *)
    printf '%s\n' '{{\"ok\":true}}'
    ;;
esac
"#,
            log = argv_log.display(),
            count = wait_count.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&fake_bin, fs::Permissions::from_mode(0o700)).unwrap();
    let script = write_script(&fixture, "stale-wait.mjs", REVISE_SCRIPT);
    let output = Command::new("node")
        .arg(&script)
        .current_dir(&fixture.scripts)
        .env("BRGR_BIN", &fake_bin)
        .env("BRGR_HOME", &fixture.home)
        .env("BRGR_WORKFLOW_RUN", "wf-aaaaaaaaaaaa")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "sdk script failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let sealed: Value =
        serde_json::from_str(&fs::read_to_string(fixture.home.join("agent-result.json")).unwrap())
            .unwrap();
    assert_eq!(sealed["revision"], 2, "{sealed}");
    assert_eq!(sealed["outcome"], "completed", "{sealed}");
    assert_eq!(sealed["report"], "REV2", "{sealed}");
}

fn step_task_id(status: &Value) -> String {
    status["steps"][0]["task_id"].as_str().unwrap().to_owned()
}

fn sealed_outcome(fixture: &Fixture, task: &str) -> String {
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["result", task],
        &owner_env(),
    ))["result"]["outcome"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn reply_to_worker_question(fixture: &Fixture, task: &str, body: &str) {
    let question = json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &[
            "message",
            "wait",
            task,
            "--for",
            "owner",
            "--timeout-seconds",
            "30",
        ],
        &owner_env(),
    ));
    let question_id = question["message_id"].as_str().unwrap();
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &[
            "message",
            "send",
            task,
            "--to",
            "worker",
            "--kind",
            "reply",
            "--reply-to",
            question_id,
            "--body",
            body,
        ],
        &owner_env(),
    ));
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["message", "ack", task, question_id, "--for", "owner"],
        &owner_env(),
    ));
}

#[test]
fn a_worker_that_outlives_a_two_second_deadline_is_cancelled() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let script = write_script(&fixture, "deadline-cancel.mjs", DEADLINE_CANCEL_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(90));
    assert_eq!(step_keys(&status), vec!["slow"], "{status}");
    assert_eq!(status["steps"][0]["state"], "terminal", "{status}");
    let sealed = agent_result(&fixture);
    assert_eq!(sealed["outcome"], "cancelled", "{sealed}");
    assert_eq!(sealed["timed_out"], true, "{sealed}");
    let task = step_task_id(&status);
    assert_eq!(sealed_outcome(&fixture, &task), "cancelled");
}

#[test]
fn a_worker_that_finishes_before_the_deadline_returns_with_timed_out_false() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let script = write_script(&fixture, "deadline-ok.mjs", DEADLINE_OK_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(90));
    assert_eq!(step_keys(&status), vec!["fast"], "{status}");
    let sealed = agent_result(&fixture);
    assert_eq!(sealed["outcome"], "completed", "{sealed}");
    assert_eq!(sealed["timed_out"], false, "{sealed}");
}

#[test]
fn invalid_deadline_values_throw_before_any_task_exists() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let script = write_script(&fixture, "deadline-types.mjs", INVALID_DEADLINE_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(30));
    assert!(
        step_keys(&status).is_empty(),
        "invalid deadline launched a task: {status}"
    );
    assert!(
        fixture.workspace.join("deadline-typeerror-ok").is_file(),
        "script did not record TypeError coverage"
    );
}

#[test]
fn a_changed_deadline_under_an_existing_step_key_diverges_and_launches_nothing_new() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    fs::write(fixture.scripts.join("deadline.txt"), "60\n").unwrap();
    let script = write_script(&fixture, "deadline-diverge.mjs", DEADLINE_DIVERGED_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    let _task = wait_for_first_step(&fixture, &run_id);
    kill_driver(&fixture, &run_id);
    fs::write(fixture.scripts.join("deadline.txt"), "30\n").unwrap();
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "resume", &run_id],
        &owner_env(),
    ));
    let status = wait_run_state(&fixture, &run_id, "failed", Duration::from_secs(30));
    assert_eq!(
        status["steps"].as_array().map(Vec::len),
        Some(1),
        "{status}"
    );
    let log = workflow_log(&fixture, &run_id);
    assert!(log.contains("diverged"), "resume did not diverge: {log}");
}

#[test]
fn resume_after_the_driver_dies_does_not_reset_the_step_deadline() {
    require_node();
    let fixture = fixture_with_script(HOLD_GJC);
    init_sdk(&fixture);
    let script = write_script(&fixture, "deadline-resume.mjs", DEADLINE_RESUME_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    let task = wait_for_first_step(&fixture, &run_id);
    wait_for_step_running(&fixture, &run_id, &task);
    kill_driver(&fixture, &run_id);
    thread::sleep(Duration::from_secs(28));
    let resumed_at = Instant::now();
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "resume", &run_id],
        &owner_env(),
    ));
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(90));
    assert!(
        resumed_at.elapsed() < Duration::from_secs(20),
        "resume waited a fresh deadline: elapsed={:?} log={}",
        resumed_at.elapsed(),
        workflow_log(&fixture, &run_id)
    );
    let sealed = agent_result(&fixture);
    assert_eq!(sealed["outcome"], "cancelled", "{sealed}");
    assert_eq!(sealed["timed_out"], true, "{sealed}");
    assert_eq!(status["steps"][0]["state"], "terminal", "{status}");
}

#[test]
fn status_created_at_follows_the_store_clock_not_the_launch_file() {
    let fixture = fixture_with_script(HOLD_GJC);
    let launched = json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &[
            "run",
            "HOLD until release",
            "--workspace",
            fixture.workspace.to_str().unwrap(),
        ],
        &owner_env(),
    ));
    let task = launched["task_id"].as_str().unwrap().to_owned();
    wait_until_running(&fixture, &task);
    let first = status_created_at(&fixture, &task);
    let created = first["created_at"]
        .as_u64()
        .expect("running task has no created_at");
    let anchored = created.saturating_sub(86_400);
    set_task_attempt_clocks(&fixture, &task, i64::try_from(anchored).unwrap());
    let after_store = status_created_at(&fixture, &task);
    assert_eq!(
        after_store["created_at"].as_u64(),
        Some(anchored),
        "created_at ignored the store clock: {after_store}"
    );
    let launch = launch_file(&fixture, &task);
    let file = fs::File::options().write(true).open(&launch).unwrap();
    file.set_modified(SystemTime::now() + Duration::from_hours(24))
        .unwrap();
    drop(file);
    let after_touch = status_created_at(&fixture, &task);
    assert_eq!(
        after_touch["created_at"].as_u64(),
        Some(anchored),
        "touching the launch file moved created_at: {after_touch}"
    );
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["cancel", &task],
        &owner_env(),
    ));
    fs::remove_file(&launch).unwrap();
    let after_remove = json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["status", &task, "--observe"],
        &owner_env(),
    ));
    assert_eq!(
        after_remove["created_at"].as_u64(),
        Some(anchored),
        "removing the launch file moved created_at: {after_remove}"
    );
}

#[cfg(debug_assertions)]
#[test]
fn status_created_at_is_null_until_an_attempt_starts() {
    let fixture = fixture_home();
    let mut envs = owner_env().to_vec();
    envs.push(("BRGR_TEST_EXIT_BEFORE_TASK_CLAIM", "1"));
    let launched = json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &[
            "run",
            "queued only",
            "--workspace",
            fixture.workspace.to_str().unwrap(),
        ],
        &envs,
    ));
    let task = launched["task_id"].as_str().unwrap();
    let status = json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["status", task, "--observe"],
        &owner_env(),
    ));
    assert_eq!(status["state"], "queued", "{status}");
    assert!(
        status["created_at"].is_null(),
        "created_at was set before an attempt started: {status}"
    );
}

#[test]
fn a_step_awaiting_input_is_not_cancelled_by_the_deadline() {
    require_node();
    let fixture = fixture_with_script(ASKING_GJC);
    init_sdk(&fixture);
    let script = write_script(&fixture, "ask-deadline.mjs", QUESTION_DEADLINE_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    let blocked = wait_run_state(&fixture, &run_id, "blocked", Duration::from_secs(90));
    let task = step_task_id(&blocked);
    thread::sleep(Duration::from_secs(5));
    let still = json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "status", &run_id],
        &owner_env(),
    ));
    assert_eq!(still["state"], "blocked", "{still}");
    assert_ne!(still["steps"][0]["state"], "terminal", "{still}");
    reply_to_worker_question(&fixture, &task, "token-42");
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(90));
    assert_eq!(step_keys(&status), vec!["ask"], "{status}");
}

fn write_fake_brgr(fixture: &Fixture, body: &str) -> PathBuf {
    let log = fixture.scripts.join("fake-brgr.log");
    let fake_bin = fixture.scripts.join("fake-brgr");
    fs::write(
        &fake_bin,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{log}\"\n{body}\n",
            log = log.display(),
            body = body
        ),
    )
    .unwrap();
    fs::set_permissions(&fake_bin, fs::Permissions::from_mode(0o700)).unwrap();
    fake_bin
}

fn sdk_script_command(fixture: &Fixture, fake_bin: &Path, script: &Path) -> Command {
    let mut command = Command::new("node");
    command
        .arg(script)
        .current_dir(&fixture.scripts)
        .env("BRGR_BIN", fake_bin)
        .env("BRGR_HOME", &fixture.home)
        .env("BRGR_WORKFLOW_RUN", "wf-aaaaaaaaaaaa")
        .env("BRGR_OWNER_ID", "codex:workflow-owner")
        .env("BRGR_SESSION_ID", "workflow-session")
        .env_remove("BRGR_PARENT_TASK_ID")
        .env_remove("BRGR_PARENT_ATTEMPT_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CURSOR_CONVERSATION_ID");
    command
}

fn run_sdk_script(fixture: &Fixture, fake_bin: &Path, script: &Path) -> std::process::Output {
    sdk_script_command(fixture, fake_bin, script)
        .output()
        .unwrap()
}

fn spawn_sdk_script(fixture: &Fixture, fake_bin: &Path, script: &Path) -> std::process::Child {
    sdk_script_command(fixture, fake_bin, script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn unix_seconds_ago(seconds: u64) -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(seconds),
    )
    .unwrap()
}

fn fake_argv_log(fixture: &Fixture) -> String {
    fs::read_to_string(fixture.scripts.join("fake-brgr.log")).unwrap_or_default()
}

fn wait_for_child(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => return None,
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn sdk_run_argv_does_not_turn_dash_values_into_options() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let fake_bin = write_fake_brgr(
        &fixture,
        r#"
task="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
rid="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
now=$(/bin/date +%s)
case " $* " in
  *" run "*) printf '{"task_id":"%s"}\n' "$task";;
  *" workflow status "*) printf '{"state":"running"}\n';;
  *" status "*) printf '{"task":{"revision":1},"state":"running","created_at":%s}\n' "$now";;
  *" wait "*) printf '{"task_id":"%s","result_id":"%s","outcome":"candidate","revision":1}\n' "$task" "$rid";;
  *" result "*) printf '{"result":{"outcome":"candidate","revision":1,"result_id":"%s"},"artifacts":[{"text":"ok"}]}\n' "$rid";;
  *) printf '{"ok":true}\n';;
esac
"#,
    );
    let script = write_script(&fixture, "dash-task.mjs", DASH_TASK_SCRIPT);
    let output = run_sdk_script(&fixture, &fake_bin, &script);
    assert!(
        output.status.success(),
        "sdk script failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fake_argv_log(&fixture);
    assert!(
        log.contains("-- --workspace /etc"),
        "task text was not protected by -- : {log}"
    );
    assert!(
        log.contains("--step=--help"),
        "step key was not passed with = : {log}"
    );
    assert!(
        log.contains("--step-deadline=60"),
        "deadline was not passed with = : {log}"
    );
}

#[test]
fn sdk_refuses_an_over_long_task_before_launching() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let fake_bin = write_fake_brgr(
        &fixture,
        r#"
printf '%s\n' "unexpected launch" >&2
exit 1
"#,
    );
    let script = write_script(&fixture, "overlong.mjs", OVERLONG_TASK_SCRIPT);
    let output = run_sdk_script(&fixture, &fake_bin, &script);
    assert!(
        output.status.success(),
        "sdk script failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fake_argv_log(&fixture);
    assert!(
        !log.contains(" run "),
        "over-long task launched brgr run: {log}"
    );
    let message = fs::read_to_string(fixture.home.join("agent-error.txt")).unwrap();
    assert!(
        message.contains("262144"),
        "TypeError must name the limit: {message}"
    );
    assert!(!message.contains(&"x".repeat(20)), "{message}");
}

#[test]
fn sdk_accepts_a_256_kib_task_and_launches() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let fake_bin = write_fake_brgr(
        &fixture,
        r#"
task="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
rid="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
now=$(/bin/date +%s)
case " $* " in
  *" run "*) printf '{"task_id":"%s"}\n' "$task";;
  *" workflow status "*) printf '{"state":"running"}\n';;
  *" status "*) printf '{"task":{"revision":1},"state":"running","created_at":%s}\n' "$now";;
  *" wait "*) printf '{"task_id":"%s","result_id":"%s","outcome":"candidate","revision":1}\n' "$task" "$rid";;
  *" result "*) printf '{"result":{"outcome":"candidate","revision":1,"result_id":"%s"},"artifacts":[{"text":"ok"}]}\n' "$rid";;
  *) printf '{"ok":true}\n';;
esac
"#,
    );
    let script = write_script(&fixture, "at-limit.mjs", EXACT_LIMIT_TASK_SCRIPT);
    let output = run_sdk_script(&fixture, &fake_bin, &script);
    assert!(
        output.status.success(),
        "sdk script failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fake_argv_log(&fixture);
    assert!(log.contains(" run "), "256 KiB task did not launch: {log}");
}

#[test]
fn a_future_created_at_does_not_make_wait_use_a_huge_timeout() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let fake_bin = write_fake_brgr(
        &fixture,
        r#"
task="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
rid="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
now=$(/bin/date +%s)
future=$((now + 800000))
case " $* " in
  *" run "*) printf '{"task_id":"%s"}\n' "$task";;
  *" workflow status "*) printf '{"state":"running"}\n';;
  *" status "*) printf '{"task":{"revision":1},"state":"running","created_at":%s}\n' "$future";;
  *" wait "*) printf '{"task_id":"%s","result_id":"%s","outcome":"candidate","revision":1}\n' "$task" "$rid";;
  *" result "*) printf '{"result":{"outcome":"candidate","revision":1,"result_id":"%s"},"artifacts":[{"text":"ok"}]}\n' "$rid";;
  *) printf '{"ok":true}\n';;
esac
"#,
    );
    let script = write_script(&fixture, "future-clock.mjs", FUTURE_CLOCK_SCRIPT);
    let output = run_sdk_script(&fixture, &fake_bin, &script);
    assert!(
        output.status.success(),
        "sdk script failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fake_argv_log(&fixture);
    let wait_line = log
        .lines()
        .find(|line| line.contains("wait "))
        .unwrap_or("");
    assert!(
        !wait_line.contains("800000") && !wait_line.contains("80000"),
        "wait used the future-skewed remaining time: {wait_line}"
    );
    let parsed = wait_line
        .split("--timeout-seconds")
        .nth(1)
        .and_then(|rest| {
            rest.trim_start_matches('=')
                .split_whitespace()
                .next()
                .and_then(|value| value.parse::<u64>().ok())
        });
    assert!(
        parsed.is_some_and(|value| value <= 2),
        "wait timeout was {parsed:?} from {wait_line}"
    );
}

#[test]
fn an_owner_cancel_after_the_deadline_is_not_timed_out() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let fake_bin = write_fake_brgr(
        &fixture,
        r#"
task="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
rid="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
now=$(/bin/date +%s)
past=$((now - 10))
case " $* " in
  *" run "*) printf '{"task_id":"%s"}\n' "$task";;
  *" workflow status "*) printf '{"state":"running"}\n';;
  *" status "*) printf '{"task":{"revision":1},"state":"terminal","created_at":%s}\n' "$past";;
  *" wait "*) printf '{"task_id":"%s","result_id":"%s","outcome":"cancelled","revision":1}\n' "$task" "$rid";;
  *" result "*) printf '{"result":{"outcome":"cancelled","revision":1,"result_id":"%s"},"artifacts":[]}\n' "$rid";;
  *" cancel "*) printf '{"task_id":"%s","state":"cancel_requested"}\n' "$task";;
  *) printf '{"ok":true}\n';;
esac
"#,
    );
    let script = write_script(&fixture, "already-cancelled.mjs", ALREADY_CANCELLED_SCRIPT);
    let output = run_sdk_script(&fixture, &fake_bin, &script);
    assert!(
        output.status.success(),
        "sdk script failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fake_argv_log(&fixture);
    assert!(
        !log.contains(" cancel "),
        "sdk issued cancel for an owner-cancelled seal: {log}"
    );
    let sealed: Value =
        serde_json::from_str(&fs::read_to_string(fixture.home.join("agent-result.json")).unwrap())
            .unwrap();
    assert_eq!(sealed["outcome"], "cancelled", "{sealed}");
    assert_eq!(sealed["timed_out"], false, "{sealed}");
}

#[test]
fn owner_cancel_after_the_deadline_on_resume_is_not_timed_out() {
    require_node();
    let fixture = fixture_with_script(HOLD_GJC);
    init_sdk(&fixture);
    let script = write_script(
        &fixture,
        "owner-cancel-after-deadline.mjs",
        OWNER_CANCEL_AFTER_DEADLINE_SCRIPT,
    );
    let run_id = start_workflow(&fixture, &script);
    let task = wait_for_first_step(&fixture, &run_id);
    wait_for_step_running(&fixture, &run_id, &task);
    kill_driver(&fixture, &run_id);
    set_task_attempt_clocks(&fixture, &task, unix_seconds_ago(120));
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["cancel", &task],
        &owner_env(),
    ));
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "resume", &run_id],
        &owner_env(),
    ));
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(90));
    assert_eq!(step_keys(&status), vec!["late"], "{status}");
    let sealed = agent_result(&fixture);
    assert_eq!(sealed["outcome"], "cancelled", "{sealed}");
    assert_eq!(sealed["timed_out"], false, "{sealed}");
    assert_eq!(sealed_outcome(&fixture, &task), "cancelled");
}

#[test]
fn a_deadline_cancel_that_seals_as_something_else_is_not_timed_out() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let fake_bin = write_fake_brgr(
        &fixture,
        r#"
task="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
rid="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
now=$(/bin/date +%s)
past=$((now - 10))
dir="$(dirname "$0")"
case " $* " in
  *" run "*) printf '{"task_id":"%s"}\n' "$task";;
  *" workflow status "*) printf '{"state":"running"}\n';;
  *" status "*) printf '{"task":{"revision":1},"state":"running","created_at":%s}\n' "$past";;
  *" wait "*)
    if [ -f "$dir/cancelled" ]; then
      printf '{"task_id":"%s","result_id":"%s","outcome":"candidate","revision":1}\n' "$task" "$rid"
    else
      printf 'Error: task %s did not produce a terminal result before the wait timeout\n' "$task" >&2
      exit 1
    fi
    ;;
  *" result "*) printf '{"result":{"outcome":"candidate","revision":1,"result_id":"%s"},"artifacts":[{"text":"ok"}]}\n' "$rid";;
  *" cancel "*)
    printf '1\n' > "$dir/cancelled"
    printf '{"task_id":"%s","state":"cancel_requested"}\n' "$task"
    ;;
  *) printf '{"ok":true}\n';;
esac
"#,
    );
    let script = write_script(&fixture, "deadline-race.mjs", ALREADY_CANCELLED_SCRIPT);
    let output = run_sdk_script(&fixture, &fake_bin, &script);
    assert!(
        output.status.success(),
        "sdk script failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fake_argv_log(&fixture);
    let runs = log.lines().filter(|line| line.contains(" run ")).count();
    assert_eq!(runs, 1, "race launched a second task: {log}");
    assert!(log.contains(" cancel "), "sdk did not issue cancel: {log}");
    let sealed = agent_result(&fixture);
    assert_eq!(sealed["outcome"], "completed", "{sealed}");
    assert_eq!(sealed["timed_out"], false, "{sealed}");
}

#[test]
fn resume_after_driver_dies_between_deadline_cancel_and_read_is_not_timed_out() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let fake_bin = write_fake_brgr(
        &fixture,
        r#"
task="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
rid="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
now=$(/bin/date +%s)
past=$((now - 10))
dir="$(dirname "$0")"
case " $* " in
  *" run "*) printf '{"task_id":"%s"}\n' "$task";;
  *" workflow status "*) printf '{"state":"running"}\n';;
  *" status "*) printf '{"task":{"revision":1},"state":"running","created_at":%s}\n' "$past";;
  *" wait "*)
    if [ -f "$dir/resume" ]; then
      printf '{"task_id":"%s","result_id":"%s","outcome":"cancelled","revision":1}\n' "$task" "$rid"
    elif [ -f "$dir/cancelled" ]; then
      while [ ! -f "$dir/resume" ]; do
        /bin/sleep 0.05
      done
      printf '{"task_id":"%s","result_id":"%s","outcome":"cancelled","revision":1}\n' "$task" "$rid"
    else
      printf 'Error: task %s did not produce a terminal result before the wait timeout\n' "$task" >&2
      exit 1
    fi
    ;;
  *" result "*) printf '{"result":{"outcome":"cancelled","revision":1,"result_id":"%s"},"artifacts":[]}\n' "$rid";;
  *" cancel "*)
    printf '1\n' > "$dir/cancelled"
    printf '{"task_id":"%s","state":"cancel_requested"}\n' "$task"
    ;;
  *) printf '{"ok":true}\n';;
esac
"#,
    );
    let script = write_script(
        &fixture,
        "deadline-resume-gap.mjs",
        ALREADY_CANCELLED_SCRIPT,
    );
    let mut first = spawn_sdk_script(&fixture, &fake_bin, &script);
    assert!(
        wait_until(Duration::from_secs(10), || fixture
            .scripts
            .join("cancelled")
            .is_file()),
        "first driver never issued cancel: log={}",
        fake_argv_log(&fixture)
    );
    let _ = first.kill();
    let _ = first.wait();
    fs::write(fixture.scripts.join("resume"), b"1").unwrap();
    let output = run_sdk_script(&fixture, &fake_bin, &script);
    assert!(
        output.status.success(),
        "resume sdk script failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fake_argv_log(&fixture);
    let cancels = log.lines().filter(|line| line.contains(" cancel ")).count();
    let runs = log.lines().filter(|line| line.contains(" run ")).count();
    assert_eq!(cancels, 1, "resume issued a second cancel: {log}");
    assert_eq!(runs, 2, "resume lost or duplicated the task: {log}");
    let sealed = agent_result(&fixture);
    assert_eq!(sealed["outcome"], "cancelled", "{sealed}");
    assert_eq!(sealed["timed_out"], false, "{sealed}");
}

#[test]
fn a_distant_past_created_at_does_not_overflow_the_wait_timeout() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let fake_bin = write_fake_brgr(
        &fixture,
        r#"
task="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
rid="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
dir="$(dirname "$0")"
case " $* " in
  *" run "*) printf '{"task_id":"%s"}\n' "$task";;
  *" workflow status "*) printf '{"state":"running"}\n';;
  *" status "*) printf '{"task":{"revision":1},"state":"running","created_at":1}\n';;
  *" wait "*)
    if [ -f "$dir/cancelled" ]; then
      printf '{"task_id":"%s","result_id":"%s","outcome":"cancelled","revision":1}\n' "$task" "$rid"
    else
      printf 'Error: task %s did not produce a terminal result before the wait timeout\n' "$task" >&2
      exit 1
    fi
    ;;
  *" result "*) printf '{"result":{"outcome":"cancelled","revision":1,"result_id":"%s"},"artifacts":[]}\n' "$rid";;
  *" cancel "*)
    printf '1\n' > "$dir/cancelled"
    printf '{"task_id":"%s","state":"cancel_requested"}\n' "$task"
    ;;
  *) printf '{"ok":true}\n';;
esac
"#,
    );
    let script = write_script(&fixture, "distant-past.mjs", ALREADY_CANCELLED_SCRIPT);
    let output = run_sdk_script(&fixture, &fake_bin, &script);
    assert!(
        output.status.success(),
        "sdk script failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fake_argv_log(&fixture);
    let wait_line = log
        .lines()
        .find(|line| line.contains("wait "))
        .unwrap_or("");
    let parsed = wait_line
        .split("--timeout-seconds")
        .nth(1)
        .and_then(|rest| {
            rest.trim_start_matches('=')
                .split_whitespace()
                .next()
                .and_then(|value| value.parse::<u64>().ok())
        });
    assert!(
        parsed == Some(1),
        "distant-past wait timeout was {parsed:?} from {wait_line}"
    );
    let sealed = agent_result(&fixture);
    assert_eq!(sealed["outcome"], "cancelled", "{sealed}");
    assert_eq!(sealed["timed_out"], true, "{sealed}");
}

#[test]
fn deadline_cancel_uses_only_the_launched_task_id() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let fake_bin = write_fake_brgr(
        &fixture,
        r#"
task="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
other="cccccccc-cccc-4ccc-8ccc-cccccccccccc"
rid="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
now=$(/bin/date +%s)
past=$((now - 10))
dir="$(dirname "$0")"
case " $* " in
  *" run "*) printf '{"task_id":"%s"}\n' "$task";;
  *" workflow status "*) printf '{"state":"running"}\n';;
  *" status "*) printf '{"task":{"revision":1},"state":"running","created_at":%s}\n' "$past";;
  *" wait "*)
    if [ -f "$dir/cancelled" ]; then
      printf '{"task_id":"%s","result_id":"%s","outcome":"cancelled","revision":1}\n' "$other" "$rid"
    else
      printf 'Error: task %s did not produce a terminal result before the wait timeout\n' "$task" >&2
      exit 1
    fi
    ;;
  *" result "*) printf '{"result":{"outcome":"cancelled","revision":1,"result_id":"%s"},"artifacts":[]}\n' "$rid";;
  *" cancel "*)
    printf '1\n' > "$dir/cancelled"
    printf '{"task_id":"%s","state":"cancel_requested"}\n' "$task"
    ;;
  *) printf '{"ok":true}\n';;
esac
"#,
    );
    let script = write_script(&fixture, "cancel-launched.mjs", ALREADY_CANCELLED_SCRIPT);
    let output = run_sdk_script(&fixture, &fake_bin, &script);
    assert!(
        output.status.success(),
        "sdk script failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fake_argv_log(&fixture);
    let cancel_line = log
        .lines()
        .find(|line| line.contains(" cancel "))
        .unwrap_or("");
    assert!(
        cancel_line.contains("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
        "cancel did not target the launched task: {log}"
    );
    assert!(
        !cancel_line.contains("cccccccc-cccc-4ccc-8ccc-cccccccccccc"),
        "cancel targeted another task id: {log}"
    );
    assert_eq!(
        log.lines().filter(|line| line.contains(" cancel ")).count(),
        1,
        "cancelled more than the launched task: {log}"
    );
}

#[test]
fn a_foreign_owner_cannot_cancel_a_workflow_step() {
    let fixture = fixture_with_script(HOLD_GJC);
    let run_id = create_workflow(&fixture, &[]);
    let launched = launch_workflow_step(&fixture, &run_id, "hold", "HOLD until release");
    let task = json_output(&launched)["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_until_running(&fixture, &task);
    let foreign = [
        ("BRGR_OWNER_ID", "codex:someone-else"),
        ("BRGR_SESSION_ID", "other-session"),
    ];
    let refused = run(
        &fixture.home,
        &fixture.workspace,
        &["cancel", &task],
        &foreign,
    );
    assert!(
        !refused.status.success(),
        "a different owner cancelled the step: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("BRGR_OWNER_ID differs"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let still = json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["status", &task],
        &owner_env(),
    ));
    assert_ne!(still["state"], "terminal", "{still}");
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["cancel", &task],
        &owner_env(),
    ));
}

#[test]
fn a_deadline_timeout_cancel_is_not_retried() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let fake_bin = write_fake_brgr(
        &fixture,
        r#"
task="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
rid="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
now=$(/bin/date +%s)
past=$((now - 10))
dir="$(dirname "$0")"
case " $* " in
  *" run "*) printf '{"task_id":"%s"}\n' "$task";;
  *" workflow status "*) printf '{"state":"running"}\n';;
  *" status "*) printf '{"task":{"revision":1},"state":"running","created_at":%s}\n' "$past";;
  *" wait "*)
    if [ -f "$dir/cancelled" ]; then
      printf '{"task_id":"%s","result_id":"%s","outcome":"cancelled","revision":1}\n' "$task" "$rid"
    else
      printf 'Error: task %s did not produce a terminal result before the wait timeout\n' "$task" >&2
      exit 1
    fi
    ;;
  *" result "*) printf '{"result":{"outcome":"cancelled","revision":1,"result_id":"%s"},"artifacts":[]}\n' "$rid";;
  *" cancel "*)
    printf '1\n' > "$dir/cancelled"
    printf '{"task_id":"%s","state":"cancel_requested"}\n' "$task"
    ;;
  *) printf '{"ok":true}\n';;
esac
"#,
    );
    let script = write_script(&fixture, "deadline-retry.mjs", DEADLINE_RETRY_SCRIPT);
    let output = run_sdk_script(&fixture, &fake_bin, &script);
    assert!(
        output.status.success(),
        "sdk script failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fake_argv_log(&fixture);
    let runs = log.lines().filter(|line| line.contains(" run ")).count();
    assert_eq!(runs, 1, "timed_out cancel was retried: {log}");
    let sealed = agent_result(&fixture);
    assert_eq!(sealed["outcome"], "cancelled", "{sealed}");
    assert_eq!(sealed["timed_out"], true, "{sealed}");
    assert_eq!(sealed["attempts"], 1, "{sealed}");
}

#[test]
fn agent_stops_waiting_when_the_workflow_run_is_no_longer_running() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let fake_bin = write_fake_brgr(
        &fixture,
        r#"
task="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
case " $* " in
  *" run "*) printf '{"task_id":"%s"}\n' "$task";;
  *" workflow status "*) printf '{"state":"cancelled"}\n';;
  *" status "*) printf '{"task":{"revision":1},"state":"queued"}\n';;
  *) printf '{"ok":true}\n';;
esac
"#,
    );
    let script = write_script(&fixture, "cancelled-run.mjs", CANCELLED_RUN_SCRIPT);
    let mut child = Command::new("node")
        .arg(&script)
        .current_dir(&fixture.scripts)
        .env("BRGR_BIN", &fake_bin)
        .env("BRGR_HOME", &fixture.home)
        .env("BRGR_WORKFLOW_RUN", "wf-aaaaaaaaaaaa")
        .env("BRGR_OWNER_ID", "codex:workflow-owner")
        .env("BRGR_SESSION_ID", "workflow-session")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let status = wait_for_child(&mut child, Duration::from_secs(5));
    assert!(
        status.is_some(),
        "agent() kept polling after the run was cancelled: log={}",
        fake_argv_log(&fixture)
    );
    wait_for_file(
        &fixture.home.join("agent-error.txt"),
        Duration::from_secs(2),
    );
    let message = fs::read_to_string(fixture.home.join("agent-error.txt")).unwrap();
    assert!(
        message.contains("cancelled") && !message.contains("agent() returned"),
        "unexpected stop: {message}"
    );
}

#[test]
fn a_step_that_fails_once_then_succeeds_returns_attempts_two() {
    require_node();
    let fixture = fixture_with_script(FAIL_ONCE_GJC);
    init_sdk(&fixture);
    enable_fail_once(&fixture);
    let script = write_script(&fixture, "retry-once.mjs", RETRY_ONCE_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(90));
    assert_eq!(
        step_keys(&status),
        vec!["retry-me", "retry-me~retry1"],
        "{status}"
    );
    let sealed = agent_result(&fixture);
    assert_eq!(sealed["outcome"], "completed", "{sealed}");
    assert_eq!(sealed["attempts"], 2, "{sealed}");
    assert_eq!(sealed["timed_out"], false, "{sealed}");
}

#[test]
fn a_step_that_fails_more_than_retries_returns_the_last_failure() {
    require_node();
    let fixture = fixture_with_script(FAILING_GJC);
    init_sdk(&fixture);
    let script = write_script(&fixture, "retry-exhausted.mjs", RETRY_EXHAUSTED_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(90));
    assert_eq!(
        step_keys(&status),
        vec!["always-fail", "always-fail~retry1", "always-fail~retry2"],
        "{status}"
    );
    let sealed = agent_result(&fixture);
    assert_eq!(sealed["outcome"], "failed", "{sealed}");
    assert_eq!(sealed["attempts"], 3, "{sealed}");
}

#[test]
fn a_lost_outcome_is_not_retried() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let fake_bin = write_fake_brgr(&fixture, &fake_terminal_body("lost"));
    let script = write_script(&fixture, "retry-lost.mjs", RETRY_LOST_SCRIPT);
    let output = run_sdk_script(&fixture, &fake_bin, &script);
    assert!(
        output.status.success(),
        "sdk script failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let sealed = agent_result(&fixture);
    assert_eq!(sealed["outcome"], "lost", "{sealed}");
    assert_eq!(sealed["attempts"], 1, "{sealed}");
    let runs = fake_argv_log(&fixture)
        .lines()
        .filter(|line| line.contains(" run "))
        .count();
    assert_eq!(runs, 1, "lost was retried: {}", fake_argv_log(&fixture));
}

#[test]
fn a_cancelled_outcome_is_not_retried() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let fake_bin = write_fake_brgr(&fixture, &fake_terminal_body("cancelled"));
    let script = write_script(&fixture, "retry-cancelled.mjs", RETRY_CANCELLED_SCRIPT);
    let output = run_sdk_script(&fixture, &fake_bin, &script);
    assert!(
        output.status.success(),
        "sdk script failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let sealed = agent_result(&fixture);
    assert_eq!(sealed["outcome"], "cancelled", "{sealed}");
    assert_eq!(sealed["attempts"], 1, "{sealed}");
    let runs = fake_argv_log(&fixture)
        .lines()
        .filter(|line| line.contains(" run "))
        .count();
    assert_eq!(
        runs,
        1,
        "cancelled was retried: {}",
        fake_argv_log(&fixture)
    );
}

#[test]
fn resume_mid_retry_does_not_launch_a_duplicate() {
    require_node();
    let fixture = fixture_with_script(FAIL_ONCE_GJC);
    init_sdk(&fixture);
    enable_fail_once(&fixture);
    fs::write(fixture.home.join("hold-retries"), b"1").unwrap();
    let script = write_script(&fixture, "retry-resume.mjs", RETRY_ONCE_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    let before = wait_for_n_steps(&fixture, &run_id, 2);
    assert_eq!(
        step_keys(&before),
        vec!["retry-me", "retry-me~retry1"],
        "{before}"
    );
    // Killing the driver before the retry's supervisor claims the task leaves
    // that task `lost`, which is correct and never retried; wait until it runs.
    let retry_task = before["steps"][1]["task_id"].as_str().unwrap().to_owned();
    wait_for_step_running(&fixture, &run_id, &retry_task);
    kill_driver(&fixture, &run_id);
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "resume", &run_id],
        &owner_env(),
    ));
    thread::sleep(Duration::from_millis(400));
    let mid = json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "status", &run_id],
        &owner_env(),
    ));
    assert_eq!(
        step_keys(&mid),
        vec!["retry-me", "retry-me~retry1"],
        "resume launched a duplicate: {mid}"
    );
    fs::write(fixture.home.join("release-hold"), b"1").unwrap();
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(90));
    assert_eq!(
        step_keys(&status),
        vec!["retry-me", "retry-me~retry1"],
        "{status}"
    );
    let sealed = agent_result(&fixture);
    assert_eq!(sealed["outcome"], "completed", "{sealed}");
    assert_eq!(sealed["attempts"], 2, "{sealed}");
}

#[test]
fn retries_zero_or_absent_does_not_relaunch_a_failed_step() {
    require_node();
    for (name, source) in [
        ("retry-zero.mjs", RETRY_ZERO_SCRIPT),
        ("retry-absent.mjs", RETRY_ABSENT_SCRIPT),
    ] {
        let fixture = fixture_with_script(FAILING_GJC);
        init_sdk(&fixture);
        let script = write_script(&fixture, name, source);
        let run_id = start_workflow(&fixture, &script);
        let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(90));
        assert_eq!(step_keys(&status), vec!["fail-step"], "{status}");
        let sealed = agent_result(&fixture);
        assert_eq!(sealed["outcome"], "failed", "{sealed}");
        assert_eq!(sealed["attempts"], 1, "{sealed}");
    }
}

#[test]
fn a_changed_retries_under_an_existing_step_key_diverges() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    fs::write(fixture.scripts.join("retries.txt"), "1\n").unwrap();
    let script = write_script(&fixture, "retry-diverge.mjs", RETRY_DIVERGED_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    let _task = wait_for_first_step(&fixture, &run_id);
    kill_driver(&fixture, &run_id);
    fs::write(fixture.scripts.join("retries.txt"), "2\n").unwrap();
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "resume", &run_id],
        &owner_env(),
    ));
    let status = wait_run_state(&fixture, &run_id, "failed", Duration::from_secs(30));
    assert_eq!(
        status["steps"].as_array().map(Vec::len),
        Some(1),
        "{status}"
    );
    let log = workflow_log(&fixture, &run_id);
    assert!(log.contains("diverged"), "resume did not diverge: {log}");
}

#[test]
fn step_deadline_is_refused_without_workflow_and_step_and_out_of_range() {
    let fixture = fixture_home();
    let refuses = |args: &[&str]| {
        let output = run(&fixture.home, &fixture.workspace, args, &owner_env());
        assert!(
            !output.status.success(),
            "accepted {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    refuses(&["run", "obj", "--step-deadline", "30"]);
    refuses(&[
        "run",
        "obj",
        "--workflow",
        "w",
        "--step",
        "s",
        "--step-deadline",
        "0",
    ]);
    refuses(&[
        "run",
        "obj",
        "--workflow",
        "w",
        "--step",
        "s",
        "--step-deadline",
        "-1",
    ]);
    refuses(&[
        "run",
        "obj",
        "--workflow",
        "w",
        "--step",
        "s",
        "--step-deadline",
        "86401",
    ]);
    refuses(&[
        "run",
        "obj",
        "--workflow",
        "w",
        "--step",
        "s",
        "--step-deadline",
        "1.5",
    ]);
    refuses(&[
        "run",
        "obj",
        "--workflow",
        "w",
        "--step",
        "s",
        "--step-deadline",
        "nope",
    ]);
    refuses(&[
        "run",
        "obj",
        "--workflow",
        "w",
        "--step",
        "s",
        "--step-deadline",
        "18446744073709551616",
    ]);
    let run_id = create_workflow(&fixture, &[]);
    let accepted = launch_workflow_step_with(
        &fixture,
        &run_id,
        "ok",
        "BRGR_FIXTURE_OK",
        &["--step-deadline", "30"],
    );
    assert!(
        accepted.status.success(),
        "--step-deadline 30 with --workflow/--step was refused: {}",
        String::from_utf8_lossy(&accepted.stderr)
    );
}

#[test]
fn step_retries_is_refused_without_workflow_and_step_and_out_of_range() {
    let fixture = fixture_home();
    let refuses = |args: &[&str]| {
        let output = run(&fixture.home, &fixture.workspace, args, &owner_env());
        assert!(
            !output.status.success(),
            "accepted {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    refuses(&["run", "obj", "--step-retries", "1"]);
    refuses(&[
        "run",
        "obj",
        "--workflow",
        "w",
        "--step",
        "s",
        "--step-retries",
        "4",
    ]);
    refuses(&[
        "run",
        "obj",
        "--workflow",
        "w",
        "--step",
        "s",
        "--step-retries",
        "-1",
    ]);
    refuses(&[
        "run",
        "obj",
        "--workflow",
        "w",
        "--step",
        "s",
        "--step-retries",
        "x",
    ]);
    refuses(&[
        "run",
        "obj",
        "--workflow",
        "w",
        "--step",
        "s",
        "--step-retries",
        "99999999999999999999",
    ]);
    let run_id = create_workflow(&fixture, &[]);
    let accepted = launch_workflow_step_with(
        &fixture,
        &run_id,
        "ok",
        "BRGR_FIXTURE_OK",
        &["--step-retries", "1"],
    );
    assert!(
        accepted.status.success(),
        "--step-retries 1 with --workflow/--step was refused: {}",
        String::from_utf8_lossy(&accepted.stderr)
    );
}

#[test]
fn a_step_without_retries_keeps_the_same_input_digest() {
    let fixture = fixture_home();
    let run_id = create_workflow(&fixture, &[]);
    let first = launch_workflow_step(&fixture, &run_id, "plain", "BRGR_FIXTURE_OK digest");
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let digest = step_digest(&fixture, &run_id, "plain");
    let replay = launch_workflow_step(&fixture, &run_id, "plain", "BRGR_FIXTURE_OK digest");
    assert!(
        replay.status.success(),
        "{}",
        String::from_utf8_lossy(&replay.stderr)
    );
    assert_eq!(step_digest(&fixture, &run_id, "plain"), digest);
    let zero = launch_workflow_step_with(
        &fixture,
        &run_id,
        "plain",
        "BRGR_FIXTURE_OK digest",
        &["--step-retries", "0"],
    );
    assert!(
        zero.status.success(),
        "retries 0 changed the digest: {}",
        String::from_utf8_lossy(&zero.stderr)
    );
    assert_eq!(step_digest(&fixture, &run_id, "plain"), digest);
    let changed = launch_workflow_step_with(
        &fixture,
        &run_id,
        "plain",
        "BRGR_FIXTURE_OK digest",
        &["--step-retries", "1"],
    );
    assert!(
        !changed.status.success(),
        "retries 1 replayed instead of diverging"
    );
    assert!(
        String::from_utf8_lossy(&changed.stderr).contains("diverged"),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    assert_eq!(step_digest(&fixture, &run_id, "plain"), digest);
}

#[test]
fn invalid_retries_values_throw_before_any_task_exists() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let script = write_script(&fixture, "retries-types.mjs", INVALID_RETRIES_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(30));
    assert!(
        step_keys(&status).is_empty(),
        "invalid retries launched a task: {status}"
    );
    assert!(
        fixture.workspace.join("retries-typeerror-ok").is_file(),
        "script did not record TypeError coverage"
    );
}

#[test]
fn sdk_retry_argv_does_not_turn_dash_values_into_options() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let fake_bin = write_fake_brgr(
        &fixture,
        r#"
task="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
rid="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
now=$(/bin/date +%s)
n_file="$(dirname "$0")/run-n"
n=0
if [ -f "$n_file" ]; then
  n=$(cat "$n_file")
fi
case " $* " in
  *" run "*)
    n=$((n + 1))
    printf '%s\n' "$n" > "$n_file"
    printf '{"task_id":"%s"}\n' "$task"
    ;;
  *" workflow status "*) printf '{"state":"running"}\n';;
  *" status "*) printf '{"task":{"revision":1},"state":"running","created_at":%s}\n' "$now";;
  *" wait "*)
    if [ "$(cat "$n_file")" -eq 1 ]; then
      printf '{"task_id":"%s","result_id":"%s","outcome":"failed","revision":1}\n' "$task" "$rid"
    else
      printf '{"task_id":"%s","result_id":"%s","outcome":"candidate","revision":1}\n' "$task" "$rid"
    fi
    ;;
  *" result "*)
    if [ "$(cat "$n_file")" -eq 1 ]; then
      printf '{"result":{"outcome":"failed","revision":1,"result_id":"%s"},"artifacts":[]}\n' "$rid"
    else
      printf '{"result":{"outcome":"candidate","revision":1,"result_id":"%s"},"artifacts":[{"text":"ok"}]}\n' "$rid"
    fi
    ;;
  *) printf '{"ok":true}\n';;
esac
"#,
    );
    let script = write_script(&fixture, "retry-dash.mjs", RETRY_DASH_SCRIPT);
    let output = run_sdk_script(&fixture, &fake_bin, &script);
    assert!(
        output.status.success(),
        "sdk script failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fake_argv_log(&fixture);
    assert!(
        log.contains("-- --workspace /etc"),
        "task text was not protected by -- : {log}"
    );
    assert!(
        log.contains("--step=--help"),
        "step key was not passed with = : {log}"
    );
    assert!(
        log.contains("--step=--help~retry1"),
        "retry key was not passed with = : {log}"
    );
    assert!(
        log.contains("--step-retries=1"),
        "retries was not passed with = : {log}"
    );
}

#[test]
fn pipeline_stage_report_argv_does_not_turn_dash_values_into_options() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let fake_bin = write_fake_brgr(
        &fixture,
        r#"
task="aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
rid="bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
now=$(/bin/date +%s)
case " $* " in
  *" run "*) printf '{"task_id":"%s"}\n' "$task";;
  *" workflow status "*) printf '{"state":"running"}\n';;
  *" status "*) printf '{"task":{"revision":1},"state":"running","created_at":%s}\n' "$now";;
  *" wait "*) printf '{"task_id":"%s","result_id":"%s","outcome":"candidate","revision":1}\n' "$task" "$rid";;
  *" result "*) printf '{"result":{"outcome":"candidate","revision":1,"result_id":"%s"},"artifacts":[{"text":"--workspace /etc"}]}\n' "$rid";;
  *) printf '{"ok":true}\n';;
esac
"#,
    );
    let script = write_script(&fixture, "pipeline-dash.mjs", PIPELINE_DASH_SCRIPT);
    let output = run_sdk_script(&fixture, &fake_bin, &script);
    assert!(
        output.status.success(),
        "sdk script failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fake_argv_log(&fixture);
    assert!(
        log.contains("-- --workspace /etc"),
        "pipeline stage report was not protected by -- : {log}"
    );
    assert!(
        log.contains("--step-deadline=60"),
        "deadline was not passed with = : {log}"
    );
    assert!(
        log.contains("--step-retries=1"),
        "retries was not passed with = : {log}"
    );
    wait_for_file(
        &fixture.home.join("pipeline-dash-ok"),
        Duration::from_secs(5),
    );
}

fn failed_launch(output: &std::process::Output) -> String {
    assert!(
        !output.status.success(),
        "launch succeeded: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn refuse_retry_step(fixture: &Fixture, run_id: &str, step: &str, extra: &[&str], needle: &str) {
    let output = launch_workflow_step_with(fixture, run_id, step, "BRGR_FIXTURE_OK", extra);
    let stderr = failed_launch(&output);
    assert!(stderr.contains(needle), "{stderr}");
}

#[test]
fn a_retry_suffix_is_bounded_and_needs_a_failed_predecessor() {
    let fixture = fixture_home();
    let run_id = create_workflow(&fixture, &[]);
    refuse_retry_step(
        &fixture,
        &run_id,
        "plain~retry4",
        &["--step-retries", "3"],
        "invalid",
    );
    refuse_retry_step(
        &fixture,
        &run_id,
        "plain~retry2",
        &["--step-retries", "1"],
        "step-retries",
    );
    refuse_retry_step(
        &fixture,
        &run_id,
        "plain~retry1",
        &["--step-retries", "1"],
        "predecessor",
    );
}

#[test]
fn a_retry_cannot_overwrite_a_step_or_attach_to_another_run() {
    let fixture = fixture_home();
    let run_id = create_workflow(&fixture, &[]);
    let other = create_workflow(&fixture, &[]);
    let first = launch_workflow_step_with(
        &fixture,
        &run_id,
        "plain",
        "BRGR_FIXTURE_OK digest-plain",
        &["--step-retries", "1"],
    );
    let first_id = json_output(&first)["task_id"].as_str().unwrap().to_owned();
    let digest = step_digest(&fixture, &run_id, "plain");
    assert!(
        wait_until(Duration::from_secs(90), || {
            run(
                &fixture.home,
                &fixture.workspace,
                &["result", &first_id],
                &owner_env(),
            )
            .status
            .success()
        }),
        "base step never sealed"
    );
    refuse_retry_step(
        &fixture,
        &run_id,
        "plain~retry1",
        &["--step-retries", "1"],
        "predecessor",
    );
    let listed = json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "status", &run_id],
        &owner_env(),
    ));
    assert_eq!(step_keys(&listed), vec!["plain"], "{listed}");
    assert_eq!(listed["steps"][0]["task_id"], first_id, "{listed}");
    assert_eq!(step_digest(&fixture, &run_id, "plain"), digest);
    refuse_retry_step(
        &fixture,
        &other,
        "plain~retry1",
        &["--step-retries", "1"],
        "predecessor",
    );
    let other_status = json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "status", &other],
        &owner_env(),
    ));
    assert!(
        step_keys(&other_status).is_empty(),
        "retry attached to another run: {other_status}"
    );
}

#[test]
fn a_three_stage_pipeline_passes_each_result_to_the_next_stage() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let script = write_script(&fixture, "pipeline-pass.mjs", PIPELINE_PASS_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(90));
    assert_eq!(step_keys(&status), vec!["0", "1", "2"], "{status}");
    let sealed = json_home_file(&fixture, "pipeline-result.json");
    assert_eq!(sealed["stopped_at"], Value::Null, "{sealed}");
    assert_eq!(
        sealed["results"].as_array().map(Vec::len),
        Some(3),
        "{sealed}"
    );
    assert_eq!(sealed["results"][0]["outcome"], "completed", "{sealed}");
    let second = status["steps"][1]["task_id"].as_str().unwrap();
    let objective = task_objective(&fixture, second);
    assert!(
        objective.contains("BRGR_FIXTURE_OK"),
        "second stage task did not include the first report: {objective}"
    );
}

#[test]
fn a_failed_pipeline_stage_stops_and_does_not_journal_later_stages() {
    require_node();
    let fixture = fixture_with_script(FAILING_GJC);
    init_sdk(&fixture);
    let script = write_script(&fixture, "pipeline-fail.mjs", PIPELINE_FAIL_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(90));
    assert_eq!(step_keys(&status), vec!["0", "1"], "{status}");
    let sealed = json_home_file(&fixture, "pipeline-result.json");
    assert_eq!(sealed["stopped_at"], 1, "{sealed}");
    assert_eq!(
        sealed["results"].as_array().map(Vec::len),
        Some(2),
        "{sealed}"
    );
    assert_eq!(sealed["results"][1]["outcome"], "failed", "{sealed}");
}

#[test]
fn resume_mid_pipeline_does_not_duplicate_or_relaunch_earlier_stages() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    fs::write(fixture.home.join("pause-before-1"), b"1").unwrap();
    let script = write_script(&fixture, "pipeline-resume.mjs", PIPELINE_RESUME_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    wait_for_file(&fixture.home.join("stage0-done"), Duration::from_secs(90));
    let before = json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "status", &run_id],
        &owner_env(),
    ));
    assert_eq!(step_keys(&before), vec!["0"], "{before}");
    let first_id = before["steps"][0]["task_id"].as_str().unwrap().to_owned();
    kill_driver(&fixture, &run_id);
    fs::remove_file(fixture.home.join("pause-before-1")).unwrap();
    json_output(&run(
        &fixture.home,
        &fixture.workspace,
        &["workflow", "resume", &run_id],
        &owner_env(),
    ));
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(90));
    assert_eq!(step_keys(&status), vec!["0", "1", "2"], "{status}");
    assert_eq!(status["steps"][0]["task_id"], first_id, "{status}");
}

#[test]
fn invalid_pipeline_arguments_throw_before_any_task_exists() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let script = write_script(&fixture, "pipeline-types.mjs", PIPELINE_INVALID_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(30));
    assert!(
        step_keys(&status).is_empty(),
        "invalid pipeline launched a task: {status}"
    );
    assert!(
        fixture.workspace.join("pipeline-typeerror-ok").is_file(),
        "script did not record TypeError coverage"
    );
}

#[test]
fn a_throwing_pipeline_stage_rejects_and_does_not_launch_later_stages() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let script = write_script(&fixture, "pipeline-throw.mjs", PIPELINE_THROW_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    let status = wait_run_state(&fixture, &run_id, "failed", Duration::from_secs(90));
    assert_eq!(step_keys(&status), vec!["0"], "{status}");
    wait_for_file(
        &fixture.home.join("pipeline-throw.txt"),
        Duration::from_secs(5),
    );
    let message = fs::read_to_string(fixture.home.join("pipeline-throw.txt")).unwrap();
    assert!(
        message.contains("stage-boom"),
        "pipeline did not reject with the stage error: {message}"
    );
}

#[test]
fn pipeline_inside_parallel_requires_a_key_prefix_before_launch() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let script = write_script(
        &fixture,
        "pipeline-parallel-type.mjs",
        PIPELINE_PARALLEL_TYPEERROR_SCRIPT,
    );
    let run_id = start_workflow(&fixture, &script);
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(30));
    assert!(
        step_keys(&status).is_empty(),
        "pipeline inside parallel launched without a key: {status}"
    );
    assert!(
        fixture
            .workspace
            .join("pipeline-parallel-typeerror-ok")
            .is_file(),
        "script did not record the parallel key TypeError"
    );
}

#[test]
fn pipeline_inside_parallel_uses_the_key_prefix_for_deterministic_keys() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let script = write_script(
        &fixture,
        "pipeline-parallel-keys.mjs",
        PIPELINE_PARALLEL_KEYS_SCRIPT,
    );
    let run_id = start_workflow(&fixture, &script);
    let status = wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(90));
    let mut keys = step_keys(&status);
    keys.sort();
    assert_eq!(
        keys,
        vec!["left-0", "left-1", "right-0", "right-1"],
        "{status}"
    );
    wait_for_file(
        &fixture.home.join("pipeline-parallel-ok"),
        Duration::from_secs(5),
    );
}

#[test]
fn declared_agent_result_fields_match_a_real_run_result() {
    require_node();
    let fixture = fixture_home();
    init_sdk(&fixture);
    let dts = fs::read_to_string(fixture.scripts.join("brgr-workflow.d.mts")).unwrap();
    let mut declared = declared_interface_fields(&dts, "AgentResult");
    declared.sort();
    let script = write_script(&fixture, "result-fields.mjs", RESULT_FIELDS_SCRIPT);
    let run_id = start_workflow(&fixture, &script);
    wait_run_state(&fixture, &run_id, "completed", Duration::from_secs(90));
    let sealed = agent_result(&fixture);
    let mut actual: Vec<String> = sealed.as_object().unwrap().keys().cloned().collect();
    actual.sort();
    assert_eq!(
        actual, declared,
        "result JSON keys {actual:?} != declared AgentResult {declared:?} payload={sealed}"
    );
}
