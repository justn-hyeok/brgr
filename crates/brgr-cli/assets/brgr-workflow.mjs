import { AsyncLocalStorage } from "node:async_hooks";
import { execFile } from "node:child_process";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);
const TEMPFAIL = 75;
const WAIT_POLL_MS = 200;
const BACKOFF_START_MS = 1000;
const BACKOFF_CAP_MS = 15000;
const MAX_OUTPUT = 16 * 1024 * 1024;
const MAX_DEADLINE_SECONDS = 86400;
const MAX_RETRIES = 3;
const MAX_PIPELINE_STAGES = 64;
const pipelineStore = new AsyncLocalStorage();
const WAIT_TIMEOUT_NEEDLE = "did not produce a terminal result before the wait timeout";
const ALREADY_TERMINAL_NEEDLE = "is already terminal";

let agentsStarted = 0;
let parallelDepth = 0;

function bumpAgentCounter() {
  if (agentsStarted >= Number.MAX_SAFE_INTEGER) {
    throw new TypeError("agent() automatic key counter overflowed");
  }
  agentsStarted += 1;
}

function reserveAutoKeys(count) {
  if (agentsStarted > Number.MAX_SAFE_INTEGER - count) {
    throw new TypeError("agent() automatic key counter overflowed");
  }
  const keys = [];
  for (let i = 0; i < count; i += 1) {
    keys.push(String(agentsStarted));
    agentsStarted += 1;
  }
  return keys;
}

function takeAutoKey() {
  const slot = pipelineStore.getStore();
  if (slot && slot.index < slot.keys.length && typeof slot.keys[slot.index] === "string") {
    const key = slot.keys[slot.index];
    slot.keys[slot.index] = null;
    return key;
  }
  const key = String(agentsStarted);
  bumpAgentCounter();
  return key;
}

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
  if (Object.hasOwn(options, "key") && options.key.includes("~")) {
    throw new TypeError("agent() key must not contain '~'");
  }
  if (Object.hasOwn(options, "criteria")) {
    if (
      !Array.isArray(options.criteria) ||
      options.criteria.some((item) => typeof item !== "string")
    ) {
      throw new TypeError("agent() criteria must be an array of strings");
    }
  }
  requireDeadline(options);
  requireRetries(options);
}

function requireDeadline(options) {
  if (!Object.hasOwn(options, "deadline")) {
    return;
  }
  const value = options.deadline;
  if (
    typeof value !== "number" ||
    !Number.isInteger(value) ||
    value < 1 ||
    value > MAX_DEADLINE_SECONDS
  ) {
    throw new TypeError(
      "agent() deadline must be a positive integer number of seconds at most 86400",
    );
  }
}

function requireRetries(options) {
  if (!Object.hasOwn(options, "retries")) {
    return 0;
  }
  const value = options.retries;
  if (
    typeof value !== "number" ||
    !Number.isInteger(value) ||
    value < 0 ||
    value > MAX_RETRIES
  ) {
    throw new TypeError("agent() retries must be an integer from 0 to 3");
  }
  return value;
}

function attemptKey(base, attempt) {
  if (attempt <= 1) {
    return base;
  }
  return `${base}~retry${attempt - 1}`;
}

function shouldRetry(sealed, attempt, retries) {
  return sealed.outcome === "failed" && !sealed.timed_out && attempt <= retries;
}

function wrapChildError(error) {
  const stderr = error && typeof error.stderr === "string" ? error.stderr.trim() : "";
  const fallback = (error && error.message) || "brgr command failed";
  const text = stderr || fallback;
  const message = text.startsWith("Error:") ? text.slice("Error:".length).trimStart() : text;
  return new Error(message);
}

function option(flag, value) {
  return `--${flag}=${value}`;
}

function sourcePanePrefix(mode) {
  const pane = process.env.BRGR_WORKFLOW_SOURCE_PANE;
  if (!pane || mode === "headless") {
    return [];
  }
  return [option("source-pane", pane)];
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

async function requireActive(runId) {
  const status = parseJson((await invoke(["workflow", "status", runId])).stdout, "workflow status");
  if (status.state !== "running" && status.state !== "blocked") {
    throw new Error(`workflow run ${runId} is ${status.state}`);
  }
}

async function launchStep(args, runId, mode) {
  let delay = BACKOFF_START_MS;
  for (;;) {
    try {
      return await execBrgr([...sourcePanePrefix(mode), "run", ...args]);
    } catch (error) {
      if (error && error.code === TEMPFAIL) {
        await requireActive(runId);
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
    timed_out: false,
  };
}

function remainingSeconds(createdAt, deadlineSeconds) {
  if (!Number.isFinite(createdAt) || !Number.isFinite(deadlineSeconds)) {
    return 0;
  }
  const now = Math.floor(Date.now() / 1000);
  const elapsed = Math.max(0, now - createdAt);
  return deadlineSeconds - elapsed;
}

function waitTimeoutSeconds(remaining) {
  if (remaining == null) {
    return null;
  }
  if (!Number.isFinite(remaining) || remaining <= 0) {
    return 1;
  }
  return Math.min(MAX_DEADLINE_SECONDS, Math.max(1, Math.ceil(remaining)));
}

function deadlineTimedOut(createdAt, deadlineSeconds, outcome) {
  return (
    outcome === "cancelled" &&
    createdAt != null &&
    remainingSeconds(createdAt, deadlineSeconds) <= 0
  );
}

function isWaitTimeout(error) {
  return error instanceof Error && error.message.includes(WAIT_TIMEOUT_NEEDLE);
}

function isAlreadyTerminal(error) {
  return error instanceof Error && error.message.includes(ALREADY_TERMINAL_NEEDLE);
}

async function readCreatedAt(taskId, runId) {
  for (;;) {
    await requireActive(runId);
    const status = parseJson((await invoke(["status", "--", taskId])).stdout, "status");
    if (typeof status.created_at === "number" && Number.isFinite(status.created_at)) {
      return status.created_at;
    }
    await sleep(WAIT_POLL_MS);
  }
}

async function waitOnce(taskId, timeoutSeconds) {
  const args = ["wait"];
  if (timeoutSeconds != null) {
    args.push(option("timeout-seconds", timeoutSeconds));
  }
  args.push("--", taskId);
  try {
    return { waited: parseJson((await invoke(args)).stdout, "wait"), timedOut: false };
  } catch (error) {
    if (timeoutSeconds != null && isWaitTimeout(error)) {
      return { waited: null, timedOut: true };
    }
    throw error;
  }
}

async function markBlocked(runId, markedBlocked) {
  if (!markedBlocked) {
    await invoke(["workflow", "set-state", runId, "blocked"]);
  }
  return true;
}

async function clearBlocked(runId, markedBlocked) {
  if (markedBlocked) {
    await invoke(["workflow", "set-state", runId, "running"]);
  }
  return false;
}

async function sealedIfCurrent(taskId, waited) {
  const status = parseJson((await invoke(["status", "--", taskId])).stdout, "status");
  const latest = status.task && status.task.revision;
  if (typeof latest !== "number") {
    throw new Error("brgr status did not return task.revision");
  }
  if (latest !== waited.revision) {
    return null;
  }
  const detailed = parseJson((await invoke(["result", "--", taskId])).stdout, "result");
  return sealedFrom(waited, detailed);
}

async function cancelAndWait(taskId, runId, createdAt, deadlineSeconds) {
  try {
    await invoke(["cancel", "--", taskId]);
  } catch (error) {
    if (!isAlreadyTerminal(error)) {
      throw error;
    }
  }
  const sealed = await waitForSealed(taskId, runId);
  return {
    ...sealed,
    timed_out: deadlineTimedOut(createdAt, deadlineSeconds, sealed.outcome),
  };
}

async function waitForSealed(taskId, runId, deadlineSeconds) {
  const createdAt =
    typeof deadlineSeconds === "number" ? await readCreatedAt(taskId, runId) : null;
  let markedBlocked = false;
  for (;;) {
    await requireActive(runId);
    const remaining =
      createdAt == null ? null : remainingSeconds(createdAt, deadlineSeconds);
    const waitTimeout = waitTimeoutSeconds(remaining);
    const { waited, timedOut } = await waitOnce(taskId, waitTimeout);
    if (waited && waited.state === "awaiting_input") {
      markedBlocked = await markBlocked(runId, markedBlocked);
      await sleep(WAIT_POLL_MS);
      continue;
    }
    markedBlocked = await clearBlocked(runId, markedBlocked);
    if (waited && waited.result_id) {
      const sealed = await sealedIfCurrent(taskId, waited);
      if (sealed) {
        return {
          ...sealed,
          timed_out: deadlineTimedOut(createdAt, deadlineSeconds, sealed.outcome),
        };
      }
      await sleep(WAIT_POLL_MS);
      continue;
    }
    if (timedOut && remaining != null && remainingSeconds(createdAt, deadlineSeconds) <= 0) {
      return cancelAndWait(taskId, runId, createdAt, deadlineSeconds);
    }
    if (timedOut && remaining != null) {
      continue;
    }
    throw new Error("brgr wait returned neither a result nor awaiting_input");
  }
}

function runArgs(options, runId, key, retries) {
  const args = [option("workflow", runId), option("step", key)];
  if (options.harness) {
    args.push(option("harness", options.harness));
  }
  if (options.model) {
    args.push(option("model", options.model));
  }
  if (options.effort) {
    args.push(option("effort", options.effort));
  }
  if (options.mode) {
    args.push(option("mode", options.mode));
  }
  if (Array.isArray(options.criteria)) {
    for (const criterion of options.criteria) {
      args.push(option("criterion", criterion));
    }
  }
  if (Object.hasOwn(options, "deadline")) {
    args.push(option("step-deadline", options.deadline));
  }
  if (retries > 0) {
    args.push(option("step-retries", retries));
  }
  args.push("--", options.task);
  return args;
}

async function runAttempt(options, runId, key, retries) {
  const launched = parseJson(
    (await launchStep(runArgs(options, runId, key, retries), runId, options.mode)).stdout,
    "run",
  );
  if (!launched.task_id) {
    throw new Error("brgr run did not return a task_id");
  }
  return waitForSealed(launched.task_id, runId, options.deadline);
}

export async function agent(options) {
  validateOptions(options);
  const retries = requireRetries(options);
  const runId = process.env.BRGR_WORKFLOW_RUN;
  if (!runId) {
    throw new Error("BRGR_WORKFLOW_RUN is not set");
  }
  const baseKey = Object.hasOwn(options, "key") ? options.key : takeAutoKey();
  if (Object.hasOwn(options, "key")) {
    bumpAgentCounter();
  }
  let sealed;
  let attempts = 0;
  for (let attempt = 1; attempt <= retries + 1; attempt += 1) {
    await requireActive(runId);
    sealed = await runAttempt(options, runId, attemptKey(baseKey, attempt), retries);
    attempts = attempt;
    if (!shouldRetry(sealed, attempt, retries)) {
      break;
    }
  }
  return { ...sealed, attempts };
}

export async function parallel(thunks) {
  if (!Array.isArray(thunks)) {
    throw new TypeError("parallel() requires an array of thunks");
  }
  for (const thunk of thunks) {
    if (typeof thunk !== "function") {
      throw new TypeError("parallel() thunks must be functions");
    }
  }
  parallelDepth += 1;
  try {
    const started = [];
    for (const thunk of thunks) {
      started.push(thunk());
    }
    return await Promise.all(started);
  } finally {
    parallelDepth -= 1;
  }
}

function validatePipeline(stages, options) {
  if (!Array.isArray(stages)) {
    throw new TypeError("pipeline() requires an array of stages");
  }
  if (stages.length === 0) {
    throw new TypeError("pipeline() requires a non-empty array of stages");
  }
  if (stages.length > MAX_PIPELINE_STAGES) {
    throw new TypeError("pipeline() supports at most 64 stages");
  }
  for (const stage of stages) {
    if (typeof stage !== "function") {
      throw new TypeError("pipeline() stages must be functions");
    }
  }
  if (options === undefined) {
    return;
  }
  if (options === null || typeof options !== "object" || Array.isArray(options)) {
    throw new TypeError("pipeline() options must be an object");
  }
  walk(options, new WeakSet());
  if (Object.hasOwn(options, "key")) {
    if (typeof options.key !== "string" || options.key.length === 0) {
      throw new TypeError("pipeline() key must be a string");
    }
    if (options.key.includes("~")) {
      throw new TypeError("pipeline() key must not contain '~'");
    }
  }
}

function pipelineKeys(stages, options) {
  if (options && Object.hasOwn(options, "key")) {
    return stages.map((_, index) => `${options.key}-${index}`);
  }
  return reserveAutoKeys(stages.length);
}

export async function pipeline(stages, options) {
  validatePipeline(stages, options);
  if (parallelDepth > 0 && !(options && Object.hasOwn(options, "key"))) {
    throw new TypeError("pipeline() inside parallel() requires a key prefix");
  }
  const keys = pipelineKeys(stages, options);
  const called = stages.map(() => false);
  return pipelineStore.run({ keys, index: 0 }, async () => {
    const results = [];
    for (let i = 0; i < stages.length; i += 1) {
      if (called[i]) {
        throw new Error("pipeline() stage called twice");
      }
      called[i] = true;
      pipelineStore.getStore().index = i;
      const previous = i === 0 ? undefined : results[i - 1];
      const result = await stages[i](previous);
      results.push(result);
      if (!result || result.outcome !== "completed") {
        return { results, stopped_at: i };
      }
    }
    return { results, stopped_at: null };
  });
}
