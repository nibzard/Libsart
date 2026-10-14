//! Real Git, CLI, hook, and TUI acceptance for SPEC 12.7.
mod support;

use std::process::{Command, Output, Stdio};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use margin::app::{ObservedEffect, Operations};
use margin::git::Git;
use margin::notes::{AppendRequest, Author, Envelope, Note, Snapshot};
use margin::tui::{Shell, TuiOptions, render};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::{Value, json};
use support::repo::{ObjectFormat, RepoOptions, TestRepo};
use support::{oracle, text};

fn command(repo: &TestRepo, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_margin"));
    command
        .current_dir(repo.path())
        .args(args)
        .stdin(Stdio::null());
    for (key, _) in std::env::vars_os() {
        if support::env::is_scrubbed(&key) {
            command.env_remove(key);
        }
    }
    command.envs(repo.env().vars());
    command
}

fn cli(repo: &TestRepo, args: &[&str]) -> Output {
    command(repo, args).output().unwrap()
}
fn data(repo: &TestRepo, args: &[&str]) -> Value {
    let output = cli(repo, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    serde_json::from_slice::<Value>(&output.stdout).unwrap()["data"].clone()
}
fn git(repo: &TestRepo) -> Git {
    Git::from_env(repo.env().vars().clone()).at(repo.path())
}
fn adopted(format: ObjectFormat, auto: bool) -> TestRepo {
    let repo = TestRepo::init_with(RepoOptions::new().with_object_format(format));
    repo.write_file("notes/a.md", b"# Title\n\nOriginal passage.\n");
    repo.commit_all("import");
    let mut args = vec!["init", "--scope", "notes/", "--json"];
    if auto {
        args.push("--auto-checkpoint");
    }
    data(&repo, &args);
    repo.write_file("notes/a.md", b"# Title\n\nRevised passage.\n");
    data(&repo, &["checkpoint", "--json"]);
    repo
}
fn native(repo: &TestRepo, commit: &str, notes: Vec<Note>) {
    let bytes = serde_json::to_vec(&Envelope {
        schema_version: 1,
        notes,
    })
    .unwrap();
    repo.git_stdin(
        ["notes", "--ref=margin", "add", "-f", "-F", "-", commit],
        &bytes,
    );
}
fn note(id: &str, author: Author, text: &str) -> Note {
    Note {
        id: id.into(),
        by: author,
        group: None,
        text: text.into(),
    }
}
fn press(shell: &mut Shell<'_>, code: KeyCode) {
    shell.handle_event(&Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}
fn screen(shell: &mut Shell<'_>) -> String {
    let mut terminal = Terminal::new(TestBackend::new(110, 50)).unwrap();
    terminal.draw(|frame| render::draw(shell, frame)).unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .chunks(110)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn cli_and_native_notes_preserve_content_and_comparison_in_both_formats() {
    for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
        let repo = adopted(format, false);
        let before = oracle::snapshot(&repo);
        let index = oracle::index_bytes(&repo);
        let status = repo.git_ok(["status", "--porcelain"]);
        let diff = data(&repo, &["diff", "--json"]);
        let status_data = data(&repo, &["status", "--json"]);
        let group = diff["changes"][0]["change_id"].as_str().unwrap();
        let commit = text(&repo.git_ok(["rev-parse", "HEAD"]));
        native(
            &repo,
            &commit,
            vec![note("native", Author::Agent, "Native rationale")],
        );
        let saved = data(
            &repo,
            &[
                "note",
                "--group",
                group,
                "--text",
                "CLI rationale",
                "--json",
            ],
        );
        assert_eq!(saved["commit"], commit);
        assert_eq!(saved["note"]["group"], group);
        let notes = data(&repo, &["notes", "--json"]);
        assert_eq!(notes["object_format"], format.as_str());
        assert_eq!(notes["notes"].as_array().unwrap().len(), 2);
        assert_eq!(notes["notes"][0]["text"], "Native rationale");
        let independent: Value =
            serde_json::from_slice(&repo.git_ok(["notes", "--ref=margin", "show", &commit]))
                .unwrap();
        assert_eq!(independent["notes"], notes["notes"]);
        assert_eq!(data(&repo, &["diff", "--json"]), diff);
        assert_eq!(data(&repo, &["status", "--json"]), status_data);
        assert_eq!(repo.git_ok(["status", "--porcelain"]), status);
        assert_eq!(oracle::index_bytes(&repo), index);
        let after = oracle::snapshot(&repo);
        assert_eq!(before.head, after.head);
        assert!(before.working_diff(&after).is_empty());
        assert!(
            oracle::refs(&repo)
                .keys()
                .all(|r| !r.starts_with("refs/notes/margin-tmp-"))
        );
        let history = data(&repo, &["log", "--json", "--all-checkpoints"]);
        assert_eq!(history["entries"][0]["notes"], notes["notes"]);
        let adapter = git(&repo);
        let operations = Operations::new(&adapter);
        let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
        shell.load_pending_background();
        press(&mut shell, KeyCode::Char('2'));
        let display = screen(&mut shell);
        assert!(display.contains("Native rationale"));
        assert!(display.contains("CLI rationale"));
        assert!(display.contains("Note (agent)"));
        press(&mut shell, KeyCode::Char('3'));
        assert!(screen(&mut shell).contains("Native rationale"));
        press(&mut shell, KeyCode::Char('1'));
        assert!(!screen(&mut shell).contains("Native rationale"));
    }
}

#[test]
fn schema_export_needs_no_repository_and_matches_shipped_bytes() {
    let repo = TestRepo::init();
    let outside = tempfile::tempdir().unwrap();
    let output = command(&repo, &["notes", "--schema"])
        .current_dir(outside.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, margin::notes::SCHEMA.as_bytes());
    serde_json::from_slice::<Value>(&output.stdout).unwrap();
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    let docs = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/notes-channel.md"
    ))
    .unwrap();
    let block = docs
        .split("```markdown\n")
        .nth(1)
        .unwrap()
        .split("\n```")
        .next()
        .unwrap();
    assert_eq!(block, margin::integrations::instructions::TEMPLATE);
}

#[test]
fn invalid_native_envelope_is_disclosed_and_never_overwritten() {
    let repo = adopted(ObjectFormat::Sha1, false);
    let head = text(&repo.git_ok(["rev-parse", "HEAD"]));
    for notes in [
        vec![
            note("same", Author::Agent, "First"),
            note("same", Author::Writer, "Second"),
        ],
        vec![note("large", Author::Agent, &"x".repeat(2001))],
    ] {
        native(&repo, &head, notes);
        let before = repo.git_ok(["rev-parse", "refs/notes/margin"]);
        assert!(
            data(&repo, &["notes", "--json"])["unreadable"]
                .as_str()
                .unwrap()
                .contains("unreadable")
        );
        let output = cli(&repo, &["note", "--text", "Must not overwrite", "--json"]);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(repo.git_ok(["rev-parse", "refs/notes/margin"]), before);
    }
}

#[test]
fn entry_limits_and_group_validation_refuse_without_publishing() {
    let repo = adopted(ObjectFormat::Sha1, false);
    let head = text(&repo.git_ok(["rev-parse", "HEAD"]));
    native(
        &repo,
        &head,
        (0..50)
            .map(|i| note(&i.to_string(), Author::Agent, "Same text"))
            .collect(),
    );
    let before = repo.git_ok(["rev-parse", "refs/notes/margin"]);
    for args in [
        vec!["note", "--text", "Fifty-first", "--json"],
        vec!["note", "--text", "Why", "--group", "bogus", "--json"],
    ] {
        assert_eq!(cli(&repo, &args).status.code(), Some(2));
        assert_eq!(repo.git_ok(["rev-parse", "refs/notes/margin"]), before);
    }
}

#[test]
fn writer_field_is_paste_safe_cancelable_and_preserves_decisions() {
    let repo = adopted(ObjectFormat::Sha1, false);
    let adapter = git(&repo);
    let operations = Operations::new(&adapter);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    shell.load_pending_background();
    press(&mut shell, KeyCode::Char('2'));
    press(&mut shell, KeyCode::Char('K'));
    press(&mut shell, KeyCode::Char('N'));
    shell.handle_event(&Event::Paste("qKr\nRestore the example".into()));
    press(&mut shell, KeyCode::Esc);
    assert!(Snapshot::load(&adapter).unwrap().head.is_none());
    let before = oracle::snapshot(&repo);
    press(&mut shell, KeyCode::Char('N'));
    shell.handle_event(&Event::Paste("Restore the example".into()));
    press(&mut shell, KeyCode::Enter);
    let display = screen(&mut shell);
    assert!(display.contains("Your note"));
    assert!(display.contains("Restore the example"));
    assert!(display.contains("Kept"));
    let notes = data(&repo, &["notes", "--json"]);
    assert_eq!(notes["notes"][0]["by"], "writer");
    assert!(notes["notes"][0]["group"].is_string());
    let after = oracle::snapshot(&repo);
    assert_eq!(before.head, after.head);
    assert!(before.working_diff(&after).is_empty());
}

#[test]
fn polling_updates_notes_without_repinning_or_changing_decisions() {
    let repo = adopted(ObjectFormat::Sha1, false);
    let adapter = git(&repo);
    let operations = Operations::new(&adapter);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    shell.load_pending_background();
    press(&mut shell, KeyCode::Char('2'));
    press(&mut shell, KeyCode::Char('K'));
    let generation = shell.generation();
    data(&repo, &["note", "--text", "Arrived from outside", "--json"]);
    let mut worker = margin::tui::refresh::RefreshWorker::spawn(adapter.clone()).unwrap();
    worker.request(margin::tui::refresh::WatchJob {
        generation,
        watch: shell.watch(),
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(reply) = worker.receive() {
            shell.observe_inspection(reply);
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let display = screen(&mut shell);
    assert!(display.contains("Arrived from outside"));
    assert!(display.contains("Kept"));
    assert!(!display.contains("Stale"));
    assert_eq!(shell.generation(), generation);
}

#[test]
fn orphaned_group_is_stated_once_and_remains_in_history() {
    let repo = adopted(ObjectFormat::Sha1, false);
    let head = text(&repo.git_ok(["rev-parse", "HEAD"]));
    let mut entry = note("orphan", Author::Writer, "Earlier comparison feedback");
    entry.group = Some(format!("sha1:{}", "f".repeat(40)));
    native(&repo, &head, vec![entry]);
    let adapter = git(&repo);
    let operations = Operations::new(&adapter);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    shell.load_pending_background();
    press(&mut shell, KeyCode::Char('2'));
    let display = screen(&mut shell);
    assert_eq!(display.matches("Earlier comparison feedback").count(), 1);
    assert!(display.contains("group is outside this comparison"));
    press(&mut shell, KeyCode::Char('3'));
    assert!(screen(&mut shell).contains("Earlier comparison feedback"));
}

#[test]
fn repinning_discloses_the_previous_candidates_group_note_once() {
    let repo = adopted(ObjectFormat::Sha1, false);
    let diff = data(&repo, &["diff", "--json"]);
    let group = diff["changes"][0]["change_id"].as_str().unwrap();
    data(
        &repo,
        &[
            "note",
            "--group",
            group,
            "--text",
            "Rationale before the repin",
            "--json",
        ],
    );
    let adapter = git(&repo);
    let operations = Operations::new(&adapter);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    shell.load_pending_background();
    press(&mut shell, KeyCode::Char('2'));
    repo.write_file("notes/a.md", b"# Title\n\nAnother revision.\n");
    data(&repo, &["checkpoint", "--json"]);
    press(&mut shell, KeyCode::Char('R'));
    let display = screen(&mut shell);
    assert_eq!(display.matches("Rationale before the repin").count(), 1);
    assert!(display.contains("group is outside this comparison"));
    press(&mut shell, KeyCode::Char('3'));
    press(&mut shell, KeyCode::Char('a'));
    assert!(screen(&mut shell).contains("Rationale before the repin"));
}

#[test]
fn malformed_feedback_does_not_suppress_valid_feedback_on_an_older_commit() {
    let repo = adopted(ObjectFormat::Sha1, false);
    let old = text(&repo.git_ok(["rev-parse", "HEAD~1"]));
    let head = text(&repo.git_ok(["rev-parse", "HEAD"]));
    native(
        &repo,
        &old,
        vec![note("good", Author::Writer, "Valid older feedback")],
    );
    native(&repo, &head, vec![note("bad", Author::Writer, "")]);
    let adapter = git(&repo);
    let operations = Operations::new(&adapter);
    let feedback = margin::notes::feedback(&operations).unwrap().unwrap();
    assert!(feedback.contains("Valid older feedback"));
    assert!(feedback.contains("Unreadable note envelopes were skipped"));
    assert!(feedback.len() <= margin::notes::MAX_CONTEXT_BYTES);
}

#[test]
fn unicode_character_bound_and_display_escaping_match_the_schema() {
    let repo = adopted(ObjectFormat::Sha1, false);
    let text = "🖋".repeat(2000);
    data(&repo, &["note", "--text", &text, "--json"]);
    assert_eq!(data(&repo, &["notes", "--json"])["notes"][0]["text"], text);
    assert_eq!(
        cli(&repo, &["note", "--text", &format!("{text}x"), "--json"])
            .status
            .code(),
        Some(2)
    );
    data(
        &repo,
        &[
            "note",
            "--text",
            "Escape: \u{1b}[31m and bidi \u{202e}",
            "--json",
        ],
    );
    let output = cli(&repo, &["notes"]);
    let display = String::from_utf8(output.stdout).unwrap();
    assert!(display.contains("<U+001B>"));
    assert!(display.contains("<U+202E>"));
    assert!(!display.contains('\u{1b}'));
    let head = support::text(&repo.git_ok(["rev-parse", "HEAD"]));
    native(
        &repo,
        &head,
        vec![note(
            "controls",
            Author::Agent,
            "Escape \u{1b}[31m, bidi \u{202e}",
        )],
    );
    let adapter = git(&repo);
    let operations = Operations::new(&adapter);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    shell.load_pending_background();
    for view in ['2', '3'] {
        press(&mut shell, KeyCode::Char(view));
        let display = screen(&mut shell);
        assert!(display.contains("<U+001B>"));
        assert!(display.contains("<U+202E>"));
        assert!(!display.contains('\u{1b}'));
        assert!(!display.contains('\u{202e}'));
    }
}

#[test]
fn notes_ref_writes_allow_dirty_and_staged_content_but_refuse_wrong_branch() {
    let repo = adopted(ObjectFormat::Sha1, false);
    repo.write_file("notes/a.md", b"# Title\n\nStaged text.\n");
    repo.git_ok(["add", "notes/a.md"]);
    repo.write_file("notes/a.md", b"# Title\n\nUncaptured text.\n");
    let index = oracle::index_bytes(&repo);
    let status = repo.git_ok(["status", "--porcelain"]);
    data(&repo, &["note", "--text", "Metadata only", "--json"]);
    assert_eq!(oracle::index_bytes(&repo), index);
    assert_eq!(repo.git_ok(["status", "--porcelain"]), status);
    repo.git_ok(["checkout", "-b", "other"]);
    let before = repo.git_ok(["rev-parse", "refs/notes/margin"]);
    assert_eq!(
        cli(&repo, &["note", "--text", "Wrong branch", "--json"])
            .status
            .code(),
        Some(3)
    );
    assert_eq!(repo.git_ok(["rev-parse", "refs/notes/margin"]), before);
}

#[test]
fn feedback_survives_capture_review_and_stops_at_a_new_start() {
    let repo = adopted(ObjectFormat::Sha1, true);
    let adapter = git(&repo);
    let operations = Operations::new(&adapter);
    let head = text(&repo.git_ok(["rev-parse", "HEAD"]));
    let result = operations.append_note(&AppendRequest {
        commit: Some(head.clone()),
        group: None,
        by: Author::Writer,
        text: "Restore the example".into(),
    });
    assert!(result.ok(), "{:?}", result.error);
    assert_eq!(result.effects.notes_ref, ObservedEffect::Changed);
    repo.write_file("notes/a.md", b"# Title\n\nAnother passage.\n");
    let event = json!({"hook_event_name":"UserPromptSubmit","cwd":repo.path()});
    let outcome = margin::integrations::claude::run(
        &operations,
        repo.path(),
        &serde_json::to_vec(&event).unwrap(),
    );
    assert_eq!(outcome.exit, 0);
    let response: Value = serde_json::from_slice(outcome.stdout.as_ref().unwrap()).unwrap();
    let context = response["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(context.contains("Restore the example"));
    assert!(context.contains(&head));
    assert_ne!(text(&repo.git_ok(["rev-parse", "HEAD"])), head);
    let diff = data(&repo, &["diff", "--json"]);
    let plan = json!({"schema_version":1,"request_id":"notes-review","review_id":diff["review_id"],"branch_ref":diff["branch_ref"],"scope_id":diff["scope_id"],"baseline_oid":diff["baseline_oid"],"candidate_oid":diff["candidate_oid"],"restore":[],"keep_remaining":true});
    repo.write_file("review.json", &serde_json::to_vec(&plan).unwrap());
    data(&repo, &["review", "--input", "review.json", "--json"]);
    assert!(
        margin::notes::feedback(&operations)
            .unwrap()
            .unwrap()
            .contains("Restore the example")
    );
    data(
        &repo,
        &["init", "--scope", "notes/", "--new-start", "--json"],
    );
    assert!(margin::notes::feedback(&operations).unwrap().is_none());
}

#[test]
fn manual_capture_delivers_feedback_and_blocked_capture_consumes_nothing() {
    let repo = adopted(ObjectFormat::Sha1, false);
    let adapter = git(&repo);
    let operations = Operations::new(&adapter);
    let head = text(&repo.git_ok(["rev-parse", "HEAD"]));
    native(
        &repo,
        &head,
        vec![note("feedback", Author::Writer, "Keep the example")],
    );
    let payload =
        serde_json::to_vec(&json!({"hook_event_name":"UserPromptSubmit","cwd":repo.path()}))
            .unwrap();
    let allowed = margin::integrations::claude::run(&operations, repo.path(), &payload);
    assert!(
        String::from_utf8(allowed.stdout.unwrap())
            .unwrap()
            .contains("Keep the example")
    );
    data(&repo, &["init", "--auto-checkpoint", "--json"]);
    repo.write_file("notes/a.md", b"# Title\n\nStaged edit.\n");
    repo.git_ok(["add", "notes/a.md"]);
    let blocked = margin::integrations::claude::run(&operations, repo.path(), &payload);
    let response: Value = serde_json::from_slice(blocked.stdout.as_ref().unwrap()).unwrap();
    assert_eq!(response["decision"], "block");
    assert!(response.get("hookSpecificOutput").is_none());
    repo.git_ok(["reset", "--mixed", "HEAD"]);
    let allowed = margin::integrations::claude::run(&operations, repo.path(), &payload);
    assert!(
        String::from_utf8(allowed.stdout.unwrap())
            .unwrap()
            .contains("Keep the example")
    );
}

#[test]
fn hook_context_is_bounded_and_truncates_multibyte_text_deterministically() {
    let repo = adopted(ObjectFormat::Sha1, false);
    let head = text(&repo.git_ok(["rev-parse", "HEAD"]));
    native(
        &repo,
        &head,
        (0..50)
            .map(|i| note(&i.to_string(), Author::Writer, &"🖋".repeat(2000)))
            .collect(),
    );
    let adapter = git(&repo);
    let operations = Operations::new(&adapter);
    let first = margin::notes::feedback(&operations).unwrap().unwrap();
    assert!(first.len() <= margin::notes::MAX_CONTEXT_BYTES);
    assert!(first.contains("omitted or shortened"));
    assert_eq!(
        first,
        margin::notes::feedback(&operations).unwrap().unwrap()
    );
}

#[test]
fn instruction_preflight_preserves_edited_files_and_hook_configuration() {
    let repo = adopted(ObjectFormat::Sha1, false);
    repo.write_file("CLAUDE.md", b"My rules\r\n");
    repo.write_file("AGENTS.md", b"Other rules\n");
    let adapter = git(&repo);
    let operations = Operations::new(&adapter);
    let dry = margin::integrations::claude::install(&operations, true);
    assert!(dry.ok());
    assert!(!repo.file_path(".claude/settings.local.json").exists());
    let installed = margin::integrations::claude::install(&operations, false);
    assert!(installed.ok(), "{:?}", installed.error);
    assert!(repo.read_file("CLAUDE.md").starts_with(b"My rules\r\n"));
    assert!(repo.read_file("AGENTS.md").starts_with(b"Other rules\n"));
    let again = margin::integrations::claude::install(&operations, false);
    assert!(again.data.unwrap().no_op);
    let mut edited = String::from_utf8(repo.read_file("AGENTS.md")).unwrap();
    edited = edited.replace("each meaningful", "every meaningful");
    repo.write_file("AGENTS.md", edited.as_bytes());
    let before = oracle::snapshot(&repo);
    for remove in [false, true] {
        let outcome = if remove {
            margin::integrations::claude::remove(&operations, false)
        } else {
            margin::integrations::claude::install(&operations, false)
        };
        assert_eq!(outcome.error.unwrap().code, "INSTRUCTION_CONFLICT");
        before.assert_unchanged(
            &oracle::snapshot(&repo),
            "edited block refused before any write",
        );
    }
    repo.write_file(
        "AGENTS.md",
        format!(
            "Other rules\n{}\n",
            margin::integrations::instructions::TEMPLATE
        )
        .as_bytes(),
    );
    assert!(margin::integrations::claude::remove(&operations, false).ok());
    assert_eq!(repo.read_file("CLAUDE.md"), b"My rules\r\n\r\n");
    assert_eq!(repo.read_file("AGENTS.md"), b"Other rules\n\n");
    assert!(
        margin::integrations::claude::remove(&operations, false)
            .data
            .unwrap()
            .no_op
    );
}

#[cfg(unix)]
fn racing_adapter(repo: &TestRepo, target: &str, entries: usize, repeat: bool) -> Git {
    use std::os::unix::fs::PermissionsExt;
    fn quote(raw: &str) -> String {
        format!("'{}'", raw.replace('\'', "'\\''"))
    }
    let envelope = repo.root().join("race-envelope.json");
    std::fs::write(
        &envelope,
        serde_json::to_vec(&Envelope {
            schema_version: 1,
            notes: (0..entries)
                .map(|i| {
                    note(
                        &format!("native-{i}"),
                        Author::Agent,
                        "Concurrent native entry",
                    )
                })
                .collect(),
        })
        .unwrap(),
    )
    .unwrap();
    let marker = repo.root().join("race-count");
    let wrapper = repo.root().join("racing-git");
    // Interleave stock Git immediately before Margin's expected-old publication.
    // The wrapper uses normal PATH; with_program does not modify it.
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = update-ref ] && [ \"$2\" = refs/notes/margin ]; then\n  count=0\n  if [ -f {marker} ]; then count=$(cat {marker}); fi\n  if [ {repeat} = yes ] || [ \"$count\" = 0 ]; then\n    count=$((count + 1))\n    printf '%s' \"$count\" > {marker}\n    git notes --ref=margin add -f -F {envelope} {target} || exit 90\n  fi\nfi\nexec git \"$@\"\n",
        marker = quote(&marker.to_string_lossy()),
        repeat = if repeat { "yes" } else { "no" },
        envelope = quote(&envelope.to_string_lossy()),
        target = quote(target)
    );
    std::fs::write(&wrapper, script).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    git(repo).with_program(wrapper)
}

#[test]
#[cfg(unix)]
fn atomic_append_preserves_native_updates_on_the_same_or_another_commit() {
    for same in [true, false] {
        let repo = adopted(ObjectFormat::Sha1, false);
        let head = text(&repo.git_ok(["rev-parse", "HEAD"]));
        let other = text(&repo.git_ok(["rev-parse", "HEAD~1"]));
        let target = if same { &head } else { &other };
        let adapter = racing_adapter(&repo, target, 1, false);
        let operations = Operations::new(&adapter);
        let result = operations.append_note(&AppendRequest {
            commit: None,
            group: None,
            by: Author::Agent,
            text: "Margin entry".into(),
        });
        assert!(result.ok(), "{:?}", result.error);
        let snapshot = Snapshot::load(&adapter).unwrap();
        let current = snapshot.read(&adapter, &head);
        assert!(current.notes.iter().any(|n| n.text == "Margin entry"));
        assert!(
            snapshot
                .read(&adapter, target)
                .notes
                .iter()
                .any(|n| n.text == "Concurrent native entry")
        );
        assert_eq!(current.notes.len(), if same { 2 } else { 1 });
        assert!(
            oracle::refs(&repo)
                .keys()
                .all(|r| !r.starts_with("refs/notes/margin-tmp-"))
        );
    }
}

#[test]
#[cfg(unix)]
fn atomic_append_rechecks_limits_and_refuses_after_three_conflicts() {
    for (entries, repeat, code) in [(50, false, "INVALID_NOTES"), (1, true, "NOTES_CONFLICT")] {
        let repo = adopted(ObjectFormat::Sha1, false);
        let head = text(&repo.git_ok(["rev-parse", "HEAD"]));
        let adapter = racing_adapter(&repo, &head, entries, repeat);
        let operations = Operations::new(&adapter);
        let result = operations.append_note(&AppendRequest {
            commit: None,
            group: None,
            by: Author::Agent,
            text: "Must not replace native entries".into(),
        });
        let error = result.error.expect("conflicting append refuses");
        assert_eq!(error.code, code);
        assert_eq!(error.retryable, repeat);
        assert_eq!(result.effects.notes_ref, ObservedEffect::Unchanged);
        let current = Snapshot::load(&adapter).unwrap().read(&adapter, &head);
        assert_eq!(current.notes.len(), entries);
        assert!(
            current
                .notes
                .iter()
                .all(|n| n.text == "Concurrent native entry")
        );
        assert!(
            oracle::refs(&repo)
                .keys()
                .all(|r| !r.starts_with("refs/notes/margin-tmp-"))
        );
        if repeat {
            assert_eq!(
                std::fs::read_to_string(repo.root().join("race-count")).unwrap(),
                "3"
            );
        }
    }
}
