import { execFile } from "node:child_process";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);
const TEMPFAIL = 75;
const WAIT_POLL_MS = 200;
const BACKOFF_START_MS = 1000;
const BACKOFF_CAP_MS = 15000;
const MAX_OUTPUT = 16 * 1024 * 1024;

let agentsStarted = 0;

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function isForbidden(value) {
  if (value === undefined || typeof value === "function" || typeof value === "bigint") {
    return true;
  }
  if (typeof value === "number" && !Number.isFinite(value)) {
    return true;
  }
  return value instanceof Date || value instanceof Map || value instanceof Set;
}

function walk(value, seen) {
  if (isForbidden(value)) {
    throw new TypeError("agent() options contain an invalid value");
  }
  if (value === null || typeof value !== "object") {
    return;
  }
  if (seen.has(value)) {
    return;
  }
  seen.add(value);
  if (Array.isArray(value)) {
    for (const item of value) {
      walk(item, seen);
    }
    return;
  }
  for (const key of Reflect.ownKeys(value)) {
    walk(value[key], seen);
  }
}

function requireString(options, field, required) {
  if (!Object.hasOwn(options, field)) {
    if (required) {
      throw new TypeError(`agent() ${field} must be a string`);
    }
    return;
  }
  if (typeof options[field] !== "string") {
    throw new TypeError(`agent() ${field} must be a string`);
  }
}

function validateOptions(options) {
  if (options === null || typeof options !== "object" || Array.isArray(options)) {
    throw new TypeError("agent() options must be an object");
  }
  walk(options, new WeakSet());
  requireString(options, "task", true);
  requireString(options, "harness", false);
  requireString(options, "model", false);
  requireString(options, "effort", false);
  requireString(options, "mode", false);
  requireString(options, "key", false);
  if (Object.hasOwn(options, "criteria")) {
    if (
      !Array.isArray(options.criteria) ||
      options.criteria.some((item) => typeof item !== "string")
    ) {
      throw new TypeError("agent() criteria must be an array of strings");
    }
  }
}

function wrapChildError(error) {
  const stderr = error && typeof error.stderr === "string" ? error.stderr : "";
  return new Error(stderr.trim() || (error && error.message) || "brgr command failed");
}

function parseJson(stdout, command) {
  try {
    return JSON.parse(String(stdout ?? "").trim());
  } catch (error) {
    throw new Error(`${command} did not return JSON: ${error.message}`);
  }
}

async function execBrgr(args) {
  const bin = process.env.BRGR_BIN;
  if (!bin) {
    throw new Error("BRGR_BIN is not set");
  }
  return execFileAsync(bin, ["--json", ...args], {
    encoding: "utf8",
    maxBuffer: MAX_OUTPUT,
  });
}

async function invoke(args) {
  try {
    return await execBrgr(args);
  } catch (error) {
    throw wrapChildError(error);
  }
}

async function requireRunning(runId) {
  const status = parseJson((await invoke(["workflow", "status", runId])).stdout, "workflow status");
  if (status.state !== "running") {
    throw new Error(`workflow run ${runId} is ${status.state}`);
  }
}

async function launchStep(args, runId) {
  let delay = BACKOFF_START_MS;
  for (;;) {
    try {
      return await execBrgr(["run", ...args]);
    } catch (error) {
      if (error && error.code === TEMPFAIL) {
        await requireRunning(runId);
        await sleep(delay);
        delay = Math.min(delay * 2, BACKOFF_CAP_MS);
        continue;
      }
      throw wrapChildError(error);
    }
  }
}

function mapOutcome(value) {
  if (value === "candidate") {
    return "completed";
  }
  if (value === "failed" || value === "cancelled" || value === "lost") {
    return value;
  }
  throw new Error(`unexpected sealed outcome ${value}`);
}

function reportFromResult(payload) {
  const artifacts = Array.isArray(payload.artifacts) ? payload.artifacts : [];
  for (const artifact of artifacts) {
    if (typeof artifact.text === "string") {
      return artifact.text;
    }
  }
  if (payload.result && typeof payload.result.error === "string") {
    return payload.result.error;
  }
  return "";
}

function sealedFrom(waited, detailed) {
  const sealed = detailed.result && typeof detailed.result === "object" ? detailed.result : {};
  return {
    outcome: mapOutcome(sealed.outcome || waited.outcome),
    task_id: waited.task_id,
    revision: sealed.revision ?? waited.revision,
    result_id: sealed.result_id || waited.result_id,
    report: reportFromResult(detailed),
  };
}

async function waitForSealed(taskId, runId) {
  let markedBlocked = false;
  for (;;) {
    const waited = parseJson((await invoke(["wait", taskId])).stdout, "wait");
    if (waited.state === "awaiting_input") {
      if (!markedBlocked) {
        await invoke(["workflow", "set-state", runId, "blocked"]);
        markedBlocked = true;
      }
      await sleep(WAIT_POLL_MS);
      continue;
    }
    if (markedBlocked) {
      await invoke(["workflow", "set-state", runId, "running"]);
      markedBlocked = false;
    }
    if (!waited.result_id) {
      throw new Error("brgr wait returned neither a result nor awaiting_input");
    }
    const detailed = parseJson((await invoke(["result", taskId])).stdout, "result");
    return sealedFrom(waited, detailed);
  }
}

function runArgs(options, runId, key) {
  const args = [options.task, "--workflow", runId, "--step", key];
  if (options.harness) {
    args.push("--harness", options.harness);
  }
  if (options.model) {
    args.push("--model", options.model);
  }
  if (options.effort) {
    args.push("--effort", options.effort);
  }
  if (options.mode) {
    args.push("--mode", options.mode);
  }
  if (Array.isArray(options.criteria)) {
    for (const criterion of options.criteria) {
      args.push("--criterion", criterion);
    }
  }
  return args;
}

export async function agent(options) {
  validateOptions(options);
  const runId = process.env.BRGR_WORKFLOW_RUN;
  if (!runId) {
    throw new Error("BRGR_WORKFLOW_RUN is not set");
  }
  const key = Object.hasOwn(options, "key") ? options.key : String(agentsStarted);
  agentsStarted += 1;
  const launched = parseJson(
    (await launchStep(runArgs(options, runId, key), runId)).stdout,
    "run",
  );
  if (!launched.task_id) {
    throw new Error("brgr run did not return a task_id");
  }
  return waitForSealed(launched.task_id, runId);
}

export async function parallel(thunks) {
  if (!Array.isArray(thunks)) {
    throw new TypeError("parallel() requires an array of thunks");
  }
  const started = [];
  for (const thunk of thunks) {
    if (typeof thunk !== "function") {
      throw new TypeError("parallel() thunks must be functions");
    }
    started.push(thunk());
  }
  return Promise.all(started);
}
