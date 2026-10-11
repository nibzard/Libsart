//! Buffer tests for the refresh slice (SPEC 5.3, 13.3, task T045).
//!
//! Every case drives the real shell against a real Git repository through
//! the shared operations, then renders into a ratatui test buffer. The
//! cases prove what the acceptance states: the worker answers inspection
//! jobs off the input loop while the shell keeps its keys; two-second
//! polling discovers new edits, a moved candidate, a changed scope, and
//! changed eligibility; a stale plan stays readable but cannot apply; and
//! one explicit refresh rebuilds the review, resets the unapplied
//! selections with a notice, and keeps the pinned context — the document,
//! its source anchor, and the reading position derived from it. The
//! independent oracle proves that polling saved nothing.

mod support;

use std::path::Path;
use std::process::Output;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use margin::app::Operations;
use margin::git::Git;
use margin::tui::{InspectionReply, RefreshWorker, Shell, TuiOptions, View};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use support::oracle;
use support::repo::TestRepo;

/// How long one worker answer may take in these tests.
const ANSWER_LIMIT: Duration = Duration::from_secs(10);

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

/// Adapter bound to `repo`'s working tree and isolated environment.
fn adapter_for(repo: &TestRepo) -> Git {
    Git::from_env(repo.env().vars().clone()).at(repo.path())
}

/// One key-press event.
fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

/// Renders one frame and returns its rows.
fn draw(shell: &mut Shell<'_>, columns: u16, rows: u16) -> Vec<String> {
    let mut terminal =
        Terminal::new(TestBackend::new(columns, rows)).expect("a test backend always initializes");
    terminal
        .draw(|frame| margin::tui::render::draw(shell, frame))
        .expect("a test backend never fails to draw");
    let buffer = terminal.backend().buffer();
    let width = buffer.area.width as usize;
    buffer
        .content
        .chunks(width)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// Joins rows for substring assertions.
fn joined(page: &[String]) -> String {
    page.join("\n")
}

/// One adopted repository: a start marker over `notes/` and one draft
/// checkpoint that edits one passage of `notes/a.md`.
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

/// One adopted repository whose pending review holds three groups in two
/// documents.
fn three_group_repo() -> TestRepo {
    let repo = TestRepo::init();
    repo.write_file(
        "notes/a.md",
        b"# One\n\nFirst passage.\n\nFiller one.\n\nFiller two.\n\nFiller three.\n\n\
           Filler four.\n\nSecond passage.\n",
    );
    repo.write_file("notes/b.md", b"# Two\n\nOther passage.\n");
    repo.commit_all("import");
    let output = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes/", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));
    repo.write_file(
        "notes/a.md",
        b"# One\n\nFirst passage, edited.\n\nFiller one.\n\nFiller two.\n\n\
           Filler three.\n\nFiller four.\n\nSecond passage, edited.\n",
    );
    repo.write_file("notes/b.md", b"# Two\n\nOther passage, edited.\n");
    let output = margin(
        repo.env(),
        repo.path(),
        &["checkpoint", "--message", "Edit three passages", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));
    repo
}

/// Runs one inspection of the shell's current generation on the worker
/// and delivers its answer, exactly as the event loop does every two
/// seconds (SPEC 13.3).
fn poll_once(worker: &mut RefreshWorker, shell: &mut Shell<'_>) {
    worker.request(shell.generation());
    let reply = worker
        .wait_for(ANSWER_LIMIT)
        .expect("one requested inspection answers");
    shell.observe_inspection(reply);
}

#[test]
fn polling_detects_new_working_edits_and_disables_application() {
    let repo = adopted_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    shell.handle_event(&key(KeyCode::Char('2')));
    let screen = joined(&draw(&mut shell, 110, 30));
    assert!(
        screen.contains("· Undecided"),
        "the pinned comparison is current"
    );

    // New edits arrive as working bytes no checkpoint captured (SPEC
    // 5.3): a dirty scoped file.
    repo.write_file("notes/a.md", b"# One\n\nFirst passage, edited again.\n");
    let before = oracle::snapshot(&repo);
    let mut worker = RefreshWorker::spawn(git.clone()).expect("the worker starts");

    poll_once(&mut worker, &mut shell);
    let screen = joined(&draw(&mut shell, 110, 30));
    assert!(screen.contains("Stale"), "every group relabels itself");
    assert!(
        screen.contains("New edits arrived. Refresh before applying."),
        "the SPEC 5.3 sentence shows without one key press",
    );
    assert!(
        screen.contains("1 scoped path holds working edits"),
        "the arrival names the dirty scoped file",
    );
    assert!(
        screen.contains("First passage, edited."),
        "the pinned text still reads"
    );

    // The keys refuse new decisions, and the finish refuses to open.
    shell.handle_event(&key(KeyCode::Char('K')));
    assert_eq!(
        shell.changes().map(|changes| changes.proposed_count()),
        Some(0),
        "no decision was recorded on the stale pair"
    );
    shell.handle_event(&key(KeyCode::Enter));
    assert!(
        shell.finish().is_none(),
        "a stale plan cannot open a finish, let alone apply"
    );

    // Polling discovered state and saved nothing (SPEC 13.3).
    worker.shutdown();
    before.assert_unchanged(&oracle::snapshot(&repo), "the polled surface");
}

#[test]
fn polling_detects_a_moved_candidate_without_a_key_press() {
    let repo = adopted_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    shell.handle_event(&key(KeyCode::Char('2')));
    assert!(
        joined(&draw(&mut shell, 110, 30)).contains("· Undecided"),
        "the pinned comparison is current"
    );

    // The agent's hook captures another draft, so the head moves past
    // the pinned candidate (SPEC 5.3).
    repo.write_file("notes/a.md", b"# One\n\nFirst passage, edited again.\n");
    let output = margin(
        repo.env(),
        repo.path(),
        &["checkpoint", "--message", "Later edits", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));
    let moved = oracle::head_oid(&repo).expect("the head stands");

    let mut worker = RefreshWorker::spawn(git.clone()).expect("the worker starts");
    poll_once(&mut worker, &mut shell);
    let screen = joined(&draw(&mut shell, 110, 30));
    assert!(screen.contains("Stale"), "the arrival relabels every group");
    assert!(
        screen.contains("New edits arrived. Refresh before applying."),
        "the SPEC 5.3 sentence shows",
    );
    assert!(
        screen.contains("the candidate moved to"),
        "the arrival names the moved candidate",
    );
    // The header summarizes the workspace the poll observed: the new
    // candidate stands in it.
    assert!(
        screen.contains(&moved[..7]),
        "the header states the observed head"
    );
    worker.shutdown();
}

#[test]
fn a_reply_from_a_superseded_generation_is_discarded() {
    let repo = adopted_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    shell.handle_event(&key(KeyCode::Char('2')));
    let live = shell.generation();

    // One inspection of a dirty scope, held back the way a slow worker
    // holds one: by the time it answers, the shell it describes may be
    // gone (SPEC 13.3).
    repo.write_file("notes/a.md", b"# One\n\nFirst passage, edited again.\n");
    let outcome = operations.inspect_workspace(&margin::app::InspectRequest {
        scope: None,
        path_filter: Vec::new(),
    });
    assert!(outcome.ok(), "the inspection succeeds over the dirty scope");

    shell.observe_inspection(InspectionReply {
        generation: live - 1,
        outcome,
    });
    assert!(
        joined(&draw(&mut shell, 110, 30)).contains("· Undecided"),
        "a superseded reply is discarded whole, arrival included"
    );
    assert_eq!(
        shell.generation(),
        live,
        "a discarded reply regenerates nothing"
    );

    // The same observation, asked for by the live generation, is exactly
    // the arrival it is.
    let outcome = operations.inspect_workspace(&margin::app::InspectRequest {
        scope: None,
        path_filter: Vec::new(),
    });
    shell.observe_inspection(InspectionReply {
        generation: live,
        outcome,
    });
    assert!(
        joined(&draw(&mut shell, 110, 30)).contains("Stale"),
        "the live reply marks the arrival"
    );
}

#[test]
fn explicit_refresh_rebuilds_the_review_and_resets_selections() {
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    shell.handle_event(&key(KeyCode::Char('2')));
    shell.handle_event(&key(KeyCode::Char('n')));
    shell.handle_event(&key(KeyCode::Char('K')));
    let screen = joined(&draw(&mut shell, 110, 30));
    assert!(screen.contains("1 kept"), "one selection stands");
    let stale_candidate = shell
        .changes()
        .map(|changes| changes.candidate_oid().to_string())
        .expect("the pinned comparison");

    // New edits arrive and the surface knows it (SPEC 5.3): the first
    // passage gains another edit, so the rebuilt review shows new bytes
    // in the group the viewport holds.
    repo.write_file(
        "notes/a.md",
        b"# One\n\nFirst passage, edited twice.\n\nFiller one.\n\nFiller two.\n\n\
           Filler three.\n\nFiller four.\n\nSecond passage, edited.\n",
    );
    let output = margin(
        repo.env(),
        repo.path(),
        &["checkpoint", "--message", "Later edits", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));
    shell.handle_event(&key(KeyCode::Char('p')));
    assert!(
        joined(&draw(&mut shell, 110, 30)).contains("Stale"),
        "the arrival relabeled the groups"
    );

    // One explicit refresh rebuilds the review (SPEC 5.3).
    shell.handle_event(&key(KeyCode::Char('R')));
    let changes = shell.changes().expect("the rebuilt comparison");
    assert_ne!(
        changes.candidate_oid(),
        stale_candidate,
        "the review rebuilt over the new candidate"
    );
    assert!(changes.stale().is_none(), "the rebuilt review is current");
    assert_eq!(changes.selection(), 0, "the group selection reset");
    assert_eq!(
        changes.proposed_count(),
        0,
        "the unapplied selections reset"
    );

    let screen = joined(&draw(&mut shell, 110, 30));
    assert!(!screen.contains("Stale"), "the stale label cleared");
    assert!(
        screen.contains("First passage, edited twice."),
        "the rebuilt review shows the new passages"
    );
    assert!(
        !screen.contains("First passage, edited."),
        "the pinned old passage is gone from the rebuilt review"
    );
    assert!(
        screen.contains("0 kept · 0 restored"),
        "the reset plan states itself"
    );
    let notice = shell.notice().unwrap_or_default();
    assert!(
        notice.contains("1 unapplied selection was reset"),
        "the notice explains the selection reset: {notice}"
    );
    assert!(
        notice.contains("selections do not carry over"),
        "the notice states the deferred selection migration: {notice}"
    );

    // The refreshed review can apply again: the finish opens.
    shell.handle_event(&key(KeyCode::Enter));
    assert!(
        shell.finish().is_some(),
        "the rebuilt review re-enables the finish"
    );
}

#[test]
fn refresh_preserves_the_reading_anchor_and_position() {
    let repo = TestRepo::init();
    let body: Vec<String> = (0..80)
        .map(|line| format!("Line {line} of the document."))
        .collect();
    let mut document = String::from("# Long\n\n");
    document.push_str(&body.join("\n"));
    document.push('\n');
    repo.write_file("notes/long.md", document.as_bytes());
    repo.commit_all("import");
    let output = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes/", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let options = TuiOptions {
        path: Some("notes/long.md".to_string()),
        view: View::Read,
        ..TuiOptions::default()
    };
    let mut shell = Shell::open(repo.path(), &operations, &options);
    // The event loop draws before it reads one key; the wrapped rows the
    // navigation derives from exist only after one prepared frame.
    draw(&mut shell, 110, 30);
    for _ in 0..24 {
        shell.handle_event(&key(KeyCode::Char('j')));
    }
    let moved = joined(&draw(&mut shell, 110, 30));
    let anchor = shell
        .current_anchor()
        .expect("the reading position rests on an anchor")
        .clone();
    let offset = shell.reading_offset();
    assert!(offset > 0, "the reader moved down the document");
    assert!(moved.contains(&format!("line {}", anchor.line)));

    // An external edit changes the bytes under the reader; one explicit
    // refresh reloads them without losing the pinned context (SPEC 5.3,
    // 5.4, 11.2).
    repo.write_file(
        "notes/long.md",
        format!("{document}Appended after the refresh.\n").as_bytes(),
    );
    shell.handle_event(&key(KeyCode::Char('R')));
    let refreshed = shell
        .current_anchor()
        .expect("the anchor survived the refresh")
        .clone();
    assert_eq!(
        refreshed.line, anchor.line,
        "the source anchor survived the refresh"
    );
    assert_eq!(refreshed.path, anchor.path);
    let screen = joined(&draw(&mut shell, 110, 30));
    assert!(
        screen.contains("no selection stood to reset"),
        "the refresh notice states what it did"
    );
    // One more key clears the notice; the context row then states the
    // anchor the refresh kept.
    shell.handle_event(&key(KeyCode::Char('1')));
    let screen = joined(&draw(&mut shell, 110, 30));
    assert!(
        screen.contains(&format!("line {}", anchor.line)),
        "the context row states the kept anchor"
    );
    assert_eq!(
        shell.reading_offset(),
        offset,
        "the reading position restored from the anchor"
    );
    assert!(
        screen.contains("Unsaved draft"),
        "the reloaded document states its new save state"
    );
}

#[test]
fn staged_changes_arrive_as_their_own_category() {
    let repo = adopted_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    shell.handle_event(&key(KeyCode::Char('2')));

    // One staged path outside the scope changes eligibility alone: the
    // scoped working content and the candidate hold still (SPEC 5.3).
    repo.write_file("outside.md", b"staged bytes\n");
    repo.git_ok(["add", "outside.md"]);
    let before = oracle::snapshot(&repo);

    let mut worker = RefreshWorker::spawn(git.clone()).expect("the worker starts");
    poll_once(&mut worker, &mut shell);
    let notice = shell.notice().unwrap_or_default();
    assert!(
        notice.contains("New edits arrived. Refresh before applying."),
        "the SPEC 5.3 sentence shows: {notice}"
    );
    assert!(
        notice.contains("staged changes:"),
        "the disabled finish explains its category: {notice}"
    );
    let screen = joined(&draw(&mut shell, 110, 30));
    assert!(screen.contains("Stale"), "every group relabels itself");
    shell.handle_event(&key(KeyCode::Enter));
    assert!(
        shell.finish().is_none(),
        "the disabled finish cannot open over the staged changes"
    );
    worker.shutdown();
    before.assert_unchanged(&oracle::snapshot(&repo), "the polled surface");
}

#[test]
fn a_failed_inspection_claims_no_arrival() {
    let repo = adopted_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    shell.handle_event(&key(KeyCode::Char('2')));

    // New working edits exist, but the worker's Git is gone: the query
    // fails, and a failed query cannot claim an arrival (SPEC 5.3).
    repo.write_file("notes/a.md", b"# One\n\nFirst passage, edited again.\n");
    let broken = Git::from_env(repo.env().vars().clone())
        .at(repo.path())
        .with_program("margin-no-such-git");
    let mut worker = RefreshWorker::spawn(broken).expect("the worker starts");
    poll_once(&mut worker, &mut shell);
    let screen = joined(&draw(&mut shell, 110, 30));
    assert!(
        screen.contains("· Undecided"),
        "a failed inspection claims nothing"
    );
    worker.shutdown();
}

#[test]
fn the_worker_coalesces_a_burst_into_two_inspections() {
    let repo = adopted_repo();
    let mut worker = RefreshWorker::spawn(adapter_for(&repo)).expect("the worker starts");

    // Five requests while one job runs: one runs, the newest waits, and
    // the other three are coalesced away (SPEC 13.3).
    for _ in 0..5 {
        worker.request(7);
    }
    let mut answers = 0;
    while worker.wait_for(ANSWER_LIMIT).is_some() {
        answers += 1;
        if answers > 3 {
            break;
        }
    }
    assert_eq!(
        answers, 2,
        "one running job plus one waiting job answered the burst"
    );
    worker.shutdown();
}
