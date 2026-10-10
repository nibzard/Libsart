//! Real-Git tests for request receipt lookup (SPEC 8.7, tasks T030/T060).
//!
//! Every case proves one receipt rule against the installed Git:
//!
//! - the scan covers the complete unfiltered first-parent history, so an
//!   empty review receipt stays findable where a path-limited log hides it
//! - the lookup precedes every present-day stale check: uncaptured bytes,
//!   staged entries, a foreign branch, and a storage transformation never
//!   hide an already validated receipt, and current `HEAD` may have
//!   advanced since the operation completed
//! - an exact retry returns the original verified commit, with the
//!   response data built from the recorded canonical scope and immutable
//!   Git objects: a scope root that became a symlink, disappeared, or
//!   became unreadable blocks a new mutation but never the receipt
//! - only a marker that satisfies every rule of SPEC 8.4 is a receipt: a
//!   review that records a baseline Section 8.5 does not select at its own
//!   candidate stays ordinary history, and a history context no marker may
//!   depend on is one typed refusal, never silence
//! - a request ID reused under a different fingerprint conflicts with
//!   `REQUEST_ID_CONFLICT`, and request IDs stay scoped to their operation
//!   kind, so a restore receipt never answers a review request
//! - a scan that cannot complete blocks instead of claiming the request
//!   absent, and rewritten history ends the deduplication promise while
//!   the expected-candidate guards still apply
//!
//! Receipts enter history through the shared marker writer, and the oracles
//! stay independent of the implementation (SPEC 16.3): commits and trees
//! come from `git rev-parse`, `git write-tree`, and `git log`, and the
//! repository state from `oracle::snapshot`.

mod support;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use margin::app::{
    CommitEffect, DiffRequest, Effects, Identities, InitRequest, OperationOutcome, Operations,
    ReceiptLookup, ReceiptScanError, ReviewDecision, ReviewOutcome, ReviewRequest, commit_marker,
    find_receipt,
};
use margin::domain::{
    CommitKind, CommitMetadata, DecisionMode, PlanIdentity, RestoreSource, ReviewPlan, ScopeInput,
};
use margin::git::{Git, Limits};
use serde_json::{Value, json};
use support::env::{IsolatedEnv, is_scrubbed};
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
/// one out-of-scope note that changed between them.
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
    initialize_with(repo, scope())
}

/// Initializes one explicit scope and returns the start marker.
fn initialize_with(repo: &TestRepo, scope: ScopeInput) -> String {
    let git = adapter_for(repo);
    let operations = Operations::new(&git);
    let result = operations.initialize_scope(&InitRequest {
        scope: Some(scope),
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

/// The pinned comparison of the fixture: two change groups, one per
/// passage, with the identity fields a plan pins to.
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
    assert_eq!(data.scope_totals.groups, 2, "two passages are pending");
    data
}

/// One complete keep-all-or-restore plan document for `pinned`.
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

/// Parses one plan document or panics with the refusal.
fn parsed_plan(plan: &str) -> ReviewPlan {
    ReviewPlan::parse(plan.as_bytes())
        .unwrap_or_else(|problem| panic!("the plan must parse: {problem}"))
}

/// The plan fingerprint of one plan document, through the shared identities
/// (SPEC 7.3, 8.6).
fn fingerprint_of(repo: &TestRepo, plan: &str) -> String {
    let parsed = parsed_plan(plan);
    Identities::new(&adapter_for(repo))
        .unwrap_or_else(|error| panic!("identities must read the repository: {error}"))
        .plan(&parsed.identity())
        .unwrap_or_else(|error| panic!("the plan must hash: {error}"))
        .id()
}

/// A restore-plan fingerprint stand-in for `request`, so restore receipts
/// carry the same identity shape the review plans do.
fn restore_fingerprint(
    repo: &TestRepo,
    pinned: &ReviewOutcome,
    request: &str,
    message: &str,
) -> String {
    Identities::new(&adapter_for(repo))
        .unwrap_or_else(|error| panic!("identities must read the repository: {error}"))
        .plan(&PlanIdentity {
            operation: "restore".to_string(),
            request_id: request.to_string(),
            review_id: pinned.review_id.clone(),
            branch_ref: pinned.branch_ref.clone(),
            scope_id: pinned.scope_id.clone(),
            baseline_oid: pinned.baseline_oid.clone(),
            candidate_oid: pinned.candidate_oid.clone(),
            restore: Vec::new(),
            decision_mode: DecisionMode::KeepRemaining,
            message: message.to_string(),
        })
        .unwrap_or_else(|error| panic!("the plan must hash: {error}"))
        .id()
}

/// Commits one verified review receipt for `plan` on the clean candidate
/// index: the empty keep-all commit of SPEC 8.1, carrying the request and
/// the plan fingerprint of the document a retry will resubmit.
fn review_receipt(
    repo: &TestRepo,
    pinned: &ReviewOutcome,
    plan: &str,
    label: &str,
) -> margin::app::ValidMarker {
    let parent = text(&repo.git_ok(["rev-parse", "HEAD"]));
    let tree = text(&repo.git_ok(["write-tree"]));
    let parsed = parsed_plan(plan);
    let metadata = CommitMetadata {
        kind: CommitKind::Review,
        scope_digest: pinned.scope_id.clone(),
        scope_spec: String::from_utf8(pinned.scope.canonical_bytes()).unwrap(),
        candidate: parent,
        result_tree: tree,
        baseline: Some(pinned.baseline_oid.clone()),
        request: Some(parsed.request_id.clone()),
        plan: Some(fingerprint_of(repo, plan)),
        source: None,
    };
    commit_marker(&adapter_for(repo), &metadata, label)
        .unwrap_or_else(|error| panic!("the review receipt must verify: {error}"))
}

/// Commits one verified restore receipt of `request` from `revision`.
fn restore_receipt(
    repo: &TestRepo,
    pinned: &ReviewOutcome,
    request: &str,
    fingerprint: &str,
    revision: &str,
    label: &str,
) -> margin::app::ValidMarker {
    let parent = text(&repo.git_ok(["rev-parse", "HEAD"]));
    let tree = text(&repo.git_ok(["write-tree"]));
    let metadata = CommitMetadata {
        kind: CommitKind::Restore,
        scope_digest: pinned.scope_id.clone(),
        scope_spec: String::from_utf8(pinned.scope.canonical_bytes()).unwrap(),
        candidate: parent,
        result_tree: tree,
        baseline: None,
        request: Some(request.to_string()),
        plan: Some(fingerprint.to_string()),
        source: Some(RestoreSource {
            path: "doc-a.md".to_string(),
            revision: revision.to_string(),
        }),
    };
    commit_marker(&adapter_for(repo), &metadata, label)
        .unwrap_or_else(|error| panic!("the restore receipt must verify: {error}"))
}

/// Runs one dry run of the plan and returns the typed outcome.
fn decide(repo: &TestRepo, plan: &str) -> OperationOutcome<ReviewDecision> {
    apply_dry(repo, plan, true)
}

/// Runs the plan through the application with the dry-run flag of the
/// request.
fn apply_dry(repo: &TestRepo, plan: &str, dry_run: bool) -> OperationOutcome<ReviewDecision> {
    let git = adapter_for(repo);
    let operations = Operations::new(&git);
    operations.apply_review(&ReviewRequest {
        scope: None,
        plan: plan.as_bytes().to_vec(),
        dry_run,
    })
}

// ---------------------------------------------------------------------------
// Present-day scope roots that block new mutations
// ---------------------------------------------------------------------------

/// The seeded workspace of one sub-directory scope: the scope root is the
/// directory `drafts`, so a retry case can replace that one root — with a
/// symlink, with absence, or with an unreadable mode — while the rest of
/// the repository, the out-of-scope note, and the history stay untouched.
struct ScopedFixture {
    repo: TestRepo,
    /// The start marker the first-parent walk anchors at: B.
    start: String,
    /// The committed candidate: C.
    candidate: String,
}

/// The scope of [`scoped_fixture`]: the one root directory `drafts`.
fn drafts_scope() -> ScopeInput {
    ScopeInput {
        include: vec!["drafts".to_string()],
        exclude: Vec::new(),
        suffixes: vec![".md".to_string()],
    }
}

fn scoped_fixture() -> ScopedFixture {
    let repo = TestRepo::init();
    repo.write_file(
        "drafts/doc-a.md",
        document("first original passage", "second original passage").as_bytes(),
    );
    repo.write_file("note.txt", b"the baseline note\n");
    let start = initialize_with(&repo, drafts_scope());
    repo.write_file(
        "drafts/doc-a.md",
        document(
            "first revised passage\nwith two lines",
            "second revised passage",
        )
        .as_bytes(),
    );
    repo.write_file("note.txt", b"the candidate note\n");
    let candidate = repo.commit_all("writing pass");
    ScopedFixture {
        repo,
        start,
        candidate,
    }
}

/// An exact retry returns its verified receipt even when the present-day
/// scope root cannot be resolved for a new mutation: the receipt response
/// is built from the recorded canonical scope and immutable Git objects, so
/// no working path is traversed and a new symlink is never followed
/// (SPEC 8.7, 9.1 step 3; the reported defect of task T060).
#[test]
fn an_exact_retry_survives_a_scope_root_that_blocks_new_mutations() {
    let fixture = scoped_fixture();
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-keep-all-01", &[], None);
    let receipt = review_receipt(&fixture.repo, &pinned, &plan, "keep every change");
    // A fresh request under the same comparison: whatever blocks it, the
    // retry of the completed request must stay unaffected.
    let fresh = plan_document(&pinned, "review-fresh-02", &[], None);
    let root = fixture.repo.file_path("drafts");

    // Both phases of the request answer with the original receipt.
    let retries = |what: &str| {
        let dry = decide(&fixture.repo, &plan);
        assert!(dry.ok(), "{what}: the dry run validates: {:?}", dry.error);
        let data = dry.data.as_ref().unwrap();
        assert_eq!(
            data.existing_receipt.as_deref(),
            Some(receipt.commit.as_str()),
            "{what}: the dry run names the receipt"
        );
        assert_eq!(
            data.candidate_oid, fixture.candidate,
            "{what}: the receipt's own candidate, not a live comparison"
        );
        assert_eq!(
            data.kept, None,
            "{what}: the historical group count is not reconstructed"
        );
        let applied = apply_dry(&fixture.repo, &plan, false);
        assert!(
            applied.ok(),
            "{what}: the application acknowledges the receipt: {:?}",
            applied.error
        );
        let data = applied.data.as_ref().unwrap();
        assert_eq!(
            data.existing_receipt.as_deref(),
            Some(receipt.commit.as_str()),
            "{what}: the application names the receipt"
        );
        assert_eq!(
            applied.effects.commit,
            CommitEffect::ExistingReceipt,
            "{what}: the receipt is the commit observation"
        );
    };

    // A symlink replaces the scope root: a new mutation cannot even adopt
    // the scope, but the receipt still answers.
    fs::remove_dir_all(&root).expect("the root directory leaves");
    symlink("nowhere", &root).expect("the scope root becomes a symlink");
    assert_refusal(
        &decide(&fixture.repo, &fresh),
        "INVALID_ARGUMENT",
        "a fresh request under a symlinked scope root",
    );
    let before = oracle::snapshot(&fixture.repo);
    retries("a symlinked scope root");
    before.assert_unchanged(
        &oracle::snapshot(&fixture.repo),
        "a receipt retry under a symlinked root",
    );

    // The installed executable behaves the same way in both modes.
    let path = plan_file(&fixture.repo, "plan.json", &plan);
    let output = margin(
        fixture.repo.env(),
        fixture.repo.path(),
        &["review", "--input", &path, "--dry-run", "--json"],
    );
    let envelope = envelope_of(&output, "a dry run under a symlinked root");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["data"]["existing_receipt"], json!(receipt.commit));
    assert_eq!(envelope["data"]["kept"], Value::Null);
    assert_eq!(envelope["effects"]["commit"], "none");
    let output = margin(
        fixture.repo.env(),
        fixture.repo.path(),
        &["review", "--input", &path, "--json"],
    );
    let envelope = envelope_of(&output, "an application under a symlinked root");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["data"]["existing_receipt"], json!(receipt.commit));
    assert_eq!(envelope["effects"]["commit"], "existing_receipt");
    assert_eq!(envelope["effects"]["working_tree"], "unchanged");
    before.assert_unchanged(
        &oracle::snapshot(&fixture.repo),
        "the executable's receipt retries",
    );

    // The root disappears: tracked deletions keep the scope itself
    // resolvable, so the fresh request now refuses on the moved comparison
    // instead — the receipt it produced anchored the baseline — while the
    // completed request still answers.
    fs::remove_file(&root).expect("the symlink leaves");
    fixture
        .repo
        .git_ok(["checkout", "--quiet", "HEAD", "--", "drafts"]);
    fs::remove_dir_all(&root).expect("the restored root leaves");
    assert_refusal(
        &decide(&fixture.repo, &fresh),
        "STALE_REVIEW",
        "a fresh request over a vanished scope root",
    );
    let before = oracle::snapshot(&fixture.repo);
    retries("a vanished scope root");
    before.assert_unchanged(
        &oracle::snapshot(&fixture.repo),
        "a receipt retry over a vanished root",
    );

    // The root becomes unreadable: resolution of a new mutation fails on
    // input and output, and the receipt still answers.
    fixture
        .repo
        .git_ok(["checkout", "--quiet", "HEAD", "--", "drafts"]);
    let before = oracle::snapshot(&fixture.repo);
    fs::set_permissions(&root, fs::Permissions::from_mode(0o000))
        .expect("the root becomes unreadable");
    assert_refusal(
        &decide(&fixture.repo, &fresh),
        "IO_FAILED",
        "a fresh request under an unreadable scope root",
    );
    retries("an unreadable scope root");
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755))
        .expect("the root becomes readable again");
    before.assert_unchanged(
        &oracle::snapshot(&fixture.repo),
        "a receipt retry over an unreadable root",
    );
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

// ---------------------------------------------------------------------------
// Exact retries
// ---------------------------------------------------------------------------

/// An exact retry returns the original verified commit, with the decision
/// data reconstructed from the receipt's own comparison (SPEC 8.7).
#[test]
fn an_exact_retry_returns_the_original_verified_commit() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-keep-all-01", &[], None);
    let receipt = review_receipt(&fixture.repo, &pinned, &plan, "keep every change");

    // The writer kept writing after the review completed, so current HEAD
    // advanced past both the candidate and the receipt. The receipt stays
    // reachable first-parent history (SPEC 8.7).
    fixture
        .repo
        .write_file("doc-a.md", b"later uncaptured words\n");
    fixture.repo.commit_all("later writing");
    let before = oracle::snapshot(&fixture.repo);

    let outcome = decide(&fixture.repo, &plan);
    assert!(
        outcome.ok(),
        "the exact retry validates: {:?}",
        outcome.error
    );
    let data = outcome.data.as_ref().unwrap();
    assert_eq!(
        data.existing_receipt.as_deref(),
        Some(receipt.commit.as_str()),
        "the retry returns the original verified commit"
    );
    assert_eq!(data.request_id, "review-keep-all-01");
    assert_eq!(data.candidate_oid, fixture.candidate);
    assert_eq!(data.baseline_oid, fixture.start);
    // The reported intended tree is the tree the receipt commit actually
    // holds, read back through stock Git.
    assert_eq!(
        data.expected_tree,
        oracle::commit_tree(&fixture.repo, &receipt.commit),
        "the receipt's verified result tree"
    );
    assert_eq!(
        data.result_scope_digest, pinned.candidate_scope_digest,
        "keep-all kept the candidate's scoped content"
    );
    // The receipt response does not rerun the grouping engine over
    // history, so the kept count is unknown, not a fabricated number
    // (SPEC 8.7): the decisions the fingerprint pinned still report.
    assert_eq!(data.kept, None, "the historical group count stays unknown");
    assert_eq!(data.restored, Vec::<String>::new());
    assert_eq!(data.paths, Vec::<String>::new());
    assert_eq!(outcome.effects, Effects::unchanged());
    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "an exact retry");
}

/// The lookup precedes every present-day stale check, so uncaptured bytes,
/// staged entries, a foreign branch, and a storage transformation never
/// hide an already validated receipt (SPEC 8.7, 9.1 step 3).
#[test]
fn present_day_blockers_never_hide_the_receipt() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-keep-all-01", &[], None);
    let receipt = review_receipt(&fixture.repo, &pinned, &plan, "keep every change");
    let still_returns = |what: &str| {
        let outcome = decide(&fixture.repo, &plan);
        assert!(outcome.ok(), "{what}: {:?}", outcome.error);
        assert_eq!(
            outcome.data.as_ref().unwrap().existing_receipt.as_deref(),
            Some(receipt.commit.as_str()),
            "{what}: the receipt still answers"
        );
        assert_eq!(outcome.effects, Effects::unchanged(), "{what}");
    };

    // Uncaptured scoped bytes normally refuse a review (SPEC 9.3); the
    // receipt returns before that check can refuse.
    fixture
        .repo
        .write_file("doc-a.md", b"edited while retrying\n");
    let dirty = oracle::snapshot(&fixture.repo);
    still_returns("uncaptured scoped content");
    dirty.assert_unchanged(&oracle::snapshot(&fixture.repo), "a receipt retry");

    // A staged out-of-scope entry blocks a mutation through the real index;
    // it does not hide the receipt.
    fixture
        .repo
        .write_file("note.txt", b"staged candidate note\n");
    fixture.repo.git_ok(["add", "note.txt"]);
    let staged = oracle::snapshot(&fixture.repo);
    still_returns("a staged entry");
    staged.assert_unchanged(&oracle::snapshot(&fixture.repo), "a receipt retry");

    // A branch other than the adopted one, even at the same commit.
    fixture
        .repo
        .git_ok(["switch", "--quiet", "--create", "other"]);
    still_returns("a foreign branch");

    // A storage transformation of the identity profile (SPEC 8.8).
    fixture.repo.set_local_config("core.autocrlf", "true");
    still_returns("a storage transformation");
}

// ---------------------------------------------------------------------------
// Marker rules a receipt must satisfy
// ---------------------------------------------------------------------------

/// A review receipt that records a baseline Section 8.5 does not select at
/// its own candidate violates SPEC 8.4 rule 6: the marker stays ordinary
/// history, so it neither answers an exact retry nor conflicts with one,
/// whatever fingerprint it carries (SPEC 8.7).
#[test]
fn a_review_receipt_with_an_incorrect_recorded_baseline_is_no_receipt() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-keep-all-01", &[], None);
    let fingerprint = fingerprint_of(&fixture.repo, &plan);

    // One forged receipt: every structural rule holds — one parent, the
    // parent's own tree, the matching scope digest — but `Margin-Baseline`
    // names the candidate instead of the start marker the first-parent
    // walk selects at it.
    let parent = text(&fixture.repo.git_ok(["rev-parse", "HEAD"]));
    let tree = text(&fixture.repo.git_ok(["write-tree"]));
    let metadata = CommitMetadata {
        kind: CommitKind::Review,
        scope_digest: pinned.scope_id.clone(),
        scope_spec: String::from_utf8(pinned.scope.canonical_bytes()).unwrap(),
        candidate: parent.clone(),
        result_tree: tree,
        baseline: Some(fixture.candidate.clone()),
        request: Some("review-keep-all-01".to_string()),
        plan: Some(fingerprint.clone()),
        source: None,
    };
    let forged = commit_marker(&adapter_for(&fixture.repo), &metadata, "forged baseline")
        .unwrap_or_else(|error| panic!("the forged marker still commits: {error}"));

    // The scan validates whole markers: the forged review confers nothing,
    // so the request ID stays unused however the fingerprints compare.
    let git = adapter_for(&fixture.repo);
    let head = text(&fixture.repo.git_ok(["rev-parse", "HEAD"]));
    assert!(
        matches!(
            find_receipt(
                &git,
                &head,
                CommitKind::Review,
                "review-keep-all-01",
                &fingerprint
            ),
            Ok(ReceiptLookup::Absent)
        ),
        "the rule-6 marker is no receipt"
    );
    let other = format!("{}:other", fingerprint);
    assert!(
        matches!(
            find_receipt(
                &git,
                &head,
                CommitKind::Review,
                "review-keep-all-01",
                &other
            ),
            Ok(ReceiptLookup::Absent)
        ),
        "the rule-6 marker never conflicts either"
    );

    // End to end the retry runs as a request without a receipt: the pinned
    // comparison moved to the forged commit, so the expected-candidate
    // guard refuses instead of a second review.
    let before = oracle::snapshot(&fixture.repo);
    let outcome = decide(&fixture.repo, &plan);
    assert_refusal(&outcome, "STALE_REVIEW", "a retry over a rule-6 marker");
    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "the refused retry");
    assert!(
        outcome
            .data
            .and_then(|data| data.existing_receipt)
            .is_none(),
        "no receipt is invented for {forged:?}"
    );
}

/// A history context no marker may depend on — grafts here — is one typed
/// unavailable outcome, distinct from an absent receipt: the scan cannot
/// validate any marker, so it blocks instead of letting the request run
/// again (SPEC 8.4 rule 7, 8.7).
#[test]
fn an_unsupported_history_context_blocks_instead_of_claiming_absence() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-keep-all-01", &[], None);
    let fingerprint = fingerprint_of(&fixture.repo, &plan);
    let receipt = review_receipt(&fixture.repo, &pinned, &plan, "keep every change");

    // One no-op graft: it restates the receipt's real first parent, so the
    // history reads exactly as before, but every marker now depends on the
    // graft file (SPEC 8.4 rule 7).
    let info = fixture.repo.path().join(".git").join("info");
    fs::create_dir_all(&info).expect("the info directory exists");
    fs::write(
        info.join("grafts"),
        format!("{} {}\n", receipt.commit, fixture.candidate),
    )
    .expect("the graft file is written");

    // The scan reports the unusable context, never absence: the very
    // receipt it found a moment ago cannot be validated under the graft.
    let git = adapter_for(&fixture.repo);
    let head = text(&fixture.repo.git_ok(["rev-parse", "HEAD"]));
    match find_receipt(
        &git,
        &head,
        CommitKind::Review,
        "review-keep-all-01",
        &fingerprint,
    ) {
        Err(error @ ReceiptScanError::UnavailableContext { .. }) => {
            assert_eq!(error.public_code(), "BASELINE_UNAVAILABLE");
            assert!(!error.retryable());
        }
        other => panic!("the unusable context is not the receipt's absence: {other:?}"),
    }

    // End to end the request stays blocked: the receipt cannot return, and
    // a second receipt-producing mutation must not start.
    let before = oracle::snapshot(&fixture.repo);
    let outcome = decide(&fixture.repo, &plan);
    assert_refusal(
        &outcome,
        "BASELINE_UNAVAILABLE",
        "a scan over grafted history",
    );
    let error = outcome.error.as_ref().unwrap();
    assert!(
        error.message.contains("review-keep-all-01"),
        "the refusal names the request: {}",
        error.message
    );
    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "the blocked scan");

    // The installed executable reports the same class 4 refusal.
    let path = plan_file(&fixture.repo, "plan.json", &plan);
    let (exit, code, envelope) = refusal(
        &fixture.repo,
        &["review", "--input", &path, "--dry-run", "--json"],
    );
    assert_eq!(exit, 4);
    assert_eq!(code, "BASELINE_UNAVAILABLE");
    assert_eq!(envelope["effects"]["commit"], "none");
}

// ---------------------------------------------------------------------------
// Conflicts and kind scoping
// ---------------------------------------------------------------------------

/// A request ID reused under a different fingerprint conflicts, whatever
/// the difference is (SPEC 7.3, 8.7).
#[test]
fn a_reused_request_id_with_another_plan_conflicts() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let original = plan_document(&pinned, "review-decision-42", &[], None);
    let receipt = review_receipt(&fixture.repo, &pinned, &original, "keep every change");

    // The same request ID with another effective message names another
    // plan, because the fingerprint covers the message.
    let reworded = plan_document(&pinned, "review-decision-42", &[], Some("another intent"));
    let before = oracle::snapshot(&fixture.repo);
    let outcome = decide(&fixture.repo, &reworded);
    assert_refusal(&outcome, "REQUEST_ID_CONFLICT", "a reworded plan");
    let error = outcome.error.as_ref().unwrap();
    assert!(
        error.message.contains(&receipt.commit),
        "the refusal names the receipt: {}",
        error.message
    );
    assert!(
        error.message.contains("review-decision-42"),
        "the refusal names the request: {}",
        error.message
    );
    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "a refused conflict");

    // The same request ID with other decisions names another plan too.
    let redecided = plan_document(
        &pinned,
        "review-decision-42",
        &[pinned.changes[0].change_id.clone()],
        None,
    );
    let outcome = decide(&fixture.repo, &redecided);
    assert_refusal(&outcome, "REQUEST_ID_CONFLICT", "a redecided plan");

    // Through the executable: conflicting context is exit class 3.
    let plan = plan_file(&fixture.repo, "conflict.json", &reworded);
    let (exit, code, envelope) = refusal(
        &fixture.repo,
        &["review", "--input", &plan, "--dry-run", "--json"],
    );
    assert_eq!(exit, 3);
    assert_eq!(code, "REQUEST_ID_CONFLICT");
    assert_eq!(envelope["effects"]["working_tree"], "unchanged");
    assert_eq!(envelope["effects"]["commit"], "none");
}

/// Request IDs stay scoped to their operation kind: a restore receipt
/// answers restore requests, never review requests of the same ID
/// (SPEC 7.3, 8.7).
#[test]
fn request_ids_stay_scoped_to_their_operation_kind() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let fingerprint =
        restore_fingerprint(&fixture.repo, &pinned, "shared-01", "restore the opening");
    let receipt = restore_receipt(
        &fixture.repo,
        &pinned,
        "shared-01",
        &fingerprint,
        &fixture.start,
        "restore the opening",
    );
    let git = adapter_for(&fixture.repo);
    let head = text(&fixture.repo.git_ok(["rev-parse", "HEAD"]));

    // The restore receipt answers its own request and fingerprint.
    assert!(
        matches!(
            find_receipt(&git, &head, CommitKind::Restore, "shared-01", &fingerprint),
            Ok(ReceiptLookup::Found(marker)) if marker.commit == receipt.commit
        ),
        "the restore receipt is findable"
    );
    // The same request with another fingerprint conflicts.
    let other = restore_fingerprint(&fixture.repo, &pinned, "shared-01", "another restoration");
    match find_receipt(&git, &head, CommitKind::Restore, "shared-01", &other) {
        Ok(ReceiptLookup::Conflict {
            commit,
            recorded,
            requested,
        }) => {
            assert_eq!(commit, receipt.commit);
            assert_eq!(recorded.as_deref(), Some(fingerprint.as_str()));
            assert_eq!(requested, other);
        }
        other => panic!("the reused identifier must conflict: {other:?}"),
    }
    // A review request of the same ID is a different request: the restore
    // receipt neither answers it nor conflicts with it.
    assert!(
        matches!(
            find_receipt(&git, &head, CommitKind::Review, "shared-01", &fingerprint),
            Ok(ReceiptLookup::Absent)
        ),
        "the restore receipt never answers a review request"
    );

    // End to end, the review request runs its own course: the restore
    // receipt advanced the candidate, so the plan that still pins the old
    // comparison is stale, never a receipt and never a conflict.
    let plan = plan_document(&pinned, "shared-01", &[], None);
    let outcome = decide(&fixture.repo, &plan);
    assert_refusal(&outcome, "STALE_REVIEW", "a review request after a restore");
}

// ---------------------------------------------------------------------------
// Incomplete scans and rewritten history
// ---------------------------------------------------------------------------

/// A scan that cannot complete blocks instead of claiming the request
/// absent, with or without a receipt in the history (SPEC 8.7).
#[test]
fn an_incomplete_scan_blocks_instead_of_claiming_absence() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-keep-all-01", &[], None);
    let fingerprint = fingerprint_of(&fixture.repo, &plan);
    let head = text(&fixture.repo.git_ok(["rev-parse", "HEAD"]));

    // One capture bound the walk cannot fit: this history holds two
    // first-parent commits, so the listing overflows long before it ends.
    let limited = Git::from_env(fixture.repo.env().vars().clone())
        .at(fixture.repo.path())
        .with_limits(Limits {
            timeout: Duration::from_millis(10_000),
            max_output_bytes: 60,
        });
    let error = find_receipt(
        &limited,
        &head,
        CommitKind::Review,
        "review-keep-all-01",
        &fingerprint,
    )
    .expect_err("the truncated walk cannot establish absence");
    assert_eq!(error.public_code(), "LIMIT_EXCEEDED");
    assert!(!error.retryable());

    // The request path fails the same way instead of proceeding: a new
    // receipt-producing mutation stays blocked.
    let operations = Operations::new(&limited);
    let request = || ReviewRequest {
        scope: None,
        plan: plan.as_bytes().to_vec(),
        dry_run: true,
    };
    let outcome = operations.validate_review(&request());
    assert_refusal(&outcome, "LIMIT_EXCEEDED", "an incomplete scan");
    let error = outcome.error.as_ref().unwrap();
    assert!(
        error.message.contains("receipt scan"),
        "the refusal says what could not complete: {}",
        error.message
    );
    assert!(
        error.message.contains("review-keep-all-01"),
        "the refusal names the request: {}",
        error.message
    );

    // A receipt that exists is no help either: the scan still cannot
    // complete, so it cannot return what it never finished reading.
    let _receipt = review_receipt(&fixture.repo, &pinned, &plan, "keep every change");
    let outcome = operations.validate_review(&request());
    assert_refusal(
        &outcome,
        "LIMIT_EXCEEDED",
        "an incomplete scan over a receipt",
    );
}

/// Rewritten history ends the deduplication promise while the
/// expected-candidate guards still apply (SPEC 8.7).
#[test]
fn rewritten_history_no_longer_promises_deduplication() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-keep-all-01", &[], None);
    let fingerprint = fingerprint_of(&fixture.repo, &plan);
    let _receipt = review_receipt(&fixture.repo, &pinned, &plan, "keep every change");
    let git = adapter_for(&fixture.repo);

    // A reset drops the receipt from the adopted branch: the object still
    // exists, but it is no longer reachable, so no receipt answers.
    fixture
        .repo
        .git_ok(["reset", "--quiet", "--hard", &fixture.candidate]);
    let head = text(&fixture.repo.git_ok(["rev-parse", "HEAD"]));
    assert!(
        matches!(
            find_receipt(
                &git,
                &head,
                CommitKind::Review,
                "review-keep-all-01",
                &fingerprint
            ),
            Ok(ReceiptLookup::Absent)
        ),
        "the unreachable receipt is no longer promised"
    );

    // The retry executes as a new request: the plan still pins this
    // comparison, no receipt answers it, and the reset left a clean tree.
    let outcome = decide(&fixture.repo, &plan);
    assert!(
        outcome.ok(),
        "the request executes again: {:?}",
        outcome.error
    );
    assert_eq!(
        outcome.data.as_ref().unwrap().existing_receipt,
        None,
        "no receipt is invented for rewritten history"
    );

    // Once the candidate moves as well, the expected-candidate guards
    // apply exactly as without any receipt.
    fixture.repo.write_file("doc-a.md", b"rewritten opening\n");
    fixture.repo.commit_all("another writing pass");
    let outcome = decide(&fixture.repo, &plan);
    assert_refusal(
        &outcome,
        "STALE_REVIEW",
        "a moved candidate after rewriting",
    );
}

// ---------------------------------------------------------------------------
// The search domain
// ---------------------------------------------------------------------------

/// The scan covers the complete unfiltered first-parent history: an empty
/// review receipt stays findable where stock Git's path-limited log hides
/// it (SPEC 8.5 note, 8.7).
#[test]
fn the_scan_sees_receipts_a_path_limited_log_hides() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-keep-all-01", &[], None);
    let fingerprint = fingerprint_of(&fixture.repo, &plan);
    let receipt = review_receipt(&fixture.repo, &pinned, &plan, "keep every change");
    let head = text(&fixture.repo.git_ok(["rev-parse", "HEAD"]));

    let path_history: Vec<String> =
        text(
            &fixture
                .repo
                .git_ok(["log", "--first-parent", "--format=%H", "--", "doc-a.md"]),
        )
        .lines()
        .map(str::to_string)
        .collect();
    assert!(
        path_history.contains(&fixture.candidate),
        "the path log lists the candidate"
    );
    assert!(
        !path_history.contains(&receipt.commit),
        "the path log drops the empty receipt commit"
    );

    let git = adapter_for(&fixture.repo);
    assert!(
        matches!(
            find_receipt(&git, &head, CommitKind::Review, "review-keep-all-01", &fingerprint),
            Ok(ReceiptLookup::Found(marker)) if marker.commit == receipt.commit
        ),
        "the unfiltered walk finds the receipt"
    );
}

/// Both documented object formats find their receipts, with the object
/// format inside the fingerprint (SPEC 8.6, 16.5).
#[test]
fn exact_retries_work_in_sha256_repositories() {
    let fixture = fixture(RepoOptions::new().with_object_format(ObjectFormat::Sha256));
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-keep-all-01", &[], None);
    let fingerprint = fingerprint_of(&fixture.repo, &plan);
    assert!(
        fingerprint.starts_with("sha256:"),
        "the fingerprint names its object format: {fingerprint}"
    );
    let receipt = review_receipt(&fixture.repo, &pinned, &plan, "keep every change");
    assert_eq!(receipt.commit.len(), 64, "a SHA-256 object ID");

    fixture
        .repo
        .write_file("doc-a.md", b"later uncaptured words\n");
    fixture.repo.commit_all("later writing");
    let outcome = decide(&fixture.repo, &plan);
    assert!(
        outcome.ok(),
        "the exact retry validates: {:?}",
        outcome.error
    );
    assert_eq!(
        outcome.data.as_ref().unwrap().existing_receipt.as_deref(),
        Some(receipt.commit.as_str())
    );
}

// ---------------------------------------------------------------------------
// Executable helpers
// ---------------------------------------------------------------------------

/// Writes one complete plan document beside the repository, so no plan
/// file ever joins the working tree.
fn plan_file(repo: &TestRepo, name: &str, document: &str) -> String {
    let path = repo.root().join(name);
    fs::write(&path, document)
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", path.display()));
    path.to_str().unwrap().to_string()
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

/// Parses the one JSON envelope of a machine-mode run (SPEC 7.1).
fn envelope_of(output: &Output, what: &str) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let envelope: Value = serde_json::from_str(stdout.trim_end())
        .unwrap_or_else(|error| panic!("{what}: the envelope must parse: {error}: {stdout}"));
    assert_eq!(envelope["schema_version"], 1, "{what}");
    envelope
}

/// The exit class, error code, and envelope of one refused run.
fn refusal(repo: &TestRepo, args: &[&str]) -> (i32, String, Value) {
    let output = margin(repo.env(), repo.path(), args);
    let envelope = envelope_of(&output, args.last().copied().unwrap_or("review"));
    let code = envelope["error"]["code"].as_str().unwrap_or("").to_string();
    (
        output
            .status
            .code()
            .unwrap_or_else(|| panic!("the binary died from a signal")),
        code,
        envelope,
    )
}

/// The installed executable reports the receipt of an exact retry in one
/// JSON envelope and one text line (SPEC 6.4, 7.1, 8.7).
#[test]
fn the_executable_reports_the_existing_receipt() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = plan_document(&pinned, "review-keep-all-01", &[], None);
    let receipt = review_receipt(&fixture.repo, &pinned, &plan, "keep every change");
    let path = plan_file(&fixture.repo, "plan.json", &plan);
    let before = oracle::snapshot(&fixture.repo);

    // The retry reaches the executable with HEAD already advanced to the
    // receipt commit itself: the pinned comparison of the plan is the
    // parent, so only the receipt keeps this request from a stale refusal.
    let output = margin(
        fixture.repo.env(),
        fixture.repo.path(),
        &["review", "--input", &path, "--dry-run", "--json"],
    );
    let envelope = envelope_of(&output, "an exact retry through the executable");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(envelope["command"], "review");
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["data"]["request_id"], "review-keep-all-01");
    assert_eq!(envelope["data"]["existing_receipt"], json!(receipt.commit));
    assert_eq!(envelope["data"]["dry_run"], true);
    assert_eq!(envelope["effects"]["working_tree"], "unchanged");
    assert_eq!(envelope["effects"]["index"], "unchanged");
    assert_eq!(envelope["effects"]["commit"], "none");
    assert_eq!(envelope["effects"]["repository"], "unchanged");
    assert_eq!(envelope["effects"]["configuration"], "unchanged");
    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "a receipt retry");

    let output = margin(
        fixture.repo.env(),
        fixture.repo.path(),
        &["review", "--input", &path, "--dry-run"],
    );
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("margin:"), "one text line: {stdout}");
    assert!(
        stdout.contains(&receipt.commit),
        "the text line names the receipt: {stdout}"
    );
    assert!(stdout.ends_with('\n'));
}
