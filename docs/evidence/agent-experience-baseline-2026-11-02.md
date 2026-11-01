# Agent experience baseline — 2026-11-02

Tracks issue #226. One Apple Silicon Mac, brgr 2.15.15 (release binary) with its skill installed (`brgr integrate claude install`, `brgr integrate codex install`), Herdr 0.9.0, Claude Code and Codex CLI 0.166.0 as installed. Each agent was a fresh session in a new Herdr pane with no earlier conversation, given the same prompt:

> Use brgr to have one worker reply with the single word OK. Use the Cursor harness (local.cursor-cli) with model cursor-grok-4.6-high and the workspace WORKSPACE. Wait for its result, check it, and accept it if it is OK. Then reply in one line listing the brgr commands you ran.

Counts come from each agent's own session record (Claude Code transcript, Codex rollout): the commands it ran and the byte size of each command's output. Only commands and sizes were read, not contents. One run per agent, so these are single observations, not averages.

## Results

| | Claude Code (`claude-opus-5-5`) | Codex (`gpt-6.1-sol`) |
|---|---|---|
| Finished the task (worker run, result read, accepted) | yes | yes |
| Wall time, prompt to last message | 79 s | about 61 s |
| Tool calls | 5: skill read 1, shell 3, monitor 1 | 11 shell calls |
| brgr commands | `run`, `watch` (as a monitor), `result`, `accept` | `run --help`, `run`, `watch`, `result`, `accept` |
| Error from brgr | 0 | 0 |
| Bytes of brgr command output | 2,228 (`run` 512, `result` 1,107, `accept` 609; `watch` ran as a monitor) | 4,575 for `run`, `watch`, `result`, `accept`; plus 2,666 for `run --help` |
| Skill text read as a tool result | via the Skill tool (size not recorded) | 7,739 (`cat` of the skill file) |
| Tokens (as the session record reports) | output 1,169; input 30 plus 531,278 cache reads plus 46,837 cache writes | total 451,662, of which 409,984 cached; output 1,041 |

Observations:
- Both agents used the loop the skill teaches (run, wait, read the result, decide) and neither made an error.
- The Codex agent's first brgr command was `run`, and it worked as the first command of the session on Codex 0.166.0 (the pane is looked up after the call ends).
- Neither task called `brgr status` at all, so **this task does not measure `status --brief`**. The earlier size measurement (a real store with 20 tasks: 858 lines and 168,028 bytes piped, 20 lines and 2,688 bytes with `--brief`) is output size, not agent behavior. Whether `--brief` reduces calls or tokens for an agent that checks status is still unmeasured; it needs a task that makes the agent look at many tasks.
- The Codex session ran in the daemon environment of the default control home, so its task lives in that home, not in a scratch home.

## Cursor (not a clean measurement)

A Cursor Agent pane given a similar task (without a brgr skill: `brgr integrate cursor install` installs a status line, not a skill) started the worker with `brgr run` correctly, but then did not wait for the completion notice. It tried `brgr show`, `brgr task show`, `brgr status` and `sqlite3` against the store on its own, each needing a shell approval, and the `FROM BRGR` notice was not delivered while it was busy, so the run never reached a decision. It is recorded here as an observation of that one attempt, not as a result for the harness.

## What this does not cover
Claude Code and Codex heads on one simple task. Not measured: other harnesses as heads (Gemini, Copilot, Devin, Cline, OpenCode, Pi), harder tasks (several workers, a rejected result, a worker question), repeated runs, and an agent without the skill.
