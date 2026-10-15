//! The default writing workflow preserves files without modal save steps.
mod support;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use margin::{
    app::{InitRequest, Operations},
    domain::ScopeInput,
    git::Git,
    requests::State,
    tui::{Shell, TuiOptions},
};
use std::time::{Duration, Instant};
use support::repo::TestRepo;
const ORIGINAL: &str = "Header\n\nTarget passage.\n\nOther paragraph.\n";
fn repo() -> TestRepo {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", ORIGINAL.as_bytes());
    repo.write_file("notes/b.md", b"Other document\n");
    repo.commit_all("Import");
    let git = git(&repo);
    let ops = Operations::new(&git);
    assert!(
        ops.initialize_scope(&InitRequest {
            scope: Some(ScopeInput {
                include: vec!["notes/".into()],
                suffixes: vec![".md".into()],
                ..Default::default()
            }),
            ..Default::default()
        })
        .ok()
    );
    repo
}
fn git(repo: &TestRepo) -> Git {
    Git::from_env(repo.env().vars().clone()).at(repo.path())
}
fn key(shell: &mut Shell<'_>, code: KeyCode, mods: KeyModifiers) {
    shell.handle_event(&Event::Key(KeyEvent::new(code, mods)));
}
fn edit(shell: &mut Shell<'_>) {
    key(shell, KeyCode::Char('e'), KeyModifiers::NONE);
}
fn paste(shell: &mut Shell<'_>, text: &str) {
    shell.handle_event(&Event::Paste(text.into()));
}
fn settle(shell: &mut Shell<'_>, now: Instant) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        shell.poll_autosave_at(now);
        if !shell.autosave_pending() {
            break;
        }
        assert!(Instant::now() < deadline, "autosave did not settle");
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn after_pause(shell: &mut Shell<'_>) {
    let now = shell
        .source_editor()
        .map_or(Instant::now(), |e| e.last_edit)
        + Duration::from_millis(751);
    settle(shell, now);
}

fn poll(shell: &mut Shell<'_>) {
    shell.notify_edit_requests();
    shell.poll_edit_requests();
}
#[test]
fn autosave_debounces_typing_and_does_not_create_commits_or_approval() {
    let repo = repo();
    let git = git(&repo);
    let ops = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &ops, &TuiOptions::default());
    let head = repo.git_ok(["rev-parse", "HEAD"]);
    let index = repo.git_ok(["write-tree"]);
    edit(&mut shell);
    paste(&mut shell, "First ");
    let first = shell.source_editor().unwrap().last_edit;
    shell.poll_autosave_at(first + Duration::from_millis(749));
    assert_eq!(repo.read_file("notes/a.md"), ORIGINAL.as_bytes());
    paste(&mut shell, "Second ");
    let latest = shell.source_editor().unwrap().last_edit;
    shell.poll_autosave_at(latest + Duration::from_millis(749));
    assert_eq!(repo.read_file("notes/a.md"), ORIGINAL.as_bytes());
    after_pause(&mut shell);
    assert!(repo.read_file("notes/a.md").starts_with(b"First Second "));
    assert!(!shell.source_editor().unwrap().dirty());
    assert_eq!(repo.git_ok(["rev-parse", "HEAD"]), head);
    assert_eq!(repo.git_ok(["write-tree"]), index);
}
#[test]
fn remote_navigation_retains_dirty_buffers_cursor_and_undo() {
    let repo = repo();
    let git = git(&repo);
    let ops = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &ops, &TuiOptions::default());
    edit(&mut shell);
    key(&mut shell, KeyCode::End, KeyModifiers::NONE);
    paste(&mut shell, " writer");
    let cursor = shell.source_editor().unwrap().cursor;
    shell
        .open_remote(&margin::desk::Request {
            path: "notes/b.md".into(),
            line: None,
        })
        .unwrap();
    assert_eq!(shell.document().unwrap().path, "notes/b.md");
    settle(&mut shell, Instant::now() + Duration::from_secs(1));
    assert!(repo.read_file("notes/a.md").starts_with(b"Header writer"));
    edit(&mut shell);
    paste(&mut shell, "B edit ");
    key(&mut shell, KeyCode::Esc, KeyModifiers::NONE);
    assert!(repo.read_file("notes/b.md").starts_with(b"B edit "));
    shell
        .open_remote(&margin::desk::Request {
            path: "notes/a.md".into(),
            line: None,
        })
        .unwrap();
    edit(&mut shell);
    assert_eq!(shell.source_editor().unwrap().cursor, cursor);
    key(&mut shell, KeyCode::Char('z'), KeyModifiers::CONTROL);
    assert_eq!(shell.source_editor().unwrap().text, ORIGINAL);
    after_pause(&mut shell);
    assert_eq!(repo.read_file("notes/a.md"), ORIGINAL.as_bytes());
}
#[test]
fn a_conflict_pauses_autosave_until_the_compared_base_is_accepted() {
    let repo = repo();
    let git = git(&repo);
    let ops = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &ops, &TuiOptions::default());
    edit(&mut shell);
    paste(&mut shell, "My text ");
    repo.write_file("notes/a.md", b"External text");
    after_pause(&mut shell);
    assert!(shell.source_editor().unwrap().autosave_error.is_some());
    assert_eq!(repo.read_file("notes/a.md"), b"External text");
    let recovery = git.git_path("margin-editing").unwrap().join("recovery");
    let count = std::fs::read_dir(&recovery).unwrap().count();
    after_pause(&mut shell);
    assert_eq!(std::fs::read_dir(recovery).unwrap().count(), count);
    key(&mut shell, KeyCode::Char('d'), KeyModifiers::CONTROL);
    key(&mut shell, KeyCode::Char('b'), KeyModifiers::CONTROL);
    after_pause(&mut shell);
    assert!(repo.read_file("notes/a.md").starts_with(b"My text "));
    assert!(shell.source_editor().unwrap().autosave_error.is_none());
}
#[test]
fn quitting_flushes_active_and_retained_input_before_the_debounce_deadline() {
    let repo = repo();
    let git = git(&repo);
    let ops = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &ops, &TuiOptions::default());
    edit(&mut shell);
    paste(&mut shell, "A ");
    shell
        .open_remote(&margin::desk::Request {
            path: "notes/b.md".into(),
            line: None,
        })
        .unwrap();
    edit(&mut shell);
    paste(&mut shell, "B ");
    key(&mut shell, KeyCode::Char('q'), KeyModifiers::CONTROL);
    assert!(shell.should_quit());
    assert!(repo.read_file("notes/a.md").starts_with(b"A "));
    assert!(repo.read_file("notes/b.md").starts_with(b"B "));
}
#[test]
fn requests_capture_current_text_merge_separate_edits_and_support_immediate_undo() {
    let repo = repo();
    let git = git(&repo);
    let ops = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &ops, &TuiOptions::default());
    edit(&mut shell);
    paste(&mut shell, "New opening\n");
    key(&mut shell, KeyCode::Down, KeyModifiers::NONE);
    key(&mut shell, KeyCode::Down, KeyModifiers::NONE);
    key(&mut shell, KeyCode::End, KeyModifiers::SHIFT);
    let selected = shell.source_editor().unwrap().selection();
    assert_eq!(
        &shell.source_editor().unwrap().text[selected],
        "Target passage."
    );
    key(&mut shell, KeyCode::F(2), KeyModifiers::NONE);
    assert!(shell.agent_prompt().is_some());
    assert!(repo.read_file("notes/a.md").starts_with(b"New opening\n"));
    paste(&mut shell, "Make this brief");
    key(&mut shell, KeyCode::Enter, KeyModifiers::NONE);
    let request = ops.list_edit_requests().unwrap().pop().unwrap();
    assert!(request.base.text.starts_with("New opening\n"));
    assert!(
        String::from_utf8(repo.git_ok(["show", "HEAD:notes/a.md"]))
            .unwrap()
            .starts_with("New opening\n")
    );
    ops.claim_edit_request(&request.id, "agent").unwrap();
    key(&mut shell, KeyCode::Char('a'), KeyModifiers::CONTROL);
    key(&mut shell, KeyCode::Right, KeyModifiers::NONE);
    paste(&mut shell, "Writer final\n");
    after_pause(&mut shell);
    let before_agent = repo.read_file("notes/a.md");
    ops.submit_edit_result(&request.id, "agent", "Brief target.", "Remove repetition")
        .unwrap();
    poll(&mut shell);
    assert_eq!(
        ops.inspect_edit_request(&request.id).unwrap().state,
        State::Applied
    );
    let current = String::from_utf8(repo.read_file("notes/a.md")).unwrap();
    assert!(current.contains("Brief target."));
    assert!(current.ends_with("Writer final\n"));
    assert!(shell.source_editor().is_some());
    assert_eq!(shell.source_editor().unwrap().text, current);
    assert_eq!(repo.git_ok(["show", "HEAD:notes/a.md"]), current.as_bytes());
    key(&mut shell, KeyCode::Char('u'), KeyModifiers::CONTROL);
    assert_eq!(repo.read_file("notes/a.md"), before_agent);
    assert_eq!(
        ops.inspect_edit_request(&request.id).unwrap().state,
        State::Undone
    );
    assert!(
        repo.git_ok(["log", "--format=%H", "--grep=Margin-Kind: review"])
            .is_empty()
    );
    assert!(
        ops.list_notes(None).data.unwrap().notes.iter().any(|n| n.by
            == margin::notes::Author::Writer
            && n.text.contains("Undid agent request"))
    );
}
#[test]
fn overlapping_results_offer_keep_mine_and_use_agent_without_losing_other_text() {
    let repo = repo();
    let git = git(&repo);
    let ops = Operations::new(&git);
    let base = ops.edit_document("notes/a.md").unwrap();
    let start = base.text.find("Target passage.").unwrap();
    let request = ops
        .create_edit_request(base.clone(), Some(start..start + 15), "Rewrite")
        .unwrap();
    ops.claim_edit_request(&request.id, "agent").unwrap();
    ops.submit_edit_result(&request.id, "agent", "Agent passage.", "Reason")
        .unwrap();
    repo.write_file(
        "notes/a.md",
        ORIGINAL
            .replace("Target passage.", "Writer alternative.")
            .as_bytes(),
    );
    let mut shell = Shell::open(repo.path(), &ops, &TuiOptions::default());
    edit(&mut shell);
    poll(&mut shell);
    poll(&mut shell);
    assert_eq!(
        ops.inspect_edit_request(&request.id).unwrap().state,
        State::Stale
    );
    assert!(shell.source_editor().unwrap().conflicting_request.is_some());
    key(&mut shell, KeyCode::F(4), KeyModifiers::NONE);
    assert_eq!(
        ops.inspect_edit_request(&request.id).unwrap().state,
        State::Cancelled
    );
    assert!(
        String::from_utf8(repo.read_file("notes/a.md"))
            .unwrap()
            .contains("Writer alternative.")
    );
    let base = ops.edit_document("notes/a.md").unwrap();
    let start = base.text.find("Writer alternative.").unwrap();
    let request = ops
        .create_edit_request(base, Some(start..start + 19), "Rewrite")
        .unwrap();
    ops.claim_edit_request(&request.id, "agent").unwrap();
    ops.submit_edit_result(&request.id, "agent", "Chosen agent version.", "Reason")
        .unwrap();
    repo.write_file(
        "notes/a.md",
        ORIGINAL
            .replace("Target passage.", "Writer changed again.")
            .as_bytes(),
    );
    poll(&mut shell);
    poll(&mut shell);
    key(&mut shell, KeyCode::F(3), KeyModifiers::NONE);
    assert_eq!(
        ops.inspect_edit_request(&request.id).unwrap().state,
        State::Applied
    );
    let text = String::from_utf8(repo.read_file("notes/a.md")).unwrap();
    assert!(text.contains("Chosen agent version."));
    assert!(text.contains("Other paragraph."));
}

#[test]
fn input_continues_during_background_contention_and_the_latest_text_is_saved() {
    let repo = repo();
    let git = git(&repo);
    let ops = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &ops, &TuiOptions::default());
    edit(&mut shell);
    paste(&mut shell, "First ");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(git.git_path("margin.lock").unwrap())
        .unwrap();
    lock.try_lock().unwrap();
    shell.poll_autosave_at(Instant::now() + Duration::from_secs(1));
    paste(&mut shell, "Second ");
    assert!(
        shell
            .source_editor()
            .unwrap()
            .text
            .starts_with("First Second ")
    );
    assert_eq!(repo.read_file("notes/a.md"), ORIGINAL.as_bytes());
    std::thread::sleep(Duration::from_millis(50));
    shell.poll_autosave_at(Instant::now() + Duration::from_secs(1));
    assert!(shell.source_editor().unwrap().autosave_error.is_none());
    lock.unlock().unwrap();
    after_pause(&mut shell);
    assert!(repo.read_file("notes/a.md").starts_with(b"First Second "));
}

#[test]
fn writing_with_unrelated_staging_preserves_the_index_and_queues_history() {
    let repo = repo();
    repo.write_file("outside.txt", b"Staged work\n");
    repo.git_ok(["add", "--", "outside.txt"]);
    let index = repo.git_ok(["write-tree"]);
    let git = git(&repo);
    let ops = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &ops, &TuiOptions::default());
    edit(&mut shell);
    assert!(shell.source_editor().is_some());
    paste(&mut shell, "My draft ");
    after_pause(&mut shell);
    assert!(repo.read_file("notes/a.md").starts_with(b"My draft "));
    assert_eq!(repo.git_ok(["write-tree"]), index);
    key(&mut shell, KeyCode::F(2), KeyModifiers::NONE);
    paste(&mut shell, "Keep the example");
    key(&mut shell, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(ops.list_edit_requests().unwrap().len(), 1);
    assert!(shell.history_notice().is_some());
    assert_eq!(repo.git_ok(["write-tree"]), index);
}

#[test]
fn clicking_a_passage_enters_source_at_its_source_line() {
    let repo = repo();
    let git = git(&repo);
    let ops = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &ops, &TuiOptions::default());
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(110, 30)).unwrap();
    terminal
        .draw(|frame| margin::tui::render::draw(&mut shell, frame))
        .unwrap();
    let (row, column) = terminal
        .backend()
        .buffer()
        .content
        .chunks(110)
        .enumerate()
        .find_map(|(row, cells)| {
            let text = cells.iter().map(|cell| cell.symbol()).collect::<String>();
            text.find("Other paragraph.").map(|column| (row, column))
        })
        .unwrap();
    shell.handle_event(&Event::Mouse(crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: column as u16,
        row: row as u16,
        modifiers: KeyModifiers::NONE,
    }));
    assert_eq!(shell.source_editor().unwrap().position().0, 4);
}
