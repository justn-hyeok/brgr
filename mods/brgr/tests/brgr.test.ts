// @ts-nocheck — engine types live in Claude Code, not this repo.
import type { On } from "claude-code"
import { expect, mock, test, type Engine } from "claude-code/testing"

type FakeTask = {
  id: string
  state: string
  harness: string
  revision: number
  questions: number
  outcome: string | null
}

type Mode = "ok" | "missing" | "exit"

const RUN_JSON = JSON.stringify({
  task_id: "aaaaaaaa-0000-4000-8000-000000000009",
  mode: "auto",
  mode_reason: "the owner decides",
})

function listJson(tasks: readonly FakeTask[]): string {
  return JSON.stringify(
    tasks.map(task => ({
      task: {
        task_id: task.id,
        revision: task.revision,
        route: { harness_id: task.harness },
      },
      state: task.state,
    })),
  )
}

function treeJson(task: FakeTask): string {
  return JSON.stringify({
    root_task_id: task.id,
    nodes: [
      {
        task_id: task.id,
        parent_task_id: null,
        depth: 0,
        harness: task.harness,
        state: task.state,
        outcome: task.outcome,
        waiting_for: task.questions > 0 ? "question" : null,
        unanswered_questions: task.questions,
      },
    ],
  })
}

async function boot(
  $: Engine,
  on: On,
  options: {
    tasks: FakeTask[]
    mode?: Mode
    surfaces?: readonly ("terminal" | "desktop" | "vscode" | "mobile")[]
    surface?: "terminal" | null
    interactive?: boolean
  },
) {
  const clock = mock.clock(on)
  mock.store(on)
  const status: Array<string | undefined> = []
  const toasts: string[] = []
  const argv: string[][] = []
  const mode = options.mode ?? "ok"
  on("ui.status", (_$, event) => {
    status.push(event.text)
    return { value: undefined }
  })
  on("ui.toast", (_$, event) => {
    toasts.push(event.text)
    return { value: undefined }
  })
  on("session.surfaces", () => ({ value: options.surfaces ?? ["terminal"] }))
  on("session.start", (_$, event) => ({ cwd: event.cwd }))
  on("command.register", () => ({ value: { command: "brgr" } }))
  on("tool.register", () => ({ value: { tool: "mcp__brgr__brgr_run" } }))
  on("process.run", (_$, event) => {
    const args = [...event.argv]
    argv.push(args)
    if (mode === "missing") return { deny: "spawn brgr ENOENT" }
    if (mode === "exit") {
      return {
        value: {
          exitCode: 1,
          stdout: "",
          stderr: "brgr: status failed",
          isStdoutTruncated: false,
          isStderrTruncated: false,
        },
      }
    }
    if (args[1] === "--json" && args[2] === "run") {
      return {
        value: {
          exitCode: 0,
          stdout: RUN_JSON,
          stderr: "",
          isStdoutTruncated: false,
          isStderrTruncated: false,
        },
      }
    }
    if (args[1] === "--json" && args[2] === "status" && args.length === 3) {
      return {
        value: {
          exitCode: 0,
          stdout: listJson(options.tasks),
          stderr: "",
          isStdoutTruncated: false,
          isStderrTruncated: false,
        },
      }
    }
    const task = options.tasks.find(item => item.id === args[3])
    return {
      value: {
        exitCode: task ? 0 : 1,
        stdout: task ? treeJson(task) : "",
        stderr: task ? "" : "missing task",
        isStdoutTruncated: false,
        isStderrTruncated: false,
      },
    }
  })
  await $.session.start({
    cwd: "/tmp",
    surface: options.surface === undefined ? "terminal" : options.surface,
    isInteractive: options.interactive !== false,
  })
  return { clock, status, toasts, argv }
}

const running = (id: string, harness = "local.gjc"): FakeTask => ({
  id,
  state: "running",
  harness,
  revision: 1,
  questions: 0,
  outcome: null,
})

const asking = (id: string): FakeTask => ({
  id,
  state: "running",
  harness: "local.claude-code",
  revision: 1,
  questions: 1,
  outcome: null,
})

const done = (id: string): FakeTask => ({
  id,
  state: "terminal",
  harness: "local.gjc",
  revision: 1,
  questions: 0,
  outcome: "candidate",
})

test("no tasks clear the status line", async ($, on) => {
  const session = await boot($, on, { tasks: [] })
  await session.clock.advance(5000)
  expect(session.status).toEqual([undefined])
})

test("running tasks set the status line without asking or done", async ($, on) => {
  const session = await boot($, on, {
    tasks: [
      running("11111111-0000-4000-8000-000000000001"),
      running("22222222-0000-4000-8000-000000000002"),
    ],
  })
  await session.clock.advance(5000)
  expect(session.status.at(-1)).toBe("brgr: 2 running · 0 asking · 0 done")
})

test("a worker question counts as asking", async ($, on) => {
  const session = await boot($, on, {
    tasks: [asking("33333333-0000-4000-8000-000000000003")],
  })
  await session.clock.advance(5000)
  expect(session.status.at(-1)).toBe("brgr: 0 running · 1 asking · 0 done")
})

test("terminal tasks count as done", async ($, on) => {
  const session = await boot($, on, {
    tasks: [
      done("44444444-0000-4000-8000-000000000004"),
      done("55555555-0000-4000-8000-000000000005"),
      done("66666666-0000-4000-8000-000000000006"),
    ],
  })
  await session.clock.advance(5000)
  expect(session.status.at(-1)).toBe("brgr: 0 running · 0 asking · 3 done")
})

test("mixed tasks summarize running, asking, and done", async ($, on) => {
  const session = await boot($, on, {
    tasks: [
      running("11111111-0000-4000-8000-000000000001"),
      running("22222222-0000-4000-8000-000000000002"),
      asking("33333333-0000-4000-8000-000000000003"),
      done("44444444-0000-4000-8000-000000000004"),
      done("55555555-0000-4000-8000-000000000005"),
      done("66666666-0000-4000-8000-000000000006"),
    ],
  })
  await session.clock.advance(5000)
  expect(session.status.at(-1)).toBe("brgr: 2 running · 1 asking · 3 done")
})

test("a question and a result toast once across a repeated poll and a reload", async ($, on) => {
  const tasks = [
    asking("33333333-0000-4000-8000-000000000003"),
    done("44444444-0000-4000-8000-000000000004"),
  ]
  const session = await boot($, on, { tasks })
  await session.clock.advance(5000)
  expect(session.toasts).toEqual([
    "brgr: 33333333 asks you",
    "brgr: 44444444 finished: candidate",
  ])
  await session.clock.advance(5000)
  expect(session.toasts).toEqual([
    "brgr: 33333333 asks you",
    "brgr: 44444444 finished: candidate",
  ])
  await $.session.start({ cwd: "/tmp", surface: "terminal", isInteractive: true })
  await session.clock.advance(5000)
  expect(session.toasts).toEqual([
    "brgr: 33333333 asks you",
    "brgr: 44444444 finished: candidate",
  ])
})

test("slash brgr lists active tasks and brgr_run always passes --mode auto", async ($, on) => {
  const session = await boot($, on, {
    tasks: [
      running("11111111-0000-4000-8000-000000000001", "local.gjc"),
      asking("33333333-0000-4000-8000-000000000003"),
      done("44444444-0000-4000-8000-000000000004"),
    ],
  })
  const listed = await $.command.run({
    command: "brgr",
    args: "",
    origin: { kind: "plugin", name: "brgr" },
    presentation: { isFullscreen: false, columns: 80 },
  })
  expect(listed.text).toBe(
    [
      "11111111 running local.gjc",
      "33333333 running local.claude-code",
    ].join("\n"),
  )

  const minimal = await $.tool.call({
    tool: "mcp__brgr__brgr_run",
    objective: "Ship it",
    harness: "local.gjc",
    criterion: ["builds"],
  })
  const full = await $.tool.call({
    tool: "mcp__brgr__brgr_run",
    objective: "Ship it",
    harness: "local.claude-code",
    criterion: ["builds", "tests pass"],
    model: "opus",
    capture_diff: true,
  })
  const runs = session.argv.filter(args => args[2] === "run")
  expect(runs[0]).toEqual([
    "brgr",
    "--json",
    "run",
    "Ship it",
    "--harness",
    "local.gjc",
    "--criterion",
    "builds",
    "--mode",
    "auto",
  ])
  expect(runs[1]).toEqual([
    "brgr",
    "--json",
    "run",
    "Ship it",
    "--harness",
    "local.claude-code",
    "--criterion",
    "builds",
    "--criterion",
    "tests pass",
    "--model",
    "opus",
    "--capture-diff",
    "--mode",
    "auto",
  ])
  for (const args of runs) {
    expect(args.at(-2)).toBe("--mode")
    expect(args.at(-1)).toBe("auto")
    for (const forbidden of ["accept", "reject", "apply", "revise", "ack", "--ack", "land"]) {
      expect(args).not.toContain(forbidden)
    }
  }
  expect(minimal.result).toEqual(JSON.parse(RUN_JSON))
  expect(full.result).toEqual(JSON.parse(RUN_JSON))
})

test("a missing brgr binary clears the status line and toasts once", async ($, on) => {
  const session = await boot($, on, {
    tasks: [running("11111111-0000-4000-8000-000000000001")],
    mode: "missing",
  })
  await session.clock.advance(5000)
  expect(session.status.at(-1)).toBeUndefined()
  expect(session.toasts).toEqual(["brgr: the brgr executable is missing"])
  await session.clock.advance(5000)
  expect(session.status.at(-1)).toBeUndefined()
  expect(session.toasts).toEqual(["brgr: the brgr executable is missing"])
})

test("a non-zero brgr status clears the status line and toasts once", async ($, on) => {
  const session = await boot($, on, {
    tasks: [running("11111111-0000-4000-8000-000000000001")],
    mode: "exit",
  })
  await session.clock.advance(5000)
  expect(session.status.at(-1)).toBeUndefined()
  expect(session.toasts).toEqual(["brgr: brgr status failed"])
  await session.clock.advance(5000)
  expect(session.toasts).toEqual(["brgr: brgr status failed"])
})

test("a session that cannot draw sets no status and no toast", async ($, on) => {
  const session = await boot($, on, {
    tasks: [asking("33333333-0000-4000-8000-000000000003"), done("44444444-0000-4000-8000-000000000004")],
    surfaces: [],
    surface: null,
    interactive: false,
  })
  await session.clock.advance(5000)
  expect(session.status).toEqual([])
  expect(session.toasts).toEqual([])
})
