# Contributing to brgr

## How brgr is built

brgr is developed by AI coding agents. One person, the maintainer, sets the
goals, uses brgr every day, and decides what ships, accepting or rejecting what
the agents produce. The agents write the code, the tests, the docs and the
release notes. They are mostly Claude Code and Codex, and they often coordinate
each other through brgr itself. **No line of brgr is guaranteed to have been
read by a human.**

What that means, plainly:

- **Code quality is not guaranteed.** Expect uneven style, modules that grew
  too long, and problems solved in a roundabout way.
- **Behaviour is what gets checked.** Every pull request must pass `rustfmt`,
  `clippy` with `-D warnings` (pedantic lints included) and the full test
  suite. Since v2.12, each bug fix ships with a test that fails without the
  fix.
- **Reviews are model reviews.** They catch real bugs: v2.12.1 to v2.12.15
  came out of two of them. They are not a human review, and the
  [readiness checklist](docs/readiness/status.md) does not count them as one.

## Reporting a bug

Include:

- `brgr --version`, `herdr --version`, and the harness involved, for example
  `local.claude-code`;
- the output of `brgr errors`, which lists the failures brgr recorded with
  paths and ids redacted;
- what you ran and what you expected.

A failed run's error quotes the last lines of the worker's screen. Read it
before posting, since it can contain your code or your prompt.

## Pull requests

Pull requests are welcome, including ones an agent wrote. They follow the same
rules the agents do, written in [AGENTS.md](AGENTS.md):

- `cargo fmt --all -- --check`,
  `cargo clippy --workspace --all-targets --all-features -- -D warnings` and
  `cargo test --workspace --all-features` pass.
- A bug fix includes a test that fails without it.
- The commit subject is a Conventional Commit, such as
  `fix(cli): keep a dotenv in an ignored dist directory`.
- The description says which contract changed, gives the test evidence, names
  any compatibility or migration risk, and confirms that no harness or model
  fallback and no acceptance decision happens implicitly.

Please also say whether an agent wrote the change, and which one. It is not a
gate; it tells the reviewer what kind of mistakes to look for.

## The most useful contribution: a human review

What brgr lacks most is a person reading its code. A review of one crate, or
one module such as `crates/brgr-cli/src/worktree_prune.rs`, filed as issues,
helps the most.

## Security

Workers run with full permissions (`--yolo`, `--dangerously-skip-permissions`
and equivalents) by design. That is not a vulnerability.

Report as a vulnerability anything where brgr does what it was not asked to:
running a command it was not given, reading or deleting outside a task's
worktree, accepting a result nobody decided, or letting one session's results
reach another. Do not post the details publicly. Open an issue titled
"Security report", without details, and the maintainer will arrange a private
channel.
