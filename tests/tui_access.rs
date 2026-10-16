//! Buffer tests for accessible keyboard and terminal behavior (SPEC 11,
//! task T046).
//!
//! Every case drives the real shell against a real Git repository
//! through the shared operations, then renders into a ratatui test
//! buffer. The cases prove what the acceptance states: the keymap's new
//! keys work, the action menu lists every action with visible disabled
//! reasons, focus cycles through navigation, document, and visible
//! actions and shows itself, text entry takes precedence over the
//! keymap, bracketed paste is literal text inside a field and ignored
//! with a notice outside, color-off rendering keeps every state in words
//! and symbols, and the reduced-update mode redraws only on change. The
//! editor handoff is proven here at its boundary — the request the shell
//! issues and the reload it absorbs — while the terminal lifecycle tests
//! in `tui_lifecycle.rs` prove the modes themselves.

mod support;

use std::path::Path;
use std::process::Output;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use margin::app::Operations;
use margin::git::Git;
use margin::tui::{Action, ColorMode, Focus, Overlay, Shell, TuiOptions, View, render};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};
use support::oracle;
use support::repo::TestRepo;

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

/// One modified key-press event.
fn modified(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

/// Renders one frame and returns its rows.
fn draw(shell: &mut Shell<'_>, columns: u16, rows: u16) -> Vec<String> {
    let mut terminal =
        Terminal::new(TestBackend::new(columns, rows)).expect("a test backend always initializes");
    terminal
        .draw(|frame| render::draw(shell, frame))
        .expect("a test backend never fails to draw");
    page_of(terminal.backend().buffer())
}

/// The rows of one buffer, trimmed.
fn page_of(buffer: &ratatui::buffer::Buffer) -> Vec<String> {
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

/// The foreground color and modifier of every cell run that renders
/// `needle`, so one case asserts the hue a rendered passage carries even
/// beside another column on the same screen row (SPEC 11.4).
fn styles_of(buffer: &ratatui::buffer::Buffer, needle: &str) -> Vec<(Option<Color>, Modifier)> {
    let width = buffer.area.width as usize;
    let mut found = Vec::new();
    for row in buffer.content.chunks(width) {
        let text: String = row.iter().map(|cell| cell.symbol()).collect();
        let mut from = 0;
        while let Some(at) = text[from..].find(needle) {
            let chars = text[..from + at].chars().count();
            let cell = &row[chars];
            found.push((Some(cell.fg), cell.modifier));
            from = from + at + needle.len();
        }
    }
    found
}

/// Joins rows for substring assertions.
fn joined(page: &[String]) -> String {
    page.join("\n")
}

/// One adopted repository whose pending review holds three groups in two
/// documents, with distinctive words to search for.
fn three_group_repo() -> TestRepo {
    let repo = TestRepo::init();
    repo.write_file(
        "notes/a.md",
        b"# One\n\nFirst passage.\n\nFiller one.\n\nFiller two.\n\nFiller three.\n\n\
           Filler four.\n\nSecond passage.\n",
    );
    repo.write_file("notes/b.md", b"# Two\n\nOther passage.\n");
    repo.write_file("notes/z.md", b"# Zed\n\nA passage about forests.\n");
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

/// Opens the shell over `repo` at the Changes view with the first group
/// visited.
fn changes_shell<'a>(repo: &TestRepo, operations: &'a Operations<'a>) -> Shell<'a> {
    let options = TuiOptions {
        view: View::Changes,
        ..TuiOptions::default()
    };
    Shell::open(repo.path(), operations, &options)
}

#[test]
fn the_action_menu_lists_every_action_with_bindings_and_disabled_reasons() {
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = changes_shell(&repo, &operations);

    // Outside the Changes view, its actions state why they cannot run.
    shell.handle_event(&key(KeyCode::Char('1')));
    shell.handle_event(&key(KeyCode::Char(':')));
    let page = draw(&mut shell, 110, 130);
    let screen = joined(&page);
    assert!(screen.contains("Action menu"), "the menu opens with :");
    // Every action of the contract is listed with its binding.
    for action in Action::ALL.into_iter().filter(|action| action.executable()) {
        assert!(
            screen.contains(action.name()),
            "the menu lists {:?} as {}",
            action,
            action.name(),
        );
    }
    assert!(screen.contains("Tab / Shift-Tab · Cycle focus"));
    assert!(screen.contains("PgUp / PgDn · Page"));
    assert!(screen.contains("q / Ctrl-C · Quit"));
    // A disabled action carries its reason in words (SPEC 11.3).
    assert!(
        screen.contains("K · Keep selected group — unavailable: the Changes view only"),
        "a view-scoped action states why it cannot run: {screen}",
    );
    assert!(
        screen.contains("E · Open external editor — unavailable: no editor is configured"),
        "the editor action states the unconfigured editor",
    );
    assert!(screen.contains("Enter · Finish review — unavailable: the Changes view only"),);

    // In the Changes view over a real review, the same actions enable.
    shell.handle_event(&key(KeyCode::Esc));
    shell.handle_event(&key(KeyCode::Char('2')));
    let page = draw(&mut shell, 110, 130);
    let screen = joined(&page);
    assert!(!screen.contains("K · Keep selected group — unavailable"));
    assert!(!screen.contains("Enter · Finish review — unavailable"));

    // Esc closes the menu; nothing ran.
    shell.handle_event(&key(KeyCode::Char(':')));
    shell.handle_event(&key(KeyCode::Esc));
    let page = draw(&mut shell, 110, 130);
    let screen = joined(&page);
    assert!(!screen.contains("Action menu"));
    assert!(screen.contains("Palette closed; nothing ran"));
}

#[test]
fn the_menu_runs_the_selected_action_and_refuses_disabled_ones() {
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = changes_shell(&repo, &operations);

    // Move to the Finish review entry and run it: the menu activation and
    // the Enter binding open the same summary (SPEC 11.3).
    shell.handle_event(&key(KeyCode::Char(':')));
    wait_palette(&mut shell);
    let finish = shell
        .palette()
        .hits
        .iter()
        .position(|hit| hit.entry.action == Some(Action::FinishReview))
        .expect("the finish entry exists");
    for _ in 0..finish {
        shell.handle_event(&key(KeyCode::Down));
    }
    let page = draw(&mut shell, 110, 40);
    assert!(
        joined(&page).contains("› Action · Enter · Finish review"),
        "the selected entry shows its marker"
    );
    wait_palette(&mut shell);
    shell.handle_event(&key(KeyCode::Enter));
    let page = draw(&mut shell, 110, 40);
    let screen = joined(&page);
    assert!(
        screen.contains("Keep remaining changes and finish"),
        "the menu's Enter opened the finish summary: {screen}",
    );

    // A disabled entry refuses with its reason and runs nothing.
    shell.handle_event(&key(KeyCode::Esc));
    shell.handle_event(&key(KeyCode::Char('1')));
    shell.handle_event(&key(KeyCode::Char(':')));
    wait_palette(&mut shell);
    let keep = shell
        .palette()
        .hits
        .iter()
        .position(|hit| hit.entry.action == Some(Action::KeepGroup))
        .expect("the keep entry exists");
    for _ in 0..keep {
        shell.handle_event(&key(KeyCode::Down));
    }
    wait_palette(&mut shell);
    shell.handle_event(&key(KeyCode::Enter));
    let page = draw(&mut shell, 110, 40);
    let screen = joined(&page);
    assert!(
        screen.contains("unavailable: the Changes view only"),
        "the refusal carries the reason",
    );
    assert!(
        shell
            .changes()
            .is_none_or(|changes| changes.proposed_count() == 0),
        "the disabled entry recorded no decision",
    );
}

#[test]
fn focus_cycles_three_stops_both_ways_and_shows_itself() {
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());

    // At a list width, Tab walks the document list and the reading
    // column, and the footer names the focused element in words (SPEC
    // 11.3, 11.4).
    let page = draw(&mut shell, 110, 30);
    assert!(joined(&page).contains("focus reading"));
    shell.handle_event(&key(KeyCode::Tab));
    assert_eq!(shell.focus(), margin::tui::Focus::Documents);
    let page = draw(&mut shell, 110, 30);
    assert!(joined(&page).contains("focus document list"));
    shell.handle_event(&key(KeyCode::Tab));
    assert_eq!(shell.focus(), margin::tui::Focus::Reading);

    // Shift-Tab cycles backward.
    shell.handle_event(&key(KeyCode::BackTab));
    assert_eq!(shell.focus(), margin::tui::Focus::Documents);

    // With the menu standing, the actions are a third stop; opening the
    // menu focuses them.
    shell.handle_event(&key(KeyCode::Char(':')));
    assert_eq!(shell.focus(), margin::tui::Focus::Actions);
    let page = draw(&mut shell, 110, 30);
    assert!(joined(&page).contains("focus action menu"));
    // The searchable palette owns input focus until it closes.
    shell.handle_event(&key(KeyCode::Tab));
    shell.handle_event(&key(KeyCode::BackTab));
    assert_eq!(shell.focus(), margin::tui::Focus::Actions);
    shell.handle_event(&key(KeyCode::Char('j')));
    assert_eq!(shell.palette().query, "j");
    shell.handle_event(&key(KeyCode::Esc));
    assert_eq!(shell.focus(), margin::tui::Focus::Documents);
}

#[test]
fn text_entry_takes_precedence_over_the_keymap() {
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = changes_shell(&repo, &operations);
    draw(&mut shell, 110, 40);

    // `/` opens the search field of the current view (SPEC 11.3).
    shell.handle_event(&key(KeyCode::Char('/')));
    let page = draw(&mut shell, 110, 40);
    let screen = joined(&page);
    assert!(screen.contains("search: "), "the field shows");
    assert!(screen.contains("Enter finds the next match"));

    // Ordinary characters — including q, r, s, K, and : — are text
    // (SPEC 11.3): no view switch, no decision, no menu, no finish.
    for raw in ['q', 'r', 's', ':', 'K', '2'] {
        shell.handle_event(&key(KeyCode::Char(raw)));
    }
    let page = draw(&mut shell, 110, 40);
    let screen = joined(&page);
    assert!(
        screen.contains("search: qrs:K2"),
        "the typed text lands in the field: {screen}",
    );
    assert_eq!(shell.view(), View::Changes, "no view key ran");
    assert!(!screen.contains("Action menu"), "the menu key was text");
    assert!(
        shell
            .changes()
            .is_none_or(|changes| changes.proposed_count() == 0),
        "no decision key ran",
    );
    assert!(shell.finish().is_none(), "Enter has not been pressed");

    // Enter submits the field, never a review (SPEC 11.3).
    shell.handle_event(&key(KeyCode::Enter));
    assert!(shell.finish().is_none(), "Enter submitted the search");
    let page = draw(&mut shell, 110, 40);
    let screen = joined(&page);
    assert!(screen.contains("search: no match for qrs:K2"));

    // A real query moves within the view without changing content.
    let before = oracle::snapshot(&repo);
    shell.handle_event(&key(KeyCode::Char('/')));
    for raw in ['S', 'e', 'c', 'o', 'n', 'd'] {
        shell.handle_event(&key(KeyCode::Char(raw)));
    }
    shell.handle_event(&key(KeyCode::Enter));
    let page = draw(&mut shell, 110, 40);
    let screen = joined(&page);
    assert!(
        screen.contains("search: match at group"),
        "the query found the group whose passages say Second: {screen}",
    );
    before.assert_unchanged(&oracle::snapshot(&repo), "the search keys");

    // Esc leaves text entry; the keymap answers again.
    shell.handle_event(&key(KeyCode::Char('/')));
    shell.handle_event(&key(KeyCode::Esc));
    let page = draw(&mut shell, 110, 40);
    assert!(joined(&page).contains("search closed"));
    shell.handle_event(&key(KeyCode::Char('1')));
    assert_eq!(shell.view(), View::Read, "the view keys answer again");
}

#[test]
fn ctrl_c_keeps_the_clean_quit_inside_text_entry() {
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = changes_shell(&repo, &operations);
    draw(&mut shell, 110, 40);

    shell.handle_event(&key(KeyCode::Char('/')));
    shell.handle_event(&modified(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(shell.should_quit(), "Ctrl-C still quits inside the field");

    // A second shell proves the unapplied-selection warning still stands:
    // one decision, then Ctrl-C warns, the second Ctrl-C quits.
    let mut shell = changes_shell(&repo, &operations);
    draw(&mut shell, 110, 40);
    shell.handle_event(&key(KeyCode::Char('K')));
    assert_eq!(
        shell.changes().map(|changes| changes.proposed_count()),
        Some(1)
    );
    shell.handle_event(&key(KeyCode::Char('/')));
    shell.handle_event(&modified(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(
        shell.should_quit(),
        "Ctrl-C flushes progress inside text entry"
    );
    assert!(
        shell
            .notice()
            .unwrap()
            .contains("saved locally and unapplied")
    );
}

#[test]
fn bracketed_paste_is_text_inside_a_field_and_ignored_outside() {
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = changes_shell(&repo, &operations);
    draw(&mut shell, 110, 40);

    // Outside text entry, one paste is ignored with a concise notice and
    // cannot trigger a sequence of global actions (SPEC 11.3).
    shell.handle_event(&Event::Paste("K:qSecond".to_string()));
    let page = draw(&mut shell, 110, 40);
    let screen = joined(&page);
    assert!(
        screen.contains("pasted text is ignored outside text entry"),
        "the notice is concise and stated: {screen}",
    );
    assert_eq!(shell.view(), View::Changes, "the pasted : did nothing");
    assert!(
        shell
            .changes()
            .is_none_or(|changes| changes.proposed_count() == 0),
        "the pasted K recorded nothing",
    );
    assert!(!shell.should_quit(), "the pasted q did not quit");

    // Inside the search field, the same paste is literal text.
    shell.handle_event(&key(KeyCode::Char('/')));
    shell.handle_event(&Event::Paste("K:qSecond".to_string()));
    let page = draw(&mut shell, 110, 40);
    let screen = joined(&page);
    assert!(
        screen.contains("search: K:qSecond"),
        "the paste is literal text in the field: {screen}",
    );
    shell.handle_event(&key(KeyCode::Enter));
    let page = draw(&mut shell, 110, 40);
    assert!(
        joined(&page).contains("search: no match for K:qSecond"),
        "the submitted paste searched as one literal query: {}",
        joined(&page),
    );
}

#[test]
fn search_moves_within_every_view_without_changing_content() {
    let repo = three_group_repo();
    // One long uncaptured document: a match deep in it must scroll the
    // reading column, not only relabel a notice.
    let long_text = format!(
        "# Long reading\n\n{}\n\nA passage about forests.\n\nOne paragraph below.\n",
        (0..40)
            .map(|number| format!("Filler paragraph {number} of the long document."))
            .collect::<Vec<_>>()
            .join("\n\n")
    );
    repo.write_file("notes/long.md", long_text.as_bytes());
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let before = oracle::snapshot(&repo);
    let mut shell = Shell::open(
        repo.path(),
        &operations,
        &TuiOptions {
            path: Some("notes/long.md".to_string()),
            ..TuiOptions::default()
        },
    );

    // Read view: the query moves the reading position to the matching
    // line (SPEC 11.3 `/`). The long document scrolls, so the position
    // moves with it.
    draw(&mut shell, 90, 30);
    shell.handle_event(&key(KeyCode::Char('/')));
    for raw in ['f', 'o', 'r', 'e', 's', 't', 's'] {
        shell.handle_event(&key(KeyCode::Char(raw)));
    }
    shell.handle_event(&key(KeyCode::Enter));
    let page = draw(&mut shell, 90, 30);
    let screen = joined(&page);
    assert!(
        screen.contains("search: match at line 83"),
        "the Read view search states its match: {screen}",
    );
    assert!(
        shell.reading_offset() > 0,
        "the viewport scrolled to the match",
    );
    assert!(
        screen.contains("A passage about forests."),
        "the match itself is on screen: {screen}",
    );

    // History is document-scoped: select a captured document before searching.
    shell.handle_event(&key(KeyCode::Char('f')));
    shell.handle_event(&Event::Paste("notes/a.md".into()));
    shell.handle_event(&key(KeyCode::Enter));
    // The event loop settles one navigation target on the quiet pass
    // after its key: no input waits here, so the picker's target loads
    // before the view switches.
    shell.settle_preview();
    // History view: the same key searches the timeline's subjects.
    shell.handle_event(&key(KeyCode::Char('3')));
    draw(&mut shell, 90, 30);
    shell.handle_event(&key(KeyCode::Char('/')));
    for raw in ['p', 'a', 's', 's', 'a', 'g', 'e', 's'] {
        shell.handle_event(&key(KeyCode::Char(raw)));
    }
    shell.handle_event(&key(KeyCode::Enter));
    let page = draw(&mut shell, 90, 30);
    let screen = joined(&page);
    assert!(
        screen.contains("search: match at revision"),
        "the History search states its match: {screen}",
    );

    // No match states itself and moves nothing.
    shell.handle_event(&key(KeyCode::Char('/')));
    shell.handle_event(&key(KeyCode::Char('z')));
    shell.handle_event(&key(KeyCode::Char('z')));
    shell.handle_event(&key(KeyCode::Enter));
    let page = draw(&mut shell, 90, 30);
    assert!(
        joined(&page).contains("search: no match for zz"),
        "the empty result is stated",
    );
    before.assert_unchanged(&oracle::snapshot(&repo), "the search keys");
}

fn wait_for_workspace_search(shell: &mut Shell<'_>) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while shell.workspace_search().searching() {
        shell.poll_workspace_search();
        assert!(
            std::time::Instant::now() < deadline,
            "workspace search timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(shell.workspace_search().results.is_some());
}

#[test]
fn workspace_search_finds_listed_working_source_and_opens_exact_lines() {
    let repo = three_group_repo();
    // Raw Markdown/comment matches, literal punctuation, and untracked
    // working files must be searchable even when no rendered row matches.
    let query = "Quartz [x].* qrs:";
    repo.write_file(
        "notes/b.md",
        format!("# Two\n\n<!-- {query} -->\n").as_bytes(),
    );
    repo.write_file(
        "notes/new.md",
        format!("# New\n\n{} {query}\n", "filler ".repeat(30)).as_bytes(),
    );
    repo.write_file("outside.md", format!("{query}\n").as_bytes());
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    let before = oracle::snapshot(&repo);
    let original_path = shell.document().unwrap().path.clone();
    shell.handle_event(&modified(KeyCode::Char('f'), KeyModifiers::CONTROL));
    shell.handle_event(&Event::Paste(query.to_lowercase()));
    assert!(!shell.should_quit());
    assert_eq!(shell.document().unwrap().path, original_path);
    shell.handle_event(&key(KeyCode::Enter));
    wait_for_workspace_search(&mut shell);
    let hits = &shell.workspace_search().results.as_ref().unwrap().hits;
    assert_eq!(hits.len(), 2);
    assert_eq!((&*hits[0].path, hits[0].line), ("notes/b.md", 3));
    assert_eq!((&*hits[1].path, hits[1].line), ("notes/new.md", 3));
    let screen = joined(&draw(&mut shell, 110, 30));
    assert!(screen.contains("Search all documents"), "{screen}");
    assert!(screen.contains("notes/b.md:3"), "{screen}");
    assert!(screen.contains("Quartz [x].* qrs:"), "{screen}");
    // Opening the comment proves that source mode makes hidden hits visible.
    shell.handle_event(&key(KeyCode::Enter));
    draw(&mut shell, 70, 24);
    assert!(shell.reading_source());
    assert_eq!(shell.document().unwrap().path, "notes/b.md");
    assert_eq!(shell.current_anchor().unwrap().line, 3);
    shell.handle_event(&modified(KeyCode::Char('f'), KeyModifiers::CONTROL));
    assert_eq!(shell.workspace_search().query, query.to_lowercase());
    assert!(shell.workspace_search().results.is_some());
    shell.handle_event(&key(KeyCode::Down));
    shell.handle_event(&key(KeyCode::Enter));
    draw(&mut shell, 70, 24);
    assert_eq!(shell.document().unwrap().path, "notes/new.md");
    assert_eq!(shell.current_anchor().unwrap().line, 3);
    draw(&mut shell, 110, 30);
    assert_eq!(shell.current_anchor().unwrap().line, 3);
    before.assert_unchanged(&oracle::snapshot(&repo), "workspace search and navigation");
}

#[test]
fn workspace_search_discards_cancelled_queries_and_revalidates_hits() {
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    let original_path = shell.document().unwrap().path.clone();
    shell.handle_event(&modified(KeyCode::Char('f'), KeyModifiers::CONTROL));
    shell.handle_event(&Event::Paste("forests".into()));
    shell.handle_event(&key(KeyCode::Enter));
    shell.handle_event(&key(KeyCode::Esc));
    shell.poll_workspace_search();
    assert!(shell.overlay().is_none());
    assert!(shell.workspace_search().results.is_none());
    // Reopening remembers the query and starts a new generation. Editing
    // it immediately invalidates that scan; only the newest query may win.
    shell.handle_event(&modified(KeyCode::Char('f'), KeyModifiers::CONTROL));
    for _ in 0.."forests".len() {
        shell.handle_event(&key(KeyCode::Backspace));
    }
    shell.handle_event(&Event::Paste("Other passage".into()));
    shell.handle_event(&key(KeyCode::Enter));
    wait_for_workspace_search(&mut shell);
    let hits = &shell.workspace_search().results.as_ref().unwrap().hits;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, "notes/b.md");
    // A changed file cannot send the reader to a now-unrelated line.
    repo.write_file("notes/b.md", b"# Two\n\nReplaced on disk.\n");
    shell.handle_event(&key(KeyCode::Enter));
    assert_eq!(shell.document().unwrap().path, original_path);
    assert!(shell.workspace_search().results.is_none());
    assert!(shell.notice().unwrap().contains("changed on disk"));
    shell.handle_event(&key(KeyCode::Enter));
    wait_for_workspace_search(&mut shell);
    assert!(
        shell
            .workspace_search()
            .results
            .as_ref()
            .unwrap()
            .hits
            .is_empty()
    );
    assert!(joined(&draw(&mut shell, 70, 24)).contains("No matches in the searched text"));
}

#[test]
fn the_document_picker_finds_and_opens_documents() {
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    draw(&mut shell, 110, 30);

    // `f` opens the picker (SPEC 11.3); typing filters, and the filter
    // text obeys text-entry precedence — even the letters of other
    // commands.
    shell.handle_event(&key(KeyCode::Char('f')));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(screen.contains("Document picker"));
    assert!(screen.contains("3 of 3 documents match"));
    for raw in ['q', 'z'] {
        shell.handle_event(&key(KeyCode::Char(raw)));
    }
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(
        screen.contains("find: qz▏ · 0 documents match"),
        "the filter states its matches: {screen}",
    );
    shell.handle_event(&key(KeyCode::Backspace));
    shell.handle_event(&key(KeyCode::Backspace));

    // A filter that matches opens the first match on Enter: the target
    // stands while no quiet pass ran, then loads on it.
    for raw in ['b', '.', 'm', 'd'] {
        shell.handle_event(&key(KeyCode::Char(raw)));
    }
    shell.handle_event(&key(KeyCode::Enter));
    assert!(
        shell.has_pending_preview(),
        "Enter names its target before the quiet pass loads it",
    );
    shell.settle_preview();
    assert_eq!(
        shell.document().map(|document| document.path.as_str()),
        Some("notes/b.md"),
        "the quiet pass opened the first match",
    );
    let page = draw(&mut shell, 110, 30);
    assert!(
        joined(&page).contains("opening notes/b.md"),
        "the opening is stated",
    );

    // Esc closes the picker without opening anything.
    shell.handle_event(&key(KeyCode::Char('f')));
    shell.handle_event(&key(KeyCode::Esc));
    let page = draw(&mut shell, 110, 30);
    assert!(
        joined(&page).contains("the document picker closed"),
        "the closing is stated",
    );
}

#[test]
fn page_keys_follow_focus_and_move_one_viewport() {
    // `PgUp` and `PgDn` move one viewport, less one held row, in the
    // focused element: the document list pages the selection, and the
    // reading column pages its rows (SPEC 11.3).
    let repo = TestRepo::init();
    let mut first = String::from("# First\n\nA long document.\n");
    for line in 1..=150 {
        first.push_str(&format!("Line {line} of the long document.\n"));
    }
    repo.write_file("notes/a01.md", first.as_bytes());
    for number in 2..=30 {
        let path = format!("notes/a{number:02}.md");
        let body = format!("# Document {number}\n\nA short passage.\n");
        repo.write_file(&path, body.as_bytes());
    }
    repo.commit_all("import");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    // One draw at 110x30 prepares the frame: the reading column and the
    // list share 26 rows, so one page is 25 outline entries.
    draw(&mut shell, 110, 30);

    // Focus the document list: one page moves the selection by 26 rows
    // and keeps the selected document loaded.
    shell.handle_event(&key(KeyCode::Tab));
    assert_eq!(shell.focus(), Focus::Documents);
    shell.handle_event(&key(KeyCode::PageDown));
    assert_eq!(
        shell.selection(),
        24,
        "one page includes the expanded heading"
    );
    shell.handle_event(&key(KeyCode::PageDown));
    assert_eq!(
        shell.selection(),
        29,
        "a second page clamps at the last document",
    );
    shell.handle_event(&key(KeyCode::PageUp));
    assert_eq!(shell.selection(), 4, "one page up moves 25 outline entries");
    shell.handle_event(&key(KeyCode::PageUp));
    assert_eq!(shell.selection(), 0, "a second page up clamps at the first");

    // Focus the reading column: the same keys page its rows by the same
    // measure, less one held row. One draw first wraps the reloaded
    // document, so the page has its rows.
    draw(&mut shell, 110, 30);
    shell.handle_event(&key(KeyCode::Tab));
    assert_eq!(shell.focus(), Focus::Reading);
    assert_eq!(shell.reading_offset(), 0);
    shell.handle_event(&key(KeyCode::PageDown));
    assert_eq!(shell.reading_offset(), 25, "one page scrolls 25 rows");
    shell.handle_event(&key(KeyCode::PageUp));
    assert_eq!(shell.reading_offset(), 0, "one page up returns to the top");

    // The key list names the paging keys (SPEC 11.3).
    shell.handle_event(&key(KeyCode::Char('?')));
    let page = draw(&mut shell, 110, 30);
    assert!(
        joined(&page).contains("PgUp / PgDn · Page"),
        "the help lists the paging keys: {}",
        joined(&page),
    );
}

#[test]
fn help_lists_the_keys_and_the_current_blockers() {
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = changes_shell(&repo, &operations);

    // `?` lists the contextual key list and the current blockers (SPEC
    // 11.3).
    shell.handle_event(&key(KeyCode::Char('?')));
    let page = draw(&mut shell, 110, 40);
    let screen = joined(&page);
    assert!(screen.contains("Margin keys"));
    assert!(screen.contains("Tab / Shift-Tab · Cycle focus"));
    assert!(screen.contains("PgUp / PgDn · Page"));
    assert!(screen.contains(": · Action menu"));
    assert!(screen.contains("/ · Search within current view"));
    assert!(screen.contains("Ctrl+F · Search all documents"));
    assert!(screen.contains("Current blockers"));
    assert!(
        screen.contains("none"),
        "an unblocked review states that no blocker stands",
    );

    // One external commit makes the pinned comparison stale; the help
    // list states the blocker in words.
    repo.write_file("notes/a.md", b"# One\n\nFirst passage, arrived.\n");
    repo.commit_all("an external edit");
    shell.handle_event(&key(KeyCode::Char('j')));
    let page = draw(&mut shell, 110, 40);
    let screen = joined(&page);
    assert!(
        screen.contains("New edits arrived. Refresh before applying."),
        "the stale blocker is listed: {screen}",
    );

    // Esc closes help.
    shell.handle_event(&key(KeyCode::Esc));
    let page = draw(&mut shell, 110, 40);
    assert!(!joined(&page).contains("Margin keys"));
}

#[test]
fn color_off_renders_every_state_in_words_and_symbols() {
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let options = TuiOptions {
        view: View::Changes,
        color: ColorMode::Never,
        ..TuiOptions::default()
    };
    let mut shell = Shell::open(repo.path(), &operations, &options);

    // Decide one group each way: kept, restored, and undecided all show
    // at once (SPEC 11.4).
    shell.handle_event(&key(KeyCode::Char('K')));
    shell.handle_event(&key(KeyCode::Char('n')));
    shell.handle_event(&key(KeyCode::Char('r')));
    let mut terminal =
        Terminal::new(TestBackend::new(110, 60)).expect("a test backend always initializes");
    terminal
        .draw(|frame| render::draw(&mut shell, frame))
        .expect("a test backend never fails to draw");
    let screen = joined(&page_of(terminal.backend().buffer()));
    assert!(screen.contains("Kept"), "the kept state is a word");
    assert!(screen.contains("Restored"), "the restored state is a word");
    assert!(
        screen.contains("Undecided"),
        "the undecided state is a word"
    );

    // The source view states additions and deletions with symbols, not
    // hue (SPEC 11.4).
    shell.handle_event(&key(KeyCode::Char('s')));
    terminal
        .draw(|frame| render::draw(&mut shell, frame))
        .expect("a test backend never fails to draw");
    let screen = joined(&page_of(terminal.backend().buffer()));
    assert!(
        screen.contains("-First passage."),
        "a deletion carries its sign"
    );
    assert!(
        screen.contains("+First passage, edited."),
        "an addition carries its sign"
    );

    // Color-off mode draws no decoration at all: no cell carries a
    // modifier, so nothing depends on attributes to be understood.
    for cell in &terminal.backend().buffer().content {
        assert!(
            cell.modifier == Modifier::empty(),
            "color-off mode drew the modifier {:?}",
            cell.modifier,
        );
    }

    // A stale arrival relabels every group in words under color-off too.
    repo.write_file("notes/a.md", b"# One\n\nFirst passage, arrived.\n");
    repo.commit_all("an external edit");
    shell.handle_event(&key(KeyCode::Char('j')));
    terminal
        .draw(|frame| render::draw(&mut shell, frame))
        .expect("a test backend never fails to draw");
    let screen = joined(&page_of(terminal.backend().buffer()));
    assert!(screen.contains("Stale"), "the stale state is a word");

    // The Read view keeps its saved-state labels in words as well.
    shell.handle_event(&key(KeyCode::Char('1')));
    terminal
        .draw(|frame| render::draw(&mut shell, frame))
        .expect("a test backend never fails to draw");
    let screen = joined(&page_of(terminal.backend().buffer()));
    assert!(
        screen.contains("Unsaved draft") || screen.contains("Saved"),
        "the saved state stays a word under color-off",
    );
}

#[test]
fn color_on_marks_headings_and_both_diff_sides_with_hue() {
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let options = TuiOptions {
        view: View::Changes,
        color: ColorMode::Always,
        ..TuiOptions::default()
    };
    let mut shell = Shell::open(repo.path(), &operations, &options);
    let mut terminal =
        Terminal::new(TestBackend::new(110, 60)).expect("a test backend always initializes");

    // The source view states both sides with signs; the hue only
    // reinforces the signs and the words (SPEC 11.4).
    shell.handle_event(&key(KeyCode::Char('s')));
    terminal
        .draw(|frame| render::draw(&mut shell, frame))
        .expect("a test backend never fails to draw");
    let buffer = terminal.backend().buffer();
    let styles = styles_of(buffer, "Group 1 of");
    assert_eq!(
        styles.as_slice(),
        [(Some(Color::Blue), Modifier::BOLD | Modifier::REVERSED)],
        "the selected group is highlighted"
    );
    let styles = styles_of(buffer, "-First passage.");
    assert_eq!(
        styles.as_slice(),
        [(Some(Color::Red), Modifier::empty())],
        "the before side draws red"
    );
    let styles = styles_of(buffer, "+First passage, edited.");
    assert_eq!(
        styles.as_slice(),
        [(Some(Color::Green), Modifier::empty())],
        "the after side draws green"
    );

    // The Read view draws one Markdown heading in the heading role; the
    // `#` marker itself stays visible. The section breadcrumb names the
    // same heading in dim chrome, so only the body occurrence carries
    // the heading role.
    shell.handle_event(&key(KeyCode::Char('1')));
    terminal
        .draw(|frame| render::draw(&mut shell, frame))
        .expect("a test backend never fails to draw");
    let buffer = terminal.backend().buffer();
    let styles = styles_of(buffer, "# One");
    assert!(
        styles.contains(&(Some(Color::Blue), Modifier::BOLD)),
        "a Markdown heading draws as a heading, but the styles were {styles:?}"
    );
}

#[test]
fn reduced_updates_redraw_only_when_the_frame_changed() {
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let options = TuiOptions {
        view: View::Changes,
        reduced_updates: true,
        ..TuiOptions::default()
    };
    let mut shell = Shell::open(repo.path(), &operations, &options);

    // The first frame draws (SPEC 11.4): the signal starts raised.
    assert!(
        shell.take_redraw(),
        "the opening frame must draw in reduced-update mode",
    );
    draw(&mut shell, 110, 40);

    // A key that changes nothing the frame shows draws nothing.
    shell.handle_event(&key(KeyCode::Char('z')));
    assert!(
        !shell.take_redraw(),
        "an unbound key drew a frame in reduced-update mode",
    );

    // A key that changes the frame draws it.
    shell.handle_event(&key(KeyCode::Char('n')));
    assert!(shell.take_redraw(), "a state change draws the frame");

    // A notice is a frame change while it stands and while it clears.
    shell.handle_event(&Event::Paste("x".to_string()));
    assert!(shell.take_redraw(), "the paste notice draws the frame");
    shell.handle_event(&key(KeyCode::Char('j')));
    assert!(shell.take_redraw(), "the cleared notice draws the frame");

    // A resize always redraws: the rows rewrap at the new size.
    shell.handle_event(&Event::Resize(80, 24));
    assert!(shell.take_redraw(), "a resize draws the frame");
}

#[test]
fn edit_without_an_editor_is_refused_with_the_reason() {
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    draw(&mut shell, 110, 30);

    shell.handle_event(&key(KeyCode::Char('E')));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(
        screen.contains("no editor is configured"),
        "the refusal names what is missing: {screen}",
    );
    assert!(
        shell.take_editor_request().is_none(),
        "nothing was requested"
    );
}

#[test]
fn the_new_surface_options_parse_on_the_command_line() {
    // Both new options parse; without a terminal the surface still
    // refuses through the typed precondition rather than a parser error.
    let repo = three_group_repo();
    for option in ["--reduced-updates", "--color"] {
        let args = match option {
            "--color" => vec!["tui", "--color", "never"],
            _ => vec!["tui", "--reduced-updates"],
        };
        let output = margin(repo.env(), repo.path(), &args);
        assert_eq!(
            output.status.code(),
            Some(4),
            "{option} parses and the refusal is the missing terminal",
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("OPERATION_UNAVAILABLE"),
            "{option}: {stderr}",
        );
    }
}

#[test]
fn edit_requests_the_configured_editor_and_reloads_on_return() {
    // The editor argv is explicit in the local settings (SPEC 12.5); a
    // harmless command stands in for the editor here.
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n\nFirst passage.\n");
    repo.commit_all("import");
    let editor = repo.path().join("stand-in-editor.sh");
    std::fs::write(
        &editor,
        b"#!/bin/sh\nprintf 'Edited in the stand-in editor.\\n' >> \"$1\"\n",
    )
    .expect("the stand-in editor");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755))
        .expect("the stand-in editor is executable");
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
    // The oracle of this case is the ref set and the index: the editor's
    // own write to the working file is the editor's, while the handoff
    // and its return must not commit or stage anything (SPEC 12.5).
    let refs_before = oracle::refs(&repo);
    let head_before = oracle::head_oid(&repo);

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    draw(&mut shell, 110, 30);

    // `E` requests the handoff: the explicit argv and the literal path of
    // the displayed document (SPEC 11.3, 12.5). The expected path is
    // canonicalized with it: a platform can keep the working tree under
    // a symlinked folder, and the handoff resolves that link.
    shell.handle_event(&key(KeyCode::Char('E')));
    let handoff = shell.take_editor_request().expect("the handoff");
    assert_eq!(handoff.argv, vec![editor.display().to_string()]);
    let expected_path = std::fs::canonicalize(repo.file_path("notes/a.md"))
        .expect("the document path canonicalizes");
    assert_eq!(
        handoff.path, expected_path,
        "the path is the displayed document's literal path",
    );
    assert!(shell.take_editor_request().is_none(), "the request is once");

    // The editor appends its line; the return reloads the document and
    // states that no checkpoint was made (SPEC 12.5).
    let status = std::process::Command::new(&editor)
        .arg(repo.file_path("notes/a.md"))
        .status()
        .expect("the stand-in editor runs");
    shell.editor_returned(Ok(status));
    assert!(shell.take_redraw(), "the return draws a frame");
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(
        screen.contains("Edited in the stand-in editor."),
        "the reloaded document shows the editor's line: {screen}",
    );
    assert!(
        screen.contains("editor returned; notes/a.md reloaded; no checkpoint was made"),
        "the return is a refresh boundary, not a capture",
    );
    assert_eq!(
        refs_before,
        oracle::refs(&repo),
        "the editor return committed nothing",
    );
    assert_eq!(head_before, oracle::head_oid(&repo));

    // A failing editor start is stated, not fatal.
    shell.handle_event(&key(KeyCode::Char('E')));
    let handoff = shell.take_editor_request().expect("the second handoff");
    let _ = handoff;
    shell.editor_returned(Err("the editor could not start: no such file".to_string()));
    let page = draw(&mut shell, 110, 30);
    assert!(
        joined(&page).contains("the editor could not start"),
        "the start failure is stated",
    );
}

#[test]
fn the_editor_capture_policy_saves_one_draft_and_approves_nothing() {
    // The stored policy turns the editor's return into one capture
    // boundary (SPEC 12.5): the same shared checkpoint the History view's
    // save offer runs, never an approval.
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n\nFirst passage.\n");
    repo.commit_all("import");
    let editor = repo.path().join("stand-in-editor.sh");
    std::fs::write(
        &editor,
        b"#!/bin/sh\nprintf 'Edited in the stand-in editor.\\n' >> \"$1\"\n",
    )
    .expect("the stand-in editor");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755))
        .expect("the stand-in editor is executable");
    let editor_json = format!("[\"{}\"]", editor.display());
    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "init",
            "--scope",
            "notes/",
            "--capture-after-edit",
            "--editor-argv-json",
            &editor_json,
            "--json",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "init with the capture policy must succeed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let head_before = oracle::head_oid(&repo);
    let reviews_before =
        String::from_utf8_lossy(&repo.git_ok(["log", "--format=%H", "--grep=Margin-Kind: review"]))
            .to_string();

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    draw(&mut shell, 110, 30);

    shell.handle_event(&key(KeyCode::Char('E')));
    let handoff = shell.take_editor_request().expect("the handoff");
    let status = std::process::Command::new(&handoff.argv[0])
        .arg(&handoff.path)
        .status()
        .expect("the stand-in editor runs");
    shell.editor_returned(Ok(status));
    assert!(shell.take_redraw(), "the return draws a frame");
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(
        screen.contains("Edited in the stand-in editor."),
        "the reloaded document shows the editor's line: {screen}",
    );
    assert!(
        screen.contains("editor returned; notes/a.md reloaded; draft saved as "),
        "the return states the capture: {screen}",
    );
    assert!(
        screen.contains("nothing was approved"),
        "the capture states what it did not do: {screen}",
    );

    // One ordinary draft commit exists, and no review marker joined it:
    // the editor's exit moved the candidate and no baseline (SPEC 12.5).
    let head = oracle::head_oid(&repo).expect("the capture made one commit");
    assert_ne!(Some(head), head_before, "the policy captured one draft");
    let message_bytes = repo.git_ok(["log", "-1", "--format=%B"]);
    assert!(
        String::from_utf8_lossy(&message_bytes).contains("Margin-Kind: draft"),
        "the capture is one ordinary draft commit",
    );
    let reviews_after =
        String::from_utf8_lossy(&repo.git_ok(["log", "--format=%H", "--grep=Margin-Kind: review"]))
            .to_string();
    assert_eq!(reviews_before, reviews_after, "the exit approved nothing");
}

fn click(column: usize, row: usize, button: crossterm::event::MouseButton) -> Event {
    Event::Mouse(crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(button),
        column: column as u16,
        row: row as u16,
        modifiers: KeyModifiers::NONE,
    })
}

fn click_text(shell: &mut Shell<'_>, text: &str) {
    let page = draw(shell, 110, 32);
    let (row, column) = page
        .iter()
        .enumerate()
        .find_map(|(row, line)| {
            line.find(text)
                .map(|column| (row, unicode_width::UnicodeWidthStr::width(&line[..column])))
        })
        .unwrap_or_else(|| panic!("missing click text {text}: {}", page.join("\n")));
    shell.handle_event(&click(column, row, crossterm::event::MouseButton::Left));
}

#[test]
fn pointer_navigator_tabs_menu_and_search_reuse_keyboard_actions() {
    use crossterm::event::MouseButton::{Left, Right};
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    let before = oracle::snapshot(&repo);
    let initial = shell.selection();
    shell.handle_event(&click(3, 4, Left)); // no first frame
    assert_eq!(shell.selection(), initial);
    click_text(&mut shell, "notes/b.md");
    assert!(shell.has_pending_preview());
    shell.settle_preview();
    assert_eq!(shell.document().unwrap().path, "notes/b.md");
    assert_eq!(shell.focus(), Focus::Documents);
    click_text(&mut shell, "2 Changes");
    assert_eq!(shell.view(), View::Changes);
    click_text(&mut shell, "3 History");
    assert_eq!(shell.view(), View::History);
    click_text(&mut shell, "1 Read");
    draw(&mut shell, 110, 32);
    shell.handle_event(&click(50, 25, Left));
    assert_eq!(shell.focus(), Focus::Reading);
    draw(&mut shell, 110, 32);
    shell.handle_event(&click(5, 31, Left));
    assert_eq!(shell.focus(), Focus::Reading);
    draw(&mut shell, 110, 32);
    shell.handle_event(&click(50, 15, Right));
    assert!(matches!(
        shell.overlay(),
        Some(margin::tui::Overlay::Menu { .. })
    ));
    draw(&mut shell, 110, 32);
    shell.handle_event(&click(50, 15, Right));
    assert!(shell.overlay().is_none());
    shell.handle_event(&modified(KeyCode::Char('f'), KeyModifiers::CONTROL));
    shell.handle_event(&Event::Paste("forests".into()));
    shell.handle_event(&key(KeyCode::Enter));
    wait_for_workspace_search(&mut shell);
    click_text(&mut shell, "notes/z.md:3");
    assert_eq!(shell.document().unwrap().path, "notes/z.md");
    assert!(shell.overlay().is_none());
    before.assert_unchanged(&oracle::snapshot(&repo), "pointer navigation");
}

#[test]
fn pointer_text_entry_and_resize_block_stale_targets() {
    use crossterm::event::MouseButton::{Left, Right};
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = changes_shell(&repo, &operations);
    draw(&mut shell, 110, 32);
    shell.handle_event(&Event::Resize(80, 24));
    let selection = shell.selection();
    shell.handle_event(&click(3, 4, Left));
    assert_eq!(shell.selection(), selection);
    shell.handle_event(&key(KeyCode::Char('/')));
    draw(&mut shell, 110, 32);
    let overlay = shell.overlay().cloned();
    shell.handle_event(&click(2, 1, Left));
    shell.handle_event(&click(50, 10, Right));
    assert_eq!(shell.overlay(), overlay.as_ref());
    assert_eq!(shell.view(), View::Changes);
}

#[test]
fn pointer_selects_groups_revisions_and_menu_entries() {
    use crossterm::event::MouseButton::Right;
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = changes_shell(&repo, &operations);
    let before = oracle::snapshot(&repo);
    // Show enough rows to include the next group, then click its actual header.
    let page = draw(&mut shell, 110, 70);
    let row = page
        .iter()
        .position(|line| line.contains("Group 2 of"))
        .unwrap();
    let geometry = margin::tui::layout::frame_layout(ratatui::layout::Rect::new(0, 0, 110, 70));
    shell.handle_event(&click(
        geometry.reading.x as usize + 2,
        row,
        crossterm::event::MouseButton::Left,
    ));
    assert_eq!(shell.changes().unwrap().selection(), 1);
    assert_eq!(shell.focus(), Focus::Reading);
    shell.handle_event(&key(KeyCode::Char('3')));
    draw(&mut shell, 110, 70);
    let history = shell.history().unwrap();
    assert!(history.rows().len() > 1);
    let subject = history.rows()[1].subject.clone();
    let page = draw(&mut shell, 110, 70);
    let row = page
        .iter()
        .position(|line| line.contains(&subject))
        .unwrap();
    shell.handle_event(&click(
        geometry.reading.x as usize + 2,
        row,
        crossterm::event::MouseButton::Left,
    ));
    assert_eq!(shell.history().unwrap().selection(), 1);
    draw(&mut shell, 110, 32);
    shell.handle_event(&click(50, 15, Right));
    click_text(&mut shell, "Read view");
    assert_eq!(shell.view(), View::Read);
    assert!(shell.overlay().is_none());
    before.assert_unchanged(&oracle::snapshot(&repo), "pointer selection and menu");
}

#[test]
fn pointer_cannot_answer_finish_or_restoration_confirmation() {
    use crossterm::event::MouseButton::{Left, Right};
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = changes_shell(&repo, &operations);
    let before = oracle::snapshot(&repo);
    shell.handle_event(&key(KeyCode::Enter));
    assert!(shell.finish().is_some());
    draw(&mut shell, 110, 32);
    for button in [Left, Right] {
        shell.handle_event(&click(50, 15, button));
    }
    assert!(shell.finish().is_some());
    assert!(shell.overlay().is_none());
    shell.handle_event(&key(KeyCode::Esc));
    shell.handle_event(&key(KeyCode::Char('3')));
    shell.handle_event(&key(KeyCode::Enter));
    assert!(shell.history().unwrap().preview().is_some());
    draw(&mut shell, 110, 32);
    for button in [Left, Right] {
        shell.handle_event(&click(50, 15, button));
    }
    assert!(shell.history().unwrap().preview().is_some());
    assert!(shell.overlay().is_none());
    before.assert_unchanged(&oracle::snapshot(&repo), "blocked confirmation clicks");
}

fn wheel(direction: i32) -> Event {
    let kind = if direction < 0 {
        MouseEventKind::ScrollUp
    } else {
        MouseEventKind::ScrollDown
    };
    Event::Mouse(MouseEvent {
        kind,
        column: 1,
        row: 1,
        modifiers: KeyModifiers::NONE,
    })
}

#[test]
fn wheel_notches_follow_focus_and_scroll_the_focused_element() {
    // One wheel notch makes the movement `j` and `k` make in the focused
    // element (SPEC 11.3): the document list moves one entry, and the
    // reading column moves three rows. The wheel is one redundant path;
    // it adds no action the keyboard cannot reach (SPEC 11.4).
    let repo = TestRepo::init();
    let mut first = String::from("# First\n\nA long document.\n");
    for line in 1..=150 {
        first.push_str(&format!("Line {line} of the long document.\n"));
    }
    repo.write_file("notes/a01.md", first.as_bytes());
    for number in 2..=30 {
        let path = format!("notes/a{number:02}.md");
        let body = format!("# Document {number}\n\nA short passage.\n");
        repo.write_file(&path, body.as_bytes());
    }
    repo.commit_all("import");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    // One draw prepares the frame, so the rows and the list exist.
    draw(&mut shell, 110, 30);

    // Focus the document list: one notch moves one entry and stays at
    // the first entry when it rolls up at the top.
    shell.handle_event(&key(KeyCode::Tab));
    assert_eq!(shell.focus(), Focus::Documents);
    shell.handle_event(&wheel(-1));
    assert_eq!(shell.navigator_cursor(), 0, "a notch up at the top stays");
    shell.handle_event(&wheel(1));
    assert_eq!(shell.navigator_cursor(), 1, "one notch moves one entry");

    // Focus the reading column: one notch moves three rows. One draw
    // first wraps the document the list opened.
    shell.handle_event(&key(KeyCode::Tab));
    assert_eq!(shell.focus(), Focus::Reading);
    draw(&mut shell, 110, 30);
    shell.handle_event(&wheel(-1));
    assert_eq!(shell.reading_offset(), 0, "a notch up at the top stays");
    shell.handle_event(&wheel(1));
    assert_eq!(shell.reading_offset(), 3, "one notch scrolls three rows");
    shell.handle_event(&wheel(1));
    assert_eq!(shell.reading_offset(), 6, "a second notch adds three rows");
    shell.handle_event(&wheel(-1));
    assert_eq!(
        shell.reading_offset(),
        3,
        "a notch up scrolls three rows back"
    );
}

#[test]
fn wheel_notches_move_list_selections_and_change_no_text_entry() {
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = changes_shell(&repo, &operations);

    // The action menu moves its selection one entry per notch and stays
    // at the first entry when it rolls up at the top (SPEC 11.3).
    shell.handle_event(&key(KeyCode::Char(':')));
    shell.handle_event(&wheel(-1));
    assert!(matches!(
        shell.overlay(),
        Some(Overlay::Menu { selection: 0 })
    ));
    shell.handle_event(&wheel(1));
    assert!(matches!(
        shell.overlay(),
        Some(Overlay::Menu { selection: 1 })
    ));

    // The wheel never closes the help page and never types into a text
    // field: the search field stands with its query empty.
    select_palette_action(&mut shell, Action::Help);
    shell.handle_event(&wheel(1));
    assert!(matches!(shell.overlay(), Some(Overlay::Help)));
    shell.handle_event(&key(KeyCode::Esc));
    assert!(matches!(shell.overlay(), Some(Overlay::Menu { .. })));
    shell.handle_event(&key(KeyCode::Esc));
    shell.handle_event(&key(KeyCode::Char('/')));
    shell.handle_event(&wheel(1));
    assert!(
        matches!(shell.overlay(), Some(Overlay::Search { .. })),
        "the wheel left the search field standing: {:?}",
        shell.overlay(),
    );
    shell.handle_event(&key(KeyCode::Esc));

    // The finish summary accepts scrolling without changing the pending plan.
    shell.handle_event(&key(KeyCode::Enter));
    assert!(
        shell.finish().is_some(),
        "Enter opened the finish summary over the pending review",
    );
    shell.handle_event(&wheel(1));
    assert!(
        shell.finish().is_some(),
        "the wheel cannot move the summary"
    );
    assert!(shell.finish_result().is_none(), "the wheel runs nothing");
}

#[test]
fn reduced_updates_redraw_on_a_wheel_scroll_but_not_on_pointer_motion() {
    // Any-motion tracking reports every pointer move; none of them may
    // draw a frame, while one wheel notch scrolls and draws (SPEC 11.4).
    let repo = TestRepo::init();
    let mut first = String::from("# First\n\nA long document.\n");
    for line in 1..=150 {
        first.push_str(&format!("Line {line} of the long document.\n"));
    }
    repo.write_file("notes/a01.md", first.as_bytes());
    repo.commit_all("import");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let options = TuiOptions {
        reduced_updates: true,
        ..TuiOptions::default()
    };
    let mut shell = Shell::open(repo.path(), &operations, &options);
    assert!(
        shell.take_redraw(),
        "the opening frame must draw in reduced-update mode",
    );
    draw(&mut shell, 110, 30);

    shell.handle_event(&Event::Mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: 5,
        row: 5,
        modifiers: KeyModifiers::NONE,
    }));
    assert!(
        !shell.take_redraw(),
        "pointer motion alone drew a frame in reduced-update mode",
    );

    shell.handle_event(&wheel(1));
    assert!(shell.take_redraw(), "a wheel scroll draws the frame");
    assert_eq!(shell.reading_offset(), 3, "the notch moved three rows");
}

#[test]
fn pointer_overlay_precedence_blocks_underlying_items_and_right_clicks() {
    use crossterm::event::MouseButton::{Left, Right};
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = changes_shell(&repo, &operations);
    for overlay_key in ['?', '/', 'f', 'N'] {
        let mut shell = changes_shell(&repo, &operations);
        shell.handle_event(&key(KeyCode::Char(overlay_key)));
        draw(&mut shell, 110, 32);
        let overlay = shell.overlay().cloned();
        assert!(overlay.is_some());
        let focus = shell.focus();
        for button in [Left, Right] {
            shell.handle_event(&click(4, 1, button));
            shell.handle_event(&click(4, 5, button));
        }
        assert_eq!(shell.overlay(), overlay.as_ref());
        assert_eq!(shell.focus(), focus);
        assert_eq!(shell.view(), View::Changes);
        shell.handle_event(&key(KeyCode::Esc));
    }
    shell.handle_event(&modified(KeyCode::Char('f'), KeyModifiers::CONTROL));
    shell.handle_event(&Event::Paste("forests".into()));
    draw(&mut shell, 110, 32);
    shell.handle_event(&click(4, 1, Right));
    shell.handle_event(&click(4, 5, Left));
    assert!(matches!(shell.overlay(), Some(Overlay::WorkspaceSearch)));
    assert_eq!(shell.workspace_search().query, "forests");
    shell.handle_event(&key(KeyCode::Esc));
    shell.handle_event(&key(KeyCode::Char(':')));
    draw(&mut shell, 110, 32);
    let selected = shell.selection();
    shell.handle_event(&click(4, 5, Left));
    shell.handle_event(&click(4, 1, Left));
    assert_eq!(shell.selection(), selected);
    assert_eq!(shell.view(), View::Changes);
    assert!(matches!(shell.overlay(), Some(Overlay::Menu { .. })));
    shell.handle_event(&click(4, 1, Right));
    assert!(shell.overlay().is_none());
}

#[test]
fn pointer_wrapped_menu_continuation_uses_its_owner_and_disabled_checks() {
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = changes_shell(&repo, &operations);
    shell.handle_event(&key(KeyCode::Char(':')));
    wait_palette(&mut shell);
    let edit = shell
        .palette()
        .hits
        .iter()
        .position(|hit| hit.entry.action == Some(Action::ExternalEditor))
        .unwrap();
    for _ in 0..edit {
        shell.handle_event(&key(KeyCode::Down));
    }
    let page = draw(&mut shell, 60, 16);
    let row = page
        .iter()
        .position(|line| line.contains("no editor") || line.contains("configured;"))
        .unwrap_or_else(|| panic!("missing wrapped editor explanation: {}", page.join("\n")));
    let before = oracle::snapshot(&repo);
    shell.handle_event(&click(4, row, crossterm::event::MouseButton::Left));
    assert_eq!(shell.overlay(), Some(&Overlay::Menu { selection: edit }));
    assert!(shell.take_editor_request().is_none());
    before.assert_unchanged(&oracle::snapshot(&repo), "disabled wrapped menu click");
    // Resizing to a too-small frame removes even right-click targets.
    draw(&mut shell, 30, 8);
    shell.handle_event(&click(4, 4, crossterm::event::MouseButton::Right));
    assert_eq!(shell.overlay(), Some(&Overlay::Menu { selection: edit }));
}

#[test]
fn help_scrolls_at_supported_sizes_and_returns_to_the_menu_selection() {
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let ops = Operations::new(&git);
    for (width, height) in [(60, 16), (80, 24), (120, 40)] {
        let mut shell = changes_shell(&repo, &ops);
        shell.handle_event(&key(KeyCode::Char(':')));
        shell.handle_event(&key(KeyCode::Down));
        select_palette_action(&mut shell, Action::Help);
        let menu_selection = shell
            .palette()
            .hits
            .iter()
            .position(|h| h.entry.action == Some(Action::Help))
            .unwrap();
        let before = joined(&draw(&mut shell, width, height));
        assert!(before.contains("Current blockers"));
        shell.handle_event(&key(KeyCode::End));
        let after = joined(&draw(&mut shell, width, height));
        assert_ne!(before, after);
        assert!(after.contains("Fold/expand outline"));
        shell.handle_event(&key(KeyCode::Up));
        assert_ne!(
            joined(&draw(&mut shell, width, height)),
            after,
            "one Up leaves the final help row"
        );
        shell.handle_event(&key(KeyCode::Home));
        shell.handle_event(&key(KeyCode::Char('/')));
        shell.handle_event(&Event::Paste("Next undecided".into()));
        shell.handle_event(&key(KeyCode::Enter));
        let filtered = joined(&draw(&mut shell, width, height));
        assert!(filtered.contains("Next undecided"));
        assert!(!filtered.contains("Open external editor"));
        shell.handle_event(&key(KeyCode::Esc));
        assert_eq!(
            shell.overlay(),
            Some(&Overlay::Menu {
                selection: menu_selection
            })
        );
    }
}

#[test]
fn menu_filter_is_literal_and_runs_only_the_selected_available_action() {
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let ops = Operations::new(&git);
    let mut shell = changes_shell(&repo, &ops);
    let before = oracle::snapshot(&repo);
    shell.handle_event(&key(KeyCode::Char(':')));
    shell.handle_event(&Event::Paste("q:rK".into()));
    assert!(!shell.should_quit());
    wait_palette(&mut shell);
    assert!(joined(&draw(&mut shell, 80, 24)).contains("No matching actions"));
    shell.handle_event(&key(KeyCode::Enter));
    assert!(shell.overlay().is_some());
    for _ in 0..4 {
        shell.handle_event(&key(KeyCode::Backspace));
    }
    shell.handle_event(&Event::Paste("Read view".into()));
    wait_palette(&mut shell);
    shell.handle_event(&key(KeyCode::Enter));
    assert_eq!(shell.view(), View::Read);
    assert!(shell.overlay().is_none());
    assert_eq!(oracle::snapshot(&repo), before);
}

#[test]
fn picker_selects_by_arrows_without_opening_until_confirmed() {
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let ops = Operations::new(&git);
    let mut shell = changes_shell(&repo, &ops);
    let original = shell.document().unwrap().path.clone();
    shell.handle_event(&key(KeyCode::Char('f')));
    shell.handle_event(&key(KeyCode::End));
    assert_eq!(shell.document().unwrap().path, original);
    assert!(joined(&draw(&mut shell, 60, 16)).contains("› notes/z.md"));
    shell.handle_event(&key(KeyCode::Esc));
    assert_eq!(shell.document().unwrap().path, original);
    shell.handle_event(&key(KeyCode::Char('f')));
    shell.handle_event(&key(KeyCode::Down));
    shell.handle_event(&key(KeyCode::Enter));
    shell.settle_preview();
    assert_eq!(shell.document().unwrap().path, "notes/b.md");
}

#[test]
fn finish_can_return_to_an_undecided_group_without_applying() {
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let ops = Operations::new(&git);
    let mut shell = changes_shell(&repo, &ops);
    shell.handle_event(&key(KeyCode::Char('K')));
    let before = oracle::snapshot(&repo);
    shell.handle_event(&key(KeyCode::Enter));
    assert!(shell.finish().is_some());
    draw(&mut shell, 60, 16);
    shell.handle_event(&key(KeyCode::End));
    assert!(joined(&draw(&mut shell, 60, 16)).contains("Cancel; nothing"));
    shell.handle_event(&key(KeyCode::Char(']')));
    assert!(shell.finish().is_none());
    assert_eq!(shell.changes().unwrap().selection(), 1);
    assert_eq!(shell.changes().unwrap().proposed_count(), 1);
    assert_eq!(oracle::snapshot(&repo), before);
}

fn wait_palette(shell: &mut Shell<'_>) {
    let start = std::time::Instant::now();
    while !shell.palette().ready() {
        shell.poll_palette();
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn select_palette_action(shell: &mut Shell<'_>, action: Action) {
    wait_palette(shell);
    let target = shell
        .palette()
        .hits
        .iter()
        .position(|h| h.entry.action == Some(action))
        .unwrap();
    // Move to a known starting selection using the existing arrow contract.
    for _ in 0..shell.palette().hits.len() {
        shell.handle_event(&key(KeyCode::Up));
    }
    for _ in 0..target {
        shell.handle_event(&key(KeyCode::Down));
    }
    shell.handle_event(&key(KeyCode::Enter));
}
