//! Terminal lifecycle and the event loop (SPEC 11.5, 13.3).
//!
//! Crossterm owns terminal mode and input; Ratatui owns drawing. One RAII
//! guard enters raw mode, the alternate screen, and bracketed paste on
//! creation and leaves them on every exit path this process controls: a
//! normal quit, `Ctrl-C`, a handled error, and a panic, because Drop runs
//! during unwinding. The same guard suspends every mode around one child
//! editor and re-enters them when the editor returns, so the editor meets
//! a cooked terminal and the surface comes back whole. An uncatchable
//! kill cannot run cleanup, and this module does not claim otherwise.
//!
//! The loop draws only when a view changed and waits on input between
//! frames; there is no idle animation. The reduced-update mode draws only
//! when the shell's frame-visible state changed since the last drawn
//! frame (SPEC 11.4). The wait never outlives the next inspection tick,
//! and it shrinks while the refresh worker answers, so one blocking Git
//! read never stalls a key press and one answered inspection reaches the
//! frame well inside the P-REFRESH budget (SPEC 13.3, 17.4). The worker
//! runs the reads; this loop only sends generations and absorbs replies.

use std::io::{self, IsTerminal};
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::app::Operations;
use crate::tui::refresh::{INSPECTION_INTERVAL, RefreshWorker, WatchJob};
use crate::tui::render;
use crate::tui::shell::{ColorMode, Shell, TuiOptions};

/// How long the input wait shrinks to while the worker runs or holds a
/// waiting job, so an answered inspection reaches the next frame instead
/// of the tick after it.
const BUSY_WAIT: Duration = Duration::from_millis(100);

/// Why the surface cannot start.
#[derive(Debug)]
pub struct LaunchError {
    /// A bounded explanation for one stderr line.
    pub message: String,
}

impl LaunchError {
    /// One launch refusal with its bounded message.
    fn new(message: impl Into<String>) -> LaunchError {
        LaunchError {
            message: message.into(),
        }
    }
}

/// Says whether this process can draw and read a terminal at all.
///
/// A redirected stream means the interactive surface cannot run: it is an
/// unavailable operation, not a broken terminal, and the caller reports it
/// through the typed contract of the command line.
pub fn has_terminal() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// Says whether the environment lets `auto` decorate: `NO_COLOR`, set to
/// one non-empty value, and `TERM=dumb` are the two conventions that turn
/// decoration off (SPEC 11.4).
fn auto_color_allowed(no_color: Option<&std::ffi::OsStr>, term: Option<&str>) -> bool {
    let no_color_set = no_color.is_some_and(|value| !value.is_empty());
    !no_color_set && term != Some("dumb")
}

/// Resolves the `auto` color mode of one launch against the environment;
/// `always` and `never` pass through unchanged.
fn resolved_color(mode: ColorMode) -> ColorMode {
    if mode == ColorMode::Auto
        && !auto_color_allowed(
            std::env::var_os("NO_COLOR").as_deref(),
            std::env::var("TERM").ok().as_deref(),
        )
    {
        return ColorMode::Never;
    }
    mode
}

/// The mode the terminal is in, as the guard tracks it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Screen {
    /// Raw mode, the alternate screen, and bracketed paste are on.
    Interactive,
    /// Every mode is off: one child editor owns the terminal (SPEC 12.5).
    Suspended,
    /// The guard left, and nothing runs again.
    Left,
}

/// The RAII terminal guard of one interactive run (SPEC 11.5).
pub struct TerminalGuard {
    /// The mode the terminal is in.
    screen: Screen,
}

impl TerminalGuard {
    /// Enters raw mode, the alternate screen, and bracketed paste.
    ///
    /// A failure halfway still leaves the terminal as it was: raw mode
    /// leaves again when the screen refuses to enter, so a failed launch
    /// never leaves a half-configured terminal behind (SPEC 11.5).
    pub fn enter() -> io::Result<TerminalGuard> {
        terminal::enable_raw_mode()?;
        if let Err(error) = execute!(
            io::stdout(),
            EnterAlternateScreen,
            crossterm::event::EnableBracketedPaste,
            crossterm::event::EnableMouseCapture
        ) {
            let _ = execute!(
                io::stdout(),
                crossterm::event::DisableMouseCapture,
                crossterm::event::DisableBracketedPaste,
                LeaveAlternateScreen,
                crossterm::cursor::Show
            );
            let _ = terminal::disable_raw_mode();
            return Err(error);
        }
        Ok(TerminalGuard {
            screen: Screen::Interactive,
        })
    }

    /// Suspends every interactive mode, so one child editor meets a
    /// cooked, visible-cursor terminal on the primary screen (SPEC 11.5,
    /// 12.5). Restoration steps still run in sequence when one fails.
    pub fn suspend(&mut self) -> io::Result<()> {
        if self.screen != Screen::Interactive {
            return Ok(());
        }
        let mut failure = None;
        if let Err(error) = terminal::disable_raw_mode() {
            failure = Some(error);
        }
        let _ = execute!(
            io::stdout(),
            crossterm::event::DisableMouseCapture,
            crossterm::event::DisableBracketedPaste,
            LeaveAlternateScreen,
            crossterm::cursor::Show,
        );
        match failure {
            Some(error) => Err(error),
            None => {
                self.screen = Screen::Suspended;
                Ok(())
            }
        }
    }

    /// Re-enters every interactive mode after one child editor returned
    /// (SPEC 11.5).
    pub fn resume(&mut self) -> io::Result<()> {
        if self.screen != Screen::Suspended {
            return Ok(());
        }
        terminal::enable_raw_mode()?;
        // If re-entry fails, Drop still owes every interactive mode.
        self.screen = Screen::Interactive;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            crossterm::event::EnableBracketedPaste,
            crossterm::event::EnableMouseCapture
        )?;
        self.screen = Screen::Interactive;
        Ok(())
    }

    /// Leaves every interactive mode. Restoration runs once, and a failure
    /// of one step still tries the next: the cursor is shown even when the
    /// alternate screen refuses to leave, because the writer must get a
    /// usable terminal back on every path this process controls.
    pub fn restore(&mut self) {
        if self.screen == Screen::Left {
            return;
        }
        let interactive = self.screen == Screen::Interactive;
        self.screen = Screen::Left;
        if interactive {
            let _ = terminal::disable_raw_mode();
            let _ = execute!(
                io::stdout(),
                crossterm::event::DisableMouseCapture,
                crossterm::event::DisableBracketedPaste,
                LeaveAlternateScreen,
            );
        }
        let _ = execute!(io::stdout(), crossterm::cursor::Show);
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Runs the interactive surface until the writer quits.
///
/// The function returns when the shell stops or the terminal fails; the
/// guard restores the terminal on both paths. The surface writes only
/// through one confirmed finish of a review (SPEC 5.2) or one confirmed
/// restoration of a document (SPEC 5.5), so a quit and an interrupted exit
/// apply nothing that waits for a decision: their repository holds the
/// last state the writer confirmed, and nothing more.
pub fn launch(
    root: &Path,
    operations: &Operations<'_>,
    options: &TuiOptions,
) -> Result<(), LaunchError> {
    if !has_terminal() {
        return Err(LaunchError::new(
            "the interactive surface needs a terminal for input and drawing; run margin tui \
             without redirection",
        ));
    }
    // `auto` resolves once, here, against the environment of this launch;
    // the shell and the renderer then hold one decided mode (SPEC 11.4).
    let mut options = options.clone();
    options.color = resolved_color(options.color);
    // Agent and document paths are repository-relative even when the
    // desk was composed from a subdirectory.
    let root = operations
        .git()
        .toplevel()
        .unwrap_or_else(|_| root.to_path_buf());
    let git = operations.git().clone().at(&root);
    let operations = Operations::new(&git);
    let mut shell = Shell::open_for_launch(&root, &operations, &options);
    let mut guard = TerminalGuard::enter().map_err(|error| {
        LaunchError::new(format!(
            "the terminal cannot enter its interactive modes: {error}"
        ))
    })?;
    let mut worker = RefreshWorker::spawn(operations.git().clone());
    let result = event_loop(&mut shell, &mut worker, &mut guard, options.reduced_updates);
    if let Some(worker) = worker.take() {
        worker.shutdown();
    }
    drop(guard);
    result.map_err(|error| LaunchError::new(format!("the terminal stopped working: {error}")))
}

/// Runs one configured editor with the literal document path appended
/// (SPEC 5.6, 12.5): the argv is explicit, the path is never interpreted
/// by a shell, and the exit status is the caller's to report. The exit
/// approves nothing and captures nothing by itself; every caller decides
/// through the configured policy. The command line and the terminal
/// surface share this one handoff, so the two cannot disagree.
pub fn run_editor(argv: &[String], path: &Path) -> Result<ExitStatus, String> {
    let mut command = argv.to_vec();
    command.push(path.to_string_lossy().into_owned());
    Command::new(&command[0])
        .args(&command[1..])
        .status()
        .map_err(|error| format!("the editor could not start: {error}"))
}

/// Drains one answered inspection, draws, waits for one event, and
/// repeats.
///
/// The reduced-update mode draws only when the shell's frame-visible
/// state changed since the last drawn frame (SPEC 11.4). One requested
/// editor handoff suspends the terminal, waits for the child, and
/// resumes; the frame that follows is a whole repaint, because the editor
/// replaced the screen.
fn event_loop(
    shell: &mut Shell<'_>,
    worker: &mut Option<RefreshWorker>,
    guard: &mut TerminalGuard,
    reduced_updates: bool,
) -> io::Result<()> {
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let result = drive(&mut terminal, shell, worker, guard, reduced_updates);
    // The terminal prints one line to standard error when its own cursor
    // cleanup fails. On the dead terminal that caused this very failure,
    // that print panics; the panic must not mask the real error, and it
    // must not skip the guard that still owes the terminal its modes
    // (SPEC 11.5).
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(terminal);
    }));
    result
}

/// Dispatch only implemented input; the bool schedules a repository inspection.
fn dispatch_input(event: &Event, handle: impl FnOnce()) -> Option<bool> {
    if crate::tui::pointer::ignored(event) {
        return None;
    }
    handle();
    Some(!matches!(
        event,
        Event::Resize(_, _)
            | Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::ScrollUp
                    | crossterm::event::MouseEventKind::ScrollDown,
                ..
            })
    ))
}

fn draw_due(drawn: bool, changed: bool, input: bool, reduced: bool) -> bool {
    !drawn || changed || (input && !reduced)
}

/// The event loop proper, over one prepared terminal.
fn drive(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    shell: &mut Shell<'_>,
    worker: &mut Option<RefreshWorker>,
    guard: &mut TerminalGuard,
    reduced_updates: bool,
) -> io::Result<()> {
    let server = crate::desk::Server::start()?;
    let mut next_inspection = Instant::now();
    let mut drawn_once = false;
    let mut redraw_for_input = false;
    loop {
        let mut acknowledgement = None;
        if let Some(server) = &server {
            server.set_busy(shell.remote_busy());
            if server.take_notification() {
                shell.notify_edit_requests();
            }
            if let Ok(pending) = server.requests.try_recv()
                && Instant::now() < pending.deadline
            {
                let error = shell
                    .open_remote_until(&pending.request, Some(pending.deadline))
                    .err();
                acknowledgement = Some((
                    pending.reply,
                    crate::desk::Reply {
                        displayed: error.is_none(),
                        error,
                    },
                ));
            }
        }
        shell.poll_workspace_search();
        if acknowledgement.is_none() {
            shell.poll_edit_requests();
        }
        shell.poll_progress();
        shell.poll_palette();
        if let Some(worker) = worker.as_mut() {
            // Every answered inspection reaches the shell before the
            // frame that shows it: a reply the shell's generation
            // superseded is discarded inside, and a failed query claims
            // nothing (SPEC 13.3, 5.3).
            while let Some(reply) = worker.receive() {
                shell.observe_inspection(reply);
            }
        }
        // Deferred work settles only when no input waits behind it: one
        // rapid run of navigation steps loads exactly the document it ends
        // on and computes exactly the excerpt of the revision it rests on,
        // so the last settled content stays readable while keys repeat.
        if shell.has_deferred_work() && !event::poll(Duration::ZERO).unwrap_or(false) {
            shell.settle_preview();
        }
        if draw_due(
            drawn_once,
            shell.take_redraw(),
            redraw_for_input,
            reduced_updates,
        ) {
            // `try_draw`, never the panicking wrapper: a terminal that
            // stopped taking writes is an error this loop reports through
            // the typed contract, not a crash (SPEC 11.5).
            terminal.try_draw(|frame| {
                render::draw(shell, frame);
                Ok::<(), io::Error>(())
            })?;
            if let Some(server) = &server {
                server.set_context(shell.agent_context());
            }
            drawn_once = true;
            redraw_for_input = false;
        }
        if let Some((sender, reply)) = acknowledgement {
            let _ = sender.send(reply);
        }
        if shell.should_quit() {
            return Ok(());
        }
        // The deferred comparison and timeline load right after the first
        // drawn frame (SPEC 17.4): keys that arrive meanwhile wait in the
        // terminal buffer and are answered by the next pass, and the load
        // raises the redraw flag itself.
        if drawn_once && shell.load_pending_background() {
            continue;
        }
        if let Some(handoff) = shell.take_editor_request() {
            // The terminal hands itself to the child editor and takes
            // itself back; the shell reloads the document afterwards
            // (SPEC 11.5, 12.5).
            if let Some(server) = &server {
                server.set_busy(true);
            }
            guard.suspend()?;
            let returned = run_editor(&handoff.argv, &handoff.path);
            guard.resume()?;
            // The editor replaced the screen, so the frame that follows
            // must repaint everything. Resizing to the size the terminal
            // still reports resets the buffers and clears the viewport
            // without querying the cursor, which nothing answers on a
            // terminal that is not an answering emulator.
            let area = terminal.size()?.into();
            terminal.resize(area)?;
            shell.editor_returned(returned);
            continue;
        }
        let now = Instant::now();
        if now >= next_inspection {
            if let Some(worker) = worker.as_mut() {
                worker.request(WatchJob {
                    generation: shell.generation(),
                    watch: shell.watch(),
                });
            }
            next_inspection = now + INSPECTION_INTERVAL;
        }
        // The input wait ends at the next tick, so polling stays
        // two-second and every key press answers at once (SPEC 13.3).
        let mut wait = next_inspection.saturating_duration_since(now);
        if let Some(autosave_wait) = shell.autosave_wait(now) {
            wait = wait.min(autosave_wait);
        }
        if worker.as_ref().is_some_and(RefreshWorker::is_busy)
            || shell.workspace_search().searching()
            || shell.workspace_work_pending()
        {
            wait = wait.min(BUSY_WAIT);
        }
        if let Some(server) = &server {
            server.set_busy(shell.remote_busy());
            wait = wait.min(BUSY_WAIT);
        }
        if event::poll(wait)? {
            let event = event::read()?;
            if let Some(inspect) = dispatch_input(&event, || shell.handle_event(&event)) {
                redraw_for_input = true;
                if inspect && let Some(worker) = worker.as_mut() {
                    worker.request(WatchJob {
                        generation: shell.generation(),
                        watch: shell.watch(),
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_color_follows_the_no_color_and_dumb_term_conventions() {
        // An empty `NO_COLOR` is absent in effect; any non-empty value
        // turns decoration off (SPEC 11.4).
        use std::ffi::OsStr;
        assert!(auto_color_allowed(None, None));
        assert!(auto_color_allowed(Some(OsStr::new("")), None));
        assert!(!auto_color_allowed(Some(OsStr::new("1")), None));
        assert!(!auto_color_allowed(
            Some(OsStr::new("1")),
            Some("xterm-256color")
        ));
        assert!(!auto_color_allowed(None, Some("dumb")));
        assert!(auto_color_allowed(None, Some("xterm-256color")));
    }
    #[test]
    fn ignored_pointer_bursts_schedule_no_shell_work_frames_or_inspections() {
        use crossterm::event::{
            KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };
        for reduced in [false, true] {
            let mut handled = 0;
            let mut frames = 0;
            let mut inspections = 0;
            for kind in [
                MouseEventKind::Moved,
                MouseEventKind::Up(MouseButton::Left),
                MouseEventKind::Drag(MouseButton::Left),
            ]
            .into_iter()
            .cycle()
            .take(1000)
            {
                let event = Event::Mouse(MouseEvent {
                    kind,
                    column: 4,
                    row: 5,
                    modifiers: KeyModifiers::NONE,
                });
                let work = dispatch_input(&event, || handled += 1);
                if work == Some(true) {
                    inspections += 1;
                }
                if draw_due(true, false, work.is_some(), reduced) {
                    frames += 1;
                }
            }
            assert_eq!((handled, frames, inspections), (0, 0, 0));
            // Background completions and periodic inspections run outside input dispatch.
            assert!(draw_due(true, true, false, reduced));
            for kind in [
                MouseEventKind::ScrollDown,
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Down(MouseButton::Right),
            ] {
                let event = Event::Mouse(MouseEvent {
                    kind,
                    column: 4,
                    row: 5,
                    modifiers: KeyModifiers::NONE,
                });
                assert_eq!(
                    dispatch_input(&event, || handled += 1),
                    Some(kind != MouseEventKind::ScrollDown)
                );
                assert!(draw_due(true, true, true, reduced));
            }
            let key = Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
            assert_eq!(dispatch_input(&key, || handled += 1), Some(true));
            assert_eq!(handled, 4);
        }
    }
}
