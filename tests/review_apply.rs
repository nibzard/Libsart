//! Real-Git tests for review application (SPEC 9.4, tasks T031 and T032).
//!
//! Every case proves one contract of the application against the installed
//! Git:
//!
//! - one complete keep-all plan creates exactly one verified empty review
//!   commit with `--allow-empty`: the parent is C, the tree is the
//!   candidate's full tree, no working file is rewritten, and the reserved
//!   trailers name the request, the plan fingerprint, B, and T
//! - one restoring plan applies the selected complete reverse patches
//!   through the guarded Git index path: the committed result equals the
//!   dry-run preview tree, the restored passages return to B's bytes, the
//!   kept passages and out-of-scope entries keep C's, and the baseline
//!   advances only over the verified commit
//! - the deterministic boundary seam proves the failure phases of SPEC 9.6:
//!   an apply failure retains the partial state it wrote, a successful
//!   apply followed by a commit rejection retains the staged result, and
//!   working bytes that change after the apply refuse before the commit —
//!   each reports the observed state with recovery guidance and never
//!   claims a rollback
//! - a hook that succeeds but edits scoped files leaves the verified
//!   commit standing while the response reports the newer content, and a
//!   hook that rewrites the reserved trailers leaves an ordinary commit
//!   reported as a contract violation with the review still pending
//! - an exact retry returns the original verified receipt without a second
//!   commit — even when the scope root became a symlink no new mutation
//!   could resolve — and a reused request ID under another fingerprint
//!   conflicts
//! - the guards refuse typed and unchanged: uncaptured scoped bytes, a
//!   staged entry, a foreign branch, and a moved candidate
//! - the precondition recheck of SPEC 9.1 step 6 refuses before the commit
//!   when a concurrent writer moves `HEAD`, the index, or the scoped bytes
//!   while the application is prepared, and the shared mutation lock
//!   refuses `BUSY` and releases on every return path
//! - the installed executable reports one envelope in JSON mode and one
//!   line in text mode, and both object formats apply
//!
//! The oracles stay independent of the implementation (SPEC 16.3): commits,
//! parents, and trees come from `git rev-parse` and `git log`, the marker
//! from the shared validator, and the repository state from
//! `oracle::snapshot`, `git write-tree`, `git mktree`, and `git status`.

mod support;

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::{Command, Output, Stdio};

use margin::app::{
    BaselineStatus, ChangeDetail, CommitEffect, DiffRequest, Effects, Identities, InitRequest,
    InspectRequest, ObservedEffect, OperationOutcome, Operations, RepositoryEffect, ReviewDecision,
    ReviewOutcome, ReviewRequest, validate_marker,
};
use margin::domain::{ReviewPlan, ReviewState, ScopeInput};
use margin::git::Git;
use serde_json::{Value, json};
use support::env::{IsolatedEnv, is_scrubbed};
use support::mutation::{SeamPass, hold_mutation_lock, seam_adapter};
use support::repo::{ObjectFormat, RepoOptions, TestRepo};
use support::{oracle, text};

// ---------------------------------------------------------------------------
// The shared fixture
// ---------------------------------------------------------------------------

/// One document with two independently reviewable passages, each inside its
/// own gap paragraph.
fn document(first: &str, second: &str) -> String {
    let mut out = String::from("# Notes\n\n");
    for line in 1..=6 {
        out.push_str(&format!("gap upper {line}\n"));
    }
    out.push_str(&format!("\n{first}\n"));
    for line in 1..=6 {
        out.push_str(&format!("gap lower {line}\n"));
    }
    out.push_str(&format!("\n{second}\n"));
    out
}

/// The seeded workspace: a start marker B and a committed candidate C, with
/// one out-of-scope note that changed between them, so no review may trade
/// the note's candidate entry for the baseline's absence.
struct Fixture {
    repo: TestRepo,
    /// The start marker the first-parent walk anchors at: B.
    start: String,
    /// The committed candidate: C.
    candidate: String,
}

fn fixture(options: RepoOptions) -> Fixture {
    let repo = TestRepo::init_with(options);
    repo.write_file(
        "doc-a.md",
        document("first original passage", "second original passage").as_bytes(),
    );
    repo.write_file("note.txt", b"the baseline note\n");
    let start = initialize(&repo);
    repo.write_file(
        "doc-a.md",
        document(
            "first revised passage\nwith two lines",
            "second revised passage",
        )
        .as_bytes(),
    );
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

/// The pinned comparison of the fixture, with the identity fields a plan
/// pins to.
fn pinned_review_of(repo: &TestRepo, start: &str) -> ReviewOutcome {
    let git = adapter_for(repo);
    let operations = Operations::new(&git);
    let outcome = operations.build_review(&DiffRequest {
        scope: None,
        path_filter: Vec::new(),
        change: None,
        cursor: None,
        limit: None,
        working: false,
    });
    assert!(
        outcome.ok(),
        "the pinned review builds: {:?}",
        outcome.error
    );
    let data = outcome.data.expect("a successful diff carries data");
    assert_eq!(data.baseline_oid, start, "B is the start marker");
    data
}

/// One complete plan document for `pinned`: keep-all when `restore` is
/// empty, one explicit message when given.
fn plan_document(
    pinned: &ReviewOutcome,
    request: &str,
    restore: &[String],
    message: Option<&str>,
) -> String {
    let mut document = json!({
        "schema_version": 1,
        "request_id": request,
        "review_id": pinned.review_id,
        "branch_ref": pinned.branch_ref,
        "scope_id": pinned.scope_id,
        "baseline_oid": pinned.baseline_oid,
        "candidate_oid": pinned.candidate_oid,
        "restore": restore,
        "keep_remaining": true,
    });
    if let Some(message) = message {
        document["message"] = json!(message);
    }
    document.to_string()
}

/// Applies one plan document through the operation layer.
fn apply(repo: &TestRepo, plan: &str) -> OperationOutcome<ReviewDecision> {
    apply_dry(repo, plan, false)
}

/// Applies one plan document through the operation layer, with the dry-run
/// flag of the request.
fn apply_dry(repo: &TestRepo, plan: &str, dry_run: bool) -> OperationOutcome<ReviewDecision> {
    let git = adapter_for(repo);
    let operations = Operations::new(&git);
    operations.apply_review(&ReviewRequest {
        scope: None,
        plan: plan.as_bytes().to_vec(),
        dry_run,
    })
}

/// The plan fingerprint of one plan document, through the shared identities
/// (SPEC 7.3, 8.6).
fn fingerprint_of(repo: &TestRepo, plan: &str) -> String {
    let parsed = ReviewPlan::parse(plan.as_bytes())
        .unwrap_or_else(|problem| panic!("the plan must parse: {problem}"));
    Identities::new(&adapter_for(repo))
        .unwrap_or_else(|error| panic!("identities must read the repository: {error}"))
        .plan(&parsed.identity())
        .unwrap_or_else(|error| panic!("the plan must hash: {error}"))
        .id()
}

/// Asserts one typed refusal: not successful, one primary code, and the
/// unchanged effects of an operation that wrote nothing.
fn assert_refusal(outcome: &OperationOutcome<ReviewDecision>, code: &str, what: &str) {
    assert!(!outcome.ok(), "{what} must not succeed");
    let error = outcome.error.as_ref().unwrap();
    assert_eq!(error.code, code, "{what} reports the wrong code");
    assert_eq!(
        outcome.effects,
        Effects::unchanged(),
        "{what} wrote nothing"
    );
}

/// The review and baseline state of the workspace, read through the one
/// query both interfaces share.
fn workspace_state(repo: &TestRepo) -> (ReviewState, BaselineStatus) {
    let git = adapter_for(repo);
    let operations = Operations::new(&git);
    let status = operations.inspect_workspace(&InspectRequest {
        scope: None,
        path_filter: Vec::new(),
    });
    assert!(status.ok(), "the status query succeeds: {:?}", status.error);
    let data = status.data.unwrap();
    (data.review, data.baseline)
}

/// The change identity of one passage of `doc-a.md`: `passage` 0 is the
/// first in source order, found through the recorded source ranges.
fn group_of(pinned: &ReviewOutcome, passage: usize) -> String {
    let mut groups: Vec<&ChangeDetail> = pinned
        .changes
        .iter()
        .filter(|change| change.path == "doc-a.md")
        .collect();
    assert_eq!(groups.len(), 2, "the document holds two passages");
    groups.sort_by_key(|change| change.ranges[0].old_start);
    groups[passage].change_id.clone()
}

/// Hashes expected bytes through stock Git and returns their blob OID.
fn blob_of(repo: &TestRepo, bytes: &[u8]) -> String {
    text(&repo.git_stdin(["hash-object", "-w", "--no-filters", "--stdin"], bytes))
}

/// The independent expected-tree oracle (SPEC 16.3): the candidate's
/// entries with `doc-a.md` replaced by `bytes`, written through
/// `git mktree`.
fn expected_tree_with_document(repo: &TestRepo, candidate: &str, bytes: &[u8]) -> String {
    let mut input = String::new();
    for entry in oracle::ls_tree(repo, candidate) {
        let record = if entry.path == "doc-a.md" {
            format!("100644 blob {}", blob_of(repo, bytes))
        } else {
            format!("{} {} {}", entry.mode, entry.kind, entry.oid)
        };
        input.push_str(&record);
        input.push('\t');
        input.push_str(&entry.path);
        input.push('\n');
    }
    text(&repo.git_stdin(["mktree"], input.as_bytes()))
}

// ---------------------------------------------------------------------------
// The verified empty review commit
// ---------------------------------------------------------------------------

/// One complete keep-all plan creates exactly one verified empty review
/// commit: the candidate's full tree, no file rewrite, the reserved
/// receipt trailers, and the advanced baseline (SPEC 9.4, 8.1).
#[test]
fn keep_all_creates_one_verified_empty_review_commit() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    assert_eq!(pinned.scope_totals.groups, 2, "two passages are pending");
    let plan = plan_document(
        &pinned,
        "review-keep-all-01",
        &[],
        Some("keep every change"),
    );
    let before = oracle::snapshot(&fixture.repo);
    let commits_before =
        text(
            &fixture
                .repo
                .git_ok(["rev-list", "--count", "--first-parent", "HEAD"]),
        );

    let outcome = apply(&fixture.repo, &plan);
    assert!(outcome.ok(), "keep-all applies: {:?}", outcome.error);
    let data = outcome.data.unwrap();
    assert!(!data.dry_run);
    assert_eq!(data.restored, Vec::<String>::new());
    assert_eq!(data.kept, Some(2), "both passages stay");
    assert_eq!(data.paths, Vec::<String>::new(), "nothing is rewritten");
    assert_eq!(data.existing_receipt, None);

    // Exactly one commit joined the history, and the response names it.
    let commit = data.commit.clone().expect("the application commits");
    assert_eq!(
        text(
            &fixture
                .repo
                .git_ok(["rev-list", "--count", "--first-parent", "HEAD"])
        ),
        (commits_before.trim().parse::<usize>().unwrap() + 1).to_string(),
        "one review commit joined"
    );
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(commit.as_str())
    );
    assert_eq!(outcome.effects.commit, CommitEffect::Created);
    assert_eq!(outcome.effects.commit_oid.as_deref(), Some(commit.as_str()));
    assert_eq!(outcome.effects.working_tree, ObservedEffect::Unchanged);
    assert_eq!(outcome.effects.index, ObservedEffect::Unchanged);
    assert_eq!(outcome.effects.repository, RepositoryEffect::Unchanged);
    assert_eq!(outcome.effects.configuration, ObservedEffect::Unchanged);

    // The empty commit: one parent, the candidate, and the candidate's full
    // tree (SPEC 8.1, 9.4).
    let parents = text(
        &fixture
            .repo
            .git_ok(["log", "-n", "1", "--format=%P", &commit]),
    );
    assert_eq!(
        parents.trim(),
        fixture.candidate,
        "one parent: the candidate"
    );
    let tree = oracle::commit_tree(&fixture.repo, &commit);
    assert_eq!(
        tree,
        oracle::commit_tree(&fixture.repo, &fixture.candidate),
        "the review commit holds the candidate's full tree"
    );
    assert_eq!(
        data.expected_tree, tree,
        "the intended tree is the commit's tree"
    );
    assert_eq!(
        data.result_scope_digest, pinned.candidate_scope_digest,
        "the scoped result is the candidate's scoped content"
    );
    oracle::assert_out_of_scope_tree_unchanged(
        &fixture.repo,
        &fixture.candidate,
        &commit,
        &["doc-a.md"],
        "the keep-all review",
    );

    // The reserved trailers the marker validator proves (SPEC 8.3-8.4).
    let marker = validate_marker(&adapter_for(&fixture.repo), &commit)
        .unwrap_or_else(|error| panic!("the review commit is a valid marker: {error}"));
    assert_eq!(marker.kind.as_str(), "review");
    assert_eq!(marker.candidate, fixture.candidate);
    assert_eq!(marker.result_tree, tree);
    assert_eq!(marker.baseline.as_deref(), Some(fixture.start.as_str()));
    assert_eq!(marker.request.as_deref(), Some("review-keep-all-01"));
    assert_eq!(
        marker.plan.as_deref(),
        Some(fingerprint_of(&fixture.repo, &plan).as_str())
    );
    let message = text(
        &fixture
            .repo
            .git_ok(["log", "-n", "1", "--format=%B", &commit]),
    );
    assert!(
        message.starts_with("keep every change\n\n"),
        "the explicit message leads the commit: {message:?}"
    );
    assert!(message.contains("Margin-Review: complete\n"));

    // No file rewrite: the working bytes, the index, and the status are
    // exactly the candidate's (SPEC 9.4).
    let after = oracle::snapshot(&fixture.repo);
    assert!(
        before.working_diff(&after).is_empty(),
        "no working file changed"
    );
    oracle::assert_index_matches_tree(&fixture.repo, "HEAD");
    assert_eq!(text(&fixture.repo.git_ok(["status", "--porcelain"])), "");

    // The displayed baseline advanced to the verified review (SPEC 4.2).
    let (review, baseline) = workspace_state(&fixture.repo);
    assert_eq!(review, ReviewState::Reviewed);
    match baseline {
        BaselineStatus::Anchored(marker) => {
            assert_eq!(marker.commit, commit);
            assert_eq!(marker.kind.as_str(), "review");
        }
        other => panic!("the review anchors the scope: {other:?}"),
    }
}

/// A review with nothing pending still completes: B equals C, so the empty
/// review commit records the approval of the anchored tree itself (SPEC
/// 8.4 rule 6, 9.4).
#[test]
fn a_review_with_nothing_pending_still_completes() {
    let fixture = fixture(RepoOptions::new());
    // Discard the writing pass: HEAD becomes the start marker again, and B
    // equals C.
    fixture
        .repo
        .git_ok(["reset", "--quiet", "--hard", &fixture.start]);
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    assert_eq!(pinned.scope_totals.groups, 0, "nothing is pending");
    assert_eq!(pinned.candidate_oid, fixture.start);
    let plan = plan_document(&pinned, "review-empty-01", &[], None);

    let outcome = apply(&fixture.repo, &plan);
    assert!(outcome.ok(), "an empty review applies: {:?}", outcome.error);
    let data = outcome.data.unwrap();
    let commit = data.commit.expect("the application commits");
    assert_eq!(data.kept, Some(0));
    assert_eq!(
        oracle::commit_tree(&fixture.repo, &commit),
        oracle::commit_tree(&fixture.repo, &fixture.start),
        "the anchored tree itself is the result"
    );
    let marker = validate_marker(&adapter_for(&fixture.repo), &commit).unwrap();
    assert_eq!(
        marker.baseline.as_deref(),
        Some(fixture.start.as_str()),
        "B equals C is a valid review baseline"
    );

    let (review, baseline) = workspace_state(&fixture.repo);
    assert_eq!(review, ReviewState::Reviewed);
    match baseline {
        BaselineStatus::Anchored(marker) => assert_eq!(marker.commit, commit),
        other => panic!("the review anchors the scope: {other:?}"),
    }
}

/// The next writing pass after a review compares against the verified
/// review, so the approval keeps its meaning (SPEC 8.5, 4.2).
#[test]
fn the_next_writing_pass_compares_against_the_review() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-keep-all-01", &[], None);
    let outcome = apply(&fixture.repo, &plan);
    assert!(outcome.ok(), "{:?}", outcome.error);
    let review = outcome.data.unwrap().commit.expect("the review commit");

    fixture.repo.write_file(
        "doc-a.md",
        document("a newer passage", "a newer second").as_bytes(),
    );
    let later = fixture.repo.commit_all("another writing pass");

    let (review_state, baseline) = workspace_state(&fixture.repo);
    assert_eq!(
        review_state,
        ReviewState::Pending,
        "new edits reopen review"
    );
    match baseline {
        BaselineStatus::Anchored(marker) => {
            assert_eq!(marker.commit, review, "the review stays the baseline");
            assert_eq!(marker.kind.as_str(), "review");
        }
        other => panic!("the review still anchors the scope: {other:?}"),
    }
    assert_ne!(later, review);
}

// ---------------------------------------------------------------------------
// Receipts and conflicts
// ---------------------------------------------------------------------------

/// An exact retry returns the original verified receipt without creating a
/// second commit (SPEC 8.7).
#[test]
fn an_exact_retry_returns_the_receipt_without_a_second_commit() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-keep-all-01", &[], None);
    let first = apply(&fixture.repo, &plan);
    assert!(first.ok(), "{:?}", first.error);
    let review = first.data.unwrap().commit.expect("the first application");
    let commits = text(
        &fixture
            .repo
            .git_ok(["rev-list", "--count", "--first-parent", "HEAD"]),
    );

    let retry = apply(&fixture.repo, &plan);
    assert!(retry.ok(), "the exact retry succeeds: {:?}", retry.error);
    let data = retry.data.unwrap();
    assert_eq!(
        data.existing_receipt.as_deref(),
        Some(review.as_str()),
        "the retry returns the original verified commit"
    );
    assert_eq!(data.commit, None, "no second review commit");
    assert!(!data.dry_run);
    assert_eq!(
        data.expected_tree,
        oracle::commit_tree(&fixture.repo, &review),
        "the decision data comes from the receipt's own comparison"
    );
    assert_eq!(retry.effects.commit, CommitEffect::ExistingReceipt);
    assert_eq!(retry.effects.commit_oid.as_deref(), Some(review.as_str()));
    assert_eq!(
        text(
            &fixture
                .repo
                .git_ok(["rev-list", "--count", "--first-parent", "HEAD"])
        ),
        commits,
        "no commit joined the history"
    );
}

/// A request ID reused under another fingerprint conflicts, and the reuse
/// writes nothing (SPEC 7.3, 8.7).
#[test]
fn a_reused_request_id_with_another_plan_conflicts() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let original = plan_document(&pinned, "review-decision-42", &[], None);
    assert!(apply(&fixture.repo, &original).ok(), "the original applies");
    let review = oracle::head_oid(&fixture.repo).unwrap();
    let before = oracle::snapshot(&fixture.repo);

    let reworded = plan_document(&pinned, "review-decision-42", &[], Some("another intent"));
    let outcome = apply(&fixture.repo, &reworded);
    assert_refusal(&outcome, "REQUEST_ID_CONFLICT", "a reworded plan");
    let error = outcome.error.unwrap();
    assert!(
        error.message.contains(&review),
        "the refusal names the receipt: {}",
        error.message
    );
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(review.as_str())
    );
    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "a refused conflict");
}

/// A library caller that sets the request's dry-run flag reaches the
/// validation, never a commit (SPEC 6.4).
#[test]
fn a_dry_run_request_reports_and_writes_nothing() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-keep-all-01", &[], None);
    let before = oracle::snapshot(&fixture.repo);

    let outcome = apply_dry(&fixture.repo, &plan, true);
    assert!(outcome.ok(), "the dry run validates: {:?}", outcome.error);
    let data = outcome.data.unwrap();
    assert!(data.dry_run);
    assert_eq!(data.commit, None);
    assert_eq!(outcome.effects, Effects::unchanged());
    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "a dry-run request");
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(fixture.candidate.as_str()),
        "no review commit appeared"
    );
    let (review, _) = workspace_state(&fixture.repo);
    assert_eq!(review, ReviewState::Pending, "nothing was approved");
}

/// The reported defect of task T060: complete one keep-all review, replace
/// the scope root with a symlink, and repeat the identical review command.
/// The retry returns the original verified receipt — the response comes
/// from the recorded canonical scope and immutable Git objects, so the
/// present-day root is never resolved and the symlink is never followed —
/// while a fresh request under the same root cannot even adopt the scope
/// (SPEC 8.7, 9.1 step 3).
#[test]
fn an_exact_retry_survives_a_scope_root_that_becomes_a_symlink() {
    // One sub-directory scope, so the retry can replace the one root the
    // descriptor records.
    let repo = TestRepo::init();
    repo.write_file("drafts/doc.md", b"# Draft\n\nfirst passage\n");
    repo.write_file("note.txt", b"out of scope\n");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let init = operations.initialize_scope(&InitRequest {
        scope: Some(ScopeInput {
            include: vec!["drafts".to_string()],
            exclude: Vec::new(),
            suffixes: vec![".md".to_string()],
        }),
        create_repository: false,
        branch: None,
        new_start: false,
        auto_checkpoint: None,
        capture_after_edit: None,
        editor_argv: None,
        dry_run: false,
    });
    assert!(init.ok(), "the scoped init succeeds: {:?}", init.error);
    let start = init
        .data
        .and_then(|data| data.start_commit)
        .expect("first use creates one start commit");
    repo.write_file("drafts/doc.md", b"# Draft\n\nrevised passage\n");
    repo.commit_all("writing pass");
    let pinned = pinned_review_of(&repo, &start);
    let plan = plan_document(&pinned, "review-keep-all-01", &[], None);

    let first = apply(&repo, &plan);
    assert!(first.ok(), "the review applies: {:?}", first.error);
    let review = first.data.unwrap().commit.expect("the receipt commit");
    let commits = text(&repo.git_ok(["rev-list", "--count", "--first-parent", "HEAD"]));

    // The root becomes a symlink: a new mutation can no longer resolve the
    // scope at all.
    let root = repo.file_path("drafts");
    fs::remove_dir_all(&root).expect("the root directory leaves");
    symlink("nowhere", &root).expect("the scope root becomes a symlink");
    let fresh = plan_document(&pinned, "review-fresh-02", &[], None);
    assert_refusal(
        &apply(&repo, &fresh),
        "INVALID_ARGUMENT",
        "a fresh request under the symlinked root",
    );

    // The identical retry still returns the original receipt, with no
    // second commit and no write of any kind.
    let before = oracle::snapshot(&repo);
    let retry = apply(&repo, &plan);
    assert!(
        retry.ok(),
        "the retry acknowledges the receipt: {:?}",
        retry.error
    );
    let data = retry.data.unwrap();
    assert_eq!(data.existing_receipt.as_deref(), Some(review.as_str()));
    assert_eq!(data.commit, None, "no second review commit");
    assert_eq!(data.kept, None, "the historical group count stays unknown");
    assert_eq!(data.paths, Vec::<String>::new(), "keep-all rewrote nothing");
    assert_eq!(retry.effects.commit, CommitEffect::ExistingReceipt);
    assert_eq!(retry.effects.commit_oid.as_deref(), Some(review.as_str()));
    assert_eq!(
        text(&repo.git_ok(["rev-list", "--count", "--first-parent", "HEAD"])),
        commits,
        "no commit joined the history"
    );
    before.assert_unchanged(&oracle::snapshot(&repo), "the receipt retry");

    // The dry run of the same request behaves the same way (SPEC 6.4).
    let dry = apply_dry(&repo, &plan, true);
    assert!(
        dry.ok(),
        "the dry run acknowledges the receipt: {:?}",
        dry.error
    );
    assert_eq!(
        dry.data.unwrap().existing_receipt.as_deref(),
        Some(review.as_str())
    );
    before.assert_unchanged(&oracle::snapshot(&repo), "the dry-run retry");
}

// ---------------------------------------------------------------------------
// The restoring application (SPEC 9.4, task T032)
// ---------------------------------------------------------------------------

/// One restoring plan applies the selected complete reverse patch through
/// the guarded Git index path and commits exactly the preview: the restored
/// passage returns to B's bytes, the kept passage and the out-of-scope note
/// keep C's, the intended tree equals the independent `mktree` oracle and
/// the dry run's preview, and the baseline advances to the verified review
/// (SPEC 9.4, 9.3).
#[test]
fn a_restoring_plan_commits_the_exact_preview_result() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    assert_eq!(pinned.scope_totals.groups, 2, "two passages are pending");
    let first = group_of(&pinned, 0);
    let second = group_of(&pinned, 1);
    let plan = plan_document(
        &pinned,
        "review-restore-01",
        std::slice::from_ref(&first),
        Some("restore the first passage"),
    );

    // The dry run previews the exact result the application must commit.
    let preview = apply_dry(&fixture.repo, &plan, true);
    assert!(preview.ok(), "the dry run validates: {:?}", preview.error);
    let preview_data = preview.data.unwrap();
    assert_eq!(preview_data.restored, vec![first.clone()]);

    let commits_before =
        text(
            &fixture
                .repo
                .git_ok(["rev-list", "--count", "--first-parent", "HEAD"]),
        );
    let outcome = apply(&fixture.repo, &plan);
    assert!(
        outcome.ok(),
        "the restoring application succeeds: {:?}",
        outcome.error
    );
    let data = outcome.data.unwrap();
    assert!(!data.dry_run);
    assert_eq!(
        data.restored,
        vec![first.clone()],
        "the passage is restored"
    );
    assert_eq!(data.kept, Some(1), "the other passage stays");
    assert_eq!(
        data.paths,
        vec!["doc-a.md".to_string()],
        "one path rewritten"
    );
    assert_eq!(data.existing_receipt, None);
    assert_eq!(
        data.expected_tree, preview_data.expected_tree,
        "the committed result matches the preview"
    );
    assert_eq!(
        data.result_scope_digest, preview_data.result_scope_digest,
        "the scoped result matches the preview"
    );

    // Exactly one commit joined, the response names it, and the effects
    // report the rewrite the application performed.
    let commit = data.commit.clone().expect("the application commits");
    assert_eq!(
        text(
            &fixture
                .repo
                .git_ok(["rev-list", "--count", "--first-parent", "HEAD"])
        ),
        (commits_before.trim().parse::<usize>().unwrap() + 1).to_string(),
        "one review commit joined"
    );
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(commit.as_str())
    );
    assert_eq!(outcome.effects.commit, CommitEffect::Created);
    assert_eq!(outcome.effects.commit_oid.as_deref(), Some(commit.as_str()));
    assert_eq!(outcome.effects.working_tree, ObservedEffect::Changed);
    assert_eq!(outcome.effects.index, ObservedEffect::Changed);
    assert_eq!(outcome.effects.repository, RepositoryEffect::Unchanged);
    assert_eq!(outcome.effects.configuration, ObservedEffect::Unchanged);
    assert!(outcome.warnings.is_empty(), "a clean result warns nothing");

    // The committed tree equals the independent oracle: the candidate's
    // entries with the restored document (SPEC 16.3).
    let expected_bytes = document("first original passage", "second revised passage");
    assert_eq!(
        data.expected_tree,
        expected_tree_with_document(&fixture.repo, &fixture.candidate, expected_bytes.as_bytes()),
        "the intended tree equals the mktree oracle"
    );
    assert_eq!(
        oracle::commit_tree(&fixture.repo, &commit),
        data.expected_tree,
        "the commit holds the preview tree"
    );
    let parents = text(
        &fixture
            .repo
            .git_ok(["log", "-n", "1", "--format=%P", &commit]),
    );
    assert_eq!(
        parents.trim(),
        fixture.candidate,
        "one parent: the candidate"
    );
    oracle::assert_out_of_scope_tree_unchanged(
        &fixture.repo,
        &fixture.candidate,
        &commit,
        &["doc-a.md"],
        "the restoring review",
    );

    // The working bytes: the restored passage is B's, the kept passage and
    // the note are C's (SPEC 9.4 step 3).
    assert_eq!(
        fixture.repo.read_file("doc-a.md"),
        expected_bytes.into_bytes(),
        "the document holds the exact reviewed result"
    );
    assert_eq!(
        fixture.repo.read_file("note.txt"),
        b"the candidate note\n",
        "the out-of-scope note keeps the candidate bytes"
    );
    oracle::assert_index_matches_tree(&fixture.repo, "HEAD");
    assert_eq!(text(&fixture.repo.git_ok(["status", "--porcelain"])), "");

    // The reserved trailers the marker validator proves (SPEC 8.3-8.4).
    let marker = validate_marker(&adapter_for(&fixture.repo), &commit)
        .unwrap_or_else(|error| panic!("the review commit is a valid marker: {error}"));
    assert_eq!(marker.kind.as_str(), "review");
    assert_eq!(marker.candidate, fixture.candidate);
    assert_eq!(marker.result_tree, data.expected_tree);
    assert_eq!(marker.baseline.as_deref(), Some(fixture.start.as_str()));
    assert_eq!(marker.request.as_deref(), Some("review-restore-01"));
    assert_eq!(
        marker.plan.as_deref(),
        Some(fingerprint_of(&fixture.repo, &plan).as_str())
    );
    let message = text(
        &fixture
            .repo
            .git_ok(["log", "-n", "1", "--format=%B", &commit]),
    );
    assert!(
        message.starts_with("restore the first passage\n\n"),
        "the explicit message leads the commit: {message:?}"
    );

    // The displayed baseline advanced to the verified review (SPEC 4.2).
    let (review, baseline) = workspace_state(&fixture.repo);
    assert_eq!(review, ReviewState::Reviewed);
    match baseline {
        BaselineStatus::Anchored(marker) => assert_eq!(marker.commit, commit),
        other => panic!("the review anchors the scope: {other:?}"),
    }
    // The unused identity of the kept passage belongs to this comparison
    // alone; the restored identity is what the response names.
    assert_ne!(first, second);
}

/// Restoring every passage of one document recomposes the baseline side of
/// that document while the out-of-scope entries keep the candidate's, so
/// the multi-unit sequence applies in the proof's order and still commits
/// one verified review (SPEC 9.4 step 2).
#[test]
fn restoring_every_passage_recomposes_the_baseline_side() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let both = [group_of(&pinned, 0), group_of(&pinned, 1)];
    let plan = plan_document(&pinned, "review-restore-all-01", &both, None);

    let outcome = apply(&fixture.repo, &plan);
    assert!(outcome.ok(), "restore-all applies: {:?}", outcome.error);
    let data = outcome.data.unwrap();
    assert_eq!(data.restored, both, "manifest order");
    assert_eq!(data.kept, Some(0));
    let commit = data.commit.expect("the application commits");

    let baseline_bytes = document("first original passage", "second original passage");
    assert_eq!(
        data.expected_tree,
        expected_tree_with_document(&fixture.repo, &fixture.candidate, baseline_bytes.as_bytes()),
        "the intended tree equals the mktree oracle"
    );
    assert_eq!(
        oracle::commit_tree(&fixture.repo, &commit),
        data.expected_tree
    );
    assert_eq!(
        fixture.repo.read_file("doc-a.md"),
        baseline_bytes.into_bytes(),
        "the document returns to the baseline side"
    );
    assert_eq!(
        fixture.repo.read_file("note.txt"),
        b"the candidate note\n",
        "the note never trades its candidate entry for the baseline's"
    );
    oracle::assert_index_matches_tree(&fixture.repo, "HEAD");
    assert_eq!(text(&fixture.repo.git_ok(["status", "--porcelain"])), "");

    let (review, baseline) = workspace_state(&fixture.repo);
    assert_eq!(review, ReviewState::Reviewed);
    match baseline {
        BaselineStatus::Anchored(marker) => assert_eq!(marker.commit, commit),
        other => panic!("the review anchors the scope: {other:?}"),
    }
}

/// Restoring the presence transitions applies through the same guarded
/// path: an added file is removed from the index and the working tree, a
/// deleted file returns to its baseline bytes, and the kept passage and
/// the out-of-scope note keep the candidate's entries (SPEC 9.4, 10.3).
#[test]
fn restoring_presence_transitions_removes_and_recreates_files() {
    let repo = TestRepo::init();
    repo.write_file("kept.md", b"# Kept\n\noriginal passage\n");
    repo.write_file("gone.md", b"# Gone\n\noriginal passage\n");
    repo.write_file("note.txt", b"the baseline note\n");
    let start = initialize(&repo);
    repo.write_file("kept.md", b"# Kept\n\nrevised passage\n");
    fs::remove_file(repo.file_path("gone.md")).unwrap();
    repo.write_file("born.md", b"# Born\n\na new passage\n");
    repo.write_file("note.txt", b"the candidate note\n");
    let candidate = repo.commit_all("writing pass");

    let pinned = pinned_review_of(&repo, &start);
    assert_eq!(
        pinned.scope_totals.groups, 3,
        "one modified passage, one deletion, one addition"
    );
    let change_of = |path: &str| {
        pinned
            .changes
            .iter()
            .find(|change| change.path == path)
            .unwrap_or_else(|| panic!("the comparison details {path}"))
            .change_id
            .clone()
    };
    let born = change_of("born.md");
    let gone = change_of("gone.md");
    let plan = plan_document(
        &pinned,
        "review-presence-01",
        &[born.clone(), gone.clone()],
        None,
    );

    let outcome = apply(&repo, &plan);
    assert!(
        outcome.ok(),
        "the restoring application succeeds: {:?}",
        outcome.error
    );
    let data = outcome.data.unwrap();
    assert_eq!(
        data.restored,
        vec![born.clone(), gone.clone()],
        "the whole-file decisions report in manifest order"
    );
    assert_eq!(data.kept, Some(1), "the modified passage stays");
    let commit = data.commit.clone().expect("the application commits");

    // The independent tree oracle (SPEC 16.3): the candidate's entries
    // with `born.md` absent and `gone.md` back to its baseline blob. The
    // deletion holds no candidate entry, so its restored record joins the
    // list explicitly and `mktree` receives the entries sorted by path.
    let mut records: Vec<(String, String)> = oracle::ls_tree(&repo, &candidate)
        .into_iter()
        .filter(|entry| entry.path != "born.md")
        .map(|entry| {
            (
                entry.path,
                format!("{} {} {}", entry.mode, entry.kind, entry.oid),
            )
        })
        .collect();
    records.push((
        "gone.md".to_string(),
        format!(
            "100644 blob {}",
            blob_of(&repo, b"# Gone\n\noriginal passage\n")
        ),
    ));
    records.sort_by(|(one, _), (other, _)| one.cmp(other));
    let mut input = String::new();
    for (path, record) in records {
        input.push_str(&record);
        input.push('\t');
        input.push_str(&path);
        input.push('\n');
    }
    assert_eq!(
        data.expected_tree,
        text(&repo.git_stdin(["mktree"], input.as_bytes())),
        "the intended tree equals the mktree oracle"
    );
    assert_eq!(oracle::commit_tree(&repo, &commit), data.expected_tree);

    // The working tree: the addition is gone, the deletion returned, the
    // kept passage and the note keep the candidate bytes (SPEC 9.4 step 3).
    assert!(
        !repo.file_path("born.md").exists(),
        "the added file is removed"
    );
    assert_eq!(
        repo.read_file("gone.md"),
        b"# Gone\n\noriginal passage\n",
        "the deleted file returns to its baseline bytes"
    );
    assert_eq!(repo.read_file("kept.md"), b"# Kept\n\nrevised passage\n");
    assert_eq!(repo.read_file("note.txt"), b"the candidate note\n");
    oracle::assert_index_matches_tree(&repo, "HEAD");
    assert_eq!(
        String::from_utf8_lossy(&repo.git_ok(["status", "--porcelain"])),
        ""
    );

    assert!(
        validate_marker(&adapter_for(&repo), &commit).is_ok(),
        "the review commit is a valid marker"
    );
    let (review, baseline) = workspace_state(&repo);
    assert_eq!(review, ReviewState::Reviewed);
    match baseline {
        BaselineStatus::Anchored(marker) => assert_eq!(marker.commit, commit),
        other => panic!("the review anchors the scope: {other:?}"),
    }
}

/// An apply failure retains the partial state it wrote: the first unit's
/// restoration stands in the index, the writer's bytes stand in the working
/// file, the candidate remains `HEAD`, and the response reports the
/// observed state with recovery guidance instead of claiming a rollback
/// (SPEC 9.4 step 2, 9.6).
#[test]
fn an_apply_failure_retains_and_reports_the_partial_application() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let both = [group_of(&pinned, 0), group_of(&pinned, 1)];
    let plan = plan_document(&pinned, "review-restore-all-01", &both, None);

    // The writer replaces the document between the two units: the second
    // unit no longer applies to the working tree.
    let outcome = apply_with_boundary_writer(
        &fixture.repo,
        &plan,
        Boundary::Apply(2),
        "printf 'a writer moved the file\\n' > doc-a.md",
    );
    assert!(!outcome.ok(), "a broken application must fail");
    let error = outcome.error.unwrap();
    assert_eq!(error.code, "PARTIAL_MUTATION");
    assert!(
        error.message.contains("doc-a.md"),
        "the refusal names the path: {}",
        error.message
    );
    assert_eq!(
        error.recovery_argv,
        vec!["margin", "status", "--json"],
        "the refusal names the recovery: {:?}",
        error.recovery_argv
    );

    // The observed effects: the first unit staged and rewrote content, the
    // writer replaced it, and no commit exists.
    assert_eq!(outcome.effects.commit, CommitEffect::None);
    assert_eq!(outcome.effects.index, ObservedEffect::Changed);
    assert_eq!(outcome.effects.working_tree, ObservedEffect::Changed);
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(fixture.candidate.as_str()),
        "the candidate stands"
    );
    let data = outcome.data.as_ref().expect("the observed state");
    assert_eq!(data.commit, None, "no commit was created");

    // The retained state, read with stock Git: the working file holds the
    // writer's bytes, the index holds the first unit's restoration — the
    // latest passage returned to the baseline side — and the status shows
    // staged and unstaged differences for the one path.
    assert_eq!(
        fixture.repo.read_file("doc-a.md"),
        b"a writer moved the file\n",
        "the writer's bytes stay in the working tree"
    );
    let staged_bytes = document(
        "first revised passage\nwith two lines",
        "second original passage",
    );
    let staged_oid = oracle::ls_files(&fixture.repo)
        .into_iter()
        .find(|entry| entry.path == "doc-a.md")
        .expect("the document stays staged")
        .oid;
    assert_eq!(
        staged_oid,
        blob_of(&fixture.repo, staged_bytes.as_bytes()),
        "the index holds the first unit's restoration"
    );
    assert_eq!(
        String::from_utf8_lossy(&fixture.repo.git_ok(["status", "--porcelain"])),
        "MM doc-a.md\n",
        "one path holds staged and unstaged differences"
    );

    // The review stays pending and the baseline did not move.
    let (review, baseline) = workspace_state(&fixture.repo);
    assert_eq!(review, ReviewState::Pending);
    match baseline {
        BaselineStatus::Anchored(marker) => assert_eq!(marker.commit, fixture.start),
        other => panic!("the start marker still anchors the scope: {other:?}"),
    }
}

/// A commit rejection after a successful apply retains the staged result:
/// the index holds T, the working tree holds R, the candidate remains
/// `HEAD`, and the response reports the observed state (SPEC 9.6: patch
/// succeeds, commit fails).
#[test]
fn a_rejected_commit_after_the_apply_reports_the_retained_result() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-restore-01", &[group_of(&pinned, 0)], None);

    let outcome = apply_with_boundary_writer(
        &fixture.repo,
        &plan,
        Boundary::Commit,
        "printf '#!/bin/sh\\nexit 1\\n' > .git/hooks/pre-commit \
         && chmod +x .git/hooks/pre-commit",
    );
    assert!(!outcome.ok(), "a rejected commit must fail");
    let error = outcome.error.unwrap();
    assert_eq!(error.code, "GIT_FAILED");

    // The observed effects: the apply staged and rewrote the scoped
    // content, the hook rejected the commit, and nothing was committed.
    assert_eq!(outcome.effects.commit, CommitEffect::None);
    assert_eq!(outcome.effects.index, ObservedEffect::Changed);
    assert_eq!(outcome.effects.working_tree, ObservedEffect::Changed);
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(fixture.candidate.as_str()),
        "the candidate stands"
    );

    // The retained state: the index holds the preview tree T and the
    // working bytes equal R, so one narrow inspection recovers the exact
    // decision (SPEC 9.7).
    let data = outcome.data.as_ref().expect("the observed state");
    assert_eq!(data.commit, None);
    let expected_bytes = document("first original passage", "second revised passage");
    assert_eq!(
        data.expected_tree,
        expected_tree_with_document(&fixture.repo, &fixture.candidate, expected_bytes.as_bytes()),
        "the intended tree the data names equals the oracle"
    );
    assert_eq!(
        oracle::index_tree(&fixture.repo).as_deref(),
        Some(data.expected_tree.as_str()),
        "the real index holds the preview tree"
    );
    assert_eq!(
        fixture.repo.read_file("doc-a.md"),
        expected_bytes.into_bytes(),
        "the working tree holds the reviewed result"
    );
    assert_eq!(
        String::from_utf8_lossy(&fixture.repo.git_ok(["status", "--porcelain"])),
        "M  doc-a.md\n",
        "the result stays staged with a clean working tree"
    );

    // The review stays pending and the baseline did not move.
    let (review, baseline) = workspace_state(&fixture.repo);
    assert_eq!(review, ReviewState::Pending);
    match baseline {
        BaselineStatus::Anchored(marker) => assert_eq!(marker.commit, fixture.start),
        other => panic!("the start marker still anchors the scope: {other:?}"),
    }
}

/// Working bytes that change between the apply and the commit refuse the
/// commit: the index holds the preview tree, the writer's change stays in
/// the working file, and the response reports the retained state (SPEC 9.4
/// step 3, 9.6).
#[test]
fn working_bytes_that_change_after_the_apply_refuse_before_the_commit() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-restore-01", &[group_of(&pinned, 0)], None);

    let outcome = apply_with_boundary_writer(
        &fixture.repo,
        &plan,
        Boundary::Verify,
        "printf ' later writer bytes' >> doc-a.md",
    );
    assert!(!outcome.ok(), "moved working bytes must refuse the commit");
    let error = outcome.error.unwrap();
    assert_eq!(error.code, "PARTIAL_MUTATION");
    assert!(
        error.message.contains("doc-a.md"),
        "the refusal names the path: {}",
        error.message
    );
    assert!(
        error.message.contains("nothing was committed"),
        "the refusal states what did not happen: {}",
        error.message
    );
    assert_eq!(
        error.recovery_argv,
        vec!["margin", "status", "--json"],
        "the refusal names the recovery: {:?}",
        error.recovery_argv
    );

    // The observed effects: the apply staged and rewrote the scoped
    // content, the writer changed it again, and no commit exists.
    assert_eq!(outcome.effects.commit, CommitEffect::None);
    assert_eq!(outcome.effects.index, ObservedEffect::Changed);
    assert_eq!(outcome.effects.working_tree, ObservedEffect::Changed);
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(fixture.candidate.as_str()),
        "the candidate stands"
    );

    // The retained state: the index holds the preview tree and the working
    // file holds the reviewed result plus the writer's bytes.
    let data = outcome.data.as_ref().expect("the observed state");
    assert_eq!(data.commit, None);
    let expected_bytes = document("first original passage", "second revised passage");
    assert_eq!(
        oracle::index_tree(&fixture.repo).as_deref(),
        Some(
            expected_tree_with_document(
                &fixture.repo,
                &fixture.candidate,
                expected_bytes.as_bytes()
            )
            .as_str()
        ),
        "the real index holds the preview tree"
    );
    let mut working = expected_bytes.into_bytes();
    working.extend_from_slice(b" later writer bytes");
    assert_eq!(
        fixture.repo.read_file("doc-a.md"),
        working,
        "the writer's change stays in the working tree"
    );
    assert_eq!(
        String::from_utf8_lossy(&fixture.repo.git_ok(["status", "--porcelain"])),
        "MM doc-a.md\n",
        "the staged result and the writer's bytes both stand"
    );

    let (review, baseline) = workspace_state(&fixture.repo);
    assert_eq!(review, ReviewState::Pending);
    match baseline {
        BaselineStatus::Anchored(marker) => assert_eq!(marker.commit, fixture.start),
        other => panic!("the start marker still anchors the scope: {other:?}"),
    }
}

/// A hook that succeeds but edits a scoped file leaves the verified commit
/// standing while the response reports the newer content as observed, never
/// as reviewed (SPEC 9.2 observation rules, 9.6, task T032).
#[test]
fn a_successful_hook_that_edits_reports_the_newer_content() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-restore-01", &[group_of(&pinned, 0)], None);
    let hook = fixture.repo.path().join(".git/hooks").join("post-commit");
    fs::write(
        &hook,
        "#!/bin/sh\nprintf ' hook bytes' >> doc-a.md\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

    let outcome = apply(&fixture.repo, &plan);
    assert!(
        outcome.ok(),
        "the hook exit does not fail the review: {:?}",
        outcome.error
    );
    let data = outcome.data.unwrap();
    let commit = data.commit.clone().expect("the application commits");

    // The verified commit holds the preview tree and confers its approval.
    let expected_bytes = document("first original passage", "second revised passage");
    assert_eq!(
        oracle::commit_tree(&fixture.repo, &commit),
        expected_tree_with_document(&fixture.repo, &fixture.candidate, expected_bytes.as_bytes()),
        "the commit holds the reviewed result"
    );
    assert!(
        validate_marker(&adapter_for(&fixture.repo), &commit).is_ok(),
        "the review commit is a valid marker"
    );

    // The hook's bytes are newer uncaptured content: the effects observe
    // them and one warning names them, and the baseline advanced over the
    // committed bytes only.
    let mut hooked = expected_bytes.into_bytes();
    hooked.extend_from_slice(b" hook bytes");
    assert_eq!(
        fixture.repo.read_file("doc-a.md"),
        hooked,
        "the hook's edit stays in the working tree"
    );
    assert_eq!(outcome.effects.commit, CommitEffect::Created);
    assert_eq!(outcome.effects.working_tree, ObservedEffect::Changed);
    assert_eq!(
        String::from_utf8_lossy(&fixture.repo.git_ok(["status", "--porcelain"])),
        " M doc-a.md\n",
        "the newer bytes stay uncaptured"
    );
    assert_eq!(outcome.warnings.len(), 1, "one warning reports the edit");
    assert_eq!(outcome.warnings[0].code, "DIRTY_SCOPE");
    assert!(
        outcome.warnings[0].message.contains("doc-a.md"),
        "the warning names the path: {}",
        outcome.warnings[0].message
    );

    // The status keeps the committed approval — only committed changes
    // reopen review — while the preservation state reports the hook's
    // bytes as content the candidate does not store (SPEC 4.2).
    let git = adapter_for(&fixture.repo);
    let operations = Operations::new(&git);
    let outcome = operations.inspect_workspace(&InspectRequest {
        scope: None,
        path_filter: Vec::new(),
    });
    assert!(
        outcome.ok(),
        "the status query succeeds: {:?}",
        outcome.error
    );
    let status = outcome.data.unwrap();
    assert_eq!(status.review, ReviewState::Reviewed);
    assert_eq!(
        status.preservation,
        margin::domain::PreservationState::Uncaptured,
        "the hook's bytes are newer uncaptured content"
    );
    let uncaptured = status
        .counts
        .uncaptured
        .expect("the uncaptured paths are known");
    assert_eq!(uncaptured.total, 1, "one scoped path is uncaptured");
    assert_eq!(uncaptured.paths, vec!["doc-a.md"]);
    match status.baseline {
        BaselineStatus::Anchored(marker) => {
            assert_eq!(marker.commit, commit, "the verified review stays baseline")
        }
        other => panic!("the review anchors the scope: {other:?}"),
    }
}

/// An exact retry after a restoring review returns the original verified
/// receipt without a second commit or any write (SPEC 8.7, 9.4).
#[test]
fn an_exact_retry_after_a_restoring_review_returns_the_receipt() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-restore-01", &[group_of(&pinned, 0)], None);
    let first = apply(&fixture.repo, &plan);
    assert!(
        first.ok(),
        "the restoring application succeeds: {:?}",
        first.error
    );
    let review = first.data.unwrap().commit.expect("the review commit");
    let commits = text(
        &fixture
            .repo
            .git_ok(["rev-list", "--count", "--first-parent", "HEAD"]),
    );

    let before = oracle::snapshot(&fixture.repo);
    let retry = apply(&fixture.repo, &plan);
    assert!(retry.ok(), "the exact retry succeeds: {:?}", retry.error);
    let data = retry.data.unwrap();
    assert_eq!(
        data.existing_receipt.as_deref(),
        Some(review.as_str()),
        "the retry returns the original verified commit"
    );
    assert_eq!(data.commit, None, "no second review commit");
    assert_eq!(data.kept, None, "the historical group count stays unknown");
    assert_eq!(
        data.paths,
        vec!["doc-a.md".to_string()],
        "the rewritten paths come from the verified trees"
    );
    assert_eq!(retry.effects.commit, CommitEffect::ExistingReceipt);
    assert_eq!(retry.effects.commit_oid.as_deref(), Some(review.as_str()));
    assert_eq!(
        retry.effects.working_tree,
        ObservedEffect::Unchanged,
        "the retry writes nothing"
    );
    assert_eq!(
        text(
            &fixture
                .repo
                .git_ok(["rev-list", "--count", "--first-parent", "HEAD"])
        ),
        commits,
        "no commit joined the history"
    );
    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "the receipt retry");
}

/// Both documented object formats apply their restoring reviews (SPEC
/// 16.5).
#[test]
fn restoring_reviews_work_in_sha256_repositories() {
    let fixture = fixture(RepoOptions::new().with_object_format(ObjectFormat::Sha256));
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-restore-01", &[group_of(&pinned, 0)], None);

    let outcome = apply(&fixture.repo, &plan);
    assert!(
        outcome.ok(),
        "the SHA-256 application succeeds: {:?}",
        outcome.error
    );
    let data = outcome.data.unwrap();
    let commit = data.commit.expect("the review commit");
    assert_eq!(commit.len(), 64, "a SHA-256 object ID");
    let expected_bytes = document("first original passage", "second revised passage");
    assert_eq!(
        oracle::commit_tree(&fixture.repo, &commit),
        expected_tree_with_document(&fixture.repo, &fixture.candidate, expected_bytes.as_bytes()),
        "the commit holds the reviewed result"
    );
    assert_eq!(
        fixture.repo.read_file("doc-a.md"),
        expected_bytes.into_bytes()
    );
    let (review, _) = workspace_state(&fixture.repo);
    assert_eq!(review, ReviewState::Reviewed);
}

// ---------------------------------------------------------------------------
// Typed refusals
// ---------------------------------------------------------------------------

/// Uncaptured scoped bytes refuse the application before anything is
/// committed (SPEC 9.3).
#[test]
fn uncaptured_scoped_bytes_refuse_the_application() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-keep-all-01", &[], None);
    fixture
        .repo
        .write_file("doc-a.md", b"edited while applying\n");
    let dirty = oracle::snapshot(&fixture.repo);

    let outcome = apply(&fixture.repo, &plan);
    assert_refusal(&outcome, "DIRTY_SCOPE", "uncaptured scoped content");
    let error = outcome.error.unwrap();
    assert!(
        error.recovery_argv == vec!["margin", "checkpoint", "--json"],
        "the refusal names the recovery: {:?}",
        error.recovery_argv
    );
    dirty.assert_unchanged(&oracle::snapshot(&fixture.repo), "a refused application");
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(fixture.candidate.as_str())
    );
}

/// A staged entry and a foreign branch refuse with the shared guard codes
/// (SPEC 9.1 step 4).
#[test]
fn staged_entries_and_a_foreign_branch_refuse_the_application() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-keep-all-01", &[], None);

    // A staged out-of-scope entry still blocks: the application commits
    // through the real index.
    fixture
        .repo
        .write_file("note.txt", b"staged candidate note\n");
    fixture.repo.git_ok(["add", "note.txt"]);
    let staged = oracle::snapshot(&fixture.repo);
    assert_refusal(
        &apply(&fixture.repo, &plan),
        "STAGED_CHANGES",
        "a staged entry",
    );
    staged.assert_unchanged(&oracle::snapshot(&fixture.repo), "a refused application");
    fixture
        .repo
        .git_ok(["restore", "--quiet", "--staged", "note.txt"]);
    fixture.repo.write_file("note.txt", b"the candidate note\n");

    // A branch other than the adopted one, even at the same commit: the
    // plan is rebuilt against the branch the workspace is on, so the
    // adopted-branch guard is what refuses.
    fixture
        .repo
        .git_ok(["switch", "--quiet", "--create", "other"]);
    let switched = pinned_review_of(&fixture.repo, &fixture.start);
    let switched_plan = plan_document(&switched, "review-keep-all-01", &[], None);
    assert_refusal(
        &apply(&fixture.repo, &switched_plan),
        "BRANCH_CHANGED",
        "a foreign branch",
    );
}

/// A moved candidate refuses the plan as stale, and the refusal writes
/// nothing (SPEC 9.3).
#[test]
fn stale_plans_refuse_without_writing() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);

    // The writer moved the candidate: the pinned comparison the plan names
    // is gone, so the plan is stale rather than silently reused.
    let keep_all = plan_document(&pinned, "review-keep-all-01", &[], None);
    fixture.repo.write_file(
        "doc-a.md",
        document("first moved passage", "second moved passage").as_bytes(),
    );
    fixture.repo.commit_all("another writing pass");
    let moved = oracle::snapshot(&fixture.repo);
    assert_refusal(
        &apply(&fixture.repo, &keep_all),
        "STALE_REVIEW",
        "a moved candidate",
    );
    moved.assert_unchanged(&oracle::snapshot(&fixture.repo), "a refused stale plan");
}

/// A hook that rewrites the reserved trailers leaves an ordinary commit
/// that confers no approval: the application reports the contract
/// violation with the created commit, and the review stays pending
/// (SPEC 8.4, 9.6).
#[test]
fn a_hook_that_rewrites_trailers_reports_a_contract_violation() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-keep-all-01", &[], None);
    let hook = fixture.repo.path().join(".git/hooks").join("commit-msg");
    fs::write(
        &hook,
        "#!/bin/sh\nprintf 'Margin-Kind: review\\n' >> \"$1\"\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

    let outcome = apply(&fixture.repo, &plan);
    assert!(!outcome.ok(), "a rewritten marker must fail verification");
    let error = outcome.error.unwrap();
    assert_eq!(error.code, "CONTRACT_VIOLATION");
    let created = oracle::head_oid(&fixture.repo).expect("the commit itself succeeded");
    assert_eq!(outcome.effects.commit, CommitEffect::Created);
    assert_eq!(
        outcome.effects.commit_oid.as_deref(),
        Some(created.as_str())
    );
    let data = outcome.data.as_ref().expect("the observed state");
    assert_eq!(data.commit.as_deref(), Some(created.as_str()));
    let git = adapter_for(&fixture.repo);
    assert!(validate_marker(&git, &created).is_err(), "no approval");

    // The displayed baseline did not advance: the walk still anchors at the
    // start marker and the review stays pending (SPEC 4.2, 9.6).
    let (review, baseline) = workspace_state(&fixture.repo);
    assert_eq!(review, ReviewState::Pending);
    match baseline {
        BaselineStatus::Anchored(marker) => assert_eq!(marker.commit, fixture.start),
        other => panic!("the start marker still anchors the scope: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The precondition recheck and the mutation lock (SPEC 9.1 steps 2 and 6)
// ---------------------------------------------------------------------------

/// Runs one application whose concurrent writer — injected at the
/// proof-tail pass of the intended-tree proof — performs `action`, and
/// returns the result.
fn apply_with_seam_writer(
    repo: &TestRepo,
    plan: &str,
    action: &str,
) -> OperationOutcome<ReviewDecision> {
    let git = seam_adapter(repo, SeamPass::ProofTail, action);
    let operations = Operations::new(&git);
    operations.apply_review(&ReviewRequest {
        scope: None,
        plan: plan.as_bytes().to_vec(),
        dry_run: false,
    })
}

/// Which boundary of one restoring application triggers an injected
/// action (SPEC 9.4, 16.6).
enum Boundary {
    /// The real `git apply --index --reverse` of the nth selected unit,
    /// one-based, after that unit's applicability check passed.
    Apply(usize),
    /// The real `write-tree` that begins the post-apply verification of
    /// the applied result, after the last unit applied.
    Verify,
    /// The final `git commit` of the already validated index.
    Commit,
}

impl Boundary {
    /// The value the wrapper compares.
    fn as_value(&self) -> String {
        match self {
            Boundary::Apply(number) => format!("apply-{number}"),
            Boundary::Verify => "verify".to_string(),
            Boundary::Commit => "commit".to_string(),
        }
    }
}

/// Runs one application whose Git boundary — the real application of one
/// selected unit, the verification after the apply, or the commit —
/// performs `action` first, through one wrapper that delegates every call
/// to the installed Git (SPEC 16.6 failpoint seam).
///
/// The wrapper never fires inside a disposable index: only the real
/// repository state is a boundary of the operation. The unit counter lives
/// outside the repository, so no scoped content observes it.
fn apply_with_boundary_writer(
    repo: &TestRepo,
    plan: &str,
    boundary: Boundary,
    action: &str,
) -> OperationOutcome<ReviewDecision> {
    let script = repo.root().join("boundary-seam.sh");
    let counter = repo.root().join("boundary-seam-count");
    fs::write(
        &script,
        concat!(
            "#!/bin/sh\n",
            "# Deterministic boundary seam (SPEC 16.6): delegate to the installed\n",
            "# git, and run the injected action when the configured boundary of a\n",
            "# restoring application arrives. The counter lives outside the\n",
            "# repository.\n",
            "count=0\n",
            "if [ -f \"$MARGIN_SEAM_COUNT\" ]; then\n",
            "  count=$(cat \"$MARGIN_SEAM_COUNT\")\n",
            "fi\n",
            "fire=0\n",
            "if [ -z \"$GIT_INDEX_FILE\" ]; then\n",
            "  case \"$1\" in\n",
            "    commit) if [ \"$MARGIN_SEAM_FIRE\" = commit ]; then fire=1; fi ;;\n",
            "    write-tree) if [ \"$MARGIN_SEAM_FIRE\" = verify ]; then fire=1; fi ;;\n",
            "  esac\n",
            "  if [ \"$3\" = apply ]; then\n",
            "    case \" $* \" in\n",
            "      *\" --check \"*|*\" --cached \"*) ;;\n",
            "      *)\n",
            "        count=$((count + 1))\n",
            "        printf '%s' \"$count\" > \"$MARGIN_SEAM_COUNT\"\n",
            "        if [ \"$MARGIN_SEAM_FIRE\" = \"apply-$count\" ]; then fire=1; fi\n",
            "        ;;\n",
            "    esac\n",
            "  fi\n",
            "fi\n",
            "if [ \"$fire\" = 1 ]; then\n",
            "  sh -c \"$MARGIN_SEAM_ACTION\"\n",
            "fi\n",
            "exec git \"$@\"\n",
        ),
    )
    .unwrap_or_else(|error| panic!("cannot write {}: {error}", script.display()));
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|error| panic!("cannot chmod {}: {error}", script.display()));
    let mut vars = repo.env().vars().clone();
    vars.insert(
        OsString::from("MARGIN_SEAM_FIRE"),
        OsString::from(boundary.as_value()),
    );
    vars.insert(OsString::from("MARGIN_SEAM_ACTION"), OsString::from(action));
    vars.insert(
        OsString::from("MARGIN_SEAM_COUNT"),
        counter.into_os_string(),
    );
    let git = Git::from_env(vars)
        .at(repo.path())
        .with_program(script.into_os_string());
    let operations = Operations::new(&git);
    operations.apply_review(&ReviewRequest {
        scope: None,
        plan: plan.as_bytes().to_vec(),
        dry_run: false,
    })
}

/// A `HEAD` that moves while the application is prepared refuses before
/// the commit, and the concurrent commit stands (SPEC 9.1 step 6).
#[test]
fn a_head_that_moves_before_the_commit_refuses_without_writing() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-keep-all-01", &[], None);

    let outcome = apply_with_seam_writer(
        &fixture.repo,
        &plan,
        "git commit --quiet --allow-empty --message concurrent-writer",
    );
    assert!(!outcome.ok(), "a moved HEAD must refuse the application");
    let error = outcome.error.unwrap();
    assert_eq!(error.code, "STALE_STATE");
    assert!(
        error.message.contains("nothing was committed"),
        "the refusal names its own pass: {}",
        error.message
    );
    assert_eq!(outcome.effects, Effects::unchanged());
    let concurrent = oracle::head_oid(&fixture.repo).expect("the concurrent commit stands");
    assert_ne!(concurrent, fixture.candidate);
    assert!(
        error.message.contains(&concurrent),
        "the refusal names the observed HEAD: {}",
        error.message
    );
    assert_eq!(
        text(
            &fixture
                .repo
                .git_ok(["diff-index", "--cached", "--name-only", "HEAD"])
        ),
        "",
        "nothing was staged over the concurrent commit"
    );

    // The review state did not advance over a commit Margin never made.
    let (review, baseline) = workspace_state(&fixture.repo);
    assert_eq!(review, ReviewState::Pending);
    match baseline {
        BaselineStatus::Anchored(marker) => assert_eq!(marker.commit, fixture.start),
        other => panic!("the start marker still anchors the scope: {other:?}"),
    }
}

/// Scoped bytes that change while the application is prepared refuse
/// before the commit, and the writer's change stays in the working tree
/// (SPEC 9.1 step 6, 9.3).
#[test]
fn scoped_bytes_that_change_before_the_commit_refuse_without_writing() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-keep-all-01", &[], None);

    // The writer edits after the intended-tree proof closed and before the
    // precondition recheck.
    let candidate_bytes = fixture.repo.read_file("doc-a.md");
    let outcome =
        apply_with_seam_writer(&fixture.repo, &plan, "printf ' writer bytes' >> doc-a.md");
    assert!(
        !outcome.ok(),
        "moved scoped bytes must refuse the application"
    );
    let error = outcome.error.unwrap();
    assert_eq!(error.code, "DIRTY_SCOPE");
    assert!(
        error.message.contains("doc-a.md"),
        "the refusal names the moved path: {}",
        error.message
    );
    assert_eq!(outcome.effects, Effects::unchanged());
    let mut expected = candidate_bytes;
    expected.extend_from_slice(b" writer bytes");
    assert_eq!(
        fixture.repo.read_file("doc-a.md"),
        expected,
        "the writer's change stays in the working tree"
    );
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(fixture.candidate.as_str()),
        "no commit appeared"
    );
}

/// A staged entry that arrives while the application is prepared refuses
/// before the commit: the review never consumes a pre-existing staged
/// selection (SPEC 9.1 step 6, 9.2).
#[test]
fn a_staged_entry_that_arrives_before_the_commit_refuses_without_writing() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-keep-all-01", &[], None);

    let outcome = apply_with_seam_writer(
        &fixture.repo,
        &plan,
        "printf 'staged' >> note.txt && git add -- note.txt",
    );
    assert!(!outcome.ok(), "a staged entry must refuse the application");
    let error = outcome.error.unwrap();
    assert_eq!(error.code, "STAGED_CHANGES");
    assert_eq!(outcome.effects, Effects::unchanged());
    assert_eq!(
        text(
            &fixture
                .repo
                .git_ok(["diff-index", "--cached", "--name-only", "HEAD"])
        ),
        "note.txt",
        "the writer's staged entry stands, and nothing else was staged"
    );
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(fixture.candidate.as_str()),
        "no commit appeared"
    );
}

/// The shared mutation lock serializes the application: one held lock
/// refuses `BUSY` without intended effects, and every return path releases
/// it for the next operation (SPEC 9.1 step 2).
#[test]
fn the_lock_serializes_the_application() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-keep-all-01", &[], None);

    let held = hold_mutation_lock(&fixture.repo);
    assert_refusal(&apply(&fixture.repo, &plan), "BUSY", "a competing lock");
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(fixture.candidate.as_str()),
        "nothing was committed"
    );
    drop(held);

    let outcome = apply(&fixture.repo, &plan);
    assert!(
        outcome.ok(),
        "the retry after release must not be BUSY: {:?}",
        outcome.error
    );
}

// ---------------------------------------------------------------------------
// The executable and both object formats
// ---------------------------------------------------------------------------

/// The installed executable applies the plan and reports the commit in one
/// JSON envelope and one text line (SPEC 7.1, 6.3).
#[test]
fn the_executable_applies_the_plan_and_reports_the_commit() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(
        &pinned,
        "review-keep-all-01",
        &[],
        Some("keep every change"),
    );
    let path = fixture.repo.root().join("plan.json");
    fs::write(&path, &plan).unwrap();
    let path = path.to_str().unwrap().to_string();

    let output = margin(
        fixture.repo.env(),
        fixture.repo.path(),
        &["review", "--input", &path, "--json"],
    );
    let envelope = envelope_of(&output, "an application through the executable");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(envelope["command"], "review");
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["data"]["dry_run"], false);
    assert_eq!(envelope["data"]["kept"], 2);
    let commit = oracle::head_oid(&fixture.repo).expect("the review commit");
    assert_eq!(envelope["data"]["commit"], json!(commit));
    assert_eq!(
        envelope["data"]["expected_tree"],
        oracle::commit_tree(&fixture.repo, &fixture.candidate)
    );
    assert_eq!(envelope["effects"]["working_tree"], "unchanged");
    assert_eq!(envelope["effects"]["index"], "unchanged");
    assert_eq!(envelope["effects"]["commit"], "created");
    assert_eq!(envelope["effects"]["commit_oid"], json!(commit));
    assert_eq!(envelope["effects"]["repository"], "unchanged");
    assert_eq!(envelope["effects"]["configuration"], "unchanged");

    // A refused application is one bounded envelope with its exit class.
    let output = margin(
        fixture.repo.env(),
        fixture.repo.path(),
        &["review", "--input", &path, "--json"],
    );
    let envelope = envelope_of(&output, "an exact retry through the executable");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(envelope["data"]["existing_receipt"], json!(commit));
    assert_eq!(envelope["effects"]["commit"], "existing_receipt");
    assert_eq!(envelope["effects"]["commit_oid"], json!(commit));

    let output = margin(
        fixture.repo.env(),
        fixture.repo.path(),
        &["review", "--input", &path],
    );
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("margin:"), "one text line: {stdout}");
    assert!(
        stdout.contains(&format!("receipt={commit}")),
        "the text line names the receipt: {stdout}"
    );
    assert!(stdout.ends_with('\n'));
}

/// The installed executable applies a restoring plan and reports the
/// rewritten path, the changed effects, and the created commit in one JSON
/// envelope and one text line (SPEC 7.1, 6.3).
#[test]
fn the_executable_applies_a_restoring_plan() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let restored = group_of(&pinned, 0);
    let plan = plan_document(
        &pinned,
        "review-restore-01",
        std::slice::from_ref(&restored),
        Some("restore the first passage"),
    );
    let path = fixture.repo.root().join("plan.json");
    fs::write(&path, &plan).unwrap();
    let path = path.to_str().unwrap().to_string();

    let output = margin(
        fixture.repo.env(),
        fixture.repo.path(),
        &["review", "--input", &path, "--json"],
    );
    let envelope = envelope_of(&output, "a restoring application through the executable");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(envelope["command"], "review");
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["data"]["restored"], json!([restored]));
    assert_eq!(envelope["data"]["kept"], 1);
    assert_eq!(envelope["data"]["paths"], json!(["doc-a.md"]));
    let commit = oracle::head_oid(&fixture.repo).expect("the review commit");
    assert_eq!(envelope["data"]["commit"], json!(commit));
    assert_eq!(
        envelope["data"]["expected_tree"],
        expected_tree_with_document(
            &fixture.repo,
            &fixture.candidate,
            document("first original passage", "second revised passage").as_bytes(),
        )
    );
    assert_eq!(envelope["effects"]["working_tree"], "changed");
    assert_eq!(envelope["effects"]["index"], "changed");
    assert_eq!(envelope["effects"]["commit"], "created");
    assert_eq!(envelope["effects"]["commit_oid"], json!(commit));

    let output = margin(
        fixture.repo.env(),
        fixture.repo.path(),
        &["review", "--input", &path],
    );
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("margin:"), "one text line: {stdout}");
    assert!(
        stdout.contains("restore=1") && stdout.contains("paths=doc-a.md"),
        "the text line names the decision: {stdout}"
    );
    assert!(stdout.ends_with('\n'));
}

/// Both documented object formats apply their keep-all reviews (SPEC 16.5).
#[test]
fn keep_all_works_in_sha256_repositories() {
    let fixture = fixture(RepoOptions::new().with_object_format(ObjectFormat::Sha256));
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-keep-all-01", &[], None);

    let outcome = apply(&fixture.repo, &plan);
    assert!(
        outcome.ok(),
        "the SHA-256 application succeeds: {:?}",
        outcome.error
    );
    let commit = outcome.data.unwrap().commit.expect("the review commit");
    assert_eq!(commit.len(), 64, "a SHA-256 object ID");
    assert_eq!(
        oracle::commit_tree(&fixture.repo, &commit),
        oracle::commit_tree(&fixture.repo, &fixture.candidate),
        "the empty review holds the candidate's full tree"
    );
    let (review, _) = workspace_state(&fixture.repo);
    assert_eq!(review, ReviewState::Reviewed);
}

// ---------------------------------------------------------------------------
// Executable helpers
// ---------------------------------------------------------------------------

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

/// Parses the one JSON envelope of a machine-mode run (SPEC 7.1).
fn envelope_of(output: &Output, what: &str) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let envelope: Value = serde_json::from_str(stdout.trim_end())
        .unwrap_or_else(|error| panic!("{what}: the envelope must parse: {error}: {stdout}"));
    assert_eq!(envelope["schema_version"], 1, "{what}");
    envelope
}
