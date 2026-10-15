//! Buffer and PTY tests for the hover and navigation plan
//! (docs/tui-hover-navigation.md).
//!
//! Every case drives the real shell against a real Git repository through
//! the shared operations, then renders into a ratatui test buffer — or,
//! for the preview lock, a real pseudo-terminal. The cases prove the
//! plan's acceptance checks: one rapid run of navigation steps loads
//! exactly the document it ends on while the content-dependent actions
//! state their reason and change nothing; one absorbed reload keeps the
//! hover by identity and gates the fallback exactly as one explicit
//! navigation does; the reading position survives a document round trip;
//! the History hover shows its bounded before/after; and one reload of
//! the same pinned review keeps the hovered change group by its ID.

mod support;

use std::path::Path;
use std::process::Output;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use margin::app::Operations;
use margin::git::Git;
use margin::tui::{RefreshWorker, Shell, TuiOptions, View, WatchJob};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use support::repo::TestRepo;

/// How long one worker answer may take in these tests.
const ANSWER_LIMIT: Duration = Duration::from_secs(10);

/// How long one PTY expectation may take.
const PTY_LIMIT: Duration = Duration::from_secs(10);

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

/// The distinctive word each test document carries: one document switch
/// must redraw whole words, because a real terminal emits only changed
/// cells, and a one-letter difference redraws one cell.
fn marker_for(name: &str) -> String {
    match name {
        "a" => "ALPHA".to_string(),
        "b" => "BRAVO".to_string(),
        "c" => "CHARLIE".to_string(),
        "d" => "DELTA".to_string(),
        other => other.to_uppercase(),
    }
}

/// One adopted repository over `names`, each document one distinctive
/// passage, plus one draft commit that edits every passage, so a pending
/// review with one group per document exists.
fn adopted_repo(names: &[&str]) -> TestRepo {
    let repo = TestRepo::init();
    for name in names {
        let marker = marker_for(name);
        repo.write_file(
            &format!("notes/{name}.md"),
            format!("# {name}\n\nOne {marker} line stands here.\n").as_bytes(),
        );
    }
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
    for name in names {
        let marker = marker_for(name);
        repo.write_file(
            &format!("notes/{name}.md"),
            format!("# {name}\n\nOne {marker} line stands edited.\n").as_bytes(),
        );
    }
    let output = margin(
        repo.env(),
        repo.path(),
        &["checkpoint", "--message", "Edit every passage", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));
    repo
}

/// Runs one inspection of the shell's current generation on the worker
/// and delivers its answer, exactly as the event loop does every two
/// seconds (SPEC 13.3).
fn poll_once(worker: &mut RefreshWorker, shell: &mut Shell<'_>) {
    worker.request(WatchJob {
        generation: shell.generation(),
        watch: shell.watch(),
    });
    let reply = worker
        .wait_for(ANSWER_LIMIT)
        .expect("one requested inspection answers");
    shell.observe_inspection(reply);
}

#[test]
fn rapid_navigation_loads_only_its_last_target_and_gates_content_actions() {
    let repo = adopted_repo(&["a", "b", "c"]);
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    draw(&mut shell, 110, 30);

    // Focus the document list and run one burst of steps: the hover
    // moves, the load defers, and the last settled content stays. The
    // burst passes every expanded heading row — each newly hovered
    // document injects its own — and clamps at the end.
    shell.handle_event(&key(KeyCode::Tab));
    for _ in 0..5 {
        shell.handle_event(&key(KeyCode::Char('j')));
    }
    assert_eq!(
        shell.selection(),
        2,
        "the burst passes the expanded headings and moves the hover to c",
    );
    assert_eq!(
        shell.document().map(|document| document.path.as_str()),
        Some("notes/a.md"),
        "the reading column keeps the last settled document",
    );
    assert!(
        shell.has_pending_preview(),
        "the target stands until one quiet pass loads it",
    );
    let page = draw(&mut shell, 110, 30);
    assert!(
        joined(&page).contains("notes/c.md is still opening"),
        "the pending target labels itself: {}",
        joined(&page),
    );

    // The content-dependent keys state the reason and change nothing:
    // `d`, `e`, and — in the Changes view — a rapid `n` then `r` leave
    // every decision where it stood.
    for code in [KeyCode::Char('d'), KeyCode::Char('e')] {
        shell.handle_event(&key(code));
        assert_eq!(
            shell.notice(),
            Some("notes/c.md is still opening; wait for it to settle"),
            "the gated key states the reason",
        );
    }
    shell.handle_event(&key(KeyCode::Char('2')));
    assert_eq!(shell.view(), View::Changes);
    shell.handle_event(&key(KeyCode::Char('n')));
    shell.handle_event(&key(KeyCode::Char('r')));
    assert!(
        shell
            .changes()
            .is_none_or(|changes| changes.proposed_count() == 0),
        "a rapid n then r changes no plan while the target stands",
    );
    assert_eq!(
        shell.notice(),
        Some("notes/c.md is still opening; wait for it to settle"),
    );

    // The action menu carries the same reason on every gated action.
    shell.handle_event(&key(KeyCode::Char(':')));
    let gated = [
        margin::tui::Action::KeepGroup,
        margin::tui::Action::RestoreGroup,
        margin::tui::Action::UndoDecision,
        margin::tui::Action::AddNote,
        margin::tui::Action::Details,
        margin::tui::Action::FinishReview,
        margin::tui::Action::EditDocument,
    ];
    for entry in shell.menu() {
        if gated.contains(&entry.action) {
            assert_eq!(
                entry.disabled.as_deref(),
                Some("notes/c.md is still opening; wait for it to settle"),
                "the menu states the reason on {:?}",
                entry.action,
            );
        }
    }

    // The quiet pass loads exactly the last target, and a fresh decision
    // lands on the passage the column then shows.
    shell.handle_event(&key(KeyCode::Esc));
    shell.settle_preview();
    assert!(!shell.has_pending_preview());
    assert_eq!(
        shell.document().map(|document| document.path.as_str()),
        Some("notes/c.md"),
        "the quiet pass loaded the last target only",
    );
    shell.handle_event(&key(KeyCode::Char('r')));
    assert_eq!(
        shell
            .changes()
            .expect("the comparison stands")
            .proposed_count(),
        1,
        "a fresh r after the settle targets the displayed group",
    );

    // A later target replaces an earlier one: only the newest loads.
    shell.handle_event(&key(KeyCode::Char('1')));
    shell.handle_event(&key(KeyCode::Tab));
    shell.handle_event(&key(KeyCode::Char('k')));
    shell.settle_preview();
    assert_eq!(
        shell.document().map(|document| document.path.as_str()),
        Some("notes/b.md"),
        "the newest target won",
    );
}

#[test]
fn an_absorbed_reload_keeps_the_hover_by_identity_and_gates_the_fallback() {
    let repo = adopted_repo(&["a", "b", "c", "d"]);
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    draw(&mut shell, 110, 30);

    // Hover `c`: one burst of steps past every expanded heading, then one
    // settle.
    shell.handle_event(&key(KeyCode::Tab));
    for _ in 0..5 {
        shell.handle_event(&key(KeyCode::Char('j')));
    }
    shell.settle_preview();
    assert_eq!(
        shell.document().map(|document| document.path.as_str()),
        Some("notes/c.md"),
    );

    // The mixed reload `[a,b,c,d] -> [b,bb,c]`: `c` survives, so it stays
    // hovered; nothing is pending, because the displayed content already
    // matches the hover.
    repo.write_file("notes/bb.md", b"# bb\n\nOne passage.\n");
    std::fs::remove_file(repo.file_path("notes/a.md")).unwrap();
    std::fs::remove_file(repo.file_path("notes/d.md")).unwrap();
    repo.commit_all("mixed reload");
    let mut worker = RefreshWorker::spawn(git.clone()).expect("the worker starts");
    poll_once(&mut worker, &mut shell);
    assert_eq!(
        shell.documents(),
        &[
            "notes/b.md".to_string(),
            "notes/bb.md".to_string(),
            "notes/c.md".to_string()
        ]
    );
    assert_eq!(
        shell.selection(),
        2,
        "the surviving hover stays by identity"
    );
    assert_eq!(
        shell.document().map(|document| document.path.as_str()),
        Some("notes/c.md"),
    );
    assert!(!shell.has_pending_preview());

    // Insertion only: the hover stays where it was.
    repo.write_file("notes/e.md", b"# e\n\nOne passage.\n");
    repo.commit_all("insert e");
    poll_once(&mut worker, &mut shell);
    assert_eq!(
        shell.document().map(|document| document.path.as_str()),
        Some("notes/c.md"),
    );
    assert_eq!(shell.selection(), 2, "an insertion never moves the hover");

    // Deletion only: the hover survives again.
    std::fs::remove_file(repo.file_path("notes/e.md")).unwrap();
    repo.commit_all("remove e");
    poll_once(&mut worker, &mut shell);
    assert_eq!(
        shell.document().map(|document| document.path.as_str()),
        Some("notes/c.md"),
    );

    // The hovered document disappears: the fallback obeys the
    // pending-preview rule — its content loads on the quiet pass, and the
    // content-dependent actions give their reason until then.
    std::fs::remove_file(repo.file_path("notes/c.md")).unwrap();
    repo.commit_all("remove c");
    poll_once(&mut worker, &mut shell);
    assert_eq!(
        shell.documents(),
        &["notes/b.md".to_string(), "notes/bb.md".to_string()]
    );
    assert_eq!(shell.selection(), 1, "the old index clamps to the new list");
    assert!(
        shell.has_pending_preview(),
        "the fallback selection is pending until it loads",
    );
    shell.handle_event(&key(KeyCode::Char('2')));
    shell.handle_event(&key(KeyCode::Char('r')));
    assert_eq!(
        shell.notice(),
        Some("notes/bb.md is still opening; wait for it to settle"),
    );
    assert!(
        shell
            .changes()
            .is_none_or(|changes| changes.proposed_count() == 0),
        "no decision lands beside unloaded content",
    );
    shell.settle_preview();
    assert_eq!(
        shell.document().map(|document| document.path.as_str()),
        Some("notes/bb.md"),
    );

    // An empty list clears the hover; the displayed document stays.
    std::fs::remove_file(repo.file_path("notes/b.md")).unwrap();
    std::fs::remove_file(repo.file_path("notes/bb.md")).unwrap();
    repo.commit_all("remove the rest");
    poll_once(&mut worker, &mut shell);
    assert!(shell.documents().is_empty(), "the list emptied");
    assert_eq!(shell.selection(), 0, "the hover is cleared");
    assert!(!shell.has_pending_preview());
    assert_eq!(
        shell.document().map(|document| document.path.as_str()),
        Some("notes/bb.md"),
        "the displayed document stays untouched",
    );
    worker.shutdown();
}

#[test]
fn reading_positions_survive_a_document_round_trip() {
    let repo = TestRepo::init();
    let mut long = String::from("# Long\n\nA long document.\n");
    for line in 1..=150 {
        long.push_str(&format!("Line {line} of the long document.\n"));
    }
    repo.write_file("notes/a.md", long.as_bytes());
    repo.write_file("notes/b.md", b"# Short\n\nA short passage.\n");
    repo.commit_all("import");
    let output = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes/", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    draw(&mut shell, 110, 30);

    // Read deep into `a`, leave for `b`, and come back: the position the
    // anchor names is the position the reader returns to (SPEC 5.4).
    for _ in 0..40 {
        shell.handle_event(&key(KeyCode::Char('j')));
    }
    let left_line = shell.current_anchor().map(|anchor| anchor.line);
    assert!(
        left_line.is_some_and(|line| line > 1),
        "the reader left the document deep inside it",
    );
    shell.handle_event(&key(KeyCode::Tab));
    shell.handle_event(&key(KeyCode::Left));
    shell.handle_event(&key(KeyCode::Char('j')));
    shell.settle_preview();
    assert_eq!(
        shell.document().map(|document| document.path.as_str()),
        Some("notes/b.md"),
    );
    assert_eq!(
        shell.current_anchor().map(|anchor| anchor.line),
        Some(1),
        "the fresh document starts at its first line",
    );
    shell.handle_event(&key(KeyCode::Char('k')));
    shell.settle_preview();
    assert_eq!(
        shell.document().map(|document| document.path.as_str()),
        Some("notes/a.md"),
    );
    assert_eq!(
        shell.current_anchor().map(|anchor| anchor.line),
        left_line,
        "the round trip restores the saved anchor",
    );
    draw(&mut shell, 110, 30);
    assert!(
        shell.reading_offset() > 0,
        "the viewport returns to the saved position",
    );
}

#[test]
fn history_hover_shows_the_bounded_before_and_after() {
    let repo = TestRepo::init();
    let mut edited = String::from("# One\n\nFirst passage, edited.\n");
    for number in 0..60 {
        edited.push_str(&format!("Changed filler line {number}.\n"));
    }
    repo.write_file("notes/a.md", b"# One\n\nFirst passage.\n");
    repo.commit_all("import");
    let output = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes/", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));
    repo.write_file("notes/a.md", edited.as_bytes());
    let output = margin(
        repo.env(),
        repo.path(),
        &["checkpoint", "--message", "Edit the document", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    shell.handle_event(&key(KeyCode::Char('3')));
    draw(&mut shell, 100, 30);

    // The hovered draft is the candidate itself, so the before/after is
    // empty and says so; the start revision below it holds the whole
    // first draft against the candidate.
    shell.settle_preview();
    let page = draw(&mut shell, 100, 30);
    assert!(
        joined(&page).contains("Identical to the current candidate"),
        "the candidate against itself states its identity: {}",
        joined(&page),
    );
    shell.handle_event(&key(KeyCode::Char('j')));
    shell.settle_preview();
    let page = draw(&mut shell, 100, 30);
    let screen = joined(&page);
    assert!(
        screen.contains("Against the current candidate"),
        "the hover excerpt stands under its revision: {screen}",
    );
    assert!(
        screen.contains("-First passage.") && screen.contains("+First passage, edited."),
        "the before/after lines show: {screen}",
    );
    assert!(
        screen.contains("+Changed filler line 0."),
        "the added line shows: {screen}",
    );
    assert!(
        screen.contains("the excerpt is bounded"),
        "the bound is stated: {screen}",
    );

    // The excerpt is a reading aid, never the preview: `Enter` still
    // opens the restoration preview that computes its own proof.
    shell.handle_event(&key(KeyCode::Enter));
    let page = draw(&mut shell, 100, 30);
    assert!(
        joined(&page).contains("Restore preview"),
        "Enter opens the full preview beside the excerpt",
    );
}

#[test]
fn one_refresh_keeps_the_hovered_group_by_identity() {
    let repo = adopted_repo(&["a", "b"]);
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    shell.handle_event(&key(KeyCode::Char('2')));
    draw(&mut shell, 110, 30);
    let changes = shell.changes().expect("the comparison stands");
    assert!(changes.groups().len() >= 2, "two groups exist to hover");
    let hovered = changes
        .selected_group()
        .map(|group| group.change_id.clone())
        .expect("a group is selected");

    // Move the hover, then refresh: the review is the same pinned B/C,
    // so the hovered group survives the reload by its change ID.
    shell.handle_event(&key(KeyCode::Char('n')));
    let moved = shell
        .changes()
        .and_then(|changes| changes.selected_group())
        .map(|group| group.change_id.clone())
        .expect("the hover moved");
    assert_ne!(hovered, moved, "the hover really moved");
    shell.handle_event(&key(KeyCode::Char('R')));
    let changes = shell.changes().expect("the reload kept the comparison");
    assert_eq!(
        changes
            .selected_group()
            .map(|group| group.change_id.as_str()),
        Some(moved.as_str()),
        "the same pinned review keeps its hover by identity",
    );
}

#[test]
fn the_document_list_keeps_one_total_order() {
    let repo = TestRepo::init();
    // Created out of byte order, mixed case and depth: the one total
    // order is ascending UTF-8 path bytes, so `B` sorts before `a`.
    for name in ["a.md", "Z.md", "B.md", "zz/x.md", "zz/A.md"] {
        repo.write_file(name, b"# One\n\nA passage.\n");
    }
    repo.commit_all("import");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    assert_eq!(
        shell.documents(),
        &[
            "B.md".to_string(),
            "Z.md".to_string(),
            "a.md".to_string(),
            "zz/A.md".to_string(),
            "zz/x.md".to_string()
        ],
        "the discovery lists ascending UTF-8 path bytes",
    );

    // The adopted scope keeps the same order through its own builder.
    let output = margin(repo.env(), repo.path(), &["init", "--scope", ".", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    assert_eq!(
        shell.documents(),
        &[
            "B.md".to_string(),
            "Z.md".to_string(),
            "a.md".to_string(),
            "zz/A.md".to_string(),
            "zz/x.md".to_string()
        ],
        "the adopted list keeps the one total order",
    );
}

#[test]
fn discovery_states_its_document_bound() {
    let repo = TestRepo::init();
    for number in 0..501 {
        repo.write_file(
            &format!("doc{number:03}.md"),
            format!("# Doc {number}\n\nOne passage.\n").as_bytes(),
        );
    }
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    assert_eq!(shell.documents().len(), 500, "the bound held");
    let page = draw(&mut shell, 110, 30);
    assert!(
        joined(&page).contains("First 500 documents"),
        "the bound is a first-class row: {}",
        joined(&page),
    );
}

#[test]
fn rapid_real_keys_load_only_the_settled_document() {
    let repo = adopted_repo(&["a", "b", "c"]);
    let mut envs: Vec<(String, String)> = repo
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
    envs.push(("TERM".to_string(), "xterm-256color".to_string()));
    let mut pty = support::pty::Pty::spawn(
        Path::new(env!("CARGO_BIN_EXE_margin")),
        &["tui"],
        &envs,
        repo.path(),
        110,
        30,
    )
    .expect("the surface spawns");
    assert!(
        pty.wait_for("One ALPHA line stands edited.", PTY_LIMIT),
        "the first document draws; raw:\n{}",
        support::pty::visible(pty.raw_output())
    );

    // One rapid burst over the whole list: keys queue faster than any
    // load between them, so the burst ends on the last document and the
    // quiet pass behind it loads that one. Key timing over a real
    // terminal is not deterministic, so the never-loaded guarantee is
    // proven by the buffer test; this case proves the real loop settles
    // the burst and draws the ended-on document.
    pty.send_key(b"\t");
    for _ in 0..12 {
        pty.send_key(b"j");
    }
    // The needle starts inside the marker: a real terminal redraws only
    // changed cells, and one cell of `CHARLIE` coincidentally equals the
    // `ALPHA` frame before it, so that cell never re-enters the byte
    // stream. The redrawn run behind it is contiguous and unique to the
    // settled document.
    assert!(
        pty.wait_for("RLIE line stands edited.", PTY_LIMIT),
        "the settled target draws; raw tail: {:?}",
        String::from_utf8_lossy(&pty.raw_output()[pty.raw_output().len().saturating_sub(200)..]),
    );
    pty.send_key(b"q");
    let status = pty.wait_exit(PTY_LIMIT);
    assert_eq!(status.map(|status| status.code()), Some(Some(0)));
}
