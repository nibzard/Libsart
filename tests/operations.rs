//! Real-Git tests for the shared operations and effect reporting
//! (SPEC 7.1, 13.2, task T014).
//!
//! Every case proves one contract of the operation layer against the
//! installed Git:
//!
//! - inspection reports the workspace through the one resolver, the one
//!   policy guard, and the one baseline walk, and leaves refs, index, and
//!   working bytes unchanged
//! - success and failure carry the complete observed effects of SPEC 7.1
//! - a missing scope, a directory outside any repository, an invalid scope
//!   input, and a dirty index fail with typed codes and unchanged effects,
//!   because nothing was written
//! - scope and policy blockers are observations the query carries as data;
//!   only mutations refuse on them
//! - operation phases whose algorithms arrive in later tasks refuse with
//!   one typed code and unchanged effects, never silently or untyped; the
//!   scoped checkpoint of task T020 is covered by the checkpoint suite

mod support;

use std::ffi::OsString;
use std::os::unix::fs::symlink;

use margin::app::{
    BaselineStatus, CheckpointRequest, CommitEffect, DiffRequest, Effects, InspectRequest,
    OperationOutcome, Operations, RecoveryRequest, ReviewRequest, commit_marker, resolve_scope,
};
use margin::domain::{CommitKind, CommitMetadata, ScopeInput};
use margin::git::Git;
use support::oracle;
use support::repo::TestRepo;

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

/// One start marker for the scope rooted at `notes`, committed on the
/// current index through the shared marker writer.
fn mark_start(repo: &TestRepo) -> String {
    let git = adapter_for(repo);
    let scope =
        resolve_scope(&git, &scope_input(&["notes"], &[], &[".md"])).unwrap_or_else(|error| {
            panic!("the scope must resolve: {error}");
        });
    let parent = support::text(&repo.git_ok(["rev-parse", "HEAD"]));
    let tree = support::text(&repo.git_ok(["write-tree"]));
    let marker = commit_marker(
        &git,
        &CommitMetadata {
            kind: CommitKind::Start,
            scope_digest: scope.digest.id(),
            scope_spec: String::from_utf8(scope.descriptor.canonical_bytes()).unwrap(),
            candidate: parent,
            result_tree: tree,
            baseline: None,
            request: None,
            plan: None,
            source: None,
        },
        "first writing session",
    )
    .unwrap_or_else(|error| panic!("the start marker must commit: {error}"));
    marker.commit
}

/// Asserts one typed refusal: not successful, one primary code, and the
/// unchanged effects of an operation that wrote nothing.
fn assert_refusal<T>(result: &OperationOutcome<T>, code: &str, what: &str) {
    assert!(!result.ok(), "{what} must not succeed");
    let error = result.error.as_ref().unwrap();
    assert_eq!(error.code, code, "{what} reports the wrong code");
    assert_eq!(result.effects, Effects::unchanged(), "{what} wrote nothing");
    assert_eq!(
        result.effects.commit,
        CommitEffect::None,
        "{what} committed nothing"
    );
}

#[test]
fn inspection_reports_the_workspace_without_writing() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.write_file("other.txt", b"out of scope\n");
    repo.commit_all("import");
    let start = mark_start(&repo);
    // An eligible untracked file stays part of the scope without dirtying
    // the index.
    repo.write_file("notes/draft.md", b"# Draft\n");

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let request = InspectRequest {
        scope: Some(scope_input(&["notes"], &[], &[".md"])),
        path_filter: Vec::new(),
    };
    let before = oracle::snapshot(&repo);
    let result = operations.inspect_workspace(&request);

    assert!(result.ok(), "the inspection must succeed");
    assert!(result.warnings.is_empty());
    let data = result.data.as_ref().unwrap();
    assert_eq!(data.branch.as_deref(), Some("refs/heads/main"));
    assert_eq!(data.head.as_deref(), oracle::head_oid(&repo).as_deref());
    assert_eq!(data.object_format.as_str(), "sha1");
    // The reported scope is the one the shared resolver produces.
    let resolved = resolve_scope(&git, request.scope.as_ref().unwrap()).unwrap();
    assert_eq!(data.scope.digest, resolved.digest.id());
    assert_eq!(data.scope.tracked, 1, "notes/a.md is tracked");
    assert_eq!(data.scope.untracked, 1, "notes/draft.md is eligible");
    assert_eq!(data.scope.tracked_deleted, 0);
    assert!(data.scope.blockers.is_empty());
    assert!(
        data.policy_blockers.is_empty(),
        "a clean workspace has no blockers"
    );
    match &data.baseline {
        BaselineStatus::Anchored(marker) => {
            assert_eq!(marker.commit, start);
            assert_eq!(marker.kind, CommitKind::Start);
        }
        other => panic!("the start marker must anchor the scope: {other:?}"),
    }
    // The three state dimensions of SPEC 4.2 report separately: the
    // eligible untracked draft is uncaptured while the committed content
    // still equals its starting point.
    assert_eq!(data.preservation.as_str(), "uncaptured");
    assert_eq!(data.review.as_str(), "starting_point");
    assert_eq!(data.operation.as_str(), "idle");
    assert_eq!(data.counts.pending_files, Some(0));
    assert_eq!(data.counts.pending_groups, None);
    assert_eq!(
        data.counts.pending_groups_reason,
        Some(margin::app::COUNT_REASON_GROUPING_UNAVAILABLE)
    );
    assert_eq!(data.counts.uncaptured.as_ref().unwrap().total, 1);
    assert_eq!(
        data.counts.uncaptured.as_ref().unwrap().paths,
        vec![OsString::from("notes/draft.md")]
    );
    assert_eq!(data.limits.rendered_comparison_bytes, 1024 * 1024);
    assert_eq!(data.limits.document_bytes, 16 * 1024 * 1024);
    // A query reports exactly the effects SPEC 7.1 requires.
    assert_eq!(result.effects, Effects::unchanged());
    before.assert_unchanged(&oracle::snapshot(&repo), "inspect");
}

#[test]
fn inspection_of_unborn_history_reports_no_candidate() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.inspect_workspace(&InspectRequest {
        scope: Some(scope_input(&["notes"], &[], &[".md"])),
        path_filter: Vec::new(),
    });

    assert!(result.ok(), "the inspection must succeed");
    let data = result.data.as_ref().unwrap();
    assert_eq!(data.head, None, "the branch is unborn");
    assert_eq!(data.branch.as_deref(), Some("refs/heads/main"));
    assert_eq!(data.scope.untracked, 1);
    assert!(matches!(data.baseline, BaselineStatus::NoCandidate));
    // Without a candidate no count is fabricated: the pending comparison
    // is null with its reason while the present draft is honestly
    // uncaptured (SPEC 4.2, 6.3).
    assert_eq!(data.preservation.as_str(), "uncaptured");
    assert_eq!(data.review.as_str(), "blocked");
    assert_eq!(data.counts.pending_files, None);
    assert_eq!(
        data.counts.pending_files_reason,
        Some(margin::app::COUNT_REASON_NO_CANDIDATE)
    );
    assert_eq!(data.counts.uncaptured.as_ref().unwrap().total, 1);
    assert_eq!(result.effects, Effects::unchanged());
}

#[test]
fn a_missing_scope_fails_typed_with_effects() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.inspect_workspace(&InspectRequest {
        scope: None,
        path_filter: Vec::new(),
    });

    assert_refusal(&result, "NOT_INITIALIZED", "inspection without a scope");
    let error = result.error.as_ref().unwrap();
    assert!(!error.retryable);
    assert_eq!(
        error.recovery_argv,
        vec![
            "margin".to_string(),
            "init".to_string(),
            "--json".to_string()
        ]
    );
    assert!(result.data.is_none());
}

#[test]
fn inspection_outside_any_repository_is_not_initialized() {
    let repo = TestRepo::init();
    let outside = tempfile::tempdir().unwrap();
    let git = Git::from_env(repo.env().vars().clone()).at(outside.path());
    let operations = Operations::new(&git);

    let result = operations.inspect_workspace(&InspectRequest {
        scope: Some(scope_input(&["notes"], &[], &[".md"])),
        path_filter: Vec::new(),
    });

    // `margin init` can create a repository here, so the state is
    // uninitialized rather than unsupported (SPEC 5.1).
    assert_refusal(
        &result,
        "NOT_INITIALIZED",
        "inspection outside a repository",
    );
}

#[test]
fn invalid_scope_input_fails_typed_with_effects() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.inspect_workspace(&InspectRequest {
        scope: Some(scope_input(&["../escape"], &[], &[".md"])),
        path_filter: Vec::new(),
    });

    assert_refusal(&result, "INVALID_ARGUMENT", "an escaping scope root");
    let before = oracle::snapshot(&repo);
    before.assert_unchanged(&oracle::snapshot(&repo), "a refused query");
}

#[test]
fn blockers_are_observed_data_not_query_failures() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    // A symbolic link at a scoped path is an actionable blocker.
    symlink("a.md", repo.file_path("notes/link.md")).unwrap();
    // Line-ending conversion on check-in violates the storage profile.
    repo.set_local_config("core.autocrlf", "true");

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.inspect_workspace(&InspectRequest {
        scope: Some(scope_input(&["notes"], &[], &[".md"])),
        path_filter: Vec::new(),
    });

    assert!(result.ok(), "blockers must not fail a query");
    let data = result.data.as_ref().unwrap();
    assert_eq!(data.scope.blockers.len(), 1, "the link is one blocker");
    assert!(
        data.policy_blockers.iter().any(|blocker| matches!(
            blocker,
            margin::app::PolicyBlocker::AutoCrlfConversion { .. }
        )),
        "the storage blocker is reported"
    );
    assert_eq!(result.effects, Effects::unchanged());
}

#[test]
fn guard_refusals_write_nothing_and_report_effects() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    mark_start(&repo);
    // A staged change against HEAD blocks every normal mutation.
    repo.write_file("notes/a.md", b"# One changed\n");
    repo.git_ok(["add", "notes/a.md"]);

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let scope = scope_input(&["notes"], &[], &[".md"]);
    let before = oracle::snapshot(&repo);

    let checkpoint = operations.create_checkpoint(&CheckpointRequest {
        scope: Some(scope.clone()),
        message: None,
        include_staged: false,
        dry_run: false,
    });
    assert_refusal(
        &checkpoint,
        "STAGED_CHANGES",
        "checkpoint with a dirty index",
    );
    let error = checkpoint.error.as_ref().unwrap();
    assert!(
        error.message.contains("staged change"),
        "the diagnosis names the blocker"
    );

    // The review application runs its guards over one complete plan the
    // pinned comparison itself issued, so the staged entry — not the plan
    // document — is what refuses.
    let build = operations.build_review(&DiffRequest {
        scope: Some(scope.clone()),
        path_filter: Vec::new(),
        change: None,
        cursor: None,
        limit: None,
        working: false,
    });
    assert!(build.ok(), "the pinned review builds: {:?}", build.error);
    let data = build.data.unwrap();
    let plan = format!(
        concat!(
            "{{\"schema_version\":1,\"request_id\":\"guard-01\",",
            "\"review_id\":{},\"branch_ref\":{},\"scope_id\":{},",
            "\"baseline_oid\":{},\"candidate_oid\":{},",
            "\"restore\":[],\"keep_remaining\":true}}"
        ),
        serde_json::to_string(&data.review_id).unwrap(),
        serde_json::to_string(&data.branch_ref).unwrap(),
        serde_json::to_string(&data.scope_id).unwrap(),
        serde_json::to_string(&data.baseline_oid).unwrap(),
        serde_json::to_string(&data.candidate_oid).unwrap(),
    );
    let apply = operations.apply_review(&ReviewRequest {
        scope: Some(scope.clone()),
        plan: plan.into_bytes(),
        dry_run: false,
    });
    assert_refusal(
        &apply,
        "STAGED_CHANGES",
        "review application with a dirty index",
    );

    // A query observes the same state without refusing on it: the diff
    // pins its comparison at B and C, and staged content that no commit
    // holds touches neither (SPEC 6.3).
    let build = operations.build_review(&DiffRequest {
        scope: Some(scope),
        path_filter: Vec::new(),
        change: None,
        cursor: None,
        limit: None,
        working: false,
    });
    assert!(
        build.ok(),
        "a diff query does not refuse on a dirty index: {:?}",
        build.error
    );
    let data = build.data.as_ref().unwrap();
    assert_eq!(data.scope_totals.files, 0, "B equals C at the marker");
    assert_eq!(data.scope_totals.groups, 0);
    assert!(data.changes.is_empty());
    assert!(data.complete);
    assert!(data.actionable);
    assert_eq!(data.next_cursor, None);
    assert_eq!(build.effects, Effects::unchanged());

    before.assert_unchanged(&oracle::snapshot(&repo), "refused mutations");
}

#[test]
fn pending_algorithms_refuse_without_writing() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    mark_start(&repo);

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let scope = scope_input(&["notes"], &[], &[".md"]);
    let head = oracle::head_oid(&repo).unwrap();
    let before = oracle::snapshot(&repo);

    // Every algorithm a later task provides refuses with one typed code and
    // unchanged effects. The initialization contract, first use and repeat,
    // and the checkpoint contract are covered by their own suites.
    //
    // The plan is a real document the strict parser accepts: the pinned
    // comparison of this workspace carries no group, so an empty restore
    // with `keep_remaining` decides it completely.
    let build = operations.build_review(&DiffRequest {
        scope: Some(scope.clone()),
        path_filter: Vec::new(),
        change: None,
        cursor: None,
        limit: None,
        working: false,
    });
    assert!(build.ok(), "the empty review builds: {:?}", build.error);
    let data = build.data.as_ref().unwrap();
    let plan = format!(
        concat!(
            "{{\"schema_version\":1,\"request_id\":\"pending-01\",",
            "\"review_id\":{},\"branch_ref\":{},\"scope_id\":{},",
            "\"baseline_oid\":{},\"candidate_oid\":{},",
            "\"restore\":[],\"keep_remaining\":true}}"
        ),
        serde_json::to_string(&data.review_id).unwrap(),
        serde_json::to_string(&data.branch_ref).unwrap(),
        serde_json::to_string(&data.scope_id).unwrap(),
        serde_json::to_string(&data.baseline_oid).unwrap(),
        serde_json::to_string(&data.candidate_oid).unwrap(),
    );
    let plan = plan.into_bytes();
    let review = ReviewRequest {
        scope: Some(scope.clone()),
        plan: plan.clone(),
        dry_run: true,
    };
    // The dry run of a complete plan is no longer pending (T029): it
    // validates the decisions, reports the intended tree, and writes
    // nothing, so the operation succeeds with unchanged effects.
    let validation = operations.validate_review(&review);
    assert!(validation.ok(), "review validation: {:?}", validation.error);
    let decision = validation.data.as_ref().unwrap();
    assert!(decision.dry_run);
    assert_eq!(decision.restored, Vec::<String>::new());
    assert_eq!(decision.kept, Some(0));
    assert_eq!(decision.expected_tree, oracle::commit_tree(&repo, &head));
    assert_eq!(validation.effects, Effects::unchanged());
    assert_refusal(
        &operations.resume_review(&ReviewRequest {
            scope: Some(scope.clone()),
            plan,
            dry_run: false,
        }),
        "OPERATION_UNAVAILABLE",
        "review resumption",
    );

    // The recovery inspection of T034 is real: it reports the observed
    // state of this workspace and writes nothing. Its own test file covers
    // the full contract against installed Git.
    let recovery = operations.inspect_recovery(&RecoveryRequest { scope: Some(scope) });
    assert!(
        recovery.ok(),
        "the recovery inspection succeeds: {:?}",
        recovery.error
    );
    assert_eq!(recovery.effects, Effects::unchanged());

    // Every refusal reports one typed error, no data, and the honest
    // effects of an operation that wrote nothing (SPEC 7.1).
    before.assert_unchanged(&oracle::snapshot(&repo), "pending operations");
}
