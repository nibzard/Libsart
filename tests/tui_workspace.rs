//! Real-Git workspace protocols: local intent, typed navigation, and read-only pins.
mod support;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use margin::app::Operations;
use margin::git::Git;
use margin::tui::{Action, Focus, Overlay, Shell, TuiOptions, View, render, session};
use ratatui::{Terminal, backend::TestBackend};
use std::time::{Duration, Instant};
use support::{oracle, repo::TestRepo};

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}
fn press(shell: &mut Shell<'_>, c: char) {
    shell.handle_event(&key(KeyCode::Char(c)));
}
fn cli(repo: &TestRepo, args: &[&str]) {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_margin"));
    cmd.args(args).current_dir(repo.path());
    for (k, _) in std::env::vars_os() {
        if support::env::is_scrubbed(&k) {
            cmd.env_remove(k);
        }
    }
    let out = cmd.envs(repo.env().vars()).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
fn fixture() -> TestRepo {
    let repo = TestRepo::init();
    repo.write_file(
        "notes/a.md",
        b"# Duplicate\n\nFirst passage.\n\n## Target\n\nA long unchanged paragraph.\n",
    );
    repo.write_file(
        "notes/b.md",
        b"# Duplicate\n\nOther passage.\n\n## Target\n\nOther text.\n",
    );
    repo.write_file("outside.md", b"# Secret outside scope\n");
    repo.commit_all("Import");
    cli(&repo, &["init", "--scope", "notes/"]);
    repo.write_file(
        "notes/a.md",
        b"# Duplicate\n\nFirst edited passage.\n\n## Target\n\nA long unchanged paragraph.\n",
    );
    repo.write_file(
        "notes/b.md",
        b"# Duplicate\n\nOther edited passage.\n\n## Target\n\nOther text.\n",
    );
    cli(&repo, &["checkpoint"]);
    repo
}
fn adapter(repo: &TestRepo) -> Git {
    Git::from_env(repo.env().vars().clone()).at(repo.path())
}
fn draw(shell: &mut Shell<'_>, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| render::draw(shell, f)).unwrap();
    term.backend()
        .buffer()
        .content
        .chunks(w as usize)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}
fn settle(shell: &mut Shell<'_>) {
    let start = Instant::now();
    loop {
        shell.poll_palette();
        if shell.palette().ready() {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "palette timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn action(shell: &mut Shell<'_>, action: Action) {
    press(shell, ':');
    settle(shell);
    let i = shell
        .palette()
        .hits
        .iter()
        .position(|h| h.entry.action == Some(action))
        .unwrap();
    for _ in 0..i {
        shell.handle_event(&key(KeyCode::Down));
    }
    shell.handle_event(&key(KeyCode::Enter));
}
fn resume(shell: &mut Shell<'_>) {
    action(shell, Action::ResumeReview);
    assert!(matches!(
        shell.overlay(),
        Some(Overlay::SavedReviews { .. })
    ));
    shell.handle_event(&key(KeyCode::Enter));
}

#[test]
fn quit_restart_retains_plan_navigation_and_requires_a_fresh_confirmation() {
    let repo = fixture();
    let before = oracle::snapshot(&repo);
    let git = adapter(&repo);
    let ops = Operations::new(&git);
    let store = session::Store::from_git(&git).unwrap();
    let expected;
    {
        let mut s = Shell::open(repo.path(), &ops, &TuiOptions::default());
        assert!(!store.directory.exists());
        draw(&mut s, 150, 25);
        press(&mut s, '2');
        press(&mut s, 'r');
        press(&mut s, 'n');
        press(&mut s, 'K');
        press(&mut s, 's');
        expected = margin::tui::FinishSummary::open(&ops, s.changes().unwrap())
            .plan_document()
            .unwrap()
            .to_vec();
        press(&mut s, '1');
        for _ in 0..5 {
            press(&mut s, 'j');
        }
        s.handle_event(&key(KeyCode::Enter));
        press(&mut s, 'q');
        assert!(s.should_quit());
        assert!(s.notice().unwrap().contains("saved locally and unapplied"));
    }
    before.assert_unchanged(
        &oracle::snapshot(&repo),
        "saving and quitting never apply choices",
    );
    let record = store.list().unwrap().0[0].record.clone().unwrap();
    assert_eq!(record.decisions.len(), 2);
    assert_eq!(record.visited.len(), 2);
    assert_eq!(record.navigation.view, "Read");
    let mut s = Shell::open(repo.path(), &ops, &TuiOptions::default());
    assert_eq!(s.view(), View::Read);
    assert_eq!(s.focus(), Focus::Reading);
    assert!(s.progress_label().unwrap().contains("saved review"));
    draw(&mut s, 90, 25);
    resume(&mut s);
    assert!(s.finish().is_none());
    assert_eq!(s.changes().unwrap().proposed_count(), 2);
    assert!(s.changes().unwrap().source_view());
    draw(&mut s, 65, 25);
    assert_eq!(
        s.current_anchor().unwrap().line,
        record
            .navigation
            .anchors
            .iter()
            .find(|a| a.view == "Read")
            .unwrap()
            .line
    );
    let actual = margin::tui::FinishSummary::open(&ops, s.changes().unwrap())
        .plan_document()
        .unwrap()
        .to_vec();
    assert_eq!(actual, expected);
    press(&mut s, '2');
    s.handle_event(&key(KeyCode::Enter));
    assert!(s.finish().is_some());
    before.assert_unchanged(
        &oracle::snapshot(&repo),
        "resuming and opening summary do not apply",
    );
    s.handle_event(&key(KeyCode::Enter));
    assert!(s.finish_result().unwrap().ok());
    assert!(store.list().unwrap().0.is_empty());
}

#[test]
fn explicit_startup_options_win_and_other_viewers_cannot_take_or_delete_an_owned_draft() {
    let repo = fixture();
    let git = adapter(&repo);
    let ops = Operations::new(&git);
    let mut owner = Shell::open(repo.path(), &ops, &TuiOptions::default());
    press(&mut owner, '2');
    press(&mut owner, 'r');
    owner.flush_progress().unwrap();
    let mut peer = Shell::open(
        repo.path(),
        &ops,
        &TuiOptions {
            path: Some("notes/b.md".into()),
            view: View::History,
            explicit_view: true,
            ..Default::default()
        },
    );
    draw(&mut peer, 100, 24);
    resume(&mut peer);
    assert!(peer.notice().unwrap().contains("owned by another viewer"));
    press(&mut peer, 'd');
    peer.handle_event(&key(KeyCode::Enter));
    assert!(peer.notice().unwrap().contains("owned by another viewer"));
    drop(owner);
    peer.handle_event(&key(KeyCode::Esc));
    resume(&mut peer);
    assert_eq!(peer.view(), View::History);
    assert_eq!(peer.document().unwrap().path, "notes/b.md");
    assert_eq!(peer.changes().unwrap().proposed_count(), 1);
}

#[test]
fn changed_comparisons_scope_cycles_and_unknown_ids_never_migrate_decisions() {
    for scenario in ["head", "scope", "start", "unknown"] {
        let repo = fixture();
        let git = adapter(&repo);
        let ops = Operations::new(&git);
        let store = session::Store::from_git(&git).unwrap();
        {
            let mut s = Shell::open(repo.path(), &ops, &TuiOptions::default());
            press(&mut s, '2');
            press(&mut s, 'r');
            s.flush_progress().unwrap();
        }
        if scenario == "head" {
            repo.write_file("notes/a.md", b"# New\nNew candidate\n");
            repo.commit_all("New HEAD");
        }
        if scenario == "scope" {
            cli(&repo, &["init", "--scope", "notes/a.md"]);
            cli(&repo, &["init", "--scope", "notes/"]);
        }
        if scenario == "start" {
            cli(&repo, &["init", "--scope", "notes/", "--new-start"]);
        }
        if scenario == "unknown" {
            let entry = store.list().unwrap().0.remove(0);
            let mut record = entry.record.unwrap();
            record.decisions[0].id = "unknown-change".into();
            let o = store.acquire(&record.id).unwrap();
            o.save(&record).unwrap();
        }
        let mut s = Shell::open(repo.path(), &ops, &TuiOptions::default());
        resume(&mut s);
        assert!(
            s.notice().unwrap().contains("Stale saved review"),
            "{scenario}: {:?}",
            s.notice()
        );
        assert_eq!(s.changes().unwrap().proposed_count(), 0);
        assert_eq!(store.list().unwrap().0.len(), 1);
    }
}

#[test]
fn dirty_and_changed_eligibility_never_make_resumed_choices_actionable() {
    let repo = fixture();
    let git = adapter(&repo);
    let ops = Operations::new(&git);
    {
        let mut s = Shell::open(repo.path(), &ops, &TuiOptions::default());
        press(&mut s, '2');
        press(&mut s, 'r');
        s.flush_progress().unwrap();
    }
    repo.write_file("notes/a.md", b"# Unsaved\nDifferent working source\n");
    let mut s = Shell::open(repo.path(), &ops, &TuiOptions::default());
    resume(&mut s);
    press(&mut s, '2');
    s.handle_event(&key(KeyCode::Enter));
    if let Some(summary) = s.finish() {
        assert!(!summary.preview().ok());
        s.handle_event(&key(KeyCode::Enter));
        assert!(!s.finish_result().unwrap().ok());
    }
    assert_eq!(
        repo.read_file("notes/a.md"),
        b"# Unsaved\nDifferent working source\n"
    );
}

#[test]
fn save_failure_preserves_memory_and_requires_explicit_quit_without_saving() {
    let repo = fixture();
    let git = adapter(&repo);
    let ops = Operations::new(&git);
    let store = session::Store::from_git(&git).unwrap();
    let mut s = Shell::open(repo.path(), &ops, &TuiOptions::default());
    press(&mut s, '2');
    press(&mut s, 'r');
    s.flush_progress().unwrap();
    let record = store.list().unwrap().0[0].record.clone().unwrap();
    let path = store.directory.join(format!("{}.json", record.id));
    let valid = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    press(&mut s, 'K');
    press(&mut s, 'q');
    assert!(!s.should_quit());
    assert!(s.progress_label().unwrap().contains("Progress not saved"));
    assert_eq!(s.changes().unwrap().proposed_count(), 1);
    assert!(s.notice().unwrap().contains("quit without saving"));
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, &valid).unwrap();
    press(&mut s, 'j');
    s.flush_progress().unwrap();
    assert_eq!(
        store.read(&record.id).unwrap().decisions[0].decision,
        margin::tui::Decision::Keep
    );
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    press(&mut s, 'r');
    press(&mut s, 'q');
    assert!(!s.should_quit());
    press(&mut s, 'q');
    assert!(s.should_quit());
}

#[test]
fn restart_recognizes_a_receipt_after_application_before_local_cleanup() {
    let repo = fixture();
    let git = adapter(&repo);
    let ops = Operations::new(&git);
    let store = session::Store::from_git(&git).unwrap();
    let record;
    {
        let mut s = Shell::open(repo.path(), &ops, &TuiOptions::default());
        press(&mut s, '2');
        press(&mut s, 'r');
        s.handle_event(&key(KeyCode::Enter));
        s.flush_progress().unwrap();
        let mut r = store.list().unwrap().0[0].record.clone().unwrap();
        let summary = s.finish().unwrap();
        r.attempt = Some(session::Attempt {
            request_id: summary.request_id().unwrap().into(),
            plan: summary.plan_document().unwrap().into(),
        });
        record = r;
    }
    {
        let owner = store.acquire(&record.id).unwrap();
        owner.save(&record).unwrap();
    }
    let attempt = record.attempt.as_ref().unwrap();
    let result = ops.apply_review(&margin::app::ReviewRequest {
        scope: None,
        plan: attempt.plan.clone(),
        dry_run: false,
    });
    assert!(result.ok());
    let before = oracle::snapshot(&repo);
    let mut s = Shell::open(repo.path(), &ops, &TuiOptions::default());
    assert!(s.saved_reviews()[0].completed.is_some());
    assert_eq!(
        s.saved_reviews()[0]
            .record
            .as_ref()
            .unwrap()
            .decisions
            .len(),
        1
    );
    resume(&mut s);
    assert!(s.notice().unwrap().contains("Completed review"));
    before.assert_unchanged(&oracle::snapshot(&repo), "startup only inspects receipts");
}

#[test]
fn palette_literal_input_disabled_reasons_generations_and_scoped_headings() {
    let repo = fixture();
    let before = oracle::snapshot(&repo);
    let git = adapter(&repo);
    let ops = Operations::new(&git);
    let mut s = Shell::open(
        repo.path(),
        &ops,
        &TuiOptions {
            reduced_updates: true,
            ..Default::default()
        },
    );
    draw(&mut s, 120, 30);
    press(&mut s, ':');
    settle(&mut s);
    assert_eq!(s.palette().hits.len(), Action::ALL.len());
    s.handle_event(&Event::Paste("jkqrs:?".into()));
    s.handle_event(&key(KeyCode::Tab));
    s.handle_event(&key(KeyCode::BackTab));
    assert_eq!(s.focus(), Focus::Actions);
    assert_eq!(s.palette().query, "jkqrs:?");
    assert!(!s.should_quit());
    s.handle_event(&key(KeyCode::Enter));
    settle(&mut s);
    assert!(s.overlay().is_some());
    s.handle_event(&key(KeyCode::Esc));
    press(&mut s, ':');
    s.handle_event(&Event::Paste("Target".into()));
    s.handle_event(&key(KeyCode::Enter));
    settle(&mut s);
    assert_eq!(s.palette().hits.len(), 2);
    assert_eq!(s.palette().hits[0].entry.path, "notes/a.md");
    repo.write_file("notes/a.md", b"# Changed\n\n## Moved target\n");
    s.handle_event(&key(KeyCode::Enter));
    assert!(s.overlay().is_some());
    assert!(s.notice().unwrap().contains("Source changed"));
    settle(&mut s);
    assert_eq!(s.view(), View::Read);
    s.handle_event(&key(KeyCode::Esc));
    press(&mut s, ':');
    s.handle_event(&Event::Paste("Secret outside scope".into()));
    s.handle_event(&key(KeyCode::Enter));
    settle(&mut s);
    assert!(s.palette().hits.is_empty());
    // Restore fixture bytes so the stock-Git oracle covers all navigation IO.
    repo.write_file(
        "notes/a.md",
        b"# Duplicate\n\nFirst edited passage.\n\n## Target\n\nA long unchanged paragraph.\n",
    );
    s.handle_event(&key(KeyCode::Esc));
    before.assert_unchanged(&oracle::snapshot(&repo), "palette is read-only");
}

#[test]
fn reference_freeze_refresh_layout_focus_and_changed_source_resume() {
    let repo = fixture();
    let git = adapter(&repo);
    let ops = Operations::new(&git);
    let mut s = Shell::open(repo.path(), &ops, &TuiOptions::default());
    draw(&mut s, 150, 28);
    action(&mut s, Action::PinDocument);
    assert!(!s.navigator_visible());
    let frozen = s.reference().unwrap().text.clone();
    let primary = s.current_anchor().cloned();
    action(&mut s, Action::FocusReference);
    for c in ['K', 'r', 'u', 'N', 'e', 'n', 'p'] {
        press(&mut s, c);
    }
    s.handle_event(&key(KeyCode::Enter));
    assert!(s.finish().is_none());
    assert_eq!(s.changes().unwrap().proposed_count(), 0);
    assert_eq!(s.current_anchor(), primary.as_ref());
    press(&mut s, 's');
    draw(&mut s, 150, 28);
    press(&mut s, '/');
    s.handle_event(&Event::Paste("Target".into()));
    s.handle_event(&key(KeyCode::Enter));
    assert!(s.reference().unwrap().descriptor.source);
    assert!(s.reference().unwrap().descriptor.line >= 5);
    let narrow = draw(&mut s, 100, 25);
    assert!(narrow.contains("Reference · read-only"));
    assert!(!narrow.contains("Documents ("));
    s.handle_event(&key(KeyCode::Esc));
    assert_eq!(s.focus(), Focus::Reading);
    assert!(s.reference().is_some());
    draw(&mut s, 150, 25);
    action(&mut s, Action::ToggleReference);
    repo.write_file("notes/a.md", b"# Working replacement\n");
    assert_eq!(s.reference().unwrap().text, frozen);
    action(&mut s, Action::RefreshReference);
    assert!(
        s.reference()
            .unwrap()
            .text
            .as_ref()
            .unwrap()
            .contains("Working replacement")
    );
    // Restore candidate, re-pin, create a decision, then change the working reference after quit.
    repo.write_file(
        "notes/a.md",
        b"# Duplicate\n\nFirst edited passage.\n\n## Target\n\nA long unchanged paragraph.\n",
    );
    action(&mut s, Action::RefreshReference);
    press(&mut s, 'R');
    press(&mut s, '2');
    press(&mut s, 'r');
    s.flush_progress().unwrap();
    drop(s);
    repo.write_file("notes/a.md", b"# Changed after saving\n");
    let mut s = Shell::open(repo.path(), &ops, &TuiOptions::default());
    resume(&mut s);
    assert_eq!(s.changes().unwrap().proposed_count(), 1);
    assert!(
        s.reference()
            .unwrap()
            .unavailable
            .as_ref()
            .unwrap()
            .contains("working source changed")
    );
    assert!(s.reference().unwrap().text.is_none());
}

#[test]
fn historical_reference_is_immutable_and_failed_replacement_retains_snapshot() {
    let repo = fixture();
    let git = adapter(&repo);
    let ops = Operations::new(&git);
    let mut s = Shell::open(repo.path(), &ops, &TuiOptions::default());
    draw(&mut s, 150, 24);
    press(&mut s, '3');
    action(&mut s, Action::PinVersion);
    let oid = s.reference().unwrap().descriptor.revision.clone().unwrap();
    let text = s.reference().unwrap().text.clone();
    repo.write_file("notes/a.md", b"# Another revision\n");
    repo.commit_all("Move head");
    assert_eq!(
        s.reference().unwrap().descriptor.revision.as_deref(),
        Some(oid.as_str())
    );
    assert_eq!(s.reference().unwrap().text, text);
    let missing = margin::tui::reference::Reference::historical(&ops, "notes/missing.md", &oid, 1);
    assert!(missing.err().unwrap().contains("missing"));
    action(&mut s, Action::RefreshReference);
    assert_eq!(
        s.reference().unwrap().descriptor.revision.as_deref(),
        Some(oid.as_str())
    );
    assert_eq!(s.reference().unwrap().text, text);
}

#[test]
fn large_scope_palette_and_save_churn_smoke() {
    let root = tempfile::tempdir().unwrap();
    let mut paths = vec![];
    for i in 0..2_000 {
        let path = format!("document-{i:04}.md");
        std::fs::write(
            root.path().join(&path),
            format!(
                "# Document {i}\n\n{}",
                (0..12)
                    .map(|n| format!("## Heading {n} shared target\n\nProse.\n"))
                    .collect::<String>()
            ),
        )
        .unwrap();
        paths.push(path);
    }
    let start = Instant::now();
    let metadata = margin::tui::palette::metadata(root.path(), &paths, || false);
    let startup = start.elapsed();
    assert_eq!(metadata.entries.len(), 20_000);
    assert!(metadata.limited);
    let mut times = vec![];
    for query in [
        "Heading 7",
        "shared target",
        "document-0555",
        "h7st",
        "missing query",
    ] {
        let start = Instant::now();
        let _ = margin::tui::palette::matching(query, metadata.entries.clone());
        times.push(start.elapsed().as_micros());
    }
    let store = session::Store {
        directory: root.path().join("margin/tui/reviews"),
    };
    let owner = store.create().unwrap();
    let id = owner.id.clone();
    let mut record = session::Record {
        schema_version: 1,
        id: id.clone(),
        updated_ms: 0,
        generation: 0,
        identity: session::Identity {
            repository: "fixture".into(),
            branch: "main".into(),
            scope: "scope".into(),
            adoption: "start".into(),
            baseline: "B".into(),
            candidate: "C".into(),
            review: "review".into(),
        },
        decisions: (0..10_000)
            .map(|i| session::Choice {
                id: format!("change-{i:06}"),
                decision: margin::tui::Decision::Keep,
                path: format!("document-{:04}.md", i % 2000),
                heading: "A bounded decision label".into(),
            })
            .collect(),
        visited: vec![],
        navigation: Default::default(),
        attempt: None,
        reference: None,
    };
    let writer = session::Writer::spawn(owner).unwrap();
    let start = Instant::now();
    for generation in 1..=100 {
        record.generation = generation;
        writer.submit(record.clone());
    }
    writer.flush(record).unwrap();
    let saving = start.elapsed();
    assert_eq!(store.read(&id).unwrap().generation, 100);
    let leftovers = std::fs::read_dir(&store.directory)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|e| e == "tmp")
        })
        .count();
    assert_eq!(leftovers, 0);
    eprintln!(
        "WORKSPACE_PERF metadata_files={} entries={} startup_ms={} query_us={times:?} save_100_snapshots_ms={}",
        metadata.scanned,
        metadata.entries.len(),
        startup.as_millis(),
        saving.as_millis()
    );
}

#[test]
fn an_old_save_completion_does_not_label_newer_choices_saved() {
    let repo = fixture();
    let git = adapter(&repo);
    let ops = Operations::new(&git);
    let mut s = Shell::open(repo.path(), &ops, &TuiOptions::default());
    press(&mut s, '2');
    press(&mut s, 'r');
    s.flush_progress().unwrap();
    press(&mut s, 'K');
    s.poll_progress();
    assert!(s.progress_label().unwrap().contains("saving locally"));
    s.flush_progress().unwrap();
    assert!(s.progress_label().unwrap().contains("saved locally"));
}

#[test]
fn local_save_and_cleanup_failure_do_not_turn_a_verified_review_into_failure() {
    let repo = fixture();
    let git = adapter(&repo);
    let ops = Operations::new(&git);
    let store = session::Store::from_git(&git).unwrap();
    let mut s = Shell::open(repo.path(), &ops, &TuiOptions::default());
    press(&mut s, '2');
    press(&mut s, 'r');
    s.flush_progress().unwrap();
    let record = store.list().unwrap().0[0].record.clone().unwrap();
    let path = store.directory.join(format!("{}.json", record.id));
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    s.handle_event(&key(KeyCode::Enter));
    assert!(s.finish().is_some());
    s.handle_event(&key(KeyCode::Enter));
    assert!(s.finish_result().unwrap().ok());
    assert!(
        s.notice()
            .unwrap()
            .contains("review succeeded; draft cleanup failed")
    );
}

#[test]
fn staged_eligibility_still_blocks_a_resumed_plan_and_reference_replacement_is_atomic() {
    let repo = fixture();
    let git = adapter(&repo);
    let ops = Operations::new(&git);
    {
        let mut s = Shell::open(repo.path(), &ops, &TuiOptions::default());
        press(&mut s, '2');
        press(&mut s, 'r');
        s.flush_progress().unwrap();
    }
    repo.write_file("notes/a.md", b"# Staged source\n");
    repo.git_ok(["add", "--", "notes/a.md"]);
    let before = oracle::snapshot(&repo);
    let mut s = Shell::open(repo.path(), &ops, &TuiOptions::default());
    resume(&mut s);
    press(&mut s, '2');
    s.handle_event(&key(KeyCode::Enter));
    if let Some(summary) = s.finish() {
        assert!(!summary.preview().ok());
    }
    before.assert_unchanged(&oracle::snapshot(&repo), "resumed blocked intent");
    if s.finish().is_some() {
        s.handle_event(&key(KeyCode::Esc));
    }
    press(&mut s, '1');
    draw(&mut s, 150, 28);
    action(&mut s, Action::PinDocument);
    let text = s.reference().unwrap().text.clone();
    std::fs::remove_file(repo.path().join("notes/a.md")).unwrap();
    action(&mut s, Action::RefreshReference);
    assert_eq!(s.reference().unwrap().text, text);
    assert!(
        s.notice().unwrap().contains("No such file") || s.notice().unwrap().contains("unavailable")
    );
}

#[test]
fn choosing_a_reference_preserves_primary_context_and_polling_marks_its_frozen_source() {
    let repo = fixture();
    let git = adapter(&repo);
    let ops = Operations::new(&git);
    let mut s = Shell::open(repo.path(), &ops, &TuiOptions::default());
    draw(&mut s, 150, 28);
    press(&mut s, '2');
    press(&mut s, 'r');
    let choices = s.changes().unwrap().restore_selection();
    let primary = s.document().unwrap().path.clone();
    let visited = s.changes().unwrap().visited_ids();
    action(&mut s, Action::ChooseReference);
    settle(&mut s);
    s.handle_event(&Event::Paste("Target notes/b.md".into()));
    s.handle_event(&key(KeyCode::Enter));
    settle(&mut s);
    s.handle_event(&key(KeyCode::Enter));
    assert_eq!(s.document().unwrap().path, primary);
    assert_eq!(s.view(), View::Changes);
    assert_eq!(s.reference().unwrap().descriptor.path, "notes/b.md");
    assert_eq!(s.reference().unwrap().descriptor.line, 5);
    assert_eq!(s.changes().unwrap().visited_ids(), visited);
    s.flush_progress().unwrap();
    let store = session::Store::from_git(&git).unwrap();
    let record = store.list().unwrap().0.remove(0).record.unwrap();
    let json = serde_json::to_string(&record).unwrap();
    assert!(
        !json.contains("Other edited passage"),
        "reference and document bodies are never stored"
    );
    let frozen = s.reference().unwrap().text.clone();
    repo.write_file("notes/b.md", b"# Changed reference source\n");
    let before = oracle::snapshot(&repo);
    let mut worker = margin::tui::RefreshWorker::spawn(git.clone()).unwrap();
    worker.request(margin::tui::WatchJob {
        generation: s.generation(),
        watch: s.watch(),
    });
    let start = Instant::now();
    loop {
        if let Some(reply) = worker.receive() {
            s.observe_inspection(reply);
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(s.reference().unwrap().changed);
    assert_eq!(s.reference().unwrap().text, frozen);
    assert_eq!(s.changes().unwrap().restore_selection(), choices);
    before.assert_unchanged(&oracle::snapshot(&repo), "reference polling only observes");
    worker.shutdown();
    let oid = git.head_commit().unwrap().unwrap();
    assert!(
        margin::tui::reference::Reference::historical(&ops, "notes/missing.md", &oid, 1)
            .err()
            .unwrap()
            .contains("not an empty")
    );
    repo.write_file("notes/empty.md", b"");
    cli(&repo, &["checkpoint"]);
    let oid = git.head_commit().unwrap().unwrap();
    let empty =
        margin::tui::reference::Reference::historical(&ops, "notes/empty.md", &oid, 1).unwrap();
    assert_eq!(empty.text.as_deref(), Some(""));
}

#[test]
fn saved_identity_keeps_the_start_marker_separate_from_a_review_baseline() {
    let repo = fixture();
    let git = adapter(&repo);
    let ops = Operations::new(&git);
    let start = ops.feedback_commits().unwrap().last().unwrap().clone();
    {
        let mut s = Shell::open(repo.path(), &ops, &TuiOptions::default());
        press(&mut s, '2');
        press(&mut s, 'K');
        s.handle_event(&key(KeyCode::Enter));
        s.handle_event(&key(KeyCode::Enter));
        assert!(s.finish_result().unwrap().ok());
    }
    repo.write_file("notes/a.md", b"# Next pass\nA new candidate.\n");
    cli(&repo, &["checkpoint"]);
    {
        let mut s = Shell::open(repo.path(), &ops, &TuiOptions::default());
        press(&mut s, '2');
        press(&mut s, 'r');
        s.flush_progress().unwrap();
    }
    let record = session::Store::from_git(&git)
        .unwrap()
        .list()
        .unwrap()
        .0
        .remove(0)
        .record
        .unwrap();
    assert_eq!(record.identity.adoption, start);
    assert_ne!(record.identity.baseline, start);
    let mut s = Shell::open(repo.path(), &ops, &TuiOptions::default());
    resume(&mut s);
    assert_eq!(s.changes().unwrap().proposed_count(), 1);
}

#[test]
fn fresh_review_from_the_saved_offer_preserves_both_prior_drafts_without_migrating_choices() {
    let repo = fixture();
    let git = adapter(&repo);
    let ops = Operations::new(&git);
    let store = session::Store::from_git(&git).unwrap();
    {
        let mut s = Shell::open(repo.path(), &ops, &TuiOptions::default());
        press(&mut s, '2');
        press(&mut s, 'r');
        s.flush_progress().unwrap();
    }
    let mut s = Shell::open(repo.path(), &ops, &TuiOptions::default());
    press(&mut s, '2');
    press(&mut s, 'K');
    action(&mut s, Action::ResumeReview);
    press(&mut s, 'n');
    assert!(s.overlay().is_none());
    assert_eq!(s.changes().unwrap().proposed_count(), 0);
    let entries = store.list().unwrap().0;
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|e| e.record.as_ref().unwrap().decisions.len() == 1)
    );
}

#[test]
fn stored_reference_paths_stay_guarded_and_escaped_even_below_minimum_size() {
    let repo = fixture();
    let git = adapter(&repo);
    let ops = Operations::new(&git);
    let store = session::Store::from_git(&git).unwrap();
    {
        let mut s = Shell::open(repo.path(), &ops, &TuiOptions::default());
        press(&mut s, '2');
        press(&mut s, 'r');
        s.flush_progress().unwrap();
    }
    let mut record = store.list().unwrap().0.remove(0).record.unwrap();
    record.reference = Some(margin::tui::reference::Descriptor {
        path: "../outside.md\x1b[2J".into(),
        revision: None,
        fingerprint: "untrusted".into(),
        captured_ms: 0,
        unsaved: false,
        line: 1,
        drift: 0,
        source: true,
        visible: true,
    });
    {
        let owner = store.acquire(&record.id).unwrap();
        owner.save(&record).unwrap();
    }
    let mut s = Shell::open(repo.path(), &ops, &TuiOptions::default());
    resume(&mut s);
    assert!(s.reference().unwrap().text.is_none());
    assert!(
        s.reference()
            .unwrap()
            .unavailable
            .as_ref()
            .unwrap()
            .contains("outside the current scope")
    );
    draw(&mut s, 150, 24);
    action(&mut s, Action::FocusReference);
    draw(&mut s, 50, 12);
    press(&mut s, 'd');
    let screen = draw(&mut s, 50, 12);
    assert!(!screen.contains('\x1b'));
    assert!(!screen.contains("Secret outside scope"));
    assert_eq!(s.changes().unwrap().proposed_count(), 1);
}

#[test]
fn checkpointed_choices_resume_under_the_new_comparison_identity() {
    let repo = fixture();
    let git = adapter(&repo);
    let ops = Operations::new(&git);
    let store = session::Store::from_git(&git).unwrap();
    let old_head = git.head_commit().unwrap().unwrap();
    let new_head;
    {
        let mut shell = Shell::open(repo.path(), &ops, &TuiOptions::default());
        press(&mut shell, '2');
        press(&mut shell, 'r');
        shell.flush_progress().unwrap();
        repo.write_file(
            "notes/b.md",
            b"# Duplicate\n\nA corrected second passage.\n",
        );
        press(&mut shell, '1');
        press(&mut shell, 'C');
        new_head = git.head_commit().unwrap().unwrap();
        assert_ne!(new_head, old_head);
        assert_eq!(shell.changes().unwrap().proposed_count(), 1);
        shell.flush_progress().unwrap();
    }
    let records = store.list().unwrap().0;
    let current = records
        .iter()
        .find(|e| e.record.as_ref().unwrap().identity.candidate == new_head)
        .unwrap();
    let previous = records
        .iter()
        .find(|e| e.record.as_ref().unwrap().identity.candidate == old_head)
        .unwrap();
    assert_eq!(current.record.as_ref().unwrap().decisions.len(), 1);
    assert_eq!(previous.record.as_ref().unwrap().decisions.len(), 1);
    let mut shell = Shell::open(repo.path(), &ops, &TuiOptions::default());
    action(&mut shell, Action::ResumeReview);
    let index = shell
        .saved_reviews()
        .iter()
        .position(|e| e.id == current.id)
        .unwrap();
    for _ in 0..index {
        shell.handle_event(&key(KeyCode::Down));
    }
    shell.handle_event(&key(KeyCode::Enter));
    assert!(shell.overlay().is_none(), "{:?}", shell.notice());
    assert_eq!(shell.changes().unwrap().proposed_count(), 1);
    assert_eq!(shell.changes().unwrap().candidate_oid(), new_head);
}

#[test]
fn reference_clicks_and_palette_preserve_read_only_restrictions() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let repo = fixture();
    let git = adapter(&repo);
    let ops = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &ops, &TuiOptions::default());
    draw(&mut shell, 160, 35);
    action(&mut shell, Action::PinDocument);
    draw(&mut shell, 160, 35);
    let rect = shell
        .frame_layout(ratatui::layout::Rect::new(0, 0, 160, 35))
        .reference
        .unwrap();
    shell.handle_event(&Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rect.x + 1,
        row: rect.y + 2,
        modifiers: KeyModifiers::NONE,
    }));
    assert_eq!(shell.focus(), Focus::Reference);
    let before = oracle::snapshot(&repo);
    for blocked in [
        Action::ExternalEditor,
        Action::AskAgent,
        Action::CheckpointDraft,
        Action::UndoAgentEdit,
        Action::PreviewRestore,
    ] {
        action(&mut shell, blocked);
        assert!(
            shell
                .notice()
                .is_some_and(|n| n.contains("Return to review")),
            "{blocked:?}: {:?}",
            shell.notice()
        );
        assert!(shell.source_editor().is_none());
        assert!(shell.take_editor_request().is_none());
        shell.handle_event(&key(KeyCode::Esc));
    }
    before.assert_unchanged(&oracle::snapshot(&repo), "reference actions stay read-only");
}
