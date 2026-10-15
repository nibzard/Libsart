//! Buffer tests for the terminal shell and the Read view (SPEC 5, 11,
//! task T041).
//!
//! Every case drives the real shell against a real Git repository through
//! the shared operations, then renders into a ratatui test buffer. The
//! assertions read the buffer like a reader does: what the surface shows,
//! what it retains across views, and what it refuses to lose at small
//! sizes. The independent oracle proves the read-only claim: no key
//! sequence changes one repository byte.

mod support;

use std::path::Path;
use std::process::{Command, Output, Stdio};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use margin::app::Operations;
use margin::git::Git;
use margin::tui::{Shell, TuiOptions, View, render};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use support::oracle;
use support::repo::TestRepo;

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

/// Adapter bound to `repo`'s working tree and isolated environment.
fn adapter_for(repo: &TestRepo) -> Git {
    Git::from_env(repo.env().vars().clone()).at(repo.path())
}

/// One key-press event.
fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

/// One modified key-press event.
fn modified(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

/// Renders one frame and returns its rows.
fn draw(shell: &mut Shell<'_>, columns: u16, rows: u16) -> Vec<String> {
    let mut terminal = TestTerminal::new(columns, rows);
    terminal.render(shell);
    terminal.page()
}

/// One test terminal: a ratatui terminal over a test buffer.
struct TestTerminal {
    terminal: Terminal<TestBackend>,
}

impl TestTerminal {
    fn new(columns: u16, rows: u16) -> TestTerminal {
        TestTerminal {
            terminal: Terminal::new(TestBackend::new(columns, rows))
                .expect("a test backend always initializes"),
        }
    }

    fn render(&mut self, shell: &mut Shell<'_>) {
        self.terminal
            .draw(|frame| render::draw(shell, frame))
            .expect("a test backend never fails to draw");
    }

    fn resize(&mut self, columns: u16, rows: u16) {
        self.terminal
            .resize(ratatui::layout::Rect::new(0, 0, columns, rows))
            .expect("a test backend never fails to resize");
    }

    fn page(&self) -> Vec<String> {
        let buffer = self.terminal.backend().buffer();
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
}

/// Joins rows for substring assertions.
fn joined(page: &[String]) -> String {
    page.join("\n")
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

#[test]
fn before_adoption_opens_read_only_with_discovery_and_a_setup_action() {
    // A plain repository Margin never wrote to: the surface still opens,
    // shows the discovered folder and documents, and offers the setup
    // action (SPEC 5.1).
    let repo = TestRepo::init();
    repo.write_file("content/essay.md", b"# Essay\n\nA first draft.\n");
    repo.write_file("content/notes.md", b"# Notes\n");
    repo.commit_all("import");
    let before = oracle::snapshot(&repo);

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(screen.contains("repo"), "the discovered folder shows");
    assert!(
        screen.contains("Documents (2)"),
        "the discovered documents list"
    );
    assert!(
        screen.contains("content/essay.md"),
        "the first document opens"
    );
    assert!(screen.contains("A first draft."), "the working text reads");
    assert!(
        screen.contains("Unsaved draft"),
        "nothing is captured before adoption"
    );
    assert!(
        screen.contains("Set up Margin to preserve this folder"),
        "the setup action is offered"
    );
    assert!(
        screen.contains("margin init --scope content/") && screen.contains("--auto-checkpoint"),
        "the setup action names the discovered scope"
    );
    // Feed the keys this build answers; none of them may write.
    for event in [
        key(KeyCode::Char('1')),
        key(KeyCode::Char('2')),
        key(KeyCode::Char('3')),
        key(KeyCode::Tab),
        key(KeyCode::Char('j')),
        key(KeyCode::Char('k')),
    ] {
        shell.handle_event(&event);
    }
    shell.handle_event(&key(KeyCode::Char('q')));
    assert!(shell.should_quit(), "quit works before adoption");

    before.assert_unchanged(&oracle::snapshot(&repo), "the read-only surface");
}

#[test]
fn the_read_view_renders_a_table_as_an_aligned_grid() {
    // A plain folder is enough: the Read presentation only reads text
    // (SPEC 11.1). The grid keeps source anchors; source mode (`s`)
    // keeps the exact pipe rows.
    let base = tempfile::TempDir::new().expect("a temporary folder");
    let folder = base.path().join("writing");
    std::fs::create_dir_all(&folder).expect("a folder");
    std::fs::write(
        folder.join("numbers.md"),
        b"# Numbers\n\n| Feature | State |\n| --- | --- |\n| tables | aligned |\n| lists | prose |\n\nAfter the table.\n",
    )
    .expect("a document");

    let git = Git::from_env(std::env::vars_os()).at(&folder);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(&folder, &operations, &TuiOptions::default());
    let screen = joined(&draw(&mut shell, 100, 24));
    assert!(screen.contains("Feature │ State"), "cells align: {screen}");
    assert!(screen.contains("tables  │ aligned"), "{screen}");
    assert!(screen.contains("────────┼────────"), "the rule: {screen}");
    assert!(screen.contains("After the table."), "{screen}");

    shell.handle_event(&key(KeyCode::Char('s')));
    let source = joined(&draw(&mut shell, 100, 24));
    assert!(source.contains("| --- | --- |"), "exact pipes: {source}");
    assert!(!source.contains("─┼─"), "no grid in source mode: {source}");
    assert!(
        std::fs::read(folder.join("numbers.md")).expect("the document")
            == b"# Numbers\n\n| Feature | State |\n| --- | --- |\n| tables | aligned |\n| lists | prose |\n\nAfter the table.\n",
        "reading never rewrites the source"
    );
}

#[test]
fn a_plain_folder_without_a_repository_still_opens_read_only() {
    // First use often starts outside any repository (SPEC 5.1): the
    // surface opens on the discovered documents instead of failing.
    let base = tempfile::TempDir::new().expect("a temporary folder");
    let folder = base.path().join("writing");
    std::fs::create_dir_all(folder.join("drafts")).expect("a subfolder");
    std::fs::write(
        folder.join("drafts/essay.md"),
        b"# Essay\n\nOpening lines.\n",
    )
    .expect("a document");
    std::fs::write(folder.join("readme.txt"), b"not a document\n").expect("other bytes");

    let git = Git::from_env(std::env::vars_os()).at(&folder);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(&folder, &operations, &TuiOptions::default());
    let page = draw(&mut shell, 100, 24);
    let screen = joined(&page);
    assert!(
        screen.contains("Documents (1)"),
        "only Markdown documents list"
    );
    assert!(screen.contains("drafts/essay.md"));
    assert!(screen.contains("Opening lines."), "the working text reads");
    assert!(screen.contains("Unsaved draft"));
    assert!(
        screen.contains("margin init --scope drafts/"),
        "the setup action"
    );
    // The folder itself is untouched: discovery only read.
    assert!(folder.join("readme.txt").exists());
    assert!(
        !folder.join(".git").exists(),
        "nothing created a repository"
    );
}

#[test]
fn the_read_view_shows_the_document_with_saved_and_review_state() {
    let repo = adopted_repo();
    // One more working edit: the displayed document becomes a labeled
    // working preview while the scope keeps its review state.
    repo.write_file("notes/a.md", b"# One\n\nFirst passage, edited twice.\n");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());

    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    // The document and its sections read.
    assert!(screen.contains("notes/a.md"));
    assert!(screen.contains("# One"));
    assert!(screen.contains("First passage, edited twice."));
    // The saved state of this document, in words (SPEC 5.4, 11.4).
    assert!(screen.contains("Unsaved draft"));
    // The whole-scope states (SPEC 4.2, 5.2).
    assert!(
        screen.contains("1 unsaved document"),
        "the preservation state"
    );
    assert!(
        screen.contains("1 document changed since your last review"),
        "the review state"
    );
    assert!(screen.contains("main"), "the adopted branch");

    // The untouched document reads as saved: fold headings and move the document selection
    // through the visible list.
    shell.handle_event(&key(KeyCode::Tab));
    shell.handle_event(&key(KeyCode::Left));
    shell.handle_event(&key(KeyCode::Char('j')));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(screen.contains("notes/b.md"));
    assert!(screen.contains("Saved"), "an unchanged document is saved");
    assert!(!screen.contains("Unsaved draft"));
}

#[test]
fn document_and_source_context_survive_view_switches() {
    let repo = adopted_repo();
    let options = TuiOptions {
        path: Some("notes/b.md".to_string()),
        view: View::Read,
        ..TuiOptions::default()
    };
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &options);
    // Scroll inside the document first, so the anchor is not line 1.
    for _ in 0..2 {
        shell.handle_event(&key(KeyCode::Char('j')));
    }
    let anchor_before = shell.current_anchor().unwrap().clone();
    assert_eq!(anchor_before.path, "notes/b.md");

    for view in [View::Changes, View::History, View::Read] {
        let code = match view {
            View::Read => KeyCode::Char('1'),
            View::Changes => KeyCode::Char('2'),
            View::History => KeyCode::Char('3'),
        };
        shell.handle_event(&key(code));
        assert_eq!(shell.view(), view);
        let page = draw(&mut shell, 110, 30);
        let screen = joined(&page);
        assert!(
            screen.contains("notes/b.md"),
            "{:?} keeps the document context",
            view
        );
        assert_eq!(
            shell.current_anchor().unwrap().path,
            "notes/b.md",
            "{:?} keeps the source anchor",
            view
        );
    }
    assert_eq!(
        shell.current_anchor().unwrap().line,
        anchor_before.line,
        "returning to Read restores the reading position"
    );

    // The Changes view summarizes the current comparison (SPEC 5.2, 7.2).
    shell.handle_event(&key(KeyCode::Char('2')));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(screen.contains("Since last review:"));
    assert!(screen.contains("notes/a.md"));
    // The History view lists the timeline in ancestry order (SPEC 5.5).
    shell.handle_event(&key(KeyCode::Char('3')));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(
        screen.contains("import"),
        "the original document version shows"
    );
    assert!(
        !screen.contains("Edit the first passage"),
        "history excludes changes to other documents"
    );
}

#[test]
fn unusual_content_states_label_themselves_honestly() {
    // An opaque document cannot pair its stored bytes with a working
    // preview, so the Read view says the save state is unknown instead of
    // guessing (SPEC 6.3, 11.4).
    let repo = adopted_repo();
    repo.write_file("notes/data.md", &[0xff, 0xfe, 0x00, 0x01]);
    let output = margin(
        repo.env(),
        repo.path(),
        &["checkpoint", "--message", "Capture opaque bytes", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let options = TuiOptions {
        path: Some("notes/data.md".to_string()),
        view: View::Read,
        ..TuiOptions::default()
    };
    let mut shell = Shell::open(repo.path(), &operations, &options);
    let page = draw(&mut shell, 100, 24);
    let screen = joined(&page);
    assert!(screen.contains("notes/data.md"));
    assert!(
        screen.contains("not valid UTF-8"),
        "the content states itself"
    );
    assert!(screen.contains("Save state unknown"));

    // A deleted working file stays distinct from an empty one: the saved
    // candidate still reads, with the deletion stated (SPEC 5.5).
    std::fs::remove_file(repo.file_path("notes/b.md")).expect("the working file");
    let options = TuiOptions {
        path: Some("notes/b.md".to_string()),
        view: View::Read,
        ..TuiOptions::default()
    };
    let mut shell = Shell::open(repo.path(), &operations, &options);
    let page = draw(&mut shell, 100, 24);
    let screen = joined(&page);
    assert!(screen.contains("notes/b.md"));
    assert!(
        screen.contains("Second passage."),
        "the saved candidate reads"
    );
    assert!(screen.contains("working file gone"));
}

#[test]
fn the_layout_earns_its_width_class() {
    let repo = adopted_repo();

    // At least 100 columns: the narrow document list plus the column.
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    let page = draw(&mut shell, 120, 30);
    let screen = joined(&page);
    assert!(screen.contains("Documents (2)"));
    assert!(screen.contains("notes/a.md"));

    // 80 to 99 columns: one document surface, no list (SPEC 11.2).
    let page = draw(&mut shell, 90, 30);
    let screen = joined(&page);
    assert!(!screen.contains("Documents (2)"));
    assert!(screen.contains("notes/a.md"));
    assert!(screen.contains("First passage"));

    // Below 80 columns: compact surface, reduced metadata, no lost keys.
    let page = draw(&mut shell, 70, 24);
    let screen = joined(&page);
    assert!(!screen.contains("Documents (2)"));
    assert!(screen.contains("notes/a.md"));
    assert!(screen.contains("q quit"), "quit stays reachable");
    assert!(
        screen.contains("1 changed since last review"),
        "the compact review state stays stated"
    );

    // The reading measure never exceeds 76 cells (SPEC 11.1). At the
    // single-surface width the reading column is the only body content,
    // so the trimmed rows are exactly the measure.
    let page = draw(&mut shell, 90, 40);
    let body = &page[1..page.len() - 2];
    let longest = body
        .iter()
        .map(|row| row.trim().chars().count())
        .max()
        .unwrap_or(0);
    assert!(
        longest <= 76,
        "the reading column measured {longest} cells, not at most 76"
    );
}

#[test]
fn below_the_useful_minimum_the_surface_explains_and_keeps_quit() {
    let repo = adopted_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());

    // Too narrow.
    let page = draw(&mut shell, 50, 24);
    let screen = joined(&page);
    assert!(screen.contains("Terminal too small"));
    assert!(screen.contains("at least 60 columns and 16 rows"));
    assert!(screen.contains("50 columns and 24 rows"));
    assert!(screen.contains("press q to quit"));

    // Too short.
    let page = draw(&mut shell, 90, 12);
    let screen = joined(&page);
    assert!(screen.contains("Terminal too small"));
    assert!(screen.contains("90 columns and 12 rows"));

    // Quit still works while the message shows (SPEC 11.2).
    shell.handle_event(&key(KeyCode::Char('q')));
    assert!(shell.should_quit());
}

#[test]
fn a_resize_restores_the_reading_position_from_the_anchor() {
    let repo = TestRepo::init();
    let long_text = format!(
        "# Long reading\n\n{}\n",
        (0..40)
            .map(|number| format!("Paragraph {number} of the long document."))
            .collect::<Vec<_>>()
            .join("\n\n")
    );
    repo.write_file("notes/long.md", long_text.as_bytes());
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
    let mut terminal = TestTerminal::new(100, 30);
    terminal.render(&mut shell);
    // Read ahead: the anchor moves off line 1.
    for _ in 0..12 {
        shell.handle_event(&key(KeyCode::Char('j')));
    }
    terminal.render(&mut shell);
    let anchor = shell.current_anchor().unwrap().clone();
    let page = terminal.page();
    // Row 0 is the header, row 1 the document title; row 2 is the top of
    // the reading column.
    let before_top = page[2].clone();
    assert!(anchor.line > 1, "the anchor moved with the reading");

    // A resize re-wraps at the new width and restores the position from
    // the anchor, not from a rendered row number (SPEC 11.2).
    terminal.resize(64, 30);
    shell.handle_event(&Event::Resize(64, 30));
    terminal.render(&mut shell);
    let restored = shell.current_anchor().unwrap().clone();
    let page = terminal.page();
    let head_of = |row: &str, count: usize| {
        row.char_indices()
            .nth(count)
            .map(|(index, _)| index)
            .unwrap_or(row.len())
    };
    assert!(
        page[2].starts_with(&before_top[..head_of(&before_top, 10)]),
        "the passage {before_top:?} stayed at the top after the resize, not {:?}",
        page[2]
    );
    assert_eq!(restored.path, anchor.path);
    assert_eq!(restored.line, anchor.line);
    assert_eq!(shell.document().unwrap().path, "notes/long.md");
}

#[test]
fn navigation_keys_change_no_repository_bytes() {
    let repo = adopted_repo();
    let before = oracle::snapshot(&repo);
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    let events = [
        Event::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 3,
            row: 4,
            modifiers: KeyModifiers::NONE,
        }),
        Event::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            column: 3,
            row: 4,
            modifiers: KeyModifiers::NONE,
        }),
        key(KeyCode::Char('1')),
        key(KeyCode::Char('2')),
        key(KeyCode::Char('3')),
        key(KeyCode::Tab),
        key(KeyCode::Char('j')),
        key(KeyCode::Char('j')),
        key(KeyCode::Char('k')),
        key(KeyCode::PageDown),
        key(KeyCode::PageUp),
        key(KeyCode::Down),
        key(KeyCode::Up),
        key(KeyCode::BackTab),
        modified(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ];
    for event in events {
        shell.handle_event(&event);
        draw(&mut shell, 100, 30);
    }
    assert!(shell.should_quit());
    before.assert_unchanged(&oracle::snapshot(&repo), "the navigation keys");
}

#[test]
fn the_command_line_refuses_redirected_streams_with_a_typed_error() {
    let repo = adopted_repo();
    let before = oracle::snapshot(&repo);
    let output = margin(repo.env(), repo.path(), &["tui"]);
    assert_eq!(output.status.code(), Some(4), "an unmet precondition");
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("OPERATION_UNAVAILABLE"));
    assert!(
        stderr.contains("terminal"),
        "the refusal names what is missing"
    );
    before.assert_unchanged(&oracle::snapshot(&repo), "the refused launch");
}

#[test]
fn an_invalid_initial_document_is_a_typed_refusal() {
    let repo = adopted_repo();
    let output = margin(repo.env(), repo.path(), &["tui", "--path", "../escape.md"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("INVALID_ARGUMENT"));
}

#[test]
fn outline_jumps_to_source_lines_survives_resize_and_never_writes() {
    let repo = TestRepo::init();
    repo.write_file("guide.md", b"<!-- source note -->\n# Guide\n\nIntro\n\n## Brewing\n\nCoffee\n\n### Recipes\n\nRecipe text\n");
    repo.commit_all("guide");
    let before = oracle::snapshot(&repo);
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    let screen = joined(&draw(&mut shell, 120, 30));
    assert!(!screen.contains("source note"));
    assert!(screen.contains("Brewing"));
    assert!(screen.contains("[1 Read]"));
    shell.handle_event(&key(KeyCode::Tab));
    shell.handle_event(&key(KeyCode::Char('j'))); // document title
    shell.handle_event(&key(KeyCode::Char('j'))); // Brewing
    draw(&mut shell, 120, 30);
    assert_eq!(shell.current_anchor().unwrap().line, 6);
    assert_eq!(shell.current_section().as_deref(), Some("Brewing"));
    shell.handle_event(&key(KeyCode::Enter));
    assert_eq!(shell.focus(), margin::tui::Focus::Reading);
    draw(&mut shell, 70, 20);
    assert_eq!(shell.current_anchor().unwrap().line, 6);
    shell.handle_event(&key(KeyCode::Char('[')));
    let screen = joined(&draw(&mut shell, 120, 30));
    assert!(!screen.contains("Documents ("));
    assert_eq!(shell.current_anchor().unwrap().line, 6);
    shell.handle_event(&key(KeyCode::Char('s')));
    draw(&mut shell, 120, 30);
    assert!(shell.reading_source());
    assert_eq!(shell.current_anchor().unwrap().line, 6);
    let after = oracle::snapshot(&repo);
    assert_eq!(before, after, "outline and source navigation are read-only");
}

#[test]
fn remote_navigation_preserves_review_selections_and_failure_preserves_document() {
    let repo = adopted_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    shell.handle_event(&key(KeyCode::Char('2')));
    shell.handle_event(&key(KeyCode::Char('K')));
    let proposed = shell.changes().unwrap().proposed_count();
    assert!(proposed > 0);
    shell
        .open_remote(&margin::desk::Request {
            path: "notes/b.md".into(),
            line: Some(3),
        })
        .unwrap();
    let page = joined(&draw(&mut shell, 110, 32));
    assert!(page.contains("Second passage"));
    assert!(shell.reading_source());
    assert_eq!(shell.current_anchor().unwrap().line, 3);
    assert_eq!(shell.changes().unwrap().proposed_count(), proposed);
    let before = shell.document().unwrap().path.clone();
    assert!(
        shell
            .open_remote_until(
                &margin::desk::Request {
                    path: "notes/a.md".into(),
                    line: None
                },
                Some(std::time::Instant::now())
            )
            .unwrap_err()
            .contains("expired")
    );
    assert_eq!(shell.document().unwrap().path, before);

    assert!(
        shell
            .open_remote(&margin::desk::Request {
                path: "outside.md".into(),
                line: None
            })
            .is_err()
    );
    assert_eq!(shell.document().unwrap().path, before);
    shell.handle_event(&key(KeyCode::Char('2')));
    shell.handle_event(&key(KeyCode::Enter));
    assert!(shell.finish().is_some());
    assert!(
        shell
            .open_remote(&margin::desk::Request {
                path: "notes/a.md".into(),
                line: None
            })
            .unwrap_err()
            .contains("busy")
    );
    assert_eq!(shell.document().unwrap().path, before);
}

#[test]
fn desk_paths_are_repository_relative_when_launched_from_a_subdirectory() {
    let repo = adopted_repo();
    let git = Git::from_env(repo.env().vars().clone()).at(repo.path().join("notes"));
    let operations = Operations::new(&git);
    let mut shell = Shell::open(
        &repo.path().join("notes"),
        &operations,
        &TuiOptions::default(),
    );
    shell
        .open_remote(&margin::desk::Request {
            path: "./notes//b.md".into(),
            line: None,
        })
        .unwrap();
    assert!(joined(&draw(&mut shell, 110, 32)).contains("Second passage"));
    assert_eq!(shell.document().unwrap().path, "notes/b.md");
}
