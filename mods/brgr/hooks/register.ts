import type { EngineInterface, Register } from "claude-code"

import {
  MISSING_TOAST,
  POLL_MS,
  RUN_TIMEOUT_MS,
  STATUS_FAILED_TOAST,
  STATUS_TIMEOUT_MS,
  STORE_KEY,
  activeText,
  emptyMemory,
  isMemory,
  joinTasks,
  noteAnnouncements,
  parseStatusList,
  parseTree,
  readRunInput,
  runArgv,
  summary,
} from "./status.ts"
import type { Memory, TaskView } from "./status.ts"

let pollTimer: { cancel: () => void } | undefined
let polling = false

type Ran = { ok: true; stdout: string } | { ok: false; kind: "missing" | "exit"; detail: string }

async function runBrgr(api: EngineInterface, argv: readonly string[], timeoutMs: number): Promise<Ran> {
  try {
    const ran = await api.process.run(argv, { timeoutMs })
    if (ran.isStdoutTruncated || ran.isStderrTruncated) {
      return { ok: false, kind: "exit", detail: "brgr output exceeded the limit" }
    }
    if (ran.exitCode !== 0) {
      const detail = (ran.stderr || ran.stdout || `exit ${ran.exitCode}`).slice(0, 500)
      return { ok: false, kind: "exit", detail }
    }
    return { ok: true, stdout: ran.stdout }
  } catch (error) {
    const detail = error instanceof Error ? error.message : String(error)
    return { ok: false, kind: "missing", detail: detail.slice(0, 500) }
  }
}

async function loadTasks(api: EngineInterface): Promise<{ ok: true; tasks: TaskView[] } | { ok: false; kind: "missing" | "exit"; detail: string }> {
  const list = await runBrgr(api, ["brgr", "--json", "status"], STATUS_TIMEOUT_MS)
  if (!list.ok) return list
  let parsed: unknown
  try {
    parsed = JSON.parse(list.stdout)
  } catch {
    return { ok: false, kind: "exit", detail: "brgr status was not JSON" }
  }
  const listed = parseStatusList(parsed)
  if (listed === null) return { ok: false, kind: "exit", detail: "brgr status had an unexpected shape" }
  const nodesByTask: Record<string, { questions: number; outcome: string | null; harness: string; state: string } | undefined> = {}
  for (const item of listed) {
    const tree = await runBrgr(api, ["brgr", "--json", "status", item.id, "--tree"], STATUS_TIMEOUT_MS)
    if (!tree.ok) return tree
    let treeParsed: unknown
    try {
      treeParsed = JSON.parse(tree.stdout)
    } catch {
      return { ok: false, kind: "exit", detail: "brgr status --tree was not JSON" }
    }
    const nodes = parseTree(treeParsed)
    if (nodes === null) return { ok: false, kind: "exit", detail: "brgr status --tree had an unexpected shape" }
    nodesByTask[item.id] = nodes[item.id]
  }
  return { ok: true, tasks: joinTasks(listed, nodesByTask) }
}

async function readMemory(api: EngineInterface): Promise<Memory> {
  try {
    const stored = await api.store.get(STORE_KEY)
    if (!isMemory(stored)) return emptyMemory()
    return {
      questions: { ...stored.questions },
      results: { ...stored.results },
      failure: stored.failure,
    }
  } catch {
    return emptyMemory()
  }
}

async function poll(api: EngineInterface): Promise<void> {
  if (polling) return
  polling = true
  try {
    const surfaces = await api.session.surfaces()
    if (surfaces.length === 0) return
    const loaded = await loadTasks(api)
    const memory = await readMemory(api)
    if (!loaded.ok) {
      if (memory.failure !== loaded.kind) {
        api.ui.toast(loaded.kind === "missing" ? MISSING_TOAST : STATUS_FAILED_TOAST)
        memory.failure = loaded.kind
      }
      api.ui.status(undefined)
      await api.store.set(STORE_KEY, memory)
      return
    }
    for (const text of noteAnnouncements(loaded.tasks, memory)) api.ui.toast(text)
    api.ui.status(summary(loaded.tasks))
    await api.store.set(STORE_KEY, memory)
  } catch {
    // A timer that throws three times unloads the mod.
  } finally {
    polling = false
  }
}

async function runTool(api: EngineInterface, event: unknown): Promise<{ result: unknown; text: string; isError?: true }> {
  const parsed = readRunInput(event)
  if (!parsed.ok) return { isError: true, result: parsed.text, text: parsed.text }
  const argv = runArgv(parsed.input)
  const ran = await runBrgr(api, argv, RUN_TIMEOUT_MS)
  if (!ran.ok) return { isError: true, result: ran.detail, text: ran.detail }
  try {
    const value: unknown = JSON.parse(ran.stdout)
    return { result: value, text: ran.stdout }
  } catch {
    return { isError: true, result: ran.stdout, text: ran.stdout }
  }
}

export const register: Register = on => {
  on("session.start", async ($, event, next) => {
    try {
      await $.command.register({
        name: "brgr",
        description: "List active brgr tasks. Read-only.",
      })
      await $.tool.register({
        name: "brgr_run",
        description:
          "Start a brgr run and return its JSON. Always passes --mode auto. Does not accept, reject, apply, revise, or acknowledge.",
        isDeferred: false,
        inputSchema: {
          type: "object",
          additionalProperties: false,
          required: ["objective", "harness", "criterion"],
          properties: {
            objective: { type: "string" },
            harness: { type: "string" },
            criterion: { type: "array", items: { type: "string" } },
            model: { type: "string" },
            capture_diff: { type: "boolean" },
          },
        },
      })
      if (event.isInteractive && event.surface !== null) {
        pollTimer?.cancel()
        pollTimer = $.clock.every(POLL_MS, () => {
          void poll($)
        })
      }
    } catch {
      // Still let the session start when registration fails.
    }
    return next(event)
  }).catch(($, event) => ({ cwd: event.cwd }))

  on("command.run", { command: "brgr" }, async $ => {
    const loaded = await loadTasks($)
    if (!loaded.ok) return { text: loaded.kind === "missing" ? MISSING_TOAST : STATUS_FAILED_TOAST }
    return { text: activeText(loaded.tasks) }
  }).catch(() => ({ text: STATUS_FAILED_TOAST }))

  on("tool.call", { tool: "mcp__brgr__brgr_run" }, async ($, event) => runTool($, event)).catch(() => ({
    isError: true as const,
    result: "brgr_run failed",
    text: "brgr_run failed",
  }))
}
