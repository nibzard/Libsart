//! Real-Git tests for the retained-plan resume (SPEC 9.7, task T035).
//!
//! Every case proves one rule of the explicit recovery contract:
//!
//! - a retained result — the index at the proved tree T, the scoped working
//!   bytes at the reviewed result R, and the candidate C still at `HEAD` —
//!   resumes into exactly the review commit the interrupted application
//!   would have created, without reapplying any patch
//! - a resume of a request that already completed reports the receipt the
//!   history holds first, so a completed decision never commits twice
//! - the retained result of another plan, later working bytes, staged
//!   content outside the restored passages, and a moved candidate each
//!   refuse before the commit and leave the observed state untouched
//! - the executable reports the same decision contract in one JSON envelope
//!   and one text line
//!
//! The retained state is produced by the real failure boundary of SPEC 16.6:
//! one rejecting pre-commit hook stops the application after the apply
//! proved and staged its result, which is the state row "patch succeeds;
//! commit fails" of SPEC 9.6. Nothing is staged or reset by hand.

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use margin::app::{
    BaselineStatus, ChangeDetail, CommitEffect, DiffRequest, Effects, Identities, InitRequest,
    InspectRequest, ObservedEffect, OperationOutcome, Operations, ReviewDecision, ReviewOutcome,
    ReviewRequest, validate_marker,
};
use margin::domain::{ReviewPlan, ReviewState, ScopeInput};
use margin::git::Git;
use serde_json::{Value, json};
use support::env::{IsolatedEnv, is_scrubbed};
use support::repo::{RepoOptions, TestRepo};
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
    run(repo, plan, false, false)
}

/// Resumes one plan document through the operation layer.
fn resume(repo: &TestRepo, plan: &str) -> OperationOutcome<ReviewDecision> {
    run(repo, plan, false, true)
}

/// Resumes one plan document as a dry run through the operation layer.
fn resume_dry(repo: &TestRepo, plan: &str) -> OperationOutcome<ReviewDecision> {
    run(repo, plan, true, true)
}

/// Runs one plan document through the operation layer, as an application,
/// a resumption, or the dry run of a resumption.
fn run(
    repo: &TestRepo,
    plan: &str,
    dry_run: bool,
    resume: bool,
) -> OperationOutcome<ReviewDecision> {
    let git = adapter_for(repo);
    let operations = Operations::new(&git);
    let request = ReviewRequest {
        scope: None,
        plan: plan.as_bytes().to_vec(),
        dry_run,
    };
    if resume {
        operations.resume_review(&request)
    } else {
        operations.apply_review(&request)
    }
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
// The rejecting commit boundary (SPEC 16.6)
// ---------------------------------------------------------------------------

/// The path of one hook of the repository, resolved with stock Git.
fn hook_path(repo: &TestRepo, name: &str) -> PathBuf {
    let resolved = text(&repo.git_ok(["rev-parse", "--git-path", "hooks"]));
    let hooks = PathBuf::from(resolved);
    let hooks = if hooks.is_absolute() {
        hooks
    } else {
        repo.path().join(hooks)
    };
    hooks.join(name)
}

/// Installs one pre-commit hook that rejects every commit, so one
/// application proves and stages its result and stops at the commit
/// boundary (SPEC 16.6): the retained state of SPEC 9.6 and 9.7.
fn install_rejecting_hook(repo: &TestRepo) {
    let hook = hook_path(repo, "pre-commit");
    fs::write(&hook, "#!/bin/sh\nexit 1\n")
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", hook.display()));
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|error| panic!("cannot chmod {}: {error}", hook.display()));
}

/// Removes the rejecting hook, so one later commit may run.
fn remove_rejecting_hook(repo: &TestRepo) {
    let _ = fs::remove_file(hook_path(repo, "pre-commit"));
}

/// Stops one application of `plan` at its commit boundary and leaves the
/// retained state behind: the index holds T, the working bytes hold R, and
/// the candidate stays `HEAD` (SPEC 9.6).
fn interrupt_at_commit(repo: &TestRepo, plan: &str) {
    install_rejecting_hook(repo);
    let outcome = apply(repo, plan);
    assert!(
        !outcome.ok(),
        "the rejecting hook must stop the application"
    );
    assert_eq!(
        outcome.error.as_ref().unwrap().code,
        "GIT_FAILED",
        "the refused commit is one Git failure"
    );
    remove_rejecting_hook(repo);
}

// ---------------------------------------------------------------------------
// The verified resume of the exact retained result
// ---------------------------------------------------------------------------

/// The exact retained result resumes into the review commit the
/// interrupted application would have created: one commit over the already
/// validated index, no patch reapplied, no file rewritten, and the reserved
/// trailers the marker validator proves (SPEC 9.7, 8.3-8.4).
#[test]
fn the_exact_retained_result_resumes_into_the_verified_commit() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let restored = group_of(&pinned, 0);
    let plan = plan_document(
        &pinned,
        "review-resume-01",
        std::slice::from_ref(&restored),
        None,
    );
    interrupt_at_commit(&fixture.repo, &plan);

    // The retained state, read with stock Git: the candidate stands, the
    // index holds T, and the working bytes hold R (SPEC 9.6).
    let expected_bytes = document("first original passage", "second revised passage");
    let expected_tree =
        expected_tree_with_document(&fixture.repo, &fixture.candidate, expected_bytes.as_bytes());
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(fixture.candidate.as_str()),
        "the candidate stands"
    );
    assert_eq!(
        oracle::index_tree(&fixture.repo).as_deref(),
        Some(expected_tree.as_str()),
        "the index holds the proved tree"
    );
    assert_eq!(
        fixture.repo.read_file("doc-a.md"),
        expected_bytes.into_bytes(),
        "the working tree holds the reviewed result"
    );

    // The bytes and staged entries the resume must not disturb.
    let staged_before = oracle::ls_files(&fixture.repo);
    let before = oracle::snapshot(&fixture.repo);
    let commits_before =
        text(
            &fixture
                .repo
                .git_ok(["rev-list", "--count", "--first-parent", "HEAD"]),
        );

    // The resume commits only (SPEC 9.7): the same decision contract the
    // application reports, over the proved retained state.
    let outcome = resume(&fixture.repo, &plan);
    assert!(outcome.ok(), "the resume commits: {:?}", outcome.error);
    let data = outcome.data.as_ref().unwrap();
    assert!(!data.dry_run);
    assert_eq!(data.restored, vec![restored]);
    assert_eq!(data.kept, Some(1), "the other passage stays");
    assert_eq!(data.paths, vec!["doc-a.md".to_string()]);
    assert_eq!(data.expected_tree, expected_tree);
    assert_eq!(data.existing_receipt, None);
    let commit = data.commit.clone().expect("the resume commits");
    assert_eq!(outcome.effects.commit, CommitEffect::Created);
    assert_eq!(outcome.effects.commit_oid.as_deref(), Some(commit.as_str()));
    assert_eq!(
        outcome.effects.working_tree,
        ObservedEffect::Unchanged,
        "the resume rewrites nothing"
    );
    assert_eq!(outcome.effects.index, ObservedEffect::Unchanged);

    // Exactly one commit joined, and the response names it.
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
    assert_eq!(
        oracle::commit_tree(&fixture.repo, &commit),
        expected_tree,
        "the resume commit holds the proved tree"
    );
    oracle::assert_out_of_scope_tree_unchanged(
        &fixture.repo,
        &fixture.candidate,
        &commit,
        &["doc-a.md"],
        "the resumed review",
    );

    // The reserved trailers the marker validator proves (SPEC 8.3-8.4):
    // exactly the receipt the interrupted application prepared.
    let marker = validate_marker(&adapter_for(&fixture.repo), &commit)
        .unwrap_or_else(|error| panic!("the resumed commit is a valid marker: {error}"));
    assert_eq!(marker.kind.as_str(), "review");
    assert_eq!(marker.candidate, fixture.candidate);
    assert_eq!(marker.result_tree, expected_tree);
    assert_eq!(marker.baseline.as_deref(), Some(fixture.start.as_str()));
    assert_eq!(marker.request.as_deref(), Some("review-resume-01"));
    assert_eq!(
        marker.plan.as_deref(),
        Some(fingerprint_of(&fixture.repo, &plan).as_str())
    );

    // No patch was reapplied: identical working bytes, identical staged
    // entries, and the index the commit itself published (SPEC 9.7).
    let after = oracle::snapshot(&fixture.repo);
    assert!(
        before.working_diff(&after).is_empty(),
        "the resume reapplied no patch"
    );
    assert_eq!(
        oracle::ls_files(&fixture.repo),
        staged_before,
        "the staged entries are the same entries"
    );
    oracle::assert_index_matches_tree(&fixture.repo, "HEAD");
    assert_eq!(
        text(&fixture.repo.git_ok(["status", "--porcelain"])),
        "",
        "the workspace is clean after the resume"
    );

    // The displayed baseline advanced to the verified review (SPEC 4.2).
    let (review, baseline) = workspace_state(&fixture.repo);
    assert_eq!(review, ReviewState::Reviewed);
    match baseline {
        BaselineStatus::Anchored(marker) => assert_eq!(marker.commit, commit),
        other => panic!("the resumed review anchors the scope: {other:?}"),
    }
}

/// A resume of a request that already completed reports the receipt the
/// history holds first, so the completed decision never commits twice
/// (SPEC 8.7, 9.7).
#[test]
fn the_resume_of_a_completed_request_reports_the_receipt() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(
        &pinned,
        "review-resume-retry-01",
        &[group_of(&pinned, 0)],
        None,
    );
    interrupt_at_commit(&fixture.repo, &plan);

    let first = resume(&fixture.repo, &plan);
    assert!(first.ok(), "the resume commits: {:?}", first.error);
    let commit = first.data.as_ref().unwrap().commit.clone().unwrap();
    let commits_after =
        text(
            &fixture
                .repo
                .git_ok(["rev-list", "--count", "--first-parent", "HEAD"]),
        );

    // The exact retry reports the verified receipt, not a second commit.
    let second = resume(&fixture.repo, &plan);
    assert!(
        second.ok(),
        "the retry reports the receipt: {:?}",
        second.error
    );
    let data = second.data.as_ref().unwrap();
    assert_eq!(data.existing_receipt.as_deref(), Some(commit.as_str()));
    assert_eq!(data.commit, None, "no second review commit was made");
    assert_eq!(second.effects.commit, CommitEffect::ExistingReceipt);
    assert_eq!(second.effects.commit_oid.as_deref(), Some(commit.as_str()));
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(commit.as_str()),
        "HEAD still names the one review commit"
    );
    assert_eq!(
        text(
            &fixture
                .repo
                .git_ok(["rev-list", "--count", "--first-parent", "HEAD"])
        ),
        commits_after,
        "no commit joined the history"
    );
}

// ---------------------------------------------------------------------------
// The unmatched states that refuse (SPEC 9.7)
// ---------------------------------------------------------------------------

/// The retained result of another plan refuses the resume: the plan
/// recomputes its own intended tree, the index does not hold it, and the
/// matching plan still resumes its own result afterwards (SPEC 9.7).
#[test]
fn the_retained_result_of_another_plan_refuses_and_keeps_the_state() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let one = plan_document(
        &pinned,
        "review-resume-one-01",
        &[group_of(&pinned, 0)],
        None,
    );
    let both = plan_document(
        &pinned,
        "review-resume-both-01",
        &[group_of(&pinned, 0), group_of(&pinned, 1)],
        None,
    );
    interrupt_at_commit(&fixture.repo, &one);
    let retained = oracle::index_tree(&fixture.repo).expect("the retained tree");

    // The other plan recomputes another T; the resume refuses before the
    // commit and leaves the observed state untouched.
    let outcome = resume(&fixture.repo, &both);
    assert_refusal(&outcome, "STALE_REVIEW", "the resume of the other plan");
    let error = outcome.error.unwrap();
    assert!(
        error.message.contains(&retained),
        "the refusal names the observed tree: {}",
        error.message
    );
    assert_eq!(
        error.recovery_argv,
        vec!["margin", "doctor", "--json"],
        "the refusal names the recovery inspection: {:?}",
        error.recovery_argv
    );
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(fixture.candidate.as_str()),
        "nothing was committed"
    );
    assert_eq!(
        oracle::index_tree(&fixture.repo).as_deref(),
        Some(retained.as_str()),
        "the retained result stays staged"
    );

    // The matching plan still resumes its own retained result.
    let outcome = resume(&fixture.repo, &one);
    assert!(
        outcome.ok(),
        "the matching plan resumes: {:?}",
        outcome.error
    );
    let commit = outcome.data.as_ref().unwrap().commit.clone().unwrap();
    assert_eq!(
        oracle::commit_tree(&fixture.repo, &commit),
        retained,
        "the resumed commit holds the retained tree"
    );
}

/// Working bytes that differ from the reviewed result refuse the resume,
/// and the writer's change stays in the working file (SPEC 9.7).
#[test]
fn working_bytes_that_differ_from_the_retained_result_refuse() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(
        &pinned,
        "review-resume-dirty-01",
        &[group_of(&pinned, 0)],
        None,
    );
    interrupt_at_commit(&fixture.repo, &plan);
    let retained = oracle::index_tree(&fixture.repo).expect("the retained tree");
    fixture
        .repo
        .write_file("doc-a.md", b"a writer moved the file\n");

    let outcome = resume(&fixture.repo, &plan);
    assert_refusal(&outcome, "STALE_REVIEW", "the resume over moved bytes");
    let error = outcome.error.unwrap();
    assert!(
        error.message.contains("doc-a.md"),
        "the refusal names the path: {}",
        error.message
    );
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(fixture.candidate.as_str()),
        "nothing was committed"
    );
    assert_eq!(
        oracle::index_tree(&fixture.repo).as_deref(),
        Some(retained.as_str()),
        "the retained index stays staged"
    );
    assert_eq!(
        fixture.repo.read_file("doc-a.md"),
        b"a writer moved the file\n",
        "the writer's bytes stay in the working tree"
    );
}

/// Staged content outside the restored passages refuses the resume, and
/// both staged entries stay staged (SPEC 9.7: no unrelated staged delta).
#[test]
fn staged_content_outside_the_restored_passages_refuses() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(
        &pinned,
        "review-resume-staged-01",
        &[group_of(&pinned, 0)],
        None,
    );
    interrupt_at_commit(&fixture.repo, &plan);
    fixture
        .repo
        .write_file("note.txt", b"the candidate note\nand staged content\n");
    fixture.repo.git_ok(["add", "--", "note.txt"]);

    let outcome = resume(&fixture.repo, &plan);
    assert_refusal(
        &outcome,
        "STALE_REVIEW",
        "the resume over unrelated staged content",
    );
    let error = outcome.error.unwrap();
    assert!(
        error.message.contains("note.txt"),
        "the refusal names the unrelated path: {}",
        error.message
    );
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(fixture.candidate.as_str()),
        "nothing was committed"
    );
    assert_eq!(
        oracle::ls_files(&fixture.repo)
            .into_iter()
            .find(|entry| entry.path == "note.txt")
            .expect("the unrelated staging stays staged")
            .oid,
        blob_of(&fixture.repo, b"the candidate note\nand staged content\n"),
        "the writer's staged entry stays exactly as staged"
    );
    assert_eq!(
        text(&fixture.repo.git_ok(["status", "--porcelain"])),
        "M  doc-a.md\nM  note.txt",
        "the observed state is untouched"
    );
}

/// A candidate that moved refuses the resume: the plan names C, `HEAD`
/// holds another commit, and the concurrent commit stands (SPEC 9.7).
#[test]
fn a_candidate_that_moved_refuses_the_resume() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(
        &pinned,
        "review-resume-moved-01",
        &[group_of(&pinned, 0)],
        None,
    );
    interrupt_at_commit(&fixture.repo, &plan);
    fixture.repo.git_ok([
        "commit",
        "--quiet",
        "--allow-empty",
        "--message",
        "concurrent-writer",
    ]);
    let moved = oracle::head_oid(&fixture.repo).expect("the concurrent commit");

    let outcome = resume(&fixture.repo, &plan);
    assert_refusal(
        &outcome,
        "STALE_REVIEW",
        "the resume over a moved candidate",
    );
    let error = outcome.error.unwrap();
    assert!(
        error.message.contains(&fixture.candidate),
        "the refusal names the candidate the plan pins: {}",
        error.message
    );
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(moved.as_str()),
        "the concurrent commit stands"
    );
}

/// A dry run of the resume reports the retained decision and commits
/// nothing (SPEC 6.4, 9.7).
#[test]
fn a_dry_run_resume_reports_the_retained_decision_without_committing() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(
        &pinned,
        "review-resume-dry-01",
        &[group_of(&pinned, 0)],
        None,
    );
    interrupt_at_commit(&fixture.repo, &plan);
    let retained = oracle::index_tree(&fixture.repo).expect("the retained tree");
    let before = oracle::snapshot(&fixture.repo);

    let outcome = resume_dry(&fixture.repo, &plan);
    assert!(outcome.ok(), "the dry run verifies: {:?}", outcome.error);
    let data = outcome.data.as_ref().unwrap();
    assert!(data.dry_run);
    assert_eq!(data.commit, None);
    assert_eq!(data.expected_tree, retained);
    assert_eq!(outcome.effects, Effects::unchanged());
    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "the dry run");

    // The state still resumes afterwards.
    let outcome = resume(&fixture.repo, &plan);
    assert!(
        outcome.ok(),
        "the retained state still resumes: {:?}",
        outcome.error
    );
}

// ---------------------------------------------------------------------------
// The executable surface
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

/// The installed executable resumes the retained result and reports the
/// created commit in one JSON envelope and one text line (SPEC 7.1, 6.3,
/// 9.7). A refused resume keeps its exit class.
#[test]
fn the_executable_resumes_the_retained_result() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let restored = group_of(&pinned, 0);
    let plan = plan_document(
        &pinned,
        "review-resume-cli-01",
        std::slice::from_ref(&restored),
        Some("resume the retained decision"),
    );
    interrupt_at_commit(&fixture.repo, &plan);
    let expected_tree = oracle::index_tree(&fixture.repo).expect("the retained tree");
    let path = fixture.repo.root().join("plan.json");
    fs::write(&path, &plan).unwrap();
    let path = path.to_str().unwrap().to_string();

    let output = margin(
        fixture.repo.env(),
        fixture.repo.path(),
        &["review", "--input", &path, "--resume", "--json"],
    );
    let envelope = envelope_of(&output, "a resumption through the executable");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(envelope["command"], "review");
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["data"]["dry_run"], false);
    assert_eq!(envelope["data"]["restored"], json!(vec![restored]));
    assert_eq!(envelope["data"]["kept"], 1);
    assert_eq!(envelope["data"]["expected_tree"], json!(expected_tree));
    let commit = oracle::head_oid(&fixture.repo).expect("the resumed review commit");
    assert_eq!(envelope["data"]["commit"], json!(commit));
    assert_eq!(envelope["effects"]["working_tree"], "unchanged");
    assert_eq!(envelope["effects"]["index"], "unchanged");
    assert_eq!(envelope["effects"]["commit"], "created");
    assert_eq!(envelope["effects"]["commit_oid"], json!(commit));

    // The exact retry reports the receipt in one text line (SPEC 6.3, 8.7).
    let output = margin(
        fixture.repo.env(),
        fixture.repo.path(),
        &["review", "--input", &path, "--resume"],
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

/// A refused resumption keeps its exit class and reports one typed envelope
/// through the executable (SPEC 7.1, 7.5, 9.7).
#[test]
fn the_executable_refuses_an_unmatched_retained_state() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(
        &pinned,
        "review-resume-refuse-01",
        &[group_of(&pinned, 0)],
        None,
    );
    interrupt_at_commit(&fixture.repo, &plan);
    fixture
        .repo
        .write_file("doc-a.md", b"a writer moved the file\n");
    let path = fixture.repo.root().join("plan.json");
    fs::write(&path, &plan).unwrap();
    let path = path.to_str().unwrap().to_string();

    let output = margin(
        fixture.repo.env(),
        fixture.repo.path(),
        &["review", "--input", &path, "--resume", "--json"],
    );
    let envelope = envelope_of(&output, "a refused resumption");
    assert_eq!(output.status.code(), Some(3), "one stale-context refusal");
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["error"]["code"], "STALE_REVIEW");
    assert_eq!(envelope["effects"]["working_tree"], "unchanged");
    assert_eq!(envelope["effects"]["index"], "unchanged");
    assert_eq!(envelope["effects"]["commit"], "none");
}
