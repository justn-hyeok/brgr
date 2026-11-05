# Linux feasibility record — 2026-11-03

Tracks issue #229. Linux is not a supported platform yet. This records what ran, what failed, and what is left.

## Judgment

Linux support is feasible and the remaining work is small. The code builds and its tests pass on x86_64 and aarch64 Linux, and Herdr ships Linux builds. What is missing is one live run with a real agent, plus packaging.

## What ran

| Check | Result |
|---|---|
| `ubuntu-24.04` x86_64, full suite | The pull request gate since #276. 973 passed and 4 failed on the first run; all four are classified below |
| `ubuntu-24.04-arm` aarch64, full suite (one run, #281) | 987 passed, 0 failed, 2 ignored |
| Real Herdr 0.9.5 in a disposable Ubuntu 24.04 x86_64 VM | See below |

## Failures on the first run

| Test | Class | Outcome |
|---|---|---|
| `a_256_kib_objective_is_admitted_by_brgr_run` | Operating system limit | Linux caps one argument at 128 KiB. Ignored on Linux |
| `a_256_kib_plus_one_objective_is_refused_by_brgr_run_without_creating_a_task` | Operating system limit | Same |
| `sdk_accepts_a_256_kib_task_and_launches` | Product portability | Fixed by `--objective-file` (#278) and passes on Linux |
| `a_stray_checkout_of_another_repository_is_never_asked_which_repository_it_is` | Test assumption | The test relied on a global git identity. Fixed in #276 |

## A defect no test showed

`pane_in_environment` used `ps -E`. On the VM, procps answers `error: unsupported SysV option`, so before #280 the check that no ancestor claims a different pane did nothing on Linux, without any error. It now reads `/proc/PID/environ`.

## Real Herdr pane, VM

Installed from the Herdr release binary, with brgr built from `main` at #280. `gjc` is the repository's fixture, not an agent.

- **Pane found:** a run started in pane `w1:p1` recorded `source_pane: w1:p1` and opened worker pane `w1:p2`.
- **Pane guard:** with an ancestor process holding `HERDR_PANE_ID=w9:p9` and the caller claiming `w1:p1`, the claim was refused and the run fell back to its session marker. The same happened from an SSH shell outside Herdr claiming `w1:p1`.
- **Large objective:** a 200 KiB `--objective-file` reached `candidate`. The same text as an argument failed with `Argument list too long`.
- **Worker did not complete:** the fixture exits at once and is not a TUI, so the run ended `native TUI host did not start`. macOS gives the same result with this fixture, so this is not a Linux failure, but it also means a worker never ran to completion in a real pane here.

## Not verified

- A real agent as head or worker on Linux, with a completion notice delivered to it, a decision, and pane and worktree cleanup. The VM had no logged-in agent. This is the main gap.
- Whether harness recipes, their screen rules and their config paths hold on Linux.
- Herdr reporting agent state for real agents on Linux.
- A distribution without merged `/usr`: `/bin/ps` and `/bin/kill` are absolute paths.
- aarch64 beyond the test suite.

## Linux-specific notes

- Default control home: `$XDG_DATA_HOME/brgr`, or `~/.local/share/brgr`.
- `sleep_guard.rs` does nothing on Linux, where macOS holds a `caffeinate` assertion.
- A pull request runs only on Linux since #276, so code under `cfg(target_os = "macos")` is compiled and linted on the push to `main`. The reverse also applies: macOS checks cannot see Linux-only code, which is how a lint failure reached the first CI run of #280.
