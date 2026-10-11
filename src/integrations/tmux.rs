//! Optional tmux pane and session composition (SPEC 12.6, task T050).
//!
//! tmux owns sessions, windows, panes, selection, and zoom. Margin only
//! composes the few documented commands that build one writing desk, and
//! reports exactly what it created: the agent argv, the Margin pane or
//! window, the session's own prefix key, and the attach and exit behavior.
//! The agent runs through tmux's direct-exec form, where every argument of
//! the explicit agent argv becomes one argv element of the pane process,
//! so no shell ever reads it and no document content is interpolated
//! (SPEC 12.6, 15.4). The module edits no tmux option or configuration
//! file, sends no keystrokes into any pane, and touches no session,
//! window, or pane it did not create: the agent stays the writer's
//! process, and quitting Margin leaves it running.

use std::collections::BTreeSet;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use crate::app::OperationError;
use crate::cli::bounded_message;

/// The terminal width from which two columns beside each other stay
/// readable (SPEC 11.2: at least 140 columns for a desk).
pub const DESK_COLUMNS: u16 = 140;

/// The largest number of agent arguments one desk accepts. The bound keeps
/// one runaway command line from flooding the tmux client's own argv.
pub const MAX_AGENT_ARGS: usize = 256;

/// The largest total size of the agent argv one desk accepts.
pub const MAX_AGENT_ARGV_BYTES: usize = 32 * 1024;

/// The window name of the agent in a dedicated desk session.
const AGENT_WINDOW: &str = "agent";

/// The window name of the Margin surface in a dedicated desk session.
const MARGIN_WINDOW: &str = "margin";

/// The window name of the agent inside a session the writer already owns
/// (SPEC 12.6: a clearly named window).
const DESK_WINDOW: &str = "margin-desk";

/// The window name of a narrow Margin surface inside an existing session.
const DESK_REVIEW_WINDOW: &str = "margin-review";

/// The share of the columns the Margin pane keeps in a side-by-side desk:
/// at the 140-column desk threshold this holds the reading measure of the
/// surface (SPEC 11.1) while the agent keeps a working column.
const MARGIN_SHARE: &str = "55%";

/// The arrangement one terminal width supports (SPEC 11.2, 12.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    /// Two panes beside each other, agent left and Margin right.
    SideBySide,
    /// One command per window, so a narrow desk stays readable instead of
    /// becoming two unreadable columns.
    Focused,
}

/// Chooses the arrangement of one desk width (SPEC 12.6).
fn layout_for(columns: u16) -> Layout {
    if columns >= DESK_COLUMNS {
        Layout::SideBySide
    } else {
        Layout::Focused
    }
}

/// Composes one optional tmux writing desk (SPEC 12.6) and returns the
/// plain-text report the writer reads.
///
/// The command line checks its terminal before calling here, so this
/// function may attach tmux on the standard streams. A missing tmux is one
/// typed refusal that explains the tab alternative; the core product
/// never depends on this path.
pub fn compose_desk(cwd: &Path, agent: &[String]) -> Result<String, OperationError> {
    validate_agent(agent)?;
    probe()?;
    let cwd: PathBuf = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let margin = margin_argv();
    if inside_tmux() {
        compose_inside(&cwd, agent, &margin)
    } else {
        compose_outside(&cwd, agent, &margin)
    }
}

/// Validates the explicit agent argv (SPEC 12.6): one executable first,
/// and both argument counts and bytes bounded. Nothing interprets the
/// argv, so the bounds exist only to keep one desk composition from
/// passing an unreasonable command line to the tmux client.
fn validate_agent(agent: &[String]) -> Result<(), OperationError> {
    if agent.first().is_none_or(|program| program.is_empty()) {
        return Err(OperationError::new(
            "INVALID_ARGUMENT",
            "the desk needs the agent executable as the first argument after --",
        ));
    }
    let bytes: usize = agent.iter().map(String::len).sum();
    if agent.len() > MAX_AGENT_ARGS || bytes > MAX_AGENT_ARGV_BYTES {
        return Err(OperationError::new(
            "LIMIT_EXCEEDED",
            format!(
                "the agent argv exceeds the desk bound of {MAX_AGENT_ARGS} arguments or \
                 {MAX_AGENT_ARGV_BYTES} bytes"
            ),
        ));
    }
    Ok(())
}

/// Says whether this process runs inside a tmux pane: tmux exports TMUX
/// only to the processes it starts (SPEC 12.6).
fn inside_tmux() -> bool {
    env::var_os("TMUX").is_some_and(|value| !value.is_empty())
}

/// The Margin argv of the desk's own pane: this very executable and its
/// interactive command, so the pane runs exactly the build the writer
/// launched.
fn margin_argv() -> Vec<String> {
    let program = env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "margin".to_string());
    vec![program, "tui".to_string()]
}

/// Probes the tmux client (SPEC 12.6): a missing executable is one typed
/// refusal with the tab guidance, because the desk is optional and the
/// core product stays available without it.
fn probe() -> Result<(), OperationError> {
    match Command::new("tmux").arg("-V").output() {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(failure("-V", &output.stderr)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(missing_tmux()),
        Err(error) => Err(failure("-V", error.to_string().as_bytes())),
    }
}

/// The typed refusal for a machine without tmux (SPEC 12.6): the writer
/// learns the two-tab arrangement, not just the missing tool.
fn missing_tmux() -> OperationError {
    OperationError::new(
        "OPERATION_UNAVAILABLE",
        "tmux was not found on PATH, so no writing desk can be composed; Margin stays useful \
         without it: run margin tui in one terminal tab and the agent in another",
    )
    .recovery(&["margin", "tui"])
}

/// One tmux execution failure: the subcommand and the first bounded line
/// of the client's own complaint (SPEC 7.6).
fn failure(command: &str, complaint: &[u8]) -> OperationError {
    OperationError::new(
        "TMUX_FAILED",
        format!(
            "tmux {command} failed: {}",
            bounded_message(&String::from_utf8_lossy(complaint))
        ),
    )
}

/// Builds one tmux command line from static words.
fn words(parts: &[&str]) -> Vec<OsString> {
    parts.iter().map(OsString::from).collect()
}

/// Appends the direct-exec separator and one explicit argv (SPEC 12.6):
/// every element stays one argv element of the pane process, and no shell
/// reads it. The desk contract this builds needs tmux 3.1 or newer: the
/// command-after-`--` form of `new-session`, `new-window`, and
/// `split-window`, and the percentage size of `MARGIN_SHARE`, both
/// arrived in that release. The CI tiers pin the release they exercise
/// in `ci/tmux.version`.
fn direct_exec(args: &mut Vec<OsString>, argv: &[String]) {
    args.push(OsString::from("--"));
    args.extend(argv.iter().map(OsString::from));
}

/// Runs one tmux control command and returns its standard output, trimmed.
/// Every failure carries the client's own bounded diagnosis.
fn run(args: &[OsString]) -> Result<String, OperationError> {
    let command = args
        .first()
        .map(|word| word.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tmux".to_string());
    let output = Command::new("tmux").args(args).output().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            missing_tmux()
        } else {
            failure(&command, error.to_string().as_bytes())
        }
    })?;
    if !output.status.success() {
        return Err(failure(&command, &output.stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Says whether one session exists; every failure means no.
fn session_exists(name: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", name])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Ends one session this desk created; a session that already ended is not
/// an error here.
fn kill_session(name: &str) {
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", name])
        .output();
}

/// Ends one window this desk created inside the writer's session; a
/// window that already closed is not an error here.
fn kill_window(window: &str) {
    let _ = Command::new("tmux")
        .args(["kill-window", "-t", window])
        .output();
}

/// The tmux sessions that exist. No server yet means none: the first desk
/// composition starts the server.
fn session_names() -> BTreeSet<String> {
    let output = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(String::from)
            .collect(),
        _ => BTreeSet::new(),
    }
}

/// Picks the session name of a new dedicated desk: `margin`, then
/// `margin-2`, and so on, so a desk never lands on the writer's own
/// session of the same name.
fn free_session_name() -> Result<String, OperationError> {
    let taken = session_names();
    first_free_name(&taken).ok_or_else(|| {
        OperationError::new(
            "TMUX_FAILED",
            "no free margin desk session name exists; close one of the 100 margin sessions first",
        )
    })
}

/// The first name no session holds: the pure walk behind
/// [`free_session_name`], testable without a tmux server.
fn first_free_name(taken: &BTreeSet<String>) -> Option<String> {
    (1..=100u32)
        .map(|index| match index {
            1 => "margin".to_string(),
            index => format!("margin-{index}"),
        })
        .find(|candidate| !taken.contains(candidate))
}

/// The prefix key of one session, exactly as tmux reports it (SPEC 12.6:
/// queried, never assumed). `-A` follows the inheritance a real client
/// obeys, so a prefix set only globally still reads here instead of
/// vanishing behind the session's own table. `None` also covers the
/// explicit `None` value of a session with the prefix disabled.
fn prefix_of(session: &str) -> Option<String> {
    let output = Command::new("tmux")
        .args(["show-options", "-A", "-t", session, "-v", "prefix"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    match String::from_utf8_lossy(&output.stdout).trim() {
        "" | "None" => None,
        prefix => Some(prefix.to_string()),
    }
}

/// Attaches the desk session and waits, with the standard streams passed
/// through untouched: the tmux client owns the terminal exactly as one
/// child editor does (SPEC 12.5).
fn attach(name: &str) -> Result<ExitStatus, OperationError> {
    Command::new("tmux")
        .args(["attach-session", "-t", name])
        .status()
        .map_err(|error| failure("attach-session", error.to_string().as_bytes()))
}

/// The honest failure for an agent that did not stay running: the desk
/// composition found its window gone, and nothing of the desk survives.
fn agent_did_not_run(program: &str) -> OperationError {
    OperationError::new(
        "TMUX_FAILED",
        format!(
            "the desk ended at once: the agent {program} did not stay running, so its pane \
             closed before the desk was complete; check the agent argv, because nothing was \
             written and nothing of the desk is left",
        ),
    )
}

/// Composes a dedicated desk session outside tmux (SPEC 12.6): one
/// detached session with an agent window, the Margin surface beside it or
/// in a window of its own, and one attach that ends on detach.
fn compose_outside(
    cwd: &Path,
    agent: &[String],
    margin: &[String],
) -> Result<String, OperationError> {
    let size = crossterm::terminal::size().ok();
    let (columns, rows) = size.unwrap_or((80, 24));
    let layout = layout_for(columns);
    let name = free_session_name()?;

    // The session starts detached with the terminal's own size, so the
    // arrangement matches what the first attach shows. The agent argv
    // follows the direct-exec separator verbatim.
    let mut session = words(&[
        "new-session",
        "-d",
        "-x",
        &columns.to_string(),
        "-y",
        &rows.to_string(),
        "-s",
        &name,
        "-n",
        AGENT_WINDOW,
        "-c",
        &cwd.display().to_string(),
    ]);
    direct_exec(&mut session, agent);
    run(&session)?;
    let built = build_desk_surfaces(&name, AGENT_WINDOW, layout, cwd, margin, &[])
        .or_else(|error| desk_or_agent_failure(&name, agent, error));

    let prefix = prefix_of(&name);
    match built {
        Ok(()) => {}
        Err(error) => {
            kill_session(&name);
            return Err(error);
        }
    }
    // An agent that exited before the desk finished leaves no session to
    // attach; the writer gets the honest reason, not an attach failure.
    if !session_exists(&name) {
        return Err(agent_did_not_run(&agent[0]));
    }
    let attached = attach(&name);
    let running = session_exists(&name);
    let mut summary = Summary::created_session(&name, layout, agent, margin, &prefix);
    match attached {
        Ok(status) if status.success() && running => summary.detached(&name),
        Ok(status) if running => summary.attach_ended(&name, &status),
        Ok(_) => summary.session_ended(),
        Err(error) => return Err(error),
    }
    Ok(summary.text())
}

/// Composes one desk window inside the session this terminal belongs to
/// (SPEC 12.6): one clearly named window, unrelated panes untouched, and
/// no attach, because the client is already here.
fn compose_inside(
    cwd: &Path,
    agent: &[String],
    margin: &[String],
) -> Result<String, OperationError> {
    let session = run(&words(&["display-message", "-p", "#{session_name}"]))?;
    if session.is_empty() {
        return Err(OperationError::new(
            "TMUX_FAILED",
            "this terminal reports no tmux session name; the desk cannot name its window",
        ));
    }

    // The new window carries the agent argv verbatim and reports its own
    // id, which addresses the window regardless of the names in the
    // writer's session.
    let mut created = words(&[
        "new-window",
        "-P",
        "-F",
        "#{window_id}",
        "-n",
        DESK_WINDOW,
        "-c",
        &cwd.display().to_string(),
    ]);
    direct_exec(&mut created, agent);
    let window = run(&created)?;
    let width = run(&words(&[
        "display-message",
        "-p",
        "-t",
        &window,
        "#{window_width}",
    ]))
    .ok()
    .and_then(|width| width.parse::<u16>().ok());
    let layout = width.map_or(Layout::Focused, layout_for);

    let built = build_desk_surfaces(
        &session,
        DESK_WINDOW,
        layout,
        cwd,
        margin,
        &[DESK_REVIEW_WINDOW],
    )
    .or_else(|error| desk_or_agent_failure(&session, agent, error));
    match built {
        Ok(()) => {}
        Err(error) => {
            kill_window(&window);
            return Err(error);
        }
    }
    if !window_exists(&window) {
        kill_window(&window);
        return Err(agent_did_not_run(&agent[0]));
    }
    // The window stays selected, so the writer lands on the desk; in the
    // focused arrangement the agent window is the desk's center.
    let _ = run(&words(&["select-window", "-t", &window]));
    let prefix = prefix_of(&session);
    Ok(Summary::joined_window(&session, layout, agent, margin, &prefix, DESK_WINDOW).text())
}

/// Says whether one window exists; every failure means no.
fn window_exists(window: &str) -> bool {
    Command::new("tmux")
        .args(["display-message", "-p", "-t", window, "#{window_id}"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Adds the Margin surface to the agent window (SPEC 12.6): one pane
/// beside the agent when two columns stay readable, or one window of its
/// own when they do not. `review_window` names the narrow window inside a
/// session the writer already owns; a dedicated session uses its own
/// shorter names.
fn build_desk_surfaces(
    session: &str,
    agent_window: &str,
    layout: Layout,
    cwd: &Path,
    margin: &[String],
    review_windows: &[&str],
) -> Result<(), OperationError> {
    let target = format!("{session}:{agent_window}");
    match layout {
        Layout::SideBySide => {
            let mut split = words(&[
                "split-window",
                "-h",
                "-l",
                MARGIN_SHARE,
                "-t",
                &target,
                "-c",
                &cwd.display().to_string(),
            ]);
            direct_exec(&mut split, margin);
            run(&split)?;
            // The writer's first words belong to the agent, so the agent
            // pane keeps the focus the split moved to the new pane.
            let _ = run(&words(&["select-pane", "-t", &format!("{target}.0")]));
        }
        Layout::Focused => {
            let name = review_windows.first().copied().unwrap_or(MARGIN_WINDOW);
            let mut review = words(&[
                "new-window",
                "-n",
                name,
                "-t",
                session,
                "-c",
                &cwd.display().to_string(),
            ]);
            direct_exec(&mut review, margin);
            run(&review)?;
            let _ = run(&words(&["select-window", "-t", &target]));
        }
    }
    Ok(())
}

/// Re-reads one composition failure so the writer never sees a pane
/// targeting error for an agent that simply did not stay running.
fn desk_or_agent_failure(
    session: &str,
    agent: &[String],
    error: OperationError,
) -> Result<(), OperationError> {
    if !session_exists(session) {
        return Err(agent_did_not_run(&agent[0]));
    }
    Err(error)
}

/// The report of one composed desk (SPEC 12.6): the processes and session
/// it created, the queried prefix, and the attach and exit behavior, as
/// one bounded block of plain text.
struct Summary {
    /// Every line of the report, in order.
    lines: Vec<String>,
}

impl Summary {
    /// The report of one dedicated session.
    fn created_session(
        name: &str,
        layout: Layout,
        agent: &[String],
        margin: &[String],
        prefix: &Option<String>,
    ) -> Summary {
        let mut lines = vec![format!("margin desk: session {name}")];
        match layout {
            Layout::SideBySide => lines.push(format!(
                "  window \"{AGENT_WINDOW}\": the agent pane runs {}; the margin pane beside it \
                 runs {}",
                show_argv(agent),
                show_argv(margin),
            )),
            Layout::Focused => {
                lines.push(format!(
                    "  window \"{AGENT_WINDOW}\": the agent runs {}",
                    show_argv(agent),
                ));
                lines.push(format!(
                    "  window \"{MARGIN_WINDOW}\": margin runs {} in a window of its own, so a \
                     narrow desk stays readable",
                    show_argv(margin),
                ));
            }
        }
        lines.push(prefix_line(prefix));
        Summary { lines }
    }

    /// The report of one window added to the session the writer owns.
    fn joined_window(
        session: &str,
        layout: Layout,
        agent: &[String],
        margin: &[String],
        prefix: &Option<String>,
        window: &str,
    ) -> Summary {
        let mut lines = vec![format!(
            "margin desk: window \"{window}\" in session {session}"
        )];
        match layout {
            Layout::SideBySide => lines.push(format!(
                "  the agent pane runs {}; the margin pane beside it runs {}",
                show_argv(agent),
                show_argv(margin),
            )),
            Layout::Focused => {
                lines.push(format!("  the agent pane runs {}", show_argv(agent)));
                lines.push(format!(
                    "  window \"{DESK_REVIEW_WINDOW}\": margin runs {} in a window of its own, \
                     so a narrow desk stays readable",
                    show_argv(margin),
                ));
            }
        }
        lines.push(prefix_line(prefix));
        lines.push(
            "  quitting margin leaves the agent running; close the desk windows with your own \
             tmux bindings"
                .to_string(),
        );
        Summary { lines }
    }

    /// The detach outcome of one dedicated session (SPEC 12.6: attach and
    /// exit behavior, reported honestly).
    fn detached(&mut self, name: &str) {
        self.lines
            .push("  detached; the session keeps running".to_string());
        self.lines.push(format!(
            "  reattach with tmux attach-session -t {name}; quitting margin leaves the agent \
             running, and the session ends when its last window closes"
        ));
    }

    /// An attach that ended abnormally while the session still runs.
    fn attach_ended(&mut self, name: &str, status: &ExitStatus) {
        self.lines.push(format!(
            "  the attach ended with {status}; the session keeps running"
        ));
        self.lines
            .push(format!("  reattach with tmux attach-session -t {name}"));
    }

    /// An attach after which no session is left.
    fn session_ended(&mut self) {
        self.lines
            .push("  the session has ended; no desk is left running".to_string());
    }

    /// The complete report.
    fn text(&self) -> String {
        let mut text = self.lines.join("\n");
        text.push('\n');
        text
    }
}

/// One argv as one line of text: control characters become U+FFFD, so an
/// argument can never carry a carriage return into the report.
fn show_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|argument| {
            argument
                .chars()
                .map(|character| {
                    if character.is_control() {
                        '\u{FFFD}'
                    } else {
                        character
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<String>>()
        .join(" ")
}

/// The prefix line of one report: the session's own key, queried.
fn prefix_line(prefix: &Option<String>) -> String {
    match prefix {
        Some(key) => format!("tmux prefix: {key} (queried from the session, never assumed)"),
        None => "tmux prefix: none is configured for this session".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_columns_start_at_the_desk_width() {
        assert_eq!(layout_for(DESK_COLUMNS), Layout::SideBySide);
        assert_eq!(layout_for(400), Layout::SideBySide);
        assert_eq!(layout_for(DESK_COLUMNS - 1), Layout::Focused);
        assert_eq!(layout_for(60), Layout::Focused);
    }

    #[test]
    fn the_agent_argv_is_explicit_and_bounded() {
        assert!(validate_agent(&["claude".to_string()]).is_ok());
        assert_eq!(
            validate_agent(&[]).unwrap_err().code,
            "INVALID_ARGUMENT",
            "the executable must be first"
        );
        assert_eq!(
            validate_agent(&["".to_string()]).unwrap_err().code,
            "INVALID_ARGUMENT",
            "the executable must not be empty"
        );
        let many: Vec<String> = (0..=MAX_AGENT_ARGS).map(|i| i.to_string()).collect();
        assert_eq!(validate_agent(&many).unwrap_err().code, "LIMIT_EXCEEDED");
        let huge = vec!["claude".to_string(), " ".repeat(MAX_AGENT_ARGV_BYTES)];
        assert_eq!(validate_agent(&huge).unwrap_err().code, "LIMIT_EXCEEDED");
    }

    #[test]
    fn the_session_name_skips_every_taken_name() {
        let mut taken = BTreeSet::new();
        assert_eq!(first_free_name(&taken), Some("margin".to_string()));
        taken.insert("margin".to_string());
        assert_eq!(first_free_name(&taken), Some("margin-2".to_string()));
        taken.insert("margin-2".to_string());
        assert_eq!(first_free_name(&taken), Some("margin-3".to_string()));
    }

    #[test]
    fn the_report_names_every_surface_and_behavior() {
        let agent = vec!["claude".to_string(), "--model".to_string()];
        let margin = vec!["/usr/bin/margin".to_string(), "tui".to_string()];
        let prefix = Some("C-b".to_string());
        let mut summary =
            Summary::created_session("margin", Layout::SideBySide, &agent, &margin, &prefix);
        summary.detached("margin");
        let text = summary.text();
        assert!(text.contains("margin desk: session margin"));
        assert!(text.contains("claude --model"));
        assert!(text.contains("/usr/bin/margin tui"));
        assert!(text.contains("tmux prefix: C-b (queried from the session, never assumed)"));
        assert!(text.contains("detached; the session keeps running"));
        assert!(text.contains("tmux attach-session -t margin"));
        assert!(text.contains("quitting margin leaves the agent running"));

        let mut ended =
            Summary::created_session("margin", Layout::SideBySide, &agent, &margin, &prefix);
        ended.session_ended();
        let ended_text = ended.text();
        assert!(ended_text.contains("the session has ended; no desk is left running"));
        assert!(
            !ended_text.contains("detached; the session keeps running"),
            "an ended session is not reported as a detach"
        );
        assert!(
            !ended_text.contains("tmux attach-session"),
            "an ended session offers no reattachment"
        );

        let narrow = Summary::created_session("margin-2", Layout::Focused, &agent, &margin, &None);
        assert!(narrow.text().contains("window \"margin\""));
        assert!(
            narrow
                .text()
                .contains("tmux prefix: none is configured for this session")
        );

        let joined = Summary::joined_window(
            "base",
            Layout::SideBySide,
            &agent,
            &margin,
            &Some("C-a".to_string()),
            DESK_WINDOW,
        );
        assert!(
            joined
                .text()
                .contains("window \"margin-desk\" in session base")
        );
        assert!(joined.text().contains("tmux prefix: C-a"));
    }

    #[test]
    fn control_characters_never_reach_the_report_lines() {
        let agent = vec!["claude".to_string(), "a\tb\u{1b}[31mc".to_string()];
        assert_eq!(show_argv(&agent), "claude a\u{FFFD}b\u{FFFD}[31mc");
        assert!(!show_argv(&agent).chars().any(char::is_control));
    }

    #[test]
    fn the_missing_tmux_refusal_explains_the_tab_alternative() {
        let error = missing_tmux();
        assert_eq!(error.code, "OPERATION_UNAVAILABLE");
        assert!(error.message.contains("tmux"));
        assert!(error.message.contains("margin tui"));
        assert_eq!(
            error.recovery_argv,
            vec!["margin".to_string(), "tui".to_string()]
        );
    }

    #[test]
    fn a_dead_agent_is_reported_honestly() {
        let error = agent_did_not_run("./missing-agent");
        assert_eq!(error.code, "TMUX_FAILED");
        assert!(error.message.contains("./missing-agent"));
        assert!(error.message.contains("did not stay running"));
        assert!(error.message.contains("nothing was written"));
    }
}
