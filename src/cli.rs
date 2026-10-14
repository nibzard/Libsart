//! Command parsing and command line output (SPEC 6, 7, and 13.1).
//!
//! This module owns everything the executable promises to a caller:
//!
//! - one parser for the commands and global options of SPEC 6.1 and 6.2,
//!   including `--json` detection that runs before argument-error
//!   reporting, so a syntax failure uses the same JSON envelope as a
//!   normal result
//! - the versioned JSON envelope of SPEC 7.1, buffered and bounded, with
//!   no terminal decoration, prompt, pager, or spinner
//! - the exit classes of SPEC 7.5, shared by the text and JSON modes
//! - structured command help for machine discovery (`--help --json`) and
//!   version data with Git capability information (`--version --json`),
//!   both generated from the live parser, never maintained by hand
//! - one bounded read of each `--input` plan document, from a file or
//!   standard input, so an oversized plan is a typed limit while every
//!   rule about the bytes stays with the one strict plan parser of the
//!   domain
//!
//! Command execution is never implemented here. Every command calls the
//! shared operation layer of [`crate::app`], serializes its result, and
//! maps its typed error to one exit class. Commands whose algorithms later
//! tasks provide report the one shared refusal of that layer.

use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, IsTerminal, Read as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::error::ErrorKind;
use clap::{Arg, ArgAction, ArgMatches, Command, value_parser};
use serde::Serialize;
use serde_json::{Value, json};

use crate::app::{
    BaselineStatus, ChangeDetail, CheckpointOutcome, CheckpointRequest, DiffRequest,
    DocumentContent, Effects, Fingerprint, InitRequest, InitializationOutcome,
    InitializationProposal, InspectRequest, LogEntry, LogOutcome, LogRequest, MarkerSummary,
    OperationError, OperationOutcome, Operations, ReceiptPresence, RecoveryInspection,
    RecoveryRequest, RestoreOutcome, RestoreRequest, ReviewDecision, ReviewOutcome, ReviewRequest,
    ScopeSummary, ShowOutcome, ShowRequest, ShowResult, StatusCounts, Warning, WorkspaceInspection,
};
use crate::domain::{
    COMMIT_TRAILER_VERSION, ENGINE_VERSION, LocalSettings, MAX_PLAN_BYTES, ScopeDescriptor,
    ScopeInput,
};
use crate::git::{Git, Limits};
use crate::integrations;

// ---------------------------------------------------------------------------
// Contract constants
// ---------------------------------------------------------------------------

/// Major contract version of every JSON envelope (SPEC 7.1).
pub const ENVELOPE_SCHEMA_VERSION: u32 = 1;

/// The default bounded execution budget of one core command (SPEC 6.1).
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;

/// The same budget as the parser's default value string; a test holds the
/// two together.
const DEFAULT_TIMEOUT_MS_ARG: &str = "10000";

/// The largest `--timeout-ms` value the parser accepts. One hour bounds
/// the budget a caller can request.
pub const MAX_TIMEOUT_MS: u64 = 3_600_000;

/// The largest number of characters one message carries into an envelope.
/// Diagnostics are bounded data, never a stream (SPEC 7.6).
pub const MAX_MESSAGE_CHARS: usize = 2_000;

/// The largest accepted `log --limit` page (SPEC 6.3).
pub const MAX_LOG_PAGE: u64 = 200;

/// The default `log --limit` page as the parser's default value string; one
/// test holds it beside the domain constant every operation falls back to.
const DEFAULT_LOG_PAGE_ARG: &str = "20";

/// The largest accepted `show` byte bound; SPEC 3.3 sets the per-file
/// ceiling at 16 MiB.
pub const MAX_SHOW_BYTES: u64 = 16 * 1024 * 1024;

/// The exit classes of SPEC 7.5. One row per class: the exit number, the
/// class name, and its representative error codes. Class 0 reports no
/// error. The exit mapping, the text help, and the structured help all
/// read this one table.
pub const EXIT_CLASSES: &[(u8, &str, &[&str])] = &[
    (0, "verified success or documented no-op", &[]),
    (
        2,
        "invalid invocation or input",
        &[
            "INVALID_ARGUMENT",
            "INVALID_PLAN",
            "INVALID_NOTES",
            "UNKNOWN_CHANGE",
            "LIMIT_EXCEEDED",
        ],
    ),
    (
        3,
        "stale or conflicting context",
        &[
            "STALE_REVIEW",
            "STALE_CURSOR",
            "STALE_STATE",
            "REQUEST_ID_CONFLICT",
            "BRANCH_CHANGED",
            "NOTES_CONFLICT",
            "INSTRUCTION_CONFLICT",
        ],
    ),
    (
        4,
        "unmet operational precondition",
        &[
            "NOT_INITIALIZED",
            "DIRTY_SCOPE",
            "STAGED_CHANGES",
            "UNSUPPORTED_REPOSITORY",
            "MISSING_IDENTITY",
            "WRITER_ACTIVE",
            "BUSY",
            "BASELINE_UNAVAILABLE",
            "INVALID_SETTINGS",
            "OPERATION_UNAVAILABLE",
        ],
    ),
    (
        5,
        "execution failure",
        &[
            "GIT_FAILED",
            "HOOK_FAILED",
            "TMUX_FAILED",
            "PARTIAL_MUTATION",
            "OUTCOME_UNKNOWN",
            "IO_FAILED",
            "CONTRACT_VIOLATION",
        ],
    ),
    (
        130,
        "user interruption when clean shutdown is possible",
        &["INTERRUPTED"],
    ),
];

/// The executable examples of SPEC 6.5. Tests parse every line with the
/// live parser, so an example that no longer invokes the program fails the
/// suite (SPEC 16.8).
const EXAMPLES: &[&str] = &[
    "margin --help --json",
    "margin status --json",
    "margin diff --path content/article.md --json",
    "margin checkpoint --message \"Shorten introduction\" --json",
    "margin review --input review.json --dry-run --json",
    "margin review --input review.json --json",
];

/// The exit class of one SPEC 7.5 error code. An unknown code is a
/// contract violation of this build, so execution failure is the safest
/// class for it.
pub fn exit_class(code: &str) -> u8 {
    for (exit, _, codes) in EXIT_CLASSES {
        if codes.contains(&code) {
            return *exit;
        }
    }
    5
}

// ---------------------------------------------------------------------------
// The JSON envelope (SPEC 7.1)
// ---------------------------------------------------------------------------

/// The complete JSON envelope of one command result. Field order matches
/// the SPEC 7.1 example, and every field is always present, including on
/// parser errors and failures.
#[derive(Debug, Serialize)]
struct Envelope {
    /// [`ENVELOPE_SCHEMA_VERSION`].
    schema_version: u32,
    /// The canonical command name, or `None` when the arguments could not
    /// identify a command.
    command: Option<String>,
    /// True only when the requested operation has a verified result,
    /// including an explicit no-op.
    ok: bool,
    /// Command-specific data on success; the observed state on failure.
    data: Option<Value>,
    /// Exactly one typed primary error on failure; `None` on success.
    error: Option<EnvelopeError>,
    /// Warnings; they never conceal a failed invariant.
    warnings: Vec<EnvelopeWarning>,
    /// What the command observed; required even on failures.
    effects: EnvelopeEffects,
}

impl Envelope {
    /// One successful envelope around interface-level data such as
    /// structured help or version data. Nothing was written.
    fn success(command: Option<&str>, data: Value) -> Envelope {
        Envelope {
            schema_version: ENVELOPE_SCHEMA_VERSION,
            command: command.map(str::to_string),
            ok: true,
            data: Some(data),
            error: None,
            warnings: Vec::new(),
            effects: EnvelopeEffects::from(&Effects::unchanged()),
        }
    }

    /// One failed envelope around a typed error of the interface layer.
    fn from_error(command: Option<&str>, error: &OperationError) -> Envelope {
        Envelope {
            schema_version: ENVELOPE_SCHEMA_VERSION,
            command: command.map(str::to_string),
            ok: false,
            data: None,
            error: Some(EnvelopeError::from(error)),
            warnings: Vec::new(),
            effects: EnvelopeEffects::from(&Effects::unchanged()),
        }
    }

    /// One envelope around a shared operation result (SPEC 13.2): the
    /// operation owns the effects, the warnings, and the one typed error.
    fn from_outcome<T>(
        command: &str,
        outcome: &OperationOutcome<T>,
        project: impl FnOnce(&T) -> Value,
    ) -> Envelope {
        Envelope {
            schema_version: ENVELOPE_SCHEMA_VERSION,
            command: Some(command.to_string()),
            ok: outcome.ok(),
            data: outcome.data.as_ref().map(project),
            error: outcome.error.as_ref().map(EnvelopeError::from),
            warnings: outcome.warnings.iter().map(EnvelopeWarning::from).collect(),
            effects: EnvelopeEffects::from(&outcome.effects),
        }
    }
}

/// The one typed primary error of a failed command (SPEC 7.1).
#[derive(Debug, Serialize)]
struct EnvelopeError {
    /// The SPEC 7.5 error code.
    code: String,
    /// One bounded explanation.
    message: String,
    /// Whether an identical retry can be useful and safe.
    retryable: bool,
    /// One recovery suggestion as an argv array, empty when none applies.
    recovery_argv: Vec<String>,
}

impl From<&OperationError> for EnvelopeError {
    fn from(error: &OperationError) -> Self {
        EnvelopeError {
            code: error.code.to_string(),
            message: bounded_message(&error.message),
            retryable: error.retryable,
            recovery_argv: error.recovery_argv.clone(),
        }
    }
}

/// One warning of a command result (SPEC 7.1).
#[derive(Debug, Serialize)]
struct EnvelopeWarning {
    /// The SPEC 7.5 code family of the warning.
    code: String,
    /// One bounded explanation.
    message: String,
}

impl From<&Warning> for EnvelopeWarning {
    fn from(warning: &crate::app::Warning) -> Self {
        EnvelopeWarning {
            code: warning.code.to_string(),
            message: bounded_message(&warning.message),
        }
    }
}

/// The observed effects of one command (SPEC 7.1).
#[derive(Debug, Serialize)]
struct EnvelopeEffects {
    working_tree: String,
    index: String,
    commit: String,
    commit_oid: Option<String>,
    repository: String,
    configuration: String,
    integration: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    notes_ref: Option<String>,
}

impl From<&Effects> for EnvelopeEffects {
    fn from(effects: &Effects) -> Self {
        EnvelopeEffects {
            working_tree: effects.working_tree.as_str().to_string(),
            index: effects.index.as_str().to_string(),
            commit: effects.commit.as_str().to_string(),
            commit_oid: effects.commit_oid.clone(),
            repository: effects.repository.as_str().to_string(),
            configuration: effects.configuration.as_str().to_string(),
            integration: effects.integration.as_str().to_string(),
            notes_ref: (effects.notes_ref != crate::app::ObservedEffect::Unchanged)
                .then(|| effects.notes_ref.as_str().to_string()),
        }
    }
}

/// Bounds one diagnostic for an envelope: the first meaningful line, no
/// terminal control characters, and at most [`MAX_MESSAGE_CHARS`]
/// characters (SPEC 6.1, 7.6). The hook adapter shares this one rule, so
/// its protocol strings get the same treatment (SPEC 12.2, 14.2).
pub(crate) fn bounded_message(raw: &str) -> String {
    let line = raw
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let line = line.strip_prefix("error: ").unwrap_or(line);
    let mut message: String = line
        .chars()
        .map(|character| {
            if character.is_control() {
                '\u{FFFD}'
            } else {
                character
            }
        })
        .collect();
    if message.chars().count() > MAX_MESSAGE_CHARS {
        let keep = MAX_MESSAGE_CHARS - 1;
        message = message.chars().take(keep).collect();
        message.push('\u{2026}');
    }
    message
}

// ---------------------------------------------------------------------------
// The parser (SPEC 6.1, 6.2)
// ---------------------------------------------------------------------------

/// Global options that consume one value. The argv walk of
/// [`command_path`] needs them to step over values without clap's help.
const GLOBAL_VALUE_OPTIONS: &[&str] = &["cwd", "timeout-ms", "color"];

/// Builds the complete command tree. This one function is the single
/// source of the command inventory: the text help, the structured help,
/// and the tests all read it back (SPEC 16.8).
fn command() -> Command {
    Command::new("margin")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Scoped draft preservation and review for writers and their agents")
        // Margin renders its own version data (SPEC 6.1 `--version --json`),
        // so the parser carries `--version` as an ordinary global flag that
        // reaches dispatch with the rest of the invocation.
        .disable_version_flag(true)
        .arg_required_else_help(true)
        .arg(
            Arg::new("json")
                .long("json")
                .action(ArgAction::SetTrue)
                .global(true)
                .help("Emit one versioned JSON envelope on stdout; no decoration or prompts"),
        )
        .arg(
            Arg::new("version")
                .long("version")
                .action(ArgAction::SetTrue)
                .global(true)
                .help("Print build, schema, engine, and Git capability information"),
        )
        .arg(
            Arg::new("cwd")
                .long("cwd")
                .value_name("PATH")
                .global(true)
                .default_value(".")
                .help("Select the repository discovery root"),
        )
        .arg(
            Arg::new("timeout-ms")
                .long("timeout-ms")
                .value_name("N")
                .global(true)
                .default_value(DEFAULT_TIMEOUT_MS_ARG)
                .value_parser(value_parser!(u64).range(1..=MAX_TIMEOUT_MS))
                .help("Bounded execution budget in milliseconds"),
        )
        .arg(
            Arg::new("color")
                .long("color")
                .value_name("WHEN")
                .global(true)
                .default_value("auto")
                .value_parser(["auto", "always", "never"])
                .help("Text decoration only; JSON output never carries color"),
        )
        .subcommand(init_command())
        .subcommand(
            Command::new("note")
                .about("Append one annotation to the candidate commit (mutation)")
                .arg(
                    Arg::new("text")
                        .long("text")
                        .required(true)
                        .value_name("TEXT")
                        .help("Reason for the change, 1–2000 Unicode characters"),
                )
                .arg(
                    Arg::new("group")
                        .long("group")
                        .value_name("DIGEST")
                        .help("Change-group digest from margin diff --json"),
                ),
        )
        .subcommand(
            Command::new("notes")
                .about("List commit annotations or print their schema (query)")
                .arg(
                    Arg::new("revision")
                        .long("revision")
                        .value_name("COMMIT")
                        .conflicts_with("schema")
                        .help("Commit to inspect; default HEAD"),
                )
                .arg(
                    Arg::new("schema")
                        .long("schema")
                        .action(ArgAction::SetTrue)
                        .conflicts_with("json")
                        .help("Print the installed raw JSON Schema without repository access"),
                ),
        )
        .subcommand(
            Command::new("status")
                .about("Report preservation and review state (query)")
                .arg(
                    Arg::new("path")
                        .long("path")
                        .value_name("PATH")
                        .action(ArgAction::Append)
                        .help("Presentation filter for one path"),
                ),
        )
        .subcommand(
            Command::new("diff")
                .about("Report the pinned review comparison (query)")
                .arg(
                    Arg::new("path")
                        .long("path")
                        .value_name("PATH")
                        .action(ArgAction::Append)
                        .help("Presentation filter for one path"),
                )
                .arg(
                    Arg::new("change")
                        .long("change")
                        .value_name("ID")
                        .conflicts_with_all(["path", "cursor", "limit"])
                        .help("Show the detail of one change group"),
                )
                .arg(
                    Arg::new("cursor")
                        .long("cursor")
                        .value_name("CURSOR")
                        .help("Continue from an earlier page position"),
                )
                .arg(
                    Arg::new("limit")
                        .long("limit")
                        .value_name("N")
                        .value_parser(value_parser!(u64).range(1..=MAX_LOG_PAGE))
                        .help(concat!(
                            "Change groups per detail page; the default is ",
                            "50, and 256 KiB of detail bounds the page too"
                        )),
                )
                .arg(
                    Arg::new("working")
                        .long("working")
                        .action(ArgAction::SetTrue)
                        .help("Preview the working tree; the preview is non-actionable"),
                ),
        )
        .subcommand(
            Command::new("show")
                .about("Show one document at an immutable revision (query)")
                .arg(
                    Arg::new("path")
                        .long("path")
                        .value_name("PATH")
                        .required(true)
                        .help("The document to show"),
                )
                .arg(
                    Arg::new("revision")
                        .long("revision")
                        .value_name("REV")
                        .required(true)
                        .help("The immutable revision to read"),
                )
                .arg(
                    Arg::new("format")
                        .long("format")
                        .value_name("FORMAT")
                        .default_value("text")
                        .value_parser(["text", "raw"])
                        .help(concat!(
                            "Presentation format; `raw` exports exact bytes and excludes ",
                            "`--json`, `--max-bytes`, and `--max-lines`"
                        )),
                )
                .arg(
                    Arg::new("max-bytes")
                        .long("max-bytes")
                        .value_name("N")
                        .value_parser(value_parser!(u64).range(1..=MAX_SHOW_BYTES))
                        .help("Largest number of content bytes to return"),
                )
                .arg(
                    Arg::new("max-lines")
                        .long("max-lines")
                        .value_name("N")
                        .value_parser(value_parser!(u64).range(1..=1_000_000))
                        .help("Largest number of content lines to return"),
                )
                .arg(
                    Arg::new("allow-tty")
                        .long("allow-tty")
                        .action(ArgAction::SetTrue)
                        .help("Permit raw exact-byte output while stdout is a terminal"),
                ),
        )
        .subcommand(
            Command::new("log")
                .about("List the ancestry-based editorial timeline (query)")
                .arg(
                    Arg::new("limit")
                        .long("limit")
                        .value_name("N")
                        .default_value(DEFAULT_LOG_PAGE_ARG)
                        .value_parser(value_parser!(u64).range(1..=MAX_LOG_PAGE))
                        .help("Entries per page"),
                )
                .arg(
                    Arg::new("cursor")
                        .long("cursor")
                        .value_name("CURSOR")
                        .help("Continue from an earlier page position"),
                )
                .arg(
                    Arg::new("path")
                        .long("path")
                        .value_name("PATH")
                        .action(ArgAction::Append)
                        .help("Presentation filter for one path"),
                )
                .arg(
                    Arg::new("all-checkpoints")
                        .long("all-checkpoints")
                        .action(ArgAction::SetTrue)
                        .help("Show collapsed draft checkpoints too"),
                ),
        )
        .subcommand(
            Command::new("checkpoint")
                .about("Commit eligible scoped content as one draft (mutation)")
                .arg(
                    Arg::new("message")
                        .long("message")
                        .value_name("TEXT")
                        .help("Explicit commit message"),
                )
                .arg(
                    Arg::new("include-staged")
                        .long("include-staged")
                        .action(ArgAction::SetTrue)
                        .requires("expect-head")
                        .requires("expect-index")
                        .requires("expect-working")
                        .help("Guarded recovery mode that captures inspected staged content"),
                )
                .arg(
                    Arg::new("expect-head")
                        .long("expect-head")
                        .value_name("OID")
                        .requires("include-staged")
                        .help("Expected HEAD commit of the recovery capture"),
                )
                .arg(
                    Arg::new("expect-index")
                        .long("expect-index")
                        .value_name("TREE")
                        .requires("include-staged")
                        .help("Expected index tree of the recovery capture"),
                )
                .arg(
                    Arg::new("expect-working")
                        .long("expect-working")
                        .value_name("FINGERPRINT")
                        .requires("include-staged")
                        .help("Expected scoped working fingerprint of the recovery capture"),
                )
                .arg(
                    Arg::new("dry-run")
                        .long("dry-run")
                        .action(ArgAction::SetTrue)
                        .help("Report the intended effects without committing"),
                ),
        )
        .subcommand(
            Command::new("review")
                .about("Validate or apply one complete pinned review plan (mutation)")
                .arg(
                    Arg::new("input")
                        .long("input")
                        .value_name("FILE")
                        .required(true)
                        .help("Review plan v1 document, or - for standard input"),
                )
                .arg(
                    Arg::new("dry-run")
                        .long("dry-run")
                        .action(ArgAction::SetTrue)
                        .conflicts_with("resume")
                        .help("Validate and report without writing"),
                )
                .arg(
                    Arg::new("resume")
                        .long("resume")
                        .action(ArgAction::SetTrue)
                        .help("Resume one retained plan"),
                ),
        )
        .subcommand(
            Command::new("restore")
                .about("Restore one document from history as an unreviewed commit (mutation)")
                .arg(
                    Arg::new("input")
                        .long("input")
                        .value_name("FILE")
                        .required(true)
                        .help("Plan file, or - for standard input"),
                )
                .arg(
                    Arg::new("dry-run")
                        .long("dry-run")
                        .action(ArgAction::SetTrue)
                        .help("Report the intended effects without committing"),
                ),
        )
        .subcommand(
            Command::new("doctor")
                .about("Report recovery state with bounded recovery suggestions (query)"),
        )
        .subcommand(
            Command::new("tui")
                .about("Open the interactive review surface (interactive)")
                .arg(
                    Arg::new("path")
                        .long("path")
                        .value_name("PATH")
                        .help("Initial document"),
                )
                .arg(
                    Arg::new("view")
                        .long("view")
                        .value_name("VIEW")
                        .value_parser(["read", "changes", "history"])
                        .help("Initial view"),
                )
                .arg(
                    Arg::new("reduced-updates")
                        .long("reduced-updates")
                        .action(ArgAction::SetTrue)
                        .help(
                            "Redraw only when the frame changed; for terminals and assistive \
                               setups sensitive to redraws",
                        ),
                ),
        )
        .subcommand(
            Command::new("edit")
                .about("Hand one document to the configured editor (interactive)")
                .arg(
                    Arg::new("path")
                        .long("path")
                        .value_name("PATH")
                        .required(true)
                        .help(
                            "The repository-relative document to edit; its literal path is \
                               appended to the configured editor argv",
                        ),
                ),
        )
        .subcommand(
            Command::new("desk")
                .about("Compose an optional tmux writing desk (interactive)")
                .arg(
                    Arg::new("agent")
                        .value_name("AGENT ARGS")
                        .num_args(1..)
                        .required(true)
                        .trailing_var_arg(true)
                        .allow_hyphen_values(true)
                        .help(
                            "Agent executable and its arguments, after --; tmux starts the \
                               argv directly, with no shell between",
                        ),
                ),
        )
        .subcommand(
            Command::new("hook")
                .about("Manage agent hook integrations")
                .subcommand_required(true)
                .subcommand(
                    Command::new("install")
                        .about("Install one integration's hook entries (mutation)")
                        .arg(
                            Arg::new("integration")
                                .value_name("INTEGRATION")
                                .required(true)
                                .value_parser(["claude"])
                                .help("The integration to install"),
                        )
                        .arg(
                            Arg::new("dry-run")
                                .long("dry-run")
                                .action(ArgAction::SetTrue)
                                .help("Show the exact configuration patch without writing"),
                        ),
                )
                .subcommand(
                    Command::new("remove")
                        .about("Remove Margin's entries from one integration (mutation)")
                        .arg(
                            Arg::new("integration")
                                .value_name("INTEGRATION")
                                .required(true)
                                .value_parser(["claude"])
                                .help("The integration to clean"),
                        )
                        .arg(
                            Arg::new("dry-run")
                                .long("dry-run")
                                .action(ArgAction::SetTrue)
                                .help("Show the exact configuration patch without writing"),
                        ),
                )
                .subcommand(Command::new("claude").about(
                    "Translate one Claude hook event on stdin into a capture operation (mutation)",
                )),
        )
        .after_help(after_help())
}

/// The `init` command of SPEC 6.2 with the setup settings of SPEC 6.2 and
/// 5.1. Opposing flags conflict at parse time, so the conflict itself uses
/// the parser-error envelope.
fn init_command() -> Command {
    Command::new("init")
        .about("Adopt a writing scope and record local settings (mutation)")
        .arg(
            Arg::new("scope")
                .long("scope")
                .value_name("ROOT")
                .action(ArgAction::Append)
                .help("One include root; repeat for several"),
        )
        .arg(
            Arg::new("exclude")
                .long("exclude")
                .value_name("ROOT")
                .action(ArgAction::Append)
                .requires("scope")
                .help("One exclude root subtracted from the scope"),
        )
        .arg(
            Arg::new("suffix")
                .long("suffix")
                .value_name("SUFFIX")
                .action(ArgAction::Append)
                .default_value(".md")
                .help("One allowed file suffix"),
        )
        .arg(
            Arg::new("auto-checkpoint")
                .long("auto-checkpoint")
                .action(ArgAction::SetTrue)
                .conflicts_with("no-auto-checkpoint")
                .help("Enable future configured capture boundaries"),
        )
        .arg(
            Arg::new("no-auto-checkpoint")
                .long("no-auto-checkpoint")
                .action(ArgAction::SetTrue)
                .help("Disable future configured capture boundaries"),
        )
        .arg(
            Arg::new("capture-after-edit")
                .long("capture-after-edit")
                .action(ArgAction::SetTrue)
                .conflicts_with("no-capture-after-edit")
                .help("Capture after the configured editor exits"),
        )
        .arg(
            Arg::new("no-capture-after-edit")
                .long("no-capture-after-edit")
                .action(ArgAction::SetTrue)
                .help("Do not capture after the configured editor exits"),
        )
        .arg(
            Arg::new("editor-argv-json")
                .long("editor-argv-json")
                .value_name("JSON")
                .help("Editor argv as a JSON array of strings, such as [\"nvim\",\"--\"]"),
        )
        .arg(
            Arg::new("create-repo")
                .long("create-repo")
                .action(ArgAction::SetTrue)
                .help("Create the repository when it does not exist"),
        )
        .arg(
            Arg::new("branch")
                .long("branch")
                .value_name("NAME")
                .help("Expected branch, or the initial branch with --create-repo"),
        )
        .arg(
            Arg::new("new-start")
                .long("new-start")
                .action(ArgAction::SetTrue)
                .help("Force a new starting anchor even when one exists"),
        )
        .arg(
            Arg::new("dry-run")
                .long("dry-run")
                .action(ArgAction::SetTrue)
                .help("Report the intended effects without writing"),
        )
}

/// The exit classes and examples of the text help, built from the same
/// tables the structured help reads (SPEC 6.1).
fn after_help() -> String {
    let mut text = String::from("Exit classes:\n");
    for (exit, class, codes) in EXIT_CLASSES {
        let codes = if codes.is_empty() {
            String::new()
        } else {
            format!(" ({})", codes.join(", "))
        };
        let _ = writeln!(text, "  {exit:<3} {class}{codes}");
    }
    text.push_str("\nExamples:\n");
    for example in EXAMPLES {
        let _ = writeln!(text, "  {example}");
    }
    text
}

/// The canonical command name of one parsed invocation, such as
/// `hook install`, or `None` when no command was given.
fn command_of(matches: &ArgMatches) -> Option<String> {
    let mut path: Vec<&str> = Vec::new();
    let mut node = matches;
    while let Some((name, sub)) = node.subcommand() {
        path.push(name);
        node = sub;
    }
    if path.is_empty() {
        None
    } else {
        Some(path.join(" "))
    }
}

/// Identifies the command one raw argv names, for the `command` field of a
/// parser-error envelope (SPEC 7.1). The walk is shallow on purpose: any
/// doubt yields `None`, and the envelope reports `command: null` instead
/// of a guess.
fn command_path(argv: &[OsString], root: &Command) -> Option<String> {
    let mut node: &Command = root;
    let mut path: Vec<&str> = Vec::new();
    let mut value_pending = false;
    for token in argv.iter().skip(1) {
        let token = token.to_str()?;
        if value_pending {
            value_pending = false;
            continue;
        }
        if token == "--" {
            break;
        }
        if let Some(body) = token.strip_prefix("--") {
            let name = body.split('=').next().unwrap_or("");
            if GLOBAL_VALUE_OPTIONS.contains(&name) {
                value_pending = !body.contains('=');
                continue;
            }
            // The global flags resolve at the root, where they are defined;
            // a long option this node does not define ends the walk with
            // what is known so far.
            let Some(argument) = long_arg(node, name) else {
                break;
            };
            value_pending = argument.get_action().takes_values() && !body.contains('=');
            continue;
        }
        if token.starts_with('-') && token.len() > 1 {
            continue;
        }
        match node.find_subcommand(token) {
            Some(sub) => {
                path.push(sub.get_name());
                node = sub;
            }
            None => break,
        }
    }
    if path.is_empty() {
        None
    } else {
        Some(path.join(" "))
    }
}

/// One long option of `node`, when this node defines it.
fn long_arg<'a>(node: &'a Command, name: &str) -> Option<&'a Arg> {
    node.get_arguments()
        .find(|argument| argument.get_long() == Some(name))
}

// ---------------------------------------------------------------------------
// Structured help and version data (SPEC 6.1)
// ---------------------------------------------------------------------------

/// The structured command help of `--help --json`, generated from the live
/// parser so the inventory cannot drift from the accepted options
/// (SPEC 16.8).
fn structured_help(root: &mut Command) -> Value {
    root.build();
    let usage = root.render_usage().to_string();
    json!({
        "name": root.get_name(),
        "version": env!("CARGO_PKG_VERSION"),
        "usage": usage,
        "global_options": root
            .get_arguments()
            .filter(|argument| !argument.is_positional())
            .filter_map(option_entry)
            .collect::<Vec<Value>>(),
        "commands": command_entries(root),
        "exit_classes": EXIT_CLASSES
            .iter()
            .map(|(exit, class, codes)| json!({
                "exit": exit,
                "class": class,
                "codes": codes,
            }))
            .collect::<Vec<Value>>(),
        "examples": EXAMPLES,
    })
}

/// One entry per command, nested paths included, in declaration order.
fn command_entries(node: &Command) -> Vec<Value> {
    let mut entries = Vec::new();
    for sub in node.get_subcommands() {
        push_command_entry(sub, "", &mut entries);
    }
    entries
}

/// Appends the entry of `sub` and of every nested command below it. Only
/// runnable commands receive an entry; a grouping command such as `hook`
/// contributes its children under their full paths, matching the SPEC 6.2
/// inventory. Clap's built-in `help` subcommand is interface plumbing, not
/// part of the inventory.
fn push_command_entry(sub: &Command, prefix: &str, entries: &mut Vec<Value>) {
    if sub.get_name() == "help" {
        return;
    }
    let name = format!("{prefix}{}", sub.get_name());
    if sub.has_subcommands() {
        let nested_prefix = format!("{name} ");
        for nested in sub.get_subcommands() {
            push_command_entry(nested, &nested_prefix, entries);
        }
        return;
    }
    entries.push(json!({
        "name": name,
        "summary": sub.get_about().map(|about| about.to_string()),
        "effect": command_effect(&name),
        "options": sub
            .get_arguments()
            .filter(|argument| !argument.is_positional() && !argument.is_global_set())
            .filter_map(option_entry)
            .collect::<Vec<Value>>(),
        "arguments": sub
            .get_arguments()
            .filter(|argument| argument.is_positional())
            .filter_map(positional_entry)
            .collect::<Vec<Value>>(),
    }));
}

/// One option of the structured inventory, read back from the parser.
fn option_entry(argument: &Arg) -> Option<Value> {
    if argument.is_hide_set() {
        return None;
    }
    let long = argument.get_long()?;
    let value = argument
        .get_value_names()
        .and_then(|names| names.first())
        .map(|name| name.to_string());
    let default = match argument.get_action() {
        ArgAction::SetTrue | ArgAction::SetFalse => Value::Bool(false),
        _ => argument
            .get_default_values()
            .first()
            .map(|value| value.to_string_lossy().into_owned())
            .map(Value::String)
            .unwrap_or(Value::Null),
    };
    Some(json!({
        "option": format!("--{long}"),
        "value": value,
        "default": default,
        "repeatable": matches!(argument.get_action(), ArgAction::Append),
        "required": argument.is_required_set(),
        "description": argument.get_help().map(|help| help.to_string()),
    }))
}

/// One positional argument of the structured inventory.
fn positional_entry(argument: &Arg) -> Option<Value> {
    if argument.is_hide_set() {
        return None;
    }
    Some(json!({
        "argument": argument.get_id().as_str(),
        "value": argument
            .get_value_names()
            .and_then(|names| names.first())
            .map(|name| name.to_string()),
        "repeatable": argument
            .get_num_args()
            .map(|range| range.max_values() > 1)
            .unwrap_or(false),
        "required": argument.is_required_set(),
        "description": argument.get_help().map(|help| help.to_string()),
    }))
}

/// What one command does to the repository. The text `about` of every
/// command repeats this word, and a test holds the two together.
fn command_effect(path: &str) -> &'static str {
    match path {
        "init" | "checkpoint" | "review" | "restore" | "hook install" | "hook remove"
        | "hook claude" | "note" => "mutation",
        "status" | "diff" | "show" | "log" | "doctor" | "notes" => "query",
        "tui" | "edit" | "desk" => "interactive",
        // A test fails when a new command reaches a caller unnamed here.
        _ => "unknown",
    }
}

/// The version data of `--version --json`: build, schema, engine, and Git
/// capability information (SPEC 6.1). The Git version is queried through
/// the bounded adapter; the object format is reported when the discovery
/// root lies inside a repository.
fn version_data(git: &Git) -> Result<Value, OperationError> {
    let git_version = git.git_version().map_err(OperationError::from)?;
    let object_format = if git.is_inside_work_tree().unwrap_or(false) {
        git.object_format().ok().map(|format| format.as_str())
    } else {
        None
    };
    Ok(json!({
        "name": "margin",
        "version": env!("CARGO_PKG_VERSION"),
        "envelope_schema_version": ENVELOPE_SCHEMA_VERSION,
        "engine_version": ENGINE_VERSION,
        "commit_trailer_version": COMMIT_TRAILER_VERSION,
        "git": {
            "version": git_version,
            "object_format": object_format,
        },
    }))
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// The bounded result of one finished command: its exit class and its
/// complete standard output and standard error.
struct Finished {
    /// The SPEC 7.5 exit class.
    exit: u8,
    /// Complete standard output, when the command wrote any.
    stdout: Option<String>,
    /// Exact standard output bytes of the one raw export path, which a
    /// document can carry outside UTF-8 (SPEC 6.3). Set takes precedence
    /// over `stdout`.
    stdout_bytes: Option<Vec<u8>>,
    /// Complete standard error, when the command wrote any.
    stderr: Option<String>,
}

/// The invocation-level choices every command shares (SPEC 6.1).
struct Invocation {
    /// Whether the caller asked for the JSON envelope.
    json: bool,
    /// The repository discovery root.
    cwd: PathBuf,
    /// The bounded execution budget.
    timeout_ms: u64,
}

impl Invocation {
    /// Reads the global options and validates the discovery root. An
    /// unusable root is an invalid input, not an execution failure.
    fn from_matches(matches: &ArgMatches) -> Result<Invocation, OperationError> {
        let cwd = PathBuf::from(
            matches
                .get_one::<String>("cwd")
                .cloned()
                .unwrap_or_else(|| ".".to_string()),
        );
        let metadata = fs::metadata(&cwd).map_err(|error| {
            OperationError::new(
                "INVALID_ARGUMENT",
                format!("--cwd {} cannot be inspected: {error}", cwd.display()),
            )
        })?;
        if !metadata.is_dir() {
            return Err(OperationError::new(
                "INVALID_ARGUMENT",
                format!("--cwd {} is not a directory", cwd.display()),
            ));
        }
        Ok(Invocation {
            json: matches.get_flag("json"),
            cwd,
            timeout_ms: *matches
                .get_one::<u64>("timeout-ms")
                .unwrap_or(&DEFAULT_TIMEOUT_MS),
        })
    }
}

/// The Git adapter every command shares, bound to the discovery root and
/// the execution budget the caller chose. The `--color` setting is
/// validated by the parser and recorded there; this build decorates no
/// text output, and JSON output is never colored.
fn adapter(invocation: &Invocation) -> Git {
    Git::from_env(std::env::vars_os())
        .at(&invocation.cwd)
        .with_limits(Limits {
            timeout: Duration::from_millis(invocation.timeout_ms),
            ..Limits::default()
        })
}

/// Parses one command line and produces its complete result. `--json` is
/// detected before normal argument-error reporting, so a syntax failure
/// uses the same envelope as a normal result (SPEC 6.1).
fn execute<I, T>(argv: I) -> Finished
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let argv: Vec<OsString> = argv.into_iter().map(Into::into).collect();
    let json = argv
        .iter()
        .skip(1)
        .any(|token| token == OsStr::new("--json"));
    match command().try_get_matches_from(argv.clone()) {
        Ok(matches) => dispatch(&matches),
        Err(error) => match error.kind() {
            ErrorKind::DisplayHelp | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
                if json {
                    let mut root = command();
                    let help = structured_help(&mut root);
                    finish_envelope(&Envelope::success(Some("help"), help), 0, true)
                } else {
                    let mut text = error.render().to_string();
                    if !text.ends_with('\n') {
                        text.push('\n');
                    }
                    Finished {
                        exit: 0,
                        stdout: Some(text),
                        stdout_bytes: None,
                        stderr: None,
                    }
                }
            }
            // The parser carries `--version` as an ordinary flag, so clap
            // itself never reports a version display.
            _ => parse_error(&error, &argv, json),
        },
    }
}

/// Reports one parser failure: one bounded envelope in JSON mode, one
/// bounded stderr line in text mode. Clap errors are caught here rather
/// than allowed to terminate through default printing (SPEC 6.1).
fn parse_error(error: &clap::Error, argv: &[OsString], json: bool) -> Finished {
    let operation_error = OperationError::new(
        "INVALID_ARGUMENT",
        bounded_message(&error.render().to_string()),
    )
    .recovery(&["margin", "--help", "--json"]);
    let command = command_path(argv, &command());
    let envelope = Envelope::from_error(command.as_deref(), &operation_error);
    finish_envelope(&envelope, exit_class("INVALID_ARGUMENT"), json)
}

/// Runs one parsed invocation. Every command calls the shared operation
/// layer; commands whose algorithms later tasks provide report the one
/// shared refusal with unchanged effects.
fn dispatch(matches: &ArgMatches) -> Finished {
    let invocation = match Invocation::from_matches(matches) {
        Ok(invocation) => invocation,
        Err(error) => {
            let command = command_of(matches);
            return error_finished(command.as_deref(), &error, matches.get_flag("json"));
        }
    };
    if matches.get_flag("version") {
        return version_finished(&invocation);
    }
    let Some((name, sub)) = matches.subcommand() else {
        return error_finished(None, &missing_command(), invocation.json);
    };
    let git = adapter(&invocation);
    let operations = Operations::new(&git);
    match name {
        "notes" if sub.get_flag("schema") => Finished {
            exit: 0,
            stdout: Some(crate::notes::SCHEMA.to_string()),
            stdout_bytes: None,
            stderr: None,
        },
        "notes" => {
            let outcome =
                operations.list_notes(sub.get_one::<String>("revision").map(String::as_str));
            outcome_finished_text(
                "notes",
                &outcome,
                |data| notes_value(data, &data.commit),
                |data| {
                    let mut text = data.unreadable.clone().unwrap_or_default();
                    for note in &data.notes {
                        text.push_str(&format!(
                            "{}{}: {}\n",
                            note.label(),
                            note.group
                                .as_ref()
                                .map(|g| format!(" ({g})"))
                                .unwrap_or_default(),
                            display_escape(&note.text)
                        ));
                    }
                    if text.is_empty() {
                        text.push_str("No notes.\n");
                    }
                    text
                },
                invocation.json,
            )
        }
        "note" => {
            let outcome = operations.append_note(&crate::notes::AppendRequest {
                commit: None,
                group: sub.get_one::<String>("group").cloned(),
                by: crate::notes::Author::Agent,
                text: sub
                    .get_one::<String>("text")
                    .expect("required text")
                    .clone(),
            });
            outcome_finished_text(
                "note",
                &outcome,
                |data| notes_value(data, &data.commit),
                |data| {
                    format!(
                        "Note saved on {}.\n",
                        &data.commit[..12.min(data.commit.len())]
                    )
                },
                invocation.json,
            )
        }
        "init" => {
            let editor_argv = match editor_argv_of(sub, invocation.json) {
                Ok(editor_argv) => editor_argv,
                Err(finished) => return finished,
            };
            let outcome = operations.initialize_scope(&InitRequest {
                scope: scope_input_of(sub),
                create_repository: sub.get_flag("create-repo"),
                branch: sub.get_one::<String>("branch").cloned(),
                new_start: sub.get_flag("new-start"),
                auto_checkpoint: switch_of(sub, "auto-checkpoint", "no-auto-checkpoint"),
                capture_after_edit: switch_of(sub, "capture-after-edit", "no-capture-after-edit"),
                editor_argv,
                dry_run: sub.get_flag("dry-run"),
            });
            outcome_finished("init", &outcome, project_initialization, invocation.json)
        }
        "status" => {
            // No `--scope` option exists: the query adopts the stored scope
            // of the local settings (SPEC 8.2) and reports the typed
            // uninitialized state when none is stored. `--path` filters the
            // reported uncaptured paths; every count stays whole-scope
            // (SPEC 6.3).
            let outcome = operations.inspect_workspace(&InspectRequest {
                scope: None,
                path_filter: option_values(sub, "path"),
            });
            outcome_finished_text(
                "status",
                &outcome,
                project_inspection,
                status_text,
                invocation.json,
            )
        }
        "diff" => {
            // The presentation inputs of SPEC 6.3: the path filter, change
            // selection, cursor, limit, and working preview. None of them
            // can change the review identity or the whole-scope totals.
            let outcome = operations.build_review(&DiffRequest {
                scope: None,
                path_filter: option_values(sub, "path"),
                change: sub.get_one::<String>("change").cloned(),
                cursor: sub.get_one::<String>("cursor").cloned(),
                limit: sub.get_one::<u64>("limit").copied(),
                working: sub.get_flag("working"),
            });
            outcome_finished_text("diff", &outcome, project_review, diff_text, invocation.json)
        }
        "show" => {
            // The presentation inputs of SPEC 6.3: the format, the byte and
            // line bounds, and the terminal protection of the raw export.
            // The raw exact-byte mode carries every byte, so it cannot pair
            // with the envelope or with a bound.
            let raw = sub.get_one::<String>("format").map(String::as_str) == Some("raw");
            let max_bytes = sub.get_one::<u64>("max-bytes").copied();
            let max_lines = sub.get_one::<u64>("max-lines").copied();
            if raw && invocation.json {
                return error_finished(
                    Some("show"),
                    &OperationError::new(
                        "INVALID_ARGUMENT",
                        "the raw exact-byte export and --json are mutually exclusive (SPEC 6.3)",
                    ),
                    invocation.json,
                );
            }
            if raw && (max_bytes.is_some() || max_lines.is_some()) {
                return error_finished(
                    Some("show"),
                    &OperationError::new(
                        "INVALID_ARGUMENT",
                        "the raw exact-byte export returns every byte, so --max-bytes and \
                         --max-lines cannot bound it",
                    ),
                    invocation.json,
                );
            }
            if let Some(error) =
                raw_destination_problem(raw, sub.get_flag("allow-tty"), io::stdout().is_terminal())
            {
                return error_finished(Some("show"), &error, invocation.json);
            }
            let outcome = operations.show_document(&ShowRequest {
                path: sub
                    .get_one::<String>("path")
                    .expect("--path is required")
                    .clone(),
                revision: sub
                    .get_one::<String>("revision")
                    .expect("--revision is required")
                    .clone(),
                max_bytes,
                max_lines,
                raw,
            });
            if raw {
                show_raw_finished(&outcome)
            } else {
                outcome_finished_text("show", &outcome, project_show, show_text, invocation.json)
            }
        }
        "log" => {
            // The presentation inputs of SPEC 6.3: the entry limit, the
            // cursor, the path filter, and the checkpoint display policy.
            // None of them narrows the unfiltered walk that labels the
            // entries, so a short log never hides an old baseline.
            let outcome = operations.list_history(&LogRequest {
                scope: None,
                path_filter: option_values(sub, "path"),
                cursor: sub.get_one::<String>("cursor").cloned(),
                limit: sub.get_one::<u64>("limit").copied(),
                all_checkpoints: sub.get_flag("all-checkpoints"),
            });
            outcome_finished_text("log", &outcome, project_log, log_text, invocation.json)
        }
        "checkpoint" => {
            let outcome = operations.create_checkpoint(&CheckpointRequest {
                scope: None,
                message: sub.get_one::<String>("message").cloned(),
                include_staged: sub.get_flag("include-staged"),
                expect_head: sub.get_one::<String>("expect-head").cloned(),
                expect_index: sub.get_one::<String>("expect-index").cloned(),
                expect_working: sub.get_one::<String>("expect-working").cloned(),
                dry_run: sub.get_flag("dry-run"),
            });
            outcome_finished("checkpoint", &outcome, project_checkpoint, invocation.json)
        }
        "review" => {
            let input = sub.get_one::<String>("input").expect("--input is required");
            if let Some(finished) = check_plan_input(input, "review", invocation.json) {
                return finished;
            }
            let plan = match read_plan(input, "review", invocation.json) {
                Ok(plan) => plan,
                Err(finished) => return finished,
            };
            let request = ReviewRequest {
                scope: None,
                plan,
                dry_run: sub.get_flag("dry-run"),
            };
            if sub.get_flag("resume") {
                // The retained-plan recovery of SPEC 9.7: the same decision
                // contract the application reports, over the proved
                // retained state instead of a new application.
                let outcome = operations.resume_review(&request);
                outcome_finished_text(
                    "review",
                    &outcome,
                    project_decision,
                    decision_text,
                    invocation.json,
                )
            } else if sub.get_flag("dry-run") {
                // The dry run of SPEC 6.4: the validated plan with its
                // proved intended result, written nowhere.
                let outcome = operations.validate_review(&request);
                outcome_finished_text(
                    "review",
                    &outcome,
                    project_decision,
                    decision_text,
                    invocation.json,
                )
            } else {
                // The application of SPEC 9.4: the same decision contract
                // the dry run reported, extended with the verified commit
                // it created — or with the receipt an exact retry matched.
                let outcome = operations.apply_review(&request);
                outcome_finished_text(
                    "review",
                    &outcome,
                    project_decision,
                    decision_text,
                    invocation.json,
                )
            }
        }
        "restore" => {
            let input = sub.get_one::<String>("input").expect("--input is required");
            if let Some(finished) = check_plan_input(input, "restore", invocation.json) {
                return finished;
            }
            // The strict plan parser of the restore contract names the
            // request, the pinned context, the one document, and the source
            // revision (SPEC 7.4); the operation owns every later rule.
            let plan = match read_plan(input, "restore", invocation.json) {
                Ok(plan) => plan,
                Err(finished) => return finished,
            };
            let outcome = operations.restore_document(&RestoreRequest {
                scope: None,
                plan,
                dry_run: sub.get_flag("dry-run"),
            });
            outcome_finished_text(
                "restore",
                &outcome,
                project_restore,
                restore_text,
                invocation.json,
            )
        }
        "doctor" => {
            // The read-only diagnostics of SPEC 9.7: the observed state and
            // the bounded recovery argv, written nowhere.
            let outcome = operations.inspect_recovery(&RecoveryRequest { scope: None });
            outcome_finished_text(
                "doctor",
                &outcome,
                project_recovery,
                doctor_text,
                invocation.json,
            )
        }
        "tui" => {
            if let Some(finished) = reject_json("tui", invocation.json) {
                return finished;
            }
            let path = sub.get_one::<String>("path").cloned();
            if let Some(path) = &path
                && let Err(error) = crate::domain::parse_document_path(path)
            {
                return error_finished(
                    Some("tui"),
                    &OperationError::new(error.public_code(), error.to_string()),
                    invocation.json,
                );
            }
            // The interactive surface owns its terminal, so it cannot run
            // through the buffered output contract every other command
            // uses. Redirected streams get one typed refusal instead
            // (SPEC 5.1, 11.5).
            if !crate::tui::run::has_terminal() {
                return error_finished(
                    Some("tui"),
                    &OperationError::new(
                        "OPERATION_UNAVAILABLE",
                        "the interactive surface needs a terminal for input and drawing; run \
                         margin tui without redirection",
                    ),
                    invocation.json,
                );
            }
            let view = sub
                .get_one::<String>("view")
                .and_then(|raw| crate::tui::View::parse(raw))
                .unwrap_or_default();
            // The global `--color` setting reaches the surface as its
            // color-off mode (SPEC 11.4): `never` draws a plain
            // monochrome surface whose words and symbols carry every
            // state. The reduced-update flag draws only changed frames.
            let color = matches
                .get_one::<String>("color")
                .map(String::as_str)
                .and_then(crate::tui::ColorMode::parse)
                .unwrap_or_default();
            let options = crate::tui::TuiOptions {
                path,
                view,
                color,
                reduced_updates: sub.get_flag("reduced-updates"),
            };
            match crate::tui::launch(&invocation.cwd, &operations, &options) {
                Ok(()) => Finished {
                    exit: 0,
                    stdout: None,
                    stdout_bytes: None,
                    stderr: None,
                },
                Err(error) => error_finished(
                    Some("tui"),
                    &OperationError::new("IO_FAILED", error.message),
                    invocation.json,
                ),
            }
        }
        "edit" => {
            if let Some(finished) = reject_json("edit", invocation.json) {
                return finished;
            }
            let path = sub
                .get_one::<String>("path")
                .expect("--path is required")
                .clone();
            edit_finished(&operations, &path)
        }
        "desk" => {
            if let Some(finished) = reject_json("desk", invocation.json) {
                return finished;
            }
            // The desk needs this terminal twice: tmux attaches on it
            // outside a session, and the composed windows answer to the
            // client inside one (SPEC 12.6). The refusal stays typed, as
            // for the surface and the editor handoff.
            if !crate::tui::run::has_terminal() {
                return error_finished(
                    Some("desk"),
                    &OperationError::new(
                        "OPERATION_UNAVAILABLE",
                        "the writing desk needs a terminal to attach tmux; run margin desk \
                         without redirection",
                    ),
                    invocation.json,
                );
            }
            let agent: Vec<String> = sub
                .get_many::<String>("agent")
                .map(|values| values.cloned().collect())
                .unwrap_or_default();
            // The desk owns no repository state: it composes panes and
            // reports them, so the interactive text contract carries the
            // report (SPEC 6.2, 12.6).
            match integrations::tmux::compose_desk(&invocation.cwd, &agent) {
                Ok(report) => Finished {
                    exit: 0,
                    stdout: Some(report),
                    stdout_bytes: None,
                    stderr: None,
                },
                Err(error) => error_finished(Some("desk"), &error, invocation.json),
            }
        }
        "hook" => match sub.subcommand() {
            // The one setup action of SPEC 12.4: plan against the bytes on
            // disk, and write only after the target proves its fingerprint.
            // A dry run answers with the same envelope and writes nothing.
            Some(("install", matches)) => outcome_finished_text(
                "hook install",
                &integrations::claude::install(&operations, matches.get_flag("dry-run")),
                project_hook_setup,
                hook_setup_text,
                invocation.json,
            ),
            Some(("remove", matches)) => outcome_finished_text(
                "hook remove",
                &integrations::claude::remove(&operations, matches.get_flag("dry-run")),
                project_hook_setup,
                hook_setup_text,
                invocation.json,
            ),
            _ => {
                // The Claude adapter answers in the upstream hook protocol,
                // not the Margin envelope (SPEC 6.2), so `--json` is a typed
                // refusal: no envelope may ever enter the protocol channel.
                if invocation.json {
                    let error = OperationError::new(
                        "INVALID_ARGUMENT",
                        "the hook adapter answers in the upstream hook protocol, not the Margin \
                         envelope; omit --json",
                    );
                    return Finished {
                        exit: exit_class(error.code),
                        stdout: None,
                        stdout_bytes: None,
                        stderr: Some(text_error(error.code, &error.message)),
                    };
                }
                // A terminal standard input carries no event. Refusing
                // keeps a manual run from waiting for bytes that never
                // arrive; the event is missing, and a missing event never
                // implies capture or approval (SPEC 12.3).
                if io::stdin().is_terminal() {
                    return hook_finished(integrations::claude::refused(
                        "no hook payload was provided on standard input; pipe one upstream event \
                         instead",
                    ));
                }
                let mut bytes = Vec::new();
                // One bounded read: the adapter sees at most the payload
                // bound plus one byte, so an oversized payload is a typed
                // refusal, never an unbounded read (SPEC 12.2).
                if io::stdin()
                    .take(integrations::claude::MAX_PAYLOAD_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
                    .is_err()
                {
                    return hook_finished(integrations::claude::refused(
                        "the hook payload could not be read from standard input; the event never \
                         arrived",
                    ));
                }
                hook_finished(integrations::claude::run(
                    &operations,
                    &invocation.cwd,
                    &bytes,
                ))
            }
        },
        other => unreachable!("the parser accepts only known commands: {other}"),
    }
}

/// The typed failure for a command line that names no command.
fn missing_command() -> OperationError {
    OperationError::new("INVALID_ARGUMENT", "a command is required")
        .recovery(&["margin", "--help", "--json"])
}

fn notes_value(data: &impl Serialize, commit: &str) -> Value {
    let mut value = serde_json::to_value(data).expect("notes serialize");
    // Operation results contain validated canonical OIDs in either supported format.
    value["object_format"] = json!(if commit.len() == 64 { "sha256" } else { "sha1" });
    value
}

/// Builds the scope input of `init` from its options. No `--scope` means
/// the stored scope of the local settings (SPEC 8.2).
fn scope_input_of(matches: &ArgMatches) -> Option<ScopeInput> {
    let include: Vec<String> = matches
        .get_many("scope")
        .map(|values| values.cloned().collect())
        .unwrap_or_default();
    if include.is_empty() {
        return None;
    }
    Some(ScopeInput {
        include,
        exclude: option_values(matches, "exclude"),
        suffixes: option_values(matches, "suffix"),
    })
}

/// Reads one repeatable string option.
fn option_values(matches: &ArgMatches, name: &str) -> Vec<String> {
    matches
        .get_many(name)
        .map(|values| values.cloned().collect())
        .unwrap_or_default()
}

/// Parses `--editor-argv-json`: a JSON array of at least one string
/// (SPEC 6.2). `None` means the option was not supplied, so initialization
/// keeps the stored editor argv.
fn editor_argv_of(matches: &ArgMatches, json: bool) -> Result<Option<Vec<String>>, Finished> {
    let Some(raw) = matches.get_one::<String>("editor-argv-json") else {
        return Ok(None);
    };
    match serde_json::from_str::<Vec<String>>(raw) {
        Ok(argv) if !argv.is_empty() => Ok(Some(argv)),
        Ok(_) => Err(error_finished(
            Some("init"),
            &OperationError::new(
                "INVALID_ARGUMENT",
                "--editor-argv-json must hold at least one string",
            ),
            json,
        )),
        Err(error) => Err(error_finished(
            Some("init"),
            &OperationError::new(
                "INVALID_ARGUMENT",
                format!("--editor-argv-json must be a JSON array of strings: {error}"),
            ),
            json,
        )),
    }
}

/// Reads one opposing setup-flag pair (SPEC 6.2): `Some(true)` for `on`,
/// `Some(false)` for `off`, `None` when neither flag was supplied. The
/// parser already conflicts the two flags.
fn switch_of(matches: &ArgMatches, on: &str, off: &str) -> Option<bool> {
    if matches.get_flag(on) {
        Some(true)
    } else if matches.get_flag(off) {
        Some(false)
    } else {
        None
    }
}

/// Validates one `--input` value: a readable regular file, or `-` for
/// standard input. Plan parsing itself belongs to the plan contract; this
/// check keeps a missing plan a typed invalid input.
fn check_plan_input(input: &str, command: &str, json: bool) -> Option<Finished> {
    if input == "-" {
        return None;
    }
    let problem = match fs::metadata(input) {
        Ok(metadata) if metadata.is_file() => None,
        Ok(_) => Some(format!("--input {input} is not a regular file")),
        Err(error) => Some(format!("--input {input} cannot be read: {error}")),
    };
    problem.map(|message| {
        error_finished(
            Some(command),
            &OperationError::new("INVALID_ARGUMENT", message),
            json,
        )
    })
}

/// Reads one plan document (SPEC 6.2 `--input`): one bounded read of the
/// regular file the argument already named, or of standard input for `-`.
///
/// The bound keeps one plan document a bounded input (SPEC 7.6); every
/// rule about what the bytes contain belongs to the one strict parser of
/// the domain, so this layer reports only an unreadable or oversized
/// input.
fn read_plan(input: &str, command: &str, json: bool) -> Result<Vec<u8>, Finished> {
    /// The largest read this layer performs, so one oversized document
    /// cannot be buffered whole before its bound is known.
    const READ_BOUND: u64 = MAX_PLAN_BYTES as u64 + 1;
    let mut bytes = Vec::new();
    let outcome = if input == "-" {
        io::stdin().take(READ_BOUND).read_to_end(&mut bytes)
    } else {
        match fs::File::open(input) {
            Ok(file) => file.take(READ_BOUND).read_to_end(&mut bytes),
            Err(error) => Err(error),
        }
    };
    match outcome {
        Ok(_) if bytes.len() > MAX_PLAN_BYTES => Err(error_finished(
            Some(command),
            &OperationError::new(
                "LIMIT_EXCEEDED",
                format!("--input exceeds the {MAX_PLAN_BYTES}-byte plan document bound"),
            ),
            json,
        )),
        Ok(_) => Ok(bytes),
        Err(error) => Err(error_finished(
            Some(command),
            &OperationError::new(
                "INVALID_ARGUMENT",
                format!("--input {input} cannot be read: {error}"),
            ),
            json,
        )),
    }
}

/// Rejects `--json` for one interactive command (SPEC 6.2): where no
/// meaningful noninteractive result exists, the rejection is a typed
/// invalid invocation.
fn reject_json(command: &str, json: bool) -> Option<Finished> {
    if !json {
        return None;
    }
    let error = OperationError::new(
        "INVALID_ARGUMENT",
        format!("the {command} command is interactive and has no noninteractive JSON result"),
    )
    .recovery(&["margin", command, "--help"]);
    Some(error_finished(Some(command), &error, true))
}

/// Runs `margin edit` (SPEC 5.6, 6.2, 12.5): one explicit handoff of one
/// document to the configured editor argv, with the literal path appended
/// and no shell between the two. The command line owns no terminal mode,
/// so the editor meets the terminal exactly as this process found it. Its
/// exit is one refresh boundary — the workspace inspects again and the
/// text view reports the state — and never an approval: a capture runs
/// only while the configured policy enables it, and only after one
/// completed editing pass, through the same shared checkpoint every other
/// save runs.
fn edit_finished(operations: &Operations<'_>, path: &str) -> Finished {
    // The one shared document-path validator of the domain (SPEC 4.1,
    // 6.2), the same one `margin tui` answers: repository-relative, no
    // `..`, no trailing separator.
    let path = match crate::domain::parse_document_path(path) {
        Ok(path) => path,
        Err(error) => {
            return error_finished(
                Some("edit"),
                &OperationError::new(error.public_code(), error.to_string()),
                false,
            );
        }
    };
    if !crate::tui::run::has_terminal() {
        return error_finished(
            Some("edit"),
            &OperationError::new(
                "OPERATION_UNAVAILABLE",
                "the editor handoff needs a terminal for the editor; run margin edit without \
                 redirection",
            ),
            false,
        );
    }
    let settings = match operations.read_settings() {
        outcome if outcome.ok() => {
            outcome
                .data
                .expect("a successful outcome carries data")
                .settings
        }
        outcome => {
            return error_finished(
                Some("edit"),
                &outcome.error.expect("a failed outcome carries its error"),
                false,
            );
        }
    };
    let Some(argv) = settings.editor_argv else {
        return error_finished(
            Some("edit"),
            &OperationError::new(
                "OPERATION_UNAVAILABLE",
                "no editor is configured; set margin init --editor-argv-json [\"nvim\",\"--\"]",
            )
            .recovery(&["margin", "init", "--editor-argv-json", "[\"nvim\",\"--\"]"]),
            false,
        );
    };
    let root = match operations.git().toplevel() {
        Ok(root) => root,
        Err(error) => return error_finished(Some("edit"), &OperationError::from(error), false),
    };
    // The literal path of the document, resolved inside the work tree the
    // adapter adopted; a document that does not exist yet keeps its joined
    // path, because creating it is the editor's business (SPEC 12.5).
    let joined = root.join(path);
    let document = fs::canonicalize(&joined).unwrap_or(joined);
    let status = match crate::tui::run::run_editor(&argv, &document) {
        Ok(status) => status,
        Err(why) => {
            return error_finished(Some("edit"), &OperationError::new("IO_FAILED", why), false);
        }
    };
    let exited = if status.success() {
        "editor returned".to_string()
    } else {
        format!("editor exited with {status}")
    };
    let mut report = String::new();
    if settings.capture_after_edit && status.success() {
        // The configured policy makes one completed editing pass a capture
        // boundary (SPEC 12.5); an editor that exited nonzero is the
        // abandoned pass every terminal tool reads it as.
        let outcome = operations.create_checkpoint(&CheckpointRequest {
            scope: None,
            message: None,
            include_staged: false,
            expect_head: None,
            expect_index: None,
            expect_working: None,
            dry_run: false,
        });
        if !outcome.ok() {
            return error_finished(
                Some("edit"),
                &outcome.error.expect("a failed outcome carries its error"),
                false,
            );
        }
        let data = outcome.data.expect("a successful outcome carries data");
        let captured = match &data.commit {
            Some(commit) => format!(
                "draft saved as {}; nothing was approved",
                crate::tui::render::short(commit)
            ),
            None => "no changes waited to be captured; nothing was approved".to_string(),
        };
        let _ = writeln!(report, "{exited}; {captured}");
    } else {
        let _ = writeln!(
            report,
            "{exited}; {}",
            if status.success() {
                "no checkpoint was made"
            } else {
                "nothing was applied"
            }
        );
    }
    // The refresh boundary itself: the workspace inspects again, and the
    // reported state is the same shape `margin status` prints (SPEC 5.6).
    let outcome = operations.inspect_workspace(&InspectRequest {
        scope: None,
        path_filter: Vec::new(),
    });
    if outcome.ok() {
        let data = outcome.data.expect("a successful outcome carries data");
        let _ = writeln!(report, "{}", status_text(&data).trim_end());
        Finished {
            exit: 0,
            stdout: Some(report),
            stdout_bytes: None,
            stderr: None,
        }
    } else {
        error_finished(
            Some("edit"),
            &outcome.error.expect("a failed outcome carries its error"),
            false,
        )
    }
}

/// Produces the version data (SPEC 6.1): plain and bounded in text mode,
/// one envelope with Git capability information in JSON mode.
fn version_finished(invocation: &Invocation) -> Finished {
    if !invocation.json {
        return Finished {
            exit: 0,
            stdout: Some(format!("margin {}\n", env!("CARGO_PKG_VERSION"))),
            stdout_bytes: None,
            stderr: None,
        };
    }
    let git = adapter(invocation);
    match version_data(&git) {
        Ok(data) => finish_envelope(&Envelope::success(Some("version"), data), 0, true),
        Err(error) => error_finished(Some("version"), &error, true),
    }
}

/// One finished failure around a typed error of the interface layer.
fn error_finished(command: Option<&str>, error: &OperationError, json: bool) -> Finished {
    let envelope = Envelope::from_error(command, error);
    finish_envelope(&envelope, exit_class(error.code), json)
}

/// One finished result around a shared operation outcome. The exit class
/// follows the operation's own typed error (SPEC 7.5). Text mode
/// acknowledges success in one line; a command with its own text view
/// passes it through [`outcome_finished_text`].
fn outcome_finished<T>(
    command: &str,
    outcome: &OperationOutcome<T>,
    project: impl FnOnce(&T) -> Value,
    json: bool,
) -> Finished {
    outcome_finished_text(
        command,
        outcome,
        project,
        |_| "margin: ok\n".to_string(),
        json,
    )
}

/// [`outcome_finished`] with the one-line text view of a successful
/// command. A failure keeps the shared typed error line, whatever the
/// command.
fn outcome_finished_text<T>(
    command: &str,
    outcome: &OperationOutcome<T>,
    project: impl FnOnce(&T) -> Value,
    text: impl FnOnce(&T) -> String,
    json: bool,
) -> Finished {
    let envelope = Envelope::from_outcome(command, outcome, project);
    let exit = outcome
        .error
        .as_ref()
        .map_or(0, |error| exit_class(error.code));
    if json || !outcome.ok() {
        finish_envelope(&envelope, exit, json)
    } else {
        let data = outcome
            .data
            .as_ref()
            .expect("a successful operation carries data");
        Finished {
            exit,
            stdout: Some(text(data)),
            stdout_bytes: None,
            stderr: None,
        }
    }
}

/// Renders one envelope. JSON mode writes one buffered object with an
/// empty stderr (SPEC 7.1, 7.6); text mode writes one bounded line. A
/// command whose task provided a text view passes it through
/// [`outcome_finished_text`] instead of the shared success line.
fn finish_envelope(envelope: &Envelope, exit: u8, json: bool) -> Finished {
    if json {
        let mut body = serde_json::to_string(envelope).expect("an envelope always serializes");
        body.push('\n');
        Finished {
            exit,
            stdout: Some(body),
            stdout_bytes: None,
            stderr: None,
        }
    } else if envelope.ok {
        Finished {
            exit,
            stdout: Some("margin: ok\n".to_string()),
            stdout_bytes: None,
            stderr: None,
        }
    } else {
        let error = envelope
            .error
            .as_ref()
            .expect("a failed envelope carries one error");
        Finished {
            exit,
            stdout: None,
            stdout_bytes: None,
            stderr: Some(text_error(&error.code, &error.message)),
        }
    }
}

/// One bounded stderr line for a typed error in text mode.
fn text_error(code: &str, message: &str) -> String {
    format!("margin: {code}: {message}\n")
}

/// Maps one upstream hook outcome to the process result (SPEC 12.2). The
/// upstream protocol owns both channels, so the outcome never passes
/// through the envelope.
fn hook_finished(outcome: integrations::claude::HookOutcome) -> Finished {
    Finished {
        exit: outcome.exit,
        stdout: None,
        stdout_bytes: outcome.stdout,
        stderr: outcome.stderr,
    }
}

// ---------------------------------------------------------------------------
// Operation projections (SPEC 7.1 `data`)
// ---------------------------------------------------------------------------

/// Projects one inspection result (SPEC 4.2, 6.3): the three separate
/// state dimensions, the identities, the known or null counts with their
/// reasons, the limits, and the blockers.
fn project_inspection(data: &WorkspaceInspection) -> Value {
    json!({
        "object_format": data.object_format.as_str(),
        "branch": data.branch,
        "head": data.head,
        "settings": settings_value(&data.settings),
        "scope": scope_summary_value(&data.scope),
        "baseline": baseline_value(&data.baseline),
        "preservation": data.preservation.as_str(),
        "review": data.review.as_str(),
        "operation": data.operation.as_str(),
        "counts": counts_value(&data.counts),
        "limits": {
            "rendered_comparison_bytes": data.limits.rendered_comparison_bytes,
            "document_bytes": data.limits.document_bytes,
        },
        "eligible": data.scope.blockers.is_empty() && data.policy_blockers.is_empty(),
        "blockers": data
            .policy_blockers
            .iter()
            .map(|blocker| blocker.to_string())
            .collect::<Vec<String>>(),
    })
}

/// Projects the pending and uncaptured counts of one inspection (SPEC 6.3):
/// each unknown count is `null` with one reason, never a fabricated zero.
fn counts_value(counts: &StatusCounts) -> Value {
    json!({
        "pending_files": counts.pending_files,
        "pending_files_reason": counts.pending_files_reason,
        "pending_groups": counts.pending_groups,
        "pending_groups_reason": counts.pending_groups_reason,
        "uncaptured_total": counts.uncaptured.as_ref().map(|uncaptured| uncaptured.total),
        "uncaptured_paths": counts
            .uncaptured
            .as_ref()
            .map(|uncaptured| {
                uncaptured
                    .paths
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect::<Vec<String>>()
            }),
        "uncaptured_reason": counts.uncaptured_reason,
    })
}

/// The one-line text view of a successful status query (SPEC 6.3): the
/// three separate states of SPEC 4.2 with the branch they belong to.
fn status_text(data: &WorkspaceInspection) -> String {
    format!(
        "margin: preservation={} review={} operation={} branch={}\n",
        data.preservation.as_str(),
        data.review.as_str(),
        data.operation.as_str(),
        data.branch.as_deref().unwrap_or("none"),
    )
}

/// Projects one hook configuration action (SPEC 12.4): the exact planned
/// configuration patch, the supported event contract, and the capabilities
/// of the entries the action installs or deletes.
fn project_hook_setup(data: &integrations::claude::SetupOutcome) -> Value {
    json!({
        "action": data.action.as_str(),
        "dry_run": data.dry_run,
        "target": data.path.display().to_string(),
        "target_existed": data.target_existed,
        "fingerprint": data.fingerprint,
        "no_op": data.no_op,
        "changes": data
            .changes
            .iter()
            .map(|change| {
                json!({
                    "event": change.event,
                    "change": if change.removed { "remove" } else { "add" },
                })
            })
            .collect::<Vec<Value>>(),
        "planned_config": data.planned_config,
        "instruction_files": data.instructions,
        "capabilities": {
            "command": integrations::claude::HOOK_COMMAND,
            "events": integrations::claude::SUPPORTED_EVENTS,
            "capture": "one ordinary draft checkpoint, only while auto-checkpoint is stored; \
                        otherwise capture is disabled",
            "feedback": "bounded writer notes in UserPromptSubmit context, independent of capture policy",
            "blocking_events": ["UserPromptSubmit"],
            "never_blocks": ["Stop", "StopFailure", "SessionStart", "SessionEnd"],
            "approval": "none: every capture is one unreviewed draft",
        },
    })
}

/// The one-line text view of one hook configuration action (SPEC 12.4):
/// what the action changed in the target file, or the explicit no-op of a
/// repeated action.
fn hook_setup_text(data: &integrations::claude::SetupOutcome) -> String {
    let action = data.action.as_str();
    let target = integrations::claude::SETTINGS_RELATIVE_PATH;
    let planned = data.changes.len();
    let line = match (data.action, data.dry_run, planned) {
        (integrations::claude::SetupAction::Install, true, 0) => {
            format!("margin: dry run {action}; every entry is already installed")
        }
        (integrations::claude::SetupAction::Install, true, _) => {
            format!("margin: dry run {action}; {planned} entries would be added to {target}")
        }
        (integrations::claude::SetupAction::Install, false, 0) => {
            format!("margin: {action} no-op; every entry is already installed")
        }
        (integrations::claude::SetupAction::Install, false, _) => {
            format!("margin: installed {planned} entries in {target}")
        }
        (integrations::claude::SetupAction::Remove, true, 0) => {
            format!("margin: dry run {action}; no margin entries are installed")
        }
        (integrations::claude::SetupAction::Remove, true, _) => {
            format!("margin: dry run {action}; {planned} entries would be deleted from {target}")
        }
        (integrations::claude::SetupAction::Remove, false, 0) => {
            format!("margin: {action} no-op; no margin entries are installed")
        }
        (integrations::claude::SetupAction::Remove, false, _) => {
            format!("margin: removed {planned} entries from {target}")
        }
    };
    let instruction_count = data.instructions.iter().filter(|p| p.changed).count();
    let mut body = if instruction_count > 0 {
        format!(
            "margin: {}{action}; {planned} hook entries and {instruction_count} instruction files {}\n",
            if data.dry_run { "dry run " } else { "" },
            if data.dry_run {
                "would change"
            } else {
                "changed"
            }
        )
    } else {
        format!("{line}\n")
    };
    if data.dry_run && !data.changes.is_empty() {
        body.push_str(&format!(
            "\n{target}:\n{}\n",
            serde_json::to_string_pretty(&data.planned_config).expect("configuration serializes")
        ));
    }
    if data.dry_run {
        for patch in data.instructions.iter().filter(|p| p.changed) {
            body.push_str(&format!(
                "\n{} (planned contents):\n{}\n",
                patch.path.display(),
                display_escape(&patch.planned_text)
            ));
        }
    }
    body
}

/// Projects the effective local settings without environment secrets
/// (SPEC 8.2). The scope appears as its canonical descriptor and the editor
/// argv as its canonical array, the documented forms of the stored values.
fn settings_value(settings: &LocalSettings) -> Value {
    json!({
        "version": settings.version,
        "branch": settings.branch,
        "scope": settings.scope.as_ref().map(descriptor_value),
        "auto_checkpoint": settings.auto_checkpoint,
        "capture_after_edit": settings.capture_after_edit,
        "editor_argv": settings.editor_argv,
    })
}

/// Projects one scope summary.
fn scope_summary_value(scope: &ScopeSummary) -> Value {
    json!({
        "digest": scope.digest,
        "descriptor": descriptor_value(&scope.descriptor),
        "tracked": scope.tracked,
        "tracked_deleted": scope.tracked_deleted,
        "untracked": scope.untracked,
        "blockers": scope
            .blockers
            .iter()
            .map(|blocker| blocker.to_string())
            .collect::<Vec<String>>(),
    })
}

/// Embeds one canonical scope descriptor through its canonical bytes
/// (SPEC 3.2).
fn descriptor_value(descriptor: &ScopeDescriptor) -> Value {
    serde_json::from_slice(&descriptor.canonical_bytes())
        .expect("the canonical descriptor is one JSON object")
}

/// Projects one baseline state (SPEC 8.5).
fn baseline_value(baseline: &BaselineStatus) -> Value {
    match baseline {
        BaselineStatus::Anchored(marker) => json!({
            "state": "anchored",
            "commit": marker.commit,
            "kind": marker.kind.as_str(),
            "scope": marker.scope_digest,
        }),
        BaselineStatus::Unavailable(reason) => json!({
            "state": "unavailable",
            "reason": reason.to_string(),
        }),
        BaselineStatus::NoCandidate => json!({
            "state": "no_candidate",
        }),
    }
}

/// Projects one initialization result (SPEC 5.1, 6.4).
fn project_initialization(data: &InitializationOutcome) -> Value {
    json!({
        "start_commit": data.start_commit,
        "repository_created": data.repository_created,
        "settings": data
            .settings
            .iter()
            .map(|setting| json!({ "key": setting.key, "value": setting.value }))
            .collect::<Vec<Value>>(),
        "dry_run": data.dry_run,
        "expected_tree": data.expected_tree,
        "tree_reason": data.tree_reason,
        "proposed": data.proposed.as_ref().map(initialization_proposal_value),
    })
}

/// Projects the proposal of one initialization whose repository does not
/// exist yet (SPEC 6.4): the exact scope and configuration as supplied.
fn initialization_proposal_value(proposal: &InitializationProposal) -> Value {
    json!({
        "create_repository": proposal.create_repository,
        "branch": proposal.branch,
        "scope": {
            "include": proposal.scope.include,
            "exclude": proposal.scope.exclude,
            "suffixes": proposal.scope.suffixes,
        },
        "auto_checkpoint": proposal.auto_checkpoint,
        "capture_after_edit": proposal.capture_after_edit,
        "editor_argv": proposal.editor_argv,
    })
}

/// Projects one checkpoint result (SPEC 9.2): the created commit, the exact
/// captured paths, the candidate it reports, and the intended tree.
fn project_checkpoint(data: &CheckpointOutcome) -> Value {
    json!({
        "created": data.commit.is_some(),
        "commit": data.commit,
        "changed": data.changed,
        "candidate": data.candidate,
        "paths": data
            .paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<String>>(),
        "dry_run": data.dry_run,
        "expected_tree": data.expected_tree,
    })
}

/// Projects one review result (SPEC 7.2): the complete review identity, the
/// whole-scope totals, and the bounded detail page the request selected.
fn project_review(data: &ReviewOutcome) -> Value {
    json!({
        "review_id": data.review_id,
        "engine_version": data.engine_version,
        "git_version": data.git_version,
        "branch_ref": data.branch_ref,
        "scope_id": data.scope_id,
        "scope": descriptor_value(&data.scope),
        "baseline_oid": data.baseline_oid,
        "candidate_oid": data.candidate_oid,
        "candidate_scope_digest": data.candidate_scope_digest,
        "manifest_digest": data.manifest_digest,
        "changes": data
            .changes
            .iter()
            .map(project_change)
            .collect::<Vec<Value>>(),
        "scope_totals": {
            "files": data.scope_totals.files,
            "groups": data.scope_totals.groups,
        },
        "omitted_detail_count": data.omitted_detail_count,
        "complete": data.complete,
        "next_cursor": data.next_cursor,
        "working": data.working,
        "actionable": data.actionable,
    })
}

/// Projects one decided review plan (SPEC 6.4, 9.3-9.4): the pinned
/// comparison it decides, the decisions, and the intended result one
/// disposable index proved. A dry run writes nothing; an application adds
/// the verified commit it created. An exact retry carries the verified
/// receipt commit of the earlier request instead (SPEC 8.7); its `kept`
/// count is `null`, because a receipt records no group count and history is
/// not regrouped merely to acknowledge completion.
fn project_decision(data: &ReviewDecision) -> Value {
    json!({
        "request_id": data.request_id,
        "review_id": data.review_id,
        "branch_ref": data.branch_ref,
        "scope_id": data.scope_id,
        "baseline_oid": data.baseline_oid,
        "candidate_oid": data.candidate_oid,
        "candidate_scope_digest": data.candidate_scope_digest,
        "restored": data.restored,
        "kept": data.kept,
        "paths": data.paths,
        "expected_tree": data.expected_tree,
        "result_scope_digest": data.result_scope_digest,
        "message": data.message,
        "dry_run": data.dry_run,
        "commit": data.commit,
        "existing_receipt": data.existing_receipt,
    })
}

/// The one-line text view of a successful decision (SPEC 6.3): the pinned
/// review with its decision counts and the intended result tree. An
/// application names the commit it created, and a retry that matched an
/// earlier receipt names that commit too (SPEC 8.7). An unknown kept count
/// — the receipt case — prints `unknown`, never a fabricated zero.
fn decision_text(data: &ReviewDecision) -> String {
    let commit = data
        .commit
        .as_deref()
        .map(|commit| format!(" commit={commit}"))
        .unwrap_or_default();
    let receipt = data
        .existing_receipt
        .as_deref()
        .map(|commit| format!(" receipt={commit}"))
        .unwrap_or_default();
    let kept = data
        .kept
        .map(|kept| kept.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    format!(
        "margin: review={} restore={} keep={} tree={} paths={}{commit}{receipt}\n",
        data.review_id,
        data.restored.len(),
        kept,
        data.expected_tree,
        if data.paths.is_empty() {
            "-".to_string()
        } else {
            data.paths.join(",")
        },
    )
}

/// Projects one change group of a detail page (SPEC 7.2): its stable
/// identity, path, kind, source ranges, group reason, and bounded
/// presentation. The serialized size stays inside [`detail_weight`], which
/// one test holds against the serializer.
fn project_change(change: &ChangeDetail) -> Value {
    json!({
        "change_id": change.change_id,
        "path": change.path,
        "kind": change.kind,
        "mode_before": change.mode_before,
        "mode_after": change.mode_after,
        "oid_before": change.oid_before,
        "oid_after": change.oid_after,
        "opaque": change.opaque,
        "whole_file": change.whole_file,
        "reason": change.reason,
        "ranges": change
            .ranges
            .iter()
            .map(|range| {
                json!({
                    "old_start": range.old_start,
                    "old_count": range.old_count,
                    "new_start": range.new_start,
                    "new_count": range.new_count,
                })
            })
            .collect::<Vec<Value>>(),
        "ranges_omitted": change.ranges_omitted,
        "presentation": {
            "heading": change.presentation.heading,
            "before": excerpt_value(&change.presentation.before),
            "after": excerpt_value(&change.presentation.after),
            "word_hints": change
                .presentation
                .word_hints
                .iter()
                .map(|hint| {
                    json!({
                        "before_line": hint.before_line,
                        "after_line": hint.after_line,
                        "prefix": hint.prefix,
                        "removed": hint.removed,
                        "added": hint.added,
                        "suffix": hint.suffix,
                    })
                })
                .collect::<Vec<Value>>(),
            "word_hints_omitted": change.presentation.word_hints_omitted,
            "whitespace_only": change.presentation.whitespace_only,
            "link_change": change.presentation.link_change,
            "invisible_change": change.presentation.invisible_change,
        },
    })
}

/// Projects one bounded excerpt (SPEC 7.2): the exact text while it is
/// readable and inside the bound, otherwise one bounded hexadecimal view
/// with the true byte length and truncation flag.
fn excerpt_value(excerpt: &crate::app::BoundedExcerpt) -> Value {
    json!({
        "first_line": excerpt.first_line,
        "last_line": excerpt.last_line,
        "total_bytes": excerpt.total_bytes,
        "truncated": excerpt.truncated,
        "text": excerpt.text,
        "hex": excerpt.hex,
    })
}

/// The one-line text view of a successful diff query (SPEC 6.3): the pinned
/// identity with the whole-scope totals and this page's position.
fn diff_text(data: &ReviewOutcome) -> String {
    format!(
        "margin: review={} files={} groups={} returned={} complete={}\n",
        data.review_id,
        data.scope_totals.files,
        data.scope_totals.groups,
        data.changes.len(),
        data.complete,
    )
}

/// Projects one restoration result (SPEC 7.4, 9.5): the pinned context,
/// the one restored document with its source revision, the intended result
/// one disposable index proved, and the baseline the restoration leaves
/// unchanged. A dry run reports the same data without the commit; an exact
/// retry carries the verified receipt commit of the earlier request instead
/// (SPEC 8.7).
fn project_restore(data: &RestoreOutcome) -> Value {
    json!({
        "request_id": data.request_id,
        "branch_ref": data.branch_ref,
        "scope_id": data.scope_id,
        "candidate_oid": data.candidate_oid,
        "candidate_scope_digest": data.candidate_scope_digest,
        "path": data.path,
        "revision_oid": data.revision_oid,
        "paths": data.paths,
        "expected_tree": data.expected_tree,
        "result_scope_digest": data.result_scope_digest,
        "baseline_oid": data.baseline_oid,
        "message": data.message,
        "dry_run": data.dry_run,
        "commit": data.commit,
        "existing_receipt": data.existing_receipt,
    })
}

/// The one-line text view of one restoration (SPEC 6.3): the document, its
/// source revision, the intended result tree, and the baseline that stays
/// unchanged. An application names the commit it created, and a retry that
/// matched an earlier receipt names that commit too (SPEC 8.7).
fn restore_text(data: &RestoreOutcome) -> String {
    let commit = data
        .commit
        .as_deref()
        .map(|commit| format!(" commit={commit}"))
        .unwrap_or_default();
    let receipt = data
        .existing_receipt
        .as_deref()
        .map(|commit| format!(" receipt={commit}"))
        .unwrap_or_default();
    format!(
        "margin: restore path={} revision={} tree={} baseline={} paths={}{commit}{receipt}\n",
        data.path,
        data.revision_oid,
        data.expected_tree,
        data.baseline_oid.as_deref().unwrap_or("unavailable"),
        if data.paths.is_empty() {
            "-".to_string()
        } else {
            data.paths.join(",")
        },
    )
}

/// Projects one recovery inspection (SPEC 9.7): the observed head, the
/// valid baseline of the one walk, the index tree, the working-scope
/// fingerprint, the staged paths with their whole total, the receipt state
/// of history, and the bounded recovery argv the report assembled.
fn project_recovery(data: &RecoveryInspection) -> Value {
    json!({
        "object_format": data.object_format.as_str(),
        "head": data.head,
        "baseline": baseline_value(&data.baseline),
        "index_tree": data.index_tree,
        "scoped_working_digest": data.scoped_working_digest.as_ref().map(Fingerprint::id),
        "staged_total": data.staged_paths.total,
        "staged_paths": data
            .staged_paths
            .paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<String>>(),
        "receipt": receipt_value(&data.receipt),
        "recovery_argv": data.recovery_argv,
    })
}

/// Projects the receipt state of one recovery inspection (SPEC 8.7, 9.7):
/// present with the commit and the request history holds, absent only when
/// the completed scan established it, or unavailable with the context that
/// stopped the scan.
fn receipt_value(receipt: &ReceiptPresence) -> Value {
    match receipt {
        ReceiptPresence::Present(found) => json!({
            "state": "present",
            "commit": found.commit,
            "kind": found.kind.as_str(),
            "scope": found.scope_digest,
            "request": found.request,
            "plan": found.plan,
        }),
        ReceiptPresence::Absent => json!({ "state": "absent" }),
        ReceiptPresence::Unavailable { what } => json!({ "state": "unavailable", "reason": what }),
    }
}

/// The one-line text view of one recovery inspection (SPEC 6.3, 9.7): the
/// observations a writer compares the recovery commands against, with the
/// receipt state of history. The recovery argv itself stays in the JSON
/// projection, because one bounded line is a summary, not a script.
fn doctor_text(data: &RecoveryInspection) -> String {
    format!(
        "margin: head={} baseline={} index={} working={} staged={} receipt={}\n",
        data.head.as_deref().unwrap_or("none"),
        match &data.baseline {
            BaselineStatus::Anchored(marker) => marker.commit.as_str(),
            BaselineStatus::Unavailable(_) => "unavailable",
            BaselineStatus::NoCandidate => "none",
        },
        data.index_tree.as_deref().unwrap_or("none"),
        data.scoped_working_digest
            .as_ref()
            .map(Fingerprint::id)
            .as_deref()
            .unwrap_or("unknown"),
        data.staged_paths.total,
        match &data.receipt {
            ReceiptPresence::Present(found) => found.commit.as_str(),
            ReceiptPresence::Absent => "absent",
            ReceiptPresence::Unavailable { .. } => "unknown",
        },
    )
}

/// Projects one document view (SPEC 6.3): the immutable revision, the
/// addressed path, and the explicit content state with its bounds. The raw
/// export never reaches this projection — the parser refuses it beside
/// `--json` — but its shape stays honest for any future caller.
fn project_show(data: &ShowOutcome) -> Value {
    let content = match &data.content {
        DocumentContent::Missing => json!({ "kind": "missing" }),
        DocumentContent::Empty { blob_oid, mode } => json!({
            "kind": "empty",
            "blob_oid": blob_oid,
            "mode": mode.as_str(),
            "total_bytes": 0,
        }),
        DocumentContent::Text(text) => json!({
            "kind": "text",
            "blob_oid": text.blob_oid,
            "mode": text.mode.as_str(),
            "text": text.text,
            "total_bytes": text.total_bytes,
            "returned_bytes": text.returned_bytes,
            "total_lines": text.total_lines,
            "returned_lines": text.returned_lines,
            "truncated": text.truncated,
        }),
        DocumentContent::Opaque(view) => json!({
            "kind": "opaque",
            "blob_oid": view.blob_oid,
            "mode": view.mode.as_str(),
            "hex": view.hex,
            "returned_bytes": view.returned_bytes,
            "total_bytes": view.total_bytes,
            "truncated": view.truncated,
        }),
        DocumentContent::Exact { .. } => json!({ "kind": "exact" }),
    };
    json!({
        "revision": data.revision_oid,
        "path": data.path,
        "object_format": data.object_format.as_str(),
        "content": content,
    })
}

/// The text view of one document (SPEC 6.3): one summary line, then the
/// display-escaped content or the bounded hexadecimal view of opaque
/// bytes. Truncation is stated in the summary and after the content,
/// never silent.
fn show_text(data: &ShowOutcome) -> String {
    match &data.content {
        DocumentContent::Missing => format!(
            "margin: path={} revision={} content=missing\n",
            data.path, data.revision_oid
        ),
        DocumentContent::Empty { blob_oid, mode } => format!(
            "margin: path={} revision={} content=empty mode={} bytes=0 blob={}\n",
            data.path,
            data.revision_oid,
            mode.as_str(),
            blob_oid
        ),
        DocumentContent::Text(text) => {
            let mut body = format!(
                "margin: path={} revision={} content=text mode={} blob={} bytes={}/{} \
                 lines={}/{}\n",
                data.path,
                data.revision_oid,
                text.mode.as_str(),
                text.blob_oid,
                text.returned_bytes,
                text.total_bytes,
                text.returned_lines,
                text.total_lines,
            );
            body.push_str(&display_escape(&text.text));
            if text.truncated {
                if !body.ends_with('\n') {
                    body.push('\n');
                }
                body.push_str(&format!(
                    "margin: truncated after {} of {} bytes\n",
                    text.returned_bytes, text.total_bytes
                ));
            }
            body
        }
        DocumentContent::Opaque(view) => {
            let mut body = format!(
                "margin: path={} revision={} content=opaque mode={} blob={} bytes={}/{}\n",
                data.path,
                data.revision_oid,
                view.mode.as_str(),
                view.blob_oid,
                view.returned_bytes,
                view.total_bytes,
            );
            body.push_str(&view.hex);
            body.push('\n');
            if view.truncated {
                body.push_str(&format!(
                    "margin: truncated after {} of {} bytes\n",
                    view.returned_bytes, view.total_bytes
                ));
            }
            body
        }
        // The parser routes the raw export through show_raw_finished, so
        // this view never carries exact bytes.
        DocumentContent::Exact { .. } => "margin: content=exact\n".to_string(),
    }
}

/// Projects one editorial timeline (SPEC 5.5, 6.3): the commit the walk
/// starts from, the adopted scope the cursor binds, and one bounded page of
/// entries in ancestry order. Every entry names its validated kind, or the
/// claimed kind of a marker the walk ignored with the reason, and a
/// collapsed writing pass counts the checkpoints it covers.
fn project_log(data: &LogOutcome) -> Value {
    json!({
        "head": data.head,
        "scope_id": data.scope_id,
        "object_format": data.object_format.as_str(),
        "entries": data
            .entries
            .iter()
            .map(project_log_entry)
            .collect::<Vec<Value>>(),
        "complete": data.complete,
        "next_cursor": data.next_cursor,
        "all_checkpoints": data.all_checkpoints,
    })
}

/// Projects one timeline entry (SPEC 5.5): the commit, its kind label, the
/// bounded subject, the exact committer timestamp, the checkpoint count of
/// a collapsed writing pass, and the reserved facts of a validated marker.
fn project_log_entry(entry: &LogEntry) -> Value {
    json!({
        "commit": entry.commit,
        "kind": entry.kind.as_str(),
        "subject": entry.subject,
        "subject_truncated": entry.subject_truncated,
        "committer_time": entry.committer_time,
        "checkpoints": entry.checkpoints,
        "marker": entry.marker.as_ref().map(marker_value),
        "ignored_reason": entry.ignored_reason,
        "notes": entry.notes.notes,
        "notes_unreadable": entry.notes.unreadable,
    })
}

/// Projects the reserved facts of one validated marker: the scope it
/// belongs to, the baseline a review completed, the request and plan of a
/// receipt, and the source a restoration read.
fn marker_value(marker: &MarkerSummary) -> Value {
    json!({
        "scope": marker.scope_digest,
        "baseline": marker.baseline,
        "request": marker.request,
        "plan": marker.plan,
        "source": marker.source.as_ref().map(|source| json!({
            "revision": source.revision,
            "path": source.path,
        })),
    })
}

/// The text view of one editorial timeline (SPEC 5.5): one summary line,
/// then one display-escaped line per entry in ancestry order. A collapsed
/// writing pass states how many checkpoints it covers; the exact
/// timestamps stay in the JSON view, because a text line that formats them
/// would imply a clock order the timeline never uses.
fn log_text(data: &LogOutcome) -> String {
    let mut body = format!(
        "margin: timeline head={} entries={} complete={}\n",
        data.head.as_deref().unwrap_or("none"),
        data.entries.len(),
        data.complete,
    );
    for entry in &data.entries {
        body.push_str(&format!(
            "{} {} {}",
            entry.kind.as_str(),
            &entry.commit[..entry.commit.len().min(12)],
            display_escape(&entry.subject),
        ));
        if entry.checkpoints > 1 {
            body.push_str(&format!(" (+{} collapsed)", entry.checkpoints - 1));
        }
        body.push('\n');
        for note in &entry.notes.notes {
            body.push_str(&format!(
                "  {}: {}\n",
                note.label(),
                display_escape(&note.text)
            ));
        }
        if let Some(reason) = &entry.notes.unreadable {
            body.push_str(&format!("  {}\n", display_escape(reason)));
        }
    }
    body
}

/// Finishes one raw export (SPEC 6.3): success writes exactly the bytes the
/// operation read — nothing else, no envelope, no trailing newline — while
/// a failure keeps the shared typed error line. An empty document exports
/// zero bytes; an absent path was already refused by the operation, so the
/// two states stay distinguishable at the destination.
fn show_raw_finished(outcome: &ShowResult) -> Finished {
    if outcome.ok() {
        let data = outcome
            .data
            .as_ref()
            .expect("a successful operation carries data");
        let bytes = match &data.content {
            DocumentContent::Exact { bytes, .. } => bytes.clone(),
            DocumentContent::Empty { .. } => Vec::new(),
            DocumentContent::Missing | DocumentContent::Text(_) | DocumentContent::Opaque(_) => {
                unreachable!("a raw export carries exact bytes or one empty document")
            }
        };
        Finished {
            exit: 0,
            stdout: None,
            stdout_bytes: Some(bytes),
            stderr: None,
        }
    } else {
        error_finished(
            Some("show"),
            outcome
                .error
                .as_ref()
                .expect("a failed outcome carries one error"),
            false,
        )
    }
}

/// The terminal protection of the raw export (SPEC 6.3): exact bytes are
/// intended for redirection, so a terminal destination without an explicit
/// `--allow-tty` refuses before one byte is written. Every other mode
/// display-escapes its content, so it needs no such refusal.
fn raw_destination_problem(
    raw: bool,
    allow_tty: bool,
    stdout_is_terminal: bool,
) -> Option<OperationError> {
    if !raw || allow_tty || !stdout_is_terminal {
        return None;
    }
    Some(
        OperationError::new(
            "INVALID_ARGUMENT",
            "the raw exact-byte export would write unescaped bytes to a terminal; redirect the \
             output or pass --allow-tty",
        )
        .recovery(&["margin", "show", "--help"]),
    )
}

/// Display-escapes one document for a text surface (SPEC 6.3, 14.2): every
/// terminal control character — the C0 controls except the newline and tab
/// a view needs, DEL, the C1 controls, and the bidirectional format
/// markers — becomes one visible escape, so document bytes cannot become
/// terminal instructions or reverse the presented reading order. The
/// command line text view and the terminal reading column share this one
/// pass; only the explicit raw export bypasses it.
pub(crate) fn display_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        let unsafe_control = (character.is_control() && character != '\n' && character != '\t')
            || is_bidirectional_marker(character);
        if unsafe_control {
            write!(escaped, "<U+{:04X}>", character as u32)
                .expect("writing one escape to a string cannot fail");
        } else {
            escaped.push(character);
        }
    }
    escaped
}

/// Says whether one character is a bidirectional format marker that can
/// reverse the display order of the text around it (SPEC 14.2).
fn is_bidirectional_marker(character: char) -> bool {
    matches!(
        character as u32,
        0x061C | 0x200E | 0x200F | 0x202A..=0x202E | 0x2066..=0x2069
    )
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Runs one command line and returns its exit class (SPEC 7.5). Output is
/// written once, buffered; a consumer that closed its pipe cannot receive
/// the envelope, and the caller retries by receipt, so a failed write
/// never changes the exit class (SPEC 7.1).
pub fn run<I, T>(argv: I) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let finished = execute(argv);
    if let Some(body) = &finished.stdout_bytes {
        let mut stdout = io::stdout().lock();
        let _ = stdout.write_all(body);
        let _ = stdout.flush();
    } else if let Some(body) = &finished.stdout {
        let mut stdout = io::stdout().lock();
        let _ = stdout.write_all(body.as_bytes());
        let _ = stdout.flush();
    }
    if let Some(body) = &finished.stderr {
        let mut stderr = io::stderr().lock();
        let _ = stderr.write_all(body.as_bytes());
        let _ = stderr.flush();
    }
    finished.exit
}

/// The entry point of the executable: runs the real command line and
/// reports its exit class to the operating system.
pub fn main() -> ExitCode {
    ExitCode::from(run(std::env::args_os()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One argv for the parser, with a program name.
    fn argv(tokens: &[&str]) -> Vec<OsString> {
        std::iter::once(OsString::from("margin"))
            .chain(tokens.iter().map(OsString::from))
            .collect()
    }

    #[test]
    fn the_log_page_default_matches_the_domain_constant() {
        assert_eq!(
            DEFAULT_LOG_PAGE_ARG, "20",
            "the parser default names the documented page of SPEC 6.3"
        );
        assert_eq!(
            DEFAULT_LOG_PAGE_ARG,
            &crate::domain::DEFAULT_LOG_PAGE_ENTRIES.to_string(),
            "the parser default and the operation fallback are one number"
        );
    }

    #[test]
    fn exit_classes_follow_the_spec_table() {
        assert_eq!(exit_class("INVALID_ARGUMENT"), 2);
        assert_eq!(exit_class("LIMIT_EXCEEDED"), 2);
        assert_eq!(exit_class("STALE_CURSOR"), 3);
        assert_eq!(exit_class("STALE_STATE"), 3);
        assert_eq!(exit_class("BRANCH_CHANGED"), 3);
        assert_eq!(exit_class("NOT_INITIALIZED"), 4);
        assert_eq!(exit_class("MISSING_IDENTITY"), 4);
        assert_eq!(exit_class("BUSY"), 4);
        assert_eq!(exit_class("BASELINE_UNAVAILABLE"), 4);
        assert_eq!(exit_class("OPERATION_UNAVAILABLE"), 4);
        assert_eq!(exit_class("GIT_FAILED"), 5);
        assert_eq!(exit_class("TMUX_FAILED"), 5);
        assert_eq!(exit_class("OUTCOME_UNKNOWN"), 5);
        assert_eq!(exit_class("CONTRACT_VIOLATION"), 5);
        assert_eq!(exit_class("INTERRUPTED"), 130);
        // An unmapped code is treated as a contract violation.
        assert_eq!(exit_class("SOMETHING_ELSE"), 5);
    }

    #[test]
    fn messages_are_bounded_and_carry_no_controls() {
        assert_eq!(bounded_message("plain"), "plain");
        assert_eq!(bounded_message("error: real problem"), "real problem");
        assert_eq!(bounded_message("first\nsecond"), "first");
        // Terminal control characters become U+FFFD inside one line.
        assert_eq!(
            bounded_message("color\u{1b}[31m and\ttab"),
            "color\u{fffd}[31m and\u{fffd}tab"
        );
        let huge = "a".repeat(MAX_MESSAGE_CHARS + 500);
        let bounded = bounded_message(&huge);
        assert!(bounded.chars().count() <= MAX_MESSAGE_CHARS);
        assert!(bounded.ends_with('\u{2026}'));
    }

    #[test]
    fn command_paths_come_from_the_argv_walk() {
        let root = command();
        assert_eq!(
            command_path(&argv(&["--json", "status"]), &root),
            Some("status".to_string())
        );
        assert_eq!(
            command_path(&argv(&["status", "--json"]), &root),
            Some("status".to_string())
        );
        assert_eq!(
            command_path(&argv(&["hook", "install", "claude", "--json"]), &root),
            Some("hook install".to_string())
        );
        assert_eq!(
            command_path(&argv(&["hook", "claude"]), &root),
            Some("hook claude".to_string())
        );
        // A global option's value never impersonates a command.
        assert_eq!(
            command_path(&argv(&["--cwd", "status", "--nope"]), &root),
            None
        );
        // A subcommand option's value is stepped over.
        assert_eq!(
            command_path(&argv(&["init", "--scope", "checkpoint", "--nope"]), &root),
            Some("init".to_string())
        );
        // Everything after `--` is payload, not a command.
        assert_eq!(
            command_path(&argv(&["desk", "--", "status", "--nope"]), &root),
            Some("desk".to_string())
        );
    }

    #[test]
    fn envelope_serializes_in_the_documented_shape() {
        let error = OperationError::new("STALE_REVIEW", "The candidate changed.")
            .recovery(&["margin", "diff", "--json"]);
        let envelope = Envelope::from_error(Some("review"), &error);
        let body = serde_json::to_string(&envelope).unwrap();
        let expected = concat!(
            r#"{"schema_version":1,"command":"review","ok":false,"data":null,"error":"#,
            r#"{"code":"STALE_REVIEW","message":"The candidate changed.","retryable":false,"#,
            r#""recovery_argv":["margin","diff","--json"]},"warnings":[],"effects":"#,
            r#"{"working_tree":"unchanged","index":"unchanged","commit":"none","commit_oid":null,"#,
            r#""repository":"unchanged","configuration":"unchanged","integration":"unchanged"}}"#
        );
        assert_eq!(body, expected);
    }

    #[test]
    fn outcome_envelopes_carry_operations_effects() {
        let outcome = OperationOutcome::<u8>::failure(
            OperationError::new("GIT_FAILED", "git failed").retryable(),
            crate::app::Effects::unknown(),
        );
        let envelope = Envelope::from_outcome("checkpoint", &outcome, |_| Value::Null);
        assert!(!envelope.ok);
        assert!(envelope.data.is_none());
        let error = envelope.error.as_ref().unwrap();
        assert_eq!(error.code, "GIT_FAILED");
        assert!(error.retryable);
        assert_eq!(envelope.effects.commit, "unknown");
        assert_eq!(envelope.effects.working_tree, "unknown");
    }

    #[test]
    fn structured_help_lists_every_leaf_with_its_effect() {
        let mut root = command();
        let help = structured_help(&mut root);
        let commands = help["commands"].as_array().unwrap();
        assert!(!commands.is_empty());
        let mut names: Vec<&str> = commands
            .iter()
            .map(|entry| entry["name"].as_str().unwrap())
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "checkpoint",
                "desk",
                "diff",
                "doctor",
                "edit",
                "hook claude",
                "hook install",
                "hook remove",
                "init",
                "log",
                "note",
                "notes",
                "restore",
                "review",
                "show",
                "status",
                "tui"
            ]
        );
        for entry in commands {
            let name = entry["name"].as_str().unwrap();
            let effect = entry["effect"].as_str().unwrap();
            assert_ne!(effect, "unknown", "{name} needs a documented effect");
            // The text `about` repeats the effect word, so both stay true.
            let summary = entry["summary"].as_str().unwrap();
            assert!(
                summary.contains(effect),
                "the summary of {name} must mention {effect}"
            );
        }
        let globals = help["global_options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["option"].as_str().unwrap())
            .collect::<Vec<&str>>();
        for expected in [
            "--json",
            "--version",
            "--cwd",
            "--timeout-ms",
            "--color",
            "--help",
        ] {
            assert!(
                globals.contains(&expected),
                "the inventory must list {expected}"
            );
        }
        let exits = help["exit_classes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["exit"].as_u64().unwrap())
            .collect::<Vec<u64>>();
        assert_eq!(exits, [0, 2, 3, 4, 5, 130]);
    }

    /// Splits one shell example into argv tokens, honoring double quotes.
    fn example_tokens(example: &str) -> Vec<OsString> {
        let mut tokens = Vec::new();
        let mut current = String::new();
        let mut quoted = false;
        for character in example.chars() {
            match character {
                '"' => quoted = !quoted,
                ' ' if !quoted => {
                    if !current.is_empty() {
                        tokens.push(OsString::from(current.clone()));
                        current.clear();
                    }
                }
                ' ' => current.push(' '),
                other => current.push(other),
            }
        }
        if !current.is_empty() {
            tokens.push(OsString::from(current));
        }
        tokens
    }

    #[test]
    fn documented_examples_still_invoke_the_program() {
        for example in EXAMPLES {
            let tokens = example_tokens(example);
            // A help request is a valid invocation too; it reports itself
            // through the display path rather than parsed matches.
            match command().try_get_matches_from(tokens) {
                Ok(_) => {}
                Err(error) => assert!(
                    matches!(
                        error.kind(),
                        ErrorKind::DisplayHelp
                            | ErrorKind::DisplayVersion
                            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                    ),
                    "the example no longer parses: {example} ({})",
                    error.render()
                ),
            }
        }
    }

    #[test]
    fn parser_errors_are_caught_not_terminated() {
        let finished = execute(argv(&["status", "--nope", "--json"]));
        assert_eq!(finished.exit, 2);
        assert!(finished.stderr.is_none(), "JSON mode writes no stderr");
        let body = finished.stdout.unwrap();
        let envelope: Value = serde_json::from_str(body.trim_end()).unwrap();
        assert_eq!(envelope["command"], "status");
        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error"]["code"], "INVALID_ARGUMENT");
        assert_eq!(envelope["effects"]["working_tree"], "unchanged");
        assert!(!body.contains('\u{1b}'), "no terminal decoration");
    }

    #[test]
    fn json_is_detected_before_argument_errors() {
        for tokens in [
            vec!["--json", "status", "--nope"],
            vec!["status", "--nope", "--json"],
            vec!["--json", "not-a-command"],
        ] {
            let finished = execute(argv(&tokens));
            assert_eq!(finished.exit, 2, "{tokens:?}");
            let body = finished.stdout.as_deref().unwrap_or_default();
            let envelope: Value = serde_json::from_str(body.trim_end()).unwrap_or_else(|error| {
                panic!("{tokens:?} produced no envelope: {error}: {body:?}")
            });
            assert_eq!(envelope["schema_version"], 1);
            assert_eq!(envelope["error"]["code"], "INVALID_ARGUMENT");
        }
        // An unknown command leaves the command field null.
        let finished = execute(argv(&["--json", "not-a-command"]));
        let body = finished.stdout.unwrap();
        let envelope: Value = serde_json::from_str(body.trim_end()).unwrap();
        assert!(envelope["command"].is_null());
    }

    #[test]
    fn text_errors_go_to_stderr() {
        let finished = execute(argv(&["status", "--nope"]));
        assert_eq!(finished.exit, 2);
        assert!(finished.stdout.is_none());
        let stderr = finished.stderr.unwrap();
        assert!(stderr.contains("INVALID_ARGUMENT"), "got: {stderr}");
    }

    #[test]
    fn interactive_commands_reject_json() {
        for tokens in [
            vec!["tui", "--json"],
            vec!["edit", "--path", "a.md", "--json"],
            vec!["desk", "--json", "--", "claude"],
        ] {
            let finished = execute(argv(&tokens));
            assert_eq!(finished.exit, 2, "{tokens:?}");
            let envelope: Value =
                serde_json::from_str(finished.stdout.unwrap().trim_end()).unwrap();
            assert_eq!(envelope["error"]["code"], "INVALID_ARGUMENT");
        }
    }

    #[test]
    fn opposing_setup_flags_conflict() {
        let finished = execute(argv(&[
            "init",
            "--scope",
            "notes/",
            "--auto-checkpoint",
            "--no-auto-checkpoint",
            "--json",
        ]));
        assert_eq!(finished.exit, 2);
        let envelope: Value = serde_json::from_str(finished.stdout.unwrap().trim_end()).unwrap();
        assert_eq!(envelope["error"]["code"], "INVALID_ARGUMENT");
    }

    #[test]
    fn budgets_and_discovery_roots_are_validated() {
        for tokens in [
            vec!["--timeout-ms", "0", "status", "--json"],
            vec!["--cwd", "/no/such/margin-directory", "status", "--json"],
        ] {
            let finished = execute(argv(&tokens));
            assert_eq!(finished.exit, 2, "{tokens:?}");
            let envelope: Value =
                serde_json::from_str(finished.stdout.as_deref().unwrap_or_default().trim_end())
                    .unwrap_or_else(|error| panic!("{tokens:?} produced no envelope: {error}"));
            assert_eq!(envelope["error"]["code"], "INVALID_ARGUMENT");
        }
    }

    #[test]
    fn huge_arguments_stay_bounded() {
        let huge = "A".repeat(100_000);
        let finished = execute(argv(&["--json", &huge]));
        assert_eq!(finished.exit, 2);
        let body = finished.stdout.unwrap();
        let envelope: Value = serde_json::from_str(body.trim_end()).unwrap();
        let message = envelope["error"]["message"].as_str().unwrap();
        assert!(message.chars().count() <= MAX_MESSAGE_CHARS);
    }

    #[test]
    fn help_and_version_json_are_envelopes() {
        let finished = execute(argv(&["--help", "--json"]));
        assert_eq!(finished.exit, 0);
        let envelope: Value = serde_json::from_str(finished.stdout.unwrap().trim_end()).unwrap();
        assert_eq!(envelope["command"], "help");
        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["error"], serde_json::Value::Null);
        assert!(envelope["data"]["commands"].is_array());

        let finished = execute(argv(&["--version", "--json"]));
        assert_eq!(finished.exit, 0);
        let body = finished.stdout.unwrap();
        let envelope: Value = serde_json::from_str(body.trim_end()).unwrap();
        assert_eq!(envelope["command"], "version");
        assert_eq!(envelope["data"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(envelope["data"]["envelope_schema_version"], 1);
        assert_eq!(envelope["data"]["engine_version"], ENGINE_VERSION);
    }

    #[test]
    fn version_and_help_stay_available_in_text_mode() {
        let finished = execute(argv(&["--version"]));
        assert_eq!(finished.exit, 0);
        assert_eq!(finished.stdout.unwrap(), "margin 0.1.0\n");

        let finished = execute(argv(&["--help"]));
        assert_eq!(finished.exit, 0);
        let help = finished.stdout.unwrap();
        assert!(help.contains("Usage:"), "got: {help}");
        assert!(help.contains("Exit classes:"));

        // A bare invocation prints usage rather than a typed failure.
        let finished = execute(argv(&[]));
        assert_eq!(finished.exit, 0);
        assert!(finished.stdout.unwrap().contains("Usage:"));
    }

    #[test]
    fn a_json_call_without_a_command_is_a_typed_failure() {
        let finished = execute(argv(&["--json"]));
        assert_eq!(finished.exit, 2);
        let envelope: Value = serde_json::from_str(finished.stdout.unwrap().trim_end()).unwrap();
        assert!(envelope["command"].is_null());
        assert_eq!(envelope["error"]["code"], "INVALID_ARGUMENT");
    }

    /// One change detail whose every string stresses JSON escaping. The
    /// page bound of SPEC 6.3 is measured with [`detail_weight`] before
    /// anything serializes, so the real serialized projection must never
    /// exceed that measure.
    #[test]
    fn projected_change_detail_never_exceeds_its_weight() {
        let excerpt = |text: Option<String>, hex: Option<String>| crate::app::BoundedExcerpt {
            first_line: u64::MAX,
            last_line: u64::MAX,
            total_bytes: usize::MAX,
            truncated: true,
            text,
            hex,
        };
        let nasty = "\"\\ minustab\t";
        let detail = ChangeDetail {
            change_id: "sha256:".to_string() + &"f".repeat(64),
            path: format!("notes/\"quoted\\{nasty}.md"),
            kind: "modified",
            mode_before: Some("100755"),
            mode_after: Some("100644"),
            oid_before: Some("f".repeat(64)),
            oid_after: Some("e".repeat(64)),
            opaque: true,
            whole_file: true,
            reason: format!("opaque {nasty} content"),
            ranges: vec![
                crate::app::ChangeRange {
                    old_start: u64::MAX,
                    old_count: u64::MAX,
                    new_start: u64::MAX,
                    new_count: u64::MAX,
                };
                8
            ],
            ranges_omitted: 3,
            presentation: crate::app::BoundedPresentation {
                heading: format!("Heading {nasty}"),
                before: excerpt(Some(format!("{nasty} before")), None),
                after: excerpt(None, Some("ab".repeat(64))),
                word_hints: vec![crate::markdown::WordHint {
                    before_line: u64::MAX,
                    after_line: u64::MAX,
                    prefix: format!("pre{nasty}"),
                    removed: "old".to_string(),
                    added: format!("new{nasty}"),
                    suffix: "post".to_string(),
                }],
                word_hints_omitted: 7,
                whitespace_only: true,
                link_change: true,
                invisible_change: true,
            },
        };
        let projected = serde_json::to_string(&project_change(&detail)).unwrap();
        let weight = crate::app::detail_weight(&detail);
        assert!(
            projected.len() <= weight,
            "the projection outgrew its measure: {} > {}",
            projected.len(),
            weight,
        );
    }

    /// The raw export refuses a terminal destination unless the caller
    /// explicitly allows one (SPEC 6.3), and every other combination
    /// passes: the check is a pure decision over three facts, so the
    /// terminal itself is not needed to prove it.
    #[test]
    fn the_raw_export_refuses_a_terminal_destination() {
        let problem = |raw, allow, terminal| {
            raw_destination_problem(raw, allow, terminal).map(|error| error.code.to_string())
        };
        assert_eq!(
            problem(true, false, true),
            Some("INVALID_ARGUMENT".to_string()),
            "an unescaped terminal write refuses"
        );
        assert_eq!(problem(true, true, true), None, "an explicit allow passes");
        assert_eq!(problem(true, false, false), None, "redirection passes");
        assert_eq!(problem(false, false, true), None, "a view escapes instead");
    }

    /// Display escaping keeps the two controls a document view needs and
    /// makes every other control visibly inert (SPEC 6.3, 14.2).
    #[test]
    fn display_escaping_keeps_newlines_and_tabs() {
        assert_eq!(display_escape("plain\ntext\ttab\n"), "plain\ntext\ttab\n");
        let escaped = display_escape("a\u{1b}[31mb\u{7}c\u{0}d\u{7f}e\u{8f}f\u{202e}g");
        assert_eq!(
            escaped, "a<U+001B>[31mb<U+0007>c<U+0000>d<U+007F>e<U+008F>f<U+202E>g",
            "every unsafe control becomes one visible escape"
        );
        assert!(!escaped.contains('\u{1b}'));
    }
}
