//! Editing saves and desk requests against real Git repositories.
mod support;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use margin::{
    app::{InitRequest, Operations},
    domain::ScopeInput,
    git::Git,
    requests::State,
    tui::{Shell, TuiOptions, render},
};
use ratatui::{Terminal, backend::TestBackend};
use support::repo::{ObjectFormat, RepoOptions, TestRepo};
fn adopted(format: ObjectFormat) -> TestRepo {
    let repo = TestRepo::init_with(RepoOptions::new().with_object_format(format));
    repo.write_file(
        "notes/a.md",
        "# Title\r\n\r\nA café 👩‍💻 passage.\r\n".as_bytes(),
    );
    repo.write_file("outside.txt", b"Outside\n");
    repo.commit_all("Import");
    let git = adapter(&repo);
    let operations = Operations::new(&git);
    let result = operations.initialize_scope(&InitRequest {
        scope: Some(ScopeInput {
            include: vec!["notes/".into()],
            suffixes: vec![".md".into()],
            ..Default::default()
        }),
        ..Default::default()
    });
    assert!(result.ok(), "{:?}", result.error);
    repo
}
fn adapter(repo: &TestRepo) -> Git {
    Git::from_env(repo.env().vars().clone()).at(repo.path())
}
fn press(shell: &mut Shell<'_>, code: KeyCode, modifiers: KeyModifiers) {
    shell.handle_event(&Event::Key(KeyEvent::new(code, modifiers)));
}
fn screen(shell: &mut Shell<'_>, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| render::draw(shell, f)).unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect()
}
#[test]
fn guarded_saves_preserve_crlf_permissions_index_head_and_review() {
    for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
        let repo = adopted(format);
        repo.set_mode("notes/a.md", 0o644);
        let git = adapter(&repo);
        let operations = Operations::new(&git);
        let base = operations.edit_document("notes/a.md").unwrap();
        let head = repo.git_ok(["rev-parse", "HEAD"]);
        let index = repo.git_ok(["write-tree"]);
        let text = base.text.replace("passage", "revision");
        let saved = operations.save_document(&base, &text).unwrap();
        assert_eq!(saved.text, text);
        assert_eq!(repo.read_file("notes/a.md"), text.as_bytes());
        assert_eq!(repo.git_ok(["rev-parse", "HEAD"]), head);
        assert_eq!(repo.git_ok(["write-tree"]), index);
        assert_eq!(repo.read_file("outside.txt"), b"Outside\n");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(repo.file_path("notes/a.md"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
        assert_eq!(
            operations.save_document(&base, "old").unwrap_err().code,
            "STALE_STATE"
        );
        assert_eq!(repo.read_file("notes/a.md"), text.as_bytes());
        assert!(operations.edit_document("outside.txt").is_err());
        assert!(operations.edit_document("../outside.md").is_err());
    }
}
#[test]
fn edit_mode_isolates_input_and_escape_saves_without_a_dialog() {
    let repo = adopted(ObjectFormat::Sha1);
    let git = adapter(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    screen(&mut shell, 110, 30);
    press(&mut shell, KeyCode::Char('e'), KeyModifiers::NONE);
    shell.handle_event(&Event::Paste("qrK:\n界".into()));
    assert!(!shell.should_quit());
    assert!(!shell.remote_busy());
    assert!(screen(&mut shell, 80, 24).contains("Saving"));
    press(&mut shell, KeyCode::Esc, KeyModifiers::NONE);
    assert!(shell.source_editor().is_none());
    assert!(
        repo.read_file("notes/a.md")
            .starts_with("qrK:\n界".as_bytes())
    );
    press(&mut shell, KeyCode::Char('e'), KeyModifiers::NONE);
    assert!(shell.source_editor().unwrap().text.starts_with("qrK:\n界"));
    assert!(!shell.source_editor().unwrap().dirty());
}

#[test]
fn grapheme_movement_undo_redo_and_crlf_do_not_split_text() {
    let repo = adopted(ObjectFormat::Sha1);
    let git = adapter(&repo);
    let operations = Operations::new(&git);
    let mut buffer =
        margin::tui::editor::Editor::new(operations.edit_document("notes/a.md").unwrap());
    buffer.key(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    buffer.insert("e\u{301}👩‍💻\r\n界");
    buffer.key(&KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    buffer.key(&KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    assert_eq!(&buffer.text[..buffer.cursor], "e\u{301}👩‍💻");
    buffer.key(&KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
    assert_eq!(buffer.text, "e\u{301}\r\n界");
    buffer.key(&KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
    assert_eq!(buffer.text, "e\u{301}👩‍💻\r\n界");
    buffer.key(&KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
    assert_eq!(buffer.text, "e\u{301}\r\n界");
}
#[test]
fn queue_claim_submission_retry_and_stale_base_are_guarded() {
    let repo = adopted(ObjectFormat::Sha1);
    let git = adapter(&repo);
    let operations = Operations::new(&git);
    let base = operations.edit_document("notes/a.md").unwrap();
    let start = base.text.find("passage").unwrap();
    let request = operations
        .create_edit_request(base.clone(), Some(start..start + 7), "Shorten this")
        .unwrap();
    operations.claim_edit_request(&request.id, "one").unwrap();
    assert!(operations.claim_edit_request(&request.id, "two").is_err());
    assert!(
        operations
            .submit_edit_result(&request.id, "two", "revision", "Shorter")
            .is_err()
    );
    operations
        .submit_edit_result(&request.id, "one", "revision", "Shorter")
        .unwrap();
    let head = repo.git_ok(["rev-parse", "HEAD"]);
    let index = repo.git_ok(["write-tree"]);
    assert_eq!(
        operations.apply_edit_result(&request.id).unwrap().state,
        State::Applied
    );
    assert_eq!(
        operations
            .submit_edit_result(&request.id, "one", "revision", "Shorter")
            .unwrap()
            .state,
        State::Applied
    );
    assert_eq!(
        operations.apply_edit_result(&request.id).unwrap().state,
        State::Applied
    );
    assert_eq!(
        repo.read_file("notes/a.md"),
        base.text.replace("passage", "revision").as_bytes()
    );
    assert_eq!(repo.git_ok(["rev-parse", "HEAD"]), head);
    assert_eq!(repo.git_ok(["write-tree"]), index);
    let base = operations.edit_document("notes/a.md").unwrap();
    let stale = operations
        .create_edit_request(base, None, "Revise")
        .unwrap();
    operations.claim_edit_request(&stale.id, "one").unwrap();
    operations
        .submit_edit_result(&stale.id, "one", "Agent version", "Reason")
        .unwrap();
    repo.write_file("notes/a.md", b"Writer version");
    assert_eq!(
        operations.apply_edit_result(&stale.id).unwrap().state,
        State::Stale
    );
    assert_eq!(repo.read_file("notes/a.md"), b"Writer version");
    assert!(operations.inspect_edit_request("../bad").is_err());
}
#[test]
fn recovery_requires_an_explicit_load_and_never_writes_the_document() {
    let repo = adopted(ObjectFormat::Sha1);
    let git = adapter(&repo);
    let operations = Operations::new(&git);
    let base = operations.edit_document("notes/a.md").unwrap();
    operations
        .retain_edit_buffer("draft-test", &base, "Recovered text")
        .unwrap();
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    press(&mut shell, KeyCode::Char('e'), KeyModifiers::NONE);
    assert_eq!(shell.source_editor().unwrap().text, base.text);
    assert!(screen(&mut shell, 110, 30).contains("recovery buffer exists"));
    press(&mut shell, KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert_eq!(shell.source_editor().unwrap().text, "Recovered text");
    shell.poll_autosave_at(std::time::Instant::now() + std::time::Duration::from_secs(3));
    assert!(!shell.autosave_pending());
    assert!(shell.source_editor().unwrap().autosave_error.is_some());
    assert_eq!(repo.read_file("notes/a.md"), base.text.as_bytes());
}

#[test]
fn concurrent_claims_serialize_and_interrupted_application_never_repeats() {
    let repo = adopted(ObjectFormat::Sha1);
    let git = adapter(&repo);
    let operations = Operations::new(&git);
    let request = operations
        .create_edit_request(
            operations.edit_document("notes/a.md").unwrap(),
            None,
            "Revise",
        )
        .unwrap();
    let barrier = std::sync::Barrier::new(2);
    let winners = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            let ops = Operations::new(&git);
            barrier.wait();
            ops.claim_edit_request(&request.id, "a").is_ok()
        });
        let b = scope.spawn(|| {
            let ops = Operations::new(&git);
            barrier.wait();
            ops.claim_edit_request(&request.id, "b").is_ok()
        });
        usize::from(a.join().unwrap()) + usize::from(b.join().unwrap())
    });
    assert_eq!(winners, 1);
    let claimed = operations.inspect_edit_request(&request.id).unwrap();
    operations
        .submit_edit_result(
            &request.id,
            claimed.owner.as_deref().unwrap(),
            "Proposed",
            "Reason",
        )
        .unwrap();
    let path = git
        .git_path("margin-editing")
        .unwrap()
        .join("requests")
        .join(format!("{}.json", request.id));
    let mut interrupted = operations.inspect_edit_request(&request.id).unwrap();
    interrupted.state = State::Applying;
    std::fs::write(&path, serde_json::to_vec(&interrupted).unwrap()).unwrap();
    let before = repo.read_file("notes/a.md");
    let result = operations.apply_edit_result(&request.id).unwrap();
    assert_eq!(result.state, State::Applying);
    assert!(result.error.unwrap().contains("Interrupted"));
    assert_eq!(repo.read_file("notes/a.md"), before);
    assert!(operations.cancel_edit_request(&request.id).is_err());
    assert_eq!(
        operations
            .reconcile_edit_request(&request.id)
            .unwrap()
            .state,
        State::Failed
    );
    assert_eq!(repo.read_file("notes/a.md"), before);
    interrupted.state = State::Applying;
    std::fs::write(&path, serde_json::to_vec(&interrupted).unwrap()).unwrap();
    repo.write_file("notes/a.md", b"Proposed");
    assert_eq!(
        operations
            .reconcile_edit_request(&request.id)
            .unwrap()
            .state,
        State::Applied
    );
    assert_eq!(repo.read_file("notes/a.md"), b"Proposed");
}
#[test]
fn source_limits_symlinks_and_scope_changes_refuse_without_overwrite() {
    let repo = adopted(ObjectFormat::Sha1);
    let git = adapter(&repo);
    let operations = Operations::new(&git);
    let base = operations.edit_document("notes/a.md").unwrap();
    assert_eq!(
        operations
            .save_document(&base, &"a".repeat(margin::editing::MAX_SOURCE_BYTES + 1))
            .unwrap_err()
            .code,
        "LIMIT_EXCEEDED"
    );
    let original = repo.read_file("notes/a.md");
    std::fs::remove_file(repo.file_path("notes/a.md")).unwrap();
    std::os::unix::fs::symlink(repo.file_path("outside.txt"), repo.file_path("notes/a.md"))
        .unwrap();
    assert!(operations.save_document(&base, "Replacement").is_err());
    assert_eq!(repo.read_file("outside.txt"), b"Outside\n");
    std::fs::remove_file(repo.file_path("notes/a.md")).unwrap();
    repo.write_file("notes/a.md", &original);
    repo.git_ok(["checkout", "-b", "other"]);
    assert_eq!(
        operations
            .save_document(&base, "Replacement")
            .unwrap_err()
            .code,
        "BRANCH_CHANGED"
    );
    assert_eq!(repo.read_file("notes/a.md"), original);
}
#[test]
fn passage_request_applies_in_the_open_editor_and_preserves_unreviewed_history() {
    let repo = adopted(ObjectFormat::Sha1);
    let git = adapter(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    press(&mut shell, KeyCode::Char('e'), KeyModifiers::NONE);
    // Select the first source character with its actual byte span.
    press(&mut shell, KeyCode::Right, KeyModifiers::SHIFT);
    press(
        &mut shell,
        KeyCode::Char('A'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert!(shell.agent_prompt().is_some());
    shell.handle_event(&Event::Paste("Improve this".into()));
    press(&mut shell, KeyCode::Enter, KeyModifiers::NONE);
    let request = operations.list_edit_requests().unwrap().pop().unwrap();
    assert_eq!((request.start, request.end), (0, 1));
    operations.claim_edit_request(&request.id, "agent").unwrap();
    operations
        .submit_edit_result(&request.id, "agent", "##", "A smaller heading")
        .unwrap();
    shell.notify_edit_requests();
    shell.poll_edit_requests();
    assert_eq!(
        operations.inspect_edit_request(&request.id).unwrap().state,
        State::Applied
    );
    assert!(shell.source_editor().is_some());
    press(&mut shell, KeyCode::Esc, KeyModifiers::NONE);
    shell.notify_edit_requests();
    shell.poll_edit_requests();
    assert_eq!(
        operations.inspect_edit_request(&request.id).unwrap().state,
        State::Applied
    );
    assert!(repo.read_file("notes/a.md").starts_with(b"## Title"));
    press(&mut shell, KeyCode::Char('C'), KeyModifiers::NONE);
    assert_eq!(shell.view(), margin::tui::View::Changes);
    let notes = operations.list_notes(None).data.unwrap();
    assert!(
        notes
            .notes
            .iter()
            .any(|n| n.text.contains("A smaller heading"))
    );
    assert!(
        String::from_utf8(repo.git_ok(["log", "-1", "--format=%B"]))
            .unwrap()
            .contains("Margin-Kind: draft")
    );
    assert_eq!(
        repo.git_ok(["log", "--format=%H", "--grep=Margin-Kind: review"]),
        b""
    );
}
#[test]
fn failed_checkpoint_after_save_keeps_saved_bytes_and_reports_both_outcomes() {
    let repo = adopted(ObjectFormat::Sha1);
    let git = adapter(&repo);
    let operations = Operations::new(&git);
    repo.set_local_config("margin.captureAfterEdit", "true");
    // Git executes this hook only after the working-file save.
    let hook = repo.file_path(".git/hooks/pre-commit");
    std::fs::write(&hook, b"#!/bin/sh\nexit 1\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    press(&mut shell, KeyCode::Char('e'), KeyModifiers::NONE);
    shell.handle_event(&Event::Paste("Saved text".into()));
    press(&mut shell, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert!(repo.read_file("notes/a.md").starts_with(b"Saved text"));
    assert!(!shell.source_editor().unwrap().dirty());
    assert!(shell.notice().unwrap().contains("Saved; checkpoint failed"));
}
#[test]
fn versioned_socket_status_notification_and_unknown_versions_are_bounded() {
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
    };
    let dir = margin::desk::directory().unwrap();
    let path = dir.join("viewer.sock");
    let server = margin::desk::Server::bind(path.clone()).unwrap();
    server.set_busy(true);
    let status = margin::desk::control_at(&path, margin::desk::Control::Status).unwrap();
    assert_eq!(status["busy"], true);
    assert_eq!(status["schema_version"], 1);
    assert!(
        margin::desk::control_at(&path, margin::desk::Control::Notify).unwrap()["notified"]
            .as_bool()
            .unwrap()
    );
    assert!(server.take_notification());
    assert!(!server.take_notification());
    let mut stream = UnixStream::connect(&path).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(1)))
        .unwrap();
    stream
        .write_all(br#"{"schema_version":99,"request":{"operation":"status"}}"#)
        .unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.contains("unsupported desk schema version"));
}

#[test]
fn generic_agent_cli_and_claude_hook_exchange_requests_without_approving() {
    use std::process::Command;
    let repo = adopted(ObjectFormat::Sha1);
    let git = adapter(&repo);
    let operations = Operations::new(&git);
    let request = operations
        .create_edit_request(
            operations.edit_document("notes/a.md").unwrap(),
            None,
            "Keep the example",
        )
        .unwrap();
    let head = repo.git_ok(["rev-parse", "HEAD"]);
    let event = serde_json::json!({"hook_event_name":"UserPromptSubmit","cwd":repo.path()});
    let hook = margin::integrations::claude::run(
        &operations,
        repo.path(),
        &serde_json::to_vec(&event).unwrap(),
    );
    assert_eq!(hook.exit, 0);
    let response: serde_json::Value =
        serde_json::from_slice(hook.stdout.as_ref().unwrap()).unwrap();
    let context = response["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(context.contains(&request.id));
    assert!(context.contains("Keep the example"));
    let run = |args: &[&str]| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_margin"));
        cmd.current_dir(repo.path()).args(args);
        for (key, _) in std::env::vars_os() {
            if support::env::is_scrubbed(&key) {
                cmd.env_remove(key);
            }
        }
        let output = cmd.envs(repo.env().vars()).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let list = run(&["requests", "list", "--json"]);
    assert_eq!(list["data"][0]["id"], request.id);
    let inspected = run(&["requests", "inspect", &request.id, "--json"]);
    assert_eq!(inspected["data"]["base"]["text"], request.base.text);
    let claim = run(&[
        "requests",
        "claim",
        &request.id,
        "--owner",
        "session",
        "--json",
    ]);
    assert_eq!(claim["effects"]["integration"], "changed");
    let retried = run(&[
        "requests",
        "claim",
        &request.id,
        "--owner",
        "session",
        "--json",
    ]);
    assert_eq!(retried["effects"]["integration"], "unchanged");
    let replacement = repo.write_file("replacement.txt", b"Agent result");
    let submitted = run(&[
        "requests",
        "submit",
        &request.id,
        "--owner",
        "session",
        "--input",
        replacement.to_str().unwrap(),
        "--explanation",
        "Keep the example",
        "--json",
    ]);
    assert_eq!(submitted["data"]["state"], "result_ready");
    assert_eq!(submitted["data"]["applied"], false);
    assert_eq!(repo.read_file("notes/a.md"), request.base.text.as_bytes());
    assert_eq!(repo.git_ok(["rev-parse", "HEAD"]), head);
    assert!(
        operations
            .inspect_edit_request(&request.id)
            .unwrap()
            .explanation
            .unwrap()
            .contains("Keep the example")
    );
}
#[test]
fn source_editing_runs_on_a_terminal_and_restores_terminal_modes() {
    use std::{path::Path, time::Duration};
    let repo = adopted(ObjectFormat::Sha1);
    let vars: Vec<_> = repo
        .env()
        .vars()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    let mut pty = support::pty::Pty::spawn(
        Path::new(env!("CARGO_BIN_EXE_margin")),
        &["tui", "--reduced-updates"],
        &vars,
        repo.path(),
        110,
        32,
    )
    .unwrap();
    let limit = Duration::from_secs(10);
    assert!(pty.wait_for("Title", limit));
    pty.send_key(b"e");
    assert!(pty.wait_for("EDITING", limit));
    pty.send_key(b"\x1b[200~qK:r pasted\x1b[201~");
    pty.pump(Duration::from_millis(100));
    let deadline = std::time::Instant::now() + limit;
    while !repo.read_file("notes/a.md").starts_with(b"qK:r pasted") {
        assert!(std::time::Instant::now() < deadline, "autosave did not run");
        pty.pump(Duration::from_millis(50));
    }
    assert!(repo.read_file("notes/a.md").starts_with(b"qK:r pasted"));
    // Home, Shift+Right, and F2 use standard terminal sequences.
    pty.send_key(b"\x1b[H\x1b[1;2C\x1bOQ");
    assert!(
        pty.wait_for("ordinary keys", limit),
        "{}",
        String::from_utf8_lossy(pty.raw_output())
    );
    pty.send_key(b"\x1b[200~Improve this character\x1b[201~");
    pty.send_key(b"\r");
    assert!(pty.wait_for("queued", limit));
    let git = adapter(&repo);
    let operations = Operations::new(&git);
    let request = operations.list_edit_requests().unwrap().pop().unwrap();
    assert_eq!((request.start, request.end), (0, 1));
    pty.resize(80, 24);
    pty.pump(Duration::from_millis(100));
    pty.send_key(b"\x1b");
    pty.send_key(b"q");
    assert_eq!(pty.wait_exit(limit).unwrap().code(), Some(0));
    assert!(pty.terminal_is_cooked());
    assert!(pty.shows_raw("\x1b[?1049l"));
}

#[test]
fn two_desks_apply_only_at_the_origin_and_preserve_its_open_editor() {
    use std::{path::Path, time::Duration};
    let repo = adopted(ObjectFormat::Sha1);
    let git = adapter(&repo);
    let operations = Operations::new(&git);
    let head = repo.git_ok(["rev-parse", "HEAD"]);
    let original = repo.read_file("notes/a.md");
    let one = margin::desk::directory().unwrap().join("viewer.sock");
    let two = margin::desk::directory().unwrap().join("viewer.sock");
    let spawn = |socket: &Path| {
        let mut vars: Vec<_> = repo
            .env()
            .vars()
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();
        vars.push(("MARGIN_DESK_SOCKET".into(), socket.display().to_string()));
        support::pty::Pty::spawn(
            Path::new(env!("CARGO_BIN_EXE_margin")),
            &["tui", "--reduced-updates"],
            &vars,
            repo.path(),
            110,
            32,
        )
        .unwrap()
    };
    let mut first = spawn(&one);
    let mut second = spawn(&two);
    let limit = Duration::from_secs(10);
    assert!(first.wait_for("Title", limit));
    assert!(second.wait_for("Title", limit));
    first.send_key(b"A");
    assert!(
        first.wait_for("Ask agent", limit),
        "{}",
        String::from_utf8_lossy(first.raw_output())
    );
    first.send_key(b"\x1b[200~Revise this document\x1b[201~");
    first.send_key(b"\r");
    assert!(first.wait_for("queued", limit));
    first.send_key(b"e");
    assert!(first.wait_for("EDITING", limit));
    first.send_key(b"\x1bOQ");
    assert!(first.wait_for("ordinary keys", limit));
    let request = operations.list_edit_requests().unwrap().pop().unwrap();
    assert_eq!(request.desk.as_deref(), one.to_str());
    operations.claim_edit_request(&request.id, "agent").unwrap();
    operations
        .submit_edit_result(
            &request.id,
            "agent",
            "# Agent result\n",
            "Make the title direct",
        )
        .unwrap();
    first.pump(Duration::from_millis(2200));
    second.pump(Duration::from_millis(2200));
    assert_eq!(
        operations.inspect_edit_request(&request.id).unwrap().state,
        State::ResultReady
    );
    assert_eq!(repo.read_file("notes/a.md"), original);
    first.send_key(b"\x1b");
    let deadline = std::time::Instant::now() + limit;
    while operations.inspect_edit_request(&request.id).unwrap().state != State::Applied
        && std::time::Instant::now() < deadline
    {
        first.pump(Duration::from_millis(50));
    }
    assert_eq!(
        operations.inspect_edit_request(&request.id).unwrap().state,
        State::Applied
    );
    assert_eq!(repo.read_file("notes/a.md"), b"# Agent result\n");
    let deadline = std::time::Instant::now() + limit;
    while repo.git_ok(["rev-parse", "HEAD"]) == head && std::time::Instant::now() < deadline {
        first.pump(Duration::from_millis(50));
    }
    assert_ne!(repo.git_ok(["rev-parse", "HEAD"]), head);
    assert!(
        repo.git_ok(["log", "--format=%H", "--grep=Margin-Kind: review"])
            .is_empty()
    );
    first.send_key(b"\x11");
    second.send_key(b"q");
    assert_eq!(first.wait_exit(limit).unwrap().code(), Some(0));
    assert_eq!(second.wait_exit(limit).unwrap().code(), Some(0));
}
#[test]
fn a_complete_long_source_line_renders_its_end_without_truncating_the_buffer() {
    let repo = adopted(ObjectFormat::Sha1);
    let git = adapter(&repo);
    let operations = Operations::new(&git);
    let text = "a".repeat(margin::editing::MAX_SOURCE_BYTES - 4) + " END";
    repo.write_file("notes/a.md", text.as_bytes());
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    let start = std::time::Instant::now();
    press(&mut shell, KeyCode::Char('e'), KeyModifiers::NONE);
    let load = start.elapsed();
    let start = std::time::Instant::now();
    press(&mut shell, KeyCode::End, KeyModifiers::NONE);
    let page = screen(&mut shell, 110, 30);
    let render = start.elapsed();
    assert!(page.contains("END"));
    assert_eq!(shell.source_editor().unwrap().text, text);
    eprintln!("1 MiB source: load={load:?}, End plus render={render:?}");
}

#[test]
fn empty_files_final_newlines_limits_and_private_recovery_are_explicit() {
    let repo = adopted(ObjectFormat::Sha1);
    let git = adapter(&repo);
    let operations = Operations::new(&git);
    for text in ["", "No final newline", "One\nTwo\n", "One\r\nTwo\r\n"] {
        repo.write_file("notes/a.md", text.as_bytes());
        let base = operations.edit_document("notes/a.md").unwrap();
        let saved = operations.save_document(&base, text).unwrap();
        assert_eq!(saved.text, text);
        assert_eq!(repo.read_file("notes/a.md"), text.as_bytes());
    }
    let base = operations.edit_document("notes/a.md").unwrap();
    operations
        .retain_edit_buffer("draft-permissions", &base, "Dirty")
        .unwrap();
    use std::os::unix::fs::PermissionsExt;
    let root = git.git_path("margin-editing").unwrap();
    assert_eq!(
        std::fs::metadata(&root).unwrap().permissions().mode() & 0o077,
        0
    );
    assert_eq!(
        std::fs::metadata(root.join("recovery/draft-permissions.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o077,
        0
    );
    assert!(
        operations
            .create_edit_request(base.clone(), None, &"x".repeat(8001))
            .is_err()
    );
    operations.forget_edit_buffer("draft-permissions").unwrap();
    repo.set_mode("notes/a.md", 0o444);
    assert!(operations.edit_document("notes/a.md").is_err());
    repo.set_mode("notes/a.md", 0o644);
    repo.write_file(
        "notes/a.md",
        &vec![b'a'; margin::editing::MAX_SOURCE_BYTES + 1],
    );
    assert_eq!(
        operations.edit_document("notes/a.md").unwrap_err().code,
        "LIMIT_EXCEEDED"
    );
}

#[test]
fn conflict_reconciliation_accepts_only_the_disk_version_the_writer_compared() {
    let repo = adopted(ObjectFormat::Sha1);
    let git = adapter(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    press(&mut shell, KeyCode::Char('e'), KeyModifiers::NONE);
    shell.handle_event(&Event::Paste("Writer text\n".into()));
    let buffer = shell.source_editor().unwrap().text.clone();
    repo.write_file("notes/a.md", b"External version one");
    press(&mut shell, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert_eq!(shell.source_editor().unwrap().text, buffer);
    assert!(shell.notice().unwrap().contains("STALE_STATE"));
    press(&mut shell, KeyCode::Char('d'), KeyModifiers::CONTROL);
    repo.write_file("notes/a.md", b"External version two");
    press(&mut shell, KeyCode::Char('b'), KeyModifiers::CONTROL);
    assert!(shell.notice().unwrap().contains("Disk changed again"));
    press(&mut shell, KeyCode::Char('d'), KeyModifiers::CONTROL);
    press(&mut shell, KeyCode::Char('b'), KeyModifiers::CONTROL);
    assert_eq!(
        shell.source_editor().unwrap().base.text,
        "External version two"
    );
    assert_eq!(shell.source_editor().unwrap().text, buffer);
    press(&mut shell, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert_eq!(repo.read_file("notes/a.md"), buffer.as_bytes());
}
#[test]
fn a_disappeared_file_still_retains_the_failed_save_buffer() {
    let repo = adopted(ObjectFormat::Sha1);
    let git = adapter(&repo);
    let operations = Operations::new(&git);
    let base = operations.edit_document("notes/a.md").unwrap();
    std::fs::remove_file(repo.file_path("notes/a.md")).unwrap();
    assert!(operations.save_document(&base, "Retain this text").is_err());
    let dir = git.git_path("margin-editing").unwrap().join("recovery");
    let entry = std::fs::read_dir(&dir).unwrap().next().unwrap().unwrap();
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(entry.path()).unwrap()).unwrap();
    assert_eq!(record["edited"], "Retain this text");
    repo.write_file("notes/a.md", base.text.as_bytes());
    assert_eq!(
        operations
            .recover_edit_buffer("notes/a.md")
            .unwrap()
            .unwrap()
            .edited,
        "Retain this text"
    );
    assert_eq!(repo.read_file("notes/a.md"), base.text.as_bytes());
}

#[test]
fn hook_request_announcements_remain_bounded_with_escaped_instructions() {
    let repo = adopted(ObjectFormat::Sha1);
    let git = adapter(&repo);
    let operations = Operations::new(&git);
    let base = operations.edit_document("notes/a.md").unwrap();
    for _ in 0..5 {
        operations
            .create_edit_request(base.clone(), None, &"\u{1b}".repeat(8000))
            .unwrap();
    }
    let context = operations.edit_request_feedback().unwrap().unwrap();
    assert!(context.chars().count() < 9000);
    assert!(!context.contains('\u{1b}'));
    assert!(context.contains("margin requests inspect"));
}

#[test]
fn editor_help_preserves_conflict_selection_and_literal_input() {
    let repo = adopted(ObjectFormat::Sha1);
    let git = adapter(&repo);
    let ops = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &ops, &TuiOptions::default());
    press(&mut shell, KeyCode::Char('e'), KeyModifiers::NONE);
    shell.handle_event(&Event::Paste("My text ".into()));
    press(&mut shell, KeyCode::Left, KeyModifiers::SHIFT);
    repo.write_file("notes/a.md", b"Changed externally");
    press(&mut shell, KeyCode::Char('s'), KeyModifiers::CONTROL);
    let editor = shell.source_editor().unwrap();
    let state = (
        editor.text.clone(),
        editor.cursor,
        editor.anchor,
        editor.conflict.clone(),
    );
    press(&mut shell, KeyCode::F(1), KeyModifiers::NONE);
    assert!(screen(&mut shell, 60, 16).contains("Autosave paused"));
    press(&mut shell, KeyCode::Char('/'), KeyModifiers::NONE);
    shell.handle_event(&Event::Paste("Ctrl+Home".into()));
    press(&mut shell, KeyCode::Enter, KeyModifiers::NONE);
    assert!(screen(&mut shell, 60, 16).contains("Ctrl+Home/End"));
    press(&mut shell, KeyCode::Esc, KeyModifiers::NONE);
    let editor = shell.source_editor().unwrap();
    assert_eq!(
        (
            editor.text.clone(),
            editor.cursor,
            editor.anchor,
            editor.conflict.clone()
        ),
        state
    );
    press(&mut shell, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert!(screen(&mut shell, 80, 24).contains("Disk comparison"));
    shell.handle_event(&Event::Paste("must not edit".into()));
    press(&mut shell, KeyCode::End, KeyModifiers::NONE);
    press(&mut shell, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(shell.source_editor().unwrap().text, state.0);
    assert_eq!(repo.read_file("notes/a.md"), b"Changed externally");
}

#[test]
fn editor_word_movement_respects_unicode_and_document_selection() {
    let repo = adopted(ObjectFormat::Sha1);
    let git = adapter(&repo);
    let ops = Operations::new(&git);
    let mut editor = margin::tui::editor::Editor::new(ops.edit_document("notes/a.md").unwrap());
    editor.key(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    editor.insert("cafe\u{301} 👩‍💻\r\nsecond word");
    editor.key(&KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));
    editor.key(&KeyEvent::new(
        KeyCode::Right,
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    assert_eq!(&editor.text[editor.selection()], "cafe\u{301} 👩‍💻\r\n");
    editor.key(&KeyEvent::new(
        KeyCode::End,
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    assert_eq!(editor.selection(), 0..editor.text.len());
    editor.key(&KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
    assert_eq!(&editor.text[editor.cursor..], "word");
    assert!(editor.anchor.is_none());
}

#[test]
fn source_clicks_map_wide_graphemes_tabs_and_selection_and_wheel_keeps_cursor() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let repo = adopted(ObjectFormat::Sha1);
    let git = adapter(&repo);
    let ops = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &ops, &TuiOptions::default());
    press(&mut shell, KeyCode::Char('e'), KeyModifiers::NONE);
    press(&mut shell, KeyCode::Char('a'), KeyModifiers::CONTROL);
    shell.handle_event(&Event::Paste(format!(
        "a\t界e\u{301}X\n{}",
        "following line\n".repeat(40)
    )));
    press(&mut shell, KeyCode::Home, KeyModifiers::CONTROL);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|f| render::draw(&mut shell, f)).unwrap();
    let cell = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .position(|c| c.symbol() == "界")
        .unwrap();
    let x = (cell % 80) as u16;
    let y = (cell / 80) as u16;
    shell.handle_event(&Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: x + 1,
        row: y,
        modifiers: KeyModifiers::NONE,
    }));
    assert_eq!(shell.source_editor().unwrap().cursor, 2);
    terminal.draw(|f| render::draw(&mut shell, f)).unwrap();
    shell.handle_event(&Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: x + 3,
        row: y,
        modifiers: KeyModifiers::SHIFT,
    }));
    assert_eq!(
        &shell.source_editor().unwrap().text[shell.source_editor().unwrap().selection()],
        "界e\u{301}"
    );
    let cursor = shell.source_editor().unwrap().cursor;
    shell.handle_event(&Event::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }));
    screen(&mut shell, 80, 24);
    assert_eq!(shell.source_editor().unwrap().cursor, cursor);
    assert_eq!(shell.source_editor().unwrap().viewport_top.get(), 3);
}
