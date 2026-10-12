//! Real-Git tests that complete the behavior matrix of SPEC 16.5 (task
//! T040).
//!
//! The suites of the per-behavior tasks own the rows they exercise. This
//! file covers the cells no dedicated suite claims, each against the
//! installed Git with the independent oracles of SPEC 16.3:
//!
//! - repository policy under signing: repository-wide SSH signing signs
//!   every marker, the reserved trailers still parse, and stock Git
//!   verifies each signature; a signing program that fails stops the
//!   capture typed, retains the staged state exactly, and loses no writing
//!   once the repository is repaired
//! - stale state: a scope that changed after the review was pinned refuses
//!   the pinned plan as stale, and the old approval never crosses the new
//!   starting marker of another scope
//! - limits: one document beyond the per-file ceiling stays preserved and
//!   counted while its comparison refuses `LIMIT_EXCEEDED` and grants no
//!   approval, and one first-parent history of hundreds of ordinary
//!   commits leaves the baseline walk, the receipt scan, and the timeline
//!   correct and bounded
//! - removal: deleting the local Margin configuration through stock Git
//!   keeps every document version readable through stock Git alone, and one
//!   explicit re-adoption rebuilds the workspace without a second start
//!
//! Set `MARGIN_MATRIX_HISTORY_COMMITS` to raise the generated history size
//! of the limits case for a CI tier.

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use margin::app::{
    BaselineError, BaselineUnavailable, CheckpointRequest, CommitEffect, DiffRequest, Effects,
    InitRequest, InspectRequest, LogRequest, OperationOutcome, Operations, RecoveryRequest,
    RestoreRequest, ReviewOutcome, ReviewRequest, TimelineKind, WorkspaceInspection, resolve_scope,
    select_baseline, validate_marker,
};
use margin::domain::{CommitKind, PreservationState, ReviewState, ScopeInput};
use margin::git::Git;
use support::env::{FIXTURE_USER_EMAIL, IsolatedEnv, is_scrubbed};
use support::repo::TestRepo;
use support::{oracle, text};

/// Ordinary commits the long-history case generates by default.
const DEFAULT_HISTORY_COMMITS: usize = 250;

/// Largest page the `log` contract allows (SPEC 6.3).
const LOG_PAGE_LIMIT: u64 = 200;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Adapter bound to `repo`'s working tree and isolated environment.
fn adapter_for(repo: &TestRepo) -> Git {
    Git::from_env(repo.env().vars().clone()).at(repo.path())
}

/// Builds scope input from literal test values.
fn scope_input(include: &[&str], exclude: &[&str], suffixes: &[&str]) -> ScopeInput {
    ScopeInput {
        include: include.iter().map(|root| root.to_string()).collect(),
        exclude: exclude.iter().map(|root| root.to_string()).collect(),
        suffixes: suffixes.iter().map(|suffix| suffix.to_string()).collect(),
    }
}

/// One first-use initialization request for `scope`.
fn first_use(scope: &ScopeInput) -> InitRequest {
    InitRequest {
        scope: Some(scope.clone()),
        create_repository: false,
        branch: None,
        new_start: false,
        auto_checkpoint: None,
        capture_after_edit: None,
        editor_argv: None,
        dry_run: false,
    }
}

/// One checkpoint request for `scope` with an optional message.
fn checkpoint_of(scope: &ScopeInput, message: Option<&str>) -> CheckpointRequest {
    CheckpointRequest {
        scope: Some(scope.clone()),
        message: message.map(str::to_string),
        include_staged: false,
        expect_head: None,
        expect_index: None,
        expect_working: None,
        dry_run: false,
    }
}

/// Initializes the `notes` scope and returns the start marker.
fn initialize(repo: &TestRepo) -> String {
    let git = adapter_for(repo);
    let operations = Operations::new(&git);
    let result = operations.initialize_scope(&first_use(&scope_input(&["notes"], &[], &[".md"])));
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

/// Captures one checkpoint over the `notes` scope and returns the draft.
fn checkpoint(repo: &TestRepo, message: &str) -> String {
    let git = adapter_for(repo);
    let operations = Operations::new(&git);
    let result = operations.create_checkpoint(&checkpoint_of(
        &scope_input(&["notes"], &[], &[".md"]),
        Some(message),
    ));
    assert!(
        result.ok(),
        "the checkpoint must succeed: {:?}",
        result.error
    );
    result
        .data
        .and_then(|data| data.commit)
        .expect("a changed capture creates one draft")
}

/// The workspace status both interfaces share.
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

/// The pinned review of the `notes` scope.
fn pinned_review_of(repo: &TestRepo) -> ReviewOutcome {
    let git = adapter_for(repo);
    let operations = Operations::new(&git);
    let review = operations.build_review(&DiffRequest {
        scope: Some(scope_input(&["notes"], &[], &[".md"])),
        path_filter: Vec::new(),
        change: None,
        limit: None,
        cursor: None,
        working: false,
    });
    assert!(review.ok(), "the review must pin: {:?}", review.error);
    review.data.expect("a pinned review carries data")
}

/// One complete keep-all plan document for `pinned` (SPEC 7.3).
fn keep_all_plan(pinned: &ReviewOutcome, request: &str) -> String {
    serde_json::json!({
        "schema_version": 1,
        "request_id": request,
        "review_id": pinned.review_id,
        "branch_ref": pinned.branch_ref,
        "scope_id": pinned.scope_id,
        "baseline_oid": pinned.baseline_oid,
        "candidate_oid": pinned.candidate_oid,
        "restore": [],
        "keep_remaining": true,
    })
    .to_string()
}

/// Applies one plan document through the operation layer.
fn apply(
    repo: &TestRepo,
    plan: &str,
    dry_run: bool,
) -> OperationOutcome<margin::app::ReviewDecision> {
    let git = adapter_for(repo);
    let operations = Operations::new(&git);
    operations.apply_review(&ReviewRequest {
        scope: None,
        plan: plan.as_bytes().to_vec(),
        dry_run,
    })
}

/// Asserts one typed refusal: not successful, one primary code, and the
/// unchanged effects of an operation that wrote nothing.
fn assert_refusal<T>(outcome: &OperationOutcome<T>, code: &str, what: &str) {
    assert!(!outcome.ok(), "{what} must not succeed");
    let error = outcome.error.as_ref().unwrap();
    assert_eq!(error.code, code, "{what} reports the wrong code");
    assert_eq!(
        outcome.effects,
        Effects::unchanged(),
        "{what} wrote nothing"
    );
}

/// Runs the installed executable once in `cwd` under `env`.
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

/// Decodes the one JSON envelope of an executable run.
fn envelope_of(output: &Output, what: &str) -> serde_json::Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("cannot decode the envelope of {what}: {error}"))
}

/// The HEAD commit object ID.
fn head_of(repo: &TestRepo) -> String {
    text(&repo.git_ok(["rev-parse", "HEAD"]))
}

/// The commits of the unfiltered first-parent chain, newest first, exactly
/// as stock Git lists them.
fn first_parent_history(repo: &TestRepo) -> Vec<String> {
    String::from_utf8_lossy(&repo.git_ok(["rev-list", "--first-parent", "HEAD"]))
        .lines()
        .map(str::to_string)
        .collect()
}

/// The staged object ID of one path, from `git ls-files --stage`.
fn staged_oid(repo: &TestRepo, path: &str) -> Option<String> {
    oracle::ls_files(repo)
        .into_iter()
        .find(|entry| entry.path == path)
        .map(|entry| entry.oid)
}

// ---------------------------------------------------------------------------
// Repository policy: signed commits (SPEC 16.5, 3.1, 6.1)
// ---------------------------------------------------------------------------

/// Configures repository-wide SSH commit signing through Git's own
/// configuration, with one throwaway key inside the test-only home.
fn configure_ssh_signing(repo: &TestRepo) {
    let key = repo.home().join("signing-key");
    let status = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-q", "-f"])
        .arg(&key)
        .stdin(Stdio::null())
        .status()
        .unwrap_or_else(|error| panic!("cannot run ssh-keygen: {error}"));
    assert!(status.success(), "ssh-keygen must create the fixture key");
    let public = fs::read_to_string(key.with_extension("pub"))
        .unwrap_or_else(|error| panic!("cannot read the fixture public key: {error}"));
    let allowed = repo.home().join("allowed-signers");
    fs::write(&allowed, format!("{FIXTURE_USER_EMAIL} {public}"))
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", allowed.display()));
    repo.set_local_config("commit.gpgsign", "true");
    repo.set_local_config("gpg.format", "ssh");
    repo.set_local_config("user.signingkey", key.to_str().unwrap());
    repo.set_local_config("gpg.ssh.allowedSignersFile", allowed.to_str().unwrap());
}

/// Installs one signing program that always fails, through the repository
/// configuration Git itself honors.
fn install_failing_signer(repo: &TestRepo) {
    let failing = repo.home().join("failing-signer");
    fs::write(&failing, b"#!/bin/sh\nexit 3\n")
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", failing.display()));
    let metadata = fs::metadata(&failing)
        .unwrap_or_else(|error| panic!("cannot inspect {}: {error}", failing.display()));
    let mut permissions = metadata.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&failing, permissions)
        .unwrap_or_else(|error| panic!("cannot set the mode of {}: {error}", failing.display()));
    repo.set_local_config("commit.gpgsign", "true");
    repo.set_local_config("gpg.format", "openpgp");
    repo.set_local_config("gpg.openpgp.program", failing.to_str().unwrap());
}

/// A repository that signs every commit still carries valid Margin
/// markers, and stock Git verifies their signatures (SPEC 3.1 hooks and
/// signing, 8.3-8.4).
#[test]
fn signed_markers_validate_and_stock_git_verifies_them() {
    let repo = TestRepo::init();
    configure_ssh_signing(&repo);
    repo.write_file(
        "notes/a.md",
        b"# One\n\nfirst passage.\n\nsecond passage.\n",
    );
    repo.commit_all("import");
    let git = adapter_for(&repo);
    let scope = scope_input(&["notes"], &[], &[".md"]);

    // One ordinary writing pass under repository-wide signing.
    let start = initialize(&repo);
    repo.write_file(
        "notes/a.md",
        b"# One\n\nfirst revised.\n\nsecond passage.\n",
    );
    let draft = checkpoint(&repo, "revise the first passage");
    let pinned = pinned_review_of(&repo);
    let applied = apply(&repo, &keep_all_plan(&pinned, "signed-1"), false);
    assert!(applied.ok(), "the review must apply: {:?}", applied.error);
    let review = applied
        .data
        .and_then(|data| data.commit)
        .expect("the application creates one review commit");

    for (commit, kind) in [
        (&start, CommitKind::Start),
        (&draft, CommitKind::Draft),
        (&review, CommitKind::Review),
    ] {
        // The reserved trailers parse and the whole marker validates, with
        // the signature header in the object (SPEC 8.4).
        let marker = validate_marker(&git, commit)
            .unwrap_or_else(|error| panic!("the signed {kind} must validate: {error}"));
        assert_eq!(marker.kind, kind);
        assert_eq!(
            marker.candidate,
            text(&repo.git_ok(["rev-parse", &format!("{commit}^")]))
        );
        let object = repo.git_ok(["cat-file", "commit", commit]);
        assert!(
            object
                .windows(b"gpgsig ".len())
                .any(|window| window == b"gpgsig "),
            "the {kind} commit carries one signature header"
        );
        // Stock Git verifies the signature of the marker itself.
        let verified = repo.git(["verify-commit", commit]);
        assert!(
            verified.status.success(),
            "stock Git must verify the signed {kind}: {}",
            String::from_utf8_lossy(&verified.stderr)
        );
    }

    // The signed review anchors the scope, so approval works over signed
    // history exactly as over unsigned history (SPEC 8.5).
    let resolved = resolve_scope(&git, &scope).unwrap();
    assert_eq!(
        select_baseline(&git, &head_of(&repo), &resolved.digest)
            .expect("the signed review anchors the scope")
            .commit,
        review
    );
}

/// A signing program that fails stops the capture typed, reports the
/// observed state, retains the staged result exactly, and recovers through
/// the guarded capture once the repository is repaired (SPEC 6.1, 9.6-9.7).
#[test]
fn a_failing_signer_stops_typed_and_recovers() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n\nfirst passage.\n");
    repo.commit_all("import");
    let start = initialize(&repo);
    install_failing_signer(&repo);
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let scope = scope_input(&["notes"], &[], &[".md"]);

    // The capture cannot commit, so it fails typed with the exact effects
    // it observed: the staging happened, no commit exists (SPEC 9.6).
    repo.write_file("notes/a.md", b"# One\n\nfirst revised.\n");
    let failed = operations.create_checkpoint(&checkpoint_of(&scope, None));
    assert!(!failed.ok(), "a capture over a failing signer must fail");
    let error = failed.error.as_ref().unwrap();
    assert_eq!(error.code, "GIT_FAILED", "the signing failure is typed");
    assert_eq!(failed.effects.commit, CommitEffect::None);
    assert_eq!(failed.effects.index, margin::app::ObservedEffect::Changed);
    assert_eq!(
        failed.effects.working_tree,
        margin::app::ObservedEffect::Unchanged
    );
    let observed = failed.data.as_ref().expect("the failure reports its state");
    assert!(
        observed.changed,
        "the capture reports the content it staged"
    );
    assert_eq!(observed.commit, None, "no commit was created");

    // Stock Git proves the retained state: one staged entry that holds the
    // exact edited bytes, one commit of history, and the working file intact.
    let staged = staged_oid(&repo, "notes/a.md").expect("the edit stays staged");
    assert_eq!(
        oracle::blob(&repo, &staged),
        b"# One\n\nfirst revised.\n",
        "the staged blob holds the exact bytes"
    );
    assert_eq!(
        first_parent_history(&repo).len(),
        2,
        "only import and start exist"
    );
    assert_eq!(
        repo.read_file("notes/a.md"),
        b"# One\n\nfirst revised.\n",
        "the working file is untouched"
    );

    // The recovery inspection observes the staged state and offers the
    // guarded capture of SPEC 9.7.
    let report = operations.inspect_recovery(&RecoveryRequest { scope: None });
    assert!(report.ok(), "the doctor query succeeds: {:?}", report.error);
    let report = report.data.unwrap();
    assert_eq!(report.staged_paths.total, 1, "one staged path is reported");
    assert!(
        !report.recovery_argv.is_empty(),
        "the report offers one bounded recovery argv"
    );

    // Once the repository stops demanding the signature it cannot produce,
    // the ordinary checkpoint refuses the staged state, and the guarded
    // capture commits the retained writing as one draft.
    repo.git_ok(["config", "--unset", "commit.gpgsign"]);
    let retry = operations.create_checkpoint(&checkpoint_of(&scope, None));
    assert_refusal(
        &retry,
        "STAGED_CHANGES",
        "the ordinary checkpoint over staged state",
    );
    let recovery = operations.create_checkpoint(&CheckpointRequest {
        scope: None,
        message: None,
        include_staged: true,
        expect_head: report.head.clone(),
        expect_index: report.index_tree.clone(),
        expect_working: report
            .scoped_working_digest
            .as_ref()
            .map(|digest| digest.id()),
        dry_run: false,
    });
    assert!(
        recovery.ok(),
        "the guarded capture recovers: {:?}",
        recovery.error
    );
    let recovered = recovery
        .data
        .unwrap()
        .commit
        .expect("one draft is committed");
    assert_eq!(
        oracle::blob(
            &repo,
            &text(&repo.git_ok(["rev-parse", &format!("{recovered}:notes/a.md")]))
        ),
        b"# One\n\nfirst revised.\n",
        "the recovered draft holds the exact bytes"
    );

    // The failure granted no approval: the start marker still anchors, and
    // the recovered draft leaves the review pending (SPEC 8.4).
    let resolved = resolve_scope(&git, &scope).unwrap();
    assert_eq!(
        select_baseline(&git, &head_of(&repo), &resolved.digest)
            .expect("the start still anchors")
            .commit,
        start
    );
    let status = status_of(&repo);
    assert_eq!(status.review, ReviewState::Pending);
    assert_eq!(status.preservation, PreservationState::Captured);
}

// ---------------------------------------------------------------------------
// Stale state: the scope changed after the review (SPEC 16.5, 3.2 rule 7)
// ---------------------------------------------------------------------------

/// A plan pinned to a review the workspace no longer holds refuses stale,
/// and the approval of the old scope never crosses the new start (SPEC 3.2
/// rule 7, 8.2, 8.5).
#[test]
fn a_scope_changed_after_the_review_refuses_the_pinned_plan() {
    let repo = TestRepo::init();
    repo.write_file(
        "notes/a.md",
        b"# One\n\nfirst passage.\n\nsecond passage.\n",
    );
    repo.write_file("other/sentinel.txt", b"untouched\n");
    repo.commit_all("import");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let notes = scope_input(&["notes"], &[], &[".md"]);
    let other = scope_input(&["other"], &[], &[".md"]);

    let start = initialize(&repo);
    repo.write_file(
        "notes/a.md",
        b"# One\n\nfirst revised.\n\nsecond passage.\n",
    );
    repo.commit_all("writing pass");
    let pinned = pinned_review_of(&repo);
    assert_eq!(pinned.baseline_oid, start);
    let plan = keep_all_plan(&pinned, "scope-change-1");

    // The writer re-initializes another scope after the review was pinned.
    let changed = operations.initialize_scope(&first_use(&other));
    assert!(
        changed.ok(),
        "the changed scope reinitializes: {:?}",
        changed.error
    );
    let new_start = changed
        .data
        .and_then(|data| data.start_commit)
        .expect("a changed scope creates its own start");
    let before = oracle::snapshot(&repo);

    // The pinned plan is stale: it names a comparison this workspace no
    // longer holds, in a dry run and in a real application alike.
    assert_refusal(
        &apply(&repo, &plan, true),
        "STALE_REVIEW",
        "the dry run of the pinned plan",
    );
    assert_refusal(
        &apply(&repo, &plan, false),
        "STALE_REVIEW",
        "the pinned plan",
    );
    before.assert_unchanged(&oracle::snapshot(&repo), "the refused stale plan");

    // The new scope anchors at its own fresh start, and the walk never
    // crosses it to revive the old approval (SPEC 8.2, 8.5).
    let notes_scope = resolve_scope(&git, &notes).unwrap();
    let other_scope = resolve_scope(&git, &other).unwrap();
    let head = head_of(&repo);
    assert_eq!(
        select_baseline(&git, &head, &other_scope.digest)
            .expect("the new scope anchors")
            .commit,
        new_start
    );
    assert!(matches!(
        select_baseline(&git, &head, &notes_scope.digest),
        Err(BaselineError::Unavailable(
            BaselineUnavailable::ForeignStart { .. }
        ))
    ));
    // No review marker was created for either scope.
    let reviews = repo.git([
        "log",
        "--first-parent",
        "--format=%H",
        "--grep=Margin-Kind: review",
    ]);
    assert!(
        String::from_utf8_lossy(&reviews.stdout)
            .lines()
            .all(|line| line.is_empty()),
        "no review commit exists"
    );
}

// ---------------------------------------------------------------------------
// Limits: the per-file ceiling (SPEC 16.5, 3.3)
// ---------------------------------------------------------------------------

/// One document beyond the per-file ceiling stays preserved and counted,
/// while its comparison refuses `LIMIT_EXCEEDED`, grants no approval, and
/// is never silently omitted (SPEC 3.3, 6.3).
#[test]
fn an_oversized_document_stays_preserved_and_unapproved() {
    let repo = TestRepo::init();
    // One byte past the 16 MiB ceiling of SPEC 3.3.
    let unit = b"paragraph of prose\n\n";
    let mut original = Vec::with_capacity(16 * 1024 * 1024 + unit.len());
    while original.len() <= 16 * 1024 * 1024 {
        original.extend_from_slice(unit);
    }
    repo.write_file("notes/big.md", &original);
    repo.commit_all("import");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let scope = scope_input(&["notes"], &[], &[".md"]);

    // Preservation has no size ceiling to enforce: the starting point and
    // one later draft hold the exact raw bytes (SPEC 3.3, 9.2).
    let start = initialize(&repo);
    assert_eq!(
        oracle::blob(
            &repo,
            &text(&repo.git_ok(["rev-parse", &format!("{start}:notes/big.md")]))
        ),
        original,
        "the start holds the oversized bytes exactly"
    );
    let rewritten: Vec<u8> = original.iter().map(|_| b'z').collect();
    repo.write_file("notes/big.md", &rewritten);
    let draft = checkpoint(&repo, "rewrite the document");
    assert_eq!(
        oracle::blob(
            &repo,
            &text(&repo.git_ok(["rev-parse", &format!("{draft}:notes/big.md")]))
        ),
        rewritten,
        "the draft holds the rewrite exactly"
    );
    validate_marker(&git, &draft).unwrap_or_else(|error| panic!("the draft validates: {error}"));

    // The comparison refuses as one typed limit, quickly and without
    // effects, and the executable reports the same refusal (SPEC 3.3, 7.5).
    let before = oracle::snapshot(&repo);
    let review = operations.build_review(&DiffRequest {
        scope: Some(scope.clone()),
        path_filter: Vec::new(),
        change: None,
        limit: None,
        cursor: None,
        working: false,
    });
    assert_refusal(
        &review,
        "LIMIT_EXCEEDED",
        "the comparison of the oversized document",
    );
    before.assert_unchanged(&oracle::snapshot(&repo), "the refused comparison");
    let output = margin(repo.env(), repo.path(), &["diff", "--json"]);
    let envelope = envelope_of(&output, "the oversized comparison");
    assert_eq!(output.status.code(), Some(2), "a typed limit exits 2");
    assert_eq!(envelope["error"]["code"], "LIMIT_EXCEEDED");

    // The document is never silently omitted: status counts it and reports
    // the group count as unknown with its reason, never as zero (SPEC 6.3).
    let status = status_of(&repo);
    assert_eq!(
        status.counts.pending_files,
        Some(1),
        "the document is counted"
    );
    assert_eq!(
        status.counts.pending_groups, None,
        "no group count is fabricated"
    );
    assert!(
        status.counts.pending_groups_reason.is_some(),
        "the unknown group count carries a reason"
    );
    assert_eq!(status.preservation, PreservationState::Captured);

    // The refusal granted no approval: no review marker exists, and the
    // walk still anchors the start (SPEC 8.4-8.5).
    let reviews = repo.git([
        "log",
        "--first-parent",
        "--format=%H",
        "--grep=Margin-Kind: review",
    ]);
    assert!(
        String::from_utf8_lossy(&reviews.stdout)
            .lines()
            .all(|line| line.is_empty()),
        "no review commit exists"
    );
    let resolved = resolve_scope(&git, &scope).unwrap();
    assert_eq!(
        select_baseline(&git, &head_of(&repo), &resolved.digest)
            .expect("the start still anchors")
            .commit,
        start
    );
}

// ---------------------------------------------------------------------------
// Limits: large histories (SPEC 16.5, 8.5, 8.7)
// ---------------------------------------------------------------------------

/// Reads the generated history size from the environment, or returns the
/// default.
fn history_budget() -> usize {
    std::env::var("MARGIN_MATRIX_HISTORY_COMMITS")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(DEFAULT_HISTORY_COMMITS)
        .max(20)
}

/// Appends one ordinary commit through stock plumbing, so hundreds of them
/// cost no index traffic.
fn ordinary_commit(repo: &TestRepo) {
    let head = text(&repo.git_ok(["rev-parse", "HEAD"]));
    let tree = text(&repo.git_ok(["rev-parse", "HEAD^{tree}"]));
    let commit = text(&repo.git_ok(["commit-tree", &tree, "-p", &head, "-m", "external edit"]));
    repo.git_ok(["update-ref", "HEAD", &commit]);
}

/// A first-parent history of hundreds of ordinary commits leaves the
/// baseline walk, the receipt scan, and the timeline correct and bounded
/// (SPEC 6.3, 8.5, 8.7).
#[test]
fn long_first_parent_histories_stay_correct_and_bounded() {
    let commits = history_budget();
    let repo = TestRepo::init();
    repo.write_file(
        "notes/a.md",
        b"# One\n\nfirst passage.\n\nsecond passage.\n",
    );
    repo.commit_all("import");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let scope = scope_input(&["notes"], &[], &[".md"]);

    // One writing pass the history will bury: start, draft, review.
    let start = initialize(&repo);
    repo.write_file(
        "notes/a.md",
        b"# One\n\nfirst revised.\n\nsecond passage.\n",
    );
    let draft = checkpoint(&repo, "revise the first passage");
    let pinned = pinned_review_of(&repo);
    assert_eq!(pinned.baseline_oid, start);
    assert_eq!(pinned.candidate_oid, draft);
    let plan = keep_all_plan(&pinned, "long-history-1");
    let applied = apply(&repo, &plan, false);
    assert!(applied.ok(), "the review must apply: {:?}", applied.error);
    let review = applied
        .data
        .and_then(|data| data.commit)
        .expect("the application creates one review commit");
    validate_marker(&git, &review).unwrap_or_else(|error| panic!("the review validates: {error}"));

    // Hundreds of ordinary external commits above it.
    for _ in 0..commits {
        ordinary_commit(&repo);
    }
    let head = head_of(&repo);
    let stock_history = first_parent_history(&repo);
    assert_eq!(stock_history.len(), commits + 4);

    // The walk still selects the review under the whole chain: an ordinary
    // commit never approves anything (SPEC 8.5).
    let resolved = resolve_scope(&git, &scope).unwrap();
    let anchored = select_baseline(&git, &head, &resolved.digest)
        .unwrap_or_else(|error| panic!("the review must anchor: {error}"));
    assert_eq!(anchored.commit, review);
    let status = status_of(&repo);
    assert!(matches!(
        status.baseline,
        margin::app::BaselineStatus::Anchored(_)
    ));
    assert_eq!(status.review, ReviewState::Reviewed);

    // An exact retry finds its receipt through the whole chain, returns the
    // verified commit, and creates nothing (SPEC 8.7).
    let before = oracle::snapshot(&repo);
    let retry = apply(&repo, &plan, false);
    assert!(
        retry.ok(),
        "the retry returns the receipt: {:?}",
        retry.error
    );
    let data = retry.data.unwrap();
    assert_eq!(data.existing_receipt.as_deref(), Some(review.as_str()));
    assert_eq!(data.commit, None, "no second commit is created");
    assert_eq!(first_parent_history(&repo), stock_history);

    // The timeline lists every commit of the chain in ancestry order, one
    // bounded page at a time, and hides none of them — not the empty review
    // commit and not the start (SPEC 6.3, 8.5).
    let mut cursor: Option<String> = None;
    let mut entries: Vec<(String, TimelineKind)> = Vec::new();
    let mut complete = false;
    for _ in 0..40 {
        let page = operations
            .list_history(&LogRequest {
                scope: None,
                path_filter: Vec::new(),
                cursor: cursor.clone(),
                limit: Some(LOG_PAGE_LIMIT),
                all_checkpoints: false,
            })
            .data
            .expect("the timeline page succeeds");
        assert!(
            page.entries.len() as u64 <= LOG_PAGE_LIMIT,
            "one page stays inside the {}-entry bound",
            LOG_PAGE_LIMIT
        );
        assert_eq!(page.head.as_deref(), Some(head.as_str()));
        entries.extend(
            page.entries
                .iter()
                .map(|entry| (entry.commit.clone(), entry.kind)),
        );
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => {
                complete = page.complete;
                break;
            }
        }
    }
    assert!(complete, "the pages must reach the end of the history");
    assert_eq!(
        entries
            .iter()
            .map(|(commit, _)| commit.clone())
            .collect::<Vec<_>>(),
        stock_history,
        "the timeline lists exactly the commits stock Git lists, in order"
    );
    assert!(
        entries
            .iter()
            .any(|(_, kind)| *kind == TimelineKind::Review),
        "the empty review commit stays visible"
    );
    assert!(
        entries.iter().any(|(_, kind)| *kind == TimelineKind::Start),
        "the start marker stays visible"
    );
    before.assert_unchanged(&oracle::snapshot(&repo), "the queries over a long history");
}

// ---------------------------------------------------------------------------
// Removal (SPEC 16.5, 8.2)
// ---------------------------------------------------------------------------

/// One recorded version of one document: what stock Git resolves at one
/// immutable revision.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DocumentVersion {
    /// The commit object ID of the revision.
    commit: String,
    /// The tree object ID of the revision.
    tree: String,
    /// The entry of the document, or absence.
    entry: Option<(String, String)>,
}

/// Records the version of `path` at every commit of the first-parent chain
/// through stock Git alone.
fn record_versions(repo: &TestRepo, path: &str) -> Vec<DocumentVersion> {
    first_parent_history(repo)
        .into_iter()
        .map(|commit| {
            let tree = text(&repo.git_ok(["rev-parse", &format!("{commit}^{{tree}}")]));
            let entry = oracle::ls_tree(repo, &commit)
                .into_iter()
                .find(|entry| entry.path == path)
                .map(|entry| (entry.mode, entry.oid));
            DocumentVersion {
                commit,
                tree,
                entry,
            }
        })
        .collect()
}

/// Deleting the local Margin configuration through stock Git keeps every
/// document version readable through stock Git alone, and one explicit
/// re-adoption rebuilds the workspace without a second start (SPEC 8.2).
#[test]
fn removing_local_configuration_keeps_every_version_readable() {
    let repo = TestRepo::init();
    repo.write_file(
        "notes/a.md",
        b"# One\n\nfirst passage.\n\nsecond passage.\n",
    );
    repo.write_file("other/sentinel.txt", b"untouched\n");
    repo.commit_all("import");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);

    // One complete writing loop, so the history holds every marker kind.
    let start = initialize(&repo);
    repo.write_file(
        "notes/a.md",
        b"# One\n\nfirst revised.\n\nsecond passage.\n",
    );
    let draft = checkpoint(&repo, "revise the first passage");
    let pinned = pinned_review_of(&repo);
    let applied = apply(&repo, &keep_all_plan(&pinned, "removal-1"), false);
    assert!(applied.ok(), "the review must apply: {:?}", applied.error);
    let review = applied
        .data
        .and_then(|data| data.commit)
        .expect("the application creates one review commit");
    let restore_plan = serde_json::json!({
        "schema_version": 1,
        "request_id": "removal-2",
        "branch_ref": pinned.branch_ref,
        "scope_id": pinned.scope_id,
        "candidate_oid": review,
        "path": "notes/a.md",
        "revision": start,
    })
    .to_string();
    let restored = operations.restore_document(&RestoreRequest {
        scope: None,
        plan: restore_plan.as_bytes().to_vec(),
        dry_run: false,
    });
    assert!(
        restored.ok(),
        "the restoration must apply: {:?}",
        restored.error
    );
    let restore = restored
        .data
        .and_then(|data| data.commit)
        .expect("the restoration creates one commit");
    for commit in [&start, &draft, &review, &restore] {
        validate_marker(&git, commit)
            .unwrap_or_else(|error| panic!("the fixture marker validates: {error}"));
    }

    // Every version of the document, recorded through stock Git.
    let versions = record_versions(&repo, "notes/a.md");
    assert_eq!(
        versions.len(),
        5,
        "import, start, draft, review, and restore"
    );
    let history = first_parent_history(&repo);
    let before = oracle::snapshot(&repo);

    // Remove the local Margin configuration through stock Git.
    repo.git_ok(["config", "--remove-section", "margin"]);
    let remaining = repo.git(["config", "--local", "--get-regexp", "^margin\\."]);
    assert!(
        !remaining.status.success(),
        "no margin configuration remains: {}",
        String::from_utf8_lossy(&remaining.stdout)
    );

    // Nothing moved but the configuration: refs, index, and every working
    // byte stay exactly as they were.
    before.assert_unchanged(&oracle::snapshot(&repo), "the configuration removal");

    // Every version of the document resolves to the same immutable objects
    // and bytes through stock Git alone (SPEC 8.2).
    assert_eq!(record_versions(&repo, "notes/a.md"), versions);
    assert_eq!(first_parent_history(&repo), history);
    for version in &versions {
        if let Some((_, oid)) = &version.entry {
            assert!(
                repo.git(["cat-file", "-e", oid]).status.success(),
                "the blob of {} stays readable",
                version.commit
            );
        }
    }

    // The workspace no longer claims adoption, the refusal writes nothing,
    // and one historical document still reads without a configured scope.
    let status = margin(repo.env(), repo.path(), &["status", "--json"]);
    let envelope = envelope_of(&status, "the status after removal");
    assert_eq!(status.status.code(), Some(4));
    assert_eq!(envelope["error"]["code"], "NOT_INITIALIZED");
    let shown = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/a.md",
            "--revision",
            &draft,
            "--json",
        ],
    );
    let shown_envelope = envelope_of(&shown, "the historical document");
    assert_eq!(shown.status.code(), Some(0));
    assert_eq!(
        shown_envelope["data"]["content"]["text"],
        "# One\n\nfirst revised.\n\nsecond passage.\n"
    );
    before.assert_unchanged(&oracle::snapshot(&repo), "the queries after removal");

    // One explicit re-adoption rebuilds the workspace from history alone:
    // no second start appears, the settings return, and the completed
    // review anchors the scope again (SPEC 8.2).
    let reinit = operations.initialize_scope(&first_use(&scope_input(&["notes"], &[], &[".md"])));
    assert!(reinit.ok(), "the re-adoption succeeds: {:?}", reinit.error);
    let data = reinit.data.unwrap();
    assert_eq!(data.start_commit, None, "no second start is created");
    assert_eq!(first_parent_history(&repo), history);
    for key in ["margin.version", "margin.branch", "margin.scope"] {
        let value = repo.git(["config", "--local", "--get", key]);
        assert!(
            value.status.success(),
            "{key} is re-adopted: {}",
            String::from_utf8_lossy(&value.stderr)
        );
    }
    let resolved = resolve_scope(&git, &scope_input(&["notes"], &[], &[".md"])).unwrap();
    assert_eq!(
        select_baseline(&git, &head_of(&repo), &resolved.digest)
            .expect("the review anchors again")
            .commit,
        review
    );
    let status = status_of(&repo);
    assert_eq!(
        status.review,
        ReviewState::Pending,
        "the restored content is pending review"
    );
}
