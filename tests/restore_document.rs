//! Real-Git tests for historical restoration (SPEC 7.4 and 9.5, task T033).
//!
//! Every case proves one contract of the restoration against the installed
//! Git:
//!
//! - one restoration previews and commits one scoped document from an
//!   available adopted-history revision: the committed result holds exactly
//!   the source revision's entry at the document — including absence and
//!   storage mode — every other entry keeps the candidate's, the reserved
//!   `restore` trailers name the request, the plan fingerprint, and the
//!   source, and the replaced candidate stays reachable as the parent
//! - the review baseline stays unchanged: the restored content is pending
//!   review against the same anchor, never approved by the receipt
//! - an exact retry returns the original verified receipt without a second
//!   commit, a reused request ID under another fingerprint conflicts, and a
//!   restoration that changes no content still creates one receipt commit
//!   per new request (SPEC 7.4)
//! - the strict plan contract of SPEC 7.4 refuses unknown or duplicate
//!   fields, missing context, malformed identities, and trailer injection
//! - the guards refuse typed and unchanged: uncaptured scoped bytes, a
//!   staged entry, a foreign branch, a stale candidate context, a path
//!   outside the scope, and a source revision outside the adopted history
//! - a skip-worktree flag cannot hide uncaptured bytes from a
//!   restoration: the specific index-flag blocker refuses both phases
//!   before any mutation, and the cleared flag leaves the ordinary
//!   `DIRTY_SCOPE` refusal over the same bytes
//! - a dry run reports the proved intended tree and writes nothing
//! - both object formats apply, and the installed executable reports one
//!   envelope in JSON mode and one line in text mode
//!
//! The oracles stay independent of the implementation (SPEC 16.3): commits,
//! parents, trees, and entries come from `git rev-parse`, `git log`, and
//! `git ls-tree`, the marker from the shared validator, and the repository
//! state from `oracle::snapshot` and `git status`.

mod support;

use std::path::Path;
use std::process::{Command, Output, Stdio};

use margin::app::{
    BaselineStatus, CommitEffect, DiffRequest, Effects, Identities, InitRequest, InspectRequest,
    OperationOutcome, Operations, RepositoryEffect, RestoreOutcome, RestoreRequest,
    WorkspaceInspection, validate_marker,
};
use margin::domain::{RestorePlan, RestoreSource, ReviewState, ScopeInput};
use margin::git::Git;
use serde_json::{Value, json};
use support::env::{IsolatedEnv, is_scrubbed};
use support::repo::{ObjectFormat, RepoOptions, TestRepo};
use support::{oracle, text};

// ---------------------------------------------------------------------------
// The shared fixture
// ---------------------------------------------------------------------------

/// The original document body.
const ORIGINAL: &str = "original passage
second original passage
";

/// The revised document body.
const REVISED: &str = "revised passage
second revised passage
";

/// The seeded workspace: a start marker A and a committed candidate C, with
/// one out-of-scope note that changed between them, so no restoration may
/// trade the note's candidate entry for the source revision's.
struct Fixture {
    repo: TestRepo,
    /// The start marker: one adopted-history source revision.
    start: String,
    /// The committed candidate C the restoration replaces.
    candidate: String,
}

fn fixture(options: RepoOptions) -> Fixture {
    let repo = TestRepo::init_with(options);
    repo.write_file("doc-a.md", ORIGINAL.as_bytes());
    repo.write_file("doc-b.md", b"doc-b original\n");
    repo.write_file("note.txt", b"the baseline note\n");
    let start = initialize(&repo);
    repo.write_file("doc-a.md", REVISED.as_bytes());
    repo.write_file("doc-b.md", b"doc-b revised\n");
    repo.write_file("note.txt", b"the candidate note\n");
    let candidate = repo.commit_all("writing pass");
    Fixture {
        repo,
        start,
        candidate,
    }
}

/// The whole-repository Markdown scope.
fn scope() -> ScopeInput {
    ScopeInput {
        include: vec![".".to_string()],
        exclude: Vec::new(),
        suffixes: vec![".md".to_string()],
    }
}

/// Adapter bound to the fixture's working tree and isolated environment.
fn adapter_for(repo: &TestRepo) -> Git {
    Git::from_env(repo.env().vars().clone()).at(repo.path())
}

/// Initializes the whole-repository scope and returns the start marker.
fn initialize(repo: &TestRepo) -> String {
    let git = adapter_for(repo);
    let operations = Operations::new(&git);
    let result = operations.initialize_scope(&InitRequest {
        scope: Some(scope()),
        create_repository: false,
        branch: None,
        new_start: false,
        auto_checkpoint: None,
        capture_after_edit: None,
        editor_argv: None,
        dry_run: false,
    });
    assert!(
        result.ok(),
        "initialization must succeed: {:?}",
        result.error
    );
    result
        .data
        .and_then(|data| data.start_commit)
        .expect("first use creates one start commit")
}

/// The workspace status both interfaces share, with the identity fields a
/// restoration plan pins to.
fn status_of(repo: &TestRepo) -> WorkspaceInspection {
    let git = adapter_for(repo);
    let operations = Operations::new(&git);
    let status = operations.inspect_workspace(&InspectRequest {
        scope: None,
        path_filter: Vec::new(),
    });
    assert!(status.ok(), "the status query succeeds: {:?}", status.error);
    status.data.expect("a successful status carries data")
}

/// One restoration plan document (SPEC 7.4): the pinned context of `status`,
/// the one document, and the source revision.
fn plan_document(
    status: &WorkspaceInspection,
    request: &str,
    path: &str,
    revision: &str,
    message: Option<&str>,
) -> String {
    let mut document = json!({
        "schema_version": 1,
        "request_id": request,
        "branch_ref": status.branch.clone().expect("an attached HEAD"),
        "scope_id": status.scope.digest,
        "candidate_oid": status.head.clone().expect("a born branch"),
        "path": path,
        "revision": revision,
    });
    if let Some(message) = message {
        document["message"] = json!(message);
    }
    document.to_string()
}

/// Restores through the operation layer with the dry-run flag of the request.
fn restore_dry(repo: &TestRepo, plan: &str, dry_run: bool) -> OperationOutcome<RestoreOutcome> {
    let git = adapter_for(repo);
    let operations = Operations::new(&git);
    operations.restore_document(&RestoreRequest {
        scope: None,
        plan: plan.as_bytes().to_vec(),
        dry_run,
    })
}

/// Restores through the operation layer.
fn restore(repo: &TestRepo, plan: &str) -> OperationOutcome<RestoreOutcome> {
    restore_dry(repo, plan, false)
}

/// The plan fingerprint of one plan document, through the shared identities
/// (SPEC 7.4, 8.6).
fn fingerprint_of(repo: &TestRepo, plan: &str) -> String {
    let parsed = RestorePlan::parse(plan.as_bytes())
        .unwrap_or_else(|problem| panic!("the plan must parse: {problem}"));
    Identities::new(&adapter_for(repo))
        .unwrap_or_else(|error| panic!("identities must read the repository: {error}"))
        .restore_plan(&parsed.identity())
        .unwrap_or_else(|error| panic!("the plan must hash: {error}"))
        .id()
}

/// Asserts one typed refusal: not successful, one primary code, and the
/// unchanged effects of an operation that wrote nothing.
fn assert_refusal(outcome: &OperationOutcome<RestoreOutcome>, code: &str, what: &str) {
    assert!(!outcome.ok(), "{what} must not succeed");
    let error = outcome.error.as_ref().unwrap();
    assert_eq!(error.code, code, "{what} reports the wrong code");
    assert_eq!(
        outcome.effects,
        Effects::unchanged(),
        "{what} wrote nothing"
    );
}

/// Runs the installed executable once with the isolated environment.
fn margin(env: &IsolatedEnv, cwd: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_margin"));
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, _) in std::env::vars_os() {
        if is_scrubbed(&key) {
            command.env_remove(&key);
        }
    }
    command.envs(env.vars());
    command
        .output()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"))
}

// ---------------------------------------------------------------------------
// The verified restoration commit
// ---------------------------------------------------------------------------

/// One restoration replaces exactly the one document with the source
/// revision's entry, commits a `restore` receipt over the candidate, and
/// leaves the baseline and every other entry unchanged (SPEC 7.4, 9.5).
#[test]
fn restoration_replaces_one_document_from_adopted_history() {
    let fixture = fixture(RepoOptions::new());
    let status = status_of(&fixture.repo);
    let plan = plan_document(
        &status,
        "restore-doc-a-01",
        "doc-a.md",
        &fixture.start,
        Some("recover the original passages"),
    );
    let before = oracle::snapshot(&fixture.repo);
    let source_entry = oracle::ls_tree(&fixture.repo, &fixture.start)
        .into_iter()
        .find(|entry| entry.path == "doc-a.md")
        .expect("the source revision holds the document");

    let outcome = restore(&fixture.repo, &plan);
    assert!(outcome.ok(), "the restoration applies: {:?}", outcome.error);
    let data = outcome.data.unwrap();
    assert!(!data.dry_run);
    assert_eq!(data.request_id, "restore-doc-a-01");
    assert_eq!(data.path, "doc-a.md");
    assert_eq!(data.revision_oid, fixture.start);
    assert_eq!(data.paths, vec!["doc-a.md".to_string()]);
    assert_eq!(data.baseline_oid.as_deref(), Some(fixture.start.as_str()));
    assert_eq!(data.message, "recover the original passages");
    assert_eq!(data.existing_receipt, None);
    assert_eq!(data.candidate_oid, fixture.candidate);

    // Exactly one commit joined, and the response names it (SPEC 7.1).
    let commit = data.commit.clone().expect("the restoration commits");
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(commit.as_str())
    );
    assert_eq!(outcome.effects.commit, CommitEffect::Created);
    assert_eq!(outcome.effects.commit_oid.as_deref(), Some(commit.as_str()));
    assert_eq!(outcome.effects.working_tree.as_str(), "changed");
    assert_eq!(outcome.effects.index.as_str(), "changed");
    assert_eq!(outcome.effects.repository, RepositoryEffect::Unchanged);

    // The replaced candidate stays reachable as the single parent (SPEC 9.5).
    let parents = text(
        &fixture
            .repo
            .git_ok(["log", "-n", "1", "--format=%P", &commit]),
    );
    assert_eq!(
        parents.trim(),
        fixture.candidate,
        "one parent: the replaced candidate"
    );

    // The committed tree holds the source entry at the document and the
    // candidate's entries everywhere else, checked against the independent
    // tree oracle (SPEC 16.3).
    let tree = oracle::commit_tree(&fixture.repo, &commit);
    assert_eq!(
        data.expected_tree, tree,
        "the preview tree is the commit tree"
    );
    let doc_a = oracle::ls_tree(&fixture.repo, &tree)
        .into_iter()
        .find(|entry| entry.path == "doc-a.md")
        .expect("the document stays present");
    assert_eq!(
        (doc_a.mode.as_str(), doc_a.oid.as_str()),
        (source_entry.mode.as_str(), source_entry.oid.as_str()),
        "the document equals the source revision's entry, mode included"
    );
    assert_eq!(oracle::blob(&fixture.repo, &doc_a.oid), ORIGINAL.as_bytes());
    oracle::assert_out_of_scope_tree_unchanged(
        &fixture.repo,
        &fixture.candidate,
        &commit,
        &["doc-a.md"],
        "the restoration",
    );

    // The working bytes and the index describe the same result (SPEC 9.4
    // step 3), and the out-of-scope note kept its candidate bytes.
    assert_eq!(fixture.repo.read_file("doc-a.md"), ORIGINAL.as_bytes());
    assert_eq!(
        fixture.repo.read_file("note.txt"),
        b"the candidate note\n",
        "an out-of-scope working file is untouched"
    );
    oracle::assert_index_matches_tree(&fixture.repo, "HEAD");
    assert_eq!(text(&fixture.repo.git_ok(["status", "--porcelain"])), "");
    before.assert_out_of_scope_unchanged(
        &oracle::snapshot(&fixture.repo),
        &["doc-a.md"],
        "the restoration",
    );

    // The reserved trailers the marker validator proves (SPEC 8.3-8.4): a
    // restore receipt, never a review marker.
    let marker = validate_marker(&adapter_for(&fixture.repo), &commit)
        .unwrap_or_else(|error| panic!("the restore commit is a valid marker: {error}"));
    assert_eq!(marker.kind.as_str(), "restore");
    assert_eq!(marker.candidate, fixture.candidate);
    assert_eq!(marker.result_tree, tree);
    assert_eq!(marker.baseline, None, "a restore records no baseline");
    assert_eq!(marker.request.as_deref(), Some("restore-doc-a-01"));
    assert_eq!(
        marker.plan.as_deref(),
        Some(fingerprint_of(&fixture.repo, &plan).as_str())
    );
    assert_eq!(
        marker.source,
        Some(RestoreSource {
            path: "doc-a.md".to_string(),
            revision: fixture.start.clone(),
        })
    );
    let message = text(
        &fixture
            .repo
            .git_ok(["log", "-n", "1", "--format=%B", &commit]),
    );
    assert!(
        message.starts_with("recover the original passages\n\n"),
        "the explicit message leads the commit: {message:?}"
    );
    assert!(!message.contains("Margin-Review"));

    // The baseline stays unchanged and the restored content is pending
    // review against the same anchor (SPEC 9.5).
    let after = status_of(&fixture.repo);
    assert_eq!(after.review, ReviewState::Pending);
    match after.baseline {
        BaselineStatus::Anchored(anchor) => {
            assert_eq!(anchor.commit, fixture.start, "the anchor never moved");
            assert_eq!(anchor.kind.as_str(), "start");
        }
        other => panic!("the start marker still anchors the scope: {other:?}"),
    }
}

/// The diff after one restoration compares the restored content against the
/// unchanged baseline, so the restoration is unreviewed content the next
/// review decides (SPEC 9.5).
#[test]
fn restored_content_is_pending_review_against_the_same_baseline() {
    let fixture = fixture(RepoOptions::new());
    let status = status_of(&fixture.repo);
    let plan = plan_document(
        &status,
        "restore-pending-01",
        "doc-a.md",
        &fixture.start,
        None,
    );
    let outcome = restore(&fixture.repo, &plan);
    assert!(outcome.ok(), "the restoration applies: {:?}", outcome.error);
    let commit = outcome.data.unwrap().commit.expect("the receipt commit");

    let git = adapter_for(&fixture.repo);
    let operations = Operations::new(&git);
    let diff = operations.build_review(&DiffRequest {
        scope: None,
        path_filter: Vec::new(),
        change: None,
        cursor: None,
        limit: None,
        working: false,
    });
    assert!(diff.ok(), "the next diff builds: {:?}", diff.error);
    let data = diff.data.unwrap();
    assert_eq!(data.baseline_oid, fixture.start, "B never moved");
    assert_eq!(data.candidate_oid, commit, "C is the restore commit");
    assert!(
        data.scope_totals.files >= 1,
        "the restored document is pending review"
    );
    assert!(data.actionable);
}

/// A restoration may remove the document: a source revision that holds no
/// entry at the path restores absence, and the working file disappears
/// (SPEC 9.5, 4.1).
#[test]
fn restoration_restores_absence() {
    let repo = TestRepo::init();
    repo.write_file("doc-a.md", ORIGINAL.as_bytes());
    // The source revision precedes the addition: doc-c.md is absent there.
    let source = repo.commit_all("source without doc-c");
    repo.write_file("doc-c.md", b"late addition\n");
    let previous = repo.commit_all("add the document");
    let anchor = initialize(&repo);
    assert_eq!(
        text(&repo.git_ok(["rev-list", "--count", "--first-parent", "HEAD"])),
        "3",
        "source, addition, and the start marker"
    );

    let status = status_of(&repo);
    let plan = plan_document(&status, "restore-absence-01", "doc-c.md", &source, None);
    let before = oracle::snapshot(&repo);

    let outcome = restore(&repo, &plan);
    assert!(
        outcome.ok(),
        "the absence restoration applies: {:?}",
        outcome.error
    );
    let data = outcome.data.unwrap();
    let commit = data.commit.expect("the receipt commit");
    assert_eq!(data.paths, vec!["doc-c.md".to_string()]);

    // The committed tree holds no entry at the document, the parent is the
    // anchor the plan pinned, and everything else keeps its entries.
    let tree = oracle::commit_tree(&repo, &commit);
    assert!(
        oracle::ls_tree(&repo, &tree)
            .iter()
            .all(|entry| entry.path != "doc-c.md"),
        "the restored tree holds no entry at the document"
    );
    let parents = text(&repo.git_ok(["log", "-n", "1", "--format=%P", &commit]));
    assert_eq!(parents.trim(), anchor);
    oracle::assert_out_of_scope_tree_unchanged(
        &repo,
        &anchor,
        &commit,
        &["doc-c.md"],
        "the absence restoration",
    );
    assert_ne!(anchor, previous);

    // The working file is gone and the status is clean (SPEC 9.4 step 3).
    assert!(
        !repo.file_path("doc-c.md").exists(),
        "a restored deletion leaves no working file"
    );
    assert_eq!(text(&repo.git_ok(["status", "--porcelain"])), "");
    oracle::assert_index_matches_tree(&repo, "HEAD");
    before.assert_out_of_scope_unchanged(
        &oracle::snapshot(&repo),
        &["doc-c.md"],
        "the absence restoration",
    );
    let marker = validate_marker(&adapter_for(&repo), &commit).unwrap();
    assert_eq!(
        marker.source,
        Some(RestoreSource {
            path: "doc-c.md".to_string(),
            revision: source,
        })
    );
}

/// A restoration restores the storage mode too: a source revision that
/// stores the document executable returns the executable entry and the
/// executable working file (SPEC 9.5).
#[test]
fn restoration_restores_the_storage_mode() {
    let repo = TestRepo::init();
    repo.write_file("doc-a.md", ORIGINAL.as_bytes());
    repo.set_mode("doc-a.md", 0o755);
    let source = repo.commit_all("executable source");
    repo.set_mode("doc-a.md", 0o644);
    repo.commit_all("plain candidate");
    initialize(&repo);

    let status = status_of(&repo);
    let plan = plan_document(&status, "restore-mode-01", "doc-a.md", &source, None);
    let outcome = restore(&repo, &plan);
    assert!(
        outcome.ok(),
        "the mode restoration applies: {:?}",
        outcome.error
    );
    let data = outcome.data.unwrap();
    let commit = data.commit.expect("the receipt commit");

    let entry = oracle::ls_tree(&repo, &commit)
        .into_iter()
        .find(|entry| entry.path == "doc-a.md")
        .expect("the document stays present");
    assert_eq!(entry.mode, "100755", "the committed entry is executable");
    assert_eq!(oracle::disk_mode(&repo, "doc-a.md") & 0o111, 0o111);
    assert_eq!(text(&repo.git_ok(["status", "--porcelain"])), "");
}

/// A restoration whose source revision holds the candidate's own entry
/// changes no content and still commits one receipt per request, so its
/// retry semantics stay durable (SPEC 7.4).
#[test]
fn a_no_content_change_restoration_still_creates_one_receipt() {
    let fixture = fixture(RepoOptions::new());
    let status = status_of(&fixture.repo);
    // The candidate itself belongs to its own adopted history: restoring
    // from it names the current entry.
    let plan = plan_document(
        &status,
        "restore-noop-01",
        "doc-a.md",
        &fixture.candidate,
        None,
    );
    let before = oracle::snapshot(&fixture.repo);
    let commits_before =
        text(
            &fixture
                .repo
                .git_ok(["rev-list", "--count", "--first-parent", "HEAD"]),
        );

    let outcome = restore(&fixture.repo, &plan);
    assert!(
        outcome.ok(),
        "the no-op restoration applies: {:?}",
        outcome.error
    );
    let data = outcome.data.unwrap();
    let commit = data.commit.expect("one receipt per new request");
    assert_eq!(data.paths, Vec::<String>::new(), "no path is rewritten");
    assert_eq!(
        data.expected_tree,
        oracle::commit_tree(&fixture.repo, &fixture.candidate),
        "T is the candidate's own tree"
    );
    assert_eq!(
        outcome.effects.commit,
        CommitEffect::Created,
        "the receipt itself is a commit"
    );
    assert_eq!(outcome.effects.working_tree.as_str(), "unchanged");
    assert_eq!(outcome.effects.index.as_str(), "unchanged");
    assert_eq!(
        text(
            &fixture
                .repo
                .git_ok(["rev-list", "--count", "--first-parent", "HEAD"])
        ),
        (commits_before.trim().parse::<usize>().unwrap() + 1).to_string(),
        "one empty receipt commit joined"
    );
    // No working file or index entry moved.
    let after = oracle::snapshot(&fixture.repo);
    assert!(
        before.working_diff(&after).is_empty(),
        "no working file changed"
    );
    oracle::assert_index_matches_tree(&fixture.repo, "HEAD");
    let marker = validate_marker(&adapter_for(&fixture.repo), &commit).unwrap();
    assert_eq!(marker.kind.as_str(), "restore");

    // The second, different request commits a second receipt (SPEC 7.4):
    // its plan pins the head the first receipt moved to.
    let advanced = status_of(&fixture.repo);
    let second = plan_document(&advanced, "restore-noop-02", "doc-a.md", &commit, None);
    let second = restore(&fixture.repo, &second);
    assert!(
        second.ok(),
        "a new request commits again: {:?}",
        second.error
    );
    assert_ne!(
        second.data.unwrap().commit,
        Some(commit),
        "one receipt per new request"
    );
}

// ---------------------------------------------------------------------------
// Receipts and retries
// ---------------------------------------------------------------------------

/// An exact retry returns the original verified receipt without a second
/// commit — even after `HEAD` advanced and uncaptured bytes appeared, because
/// the lookup precedes every present-day stale check (SPEC 8.7).
#[test]
fn an_exact_retry_returns_the_original_receipt() {
    let fixture = fixture(RepoOptions::new());
    let status = status_of(&fixture.repo);
    let plan = plan_document(
        &status,
        "restore-retry-01",
        "doc-a.md",
        &fixture.start,
        None,
    );
    let first = restore(&fixture.repo, &plan);
    assert!(
        first.ok(),
        "the first restoration applies: {:?}",
        first.error
    );
    let receipt = first
        .data
        .as_ref()
        .unwrap()
        .commit
        .clone()
        .expect("the receipt commit");

    // New uncaptured bytes and a new commit: both would block a new
    // mutation, and neither hides the receipt the history holds.
    fixture
        .repo
        .write_file("doc-a.md", b"newer uncaptured content\n");
    let advanced = fixture.repo.commit_all("advance the branch");

    let retry = restore(&fixture.repo, &plan);
    assert!(
        retry.ok(),
        "the exact retry reports its receipt: {:?}",
        retry.error
    );
    let data = retry.data.unwrap();
    assert_eq!(data.existing_receipt.as_deref(), Some(receipt.as_str()));
    assert_eq!(data.commit, None, "no second restore commit");
    assert_eq!(
        data.candidate_oid, fixture.candidate,
        "the recorded context"
    );
    assert_eq!(
        data.expected_tree,
        first.data.as_ref().unwrap().expected_tree,
        "the receipt's result tree answers from history"
    );
    assert_eq!(retry.effects.commit, CommitEffect::ExistingReceipt);
    assert_eq!(retry.effects.commit_oid.as_deref(), Some(receipt.as_str()));
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(advanced.as_str()),
        "the retry commits nothing"
    );
}

/// A request ID reused under another fingerprint conflicts, and a dry run of
/// a completed request reports the same receipt (SPEC 7.3, 8.7).
#[test]
fn a_reused_request_id_under_another_fingerprint_conflicts() {
    let fixture = fixture(RepoOptions::new());
    let status = status_of(&fixture.repo);
    let plan = plan_document(
        &status,
        "restore-conflict-01",
        "doc-a.md",
        &fixture.start,
        None,
    );
    let first = restore(&fixture.repo, &plan);
    assert!(
        first.ok(),
        "the first restoration applies: {:?}",
        first.error
    );

    // The same request ID with another message names another plan.
    let changed = plan_document(
        &status,
        "restore-conflict-01",
        "doc-a.md",
        &fixture.start,
        Some("a different decision"),
    );
    assert_refusal(
        &restore(&fixture.repo, &changed),
        "REQUEST_ID_CONFLICT",
        "the reused request ID",
    );
    // The conflict holds for the dry run too.
    assert_refusal(
        &restore_dry(&fixture.repo, &changed, true),
        "REQUEST_ID_CONFLICT",
        "the reused request ID in a dry run",
    );

    // A dry run of the exact completed request reports the receipt (SPEC 6.4).
    let dry = restore_dry(&fixture.repo, &plan, true);
    assert!(dry.ok(), "the dry run reports the receipt: {:?}", dry.error);
    let data = dry.data.unwrap();
    assert!(data.dry_run);
    assert_eq!(data.commit, None);
    assert_eq!(
        data.existing_receipt.as_deref(),
        first.data.unwrap().commit.as_deref(),
        "the dry run reports the verified receipt"
    );
}

// ---------------------------------------------------------------------------
// The strict plan contract (SPEC 7.4)
// ---------------------------------------------------------------------------

/// The strict parser accepts exactly the documented shape and refuses every
/// undocumented one (SPEC 7.4, 7.6).
#[test]
fn the_plan_contract_parses_strictly() {
    let valid = json!({
        "schema_version": 1,
        "request_id": "restore-parse-01",
        "branch_ref": "refs/heads/writing",
        "scope_id": "sha1:0123456789abcdef0123456789abcdef01234567",
        "candidate_oid": "0123456789abcdef0123456789abcdef01234567",
        "path": "./drafts/My Draft.md",
        "revision": "89abcdef0123456789abcdef0123456789abcdef",
    })
    .to_string();
    let plan = RestorePlan::parse(valid.as_bytes())
        .unwrap_or_else(|problem| panic!("the documented shape parses: {problem}"));
    assert_eq!(plan.path, "drafts/My Draft.md", "the path normalizes");
    assert_eq!(
        plan.message, "margin: restore document",
        "an omitted message uses the deterministic default"
    );

    let reordered = json!({
        "revision": "89abcdef0123456789abcdef0123456789abcdef",
        "path": "a.md",
        "candidate_oid": "0123456789abcdef0123456789abcdef01234567",
        "scope_id": "sha1:0123456789abcdef0123456789abcdef01234567",
        "branch_ref": "refs/heads/writing",
        "request_id": "restore-parse-02",
        "schema_version": 1,
    })
    .to_string();
    assert!(RestorePlan::parse(reordered.as_bytes()).is_ok());

    let base = json!({
        "schema_version": 1,
        "request_id": "restore-parse-03",
        "branch_ref": "refs/heads/writing",
        "scope_id": "sha1:0123456789abcdef0123456789abcdef01234567",
        "candidate_oid": "0123456789abcdef0123456789abcdef01234567",
        "path": "a.md",
        "revision": "89abcdef0123456789abcdef0123456789abcdef",
    });
    let refused = |document: Value| {
        let problem =
            RestorePlan::parse(document.to_string().as_bytes()).expect_err("the plan refuses");
        assert_eq!(problem.public_code(), "INVALID_PLAN");
    };
    let mut unknown = base.clone();
    unknown["keep"] = json!([]);
    refused(unknown);
    let mut missing = base.clone();
    missing.as_object_mut().unwrap().remove("path");
    refused(missing);
    let mut bad_revision = base.clone();
    bad_revision["revision"] = json!("HEAD~1");
    refused(bad_revision);
    let mut bad_request = base.clone();
    bad_request["request_id"] = json!("not an id!");
    refused(bad_request);
    let mut wrong_type = base.clone();
    wrong_type["schema_version"] = json!("1");
    refused(wrong_type);
    let mut wrong_version = base.clone();
    wrong_version["schema_version"] = json!(2);
    refused(wrong_version);
    let mut escaped = base.clone();
    escaped["path"] = json!("../outside.md");
    refused(escaped);
    let mut injection = base.clone();
    injection["message"] = json!("line\nMargin-Kind: review");
    refused(injection);
    let mut null_message = base.clone();
    null_message["message"] = Value::Null;
    refused(null_message);

    // A repeated key stays visible, exactly as for a review plan.
    let duplicated = concat!(
        "{\"schema_version\":1,\"request_id\":\"restore-parse-04\",",
        "\"branch_ref\":\"refs/heads/writing\",",
        "\"scope_id\":\"sha1:0123456789abcdef0123456789abcdef01234567\",",
        "\"candidate_oid\":\"0123456789abcdef0123456789abcdef01234567\",",
        "\"path\":\"a.md\",\"path\":\"b.md\",",
        "\"revision\":\"89abcdef0123456789abcdef0123456789abcdef\"}"
    );
    assert!(RestorePlan::parse(duplicated.as_bytes()).is_err());

    // The fingerprint covers every context field, the document, the
    // revision, and the message (SPEC 8.6): changing any of them changes
    // the canonical bytes.
    let identity = plan.identity();
    let mut changed = identity.clone();
    changed.path = "other.md".to_string();
    assert_ne!(
        margin::domain::restore_plan_fingerprint_input_bytes(&identity),
        margin::domain::restore_plan_fingerprint_input_bytes(&changed)
    );
    let mut changed = identity.clone();
    changed.revision = "0123456789abcdef0123456789abcdef01234567".to_string();
    assert_ne!(
        margin::domain::restore_plan_fingerprint_input_bytes(&identity),
        margin::domain::restore_plan_fingerprint_input_bytes(&changed)
    );
    let mut changed = identity.clone();
    changed.message = "explicit".to_string();
    assert_ne!(
        margin::domain::restore_plan_fingerprint_input_bytes(&identity),
        margin::domain::restore_plan_fingerprint_input_bytes(&changed)
    );
    // The operation kind separates a restoration fingerprint from a review
    // fingerprint in the shared domain.
    let mut review = identity.clone();
    review.operation = "review".to_string();
    assert_ne!(
        margin::domain::restore_plan_fingerprint_input_bytes(&identity),
        margin::domain::restore_plan_fingerprint_input_bytes(&review)
    );
}

// ---------------------------------------------------------------------------
// Dry runs and guards
// ---------------------------------------------------------------------------

/// A dry run reports the proved intended result and writes nothing (SPEC 6.4).
#[test]
fn a_dry_run_reports_the_intended_result_and_writes_nothing() {
    let fixture = fixture(RepoOptions::new());
    let status = status_of(&fixture.repo);
    let plan = plan_document(&status, "restore-dry-01", "doc-a.md", &fixture.start, None);
    let before = oracle::snapshot(&fixture.repo);

    let outcome = restore_dry(&fixture.repo, &plan, true);
    assert!(outcome.ok(), "the dry run validates: {:?}", outcome.error);
    let data = outcome.data.unwrap();
    assert!(data.dry_run);
    assert_eq!(data.commit, None);
    assert_eq!(data.existing_receipt, None);
    assert_eq!(data.paths, vec!["doc-a.md".to_string()]);
    // The preview tree holds the source entry at the document, written by
    // the independent mktree oracle.
    let source = oracle::ls_tree(&fixture.repo, &fixture.start)
        .into_iter()
        .find(|source| source.path == "doc-a.md")
        .expect("the source holds the document");
    let mut input = String::new();
    for entry in oracle::ls_tree(&fixture.repo, &fixture.candidate) {
        let record = if entry.path == "doc-a.md" {
            format!("{} {} {}", source.mode, source.kind, source.oid)
        } else {
            format!("{} {} {}", entry.mode, entry.kind, entry.oid)
        };
        input.push_str(&record);
        input.push('\t');
        input.push_str(&entry.path);
        input.push('\n');
    }
    let expected = text(&fixture.repo.git_stdin(["mktree"], input.as_bytes()));
    assert_eq!(data.expected_tree, expected);
    assert_eq!(outcome.effects, Effects::unchanged());
    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "the dry run");
}

/// The reported defect of task T061 at the restoration boundary: an edit
/// behind `skip-worktree` is invisible to Git's change reports, so a
/// restoration used to overwrite content the writer never saw. Both
/// phases refuse the specific index flag before any mutation, and the
/// cleared flag leaves the ordinary clean-scope refusal of SPEC 9.5 over
/// the same bytes (SPEC 3.1, 9.1 step 4).
#[test]
fn an_index_flag_cannot_hide_dirty_content_from_a_restoration() {
    let fixture = fixture(RepoOptions::new());
    let status = status_of(&fixture.repo);
    let plan = plan_document(&status, "restore-01", "doc-a.md", &fixture.start, None);
    // Uncaptured working content Git itself reports as clean.
    fixture
        .repo
        .write_file("doc-a.md", b"hidden working passage\n");
    fixture
        .repo
        .git_ok(["update-index", "--skip-worktree", "--", "doc-a.md"]);
    assert_eq!(
        text(&fixture.repo.git_ok(["status", "--porcelain"])),
        "",
        "stock Git reports no change behind the flag"
    );
    let before = oracle::snapshot(&fixture.repo);

    for (what, dry_run) in [("a dry run", true), ("a restoration", false)] {
        let outcome = restore_dry(&fixture.repo, &plan, dry_run);
        assert_refusal(&outcome, "UNSUPPORTED_REPOSITORY", what);
        let message = outcome.error.as_ref().unwrap().message.clone();
        assert!(
            message.contains("skip-worktree") && message.contains("doc-a.md"),
            "the diagnosis names the flag and the path: {message}"
        );
    }
    // The refusals wrote nothing and hid nothing.
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(fixture.candidate.as_str()),
        "no restoration commit appeared"
    );
    assert_eq!(
        text(
            &fixture
                .repo
                .git_ok(["diff-index", "--cached", "--name-only", "HEAD"])
        ),
        "",
        "the refusals staged nothing"
    );
    assert_eq!(
        fixture.repo.read_file("doc-a.md"),
        b"hidden working passage\n",
        "the hidden edit stays on disk"
    );
    assert_eq!(
        text(&fixture.repo.git_ok(["ls-files", "-v", "--", "doc-a.md"])),
        "S doc-a.md",
        "the user's flag survives both refusals"
    );
    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "a flagged restoration");

    // With the flag cleared, the same dirty content refuses through the
    // ordinary clean-scope guard: the hidden state became visible
    // (SPEC 9.5).
    fixture
        .repo
        .git_ok(["update-index", "--no-skip-worktree", "--", "doc-a.md"]);
    let outcome = restore(&fixture.repo, &plan);
    assert_refusal(&outcome, "DIRTY_SCOPE", "a restoration over visible dirt");
    assert!(
        outcome.error.as_ref().unwrap().message.contains("doc-a.md"),
        "the refusal names the dirty path: {:?}",
        outcome.error
    );
}

/// The guards refuse typed and unchanged (SPEC 9.1, 9.5, 7.5).
#[test]
fn the_guards_refuse_typed_and_unchanged() {
    let fixture = fixture(RepoOptions::new());
    let status = status_of(&fixture.repo);

    // Uncaptured scoped bytes block the restoration (SPEC 9.5).
    let plan = plan_document(
        &status,
        "restore-guard-01",
        "doc-a.md",
        &fixture.start,
        None,
    );
    fixture
        .repo
        .write_file("doc-a.md", b"uncaptured newer bytes\n");
    assert_refusal(
        &restore(&fixture.repo, &plan),
        "DIRTY_SCOPE",
        "uncaptured scoped bytes",
    );
    fixture.repo.write_file("doc-a.md", REVISED.as_bytes());

    // A staged entry blocks it through the shared policy guards.
    fixture.repo.write_file("note.txt", b"staged note\n");
    fixture.repo.git_ok(["add", "--", "note.txt"]);
    let staged = plan_document(
        &status,
        "restore-guard-02",
        "doc-a.md",
        &fixture.start,
        None,
    );
    assert_refusal(
        &restore(&fixture.repo, &staged),
        "STAGED_CHANGES",
        "a staged entry",
    );
    fixture.repo.git_ok(["reset", "--quiet", "--", "note.txt"]);
    fixture
        .repo
        .git_ok(["checkout", "--quiet", "--", "note.txt"]);

    // A branch other than the adopted one refuses before any index write
    // (SPEC 8.2, T059): the plan names the branch the workspace moved to,
    // while the stored adoption still names the writing branch.
    fixture
        .repo
        .git_ok(["switch", "--quiet", "--create", "other"]);
    let foreign_status = status_of(&fixture.repo);
    let foreign = plan_document(
        &foreign_status,
        "restore-guard-03",
        "doc-a.md",
        &fixture.start,
        None,
    );
    assert_refusal(
        &restore(&fixture.repo, &foreign),
        "BRANCH_CHANGED",
        "a branch other than the adopted one",
    );
    fixture.repo.git_ok(["switch", "--quiet", "--", "main"]);
    // A plan that still names the moved-from branch is stale context the
    // same way a review plan is (SPEC 7.4).
    assert_refusal(
        &restore(&fixture.repo, &foreign),
        "STALE_REVIEW",
        "a plan naming another branch",
    );

    // A path outside the writing scope names no restorable document.
    let outside = plan_document(
        &status,
        "restore-guard-04",
        "note.txt",
        &fixture.start,
        None,
    );
    assert_refusal(
        &restore(&fixture.repo, &outside),
        "INVALID_ARGUMENT",
        "a path outside the scope",
    );

    // A source revision outside the adopted history is refused (SPEC 7.4):
    // one parentless commit no first-parent walk of the candidate reaches.
    let tree = text(&fixture.repo.git_ok(["write-tree"]));
    let orphan = text(&fixture.repo.git_ok(["commit-tree", &tree, "-m", "orphan"]));
    let unreachable = plan_document(&status, "restore-guard-05", "doc-a.md", &orphan, None);
    assert_refusal(
        &restore(&fixture.repo, &unreachable),
        "INVALID_ARGUMENT",
        "a source revision outside the adopted history",
    );

    // A revision that resolves to no commit at all is one invalid input.
    let dangling = "f".repeat(40);
    let unresolved = plan_document(&status, "restore-guard-06", "doc-a.md", &dangling, None);
    assert_refusal(
        &restore(&fixture.repo, &unresolved),
        "INVALID_ARGUMENT",
        "a source revision that resolves to no commit",
    );

    // A plan against a moved candidate is stale, never silently reused.
    fixture.repo.write_file("doc-a.md", b"a later pass\n");
    let moved = fixture.repo.commit_all("a later pass");
    assert_ne!(moved, fixture.candidate);
    let stale = plan_document(
        &status,
        "restore-guard-07",
        "doc-a.md",
        &fixture.start,
        None,
    );
    assert_refusal(
        &restore(&fixture.repo, &stale),
        "STALE_REVIEW",
        "a stale candidate context",
    );

    // A wrong scope identity is stale the same way.
    let fresh = status_of(&fixture.repo);
    let mut wrong_scope: Value = serde_json::from_str(&plan_document(
        &fresh,
        "restore-guard-08",
        "doc-a.md",
        &fixture.start,
        None,
    ))
    .unwrap();
    wrong_scope["scope_id"] = json!("sha1:0123456789abcdef0123456789abcdef01234567");
    assert_refusal(
        &restore(&fixture.repo, &wrong_scope.to_string()),
        "STALE_REVIEW",
        "a wrong scope identity",
    );
}

// ---------------------------------------------------------------------------
// Object formats and the executable
// ---------------------------------------------------------------------------

/// The restoration applies under the SHA-256 object format (SPEC 16.5).
#[test]
fn the_restoration_applies_under_sha256() {
    let fixture = fixture(RepoOptions::new().with_object_format(ObjectFormat::Sha256));
    let status = status_of(&fixture.repo);
    assert_eq!(status.object_format.as_str(), "sha256");
    let plan = plan_document(
        &status,
        "restore-sha256-01",
        "doc-a.md",
        &fixture.start,
        None,
    );

    let outcome = restore(&fixture.repo, &plan);
    assert!(
        outcome.ok(),
        "the sha256 restoration applies: {:?}",
        outcome.error
    );
    let data = outcome.data.unwrap();
    let commit = data.commit.expect("the receipt commit");
    assert_eq!(commit.len(), 64, "full sha256 object IDs throughout");
    assert!(data.scope_id.starts_with("sha256:"));
    assert!(data.candidate_scope_digest.starts_with("sha256:"));
    assert_eq!(fixture.repo.read_file("doc-a.md"), ORIGINAL.as_bytes());
    let marker = validate_marker(&adapter_for(&fixture.repo), &commit).unwrap();
    assert_eq!(marker.kind.as_str(), "restore");
}

/// The installed executable restores through one JSON envelope and one text
/// line (SPEC 6.2, 7.1).
#[test]
fn the_executable_restores_through_the_envelope() {
    let fixture = fixture(RepoOptions::new());
    let status = status_of(&fixture.repo);
    let plan = plan_document(&status, "restore-cli-01", "doc-a.md", &fixture.start, None);
    let plan_file = fixture.repo.root().join("restore-plan.json");
    std::fs::write(&plan_file, &plan).expect("the plan file writes");
    let commit_before = oracle::head_oid(&fixture.repo);

    // The dry run writes nothing and reports one envelope.
    let dry = margin(
        fixture.repo.env(),
        fixture.repo.path(),
        &[
            "restore",
            "--input",
            plan_file.to_str().unwrap(),
            "--dry-run",
            "--json",
        ],
    );
    assert_eq!(dry.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&dry.stdout);
    assert_eq!(stdout.matches('\n').count(), 1, "exactly one envelope line");
    assert!(dry.stderr.is_empty());
    let envelope: Value = serde_json::from_str(stdout.trim_end()).unwrap();
    assert_eq!(envelope["command"], "restore");
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["data"]["dry_run"], true);
    assert_eq!(envelope["data"]["path"], "doc-a.md");
    assert_eq!(envelope["data"]["revision_oid"], json!(fixture.start));
    assert_eq!(envelope["effects"]["working_tree"], "unchanged");
    assert_eq!(
        oracle::head_oid(&fixture.repo),
        commit_before,
        "the dry run moves no ref"
    );

    // The application commits and reports the receipt commit.
    let applied = margin(
        fixture.repo.env(),
        fixture.repo.path(),
        &["restore", "--input", plan_file.to_str().unwrap(), "--json"],
    );
    assert_eq!(applied.status.code(), Some(0));
    let envelope: Value =
        serde_json::from_str(String::from_utf8_lossy(&applied.stdout).trim_end()).unwrap();
    assert_eq!(envelope["ok"], true);
    let commit = envelope["data"]["commit"].as_str().unwrap().to_string();
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(commit.as_str())
    );
    assert_eq!(envelope["effects"]["commit"], "created");

    // Text mode writes one bounded line, against the head that moved to.
    let advanced = status_of(&fixture.repo);
    let second = plan_document(
        &advanced,
        "restore-cli-02",
        "doc-b.md",
        &fixture.start,
        None,
    );
    let second_file = fixture.repo.root().join("restore-plan-2.json");
    std::fs::write(&second_file, &second).expect("the second plan writes");
    let text_run = margin(
        fixture.repo.env(),
        fixture.repo.path(),
        &["restore", "--input", second_file.to_str().unwrap()],
    );
    assert_eq!(text_run.status.code(), Some(0));
    let line = String::from_utf8_lossy(&text_run.stdout);
    assert_eq!(line.matches('\n').count(), 1, "exactly one text line");
    assert!(line.starts_with("margin: restore path=doc-b.md "), "{line}");
    assert_eq!(fixture.repo.read_file("doc-b.md"), b"doc-b original\n");
}

/// A plan document that names no readable file is one typed invalid input
/// through the executable (SPEC 6.2).
#[test]
fn the_executable_refuses_an_unreadable_plan() {
    let fixture = fixture(RepoOptions::new());
    let output = margin(
        fixture.repo.env(),
        fixture.repo.path(),
        &["restore", "--input", "/nonexistent/plan.json", "--json"],
    );
    assert_eq!(output.status.code(), Some(2));
    let envelope: Value =
        serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim_end()).unwrap();
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["error"]["code"], "INVALID_ARGUMENT");
}

/// A restoration may resurrect a document the candidate deleted: the file
/// returns with the source bytes, and the freshly resolved scope carries it
/// into the working-content verification (SPEC 9.5, 9.4 step 3).
#[test]
fn a_resurrected_document_returns_to_the_working_tree() {
    let fixture = fixture(RepoOptions::new());
    // Commit the deletion of doc-b.md so the scope no longer lists it.
    fixture.repo.git_ok(["rm", "--quiet", "doc-b.md"]);
    let head = fixture.repo.commit_all("delete the second document");
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(head.as_str())
    );
    assert!(
        !fixture.repo.file_path("doc-b.md").exists(),
        "the document is gone from the working tree"
    );

    let status = status_of(&fixture.repo);
    let plan = plan_document(
        &status,
        "restore-resurrect-01",
        "doc-b.md",
        &fixture.start,
        None,
    );
    let outcome = restore(&fixture.repo, &plan);
    assert!(
        outcome.ok(),
        "the resurrection applies: {:?}",
        outcome.error
    );
    let data = outcome.data.unwrap();
    assert_eq!(data.paths, vec!["doc-b.md".to_string()]);

    // The file is back with the source bytes, the index describes the same
    // entry, and the status is clean.
    assert_eq!(fixture.repo.read_file("doc-b.md"), b"doc-b original\n");
    oracle::assert_index_matches_tree(&fixture.repo, "HEAD");
    assert_eq!(text(&fixture.repo.git_ok(["status", "--porcelain"])), "");
    let entry = oracle::ls_tree(&fixture.repo, data.commit.as_deref().unwrap())
        .into_iter()
        .find(|entry| entry.path == "doc-b.md")
        .expect("the committed tree holds the resurrected document");
    let source = oracle::ls_tree(&fixture.repo, &fixture.start)
        .into_iter()
        .find(|entry| entry.path == "doc-b.md")
        .unwrap();
    assert_eq!(
        (entry.mode.as_str(), entry.oid.as_str()),
        (source.mode.as_str(), source.oid.as_str())
    );
}

/// A document whose name holds spaces and whose bytes hold no valid UTF-8
/// restores exactly: the literal path survives every protocol, and the
/// binary payload of the fixed diff contract applies (SPEC 3.3, 10.1).
#[test]
fn unusual_paths_and_opaque_bytes_restore_exactly() {
    let repo = TestRepo::init();
    let document = "drafts/My Draft \"v2\".md";
    let original: &[u8] = b"heading\n\xff\xfe opaque tail\n\x00 middle\n";
    repo.write_file(document, original);
    // One Markdown sibling keeps the scope nonempty besides the document.
    repo.write_file("doc-a.md", ORIGINAL.as_bytes());
    let source = repo.commit_all("source with the opaque document");
    repo.write_file(document, b"heading\nrevised plain tail\n");
    let candidate = repo.commit_all("revise the document");
    assert_ne!(source, candidate);
    initialize(&repo);

    let status = status_of(&repo);
    let plan = plan_document(&status, "restore-opaque-01", document, &source, None);
    let outcome = restore(&repo, &plan);
    assert!(
        outcome.ok(),
        "the opaque restoration applies: {:?}",
        outcome.error
    );
    let data = outcome.data.unwrap();
    assert_eq!(data.path, document);
    assert_eq!(data.paths, vec![document.to_string()]);

    // The exact raw bytes return, and nothing stays dirty (SPEC 8.8).
    assert_eq!(repo.read_file(document), original);
    assert_eq!(text(&repo.git_ok(["status", "--porcelain"])), "");
    oracle::assert_index_matches_tree(&repo, "HEAD");
    let marker = validate_marker(
        &adapter_for(&repo),
        data.commit.as_deref().expect("the receipt commit"),
    )
    .unwrap();
    assert_eq!(
        marker.source,
        Some(RestoreSource {
            path: document.to_string(),
            revision: source,
        })
    );
}
