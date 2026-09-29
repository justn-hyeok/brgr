# Agent-authored process manifests

An approved installed CLI can join brgr without changing the orchestration
core. The agent first checks its exact executable, `--version`, and `--help` in
a bounded probe. `brgr harness draft EXECUTABLE` handles known recipes and
unknown CLIs that document `--prompt-file <path>`. For another documented
one-shot shape, write a declarative `process/v1` JSON manifest instead of
guessing flags or generating executable adapter code.

For example, this is a **synthetic** CLI whose help documents `-p` and whose
non-interactive output is the final answer on stdout. Replace the path only
with an executable the user has approved and observed locally:

```json
{
  "schema": "brgr.harness/v1",
  "id": "local.example-agent",
  "adapter": "process/v1",
  "executable": "/opt/local/bin/example-agent",
  "probe": {"version_argv": ["--version"], "help_argv": ["--help"]},
  "launch": {
    "argv": ["-p", "${input.prompt}"],
    "model_argv": [], "effort_argv": [],
    "env_allow": ["HOME", "PATH", "LANG", "TMPDIR"],
    "mode": "one_shot"
  },
  "result": {
    "source": {"kind": "stdout"}, "media_type": "text/plain",
    "max_bytes": 1048576, "success_exit_codes": [0]
  },
  "capabilities": {
    "completion": {
      "status": "supported", "semantics": "process_exit_with_nonempty_stdout",
      "evidence_ref": "observed-help", "tested_identity": null
    }
  }
}
```

Use the executable's canonical real path. Then run `brgr harness test
--manifest /absolute/path/manifest.json`, followed by `brgr harness activate
--manifest /absolute/path/manifest.json --workspace /absolute/scratch
--prompt "small authorized test"` and `brgr harness status local.example-agent`.
Activation invokes the actual model once; contract testing does not. Pass
`--model MODEL` or `--effort LEVEL` only when the manifest documents and claims
that capability. A successful task still needs a sealed result, owner inbox,
and explicit Codex accept/reject.

Custom manifests cannot shadow built-in IDs, request arbitrary environment
variables, use unknown placeholders, claim flags not in the installed help, or
silently enable force/yolo/auto-accept permission modes.
Brgr passes argv without evaluating a shell command string; the approved CLI
itself remains executable code. The scratch directory is **not a sandbox**: do not probe
or activate an untrusted downloaded executable, or grant it secrets, merely
because a generated manifest looks valid. Treat unknown capabilities as
unsupported and re-test when the executable or help identity changes.

For a model-selecting CLI, add a bounded read-only catalog recipe under
`probe.model_catalog` before claiming `model_select`:

```json
{
  "model_catalog": {
    "argv": ["--list-models"],
    "format": { "kind": "dash_separated" }
  }
}
```

The supported formats are `json_selectors` (with `pointer` and `field`),
`canonical_provider_table`, `dash_separated`, and `first_column`. An argv
element may contain `${model.query}` (full selector) or `${model.id}` (the
part after the final `/`) to filter the native list. Brgr matches
the requested selector exactly; an absent or malformed catalog fails before
the scratch model or task workspace starts. Use only documented read-only
catalog commands, not a flag whose behavior you inferred from its name.
