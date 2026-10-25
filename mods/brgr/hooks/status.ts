// Pure reading of `brgr --json status` and `brgr --json status TASK --tree`.
// The list is the owner's tasks (task_commands.rs). The tree row carries
// unanswered_questions and outcome (tree_status.rs). The mod never decides.

export const POLL_MS = 5000
export const STATUS_TIMEOUT_MS = 3000
export const RUN_TIMEOUT_MS = 10_000
export const STORE_KEY = "announced"
export const MISSING_TOAST = "brgr: the brgr executable is missing"
export const STATUS_FAILED_TOAST = "brgr: brgr status failed"

export type TaskView = {
  id: string
  state: string
  harness: string
  revision: number
  questions: number
  outcome: string | null
}

export type Memory = {
  questions: Record<string, number>
  results: Record<string, string>
  failure: "missing" | "exit" | null
}

type Listed = {
  id: string
  state: string
  harness: string
  revision: number
}

type TreeNode = {
  questions: number
  outcome: string | null
  harness: string
  state: string
}

export function emptyMemory(): Memory {
  return { questions: {}, results: {}, failure: null }
}

export function isMemory(value: unknown): value is Memory {
  if (typeof value !== "object" || value === null) return false
  const record = value as Record<string, unknown>
  return (
    typeof record.questions === "object" &&
    record.questions !== null &&
    typeof record.results === "object" &&
    record.results !== null &&
    (record.failure === null || record.failure === "missing" || record.failure === "exit")
  )
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}

function stateText(value: unknown): string | null {
  if (typeof value === "string" && value.length > 0) return value
  if (value === null) return "unknown"
  return null
}

export function parseStatusList(value: unknown): Listed[] | null {
  if (!Array.isArray(value)) return null
  const listed: Listed[] = []
  for (const item of value) {
    if (!isRecord(item) || !isRecord(item.task)) return null
    const id = item.task.task_id
    const state = stateText(item.state)
    if (typeof id !== "string" || id.length === 0 || state === null) return null
    const route = item.task.route
    const harness = isRecord(route) && typeof route.harness_id === "string" ? route.harness_id : ""
    const revision = typeof item.task.revision === "number" ? item.task.revision : 0
    listed.push({ id, state, harness, revision })
  }
  return listed
}

export function parseTree(value: unknown): Record<string, TreeNode> | null {
  if (!isRecord(value) || !Array.isArray(value.nodes)) return null
  const nodes: Record<string, TreeNode> = {}
  for (const node of value.nodes) {
    if (!isRecord(node) || typeof node.task_id !== "string" || node.task_id.length === 0) return null
    const questions = typeof node.unanswered_questions === "number" ? node.unanswered_questions : 0
    const outcome = typeof node.outcome === "string" && node.outcome.length > 0 ? node.outcome : null
    const harness = typeof node.harness === "string" ? node.harness : ""
    const state = stateText(node.state) ?? ""
    nodes[node.task_id] = { questions, outcome, harness, state }
  }
  return nodes
}

export function joinTasks(listed: readonly Listed[], trees: Readonly<Record<string, TreeNode | undefined>>): TaskView[] {
  return listed.map(item => {
    const node = trees[item.id]
    return {
      id: item.id,
      state: node?.state || item.state,
      harness: node?.harness || item.harness,
      revision: item.revision,
      questions: node?.questions ?? 0,
      outcome: node?.outcome ?? null,
    }
  })
}

export function bucket(task: TaskView): "running" | "asking" | "done" {
  if (task.state === "terminal") return "done"
  if (task.questions > 0) return "asking"
  return "running"
}

export function summary(tasks: readonly TaskView[]): string | undefined {
  if (tasks.length === 0) return undefined
  let running = 0
  let asking = 0
  let done = 0
  for (const task of tasks) {
    const kind = bucket(task)
    if (kind === "running") running += 1
    else if (kind === "asking") asking += 1
    else done += 1
  }
  return `brgr: ${running} running · ${asking} asking · ${done} done`
}

export function id8(id: string): string {
  return id.slice(0, 8)
}

export function activeText(tasks: readonly TaskView[]): string {
  const lines = tasks
    .filter(task => task.state !== "terminal")
    .map(task => `${id8(task.id)} ${task.state} ${task.harness}`)
  return lines.length === 0 ? "brgr: no active tasks" : lines.join("\n")
}

export function noteAnnouncements(tasks: readonly TaskView[], memory: Memory): string[] {
  const toasts: string[] = []
  for (const task of tasks) {
    const seen = memory.questions[task.id] ?? 0
    for (let count = seen; count < task.questions; count += 1) {
      toasts.push(`brgr: ${id8(task.id)} asks you`)
    }
    memory.questions[task.id] = task.questions
    if (task.outcome !== null) {
      const key = `${task.revision}:${task.outcome}`
      if (memory.results[task.id] !== key) {
        toasts.push(`brgr: ${id8(task.id)} finished: ${task.outcome}`)
        memory.results[task.id] = key
      }
    }
  }
  memory.failure = null
  return toasts
}

export type RunInput = {
  objective: string
  harness: string
  criterion: string[]
  model?: string
  captureDiff: boolean
}

export function readRunInput(value: unknown): { ok: true; input: RunInput } | { ok: false; text: string } {
  if (!isRecord(value)) return { ok: false, text: "brgr_run input must be an object" }
  const objective = value.objective
  const harness = value.harness
  const criterion = value.criterion
  if (typeof objective !== "string" || objective.trim().length === 0) {
    return { ok: false, text: "objective must be a string" }
  }
  if (typeof harness !== "string" || harness.trim().length === 0) {
    return { ok: false, text: "harness must be a string" }
  }
  if (!Array.isArray(criterion) || criterion.some(item => typeof item !== "string" || item.trim().length === 0)) {
    return { ok: false, text: "criterion must be a list of strings" }
  }
  const model = typeof value.model === "string" && value.model.trim().length > 0 ? value.model : undefined
  return {
    ok: true,
    input: {
      objective,
      harness,
      criterion: criterion.filter((item): item is string => typeof item === "string"),
      model,
      captureDiff: value.capture_diff === true,
    },
  }
}

export function runArgv(input: RunInput): string[] {
  const argv = ["brgr", "--json", "run", input.objective, "--harness", input.harness]
  for (const criterion of input.criterion) argv.push("--criterion", criterion)
  if (input.model !== undefined) argv.push("--model", input.model)
  if (input.captureDiff) argv.push("--capture-diff")
  argv.push("--mode", "auto")
  return argv
}
