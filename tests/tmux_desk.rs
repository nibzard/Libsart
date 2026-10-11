//! tmux desk composition tests (SPEC 12.6, task T050).
//!
//! Every case runs the built binary against a real tmux server that lives
//! on a private socket, so the claims are about processes, not intentions:
//! the desk launches the explicit agent argv verbatim beside a Margin pane
//! or window, edits no global tmux option and no unrelated pane or session,
//! keeps a narrow arrangement as one command per window instead of two
//! unreadable columns, explains the two-tab alternative when tmux is
//! missing, and leaves the agent running when Margin quits. The report is
//! read as the writer reads it: the processes and session the desk created,
//! the prefix key queried from the session, and the attach and exit
//! behavior.
//!
//! Each server starts with `-f /dev/null` under a short private
//! `TMUX_TMPDIR`, so no user configuration is read; the harness cannot use
//! the runner's own temporary directory for the socket because that path is
//! longer than a Unix socket address allows. Every directory name carries
//! the Looper resource prefix, the process id, and a counter, so parallel
//! tests never share a server.

#![cfg(unix)]

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use support::pty::Pty;
use support::repo::TestRepo;

/// How long any one wait in these tests may take.
const LIMIT: Duration = Duration::from_secs(20);

/// The longest tmux socket address this harness accepts. A Unix socket
/// path is bounded well below a normal file path, and the fixed tail
/// `/tmux-<uid>/default` has to fit beside the directory name.
const MAX_SOCKET_PATH: usize = 96;

/// Numbers the private socket directories of one test process.
static SERVERS: AtomicU32 = AtomicU32::new(0);

/// Says whether this machine has a tmux the desk can compose with. The
/// desk is optional (SPEC 12.6), so the composition claims are exercised
/// wherever tmux exists; the missing-tool guidance has its own case.
fn tmux_present() -> bool {
    Command::new("tmux")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// One private tmux server, removed with everything it created when the
/// test ends.
struct DeskServer {
    /// The `TMUX_TMPDIR` of this server: the socket lives below it.
    dir: PathBuf,
}

impl DeskServer {
    /// Creates the private socket directory of one server.
    fn new(tag: &str) -> DeskServer {
        let prefix =
            std::env::var("LOOPER_RESOURCE_PREFIX").unwrap_or_else(|_| "margin".to_string());
        let unique = format!(
            "desk-{prefix}-{tag}-{}-{}",
            std::process::id(),
            SERVERS.fetch_add(1, Ordering::SeqCst),
        );
        // The runner's temporary directory can exceed a socket address,
        // so the first base that keeps the whole path short enough wins.
        let mut bases = [std::env::temp_dir(), PathBuf::from("/tmp")];
        let dir = bases
            .iter_mut()
            .map(|base| base.join(&unique))
            .find(|dir| dir.as_os_str().len() + "/tmux-10000000/default".len() < MAX_SOCKET_PATH)
            .unwrap_or_else(|| PathBuf::from("/tmp").join(unique));
        fs::create_dir_all(&dir)
            .unwrap_or_else(|error| panic!("cannot create {}: {error}", dir.display()));
        DeskServer { dir }
    }

    /// One tmux client call on this server, with the socket directory set
    /// and no inherited session of the runner's own terminal.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("tmux");
        command
            .args(args)
            .env("TMUX_TMPDIR", &self.dir)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// Runs one tmux call that must succeed and returns its trimmed
    /// standard output.
    fn run(&self, args: &[&str]) -> String {
        let output = self.command(args).output().unwrap_or_else(|error| {
            panic!("cannot run tmux {}: {error}", args.first().unwrap_or(&""))
        });
        assert!(
            output.status.success(),
            "tmux {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// Runs one tmux call and says whether it succeeded.
    fn try_run(&self, args: &[&str]) -> bool {
        self.command(args)
            .output()
            .is_ok_and(|output| output.status.success())
    }

    /// Starts the server hermetically with one detached session whose
    /// single window runs `command`.
    fn start(&self, session: &str, window: &str, columns: u16, rows: u16, command: &str) {
        self.run(&[
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-x",
            &columns.to_string(),
            "-y",
            &rows.to_string(),
            "-s",
            session,
            "-n",
            window,
            command,
        ]);
    }

    /// The socket address of this server, as tmux itself reports it.
    fn socket(&self, session: &str) -> String {
        self.run(&["display-message", "-p", "-t", session, "#{socket_path}"])
    }

    /// The server process id behind one session.
    fn server_pid(&self, session: &str) -> String {
        self.run(&["display-message", "-p", "-t", session, "#{pid}"])
    }

    /// The pane identity a process inside `session` would carry.
    fn inside_pane(&self, session: &str) -> String {
        self.run(&[
            "list-panes",
            "-t",
            &format!("{session}:0"),
            "-F",
            "#{pane_id}",
        ])
    }

    /// One formatted line per pane of a window, in pane order.
    fn panes(&self, target: &str, format: &str) -> Vec<String> {
        self.run(&["list-panes", "-t", target, "-F", format])
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(String::from)
            .collect()
    }

    /// One formatted line per window of a session, in window order.
    fn windows(&self, session: &str, format: &str) -> Vec<String> {
        self.run(&["list-windows", "-t", session, "-F", format])
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(String::from)
            .collect()
    }

    /// One formatted line per window of every session, in name order.
    fn all_windows(&self, format: &str) -> Vec<String> {
        let mut lines = self
            .run(&["list-windows", "-a", "-F", format])
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(String::from)
            .collect::<Vec<String>>();
        lines.sort_unstable();
        lines
    }

    /// The complete global option table, for an unchanged comparison.
    fn global_options(&self) -> String {
        self.run(&["show-options", "-g"])
    }
}

impl Drop for DeskServer {
    fn drop(&mut self) {
        // The server owns the pane processes of the case, so it ends them
        // with itself; a server that never started is not an error here.
        let _ = self.try_run(&["kill-server"]);
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Writes one agent that records its process id and its exact argument
/// vector, then stays running until the server ends.
fn write_agent(dir: &Path, log: &Path) -> PathBuf {
    let script = dir.join("desk-agent.sh");
    let body = format!(
        "#!/bin/sh\n\
         printf 'pid=%s\\n' \"$$\" >> '{log}'\n\
         for argument in \"$@\"; do printf 'arg=%s\\n' \"$argument\" >> '{log}'; done\n\
         exec sleep 600\n",
        log = log.display(),
    );
    fs::write(&script, body)
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", script.display()));
    let mut permissions = fs::metadata(&script)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", script.display()))
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions)
        .unwrap_or_else(|error| panic!("cannot chmod {}: {error}", script.display()));
    script
}

/// Reads the agent log once it holds every expected line.
fn agent_log(log: &Path, expected: &[&str]) -> String {
    let deadline = Instant::now() + LIMIT;
    loop {
        let text = fs::read_to_string(log).unwrap_or_default();
        if expected.iter().all(|line| text.contains(line)) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "the agent did not record {expected:?}; the log holds: {text}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

/// Says whether a condition turns true within `limit`.
fn soon<F: FnMut() -> bool>(limit: Duration, mut condition: F) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// The child environment of one desk: the isolated variables, this
/// server's socket directory, and the session identity when the desk runs
/// inside tmux. The runner's own `TMUX` pair never leaks in.
fn desk_env(
    repo: &TestRepo,
    server: &DeskServer,
    inside: Option<(&str, &str)>,
) -> Vec<(String, String)> {
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
    vars.retain(|(key, _)| key != "TMUX" && key != "TMUX_PANE");
    vars.push(("TERM".to_string(), "xterm-256color".to_string()));
    vars.push(("TMUX_TMPDIR".to_string(), server.dir.display().to_string()));
    if let Some((tmux, pane)) = inside {
        vars.push(("TMUX".to_string(), tmux.to_string()));
        vars.push(("TMUX_PANE".to_string(), pane.to_string()));
    }
    vars
}

/// Spawns the desk of one case on a pseudo-terminal.
fn spawn_desk(
    repo: &TestRepo,
    server: &DeskServer,
    agent: &[&str],
    columns: u16,
    rows: u16,
    inside: Option<(&str, &str)>,
) -> Pty {
    let mut args = vec!["desk", "--"];
    args.extend(agent.iter().copied());
    Pty::spawn(
        Path::new(env!("CARGO_BIN_EXE_margin")),
        &args,
        &desk_env(repo, server, inside),
        repo.path(),
        columns,
        rows,
    )
    .expect("the desk spawns on a pseudo-terminal")
}

/// Detaches the attached desk session with the default prefix.
fn detach(pty: &mut Pty) {
    pty.send_key(b"\x02d");
}

/// The `TMUX` value of a process inside one session of this server.
fn inside_identity(server: &DeskServer, session: &str) -> (String, String) {
    let tmux = format!(
        "{},{},0",
        server.socket(session),
        server.server_pid(session),
    );
    let pane = server.inside_pane(session);
    (tmux, pane)
}

#[test]
fn outside_tmux_the_desk_composes_an_agent_pane_and_a_margin_pane() {
    if !tmux_present() {
        eprintln!("skipped: tmux is not installed");
        return;
    }
    let repo = TestRepo::init();
    let work = tempfile::TempDir::new().expect("a private work directory");
    let server = DeskServer::new("outside");
    let agent = write_agent(work.path(), &work.path().join("agent.log"));
    let log = work.path().join("agent.log");

    let mut pty = spawn_desk(
        &repo,
        &server,
        &[
            agent.to_str().unwrap(),
            "--flag",
            "value with spaces",
            "--",
            "$(id -u)",
            "a;rm -rf /nonexistent",
            "'quoted value'",
        ],
        170,
        45,
        None,
    );
    // The attached client draws the desk session's status line.
    assert!(
        pty.wait_for("0:agent", LIMIT),
        "the desk attached and selected the agent window"
    );
    detach(&mut pty);
    let status = pty
        .wait_exit(LIMIT)
        .expect("the desk finished after the detach");
    assert_eq!(status.code(), Some(0), "a composed desk succeeds");

    // The report names every surface, the queried prefix, and the exit
    // behavior (SPEC 12.6).
    assert!(pty.shows("margin desk: session margin"));
    assert!(pty.shows(&format!(
        "the agent pane runs {} --flag value with spaces --",
        agent.display()
    )));
    assert!(pty.shows(&format!(
        "the margin pane beside it runs {} tui",
        env!("CARGO_BIN_EXE_margin")
    )));
    assert!(pty.shows("tmux prefix: C-b (queried from the session"));
    assert!(pty.shows("detached; the session keeps running"));
    assert!(pty.shows("tmux attach-session -t margin"));
    assert!(pty.shows("quitting margin leaves the agent running"));

    // The agent received the explicit argv verbatim: one argument with
    // spaces stays one argument, and no shell reads the argv, so the
    // substitution, the semicolon, and the quotes arrive as text
    // (SPEC 12.6, direct execution).
    let recorded = agent_log(
        &log,
        &[
            "pid=",
            "arg=--flag",
            "arg=value with spaces",
            "arg=--",
            "arg=$(id -u)",
            "arg=a;rm -rf /nonexistent",
            "arg='quoted value'",
        ],
    );
    let agent_pid = recorded
        .lines()
        .find_map(|line| line.strip_prefix("pid="))
        .expect("the agent recorded its process id")
        .to_string();
    let arguments: Vec<&str> = recorded
        .lines()
        .filter_map(|line| line.strip_prefix("arg="))
        .collect();
    assert_eq!(
        arguments,
        [
            "--flag",
            "value with spaces",
            "--",
            "$(id -u)",
            "a;rm -rf /nonexistent",
            "'quoted value'",
        ],
        "the pane process argv is exactly the desk argument vector"
    );

    // The session holds one agent window split into the agent pane and
    // the Margin pane, and both processes stay running after the detach.
    let windows = server.windows("margin", "#{window_name}");
    assert_eq!(windows, ["agent"], "one dedicated agent window");
    assert!(soon(LIMIT, || {
        server.panes("margin:agent", "#{pane_id}").len() == 2
    }));
    let panes = server.panes(
        "margin:agent",
        "#{pane_pid} dead=#{pane_dead} command=#{pane_current_command} start=#{pane_start_command}",
    );
    assert!(
        panes.iter().any(|pane| pane.contains(&agent_pid)
            && pane.contains("dead=0")
            && pane.contains(&format!("start={}", agent.display()))),
        "the agent pane runs the recorded process: {panes:?}"
    );
    assert!(
        panes.iter().any(|pane| pane.contains("dead=0")
            && pane.contains(&format!("start={}", env!("CARGO_BIN_EXE_margin")))
            && pane.ends_with("tui")),
        "the margin pane runs this build's interactive surface: {panes:?}"
    );

    // The first words belong to the agent: its pane keeps the focus the
    // split moved to the new pane.
    let active = server.panes(
        "margin:agent",
        "active=#{pane_active} #{pane_start_command}",
    );
    assert!(
        active
            .iter()
            .any(|pane| pane.starts_with("active=1") && pane.contains(agent.to_str().unwrap())),
        "the agent pane keeps the focus: {active:?}"
    );

    // No tmux configuration was written anywhere (SPEC 12.6).
    assert!(
        !repo.env().home().join(".tmux.conf").exists(),
        "the desk writes no tmux configuration"
    );
}

#[test]
fn outside_tmux_a_narrow_desk_keeps_one_command_per_window() {
    if !tmux_present() {
        eprintln!("skipped: tmux is not installed");
        return;
    }
    let repo = TestRepo::init();
    let work = tempfile::TempDir::new().expect("a private work directory");
    let server = DeskServer::new("narrow");
    let agent = write_agent(work.path(), &work.path().join("agent.log"));

    let mut pty = spawn_desk(
        &repo,
        &server,
        &[agent.to_str().unwrap(), "narrow"],
        90,
        40,
        None,
    );
    assert!(
        pty.wait_for("0:agent", LIMIT),
        "the narrow desk attached and selected the agent window"
    );
    detach(&mut pty);
    let status = pty
        .wait_exit(LIMIT)
        .expect("the narrow desk finished after the detach");
    assert_eq!(status.code(), Some(0));

    // Below the desk width of 140 columns the arrangement is one command
    // per window, never two unreadable columns (SPEC 11.2, 12.6).
    assert!(pty.shows("margin desk: session margin"));
    assert!(pty.shows("in a window of its own, so a narrow desk stays readable"));
    let windows = server.windows("margin", "#{window_name} panes=#{window_panes}");
    assert_eq!(
        windows,
        ["agent panes=1", "margin panes=1"],
        "each surface owns a full-width window"
    );
    let widths = server.windows("margin", "#{window_name}=#{window_width}");
    assert_eq!(
        widths,
        ["agent=90", "margin=90"],
        "no pane is squeezed beside the other"
    );
}

#[test]
fn inside_tmux_the_desk_adds_one_window_and_touches_nothing_else() {
    if !tmux_present() {
        eprintln!("skipped: tmux is not installed");
        return;
    }
    let repo = TestRepo::init();
    let work = tempfile::TempDir::new().expect("a private work directory");
    let server = DeskServer::new("inside");
    let agent = write_agent(work.path(), &work.path().join("agent.log"));
    server.start("writer", "drafting", 170, 45, "sleep 600");
    server.start("unrelated", "keep", 100, 30, "sleep 600");
    // A prefix the desk must query instead of assuming the default
    // (SPEC 12.6).
    server.run(&["set-option", "-g", "prefix", "C-a"]);

    let before_windows = server.all_windows("#{session_name}:#{window_name} pane=#{pane_pid}");
    let before_options = server.global_options();
    let (tmux, pane) = inside_identity(&server, "writer");

    let mut pty = spawn_desk(
        &repo,
        &server,
        &[agent.to_str().unwrap(), "inside", "two words"],
        170,
        45,
        Some((&tmux, &pane)),
    );
    assert!(
        pty.wait_for(
            "margin desk: window \"margin-desk\" in session writer",
            LIMIT
        ),
        "the desk reported the window it added to this session"
    );
    let status = pty.wait_exit(LIMIT).expect("the inside desk returns");
    assert_eq!(status.code(), Some(0));

    assert!(pty.shows(&format!(
        "the agent pane runs {} inside two words; the margin pane beside it",
        agent.display()
    )));
    assert!(pty.shows("tmux prefix: C-a (queried from the session"));
    assert!(pty.shows("quitting margin leaves the agent running"));

    // The session keeps its own window and gains exactly one desk window
    // with both panes; the unrelated session is untouched.
    let after_windows = server.all_windows("#{session_name}:#{window_name} pane=#{pane_pid}");
    assert_eq!(
        after_windows.len(),
        before_windows.len() + 1,
        "exactly one new window exists: {after_windows:?} was {before_windows:?}"
    );
    for line in &before_windows {
        assert!(
            after_windows.contains(line),
            "an existing pane changed: {line} in {after_windows:?}"
        );
    }
    assert!(soon(LIMIT, || {
        server.panes("writer:margin-desk", "#{pane_id}").len() == 2
    }));
    let drafting = server.windows("writer", "#{window_name}");
    assert_eq!(
        drafting,
        ["drafting", "margin-desk"],
        "the writer's own window comes first and the desk window is clearly named"
    );

    // No global tmux option changed (SPEC 12.6).
    assert_eq!(
        server.global_options(),
        before_options,
        "the global option table is unchanged"
    );
    assert!(
        !repo.env().home().join(".tmux.conf").exists(),
        "the desk writes no tmux configuration"
    );
}

#[test]
fn inside_tmux_a_narrow_desk_puts_margin_in_its_own_window() {
    if !tmux_present() {
        eprintln!("skipped: tmux is not installed");
        return;
    }
    let repo = TestRepo::init();
    let work = tempfile::TempDir::new().expect("a private work directory");
    let server = DeskServer::new("narrow-inside");
    let agent = write_agent(work.path(), &work.path().join("agent.log"));
    server.start("writer", "drafting", 90, 40, "sleep 600");
    let before_windows = server.all_windows("#{session_name}:#{window_name} pane=#{pane_pid}");
    let (tmux, pane) = inside_identity(&server, "writer");

    let mut pty = spawn_desk(
        &repo,
        &server,
        &[agent.to_str().unwrap(), "narrow"],
        90,
        40,
        Some((&tmux, &pane)),
    );
    assert!(
        pty.wait_for(
            "margin desk: window \"margin-desk\" in session writer",
            LIMIT
        ),
        "the narrow inside desk reported its window"
    );
    let status = pty.wait_exit(LIMIT).expect("the inside desk returns");
    assert_eq!(status.code(), Some(0));
    assert!(pty.shows("in a window of its own, so a narrow desk stays readable"));

    let windows = server.windows("writer", "#{window_name} panes=#{window_panes}");
    assert_eq!(
        windows,
        [
            "drafting panes=1",
            "margin-desk panes=1",
            "margin-review panes=1"
        ],
        "a narrow desk keeps the agent and Margin as full-width windows"
    );
    let after_windows = server.all_windows("#{session_name}:#{window_name} pane=#{pane_pid}");
    for line in &before_windows {
        assert!(
            after_windows.contains(line),
            "an existing pane changed: {line} in {after_windows:?}"
        );
    }
}

#[test]
fn quitting_margin_leaves_the_agent_running() {
    if !tmux_present() {
        eprintln!("skipped: tmux is not installed");
        return;
    }
    let repo = TestRepo::init();
    let work = tempfile::TempDir::new().expect("a private work directory");
    let server = DeskServer::new("quit");
    let log = work.path().join("agent.log");
    let agent = write_agent(work.path(), &log);
    server.start("writer", "drafting", 170, 45, "sleep 600");
    server.start("unrelated", "keep", 100, 30, "sleep 600");
    let before_unrelated = server.all_windows("#{session_name}:#{window_name} pane=#{pane_pid}");
    let (tmux, pane) = inside_identity(&server, "writer");

    let mut pty = spawn_desk(
        &repo,
        &server,
        &[agent.to_str().unwrap(), "lasting"],
        170,
        45,
        Some((&tmux, &pane)),
    );
    let status = pty
        .wait_exit(LIMIT)
        .expect("the desk returns without attaching");
    assert_eq!(status.code(), Some(0));
    assert!(soon(LIMIT, || {
        server.panes("writer:margin-desk", "#{pane_id}").len() == 2
    }));
    let agent_pid = agent_log(&log, &["pid=", "arg=lasting"])
        .lines()
        .find_map(|line| line.strip_prefix("pid="))
        .expect("the agent recorded its process id")
        .to_string();

    // Quit the Margin surface the way a writer does: one `q` to the pane
    // that runs it. The desk owns neither the agent nor the session, so
    // both must survive the surface (SPEC 12.6).
    let panes = server.panes("writer:margin-desk", "#{pane_index} #{pane_start_command}");
    let margin_index = panes
        .iter()
        .find(|pane| pane.contains(env!("CARGO_BIN_EXE_margin")))
        .and_then(|pane| pane.split_whitespace().next())
        .expect("a margin pane exists")
        .to_string();
    server.run(&[
        "send-keys",
        "-t",
        &format!("writer:margin-desk.{margin_index}"),
        "q",
    ]);
    assert!(
        soon(LIMIT, || {
            server.panes("writer:margin-desk", "#{pane_id}").len() == 1
        }),
        "quitting margin closed only the margin pane"
    );

    let survivor = &server.panes(
        "writer:margin-desk",
        "#{pane_pid} dead=#{pane_dead} command=#{pane_current_command} start=#{pane_start_command}",
    )[0];
    assert!(
        survivor.contains(&agent_pid)
            && survivor.contains("dead=0")
            && survivor.contains("command=sleep")
            && survivor.contains(&format!("start={}", agent.display())),
        "the agent process still runs in its pane: {survivor}"
    );
    let drafting = server.windows("writer", "#{window_name}");
    assert_eq!(
        drafting,
        ["drafting", "margin-desk"],
        "the desk window stays for the agent"
    );
    let after_unrelated = server.all_windows("#{session_name}:#{window_name} pane=#{pane_pid}");
    for line in &before_unrelated {
        assert!(
            after_unrelated.contains(line),
            "an unrelated pane changed when margin quit: {line} in {after_unrelated:?}"
        );
    }
}

#[test]
fn a_new_desk_session_never_lands_on_an_existing_margin_session() {
    if !tmux_present() {
        eprintln!("skipped: tmux is not installed");
        return;
    }
    let repo = TestRepo::init();
    let work = tempfile::TempDir::new().expect("a private work directory");
    let server = DeskServer::new("second");
    let agent = write_agent(work.path(), &work.path().join("agent.log"));
    // A session the writer already owns under the desk's first name.
    server.start("margin", "mine", 170, 45, "sleep 600");
    let before = server.all_windows("#{session_name}:#{window_name} pane=#{pane_pid}");

    let mut pty = spawn_desk(&repo, &server, &[agent.to_str().unwrap()], 170, 45, None);
    assert!(
        pty.wait_for("0:agent", LIMIT),
        "the desk attached to its own session"
    );
    detach(&mut pty);
    assert!(pty.wait_exit(LIMIT).expect("the desk finished").code() == Some(0));
    assert!(
        pty.shows("margin desk: session margin-2"),
        "the desk names the session it picked"
    );

    let after = server.all_windows("#{session_name}:#{window_name} pane=#{pane_pid}");
    for line in &before {
        assert!(
            after.contains(line),
            "the existing session changed: {line} in {after:?}"
        );
    }
    assert_eq!(
        server.windows("margin", "#{window_name}"),
        ["mine"],
        "the writer's own session keeps its window"
    );
    assert_eq!(
        server.windows("margin-2", "#{window_name}"),
        ["agent"],
        "the desk built its session beside it"
    );
}

#[test]
fn an_agent_that_exits_at_once_leaves_nothing_behind() {
    if !tmux_present() {
        eprintln!("skipped: tmux is not installed");
        return;
    }
    let repo = TestRepo::init();
    let server = DeskServer::new("dead");

    let mut pty = spawn_desk(&repo, &server, &["/bin/false"], 170, 45, None);
    let status = pty
        .wait_exit(LIMIT)
        .expect("the desk reports the dead agent");
    assert_eq!(status.code(), Some(5), "an execution failure");
    assert!(
        pty.shows("the desk ended at once: the agent /bin/false did not stay running"),
        "the failure names the agent honestly"
    );
    assert!(
        pty.shows("nothing was written and nothing of the desk is left"),
        "the failure states the recovery truth"
    );
    assert!(
        !server.try_run(&["has-session", "-t", "margin"]),
        "no half-built desk session survives the failure"
    );
}

#[test]
fn missing_tmux_explains_the_two_tab_alternative() {
    let repo = TestRepo::init();
    let work = tempfile::TempDir::new().expect("a private work directory");
    // A path with the binary and nothing else, so tmux cannot be found.
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_margin"), work.path().join("margin"))
        .expect("a binary link");
    let mut vars: Vec<(String, String)> = vec![
        ("PATH".to_string(), work.path().display().to_string()),
        ("HOME".to_string(), repo.env().home().display().to_string()),
        ("TERM".to_string(), "xterm-256color".to_string()),
    ];
    vars.retain(|(key, _)| key != "TMUX" && key != "TMUX_PANE");
    let mut pty = Pty::spawn(
        Path::new(env!("CARGO_BIN_EXE_margin")),
        &["desk", "--", "claude"],
        &vars,
        repo.path(),
        120,
        35,
    )
    .expect("the desk spawns on a pseudo-terminal");

    let status = pty.wait_exit(LIMIT).expect("the refusal returns");
    assert_eq!(status.code(), Some(4), "an unmet operational precondition");
    assert!(
        pty.shows("OPERATION_UNAVAILABLE"),
        "the refusal is typed: {}",
        String::from_utf8_lossy(pty.raw_output())
    );
    assert!(
        pty.shows("tmux was not found"),
        "the refusal names the missing tool"
    );
    assert!(
        pty.shows("run margin tui in one terminal tab and the agent in another"),
        "the refusal explains the tab arrangement, so the core product stays reachable"
    );
}

#[test]
fn a_desk_without_a_terminal_refuses_typed() {
    let repo = TestRepo::init();
    let mut command = Command::new(env!("CARGO_BIN_EXE_margin"));
    command
        .args(["desk", "--", "claude"])
        .current_dir(repo.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, _) in std::env::vars_os() {
        if support::env::is_scrubbed(&key) {
            command.env_remove(&key);
        }
    }
    command.envs(repo.env().vars());
    let output: Output = command
        .output()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"));
    assert_eq!(output.status.code(), Some(4), "a desk needs the terminal");
    assert!(output.stdout.is_empty(), "the refusal is one stderr line");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("OPERATION_UNAVAILABLE"), "got: {stderr}");
    assert!(stderr.contains("needs a terminal"), "got: {stderr}");
}
