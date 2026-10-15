//! Terminal-lifecycle tests over a real pseudo-terminal (SPEC 11.5,
//! 16.9, tasks T046 and T051).
//!
//! The buffer tests prove what the surface draws; these tests prove what
//! it does to the terminal itself. Every case spawns the built binary on
//! a pseudo-terminal, feeds it keys, and reads the bytes it writes, so
//! the claims are about modes, not about intentions: raw mode, the
//! alternate screen, bracketed paste, and cursor visibility are restored
//! on a normal quit, on `Ctrl-C`, on a terminal error, and around a
//! resize; the editor handoff suspends every mode for the child editor
//! and takes them back when it returns. One case drives a whole
//! selection-and-finish sequence through the keys and compares the
//! commit with the same plan applied by the command line, one observes a
//! stale arrival reach the frame through the background inspection
//! without a key press, and one meets a failing editor child. An
//! uncatchable kill cannot run cleanup, and these tests do not claim
//! otherwise.

#![cfg(unix)]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use margin::app::Operations;
use margin::git::Git;
use margin::tui::{FinishSummary, Shell, TuiOptions};
use support::pty::Pty;
use support::repo::TestRepo;

/// How long any one wait in these tests may take.
const LIMIT: Duration = Duration::from_secs(20);

/// Runs the built binary in `cwd` with the isolated environment.
fn margin(env: &support::env::IsolatedEnv, cwd: &Path, args: &[&str]) -> Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_margin"));
    command
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (key, _) in std::env::vars_os() {
        if support::env::is_scrubbed(&key) {
            command.env_remove(&key);
        }
    }
    command.envs(env.vars());
    command
        .output()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"))
}

/// The child environment of one spawned surface, as the harness passes
/// it, plus any extra pairs the case needs.
fn envs(repo: &TestRepo, extra: &[(&str, PathBuf)]) -> Vec<(String, String)> {
    let mut vars: Vec<(String, String)> = repo
        .env()
        .vars()
        .iter()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect();
    for (key, value) in extra {
        vars.push((key.to_string(), value.display().to_string()));
    }
    vars
}

/// One adopted repository: a start marker over `notes/` and one draft
/// checkpoint of an edit to `notes/a.md`.
fn adopted_repo() -> TestRepo {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n\nFirst passage.\n");
    repo.write_file("notes/b.md", b"# Two\n\nSecond passage.\n");
    repo.commit_all("import");
    let output = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes/", "--json"],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "init must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    repo.write_file("notes/a.md", b"# One\n\nFirst passage, edited.\n");
    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "checkpoint",
            "--message",
            "Edit the first passage",
            "--json",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "checkpoint must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    repo
}

/// Spawns the interactive surface on a pseudo-terminal.
fn spawn_tui(repo: &TestRepo, extra: &[(&str, PathBuf)], columns: u16, rows: u16) -> Pty {
    spawn_tui_with_args(repo, extra, &["tui"], columns, rows)
}

/// Spawns the interactive surface with explicit arguments.
fn spawn_tui_with_args(
    repo: &TestRepo,
    extra: &[(&str, PathBuf)],
    args: &[&str],
    columns: u16,
    rows: u16,
) -> Pty {
    Pty::spawn(
        Path::new(env!("CARGO_BIN_EXE_margin")),
        args,
        &envs(repo, extra),
        repo.path(),
        columns,
        rows,
    )
    .expect("the surface spawns on a pseudo-terminal")
}

/// Waits for the first real frame and asserts the modes it entered.
fn drew_first_frame(pty: &mut Pty) {
    assert!(
        pty.wait_for("First passage", LIMIT),
        "the surface drew its first frame: {}",
        String::from_utf8_lossy(&pty_snapshot(pty)),
    );
    assert!(
        pty.shows_raw("\x1b[?1049h"),
        "the alternate screen was entered",
    );
    assert!(pty.shows_raw("\x1b[?2004h"), "bracketed paste was enabled",);
    assert!(
        pty.shows_raw("\x1b[?1006h"),
        "SGR mouse capture was enabled"
    );
    assert!(
        !pty.terminal_is_cooked(),
        "raw mode is on while the surface runs",
    );
}

/// A copy of the raw output so far, for failure messages.
fn pty_snapshot(pty: &Pty) -> Vec<u8> {
    pty.raw_output().to_vec()
}

/// Asserts the full cleanup of one exit path (SPEC 11.5): alternate
/// screen left, bracketed paste disabled, cursor shown, raw mode off.
fn assert_restored(pty: &mut Pty, exit: i32) {
    let status = pty
        .wait_exit(LIMIT)
        .unwrap_or_else(|| panic!("the surface did not exit after its final key"));
    assert_eq!(status.code(), Some(exit), "the exit class of this path");
    assert!(
        pty.shows_raw("\x1b[?1049l"),
        "the alternate screen was left",
    );
    assert!(pty.shows_raw("\x1b[?2004l"), "bracketed paste was disabled",);
    assert!(
        pty.shows_raw("\x1b[?1006l"),
        "SGR mouse capture was disabled"
    );
    assert!(pty.shows_raw("\x1b[?25h"), "the cursor is visible again");
    assert!(
        pty.terminal_is_cooked(),
        "raw mode was restored to cooked input",
    );
}

#[test]
fn a_normal_quit_restores_the_terminal() {
    let repo = adopted_repo();
    let mut pty = spawn_tui(&repo, &[], 110, 32);
    drew_first_frame(&mut pty);

    pty.send_key(b"q");
    assert_restored(&mut pty, 0);
}

#[test]
fn ctrl_c_quits_cleanly_and_restores_the_terminal() {
    let repo = adopted_repo();
    let mut pty = spawn_tui(&repo, &[], 110, 32);
    drew_first_frame(&mut pty);

    // Raw mode turns `Ctrl-C` into a key, not a signal: the surface
    // answers it with its clean quit (SPEC 11.3).
    pty.send_key(b"\x03");
    assert_restored(&mut pty, 0);
}

#[test]
fn a_terminal_error_still_restores_the_terminal() {
    let repo = adopted_repo();
    // One terminal for input, a second for output. Closing the output
    // terminal's master makes every write of the surface fail while its
    // input stays healthy, which is the mid-run write error the
    // handled-error path takes (SPEC 11.5).
    let mut output = support::pty::OutputPty::new(110, 32).expect("an output terminal");
    let mut pty = Pty::spawn_with_output(
        Path::new(env!("CARGO_BIN_EXE_margin")),
        &["tui"],
        &envs(&repo, &[]),
        repo.path(),
        110,
        32,
        Some(output.slave().expect("the output slave")),
    )
    .expect("the surface spawns on a pseudo-terminal");

    // The first frame draws through the output terminal.
    assert!(
        output.wait_for("First passage", LIMIT),
        "the surface drew its first frame",
    );
    assert!(
        !pty.terminal_is_cooked(),
        "raw mode is on while the surface runs",
    );

    // Kill the output terminal, then wake the loop with one key: the
    // next draw cannot write, the loop fails, and the exit is the typed
    // execution failure — after the guard restored the input terminal.
    output.kill();
    pty.send_key(b"j");
    let status = pty
        .wait_exit(LIMIT)
        .unwrap_or_else(|| panic!("the surface did not exit after its terminal failed"));
    assert_eq!(
        status.code(),
        Some(5),
        "a dead output terminal is the typed IO_FAILED execution failure",
    );
    assert!(
        pty.terminal_is_cooked(),
        "the error path restored raw mode to cooked input",
    );
}

#[test]
fn a_resize_redraws_and_keeps_quit_working() {
    let repo = adopted_repo();
    let mut pty = spawn_tui(&repo, &[], 110, 32);
    drew_first_frame(&mut pty);

    // Below the useful minimum the surface explains itself and keeps
    // quit and resize working (SPEC 11.2).
    pty.resize(50, 10);
    assert!(
        pty.wait_for("Terminal too small", LIMIT),
        "the resize redrew the minimum-size message",
    );

    // Growing back redraws the document; the repository never changed.
    pty.resize(110, 32);
    assert!(
        pty.wait_for("First passage", LIMIT),
        "the resize back redrew the document",
    );
    pty.send_key(b"q");
    assert_restored(&mut pty, 0);
}

#[test]
fn reduced_updates_and_color_off_run_the_same_lifecycle() {
    let repo = adopted_repo();
    let mut pty = spawn_tui_with_args(
        &repo,
        &[],
        &["tui", "--reduced-updates", "--color", "never"],
        110,
        32,
    );
    // The first frame draws in reduced-update mode (SPEC 11.4), the
    // color-off surface still shows every label, and the quit path still
    // restores every mode.
    drew_first_frame(&mut pty);
    pty.send_key(b"j");
    pty.send_key(b"q");
    assert_restored(&mut pty, 0);
}

#[test]
fn the_editor_handoff_suspends_the_terminal_and_takes_it_back() {
    let scratch = tempfile::TempDir::new().expect("a scratch folder");
    let probe = scratch.path().join("editor-terminal.txt");
    let editor = scratch.path().join("stand-in-editor.sh");
    std::fs::write(
        &editor,
        "#!/bin/sh\nstty -a > \"$MARGIN_EDIT_PROBE\"\nprintf 'Edited in the editor.\\n' >> \
         \"$1\"\n"
            .as_bytes(),
    )
    .expect("the stand-in editor");
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755))
        .expect("the stand-in editor is executable");

    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n\nFirst passage.\n");
    repo.commit_all("import");
    let editor_json = format!("[\"{}\"]", editor.display());
    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "init",
            "--scope",
            "notes/",
            "--auto-checkpoint",
            "--editor-argv-json",
            &editor_json,
            "--json",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "init with an editor must succeed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let head_at_init = support::oracle::head_oid(&repo).expect("the init recorded a head");

    let mut pty = spawn_tui(&repo, &[("MARGIN_EDIT_PROBE", probe.clone())], 110, 32);
    drew_first_frame(&mut pty);

    // `e` hands the document to the configured editor (SPEC 11.3, 12.5).
    pty.send_key(b"E");
    assert!(
        pty.wait_for("Edited in the editor.", LIMIT),
        "the editor ran and the reloaded document shows its line: {}",
        String::from_utf8_lossy(pty.raw_output()),
    );
    // The handoff left the alternate screen for the child and re-entered
    // it on return, beside the one pair of the opening frame.
    assert!(
        pty.count_raw("\x1b[?1049l") >= 1,
        "the handoff left the alternate screen for the editor",
    );
    assert!(
        pty.count_raw("\x1b[?1049h") >= 2,
        "the handoff re-entered the alternate screen on return",
    );
    assert!(
        pty.count_raw("\x1b[?2004h") >= 2,
        "bracketed paste was re-enabled on return",
    );

    // The editor itself met a cooked terminal: its own stty report shows
    // canonical input and echo, the modes raw mode had turned off.
    let report = std::fs::read_to_string(&probe).expect("the editor wrote its terminal report");
    let flags = report.split_whitespace();
    for flag in ["-icanon", "-echo", "-isig"] {
        assert!(
            !flags.clone().any(|set| set == flag),
            "the editor met cooked input and echo, not {flag}: {report}",
        );
    }
    // The document carries the editor's line, and nothing committed it:
    // the head the init recorded still stands (SPEC 12.5).
    let edited = repo.read_file("notes/a.md");
    assert!(
        String::from_utf8_lossy(&edited).contains("Edited in the editor."),
        "the editor's line is in the document",
    );
    assert_eq!(
        support::oracle::head_oid(&repo),
        Some(head_at_init),
        "the editor return committed nothing",
    );

    pty.send_key(b"q");
    assert_restored(&mut pty, 0);
}

/// One key-press event for the in-process shell of the parity side.
fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

/// Trims one Git output to text.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

#[test]
fn a_selection_and_finish_sequence_applies_through_the_keys() {
    // The whole review sequence through the keys of a real terminal
    // (SPEC 16.9): select one passage, restore it, finish, and apply.
    // Stock Git proves the commit; the command line proves the parity —
    // the same decisions as one plan document the surface itself
    // composes, applied by the installed executable on an identical
    // repository, agree on tree, marker, and working bytes.
    let repo = adopted_repo();
    let candidate = support::oracle::head_oid(&repo).expect("the draft stands at HEAD");
    let mut pty = spawn_tui(&repo, &[], 110, 32);
    drew_first_frame(&mut pty);

    // The Changes view, one restoration, the summary, the apply.
    pty.send_key(b"2");
    assert!(
        pty.wait_for("Group 1 of 1", LIMIT),
        "the changes view shows the undecided passage: {}",
        String::from_utf8_lossy(&pty_snapshot(&pty)),
    );
    assert!(
        pty.wait_for("Undecided", LIMIT),
        "the passage starts undecided: {}",
        String::from_utf8_lossy(&pty_snapshot(&pty)),
    );
    pty.send_key(b"r");
    // The repaint redraws only changed cells, so the fresh state label
    // is the text to read: a partial frame may leave separator and
    // notice cells undrawn beside it.
    assert!(
        pty.wait_for("Restored", LIMIT),
        "the key recorded the proposed restoration: {}",
        String::from_utf8_lossy(&pty_snapshot(&pty)),
    );
    pty.send_key(b"\r");
    assert!(
        pty.wait_for("Finishing keeps every passage", LIMIT),
        "the first Enter opened the summary: {}",
        String::from_utf8_lossy(&pty_snapshot(&pty)),
    );
    pty.send_key(b"\r");
    assert!(
        pty.wait_for("review recorded", LIMIT),
        "the second Enter applied the plan: {}",
        String::from_utf8_lossy(&pty_snapshot(&pty)),
    );

    pty.send_key(b"q");
    assert_restored(&mut pty, 0);

    // Stock Git proves the application: one commit joined, it carries the
    // review marker, and the working document holds the restored passage
    // (SPEC 16.3, 16.9).
    let applied = support::oracle::head_oid(&repo).expect("the finish committed");
    assert_ne!(applied, candidate, "one commit joined");
    let parents = text(&repo.git_ok(["log", "-n", "1", "--format=%P", &applied]));
    assert_eq!(parents, candidate, "one parent: the candidate C");
    let message = text(&repo.git_ok(["log", "-n", "1", "--format=%B", &applied]));
    assert!(
        message.contains("Margin-Kind: review"),
        "the commit carries the review marker: {message}",
    );
    assert_eq!(
        repo.read_file("notes/a.md"),
        b"# One\n\nFirst passage.\n",
        "the restored passage returned to the baseline bytes",
    );

    // The command line applies the same decisions on an identical
    // repository (SPEC 16.9): the plan document the surface composes,
    // written out and applied by the installed executable. Commit OIDs
    // may differ; the trees, the marker kind, and the bytes may not.
    let peer = adopted_repo();
    let peer_git = Git::from_env(peer.env().vars().clone()).at(peer.path());
    let peer_operations = Operations::new(&peer_git);
    let mut shell = Shell::open(peer.path(), &peer_operations, &TuiOptions::default());
    shell.handle_event(&key(KeyCode::Char('2')));
    shell.handle_event(&key(KeyCode::Char('r')));
    shell.handle_event(&key(KeyCode::Enter));
    let document = shell
        .finish()
        .and_then(FinishSummary::plan_document)
        .map(|document| document.to_vec())
        .expect("the summary composes one plan document");
    shell.handle_event(&key(KeyCode::Esc));
    peer.write_file("peer-plan.json", &document);
    let cli = margin(
        peer.env(),
        peer.path(),
        &["review", "--input", "peer-plan.json", "--json"],
    );
    assert_eq!(
        cli.status.code(),
        Some(0),
        "the command line applies the same plan: {}",
        String::from_utf8_lossy(&cli.stderr),
    );
    assert_eq!(
        text(&peer.git_ok(["rev-parse", "HEAD^{tree}"])),
        text(&repo.git_ok(["rev-parse", "HEAD^{tree}"])),
        "both entry points produced one tree",
    );
    let cli_message = text(&peer.git_ok(["log", "-n", "1", "--format=%B"]));
    assert!(cli_message.contains("Margin-Kind: review"));
    assert_eq!(peer.read_file("notes/a.md"), repo.read_file("notes/a.md"));
}

#[test]
fn a_stale_arrival_reaches_the_frame_without_a_key_press() {
    // The background inspection observes new edits while the surface
    // waits for input, and the frame states the arrival on its own
    // (SPEC 5.3, 13.3): no key press drives the notice.
    let repo = adopted_repo();
    let mut pty = spawn_tui(&repo, &[], 110, 32);
    drew_first_frame(&mut pty);

    // New edits arrive: another checkpoint moves the head past the
    // comparison the open surface pinned.
    repo.write_file("notes/a.md", b"# One\n\nFirst passage, edited again.\n");
    let output = margin(
        repo.env(),
        repo.path(),
        &["checkpoint", "--message", "Later edits", "--json"],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "the later checkpoint must succeed: {}",
        String::from_utf8_lossy(&output.stderr),
    );

    assert!(
        pty.wait_for("New edits arrived", LIMIT),
        "the inspection tick surfaced the arrival without a key: {}",
        String::from_utf8_lossy(&pty_snapshot(&pty)),
    );
    pty.send_key(b"q");
    assert_restored(&mut pty, 0);
}

#[test]
fn a_failing_editor_child_is_stated_and_the_surface_survives() {
    // One child of the surface fails while it runs (SPEC 16.9): the
    // editor exits nonzero, the surface states the abandoned pass, takes
    // its modes back, and keeps working until a clean quit.
    let scratch = tempfile::TempDir::new().expect("a scratch folder");
    let editor = scratch.path().join("failing-editor.sh");
    std::fs::write(&editor, b"#!/bin/sh\nexit 3\n").expect("the failing editor");
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755))
        .expect("the failing editor is executable");

    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n\nFirst passage.\n");
    repo.commit_all("import");
    let editor_json = format!("[\"{}\"]", editor.display());
    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "init",
            "--scope",
            "notes/",
            "--editor-argv-json",
            &editor_json,
            "--json",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "init with an editor must succeed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let head_at_init = support::oracle::head_oid(&repo).expect("the init recorded a head");

    let mut pty = spawn_tui(&repo, &[], 110, 32);
    drew_first_frame(&mut pty);

    // `e` hands the document to the editor; the child fails at once.
    pty.send_key(b"E");
    assert!(
        pty.wait_for("editor exited with", LIMIT),
        "the failed pass is stated: {}",
        String::from_utf8_lossy(&pty_snapshot(&pty)),
    );
    assert!(
        pty.wait_for("nothing was applied", LIMIT),
        "the failed pass applies nothing: {}",
        String::from_utf8_lossy(&pty_snapshot(&pty)),
    );
    assert!(
        pty.count_raw("\x1b[?1049h") >= 2,
        "the surface re-entered the alternate screen after the child",
    );

    // The failure committed and rewrote nothing, and the surface still
    // quits cleanly through its restored terminal.
    assert_eq!(
        support::oracle::head_oid(&repo),
        Some(head_at_init),
        "the failed editor pass committed nothing",
    );
    assert_eq!(
        repo.read_file("notes/a.md"),
        b"# One\n\nFirst passage.\n",
        "the failed editor pass changed no byte",
    );
    pty.send_key(b"q");
    assert_restored(&mut pty, 0);
}

#[test]
fn sgr_pointer_press_opens_a_document_and_release_is_ignored() {
    let repo = adopted_repo();
    repo.write_file(
        "notes/b.md",
        b"# Click target\n\nZYXWVUTSRQPONMLKJIHGFEDCBA.\n",
    );
    let before = support::oracle::snapshot(&repo);
    let git = Git::from_env(repo.env().vars().clone()).at(repo.path());
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    let backend = ratatui::backend::TestBackend::new(110, 32);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| margin::tui::render::draw(&mut shell, frame))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let row = (0..32)
        .find(|y| {
            (0..28)
                .map(|x| buffer[(x, *y)].symbol())
                .collect::<String>()
                .contains("notes/b.md")
        })
        .unwrap();
    let mut pty = spawn_tui(&repo, &[], 110, 32);
    drew_first_frame(&mut pty);
    // SGR coordinates are one-based, unlike crossterm's terminal cells.
    pty.send_key(format!("\x1b[<0;4;{}M", row + 1).as_bytes());
    assert!(pty.wait_for("ZYXWVUTSRQPONMLKJIHGFEDCBA", LIMIT));
    pty.send_key(b"\x1b[<0;4;4m");
    pty.send_key(b"q");
    assert_eq!(pty.wait_exit(LIMIT).unwrap().code(), Some(0));
    assert!(pty.shows_raw("\x1b[?1006l"), "mouse capture was disabled");

    before.assert_unchanged(&support::oracle::snapshot(&repo), "SGR pointer navigation");
}
