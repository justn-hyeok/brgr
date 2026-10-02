//! The built-in harness recipes and the process manifests drafted from them.

use std::path::PathBuf;

use brgr_runner::{
    Capability, CapabilityStatus, ExecutionMode, HarnessManifest, InteractiveSpec, LaunchSpec,
    MANIFEST_SCHEMA_V1, ModelCatalogFormat, ModelCatalogSpec, OMP_ROLE_ADAPTER_V1,
    PROCESS_ADAPTER_V1, PermissionArgv, ProbeSpec, ResultSource, ResultSpec,
};

use crate::{RegistryError, validate_harness_id};

/// How a recipe runs as its own TUI in a Herdr pane.
pub(crate) struct Interactive {
    /// The agent kind `herdr agent start` recognizes; Herdr runs the CLI of
    /// that name.
    pub(crate) kind: &'static str,
    /// Arguments before the permission, model, and effort ones.
    pub(crate) argv: &'static [&'static str],
    pub(crate) effort_print_only: bool,
}

/// A harness brgr recognizes by name, and the process manifest it drafts.
///
/// Data rather than one function per harness: the seven drafts were ~500 lines
/// of the same struct literal differing in a handful of fields, which is where a
/// copy-and-edit slip hides.
pub(crate) struct Recipe {
    pub(crate) names: &'static [&'static str],
    pub(crate) id: &'static str,
    pub(crate) adapter: &'static str,
    /// Every one must appear in the help text before anything is drafted.
    pub(crate) required_flags: &'static [&'static str],
    pub(crate) version_argv: &'static str,
    pub(crate) catalog: Option<(&'static [&'static str], Catalog)>,
    pub(crate) argv: &'static [&'static str],
    pub(crate) model_argv: &'static [&'static str],
    pub(crate) effort_argv: &'static [&'static str],
    /// When set, effort is offered only if the help text documents this flag.
    pub(crate) effort_requires: Option<&'static str>,
    pub(crate) env_allow: &'static [&'static str],
    pub(crate) source: Source,
    pub(crate) media_type: &'static str,
    pub(crate) capabilities: &'static [(&'static str, Cap)],
    /// Where the help that documents `required_flags` lives, when the flags
    /// belong to a subcommand (`opencode run --help`).
    pub(crate) help_argv: &'static [&'static str],
    /// Arguments for each permission level; `None` is a level the CLI has no
    /// way to honour, which brgr then refuses rather than run wider.
    pub(crate) full: Option<&'static [&'static str]>,
    pub(crate) edits: Option<&'static [&'static str]>,
    pub(crate) read_only: Option<&'static [&'static str]>,
    /// The Herdr agent kind and leading arguments for running it as its own
    /// TUI in a pane. Set only after a live run in a Herdr pane.
    pub(crate) interactive: Option<Interactive>,
}

#[derive(Clone, Copy)]
pub(crate) enum Catalog {
    ProviderTable,
    OmpSelectors,
    DashSeparated,
    FirstColumn,
    Lines,
    CliValidated,
}

#[derive(Clone, Copy)]
pub(crate) enum Source {
    Stdout,
    JsonlAssistantFinal,
}

#[derive(Clone, Copy)]
pub(crate) enum Cap {
    Supported(&'static str),
    Unsupported(&'static str),
    Unknown(&'static str),
}

pub(crate) const BASE_ENV: [&str; 4] = ["HOME", "PATH", "LANG", "TMPDIR"];
pub(crate) const PROCESS_CAPS: [(&str, Cap); 2] = [
    ("cancel", Cap::Supported("local_process_only")),
    (
        "completion",
        Cap::Supported("process_exit_with_nonempty_stdout"),
    ),
];
pub(crate) const JSON_CAPS: [(&str, Cap); 4] = [
    ("cancel", Cap::Supported("local_process_only")),
    (
        "completion",
        Cap::Supported("process_exit_with_json_capture"),
    ),
    ("model_select", Cap::Supported("--model")),
    ("effort_select", Cap::Supported("--thinking")),
];
pub(crate) const MODEL: &[&str] = &["--model", "${route.model}"];
pub(crate) const OMP_CATALOG: &[&str] = &["models", "find", "${model.query}", "--json"];

pub(crate) const GENERIC: Recipe = Recipe {
    names: &[],
    id: "",
    adapter: PROCESS_ADAPTER_V1,
    required_flags: &[],
    version_argv: "--version",
    catalog: None,
    argv: &["--prompt-file", "${input.prompt_file}"],
    model_argv: &[],
    effort_argv: &[],
    effort_requires: None,
    env_allow: &["HOME", "PATH", "LANG"],
    source: Source::Stdout,
    media_type: "text/plain",
    capabilities: &[
        PROCESS_CAPS[0],
        PROCESS_CAPS[1],
        ("model_select", Cap::Unsupported("not_observed")),
        ("effort_select", Cap::Unsupported("not_observed")),
    ],
    help_argv: &["--help"],
    // A generic CLI's approval model is unknown, so it declares no levels and
    // runs only as it always has.
    full: None,
    edits: None,
    read_only: None,
    interactive: None,
};

pub(crate) const RECIPES: &[Recipe] = &[
    Recipe {
        names: &["gjc"],
        id: "local.gjc",
        required_flags: &["--mode=<value>", "--no-session", "--no-mcp", "-p, --print"],
        catalog: Some((&["--list-models=${model.id}"], Catalog::ProviderTable)),
        argv: &[
            "-p",
            "--mode=json",
            "--no-session",
            "--no-mcp",
            "@${input.prompt_file}",
        ],
        model_argv: MODEL,
        effort_argv: &["--thinking", "${route.effort}"],
        env_allow: &BASE_ENV,
        source: Source::JsonlAssistantFinal,
        capabilities: &JSON_CAPS,
        full: Some(&[]),
        edits: None,
        read_only: None,
        interactive: Some(Interactive {
            kind: "gjc",
            argv: &[],
            effort_print_only: false,
        }),
        ..GENERIC
    },
    Recipe {
        names: &["omp"],
        id: "local.omp",
        required_flags: &[
            "-p, --print",
            "--mode=<value>",
            "--no-session",
            "--no-prewalk",
            "--no-extensions",
            "--no-title",
            "--model=<value>",
            "--thinking=<value>",
        ],
        catalog: Some((OMP_CATALOG, Catalog::OmpSelectors)),
        argv: &[
            "-p",
            "--mode=json",
            "--no-session",
            "--no-prewalk",
            "--no-extensions",
            "--no-title",
            "@${input.prompt_file}",
        ],
        model_argv: MODEL,
        effort_argv: &["--thinking", "${route.effort}"],
        env_allow: &BASE_ENV,
        source: Source::JsonlAssistantFinal,
        capabilities: &JSON_CAPS,
        full: Some(&["--approval-mode=yolo"]),
        edits: Some(&["--approval-mode=write"]),
        read_only: None,
        interactive: Some(Interactive {
            kind: "omp",
            argv: &[],
            effort_print_only: false,
        }),
        ..GENERIC
    },
    Recipe {
        names: &["cursor", "cursor-agent", "cursor-cli"],
        id: "local.cursor-cli",
        required_flags: &[
            "--print",
            "--mode <mode>",
            "--output-format <format>",
            "--model <model>",
        ],
        catalog: Some((&["models"], Catalog::DashSeparated)),
        argv: &[
            "--print",
            "--output-format",
            "text",
            "--trust",
            "--workspace",
            "${task.workspace}",
            "${input.prompt}",
        ],
        model_argv: MODEL,
        env_allow: &BASE_ENV,
        capabilities: &[
            PROCESS_CAPS[0],
            PROCESS_CAPS[1],
            ("model_select", Cap::Supported("--model")),
            ("effort_select", Cap::Unsupported("not_observed")),
        ],
        full: Some(&["--force"]),
        edits: Some(&[]),
        read_only: Some(&["--mode", "plan"]),
        interactive: Some(Interactive {
            kind: "cursor",
            argv: &["--trust"],
            effort_print_only: false,
        }),
        ..GENERIC
    },
    Recipe {
        names: &["command-code", "commandcode", "cmdc"],
        id: "local.command-code",
        required_flags: &[
            "--print [query]",
            "--permission-mode <mode>",
            "--no-session",
            "--no-skills",
            "--skip-onboarding",
            "--no-auto-update",
            "--max-turns <number>",
            "--model <model>",
        ],
        catalog: Some((&["--list-models"], Catalog::FirstColumn)),
        argv: &[
            "--no-session",
            "--no-skills",
            "--skip-onboarding",
            "--no-auto-update",
            "--print",
            "${input.prompt}",
        ],
        model_argv: MODEL,
        effort_argv: &["--effort", "${route.effort}"],
        effort_requires: Some("--effort <level>"),
        env_allow: &["HOME", "PATH", "LANG", "TMPDIR", "COMMAND_CODE_API_KEY"],
        capabilities: &[
            PROCESS_CAPS[0],
            PROCESS_CAPS[1],
            ("model_select", Cap::Supported("--model")),
            ("effort_select", Cap::Supported("--effort")),
        ],
        full: Some(&["--permission-mode", "yolo"]),
        edits: Some(&["--permission-mode", "accept-edits"]),
        read_only: Some(&["--permission-mode", "plan"]),
        interactive: Some(Interactive {
            kind: "command-code",
            argv: &["--trust", "--no-auto-update", "--skip-onboarding"],
            effort_print_only: false,
        }),
        ..GENERIC
    },
    Recipe {
        names: &["devin"],
        id: "local.devin",
        required_flags: &[
            "--prompt-file <FILE>",
            "-p, --print",
            "--permission-mode <PERMISSION_MODE>",
            "--respect-workspace-trust [<RESPECT_WORKSPACE_TRUST>]",
        ],
        argv: &[
            "--respect-workspace-trust",
            "false",
            "--prompt-file",
            "${input.prompt_file}",
            "-p",
        ],
        env_allow: &BASE_ENV,
        capabilities: &[
            PROCESS_CAPS[0],
            (
                "completion",
                Cap::Supported("print_mode_process_exit_with_nonempty_stdout"),
            ),
            ("model_select", Cap::Unsupported("configured_default_only")),
            ("effort_select", Cap::Unsupported("not_observed")),
        ],
        full: Some(&["--permission-mode", "dangerous"]),
        edits: Some(&["--permission-mode", "accept-edits"]),
        read_only: Some(&["--permission-mode", "auto"]),
        interactive: Some(Interactive {
            kind: "devin",
            argv: &["--respect-workspace-trust", "false"],
            effort_print_only: false,
        }),
        ..GENERIC
    },
    Recipe {
        names: &["omp-role"],
        id: "local.omp-herdr",
        adapter: OMP_ROLE_ADAPTER_V1,
        required_flags: &[
            "--expected-report",
            "--reuse-worktree-objective",
            "--reuse-worktree-owner",
            "--model",
            "--effort",
        ],
        version_argv: "--help",
        catalog: Some((OMP_CATALOG, Catalog::OmpSelectors)),
        argv: &[],
        env_allow: &["HOME", "PATH", "LANG", "HERDR_ENV", "HERDR_PANE_ID"],
        media_type: "text/markdown",
        capabilities: &[
            (
                "completion",
                Cap::Supported("contracted_report_and_terminal_herdr_state"),
            ),
            ("model_select", Cap::Supported("omp-role --model")),
            ("effort_select", Cap::Supported("omp-role --effort")),
            ("presentation", Cap::Supported("herdr_optional_adapter")),
            ("cancel", Cap::Unknown("not_certified_in_v1")),
        ],
        ..GENERIC
    },
    Recipe {
        names: &["claude", "claude-code"],
        id: "local.claude-code",
        required_flags: &[
            "-p, --print",
            "--output-format <format>",
            "--model <model>",
            "--permission-mode <mode>",
        ],
        argv: &[
            "-p",
            "--output-format",
            "text",
            "--no-session-persistence",
            "${input.prompt}",
        ],
        catalog: Some((&[], Catalog::CliValidated)),
        model_argv: MODEL,
        effort_argv: &["--effort", "${route.effort}"],
        effort_requires: Some("--effort <level>"),
        // `USER` names the macOS keychain entry holding the login; without it
        // Claude Code reports "Not logged in" even when it is.
        env_allow: &[
            "HOME",
            "PATH",
            "LANG",
            "TMPDIR",
            "USER",
            "ANTHROPIC_API_KEY",
        ],
        capabilities: &[
            PROCESS_CAPS[0],
            PROCESS_CAPS[1],
            // Claude Code has no model list, but it refuses a name its own
            // catalog does not describe in about three seconds, locally, before
            // any request; `haiku` or a full model name passes.
            ("model_select", Cap::Supported("--model")),
            ("effort_select", Cap::Supported("--effort")),
        ],
        full: Some(&["--permission-mode", "bypassPermissions"]),
        edits: Some(&["--permission-mode", "acceptEdits"]),
        read_only: Some(&["--permission-mode", "plan"]),
        interactive: Some(Interactive {
            kind: "claude",
            argv: &[],
            effort_print_only: false,
        }),
        ..GENERIC
    },
    Recipe {
        names: &["cline"],
        id: "local.cline",
        required_flags: &[
            "-p, --plan",
            "--auto-approve <boolean>",
            "-m, --model <model-id>",
            "--thinking <level>",
        ],
        argv: &["${input.prompt}"],
        effort_argv: &["--thinking", "${route.effort}"],
        env_allow: &BASE_ENV,
        capabilities: &[
            PROCESS_CAPS[0],
            PROCESS_CAPS[1],
            // Cline's `--model` is documented as per session, but Cline 3.0.65
            // saved an unknown name as the provider's default: every later
            // run, with or without brgr, failed "model not found" until the
            // setting was restored by hand. brgr never changes a CLI's own
            // configuration, so Cline runs its configured model only.
            ("model_select", Cap::Unsupported("configured_default_only")),
            ("effort_select", Cap::Supported("--thinking")),
        ],
        full: Some(&["--auto-approve", "true"]),
        edits: None,
        read_only: Some(&["--plan"]),
        interactive: Some(Interactive {
            kind: "cline",
            argv: &[],
            effort_print_only: false,
        }),
        ..GENERIC
    },
    Recipe {
        names: &["opencode"],
        id: "local.opencode",
        required_flags: &["--model", "--variant", "--agent", "--auto"],
        help_argv: &["run", "--help"],
        catalog: Some((&["models"], Catalog::Lines)),
        argv: &["run", "${input.prompt}"],
        model_argv: &["--model", "${route.model}"],
        effort_argv: &["--variant", "${route.effort}"],
        env_allow: &BASE_ENV,
        capabilities: &[
            PROCESS_CAPS[0],
            PROCESS_CAPS[1],
            ("model_select", Cap::Supported("--model")),
            ("effort_select", Cap::Supported("--variant")),
        ],
        full: Some(&["--auto"]),
        edits: None,
        read_only: Some(&["--agent", "plan"]),
        interactive: Some(Interactive {
            kind: "opencode",
            argv: &[],
            effort_print_only: true,
        }),
        ..GENERIC
    },
];

pub(crate) fn generate_manifest(
    requested_name: &str,
    executable: PathBuf,
    help: &str,
) -> Result<HarnessManifest, RegistryError> {
    if let Some(recipe) = RECIPES
        .iter()
        .find(|recipe| recipe.names.contains(&requested_name))
    {
        if !recipe.required_flags.iter().all(|flag| help.contains(flag)) {
            return Err(RegistryError::RequiredFlagsMissing);
        }
        return Ok(draft(recipe, recipe.id.to_owned(), executable, help));
    }
    // The only v1 generic profile is an explicitly documented fresh run that
    // accepts a prompt file and writes its final result to stdout. Other
    // shapes stay draft-blocked rather than receiving guessed arguments.
    if !help.lines().any(|line| {
        let words: Vec<_> = line.split_whitespace().collect();
        words.windows(2).any(|pair| {
            pair[0] == "--prompt-file" && matches!(pair[1], "<path>" | "<file>" | "PATH" | "FILE")
        })
    }) {
        return Err(RegistryError::RequiredFlagsMissing);
    }
    let id = format!("local.{}", requested_name.to_ascii_lowercase());
    validate_harness_id(&id)?;
    if matches!(id.as_str(), "local.gjc" | "local.omp" | "local.omp-herdr") {
        return Err(RegistryError::UnsupportedHarness);
    }
    Ok(draft(&GENERIC, id, executable, help))
}

pub(crate) fn draft(
    recipe: &Recipe,
    id: String,
    executable: PathBuf,
    help: &str,
) -> HarnessManifest {
    let strings = |values: &[&str]| values.iter().map(|value| (*value).to_owned()).collect();
    let effort = recipe
        .effort_requires
        .is_none_or(|flag| help.contains(flag));
    HarnessManifest {
        schema: MANIFEST_SCHEMA_V1.to_owned(),
        id,
        adapter: recipe.adapter.to_owned(),
        executable,
        probe: ProbeSpec {
            version_argv: vec![recipe.version_argv.to_owned()],
            help_argv: strings(recipe.help_argv),
            model_catalog: recipe.catalog.map(|(argv, format)| ModelCatalogSpec {
                argv: strings(argv),
                format: match format {
                    Catalog::ProviderTable => ModelCatalogFormat::CanonicalProviderTable,
                    Catalog::OmpSelectors => ModelCatalogFormat::JsonSelectors {
                        pointer: "/models".to_owned(),
                        field: "selector".to_owned(),
                    },
                    Catalog::DashSeparated => ModelCatalogFormat::DashSeparated,
                    Catalog::FirstColumn => ModelCatalogFormat::FirstColumn,
                    Catalog::Lines => ModelCatalogFormat::Lines,
                    Catalog::CliValidated => ModelCatalogFormat::CliValidated,
                },
            }),
        },
        launch: LaunchSpec {
            argv: strings(recipe.argv),
            model_argv: strings(recipe.model_argv),
            effort_argv: if effort {
                strings(recipe.effort_argv)
            } else {
                vec![]
            },
            env_allow: strings(recipe.env_allow),
            mode: ExecutionMode::OneShot,
            permission_argv: PermissionArgv {
                full: recipe.full.map(strings),
                edits: recipe.edits.map(strings),
                read_only: recipe.read_only.map(strings),
            },
            interactive: recipe
                .interactive
                .as_ref()
                .map(|interactive| InteractiveSpec {
                    herdr_kind: interactive.kind.to_owned(),
                    argv: strings(interactive.argv),
                    effort_print_only: interactive.effort_print_only,
                    native_host: true,
                }),
        },
        result: ResultSpec {
            source: match recipe.source {
                Source::Stdout => ResultSource::Stdout,
                Source::JsonlAssistantFinal => ResultSource::JsonlAssistantFinal,
            },
            media_type: recipe.media_type.to_owned(),
            max_bytes: 1_048_576,
            success_exit_codes: vec![0],
        },
        capabilities: recipe
            .capabilities
            .iter()
            .map(|&(name, cap)| {
                let cap = match cap {
                    Cap::Supported(_) if name == "effort_select" && !effort => {
                        unsupported("not_observed")
                    }
                    Cap::Supported(semantics) => supported(semantics),
                    Cap::Unsupported(reason) => unsupported(reason),
                    Cap::Unknown(semantics) => Capability {
                        status: CapabilityStatus::Unknown,
                        semantics: semantics.to_owned(),
                        evidence_ref: None,
                        tested_identity: None,
                    },
                };
                (name.to_owned(), cap)
            })
            .collect(),
    }
}

pub(crate) fn supported(semantics: &str) -> Capability {
    Capability {
        status: CapabilityStatus::Supported,
        semantics: semantics.to_owned(),
        evidence_ref: Some("activation-help-digest".to_owned()),
        tested_identity: None,
    }
}

pub(crate) fn unsupported(semantics: &str) -> Capability {
    Capability {
        status: CapabilityStatus::Unsupported,
        semantics: semantics.to_owned(),
        evidence_ref: None,
        tested_identity: None,
    }
}
