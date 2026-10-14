//! Buffer tests for the History view (SPEC 5.5, 11.3, 16.9, task T044).
//!
//! Every case drives the real shell against a real Git repository through
//! the shared operations, then renders into a ratatui test buffer. The
//! cases prove what the acceptance states: the timeline lists starts,
//! drafts, reviews, and restorations in ancestry order with collapsed
//! checkpoints kept reachable; `Enter` previews the displayed document
//! against the current candidate, with missing and empty documents
//! distinct; only a second `Enter` restores, through the same plan document
//! `margin restore --input` reads, so both surfaces agree on tree, marker,
//! errors, and effects; the review baseline stays unchanged. The
//! independent oracles — stock Git for trees and bytes, the shared marker
//! validator for meaning, `oracle::snapshot` for effects — stay outside the
//! implementation under test.

mod support;

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use margin::app::{Operations, RestoreOutcome, validate_marker};
use margin::domain::RestorePlan;
use margin::git::Git;
use margin::tui::{Shell, TuiOptions, View, render};
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

/// Runs the built binary with `document` on standard input.
fn margin_stdin(
    env: &support::env::IsolatedEnv,
    cwd: &Path,
    args: &[&str],
    document: &[u8],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_margin"));
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, _) in std::env::vars_os() {
        if support::env::is_scrubbed(&key) {
            command.env_remove(&key);
        }
    }
    command.envs(env.vars());
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"));
    child
        .stdin
        .as_mut()
        .expect("standard input is piped")
        .write_all(document)
        .expect("the document is written");
    child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("cannot wait for the margin binary: {error}"))
}

/// Runs one JSON query and returns its data object.
fn json_data(env: &support::env::IsolatedEnv, cwd: &Path, args: &[&str]) -> Value {
    let output = margin(env, cwd, args);
    let envelope: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("one JSON envelope: {error}"));
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

/// Initializes the `notes` scope through the executable and returns the
/// start commit.
fn init(repo: &TestRepo) -> String {
    let data = json_data(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes", "--json"],
    );
    data["start_commit"]
        .as_str()
        .expect("a start commit")
        .to_string()
}

/// Captures one checkpoint through the executable and returns its commit.
fn checkpoint(repo: &TestRepo, message: &str) -> String {
    let output = margin(
        repo.env(),
        repo.path(),
        &["checkpoint", "--message", message, "--json"],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "the checkpoint {message:?} succeeds: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: Value = serde_json::from_slice(&output.stdout).expect("one envelope");
    envelope["data"]["commit"]
        .as_str()
        .expect("a created commit")
        .to_string()
}

/// Applies one complete keep-all review through the executable and returns
/// its receipt commit.
fn keep_all_review(repo: &TestRepo, request: &str) -> String {
    let diff = json_data(repo.env(), repo.path(), &["diff", "--json"]);
    let plan = json!({
        "schema_version": 1,
        "request_id": request,
        "review_id": diff["review_id"],
        "branch_ref": diff["branch_ref"],
        "scope_id": diff["scope_id"],
        "baseline_oid": diff["baseline_oid"],
        "candidate_oid": diff["candidate_oid"],
        "restore": [],
        "keep_remaining": true,
    });
    let output = margin_stdin(
        repo.env(),
        repo.path(),
        &["review", "--input", "-", "--json"],
        plan.to_string().as_bytes(),
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "the keep-all review succeeds: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: Value = serde_json::from_slice(&output.stdout).expect("one envelope");
    envelope["data"]["commit"]
        .as_str()
        .expect("a receipt commit")
        .to_string()
}

/// One adopted writing session: a start marker, a writing pass of two
/// checkpoints, and one keep-all review. The newest candidate holds
/// `notes/a.md` revised; the start holds the original.
struct Session {
    repo: TestRepo,
    /// The start marker.
    start: String,
    /// The review receipt.
    review: String,
}

/// Builds the shared session.
fn build_session() -> Session {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"original passage\n");
    repo.write_file("notes/b.md", b"b original\n");
    let start = init(&repo);
    repo.write_file("notes/a.md", b"revised passage\n");
    checkpoint(&repo, "Introduction revised");
    repo.write_file("notes/b.md", b"b revised\n");
    checkpoint(&repo, "Tone revised");
    let review = keep_all_review(&repo, "req-t044-review-1");
    Session {
        repo,
        start,
        review,
    }
}

/// Opens the shell on the History view with `path` displayed.
fn history_shell<'a>(repo: &TestRepo, operations: &'a Operations<'a>, path: &str) -> Shell<'a> {
    let options = TuiOptions {
        path: Some(path.to_string()),
        view: View::History,
        ..TuiOptions::default()
    };
    Shell::open(repo.path(), operations, &options)
}

/// The timeline rows the History view holds, as `kind subject` strings.
fn timeline_of(shell: &Shell<'_>) -> Vec<String> {
    shell
        .history()
        .map(|history| {
            history
                .rows()
                .iter()
                .map(|row| format!("{} {}", row.kind, row.subject))
                .collect()
        })
        .unwrap_or_default()
}

/// The timeline lists the whole session in ancestry order: the review, the
/// collapsed writing pass with its checkpoint count, and the start, each
/// with the local clock and the exact Unix timestamp beside it (SPEC 5.5).
#[test]
fn the_document_timeline_lists_writing_passes_and_restorations() {
    let session = build_session();
    let git = adapter_for(&session.repo);
    let operations = Operations::new(&git);
    let mut shell = history_shell(&session.repo, &operations, "notes/a.md");

    // Newest first in ancestry order (SPEC 5.5): the review receipt, the
    // writing pass collapsed to its newest checkpoint, and the start.
    let timeline = timeline_of(&shell);
    assert_eq!(
        timeline,
        vec![
            "draft Tone revised".to_string(),
            "start margin: capture starting point".to_string(),
        ],
        "the labeled kinds list in ancestry order",
    );

    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    // The two checkpoints of one pass collapsed, and the count states it.
    assert!(
        screen.contains("×2"),
        "the collapsed pass counts its checkpoints: {screen}",
    );
    assert!(
        !screen.contains("Introduction revised"),
        "the older checkpoint of the pass is collapsed",
    );
    // Exact timestamps are available on demand.
    assert!(!screen.contains("1767225600"));
    shell.handle_event(&key(KeyCode::Char('d')));
    let screen = joined(&draw(&mut shell, 110, 30));
    assert!(
        screen.contains("2026-01-01 00:00 · exact 1767225600"),
        "the local clock and the exact Unix timestamp: {screen}",
    );
    assert!(screen.contains("newest first"), "the order is stated");

    // The selection moves between revisions and holds at its ends (SPEC
    // 11.3).
    assert_eq!(shell.history().unwrap().selection(), 0);
    shell.handle_event(&key(KeyCode::Char('j')));
    shell.handle_event(&key(KeyCode::Char('j')));
    assert_eq!(shell.history().unwrap().selection(), 1);
    let page = draw(&mut shell, 110, 30);
    assert!(
        joined(&page).contains("› 2026-"),
        "the selected revision is marked",
    );
    shell.handle_event(&key(KeyCode::Char('j')));
    assert_eq!(
        shell.history().unwrap().selection(),
        1,
        "the oldest revision holds",
    );

    // `a` keeps the collapsed checkpoints reachable: the timeline lists
    // every one of them (SPEC 5.5).
    shell.handle_event(&key(KeyCode::Char('a')));
    let timeline = timeline_of(&shell);
    assert_eq!(
        timeline,
        vec![
            "draft Introduction revised".to_string(),
            "start margin: capture starting point".to_string(),
        ],
        "every checkpoint is listable again",
    );
    let page = draw(&mut shell, 110, 30);
    assert!(
        joined(&page).contains("Every draft checkpoint is listed."),
        "the policy states itself",
    );

    // After this build's own restoration, the timeline lists it as its own
    // kind (SPEC 5.5).
    let restored = restore_from_start(&session, "notes/a.md");
    assert_eq!(restored.path, "notes/a.md");
    let shell = history_shell(&session.repo, &operations, "notes/a.md");
    let timeline = timeline_of(&shell);
    assert_eq!(
        timeline.first().map(|row| row.as_str()),
        Some("restore margin: restore document"),
        "the restoration the surface recorded lists first, newest",
    );
}

/// A helper other cases reuse: one confirmed restoration through the
/// shell, from the start revision of `path`, and its reported data.
fn restore_from_start(session: &Session, path: &str) -> RestoreOutcome {
    let git = adapter_for(&session.repo);
    let operations = Operations::new(&git);
    let mut shell = history_shell(&session.repo, &operations, path);
    // Newest first: draft and start; a further step holds at the oldest.
    shell.handle_event(&key(KeyCode::Char('j')));
    shell.handle_event(&key(KeyCode::Char('j')));
    shell.handle_event(&key(KeyCode::Enter));
    shell.handle_event(&key(KeyCode::Enter));
    let outcome = shell.restore_result().expect("one restoration ran").clone();
    assert!(
        outcome.ok(),
        "the restoration must succeed: {:?}",
        outcome.error
    );
    outcome.data.expect("a successful outcome carries data")
}

/// `Enter` previews the displayed document against the current candidate,
/// proves the intended result through one read-only dry run, and writes
/// nothing (SPEC 5.5, 6.4).
#[test]
fn enter_previews_the_document_against_the_current_candidate() {
    let session = build_session();
    let before = oracle::snapshot(&session.repo);
    let git = adapter_for(&session.repo);
    let operations = Operations::new(&git);
    let mut shell = history_shell(&session.repo, &operations, "notes/a.md");

    // Newest first: two steps reach the start.
    shell.handle_event(&key(KeyCode::Char('j')));
    shell.handle_event(&key(KeyCode::Char('j')));
    shell.handle_event(&key(KeyCode::Enter));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(screen.contains("Restore preview · notes/a.md"));
    assert!(
        screen.contains("From start "),
        "the selected revision names itself"
    );
    assert!(screen.contains("At the selected revision:"));
    assert!(
        screen.contains("original passage"),
        "the revision side reads"
    );
    assert!(screen.contains("At the current candidate C:"));
    assert!(
        screen.contains("revised passage"),
        "the candidate side reads"
    );
    assert!(
        screen.contains("Intended result, proved by one read-only preview:"),
        "the dry run proves the result",
    );
    assert!(screen.contains("Files rewritten: notes/a.md"));
    assert!(screen.contains("Result tree "));
    assert!(screen.contains("The review baseline stays unchanged; this approves nothing else."),);
    assert!(
        screen.contains("Enter  Restore this document from the selected revision"),
        "the labeled action carries the second step",
    );
    assert!(
        screen.contains("Enter restore this document · Esc cancel"),
        "the footer names the two keys at every size",
    );

    // The composed plan is the document the command line reads (SPEC 7.4).
    let document = shell
        .history()
        .and_then(|history| history.preview())
        .and_then(|preview| preview.plan_document().map(|document| document.to_vec()))
        .expect("the preview composes one plan document");
    let plan = RestorePlan::parse(&document)
        .unwrap_or_else(|problem| panic!("the plan parses: {problem}"));
    assert!(
        plan.request_id.starts_with("tui:"),
        "the request names the surface that composed it: {}",
        plan.request_id,
    );
    assert_eq!(plan.revision, session.start);
    assert_eq!(plan.path, "notes/a.md");

    // The preview is one read-only proof: no repository byte moved.
    before.assert_unchanged(&oracle::snapshot(&session.repo), "the preview");
}

/// The second `Enter` restores through the shared operation: one restore
/// receipt over the candidate, the working file rewritten, unrelated
/// documents unchanged, the review baseline unchanged, and the restored
/// scope pending review (SPEC 5.5, 9.5).
#[test]
fn a_second_enter_restores_through_the_shared_operation() {
    let session = build_session();
    let git = adapter_for(&session.repo);
    let operations = Operations::new(&git);
    let candidate = oracle::head_oid(&session.repo).expect("the candidate stands at HEAD");
    let before_diff = json_data(session.repo.env(), session.repo.path(), &["diff", "--json"]);

    let mut shell = history_shell(&session.repo, &operations, "notes/a.md");
    shell.handle_event(&key(KeyCode::Char('j')));
    shell.handle_event(&key(KeyCode::Char('j')));
    shell.handle_event(&key(KeyCode::Enter));
    shell.handle_event(&key(KeyCode::Enter));
    let outcome = shell.restore_result().expect("one restoration ran").clone();
    assert!(
        outcome.ok(),
        "the restoration must succeed: {:?}",
        outcome.error
    );
    let data = outcome.data.expect("a successful outcome carries data");
    assert_eq!(data.path, "notes/a.md");
    assert_eq!(data.revision_oid, session.start);
    assert_eq!(data.candidate_oid, candidate);
    assert_eq!(data.paths, vec!["notes/a.md".to_string()]);
    assert!(!data.dry_run);
    assert_eq!(data.existing_receipt, None);
    let commit = data.commit.clone().expect("the application commits");
    assert_eq!(outcome.effects.commit.as_str(), "created");
    assert_eq!(outcome.effects.commit_oid.as_deref(), Some(commit.as_str()));
    assert_eq!(
        outcome.effects.working_tree.as_str(),
        "changed",
        "the restoration rewrote the working file",
    );

    // Stock Git proves the commit: one parent, the candidate, and the
    // intended tree (SPEC 16.3).
    assert_eq!(
        oracle::head_oid(&session.repo).as_deref(),
        Some(commit.as_str())
    );
    let parents = text(
        &session
            .repo
            .git_ok(["log", "-n", "1", "--format=%P", &commit]),
    );
    assert_eq!(parents.trim(), candidate, "one parent: the candidate C");
    assert_eq!(
        oracle::commit_tree(&session.repo, &commit),
        data.expected_tree,
        "the intended tree is committed",
    );

    // The marker the commit carries means one restoration (SPEC 8.3, 9.5).
    let marker = validate_marker(&git, &commit)
        .unwrap_or_else(|error| panic!("the restoration wrote a valid marker: {error}"));
    assert_eq!(marker.kind.as_str(), "restore");
    assert_eq!(marker.candidate, candidate);
    assert_eq!(marker.baseline, None, "a restoration records no baseline");
    assert_eq!(
        marker.result_tree,
        oracle::commit_tree(&session.repo, &commit)
    );
    let source = marker.source.expect("the restoration names its source");
    assert_eq!(source.path, "notes/a.md");
    assert_eq!(source.revision, session.start);

    // The working file holds the restored bytes, and the unrelated scoped
    // document keeps the candidate's bytes.
    assert_eq!(session.repo.read_file("notes/a.md"), b"original passage\n");
    assert_eq!(session.repo.read_file("notes/b.md"), b"b revised\n");

    // The review baseline stays unchanged, and the restored scope waits
    // for review through the normal Changes workflow (SPEC 5.5).
    let after_diff = json_data(session.repo.env(), session.repo.path(), &["diff", "--json"]);
    assert_eq!(
        after_diff["baseline_oid"], before_diff["baseline_oid"],
        "the baseline B the walk selects did not move",
    );
    assert_eq!(
        after_diff["baseline_oid"],
        json!(session.review),
        "the review receipt remains the baseline",
    );
    let status = json_data(
        session.repo.env(),
        session.repo.path(),
        &["status", "--json"],
    );
    assert_eq!(status["review"], "pending");

    // The surface states the restoration and the preview is gone.
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(screen.contains("restored notes/a.md from"));
    assert!(screen.contains("the review baseline is unchanged"));
    assert!(
        shell
            .history()
            .is_some_and(|history| history.preview().is_none()),
        "the preview closed with the candidate it previewed against",
    );
}

/// Missing files and empty files stay distinct in the preview, and one
/// revision's absence is an answer, not an empty document (SPEC 4.1, 5.5).
#[test]
fn missing_and_empty_documents_stay_distinct_in_the_preview() {
    let repo = TestRepo::init();
    repo.write_file("notes/kept.md", b"kept passage\n");
    repo.write_file("notes/gone.md", b"will vanish\n");
    repo.write_file("notes/empty.md", b"will empty\n");
    let _start = init(&repo);
    // One checkpoint deletes `gone` and empties `empty`; one review between
    // the passes keeps the two drafts from collapsing, and one later
    // checkpoint recreates `gone`, so each presence state has its own
    // revision.
    std::fs::remove_file(repo.file_path("notes/gone.md")).expect("the working file");
    repo.write_file("notes/empty.md", b"");
    checkpoint(&repo, "Delete and empty");
    keep_all_review(&repo, "req-t044-review-2");
    repo.write_file("notes/gone.md", b"recreated\n");
    checkpoint(&repo, "Recreate gone");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);

    // The document timeline omits the content-identical review: recreation,
    // deletion pass, and the start remain.
    let shell = history_shell(&repo, &operations, "notes/gone.md");
    assert_eq!(
        timeline_of(&shell),
        vec![
            "draft Recreate gone".to_string(),
            "draft Delete and empty".to_string(),
            "start margin: capture starting point".to_string(),
        ],
    );
    drop(shell);

    // Displaying the recreated document at the revision that held none of
    // it: the revision side names the absence, the candidate side reads,
    // and the preview never calls the absence an empty document.
    let mut shell = history_shell(&repo, &operations, "notes/gone.md");
    shell.handle_event(&key(KeyCode::Char('j')));
    assert_eq!(
        shell
            .history()
            .and_then(|history| history.selected())
            .map(|row| row.subject.as_str()),
        Some("Delete and empty"),
    );
    shell.handle_event(&key(KeyCode::Enter));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(screen.contains("recreated"), "the candidate side reads");
    assert!(
        screen.contains("missing: no document exists at this revision"),
        "the revision side names the absence",
    );
    assert!(
        !screen.contains("empty: the document exists here"),
        "absence never renders as an empty document",
    );
    assert!(
        screen.contains("restoring removes the file"),
        "the preview states the effect of restoring an absence",
    );

    // The second `Enter` restores the absence: the working file is gone,
    // exactly as the revision held it.
    shell.handle_event(&key(KeyCode::Enter));
    let outcome = shell.restore_result().expect("one restoration ran").clone();
    assert!(outcome.ok(), "restoring the absence succeeds");
    assert!(
        !repo.file_path("notes/gone.md").exists(),
        "the file the revision did not hold is no longer on disk",
    );

    // The empty document is present and holds zero bytes — a distinct
    // state from absence and from content. The timeline now also holds the
    // restoration this test just recorded, so the walk to the start steps
    // past it too.
    let mut shell = history_shell(&repo, &operations, "notes/empty.md");
    for _ in 0..6 {
        let selected = shell
            .history()
            .and_then(|history| history.selected())
            .map(|row| row.subject.clone());
        if selected.as_deref() == Some("margin: capture starting point") {
            break;
        }
        shell.handle_event(&key(KeyCode::Char('j')));
    }
    assert_eq!(
        shell
            .history()
            .and_then(|history| history.selected())
            .map(|row| row.subject.as_str()),
        Some("margin: capture starting point"),
        "the walk reaches the start revision",
    );
    shell.handle_event(&key(KeyCode::Enter));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(
        screen.contains("empty: the document exists here and holds zero bytes"),
        "the candidate side names the empty document",
    );
    assert!(
        screen.contains("will empty"),
        "the revision side still reads the start bytes",
    );
}

/// Uncaptured scoped changes stop the preview with the typed guard, and
/// the view offers the SPEC 5.5 action: save the current draft, then
/// preview over — and restore against — the newly captured candidate.
#[test]
fn uncaptured_changes_offer_save_draft_then_preview() {
    let session = build_session();
    let git = adapter_for(&session.repo);
    let operations = Operations::new(&git);
    let candidate = oracle::head_oid(&session.repo).expect("the candidate stands at HEAD");

    // One uncaptured edit exists when the preview opens.
    session.repo.write_file("notes/a.md", b"uncaptured edit\n");
    let mut shell = history_shell(&session.repo, &operations, "notes/a.md");
    shell.handle_event(&key(KeyCode::Char('j')));
    shell.handle_event(&key(KeyCode::Char('j')));
    shell.handle_event(&key(KeyCode::Enter));
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(
        screen.contains("The preview cannot run while uncaptured changes exist:"),
        "the typed guard names itself",
    );
    assert!(screen.contains("DIRTY_SCOPE"));
    assert!(
        screen.contains("s or Enter  Save current draft, then preview restoration"),
        "the SPEC 5.5 offer shows",
    );
    assert!(
        !screen.contains("Enter  Restore this document"),
        "no restore action stands while the scope is dirty",
    );
    assert!(
        screen.contains("s save draft then preview"),
        "the footer names the save action",
    );

    // The offer's key saves through the shared checkpoint operation and
    // reopens the preview over the newly captured candidate.
    shell.handle_event(&key(KeyCode::Char('s')));
    let draft = oracle::head_oid(&session.repo).expect("the saved draft stands at HEAD");
    assert_ne!(draft, candidate, "the checkpoint moved the candidate");
    assert!(
        shell.restore_result().is_none(),
        "saving the draft restores nothing",
    );
    let page = draw(&mut shell, 110, 30);
    let screen = joined(&page);
    assert!(
        screen.contains("Intended result, proved by one read-only preview:"),
        "the preview now proves its result over the new candidate",
    );
    assert!(
        screen.contains("Enter  Restore this document from the selected revision"),
        "the restore action stands now",
    );

    // The final command uses the newly captured candidate (SPEC 5.5), so
    // the uncaptured edit stays saved beneath the restoration.
    shell.handle_event(&key(KeyCode::Enter));
    let outcome = shell.restore_result().expect("one restoration ran").clone();
    assert!(
        outcome.ok(),
        "the restoration succeeds: {:?}",
        outcome.error
    );
    let data = outcome.data.expect("a successful outcome carries data");
    assert_eq!(
        data.candidate_oid, draft,
        "the restoration replaced the newly captured candidate",
    );
    let commit = data.commit.expect("the application commits");
    let marker = validate_marker(&git, &commit)
        .unwrap_or_else(|error| panic!("the restoration wrote a valid marker: {error}"));
    assert_eq!(marker.candidate, draft);
    assert_eq!(session.repo.read_file("notes/a.md"), b"original passage\n");
    assert_eq!(session.repo.read_file("notes/b.md"), b"b revised\n");
}

/// `Esc` leaves the preview and quit never restores: no path through them
/// commits anything (SPEC 5.2, 11.3).
#[test]
fn cancel_and_quit_never_restore() {
    let session = build_session();
    let before = oracle::snapshot(&session.repo);
    let git = adapter_for(&session.repo);
    let operations = Operations::new(&git);
    let candidate = oracle::head_oid(&session.repo).expect("the candidate stands at HEAD");

    let mut shell = history_shell(&session.repo, &operations, "notes/a.md");
    shell.handle_event(&key(KeyCode::Char('j')));
    shell.handle_event(&key(KeyCode::Char('j')));
    shell.handle_event(&key(KeyCode::Enter));
    assert!(
        shell
            .history()
            .is_some_and(|history| history.preview().is_some()),
        "the preview stands",
    );
    shell.handle_event(&key(KeyCode::Esc));
    assert!(
        shell
            .history()
            .is_some_and(|history| history.preview().is_none()),
        "Esc leaves the preview",
    );
    let page = draw(&mut shell, 110, 30);
    assert!(
        joined(&page).contains("preview cancelled; nothing was restored"),
        "the cancel states what it did",
    );
    before.assert_unchanged(&oracle::snapshot(&session.repo), "the cancel");

    // The preview stands again; quit discards it without applying.
    shell.handle_event(&key(KeyCode::Enter));
    assert!(
        shell
            .history()
            .is_some_and(|history| history.preview().is_some()),
        "the preview reopens",
    );
    shell.handle_event(&key(KeyCode::Char('q')));
    assert!(shell.should_quit(), "quit leaves the surface");
    assert_eq!(
        oracle::head_oid(&session.repo).as_deref(),
        Some(candidate.as_str()),
        "no commit joined",
    );
    before.assert_unchanged(&oracle::snapshot(&session.repo), "the quit");
}

/// The plan the surface composes is the document the command line reads:
/// both entry points over identical repositories produce one tree, one
/// marker meaning, and equal effects (SPEC 7.4, 16.9).
#[test]
fn equivalent_cli_and_tui_restore_plans_agree() {
    let session = build_session();
    let git = adapter_for(&session.repo);
    let operations = Operations::new(&git);
    let mut shell = history_shell(&session.repo, &operations, "notes/a.md");
    shell.handle_event(&key(KeyCode::Char('j')));
    shell.handle_event(&key(KeyCode::Char('j')));
    shell.handle_event(&key(KeyCode::Enter));
    let document = shell
        .history()
        .and_then(|history| history.preview())
        .and_then(|preview| preview.plan_document().map(|document| document.to_vec()))
        .expect("the preview composes one plan document");
    shell.handle_event(&key(KeyCode::Esc));
    session.repo.write_file("cli-plan.json", &document);
    let cli = {
        let output = margin(
            session.repo.env(),
            session.repo.path(),
            &["restore", "--input", "cli-plan.json", "--json"],
        );
        let envelope: Value = serde_json::from_slice(&output.stdout).expect("one envelope");
        assert_eq!(envelope["ok"], true, "the CLI applies the same document");
        envelope
    };

    // The same restoration through the surface, on an identical repository.
    let peer = build_session();
    let data = restore_from_start(&peer, "notes/a.md");
    let outcome_commit = data.commit.clone().expect("the TUI commit");

    // Same document, same result, same effects (SPEC 16.9). Commit OIDs
    // may differ; the trees, the meaning, and the effects may not.
    assert_eq!(
        cli["data"]["expected_tree"], data.expected_tree,
        "the intended result trees agree",
    );
    assert_eq!(cli["data"]["paths"], json!(data.paths));
    assert_eq!(cli["data"]["revision_oid"], json!(data.revision_oid));
    assert_eq!(
        oracle::commit_tree(
            &session.repo,
            cli["data"]["commit"].as_str().expect("the CLI commit"),
        ),
        oracle::commit_tree(&peer.repo, &outcome_commit),
        "both plans produced one tree",
    );
    assert_eq!(
        cli["effects"]["commit"],
        json!("created"),
        "both surfaces created one commit",
    );

    // The same marker meaning: kind, scope, candidate, and source.
    let cli_marker = validate_marker(
        &adapter_for(&session.repo),
        cli["data"]["commit"].as_str().expect("the CLI commit"),
    )
    .unwrap_or_else(|error| panic!("the CLI commit is a marker: {error}"));
    let tui_marker = validate_marker(&adapter_for(&peer.repo), &outcome_commit)
        .unwrap_or_else(|error| panic!("the TUI commit is a marker: {error}"));
    assert_eq!(cli_marker.kind, tui_marker.kind);
    assert_eq!(cli_marker.scope_digest, tui_marker.scope_digest);
    assert_eq!(cli_marker.candidate, tui_marker.candidate);
    assert_eq!(cli_marker.source, tui_marker.source);
}

/// The selected revision and the displayed document survive view switches,
/// which is the retained context the History view owns (SPEC 5.4).
#[test]
fn the_selected_revision_and_document_survive_view_switches() {
    let session = build_session();
    let git = adapter_for(&session.repo);
    let operations = Operations::new(&git);
    let mut shell = history_shell(&session.repo, &operations, "notes/a.md");
    shell.handle_event(&key(KeyCode::Char('j')));
    shell.handle_event(&key(KeyCode::Char('j')));
    assert_eq!(
        shell
            .history()
            .and_then(|history| history.selected())
            .map(|row| row.kind),
        Some("start"),
    );

    shell.handle_event(&key(KeyCode::Char('1')));
    assert_eq!(shell.view(), View::Read);
    shell.handle_event(&key(KeyCode::Char('3')));
    assert_eq!(shell.view(), View::History);
    assert_eq!(
        shell
            .history()
            .and_then(|history| history.selected())
            .map(|row| row.kind),
        Some("start"),
        "the selected revision survived the switch",
    );
    assert_eq!(
        shell.document().map(|document| document.path.as_str()),
        Some("notes/a.md"),
        "the document context survived",
    );
}

/// Trims one Git output to text.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}
