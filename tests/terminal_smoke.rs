//! The supported terminal smoke matrix (SPEC 16.9, task T051).
//!
//! The buffer suite proves what the surface draws and the lifecycle
//! suite proves what it does to one pseudo-terminal. This file is the
//! release smoke matrix: it runs the installed binary across the
//! terminal profiles a release claims — one `TERM` value after another
//! over real pseudo-terminals, the classic 80x24 geometry, color-off,
//! and reduced updates — and once through an actual terminal emulator,
//! a private tmux server whose `capture-pane` reports the screen it
//! interpreted rather than the bytes the process wrote. The observed
//! matrix, the environment that produced it, and the combinations this
//! machine cannot observe are recorded in
//! [docs/terminal-smoke-matrix.md](../docs/terminal-smoke-matrix.md);
//! this file is how that record is regenerated.

#![cfg(unix)]

mod support;

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use support::pty::Pty;
use support::repo::TestRepo;

/// How long any one wait in these tests may take.
const LIMIT: Duration = Duration::from_secs(20);

/// One terminal profile of the smoke matrix: the `TERM` the process
/// sees, the window geometry, and the surface options.
struct Profile {
    name: &'static str,
    term: Option<&'static str>,
    columns: u16,
    rows: u16,
    args: &'static [&'static str],
}

/// The profiles a release smoke run covers on a headless Unix machine.
const PROFILES: &[Profile] = &[
    Profile {
        name: "no TERM at all",
        term: None,
        columns: 110,
        rows: 32,
        args: &[],
    },
    Profile {
        name: "xterm-256color",
        term: Some("xterm-256color"),
        columns: 110,
        rows: 32,
        args: &[],
    },
    Profile {
        name: "xterm",
        term: Some("xterm"),
        columns: 110,
        rows: 32,
        args: &[],
    },
    Profile {
        name: "screen-256color",
        term: Some("screen-256color"),
        columns: 110,
        rows: 32,
        args: &[],
    },
    Profile {
        name: "tmux-256color",
        term: Some("tmux-256color"),
        columns: 110,
        rows: 32,
        args: &[],
    },
    Profile {
        name: "vt100",
        term: Some("vt100"),
        columns: 110,
        rows: 32,
        args: &[],
    },
    Profile {
        name: "linux",
        term: Some("linux"),
        columns: 110,
        rows: 32,
        args: &[],
    },
    Profile {
        name: "dumb",
        term: Some("dumb"),
        columns: 110,
        rows: 32,
        args: &[],
    },
    Profile {
        name: "xterm-256color at the classic 80x24",
        term: Some("xterm-256color"),
        columns: 80,
        rows: 24,
        args: &[],
    },
    Profile {
        name: "xterm-256color with color off",
        term: Some("xterm-256color"),
        columns: 110,
        rows: 32,
        args: &["--color", "never"],
    },
    Profile {
        name: "xterm-256color with reduced updates",
        term: Some("xterm-256color"),
        columns: 110,
        rows: 32,
        args: &["--reduced-updates"],
    },
];

/// Runs the built binary in `cwd` with the isolated environment.
fn margin(env: &support::env::IsolatedEnv, cwd: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_margin"));
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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

/// One adopted repository: a start marker over `notes/` and one draft
/// checkpoint of an edit to `notes/a.md`.
fn adopted_repo() -> TestRepo {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n\nFirst passage.\n");
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

/// The child environment of one spawned surface with `TERM` replaced by
/// `term`, or removed entirely when it is `None`.
fn envs_with_term(repo: &TestRepo, term: Option<&str>) -> Vec<(String, String)> {
    let mut vars: Vec<(String, String)> = repo
        .env()
        .vars()
        .iter()
        .filter(|(key, _)| key.as_os_str() != "TERM" && key.as_os_str() != "NO_COLOR")
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect();
    // These profiles control decoration explicitly; do not inherit the runner preference.
    vars.push(("NO_COLOR".to_string(), String::new()));
    if let Some(term) = term {
        vars.push(("TERM".to_string(), term.to_string()));
    }
    vars
}

/// Spawns the interactive surface on a pseudo-terminal under one profile.
fn spawn_profile(repo: &TestRepo, profile: &Profile) -> Pty {
    let mut args = vec!["tui"];
    args.extend(profile.args.iter().copied());
    Pty::spawn(
        Path::new(env!("CARGO_BIN_EXE_margin")),
        &args,
        &envs_with_term(repo, profile.term),
        repo.path(),
        profile.columns,
        profile.rows,
    )
    .unwrap_or_else(|error| panic!("{}: the surface spawns: {error}", profile.name))
}

/// Asserts the full cleanup of one exit path (SPEC 11.5).
fn assert_restored(pty: &mut Pty, profile: &str, exit: i32) {
    let status = pty
        .wait_exit(LIMIT)
        .unwrap_or_else(|| panic!("{profile}: the surface did not exit after its final key"));
    assert_eq!(status.code(), Some(exit), "{profile}: the exit class");
    assert!(
        pty.shows_raw("\x1b[?1049l"),
        "{profile}: the alternate screen was left",
    );
    assert!(
        pty.shows_raw("\x1b[?2004l"),
        "{profile}: bracketed paste was disabled",
    );
    assert!(
        pty.shows_raw("\x1b[?25h"),
        "{profile}: the cursor is visible again",
    );
    assert!(
        pty.terminal_is_cooked(),
        "{profile}: raw mode was restored to cooked input",
    );
}

#[test]
fn every_term_profile_draws_and_restores() {
    // One cell per profile of the matrix: the first frame draws the
    // document, the surface enters its modes, and a plain quit leaves
    // every one of them (SPEC 11.5). The surface emits the same mode and
    // drawing sequences whatever `TERM` says — it queries no terminfo
    // database — so the profiles differ only in what an emulator outside
    // this process would do with the bytes.
    let repo = adopted_repo();
    for profile in PROFILES {
        let mut pty = spawn_profile(&repo, profile);
        assert!(
            pty.wait_for("First passage", LIMIT),
            "{}: the surface drew its first frame: {}",
            profile.name,
            String::from_utf8_lossy(pty.raw_output()),
        );
        assert!(
            pty.shows_raw("\x1b[?1049h"),
            "{}: the alternate screen was entered",
            profile.name,
        );
        assert!(
            pty.shows_raw("\x1b[?2004h"),
            "{}: bracketed paste was enabled",
            profile.name,
        );
        assert!(
            !pty.terminal_is_cooked(),
            "{}: raw mode is on while the surface runs",
            profile.name,
        );

        pty.send_key(b"q");
        assert_restored(&mut pty, profile.name, 0);
    }
}

#[test]
fn color_off_draws_no_decoration_codes() {
    // The decoration cell of the matrix (SPEC 11.4): the default surface
    // decorates with dim and bold select-graphic-rendition sequences, and
    // `--color never` draws none of them while the words stay readable.
    let repo = adopted_repo();
    let mut decorated = spawn_profile(
        &repo,
        &Profile {
            name: "xterm-256color",
            term: Some("xterm-256color"),
            columns: 110,
            rows: 32,
            args: &[],
        },
    );
    assert!(
        decorated.wait_for("First passage", LIMIT),
        "the decorated surface drew its first frame",
    );
    let dim = decorated.count_raw("\x1b[2m");
    let bold = decorated.count_raw("\x1b[1m");
    assert!(
        dim + bold > 0,
        "the default surface decorates with dim or bold codes",
    );
    decorated.send_key(b"q");
    assert_restored(&mut decorated, "xterm-256color", 0);

    let mut plain = spawn_profile(
        &repo,
        &Profile {
            name: "xterm-256color with color off",
            term: Some("xterm-256color"),
            columns: 110,
            rows: 32,
            args: &["--color", "never"],
        },
    );
    assert!(
        plain.wait_for("First passage", LIMIT),
        "the monochrome surface drew its first frame",
    );
    assert_eq!(
        plain.count_raw("\x1b[2m") + plain.count_raw("\x1b[1m"),
        0,
        "color off draws no dim or bold code: {}",
        String::from_utf8_lossy(plain.raw_output()),
    );
    plain.send_key(b"q");
    assert_restored(&mut plain, "xterm-256color with color off", 0);
}

/// Says whether this machine has a tmux the smoke run can compose with.
fn tmux_present() -> bool {
    Command::new("tmux")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// One private tmux server: a short socket directory of its own, so the
/// smoke run shares no server with any other process, and no
/// configuration is read (`-f /dev/null`).
struct SmokeServer {
    dir: std::path::PathBuf,
}

impl SmokeServer {
    fn new(tag: &str) -> SmokeServer {
        let prefix =
            std::env::var("LOOPER_RESOURCE_PREFIX").unwrap_or_else(|_| "margin".to_string());
        let unique = format!("smoke-{prefix}-{tag}-{}", std::process::id());
        // A Unix socket address is bounded well below a normal file path,
        // and the fixed tail `/tmux-<uid>/default` has to fit beside the
        // directory name.
        let mut bases = [std::env::temp_dir(), std::path::PathBuf::from("/tmp")];
        let dir = bases
            .iter_mut()
            .map(|base| base.join(&unique))
            .find(|dir| dir.as_os_str().len() + "/tmux-10000000/default".len() < 96)
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp").join(unique));
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|error| panic!("cannot create {}: {error}", dir.display()));
        SmokeServer { dir }
    }

    /// One tmux client call on this server, with no inherited session of
    /// the runner's own terminal.
    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new("tmux");
        command
            .args(args)
            .env("TMUX_TMPDIR", &self.dir)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        command.output().expect("cannot run tmux")
    }

    /// One tmux call that must succeed.
    fn ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "tmux {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// The screen one pane shows, as the emulator interpreted it.
    fn screen(&self, session: &str) -> String {
        let output = self.ok(&["capture-pane", "-p", "-t", session]);
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Says whether the session still exists.
    fn session_alive(&self, session: &str) -> bool {
        self.run(&["has-session", "-t", session]).status.success()
    }
}

impl Drop for SmokeServer {
    fn drop(&mut self) {
        let _ = self.run(&["kill-server"]);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

#[test]
fn a_real_emulator_renders_the_surface_and_passes_it_a_resize() {
    // The one cell of the matrix a real terminal fills: a tmux server
    // interprets the bytes the surface writes, `capture-pane` reports the
    // screen it shows, a window resize reaches the process as the resize
    // a terminal sends, and a quit through the emulator's own key
    // delivery ends the process (SPEC 11.2, 11.5, 16.9).
    if !tmux_present() {
        return;
    }
    let server = SmokeServer::new("matrix");
    let repo = adopted_repo();

    // The pane environment is exactly the isolated environment of the
    // repository, carried through `env -i` so the server's own
    // inheritance cannot add or override anything.
    let mut command = String::from("env -i");
    for (key, value) in repo.env().vars() {
        let key = key.to_string_lossy();
        let value = value.to_string_lossy();
        assert!(
            !key.contains([' ', '\'']),
            "the environment carries no value a shell must interpret: {key}",
        );
        command.push_str(&format!(" {key}='{value}'"));
    }
    command.push_str(&format!(
        " '{}' tui",
        env!("CARGO_BIN_EXE_margin").replace('\'', "'\\''"),
    ));
    server.ok(&[
        "-f",
        "/dev/null",
        "new-session",
        "-d",
        "-x",
        "110",
        "-y",
        "32",
        "-s",
        "smoke",
        "-n",
        "margin",
        "-c",
        &repo.path().display().to_string(),
        &command,
    ]);

    // The emulator — not this test — interprets the frame.
    let deadline = std::time::Instant::now() + LIMIT;
    let mut screen = server.screen("smoke:margin");
    while !screen.contains("First passage") && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(120));
        screen = server.screen("smoke:margin");
    }
    assert!(
        screen.contains("First passage"),
        "the emulator shows the document: {screen}",
    );

    // A resize through the emulator reaches the process, which redraws
    // its minimum-size explanation; growing back redraws the document.
    server.ok(&[
        "resize-window",
        "-t",
        "smoke:margin",
        "-x",
        "50",
        "-y",
        "10",
    ]);
    let deadline = std::time::Instant::now() + LIMIT;
    let mut screen = server.screen("smoke:margin");
    while !screen.contains("Terminal too small") && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(120));
        screen = server.screen("smoke:margin");
    }
    assert!(
        screen.contains("Terminal too small"),
        "the resized emulator shows the minimum-size explanation: {screen}",
    );
    server.ok(&[
        "resize-window",
        "-t",
        "smoke:margin",
        "-x",
        "110",
        "-y",
        "32",
    ]);
    let deadline = std::time::Instant::now() + LIMIT;
    let mut screen = server.screen("smoke:margin");
    while !screen.contains("First passage") && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(120));
        screen = server.screen("smoke:margin");
    }
    assert!(
        screen.contains("First passage"),
        "the grown-back emulator shows the document again: {screen}",
    );

    // A quit delivered as the emulator delivers keys ends the process and
    // with it the session.
    server.ok(&["send-keys", "-t", "smoke:margin", "q"]);
    let deadline = std::time::Instant::now() + LIMIT;
    while server.session_alive("smoke") && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(120));
    }
    assert!(
        !server.session_alive("smoke"),
        "the quit through the emulator ended the surface",
    );
}
