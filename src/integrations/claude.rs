//! Translation between Claude Code hooks and application operations.
//!
//! The adapter pins the upstream hook contract of SPEC 12.2: it reads one
//! bounded JSON payload, validates the event and the workspace it names,
//! selects bounded writer feedback before applying the stored capture policy
//! through the shared checkpoint operation, and answers only in the upstream
//! hook protocol. It never
//! prints a Margin envelope into that channel, never emits a response
//! that forces the agent to continue after a Stop, and never grants
//! approval: a boundary capture is one ordinary draft commit, exactly the
//! commit `checkpoint` creates, and a duplicate boundary is the same
//! harmless no-op (SPEC 12.3).
//!
//! The same module owns the configuration surface of that contract
//! (SPEC 12.4): installing and removing the adapter's own entries in the
//! project-local Claude Code settings file, and nothing else in it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::app::{
    CheckpointRequest, Effects, ObservedEffect, OperationError, OperationOutcome, Operations,
};
use crate::cli::bounded_message;
use crate::git::GitError;

/// The largest hook payload one invocation accepts (SPEC 12.2). The bound
/// keeps a hostile or looping writer from feeding Margin unbounded input;
/// anything larger is refused before any parse.
pub const MAX_PAYLOAD_BYTES: usize = 128 * 1024;

/// The upstream hook events the adapter supports (SPEC 12.2): the two
/// turn boundaries and the three session boundaries. Every other event
/// name is one typed refusal, because the adapter must not guess what an
/// unknown boundary means.
pub const SUPPORTED_EVENTS: &[&str] = &[
    "UserPromptSubmit",
    "Stop",
    "StopFailure",
    "SessionStart",
    "SessionEnd",
];

/// One validated upstream event: the pinned contract of the payload
/// (SPEC 12.2). Unknown fields are ignored, because the upstream schema
/// grows independently of Margin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookEvent {
    /// The upstream event name.
    pub name: String,
    /// The workspace directory the event names (`cwd`).
    pub workspace: PathBuf,
    /// Whether upstream already continued the agent from a Stop hook
    /// (`stop_hook_active`). Margin never emits a blocking decision, so
    /// this flag changes no behavior here; the adapter validates its type
    /// only to keep the pinned contract strict.
    pub stop_hook_active: bool,
}

/// Why one payload was refused. A refusal captures and approves nothing
/// (SPEC 12.3): the caller reports one bounded reason and stops.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PayloadError {
    /// No bytes arrived: a missing or interrupted event.
    Empty,
    /// The payload exceeded [`MAX_PAYLOAD_BYTES`].
    TooLarge { bound: usize },
    /// The bytes were not one JSON document.
    Malformed { reason: String },
    /// `hook_event_name` was absent.
    MissingEvent,
    /// `hook_event_name` named no supported event.
    UnsupportedEvent { found: String },
    /// `cwd` was absent, empty, or not a string.
    MissingWorkspace,
    /// A pinned field held the wrong JSON type.
    WrongType { field: &'static str },
}

impl PayloadError {
    /// One bounded, one-line reason for standard error.
    pub fn reason(&self) -> String {
        match self {
            PayloadError::Empty => "the hook payload was empty; the event never arrived".into(),
            PayloadError::TooLarge { bound } => format!(
                "the hook payload exceeds the {bound}-byte bound; nothing was captured or approved"
            ),
            PayloadError::Malformed { reason } => {
                format!("the hook payload is not one JSON document: {reason}")
            }
            PayloadError::MissingEvent => "the hook payload names no hook_event_name".into(),
            PayloadError::UnsupportedEvent { found } => format!(
                "the hook event '{found}' is not supported; nothing was captured or approved"
            ),
            PayloadError::MissingWorkspace => {
                "the hook payload names no workspace directory (cwd)".into()
            }
            PayloadError::WrongType { field } => {
                format!("the hook field '{field}' holds the wrong JSON type")
            }
        }
    }
}

/// Validates one bounded upstream payload (SPEC 12.2). The parse reads
/// only the pinned fields; every unknown field is ignored, so a newer
/// upstream schema cannot fail an installed adapter.
pub fn parse_payload(bytes: &[u8]) -> Result<HookEvent, PayloadError> {
    if bytes.is_empty() {
        return Err(PayloadError::Empty);
    }
    if bytes.len() > MAX_PAYLOAD_BYTES {
        return Err(PayloadError::TooLarge {
            bound: MAX_PAYLOAD_BYTES,
        });
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|error| PayloadError::Malformed {
        reason: error.to_string(),
    })?;
    let object = value.as_object().ok_or_else(|| PayloadError::Malformed {
        reason: "the payload is not a JSON object".into(),
    })?;
    let name = match object.get("hook_event_name") {
        Some(Value::String(name)) if SUPPORTED_EVENTS.contains(&name.as_str()) => name.clone(),
        Some(Value::String(found)) => {
            return Err(PayloadError::UnsupportedEvent {
                found: found.clone(),
            });
        }
        Some(_) => {
            return Err(PayloadError::WrongType {
                field: "hook_event_name",
            });
        }
        None => return Err(PayloadError::MissingEvent),
    };
    let workspace = match object.get("cwd") {
        Some(Value::String(cwd)) if !cwd.is_empty() => PathBuf::from(cwd),
        Some(Value::String(_)) | None => return Err(PayloadError::MissingWorkspace),
        Some(_) => return Err(PayloadError::WrongType { field: "cwd" }),
    };
    let stop_hook_active = match object.get("stop_hook_active") {
        Some(Value::Bool(active)) => *active,
        Some(_) => {
            return Err(PayloadError::WrongType {
                field: "stop_hook_active",
            });
        }
        None => false,
    };
    Ok(HookEvent {
        name,
        workspace,
        stop_hook_active,
    })
}

/// The policy class of one supported event: the row of the adapter policy
/// table of SPEC 12.2 that governs the translation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Boundary {
    /// `UserPromptSubmit`: the last chance to save outstanding content
    /// before a new prompt begins. The one boundary with a supported
    /// blocking response.
    BeforePrompt,
    /// `Stop`: a completed pass. The agent must be allowed to stop.
    Stop,
    /// `StopFailure`, `SessionStart`, and `SessionEnd`: a best-effort
    /// boundary whose delivery failure leaves the next status or startup
    /// inspection authoritative.
    Session,
}

/// The boundary of one validated event name.
fn boundary_of(name: &str) -> Boundary {
    match name {
        "UserPromptSubmit" => Boundary::BeforePrompt,
        "Stop" => Boundary::Stop,
        _ => Boundary::Session,
    }
}

/// The stored capture policy as one boundary sees it (SPEC 12.2).
enum Policy {
    /// Margin is absent here, or the stored policy is off: preservation
    /// stays manual and the adapter is silent.
    Manual,
    /// The stored policy enables checkpoint capture at this boundary.
    Enabled,
    /// The settings could not be read, so the adapter cannot know whether
    /// preservation was promised.
    Unknown(OperationError),
}

/// Reads the one shared settings operation (SPEC 8.2). A repository
/// Margin cannot even inspect is not a promise: there the adapter stays
/// silent and the manual workflow governs.
fn policy_of(operations: &Operations<'_>) -> Policy {
    let result = operations.read_settings();
    if let Some(error) = result.error {
        return match error.code {
            "NOT_INITIALIZED" | "UNSUPPORTED_REPOSITORY" => Policy::Manual,
            _ => Policy::Unknown(error),
        };
    }
    match result.data.map(|outcome| outcome.settings.auto_checkpoint) {
        Some(true) => Policy::Enabled,
        _ => Policy::Manual,
    }
}

/// The generic checkpoint reason of one boundary (SPEC 12.3). The words
/// come from the event class alone, so no prompt, transcript, message
/// body, or session identifier ever reaches a commit title.
fn reason_for(name: &str) -> String {
    match name {
        "UserPromptSubmit" => "Captured before the next prompt",
        "Stop" => "Completed writing pass",
        _ => "Captured at a session boundary",
    }
    .into()
}

/// One ordinary draft checkpoint through the shared operation (SPEC 9.2),
/// with the generic reason of the boundary. Returns the typed refusal
/// when the capture could not be made; `None` covers both a created
/// commit and the explicit no-op of unchanged content, because a
/// duplicate boundary must stay harmless (SPEC 12.3).
fn attempt_capture(operations: &Operations<'_>, event: &HookEvent) -> Option<OperationError> {
    let result = operations.create_checkpoint(&CheckpointRequest {
        scope: None,
        message: Some(reason_for(&event.name)),
        include_staged: false,
        expect_head: None,
        expect_index: None,
        expect_working: None,
        dry_run: false,
    });
    result.error
}

/// One bounded report of a capture that could not be made. It names the
/// typed refusal, states that nothing was approved, and points at the
/// inspection command. It never asks the agent to continue.
fn capture_failure_text(error: &OperationError) -> String {
    bounded_message(&format!(
        "Margin could not save the writing pass: {}: {}. Nothing was approved; run margin \
         status to inspect the state.",
        error.code, error.message
    ))
}

/// One bounded report of settings the adapter could not read, so it
/// cannot know whether preservation was promised at this boundary.
fn unreadable_settings_text(error: &OperationError) -> String {
    bounded_message(&format!(
        "Margin could not read its stored settings: {}: {}. Nothing was captured or approved; \
         run margin doctor to inspect the repository.",
        error.code, error.message
    ))
}

/// One supported upstream response (SPEC 12.2). Only these shapes exist,
/// so the adapter cannot print a Margin envelope or invent a field.
enum HookResponse {
    /// No protocol output and a success exit: the minimal response that
    /// lets the agent proceed or stop.
    Allow,
    /// The supported pre-start blocking response of `UserPromptSubmit`.
    Block { reason: String },
    /// A non-blocking `systemMessage` warning. The agent still stops.
    Notice { message: String },
}

/// What one hook invocation did, in the channels the upstream protocol
/// owns: one exit code, optional protocol bytes for standard output, and
/// one optional bounded diagnostic line for standard error.
pub struct HookOutcome {
    /// The upstream exit contract: 0 lets the agent proceed, 1 is one
    /// non-blocking adapter refusal. The adapter never uses the blocking
    /// exit 2 itself (SPEC 12.2).
    pub exit: u8,
    /// Exact protocol bytes for standard output; `None` prints nothing.
    pub stdout: Option<Vec<u8>>,
    /// One bounded diagnostic line for standard error; `None` prints
    /// nothing.
    pub stderr: Option<String>,
}

/// One typed non-blocking refusal: exit 1 and one bounded line on
/// standard error. Nothing was captured and nothing was approved
/// (SPEC 12.3).
pub fn refused(reason: &str) -> HookOutcome {
    HookOutcome {
        exit: 1,
        stdout: None,
        stderr: Some(format!("margin hook: {}\n", bounded_message(reason))),
    }
}

/// Renders one supported response (SPEC 12.2). The exit stays 0, because
/// upstream applies JSON from a successful hook and the adapter never
/// uses the blocking exit 2.
fn respond(response: HookResponse) -> HookOutcome {
    let stdout = match response {
        HookResponse::Allow => None,
        HookResponse::Block { reason } => {
            Some(render(&json!({ "decision": "block", "reason": reason })))
        }
        HookResponse::Notice { message } => Some(render(&json!({ "systemMessage": message }))),
    };
    HookOutcome {
        exit: 0,
        stdout,
        stderr: None,
    }
}

/// Serializes one response object to exact protocol bytes.
fn render(value: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(value).expect("the response always serializes");
    bytes.push(b'\n');
    bytes
}

/// One best-effort diagnostic for a boundary whose failure must not reach
/// the agent (SPEC 12.2): exit 0, no protocol output, one bounded line on
/// standard error for the debug log. The next status or startup
/// inspection stays authoritative.
fn best_effort_diagnostic(error: &OperationError) -> Option<String> {
    Some(format!(
        "margin hook: best-effort capture failed: {}: {}\n",
        error.code,
        bounded_message(&error.message)
    ))
}

/// Whether the workspace the event names is the workspace this Margin
/// process serves (SPEC 12.2): both paths resolve to the same directory.
/// A payload that names another directory never captures or approves
/// anything here.
fn names_this_workspace(event: &HookEvent, served: &Path) -> bool {
    match (fs::canonicalize(&event.workspace), fs::canonicalize(served)) {
        (Ok(named), Ok(actual)) => named == actual,
        _ => false,
    }
}

/// The adapter policy table of SPEC 12.2: one row per event class, one
/// shared checkpoint operation, and only supported responses. A missing
/// or interrupted event never reaches this table, and no row grants
/// approval.
fn translate(operations: &Operations<'_>, event: &HookEvent) -> HookOutcome {
    let boundary = boundary_of(&event.name);
    match policy_of(operations) {
        // Preservation stays manual: the adapter is silent, so it never
        // claims preservation it did not provide.
        Policy::Manual => respond(HookResponse::Allow),
        Policy::Unknown(error) => match boundary {
            // The one boundary that may block: Margin cannot confirm its
            // capture promise, so it refuses to let the prompt proceed.
            Boundary::BeforePrompt => respond(HookResponse::Block {
                reason: unreadable_settings_text(&error),
            }),
            Boundary::Stop => respond(HookResponse::Notice {
                message: unreadable_settings_text(&error),
            }),
            Boundary::Session => HookOutcome {
                exit: 0,
                stdout: None,
                stderr: best_effort_diagnostic(&error),
            },
        },
        Policy::Enabled => match (boundary, attempt_capture(operations, event)) {
            // A capture, or the explicit no-op of a duplicate boundary:
            // both let the agent proceed, and neither approves anything.
            (_, None) => respond(HookResponse::Allow),
            // A refused capture before a prompt is the one supported
            // blocking response (SPEC 12.2): Margin does not claim
            // preservation it could not provide.
            (Boundary::BeforePrompt, Some(error)) => respond(HookResponse::Block {
                reason: capture_failure_text(&error),
            }),
            // A refused capture at a Stop is reported without blocking:
            // no decision, no continuation context, and never exit 2, so
            // the agent stops and no continuation loop can form.
            (Boundary::Stop, Some(error)) => respond(HookResponse::Notice {
                message: capture_failure_text(&error),
            }),
            // A refused capture at a session boundary is best effort.
            (Boundary::Session, Some(error)) => HookOutcome {
                exit: 0,
                stdout: None,
                stderr: best_effort_diagnostic(&error),
            },
        },
    }
}

/// Answers one bounded payload. The call validates the event, checks the
/// workspace it names, applies the stored policy, and returns only
/// supported upstream responses (SPEC 12.2).
pub fn run(operations: &Operations<'_>, workspace_root: &Path, bytes: &[u8]) -> HookOutcome {
    let event = match parse_payload(bytes) {
        Ok(event) => event,
        Err(error) => return refused(&error.reason()),
    };
    if !names_this_workspace(&event, workspace_root) {
        return refused(&format!(
            "the event names the workspace {}, but this adapter serves {}; nothing was captured \
             or approved",
            event.workspace.display(),
            workspace_root.display(),
        ));
    }
    let feedback = (event.name == "UserPromptSubmit").then(|| crate::notes::feedback(operations));
    let mut outcome = translate(operations, &event);
    let mut response = outcome
        .stdout
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok())
        .unwrap_or_else(|| json!({}));
    let blocked = response.get("decision").and_then(Value::as_str) == Some("block");
    if !blocked && outcome.exit == 0 {
        match feedback {
            Some(Ok(Some(context))) => {
                response["hookSpecificOutput"] = json!({
                    "hookEventName": "UserPromptSubmit", "additionalContext": context,
                });
                outcome.stdout = Some(render(&response));
            }
            Some(Err(error)) => {
                outcome.stderr = Some(format!(
                    "margin hook: feedback unavailable: {}\n",
                    bounded_message(&error.message)
                ))
            }
            _ => {}
        }
    }
    outcome
}

// ---------------------------------------------------------------------------
// Local configuration editing (SPEC 12.4)
// ---------------------------------------------------------------------------

/// The project-local Claude Code settings file Margin edits (SPEC 12.4).
/// The local file is the one settings surface that belongs to this
/// checkout alone, so an installation changes nothing a teammate, the
/// agent's own global configuration, or one shared project file owns.
pub const SETTINGS_RELATIVE_PATH: &str = ".claude/settings.local.json";

/// The exact command every installed entry runs (SPEC 12.4): the adapter
/// subcommand by name, so one entry survives replacement of the installed
/// executable. One hook is one Margin entry exactly when its command is
/// this string, which is the one fact removal uses to identify what it
/// may delete.
pub const HOOK_COMMAND: &str = "margin hook claude";

/// Which configuration action one invocation performs (SPEC 12.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupAction {
    /// Add the adapter entries that are missing.
    Install,
    /// Delete the adapter entries Margin can identify.
    Remove,
}

impl SetupAction {
    /// The envelope word of this action.
    pub fn as_str(self) -> &'static str {
        match self {
            SetupAction::Install => "install",
            SetupAction::Remove => "remove",
        }
    }
}

/// One planned entry change (SPEC 12.4): the supported event whose adapter
/// entry is added or deleted. One row per event, never one row per file,
/// so the report names exactly which boundaries change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedEntry {
    /// The upstream event the entry belongs to.
    pub event: &'static str,
    /// Whether the action deletes the entry instead of adding it.
    pub removed: bool,
}

/// What one configuration action planned and did (SPEC 12.4): the target
/// file, the fingerprint the plan was computed against, the exact planned
/// configuration, and the explicit no-op state of a repeated action.
#[derive(Clone, Debug)]
pub struct SetupOutcome {
    /// The action this result reports.
    pub action: SetupAction,
    /// Whether the invocation reported the plan without writing.
    pub dry_run: bool,
    /// The absolute target file.
    pub path: PathBuf,
    /// Whether the target existed when the plan was computed.
    pub target_existed: bool,
    /// The fingerprint of the bytes the plan was computed against; `None`
    /// when the target did not exist.
    pub fingerprint: Option<String>,
    /// The entry changes planned, in the contract order of the adapter.
    pub changes: Vec<PlannedEntry>,
    /// The exact configuration the action wrote, or would have written.
    pub planned_config: Value,
    /// Whether the action changed nothing, because every entry already sat
    /// in its target state.
    pub no_op: bool,
    /// Exact instruction-file plans, owned only when their template is recognized.
    pub instructions: Vec<super::instructions::Patch>,
}

/// The result type of one configuration action (SPEC 12.4).
pub type SetupResult = OperationOutcome<SetupOutcome>;

/// Installs the adapter's hook entries in the project-local Claude Code
/// configuration (SPEC 12.4).
///
/// The plan adds one identifiable entry per supported event and leaves
/// every unrelated key, every unrelated event, and every user hook
/// exactly where it was. A dry run reports the plan and writes nothing; a
/// file that does not exist yet is created only by this explicit action.
/// A repeat that finds every entry already installed is the documented
/// no-op.
pub fn install(operations: &Operations<'_>, dry_run: bool) -> SetupResult {
    setup(operations, SetupAction::Install, dry_run)
}

/// Removes the adapter's hook entries from the project-local Claude Code
/// configuration (SPEC 12.4).
///
/// Only entries Margin can identify are deleted; the hooks of a group
/// that also held user hooks survive, and no old whole configuration is
/// restored over newer edits. A missing file and a configuration without
/// Margin entries are both the documented no-op, and nothing is created.
pub fn remove(operations: &Operations<'_>, dry_run: bool) -> SetupResult {
    setup(operations, SetupAction::Remove, dry_run)
}

/// One configuration action (SPEC 12.4): plan against the exact bytes on
/// disk, report the plan for a dry run or an explicit no-op, and
/// otherwise reread the target, prove its fingerprint, and write through
/// one temporary file.
fn setup(operations: &Operations<'_>, action: SetupAction, dry_run: bool) -> SetupResult {
    let root = match project_root(operations) {
        Ok(root) => root,
        Err(error) => return OperationOutcome::failure(error, Effects::unchanged()),
    };
    let _lock = if dry_run {
        None
    } else {
        match operations.acquire_mutation_lock() {
            Ok(lock) => Some(lock),
            Err(error) => return OperationOutcome::failure(error, Effects::unchanged()),
        }
    };
    let path = root.join(SETTINGS_RELATIVE_PATH);
    let target = match read_target(&path) {
        Ok(target) => target,
        Err(error) => return OperationOutcome::failure(error, Effects::unchanged()),
    };
    let expected_fingerprint = target.bytes.as_deref().map(fingerprint);
    let planned = match action {
        SetupAction::Install => plan_install(&path, target.value),
        SetupAction::Remove => plan_removal(&path, target.value),
    };
    let (planned_config, changes) = match planned {
        Ok(plan) => plan,
        Err(error) => return OperationOutcome::failure(error, Effects::unchanged()),
    };
    let instructions = match super::instructions::plan(&root, action) {
        Ok(patches) => patches,
        Err(error) => return OperationOutcome::failure(error, Effects::unchanged()),
    };
    let outcome = SetupOutcome {
        action,
        dry_run,
        path: path.clone(),
        target_existed: target.bytes.is_some(),
        fingerprint: expected_fingerprint.clone(),
        changes: changes.clone(),
        planned_config: planned_config.clone(),
        no_op: changes.is_empty() && instructions.iter().all(|p| !p.changed),
        instructions: instructions.clone(),
    };
    if dry_run || outcome.no_op {
        // A dry run changes no configuration, and neither does the
        // explicit no-op of a repeated action (SPEC 12.4).
        return OperationOutcome::success(outcome, Effects::unchanged());
    }
    let content = serialize_target(&planned_config);
    let mut published = Vec::new();
    let publication = (|| {
        // Preflight every fingerprint before any intended write.
        if read_current(&path)?.as_deref().map(fingerprint).as_deref()
            != expected_fingerprint.as_deref()
        {
            return Err(stale_configuration(&path, action));
        }
        for patch in &instructions {
            super::instructions::verify(patch)?;
        }
        if !changes.is_empty() {
            apply(&path, expected_fingerprint.as_deref(), &content, action)?;
            published.push(path.clone());
        }
        for patch in instructions.iter().filter(|p| p.changed) {
            super::instructions::verify(patch)?;
            write_target(&patch.path, &patch.planned_text)?;
            published.push(patch.path.clone());
        }
        Ok(())
    })();
    match publication {
        Ok(()) => {
            let mut effects = Effects::unchanged();
            effects.observe_integration(ObservedEffect::Changed);
            OperationOutcome::success(outcome, effects)
        }
        Err(error) => {
            // A failed write may have left half a configuration behind,
            // so the integration effect stays unknown, never unchanged
            // (SPEC 7.1).
            let mut effects = Effects::unchanged();
            if !published.is_empty() || error.code == "IO_FAILED" {
                effects.observe_integration(ObservedEffect::Unknown);
            }
            let error = if published.is_empty() {
                error
            } else {
                OperationError::new(
                    "PARTIAL_MUTATION",
                    format!(
                        "integration setup stopped after publishing {} file(s): {}",
                        published.len(),
                        error.message
                    ),
                )
            };
            OperationOutcome::failure(error, effects)
        }
    }
}

/// Applies one planned configuration (SPEC 12.4): reread the target,
/// compare the fingerprint the plan was computed against, and write only
/// through one temporary file. One file that appeared, disappeared, or
/// changed since the plan is one stale refusal, because writing over an
/// edit this process never saw would delete configuration it does not
/// own.
fn apply(
    path: &Path,
    expected: Option<&str>,
    content: &str,
    action: SetupAction,
) -> Result<(), OperationError> {
    let current = read_current(path)?;
    if current.as_deref().map(fingerprint).as_deref() != expected {
        return Err(stale_configuration(path, action));
    }
    write_target(path, content)
}

/// The working-tree root the target configuration belongs to (SPEC 12.4):
/// the one project directory the adapter serves. Outside any repository
/// there is no project to configure, and a bare repository has no
/// project directory at all.
fn project_root(operations: &Operations<'_>) -> Result<PathBuf, OperationError> {
    match operations.git().toplevel() {
        Ok(root) => Ok(root),
        Err(GitError::Exit { .. }) => Err(OperationError::new(
            "NOT_INITIALIZED",
            "this directory lies inside no Git repository, so there is no project the hook \
             entries could serve",
        )
        .recovery(&["margin", "init", "--json"])),
        Err(GitError::MalformedOutput { .. }) => Err(OperationError::new(
            "UNSUPPORTED_REPOSITORY",
            "a bare repository has no working tree, so there is no project the hook entries \
             could serve",
        )),
        Err(error) => Err(OperationError::from(error)),
    }
}

/// The target configuration as one plan's input: the exact bytes on disk
/// and the object they parse to, so the write can prove the file never
/// moved under the plan (SPEC 12.4).
struct TargetConfig {
    /// The bytes that were read; `None` when no file existed.
    bytes: Option<Vec<u8>>,
    /// The parsed object; one empty object when no file existed.
    value: Value,
}

/// Reads the target configuration (SPEC 12.4). A missing file is one
/// empty configuration, never a failure: only the explicit install
/// action creates the file, and removal over a missing file is the
/// documented no-op.
fn read_target(path: &Path) -> Result<TargetConfig, OperationError> {
    match fs::read(path) {
        Ok(bytes) => Ok(TargetConfig {
            value: parse_target(path, &bytes)?,
            bytes: Some(bytes),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(TargetConfig {
            bytes: None,
            value: json!({}),
        }),
        Err(error) => Err(read_failure(
            path,
            &format!("the file cannot be read: {error}"),
        )),
    }
}

/// Reads the bytes that are on disk now, or `None` when the target is
/// absent. The apply phase rereads through this one function, so the
/// fingerprint comparison observes the file and not a cached copy.
fn read_current(path: &Path) -> Result<Option<Vec<u8>>, OperationError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(read_failure(
            path,
            &format!("the file cannot be read: {error}"),
        )),
    }
}

/// One typed read failure of the target configuration.
fn read_failure(path: &Path, detail: &str) -> OperationError {
    OperationError::new(
        "IO_FAILED",
        bounded_message(&format!(
            "the Claude Code configuration {} is unreadable: {detail}",
            path.display(),
        )),
    )
}

/// Parses the target configuration (SPEC 12.4): one JSON object. Anything
/// else is one typed refusal that writes nothing, because overwriting a
/// file this build cannot understand would delete configuration it never
/// owned.
fn parse_target(path: &Path, bytes: &[u8]) -> Result<Value, OperationError> {
    let unusable = |detail: String| {
        OperationError::new(
            "INVALID_SETTINGS",
            bounded_message(&format!(
                "the Claude Code configuration {} is unusable: {detail}",
                path.display(),
            )),
        )
    };
    let text =
        str::from_utf8(bytes).map_err(|error| unusable(format!("it is not UTF-8: {error}")))?;
    let value: Value = serde_json::from_str(text)
        .map_err(|error| unusable(format!("it is not one JSON document: {error}")))?;
    if !value.is_object() {
        return Err(unusable("it is not one JSON object".into()));
    }
    Ok(value)
}

/// Validates the shape of every supported event the plan must read
/// (SPEC 12.4): the `hooks` key, when present, is one object, and every
/// supported event under it is one array of groups, each one object whose
/// `hooks`, when present, is one array. One event Margin cannot understand
/// is one typed refusal, so a write never destroys entries it misread.
/// Events outside the supported contract are never read and therefore
/// never judged.
fn validate_supported_events(path: &Path, value: &Value) -> Result<(), OperationError> {
    let unusable = |detail: &str| {
        OperationError::new(
            "INVALID_SETTINGS",
            bounded_message(&format!(
                "the Claude Code configuration {} is unusable: {detail}",
                path.display(),
            )),
        )
    };
    match value.get("hooks") {
        None | Some(Value::Null) => return Ok(()),
        Some(hooks) if hooks.is_object() => {}
        Some(_) => return Err(unusable("its hooks are not one object")),
    }
    for &event in SUPPORTED_EVENTS {
        let Some(entries) = value.get("hooks").and_then(|hooks| hooks.get(event)) else {
            continue;
        };
        let unusable = |detail: &str| event_unusable(path, event, detail);
        let groups = entries
            .as_array()
            .ok_or_else(|| unusable("its entries are not one array"))?;
        for group in groups {
            if !group.is_object() {
                return Err(unusable("one of its entries is not one object"));
            }
            if let Some(hooks) = group.get("hooks")
                && !hooks.is_array()
            {
                return Err(unusable("one entry holds hooks that are not one array"));
            }
        }
    }
    Ok(())
}

/// One typed refusal about the shape of one supported event.
fn event_unusable(path: &Path, event: &str, detail: &str) -> OperationError {
    OperationError::new(
        "INVALID_SETTINGS",
        bounded_message(&format!(
            "the Claude Code configuration {} holds one '{event}' entry Margin cannot read: \
             {detail}",
            path.display(),
        )),
    )
}

/// Whether one hook object is an adapter entry Margin installed
/// (SPEC 12.4): its command is exactly [`HOOK_COMMAND`]. Position,
/// ordering, timeouts, and neighboring user hooks never identify
/// anything.
fn is_margin_hook(hook: &Value) -> bool {
    hook.get("command").and_then(Value::as_str) == Some(HOOK_COMMAND)
}

/// Whether one supported event already holds an identifiable adapter
/// entry.
fn has_margin_entry(value: &Value, event: &str) -> bool {
    value
        .get("hooks")
        .and_then(|hooks| hooks.get(event))
        .and_then(Value::as_array)
        .is_some_and(|groups| {
            groups.iter().any(|group| {
                group
                    .get("hooks")
                    .and_then(Value::as_array)
                    .is_some_and(|hooks| hooks.iter().any(is_margin_hook))
            })
        })
}

/// Plans one installation (SPEC 12.4): one adapter entry for every
/// supported event that holds none. Everything the file already carries
/// stays exactly where it was; only missing adapter entries are appended.
fn plan_install(
    path: &Path,
    mut value: Value,
) -> Result<(Value, Vec<PlannedEntry>), OperationError> {
    // Every shape is judged before the first mutation, so one unusable
    // event changes nothing (SPEC 12.4).
    validate_supported_events(path, &value)?;
    let mut changes = Vec::new();
    for &event in SUPPORTED_EVENTS {
        if has_margin_entry(&value, event) {
            continue;
        }
        append_entry(&mut value, event);
        changes.push(PlannedEntry {
            event,
            removed: false,
        });
    }
    Ok((value, changes))
}

/// Appends one adapter entry to one supported event, creating the `hooks`
/// object and the event array only when they do not exist.
fn append_entry(value: &mut Value, event: &str) {
    if !value.get("hooks").is_some_and(Value::is_object) {
        value["hooks"] = json!({});
    }
    let hooks = value.get_mut("hooks").expect("the hooks object exists");
    if !hooks.get(event).is_some_and(Value::is_array) {
        hooks[event] = json!([]);
    }
    let groups = hooks
        .get_mut(event)
        .and_then(Value::as_array_mut)
        .expect("the event holds one array");
    groups.push(json!({ "hooks": [{ "type": "command", "command": HOOK_COMMAND }] }));
}

/// Plans one removal (SPEC 12.4): delete every identifiable adapter entry,
/// keep the user hooks of a group that also held one, and drop an event
/// array — and then the `hooks` object — only once nothing remains in it.
/// No old whole configuration is restored over newer edits.
fn plan_removal(
    path: &Path,
    mut value: Value,
) -> Result<(Value, Vec<PlannedEntry>), OperationError> {
    validate_supported_events(path, &value)?;
    let mut changes = Vec::new();
    for &event in SUPPORTED_EVENTS {
        let Some(groups) = value
            .get_mut("hooks")
            .and_then(|hooks| hooks.get_mut(event))
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        let mut removed_here = false;
        groups.retain_mut(|group| {
            let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            let before = hooks.len();
            hooks.retain(|hook| !is_margin_hook(hook));
            removed_here |= hooks.len() < before;
            // A group that held only the adapter entry goes with it; a
            // group with user hooks keeps them (SPEC 12.4).
            !hooks.is_empty()
        });
        if removed_here {
            changes.push(PlannedEntry {
                event,
                removed: true,
            });
        }
        let empty = value
            .get("hooks")
            .and_then(|hooks| hooks.get(event))
            .and_then(Value::as_array)
            .is_some_and(|groups| groups.is_empty());
        if empty {
            value
                .get_mut("hooks")
                .and_then(|hooks| hooks.as_object_mut())
                .expect("the hooks object exists")
                .remove(event);
        }
    }
    if value
        .get("hooks")
        .and_then(Value::as_object)
        .is_some_and(|hooks| hooks.is_empty())
    {
        value
            .as_object_mut()
            .expect("the target is one object")
            .remove("hooks");
    }
    Ok((value, changes))
}

/// Serializes one planned configuration to the exact bytes the file will
/// hold: one pretty-printed JSON document with one trailing newline. The
/// keys print in one sorted order, which is one fixed form of the same
/// content: every key and every hook survives, and the dry run shows this
/// exact form before any of it is written.
fn serialize_target(value: &Value) -> String {
    let mut text =
        serde_json::to_string_pretty(value).expect("the configuration always serializes");
    text.push('\n');
    text
}

/// The fingerprint of one configuration file (SPEC 12.4): FNV-1a over the
/// exact bytes, printed as sixteen hexadecimal digits. The algorithm
/// lives in this code alone, so one invocation can print the value
/// another one compares, and any concurrent edit — even one that only
/// reformats — changes it.
fn fingerprint(bytes: &[u8]) -> String {
    /// The FNV-1a offset basis of 64 bits.
    const BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    /// The FNV-1a prime of 64 bits.
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:016x}")
}

/// Writes one planned configuration (SPEC 12.4): one temporary file inside
/// the target directory, then one rename over the target, so an
/// interrupted write cannot leave half a configuration behind. A target
/// that already exists keeps its permission bits, because the replacement
/// is this build's write of the user's file, not a new file. The
/// directory and the temporary file exist only inside this explicit
/// action; no other path is ever created.
fn write_target(path: &Path, content: &str) -> Result<(), OperationError> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    if let Err(error) = fs::create_dir_all(directory) {
        return Err(write_failure(
            path,
            &format!("its directory cannot be created: {error}"),
            None,
        ));
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary = directory.join(format!(".{name}.margin-{}", std::process::id()));
    let failure = |detail: String| write_failure(path, &detail, Some(&temporary));
    if let Err(error) = fs::write(&temporary, content) {
        return Err(failure(format!(
            "the planned file cannot be written: {error}"
        )));
    }
    if let Ok(metadata) = fs::metadata(path)
        && let Err(error) = fs::set_permissions(&temporary, metadata.permissions())
    {
        return Err(failure(format!(
            "the target's permission bits cannot be kept: {error}"
        )));
    }
    if let Err(error) = fs::rename(&temporary, path) {
        return Err(failure(format!(
            "the planned file cannot replace the target: {error}"
        )));
    }
    Ok(())
}

/// One typed write failure of the target configuration. `temporary` names
/// the helper file this write leaves behind when it can, so a failed
/// write never pretends it cleaned up after itself.
fn write_failure(path: &Path, detail: &str, temporary: Option<&Path>) -> OperationError {
    if let Some(temporary) = temporary {
        let _ = fs::remove_file(temporary);
    }
    OperationError::new(
        "IO_FAILED",
        bounded_message(&format!(
            "the Claude Code configuration {} cannot be written: {detail}",
            path.display(),
        )),
    )
}

/// The typed refusal of one configuration that moved under its plan
/// (SPEC 12.4). Nothing was written; an identical retry replans against
/// the bytes that are there now, so the refusal is retryable.
fn stale_configuration(path: &Path, action: SetupAction) -> OperationError {
    OperationError::new(
        "STALE_STATE",
        bounded_message(&format!(
            "the Claude Code configuration {} changed after the plan was computed; nothing was \
             written",
            path.display(),
        )),
    )
    .retryable()
    .recovery(&["margin", "hook", action.as_str(), "claude"])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One valid payload with every pinned field in place.
    fn stop_payload() -> Vec<u8> {
        br#"{"hook_event_name":"Stop","cwd":"/tmp","stop_hook_active":false}"#.to_vec()
    }

    #[test]
    fn a_valid_payload_pins_the_supported_contract() {
        let event = parse_payload(&stop_payload()).expect("the payload is valid");
        assert_eq!(event.name, "Stop");
        assert_eq!(event.workspace, PathBuf::from("/tmp"));
        assert!(!event.stop_hook_active);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let payload = br#"{"hook_event_name":"SessionStart","cwd":"/tmp","source":"startup",
            "session_id":"x","prompt":"y","future_field":[1,2,3]}"#;
        let event = parse_payload(payload).expect("unknown fields never fail the parse");
        assert_eq!(event.name, "SessionStart");
    }

    #[test]
    fn an_empty_payload_is_one_missing_event() {
        assert_eq!(parse_payload(b""), Err(PayloadError::Empty));
    }

    #[test]
    fn an_oversized_payload_is_refused_before_any_parse() {
        let mut payload = br#"{"pad":""#.to_vec();
        payload.resize(MAX_PAYLOAD_BYTES + 1, b'a');
        assert_eq!(
            parse_payload(&payload),
            Err(PayloadError::TooLarge {
                bound: MAX_PAYLOAD_BYTES
            }),
        );
    }

    #[test]
    fn a_non_object_payload_is_malformed() {
        assert!(matches!(
            parse_payload(b"[1,2,3]"),
            Err(PayloadError::Malformed { .. }),
        ));
        assert!(matches!(
            parse_payload(b"{\"hook_event_name\":\"Stop\""),
            Err(PayloadError::Malformed { .. }),
        ));
    }

    #[test]
    fn an_unnamed_or_unsupported_event_is_refused() {
        assert_eq!(
            parse_payload(br#"{"cwd":"/tmp"}"#),
            Err(PayloadError::MissingEvent),
        );
        assert_eq!(
            parse_payload(br#"{"hook_event_name":"PreToolUse","cwd":"/tmp"}"#),
            Err(PayloadError::UnsupportedEvent {
                found: "PreToolUse".into()
            }),
        );
    }

    #[test]
    fn pinned_fields_must_hold_the_pinned_types() {
        assert_eq!(
            parse_payload(br#"{"hook_event_name":7,"cwd":"/tmp"}"#),
            Err(PayloadError::WrongType {
                field: "hook_event_name"
            }),
        );
        assert_eq!(
            parse_payload(br#"{"hook_event_name":"Stop","cwd":7}"#),
            Err(PayloadError::WrongType { field: "cwd" }),
        );
        assert_eq!(
            parse_payload(br#"{"hook_event_name":"Stop","cwd":"/tmp","stop_hook_active":"yes"}"#),
            Err(PayloadError::WrongType {
                field: "stop_hook_active"
            }),
        );
        assert_eq!(
            parse_payload(br#"{"hook_event_name":"Stop"}"#),
            Err(PayloadError::MissingWorkspace),
        );
        assert_eq!(
            parse_payload(br#"{"hook_event_name":"Stop","cwd":""}"#),
            Err(PayloadError::MissingWorkspace),
        );
    }

    #[test]
    fn every_boundary_names_its_row_of_the_policy_table() {
        for name in SUPPORTED_EVENTS {
            let boundary = boundary_of(name);
            let is_session = matches!(boundary, Boundary::Session);
            assert_eq!(
                is_session,
                !matches!(name, &"UserPromptSubmit" | &"Stop"),
                "{name} maps to its policy row",
            );
        }
    }

    // -----------------------------------------------------------------------
    // Local configuration editing (SPEC 12.4)
    // -----------------------------------------------------------------------

    /// One scratch directory for one configuration test.
    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("margin-hook-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("the scratch directory is created");
        path
    }

    /// Parses one configuration document for one planning test.
    fn config_of(text: &str) -> Value {
        serde_json::from_str(text).expect("the fixture is one JSON object")
    }

    /// The adapter entry exactly as installation writes it.
    fn installed_entry() -> Value {
        json!({ "hooks": [{ "type": "command", "command": HOOK_COMMAND }] })
    }

    #[test]
    fn the_fingerprint_tracks_every_byte_of_the_file() {
        let first = fingerprint(b"{\"hooks\":{}}\n");
        assert_eq!(first.len(), 16, "the fingerprint prints as sixteen digits");
        assert_eq!(
            fingerprint(b"{\"hooks\":{}}\n"),
            first,
            "it is deterministic"
        );
        assert_ne!(
            fingerprint(b"{ \"hooks\": {} }\n"),
            first,
            "one reformat alone changes it",
        );
        assert_ne!(fingerprint(b""), first, "an empty file differs");
    }

    #[test]
    fn installation_appends_one_entry_per_uncovered_event() {
        let path = PathBuf::from("/repo/.claude/settings.local.json");
        let (planned, changes) = plan_install(&path, config_of(r#"{"permissions":{"allow":[]}}"#))
            .expect("one ordinary configuration plans");
        let hooks = planned["hooks"].as_object().expect("hooks is one object");
        for event in SUPPORTED_EVENTS {
            let groups = hooks[*event].as_array().expect("one event array");
            assert_eq!(groups.len(), 1, "{event} holds exactly one new group");
            assert_eq!(groups[0], installed_entry(), "{event} holds the entry");
        }
        assert_eq!(
            planned["permissions"],
            json!({"allow":[]}),
            "the unrelated key survives",
        );
        assert_eq!(changes.len(), SUPPORTED_EVENTS.len());
        assert!(changes.iter().all(|change| !change.removed));
    }

    #[test]
    fn planning_an_install_over_a_full_configuration_changes_nothing() {
        let path = PathBuf::from("/repo/.claude/settings.local.json");
        let mut already = config_of("{}");
        for event in SUPPORTED_EVENTS {
            already["hooks"][event] = json!([installed_entry()]);
        }
        let (planned, changes) = plan_install(&path, already.clone()).expect("the plan computes");
        assert_eq!(planned, already, "a repeated install rewrites nothing");
        assert!(changes.is_empty(), "the report is the explicit no-op");
    }

    #[test]
    fn removal_keeps_the_user_hooks_of_one_shared_group() {
        let path = PathBuf::from("/repo/.claude/settings.local.json");
        let mut shared = config_of("{}");
        shared["hooks"]["Stop"] = json!([{ "matcher": "", "hooks": [
            { "type": "command", "command": "notify.sh" },
            { "type": "command", "command": HOOK_COMMAND },
        ] }]);
        shared["hooks"]["UserPromptSubmit"] = json!([installed_entry()]);
        shared["hooks"]["PreToolUse"] =
            json!([{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "guard.sh" }] }]);
        let (planned, changes) = plan_removal(&path, shared).expect("the plan computes");
        let stop = planned["hooks"]["Stop"].as_array().expect("Stop remains");
        assert_eq!(
            stop[0]["hooks"],
            json!([{ "type": "command", "command": "notify.sh" }]),
            "the user hook of the shared group survives",
        );
        assert!(
            planned["hooks"].get("UserPromptSubmit").is_none(),
            "the event that held only the adapter entry goes",
        );
        assert_eq!(
            planned["hooks"]["PreToolUse"],
            json!([{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "guard.sh" }] }]),
            "one unsupported event is never touched",
        );
        assert_eq!(changes.len(), 2, "one row per changed event");
        assert!(changes.iter().all(|change| change.removed));
    }

    #[test]
    fn removal_drops_the_hooks_object_only_once_it_is_empty() {
        let path = PathBuf::from("/repo/.claude/settings.local.json");
        let mut only_margin = config_of(r#"{"outputStyle":"plain"}"#);
        only_margin["hooks"]["Stop"] = json!([installed_entry()]);
        let (planned, changes) = plan_removal(&path, only_margin).expect("the plan computes");
        assert!(
            planned.get("hooks").is_none(),
            "the empty hooks object goes with the last entry",
        );
        assert_eq!(planned["outputStyle"], json!("plain"), "other keys survive");
        assert_eq!(changes.len(), 1);
    }

    #[test]
    fn one_unreadable_configuration_is_refused_before_any_plan() {
        let path = PathBuf::from("/repo/.claude/settings.local.json");
        for bytes in [
            &b"{"[..],
            b"[1, 2, 3]",
            b"\"one string\"",
            b"{\"hooks\":\"no\"}",
            b"{\"hooks\":{\"Stop\":\"no\"}}",
            b"{\"hooks\":{\"Stop\":[\"no\"]}}",
            b"{\"hooks\":{\"Stop\":[{\"hooks\":\"no\"}]}}",
        ] {
            let error = match parse_target(&path, bytes) {
                Err(error) => error,
                Ok(value) => validate_supported_events(&path, &value).expect_err(&format!(
                    "the shape {:?} must be refused",
                    String::from_utf8_lossy(bytes),
                )),
            };
            assert_eq!(
                error.code,
                "INVALID_SETTINGS",
                "{:?}: {}",
                String::from_utf8_lossy(bytes),
                error.message,
            );
        }
    }

    #[test]
    fn one_write_refuses_when_the_target_moved_under_the_plan() {
        let scratch = scratch("stale");
        let path = scratch.join(SETTINGS_RELATIVE_PATH);
        fs::create_dir_all(scratch.join(".claude")).expect("the directory is created");
        let planned_bytes = br#"{"hooks":{}}"#;
        fs::write(&path, planned_bytes).expect("the fixture is written");
        let expected = fingerprint(planned_bytes);

        // A concurrent edit lands between the plan and the write.
        fs::write(&path, br#"{"hooks":{"Stop":[]}}"#).expect("the edit is written");
        let error = apply(&path, Some(&expected), "{}\n", SetupAction::Install)
            .expect_err("the moved target refuses the write");
        assert_eq!(error.code, "STALE_STATE", "{}", error.message);
        assert!(error.retryable, "the identical retry replans");
        assert_eq!(
            fs::read(&path).unwrap(),
            br#"{"hooks":{"Stop":[]}}"#.to_vec(),
            "the concurrent edit survives untouched",
        );

        // The plan computed against the bytes that are there now applies.
        apply(
            &path,
            Some(&fingerprint(br#"{"hooks":{"Stop":[]}}"#)),
            "{}\n",
            SetupAction::Install,
        )
        .expect("a current fingerprint applies");
        assert_eq!(fs::read(&path).unwrap(), b"{}\n".to_vec());

        // A target that disappeared since the plan refuses the same way.
        fs::remove_file(&path).expect("the target is removed");
        let error =
            apply(&path, Some(&expected), "{}\n", SetupAction::Remove).expect_err("refuses");
        assert_eq!(error.code, "STALE_STATE");
        let _ = fs::remove_dir_all(&scratch);
    }

    #[test]
    fn one_write_creates_the_file_only_inside_the_explicit_action() {
        let scratch = scratch("write");
        let path = scratch.join(SETTINGS_RELATIVE_PATH);
        apply(&path, None, "{}\n", SetupAction::Install).expect("the missing target is created");
        assert_eq!(fs::read(&path).unwrap(), b"{}\n".to_vec());
        let directory = scratch.join(".claude");
        let leftovers: Vec<_> = fs::read_dir(&directory)
            .expect("the directory exists")
            .map(|entry| {
                entry
                    .expect("one entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(leftovers, vec!["settings.local.json".to_string()]);
        let _ = fs::remove_dir_all(&scratch);
    }

    #[test]
    fn one_replacement_keeps_the_targets_permission_bits() {
        let scratch = scratch("mode");
        let path = scratch.join(SETTINGS_RELATIVE_PATH);
        fs::create_dir_all(scratch.join(".claude")).expect("the directory is created");
        fs::write(&path, "{}\n").expect("the fixture is written");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("the mode is set");
            apply(
                &path,
                Some(&fingerprint(b"{}\n")),
                "{\"hooks\":{}}\n",
                SetupAction::Install,
            )
            .expect("the write applies");
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the replacement keeps the mode");
        }
        let _ = fs::remove_dir_all(&scratch);
    }
}
