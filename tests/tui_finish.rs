//! Buffer tests for the finish flow (SPEC 5.2, 11.3, 16.9, task T043).
//!
//! Every case drives the real shell against a real Git repository through
//! the shared operations, then renders into a ratatui test buffer. The
//! cases prove what the acceptance states: the finish summary covers the
//! entire writing scope, unvisited documents included, and only a second
//! `Enter` on the labeled keep-remaining action applies it; the plan the
//! surface composes is the same document the command line reads, so
//! equivalent plans agree on result tree, marker meaning, errors, and
//! effects; `Esc` and quit never commit a selection. The independent
//! oracles — stock Git for trees and bytes, the shared marker validator
//! for meaning, `oracle::snapshot` for effects — stay outside the
//! implementation under test.

mod support;

use std::path::Path;
use std::process::{Command, Output, Stdio};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use margin::app::{Operations, ReviewDecision, validate_marker};
use margin::git::Git;
use margin::tui::{FinishSummary, Shell, TuiOptions, render};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
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

/// Runs one JSON query and returns its envelope.
fn envelope(env: &support::env::IsolatedEnv, cwd: &Path, args: &[&str]) -> Value {
    let output = margin(env, cwd, args);
    let envelope: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("one JSON envelope: {error}"));
    envelope
}

/// Runs one JSON query and returns its data object.
fn json_data(env: &support::env::IsolatedEnv, cwd: &Path, args: &[&str]) -> Value {
    let envelope = envelope(env, cwd, args);
    assert_eq!(
        envelope["ok"], true,
        "the query must succeed: {}",
        envelope["error"]["message"]
    );
    envelope["data"].clone()
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
        .draw(|frame| render::draw(shell, frame))
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

/// One adopted repository whose pending review holds three groups in two
/// documents: `notes/a.md` edits two far-apart passages, `notes/b.md` edits
/// one. One out-of-scope file also changed between B and C, so every finish
/// must leave it exactly as the candidate holds it.
fn three_group_repo() -> TestRepo {
    let repo = TestRepo::init();
    repo.write_file(
        "notes/a.md",
        b"# One\n\nFirst passage.\n\nFiller one.\n\nFiller two.\n\nFiller three.\n\n\
           Filler four.\n\nSecond passage.\n",
    );
    repo.write_file("notes/b.md", b"# Two\n\nOther passage.\n");
    repo.write_file("outside.txt", b"the baseline note\n");
    repo.commit_all("import");
    let output = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes/", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));
    repo.write_file("outside.txt", b"the candidate note\n");
    repo.commit_all("out-of-scope edit");
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
    assert_eq!(
        output.status.code(),
        Some(0),
        "checkpoint must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    repo
}

/// The decided outcome of one applied finish, with its error when it
/// refused.
fn decision_or_panic(shell: &Shell<'_>) -> ReviewDecision {
    let outcome = shell.finish_result().expect("one finish ran");
    let error = outcome
        .error
        .as_ref()
        .map(|error| format!("{}: {}", error.code, error.message))
        .unwrap_or_default();
    assert!(outcome.ok(), "the finish must succeed: {error}");
    outcome
        .data
        .clone()
        .expect("a successful finish carries data")
}

#[test]
fn the_summary_states_the_entire_scope_and_requires_explicit_intent() {
    let repo = three_group_repo();
    let before = oracle::snapshot(&repo);
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());

    // Restore one passage of `notes/a.md`, keep the other explicitly, and
    // never open `notes/b.md`: the summary must state that document too.
    shell.handle_event(&key(KeyCode::Char('2')));
    shell.handle_event(&key(KeyCode::Char('r')));
    shell.handle_event(&key(KeyCode::Char('n')));
    shell.handle_event(&key(KeyCode::Char('K')));

    // `Enter` opens the summary; it applies nothing by itself (SPEC 5.2).
    shell.handle_event(&key(KeyCode::Enter));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(screen.contains("Finish review"));
    assert!(
        screen.contains("2 documents affected · 3 passage groups"),
        "the entire scope, document by document",
    );
    assert!(screen.contains("1 passage restored · 1 kept · 1 undecided"));
    assert!(
        screen.contains("1 passage group and 1 document never opened"),
        "the unvisited scope stays stated",
    );
    assert!(screen.contains("Finishing keeps every passage you did not restore, opened or not."),);
    assert!(
        !screen.contains(" · review "),
        "technical identifiers stay in Details"
    );
    assert!(
        screen.contains("Intended result, proved by one read-only preview:"),
        "the result half of the summary",
    );
    assert!(screen.contains("2 of 3 passage groups kept · 1 restored"));
    assert!(screen.contains("Files rewritten: notes/a.md"));
    assert!(!screen.contains("Result tree "));
    shell.handle_event(&key(KeyCode::Char('d')));
    let details = joined(&draw(&mut shell, 110, 40));
    assert!(details.contains("Result tree "));
    assert!(details.contains(" · review "));
    assert!(
        screen.contains("Enter  Keep remaining changes and finish"),
        "the labeled action carries the SPEC 5.2 wording",
    );
    assert!(screen.contains("Esc    Cancel; nothing is applied"));
    assert!(
        screen.contains("Enter keep remaining and finish · Esc cancel"),
        "the footer names the two keys at every size",
    );

    // The summary is one read-only preview: no repository byte moved and
    // no commit joined (SPEC 6.4).
    before.assert_unchanged(&oracle::snapshot(&repo), "the finish summary");
    assert_eq!(
        shell.finish().expect("the summary stands").counts().groups,
        3,
    );

    // Different decisions take a different request identity, so a failed
    // finish followed by a changed plan never conflicts under one request
    // (SPEC 7.3, 8.7), while a retry of the same decisions reuses it.
    let first = shell
        .finish()
        .and_then(FinishSummary::request_id)
        .map(str::to_string);
    shell.handle_event(&key(KeyCode::Esc));
    shell.handle_event(&key(KeyCode::Char('n')));
    shell.handle_event(&key(KeyCode::Char('r')));
    shell.handle_event(&key(KeyCode::Enter));
    let second = shell
        .finish()
        .and_then(FinishSummary::request_id)
        .map(str::to_string);
    assert_ne!(
        first.as_deref(),
        second.as_deref(),
        "one comparison, two plans, two identities",
    );
    before.assert_unchanged(&oracle::snapshot(&repo), "the reopened summary");
}

#[test]
fn a_second_enter_applies_the_plan_through_the_shared_operation() {
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    let candidate = oracle::head_oid(&repo).expect("the candidate stands at HEAD");
    let pinned = json_data(repo.env(), repo.path(), &["diff", "--json"]);
    let baseline = pinned["baseline_oid"]
        .as_str()
        .expect("the diff names B")
        .to_string();

    // Restore the first passage of `notes/a.md`; keep the other two.
    shell.handle_event(&key(KeyCode::Char('2')));
    shell.handle_event(&key(KeyCode::Char('r')));
    shell.handle_event(&key(KeyCode::Enter));
    shell.handle_event(&key(KeyCode::Enter));
    let data = decision_or_panic(&shell);

    // The decision the shared operation reports (SPEC 9.4).
    assert_eq!(data.restored.len(), 1, "one passage restored");
    assert_eq!(data.kept, Some(2), "the other two stay");
    assert_eq!(data.paths, vec!["notes/a.md".to_string()]);
    assert!(!data.dry_run);
    assert_eq!(data.existing_receipt, None);
    let outcome = shell.finish_result().expect("the outcome stands");
    let commit = data.commit.clone().expect("the application commits");
    assert_eq!(outcome.effects.commit.as_str(), "created");
    assert_eq!(outcome.effects.commit_oid.as_deref(), Some(commit.as_str()));
    assert_eq!(
        outcome.effects.working_tree.as_str(),
        "changed",
        "the restored passage rewrote its working file",
    );
    assert_eq!(
        outcome.effects.index.as_str(),
        "changed",
        "the guarded index path applied the result before the commit",
    );
    assert_eq!(outcome.effects.repository.as_str(), "unchanged");
    assert_eq!(outcome.effects.configuration.as_str(), "unchanged");
    assert_eq!(outcome.effects.integration.as_str(), "unchanged");

    // Stock Git proves the commit: one parent, the candidate, and the
    // intended tree (SPEC 16.3).
    assert_eq!(oracle::head_oid(&repo).as_deref(), Some(commit.as_str()));
    let parents = text(&repo.git_ok(["log", "-n", "1", "--format=%P", &commit]));
    assert_eq!(parents.trim(), candidate, "one parent: the candidate C");
    let tree = oracle::commit_tree(&repo, &commit);
    assert_eq!(data.expected_tree, tree, "the intended tree is committed");
    oracle::assert_out_of_scope_tree_unchanged(
        &repo,
        &candidate,
        &commit,
        &["notes/a.md"],
        "the applied finish",
    );

    // The marker the commit carries means one completed review (SPEC 8.3).
    let marker = validate_marker(&git, &commit)
        .unwrap_or_else(|error| panic!("the finish wrote a valid marker: {error}"));
    assert_eq!(marker.kind.as_str(), "review");
    assert_eq!(marker.candidate, candidate);
    assert_eq!(marker.baseline.as_deref(), Some(baseline.as_str()));
    assert_eq!(marker.result_tree, tree);
    assert!(
        marker
            .request
            .as_deref()
            .is_some_and(|request| request.starts_with("tui:")),
        "the request names the surface that composed it: {:?}",
        marker.request,
    );

    // The working files hold the decided result: the restored passage
    // returned to B, the kept passages and the out-of-scope note hold C.
    let decided = String::from_utf8_lossy(&repo.read_file("notes/a.md")).into_owned();
    assert!(
        decided.starts_with("# One\n\nFirst passage.\n\n"),
        "the restored passage returned to the baseline bytes",
    );
    assert!(decided.contains("Second passage, edited.\n"));
    assert!(
        String::from_utf8_lossy(&repo.read_file("notes/b.md")).contains("Other passage, edited.\n")
    );
    assert_eq!(repo.read_file("outside.txt"), b"the candidate note\n");
    assert_eq!(text(&repo.git_ok(["status", "--porcelain"])), "");

    // The surface states the finished review, and the applied plan is gone.
    let status = json_data(repo.env(), repo.path(), &["status", "--json"]);
    assert_eq!(status["review"], "reviewed");
    shell.handle_event(&key(KeyCode::Enter));
    assert!(
        shell.finish().is_none(),
        "no comparison waits for a second finish",
    );
    let page = draw(&mut shell, 110, 30);
    assert!(joined(&page).contains("nothing waits for review"));
}

/// The keep-all finish of SPEC 5.2: when everything is good, the review
/// commit is the empty one that records approval, no file is rewritten,
/// and no save confirmation comes first.
#[test]
fn a_finish_without_restorations_records_the_empty_review_commit() {
    let repo = three_group_repo();
    let before = oracle::snapshot(&repo);
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    let candidate = oracle::head_oid(&repo).expect("the candidate stands at HEAD");

    // No decision at all: the summary states the scope it will keep.
    shell.handle_event(&key(KeyCode::Char('2')));
    shell.handle_event(&key(KeyCode::Enter));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(screen.contains("3 passage groups"));
    assert!(screen.contains("0 passages restored · 0 kept · 3 undecided"));
    assert!(screen.contains("3 of 3 passage groups kept · 0 restored"));
    assert!(screen.contains("Files rewritten: none"));

    shell.handle_event(&key(KeyCode::Enter));
    let data = decision_or_panic(&shell);
    let outcome = shell.finish_result().expect("the outcome stands");
    assert_eq!(data.restored, Vec::<String>::new());
    assert_eq!(data.kept, Some(3), "every passage stays");
    assert_eq!(data.paths, Vec::<String>::new(), "no file is rewritten");
    let commit = data.commit.clone().expect("the application commits");
    assert_eq!(outcome.effects.commit.as_str(), "created");
    assert_eq!(outcome.effects.working_tree.as_str(), "unchanged");
    assert_eq!(outcome.effects.index.as_str(), "unchanged");

    // The empty commit: one parent, the candidate's full tree (SPEC 8.1).
    assert_eq!(oracle::head_oid(&repo).as_deref(), Some(commit.as_str()));
    let parents = text(&repo.git_ok(["log", "-n", "1", "--format=%P", &commit]));
    assert_eq!(parents.trim(), candidate, "one parent: the candidate");
    assert_eq!(
        oracle::commit_tree(&repo, &commit),
        oracle::commit_tree(&repo, &candidate),
        "the review commit holds the candidate's full tree",
    );
    let marker = validate_marker(&git, &commit)
        .unwrap_or_else(|error| panic!("the finish wrote a valid marker: {error}"));
    assert_eq!(marker.kind.as_str(), "review");
    assert_eq!(marker.result_tree, oracle::commit_tree(&repo, &commit));

    // The drafts stay exactly as the candidate captured them — the empty
    // commit touches no file and no index entry — and the review is
    // recorded.
    let after = oracle::snapshot(&repo);
    assert_eq!(before.working, after.working, "no working byte moved");
    assert_eq!(before.index_tree, after.index_tree, "the index holds");
    let status = json_data(repo.env(), repo.path(), &["status", "--json"]);
    assert_eq!(status["review"], "reviewed");
}

#[test]
fn equivalent_cli_and_tui_plans_agree_on_tree_marker_errors_and_effects() {
    // The command line side: the plan document the surface itself
    // composed, taken from one open summary and applied by the installed
    // executable (SPEC 7.3, 16.9). One document, two entry points.
    let repo = three_group_repo();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    shell.handle_event(&key(KeyCode::Char('2')));
    shell.handle_event(&key(KeyCode::Char('n')));
    shell.handle_event(&key(KeyCode::Char('r')));
    shell.handle_event(&key(KeyCode::Enter));
    let document = shell
        .finish()
        .and_then(FinishSummary::plan_document)
        .map(|document| document.to_vec())
        .expect("the summary composes one plan document");
    shell.handle_event(&key(KeyCode::Esc));
    assert!(shell.finish().is_none(), "the cancel applies nothing");
    repo.write_file("cli-plan.json", &document);
    let cli = envelope(
        repo.env(),
        repo.path(),
        &["review", "--input", "cli-plan.json", "--json"],
    );
    assert_eq!(cli["ok"], true, "the CLI applies the same document");

    // The same decisions through the surface, on an identical repository.
    let peer = three_group_repo();
    let peer_git = adapter_for(&peer);
    let peer_operations = Operations::new(&peer_git);
    let mut peer_shell = Shell::open(peer.path(), &peer_operations, &TuiOptions::default());
    peer_shell.handle_event(&key(KeyCode::Char('2')));
    peer_shell.handle_event(&key(KeyCode::Char('n')));
    peer_shell.handle_event(&key(KeyCode::Char('r')));
    peer_shell.handle_event(&key(KeyCode::Enter));
    peer_shell.handle_event(&key(KeyCode::Enter));
    let data = decision_or_panic(&peer_shell);
    let outcome = peer_shell.finish_result().expect("the outcome stands");

    // Same decisions, same result, same effects (SPEC 16.9). Commit OIDs
    // may differ; the trees, the meaning, and the effects may not.
    assert_eq!(
        cli["data"]["restored"].as_array().map(Vec::len),
        Some(data.restored.len()),
        "both plans restored one passage",
    );
    assert_eq!(cli["data"]["kept"], json!(data.kept));
    assert_eq!(cli["data"]["paths"], json!(data.paths));
    assert_eq!(
        cli["data"]["expected_tree"], data.expected_tree,
        "the intended result trees agree",
    );
    assert_eq!(
        cli["effects"]["working_tree"],
        json!(outcome.effects.working_tree.as_str()),
    );
    assert_eq!(
        cli["effects"]["index"],
        json!(outcome.effects.index.as_str()),
    );
    assert_eq!(
        cli["effects"]["commit"],
        json!(outcome.effects.commit.as_str()),
    );
    assert_eq!(
        cli["effects"]["repository"],
        json!(outcome.effects.repository.as_str()),
    );
    assert_eq!(
        cli["effects"]["configuration"],
        json!(outcome.effects.configuration.as_str()),
    );
    assert_eq!(
        cli["effects"]["integration"],
        json!(outcome.effects.integration.as_str()),
    );
    let cli_commit = cli["data"]["commit"].as_str().expect("the CLI commit");
    assert_eq!(
        oracle::commit_tree(&repo, cli_commit),
        oracle::commit_tree(&peer, data.commit.as_deref().expect("the TUI commit")),
        "both plans produced one tree",
    );

    // The same marker meaning: kind, scope, pinned pair, and result.
    let cli_marker = validate_marker(&adapter_for(&repo), cli_commit)
        .unwrap_or_else(|error| panic!("the CLI commit is a marker: {error}"));
    let peer_marker = validate_marker(&peer_git, data.commit.as_deref().expect("the commit"))
        .unwrap_or_else(|error| panic!("the TUI commit is a marker: {error}"));
    assert_eq!(cli_marker.kind, peer_marker.kind);
    assert_eq!(cli_marker.scope_digest, peer_marker.scope_digest);
    assert_eq!(cli_marker.candidate, peer_marker.candidate);
    assert_eq!(cli_marker.baseline, peer_marker.baseline);
    assert_eq!(cli_marker.result_tree, peer_marker.result_tree);
    assert_eq!(cli_marker.candidate, data.candidate_oid);

    // The same typed error: uncaptured scoped bytes refuse both surfaces
    // with one code and one class (SPEC 7.5, 16.9).
    let dirty = three_group_repo();
    let dirty_git = adapter_for(&dirty);
    let dirty_operations = Operations::new(&dirty_git);
    let mut composer = Shell::open(dirty.path(), &dirty_operations, &TuiOptions::default());
    composer.handle_event(&key(KeyCode::Char('2')));
    composer.handle_event(&key(KeyCode::Enter));
    let document = composer
        .finish()
        .and_then(FinishSummary::plan_document)
        .map(|document| document.to_vec())
        .expect("a keep-all summary composes one document");
    composer.handle_event(&key(KeyCode::Esc));
    dirty.write_file("dirty-plan.json", &document);
    dirty.write_file("notes/a.md", b"# One\n\nUncaptured edit.\n");
    let refused = envelope(
        dirty.env(),
        dirty.path(),
        &["review", "--input", "dirty-plan.json", "--json"],
    );
    assert_eq!(refused["ok"], false, "the CLI refuses the dirty scope");

    let dirty_peer = three_group_repo();
    dirty_peer.write_file("notes/a.md", b"# One\n\nUncaptured edit.\n");
    let peer_git = adapter_for(&dirty_peer);
    let peer_operations = Operations::new(&peer_git);
    let mut dirty_shell = Shell::open(dirty_peer.path(), &peer_operations, &TuiOptions::default());
    dirty_shell.handle_event(&key(KeyCode::Char('2')));
    dirty_shell.handle_event(&key(KeyCode::Enter));
    let page = draw(&mut dirty_shell, 110, 30);
    assert!(
        joined(&page).contains("DIRTY_SCOPE"),
        "the summary states the typed blocker the preview proved",
    );
    dirty_shell.handle_event(&key(KeyCode::Enter));
    let outcome = dirty_shell.finish_result().expect("one finish ran");
    let error = outcome.error.as_ref().expect("the dirty scope refuses");
    assert_eq!(
        error.code,
        refused["error"]["code"]
            .as_str()
            .expect("the CLI error code"),
        "both surfaces report one error class",
    );
    assert_eq!(
        outcome.effects.working_tree.as_str(),
        refused["effects"]["working_tree"]
            .as_str()
            .expect("the CLI effects"),
    );
}

#[test]
fn cancel_and_quit_never_commit_a_selection() {
    let repo = three_group_repo();
    let before = oracle::snapshot(&repo);
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    let candidate = oracle::head_oid(&repo).expect("the candidate stands at HEAD");

    // One proposed restoration, then the summary: `Esc` leaves it without
    // applying anything (SPEC 11.3).
    shell.handle_event(&key(KeyCode::Char('2')));
    shell.handle_event(&key(KeyCode::Char('r')));
    shell.handle_event(&key(KeyCode::Enter));
    assert!(shell.finish().is_some(), "the summary stands");
    shell.handle_event(&key(KeyCode::Esc));
    assert!(shell.finish().is_none(), "Esc leaves the summary");
    let page = draw(&mut shell, 110, 30);
    assert!(
        joined(&page).contains("finish cancelled; no selection was applied"),
        "the cancel states what it did",
    );
    before.assert_unchanged(&oracle::snapshot(&repo), "the cancelled finish");

    // Quit flushes local intent and never applies even while the summary stands.
    shell.handle_event(&key(KeyCode::Enter));
    assert!(shell.finish().is_some());
    shell.handle_event(&key(KeyCode::Char('q')));
    assert!(shell.should_quit());
    assert!(
        shell
            .notice()
            .unwrap()
            .contains("saved locally and unapplied")
    );
    assert_eq!(
        oracle::head_oid(&repo).as_deref(),
        Some(candidate.as_str()),
        "no commit joined",
    );
    before.assert_unchanged(&oracle::snapshot(&repo), "the quit sequence");
}

#[test]
fn a_stale_comparison_refuses_to_open_a_finish() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n\nFirst passage.\n");
    repo.write_file("notes/b.md", b"# Two\n\nSecond passage.\n");
    repo.commit_all("import");
    let output = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes/", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));
    repo.write_file("notes/a.md", b"# One\n\nFirst passage, edited.\n");
    let output = margin(
        repo.env(),
        repo.path(),
        &["checkpoint", "--message", "Edit", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let mut shell = Shell::open(repo.path(), &operations, &TuiOptions::default());
    shell.handle_event(&key(KeyCode::Char('2')));

    // New edits arrive: the candidate moves past the pinned comparison
    // (SPEC 5.3), and the finish refuses over the readable text.
    repo.write_file("notes/b.md", b"# Two\n\nSecond passage, edited.\n");
    let output = margin(
        repo.env(),
        repo.path(),
        &["checkpoint", "--message", "Later edits", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));
    let after_arrival = oracle::snapshot(&repo);

    shell.handle_event(&key(KeyCode::Char('n')));
    shell.handle_event(&key(KeyCode::Enter));
    assert!(shell.finish().is_none(), "no summary opens on a stale pair");
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(
        screen.contains("New edits arrived. Refresh before applying."),
        "the refusal names the stale arrival",
    );
    after_arrival.assert_unchanged(&oracle::snapshot(&repo), "the refused finish");
}

/// Trims one Git output to text.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}
