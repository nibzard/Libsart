//! Terminal-lifecycle tests over a real pseudo-terminal (SPEC 11.5,
//! task T046).
//!
//! The buffer tests prove what the surface draws; these tests prove what
//! it does to the terminal itself. Every case spawns the built binary on
//! a pseudo-terminal, feeds it keys, and reads the bytes it writes, so
//! the claims are about modes, not about intentions: raw mode, the
//! alternate screen, bracketed paste, and cursor visibility are restored
//! on a normal quit, on `Ctrl-C`, on a terminal error, and around a
//! resize; the editor handoff suspends every mode for the child editor
//! and takes them back when it returns. An uncatchable kill cannot run
//! cleanup, and these tests do not claim otherwise.

#![cfg(unix)]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

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
        format!(
            "#!/bin/sh\nstty -a > \"$MARGIN_EDIT_PROBE\"\nprintf 'Edited in the editor.\\n' >> \
             \"$1\"\n"
        )
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
    pty.send_key(b"e");
    assert!(
        pty.wait_for("Edited in the editor.", LIMIT),
        "the editor ran and the reloaded document shows its line: {}",
        String::from_utf8_lossy(&pty.raw_output()),
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
