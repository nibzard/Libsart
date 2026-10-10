//! Real-Git tests for the status report (SPEC 4.2, 6.3, task T019).
//!
//! Every case proves one contract of the preservation and review status
//! report against the installed Git:
//!
//! - the three state dimensions of SPEC 4.2 stay separate and each one
//!   moves on its own evidence through a full writing pass: adoption,
//!   unsaved edits, a committed draft, and a completed review
//! - expensive counts are known or `null` with one reason, never
//!   fabricated zeros, when no candidate or no valid baseline exists
//! - storage transformation blockers turn preservation honestly unknown
//!   while the committed comparison stays computable
//! - staged changes and interrupted Git operations ask for recovery; a
//!   clean workspace stays idle
//! - a branch other than the adopted one is one actionable blocker the
//!   query reports without refusing on it, and the query stays read-only
//! - a `--path` filter narrows the reported path list only; every count
//!   stays whole-scope
//! - every query leaves refs, the index, and working bytes unchanged,
//!   including the raw index file itself (INV-12)

mod support;

use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use margin::app::{Identities, InspectRequest, Operations, commit_marker, resolve_scope};
use margin::domain::{CommitKind, CommitMetadata, DecisionMode, PlanIdentity, ScopeInput};
use margin::git::Git;
use serde_json::Value;
use support::repo::TestRepo;

/// Runs the installed executable once with the isolated environment.
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

/// Parses the one JSON envelope of a successful machine-mode run and checks
/// the envelope contract itself (SPEC 7.1).
fn data_of(output: &Output, what: &str) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.matches('\n').count(),
        1,
        "{what}: exactly one envelope line, got: {stdout:?}"
    );
    let envelope: Value = serde_json::from_str(stdout.trim_end())
        .unwrap_or_else(|error| panic!("{what}: the envelope must parse: {error}: {stdout}"));
    assert_eq!(envelope["ok"], true, "{what}: the query succeeds");
    assert_eq!(envelope["error"], Value::Null, "{what}: no error");
    assert_eq!(
        envelope["effects"]["working_tree"], "unchanged",
        "{what}: a query writes no file"
    );
    assert_eq!(
        envelope["effects"]["index"], "unchanged",
        "{what}: a query writes no index entry"
    );
    assert_eq!(
        envelope["effects"]["commit"], "none",
        "{what}: a query creates no commit"
    );
    assert!(
        output.stderr.is_empty(),
        "{what}: machine mode writes no stderr"
    );
    envelope["data"].clone()
}

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

/// Initializes the scope rooted at `notes` through the real command and
/// returns the start commit the verified marker reports.
fn init_notes_scope(repo: &TestRepo) -> String {
    let output = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes", "--json"],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "init succeeds: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: Value = serde_json::from_slice(&output.stdout).expect("one envelope");
    envelope["data"]["start_commit"]
        .as_str()
        .expect("the start commit is reported")
        .to_string()
}

/// Commits one verified keep-all review marker on the current index whose
/// recorded baseline is `baseline` (SPEC 8.3, 8.4 rule 6).
fn review_marker(repo: &TestRepo, scope: &margin::app::ResolvedScope, baseline: &str) -> String {
    let git = adapter_for(repo);
    let parent = support::text(&repo.git_ok(["rev-parse", "HEAD"]));
    let tree = support::text(&repo.git_ok(["write-tree"]));
    let request = "review-status-report".to_string();
    let plan = Identities::new(&git)
        .unwrap_or_else(|error| panic!("identities must read the repository: {error}"))
        .plan(&PlanIdentity {
            operation: "review".to_string(),
            request_id: request.clone(),
            review_id: format!("sha1:{}", "0".repeat(40)),
            branch_ref: "refs/heads/main".to_string(),
            scope_id: scope.digest.oid.clone(),
            baseline_oid: baseline.to_string(),
            candidate_oid: parent.clone(),
            restore: Vec::new(),
            decision_mode: DecisionMode::KeepRemaining,
            message: "keep every change".to_string(),
        })
        .unwrap_or_else(|error| panic!("the plan must hash: {error}"))
        .id();
    let marker = commit_marker(
        &git,
        &CommitMetadata {
            kind: CommitKind::Review,
            scope_digest: scope.digest.id(),
            scope_spec: String::from_utf8(scope.descriptor.canonical_bytes()).unwrap(),
            candidate: parent,
            result_tree: tree,
            baseline: Some(baseline.to_string()),
            request: Some(request),
            plan: Some(plan),
            source: None,
        },
        "keep every change",
    )
    .unwrap_or_else(|error| panic!("the review marker must verify: {error}"));
    marker.commit
}

/// Reads the raw bytes of the real index, proving a query never rewrites it
/// (INV-12).
fn index_bytes(repo: &TestRepo) -> Vec<u8> {
    fs::read(repo.path().join(".git/index"))
        .unwrap_or_else(|error| panic!("the index can be read: {error}"))
}

#[test]
fn the_three_states_move_on_their_own_evidence() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.write_file("notes/b.md", b"# Two\n");
    repo.write_file("other/sentinel.txt", b"untouched\n");
    repo.commit_all("seed");
    let start = init_notes_scope(&repo);

    // Adoption: the starting content is in the candidate, nothing is
    // approved yet, and nothing asks for recovery (SPEC 4.2).
    let before = support::oracle::snapshot(&repo);
    let output = margin(repo.env(), repo.path(), &["status", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let data = data_of(&output, "status after adoption");
    for (field, expected) in [
        ("preservation", "captured"),
        ("review", "starting_point"),
        ("operation", "idle"),
    ] {
        assert_eq!(data[field], expected, "{field} after adoption");
    }
    assert_eq!(data["head"], support::oracle::head_oid(&repo).unwrap());
    assert_eq!(data["branch"], "refs/heads/main");
    assert_eq!(data["baseline"]["state"], "anchored");
    assert_eq!(data["baseline"]["kind"], "start");
    assert_eq!(data["baseline"]["commit"], start);
    assert_eq!(data["eligible"], true);
    assert_eq!(data["blockers"], serde_json::json!([]));
    assert_eq!(
        data["limits"],
        serde_json::json!({
            "rendered_comparison_bytes": 1024 * 1024,
            "document_bytes": 16 * 1024 * 1024,
        }),
        "the SPEC 3.3 limits ship with the inspection"
    );
    let counts = &data["counts"];
    assert_eq!(counts["pending_files"], 0);
    assert_eq!(counts["pending_files_reason"], Value::Null);
    assert_eq!(counts["pending_groups"], Value::Null);
    assert_eq!(
        counts["pending_groups_reason"], "grouping_unavailable",
        "grouping is honestly pending, not zero"
    );
    assert_eq!(counts["uncaptured_total"], 0);
    assert_eq!(counts["uncaptured_paths"], serde_json::json!([]));
    assert_eq!(counts["uncaptured_reason"], Value::Null);
    before.assert_unchanged(&support::oracle::snapshot(&repo), "a clean status");

    // Unsaved edits: preservation alone moves. A saved-and-unreviewed
    // document, a new draft, and a deletion are all uncaptured working
    // content (SPEC 4.2).
    repo.write_file("notes/a.md", b"# One edited\n");
    repo.write_file("notes/draft.md", b"# Draft\n");
    fs::remove_file(repo.file_path("notes/b.md")).unwrap();
    let before = support::oracle::snapshot(&repo);
    let index_before = index_bytes(&repo);
    let output = margin(repo.env(), repo.path(), &["status", "--json"]);
    let data = data_of(&output, "status with unsaved edits");
    assert_eq!(data["preservation"], "uncaptured");
    assert_eq!(data["review"], "starting_point", "no committed change yet");
    assert_eq!(data["operation"], "idle", "the index stays clean");
    assert_eq!(data["counts"]["uncaptured_total"], 3);
    assert_eq!(
        data["counts"]["uncaptured_paths"],
        serde_json::json!(["notes/a.md", "notes/b.md", "notes/draft.md"])
    );
    assert_eq!(
        data["counts"]["pending_files"], 0,
        "the baseline still equals the candidate"
    );
    before.assert_unchanged(&support::oracle::snapshot(&repo), "a dirty status");
    assert_eq!(
        index_bytes(&repo),
        index_before,
        "the query never rewrites the real index, even with stat-stale paths"
    );

    // A committed draft: preservation returns to captured while review
    // alone moves to pending, with the file count of the scoped
    // baseline-to-candidate comparison (SPEC 4.2, 6.3).
    repo.commit_all("draft pass");
    let before = support::oracle::snapshot(&repo);
    let output = margin(repo.env(), repo.path(), &["status", "--json"]);
    let data = data_of(&output, "status after a draft");
    assert_eq!(data["preservation"], "captured");
    assert_eq!(data["review"], "pending");
    assert_eq!(data["operation"], "idle");
    assert_eq!(data["counts"]["pending_files"], 3);
    assert_eq!(data["counts"]["uncaptured_total"], 0);
    // The baseline stays the starting point: a draft is not approval.
    assert_eq!(data["baseline"]["kind"], "start");
    assert_eq!(data["baseline"]["commit"], start);
    before.assert_unchanged(&support::oracle::snapshot(&repo), "a pending status");

    // A completed review: review alone moves again, and only a valid
    // review marker reports `reviewed` (SPEC 4.1, 8.4).
    let scope = resolve_scope(&adapter_for(&repo), &scope_input(&["notes"], &[], &[".md"]))
        .unwrap_or_else(|error| panic!("the scope must resolve: {error}"));
    let review = review_marker(&repo, &scope, &start);
    let output = margin(repo.env(), repo.path(), &["status", "--json"]);
    let data = data_of(&output, "status after a review");
    assert_eq!(data["preservation"], "captured");
    assert_eq!(data["review"], "reviewed");
    assert_eq!(data["operation"], "idle");
    assert_eq!(data["counts"]["pending_files"], 0);
    assert_eq!(data["baseline"]["state"], "anchored");
    assert_eq!(data["baseline"]["kind"], "review");
    assert_eq!(data["baseline"]["commit"], review);
}

#[test]
fn unknown_counts_carry_reasons_instead_of_fabricated_zeros() {
    // No candidate: an unborn branch holds no commit to compare against,
    // so the pending counts are null with one reason while the present
    // working paths are honestly uncaptured (SPEC 6.3).
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.write_file("notes/gone.md", b"# Gone\n");
    repo.git_ok(["add", "--", "notes/gone.md"]);
    fs::remove_file(repo.file_path("notes/gone.md")).unwrap();
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.inspect_workspace(&InspectRequest {
        scope: Some(scope_input(&["notes"], &[], &[".md"])),
        path_filter: Vec::new(),
    });
    assert!(result.ok(), "the query itself succeeds");
    let data = result.data.as_ref().unwrap();
    assert_eq!(data.head, None);
    assert_eq!(data.preservation.as_str(), "uncaptured");
    assert_eq!(data.review.as_str(), "blocked");
    assert_eq!(data.operation.as_str(), "recovery_required");
    assert_eq!(data.counts.pending_files, None);
    assert_eq!(
        data.counts.pending_files_reason,
        Some(margin::app::COUNT_REASON_NO_CANDIDATE)
    );
    // The staged entry on the unborn branch is itself recovery state; the
    // one present path is the uncaptured content.
    assert_eq!(data.counts.uncaptured.as_ref().unwrap().total, 1);
    assert_eq!(
        data.counts.uncaptured.as_ref().unwrap().paths,
        vec![std::ffi::OsString::from("notes/a.md")]
    );
    assert_eq!(result.effects, margin::app::Effects::unchanged());

    // No valid baseline: history without any marker leaves the review
    // question unanswerable, never zeroed (SPEC 8.5, 6.3).
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("ordinary history");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.inspect_workspace(&InspectRequest {
        scope: Some(scope_input(&["notes"], &[], &[".md"])),
        path_filter: Vec::new(),
    });
    assert!(result.ok());
    let data = result.data.as_ref().unwrap();
    assert!(data.head.is_some());
    assert_eq!(data.review.as_str(), "blocked");
    assert_eq!(data.counts.pending_files, None);
    assert_eq!(
        data.counts.pending_files_reason,
        Some(margin::app::COUNT_REASON_BASELINE_UNAVAILABLE)
    );
    assert_eq!(data.preservation.as_str(), "captured");
    assert_eq!(result.effects, margin::app::Effects::unchanged());
}

#[test]
fn storage_transformations_make_preservation_honestly_unknown() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\r\nline endings\r\n");
    repo.commit_all("seed");
    init_notes_scope(&repo);
    repo.write_file("notes/a.md", b"# One\r\nedited\r\n");
    // A transformation outside the identity profile makes the raw working
    // bytes and the stored blobs incomparable (SPEC 8.8).
    repo.set_local_config("core.autocrlf", "true");
    let before = support::oracle::snapshot(&repo);

    let output = margin(repo.env(), repo.path(), &["status", "--json"]);
    assert_eq!(output.status.code(), Some(0), "inspection stays available");
    let data = data_of(&output, "status under a transformation");
    assert_eq!(data["preservation"], "unknown");
    assert_eq!(
        data["counts"]["uncaptured_total"],
        Value::Null,
        "no fabricated count"
    );
    assert_eq!(data["counts"]["uncaptured_paths"], Value::Null);
    assert_eq!(
        data["counts"]["uncaptured_reason"],
        "storage_transformation"
    );
    // The committed comparison stays computable: the start marker still
    // anchors an unchanged candidate.
    assert_eq!(data["review"], "starting_point");
    assert_eq!(data["counts"]["pending_files"], 0);
    assert_eq!(data["eligible"], false);
    assert!(
        data["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|blocker| blocker.as_str().unwrap_or("").contains("core.autocrlf")),
        "the blocker names the transformation: {}",
        data["blockers"]
    );
    before.assert_unchanged(&support::oracle::snapshot(&repo), "a transformed status");
}

#[test]
fn interrupted_repository_state_asks_for_recovery() {
    // Staged changes against HEAD are recovery state, not a query failure.
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("seed");
    init_notes_scope(&repo);
    repo.write_file("notes/a.md", b"# One staged\n");
    repo.git_ok(["add", "--", "notes/a.md"]);
    let before = support::oracle::snapshot(&repo);
    let output = margin(repo.env(), repo.path(), &["status", "--json"]);
    let data = data_of(&output, "status with staged changes");
    assert_eq!(data["operation"], "recovery_required");
    assert_eq!(data["preservation"], "uncaptured");
    assert_eq!(data["review"], "starting_point");
    before.assert_unchanged(&support::oracle::snapshot(&repo), "a staged status");

    // A conflicted merge is the same recovery question with more evidence.
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("seed");
    init_notes_scope(&repo);
    repo.git_ok(["checkout", "-q", "-b", "side"]);
    repo.write_file("notes/a.md", b"# Side\n");
    repo.commit_all("side edits");
    repo.git_ok(["checkout", "-q", "main"]);
    repo.write_file("notes/a.md", b"# Main\n");
    repo.commit_all("main edits");
    let merge = repo.git(["merge", "side"]);
    assert!(!merge.status.success(), "the merge conflicts");
    let before = support::oracle::snapshot(&repo);
    let output = margin(repo.env(), repo.path(), &["status", "--json"]);
    assert_eq!(output.status.code(), Some(0), "the query still observes");
    let data = data_of(&output, "status inside a conflicted merge");
    assert_eq!(data["operation"], "recovery_required");
    let blockers = data["blockers"].to_string();
    assert!(
        blockers.contains("merge"),
        "the blockers report it: {blockers}"
    );
    assert!(
        blockers.contains("conflict"),
        "the conflicts are named: {blockers}"
    );
    before.assert_unchanged(&support::oracle::snapshot(&repo), "a conflicted status");
}

#[test]
fn a_branch_mismatch_is_one_actionable_blocker() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("seed");
    init_notes_scope(&repo);
    // Another branch at the same commit with an unsaved edit: the query
    // observes both the branch change and the uncaptured content.
    repo.git_ok(["switch", "--quiet", "--create", "other"]);
    repo.write_file("notes/a.md", b"# One edited\n");
    let before = support::oracle::snapshot(&repo);
    let index_before = index_bytes(&repo);

    // The query never refuses on data: the current branch, the stored
    // adoption, and one blocker that names the explicit init which adopts
    // the branch (SPEC 6.3, 8.2).
    let output = margin(repo.env(), repo.path(), &["status", "--json"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "a mismatched branch stays one successful query"
    );
    let data = data_of(&output, "a mismatched status");
    assert_eq!(data["branch"], "refs/heads/other");
    assert_eq!(data["settings"]["branch"], "main");
    assert_eq!(data["eligible"], false);
    assert_eq!(
        data["operation"], "idle",
        "a switched branch is not recovery state"
    );
    assert_eq!(data["preservation"], "uncaptured");
    assert_eq!(data["review"], "starting_point");
    let blockers = data["blockers"].to_string();
    assert!(
        blockers.contains("other"),
        "the blocker names the current branch: {blockers}"
    );
    assert!(
        blockers.contains("main"),
        "the blocker names the adopted branch: {blockers}"
    );
    assert!(
        blockers.contains("init"),
        "the blocker names the adoption path: {blockers}"
    );

    // The query leaves refs, working bytes, and the raw index untouched
    // (INV-12).
    before.assert_unchanged(&support::oracle::snapshot(&repo), "a mismatched status");
    assert_eq!(index_bytes(&repo), index_before);
}

#[test]
fn a_path_filter_is_presentation_only() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.write_file("drafts/b.md", b"# Two\n");
    repo.commit_all("seed");
    let output = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes", "--scope", "drafts", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));
    repo.write_file("notes/a.md", b"# One edited\n");
    repo.write_file("drafts/b.md", b"# Two edited\n");

    let query = |args: &[&str]| {
        let output = margin(repo.env(), repo.path(), args);
        assert_eq!(output.status.code(), Some(0));
        data_of(&output, "a filtered status")
    };

    // No filter: every uncaptured path shows.
    let data = query(&["status", "--json"]);
    assert_eq!(
        data["counts"]["uncaptured_paths"],
        serde_json::json!(["drafts/b.md", "notes/a.md"])
    );
    assert_eq!(data["counts"]["uncaptured_total"], 2);

    // One directory prefix narrows the list; the totals, the states, and
    // the pending counts stay whole-scope (SPEC 6.3, 7.2).
    for (filter, expected) in [
        (&["--path", "notes"][..], serde_json::json!(["notes/a.md"])),
        // An exact file path is its own prefix, and a trailing slash or a
        // sibling name is not a component match.
        (
            &["--path", "notes/a.md"][..],
            serde_json::json!(["notes/a.md"]),
        ),
        (&["--path", "notes/"][..], serde_json::json!(["notes/a.md"])),
        (&["--path", "note"][..], serde_json::json!([])),
        (&["--path", "nowhere"][..], serde_json::json!([])),
        // `.` covers the whole scope.
        (
            &["--path", "."][..],
            serde_json::json!(["drafts/b.md", "notes/a.md"]),
        ),
    ] {
        let data = query(&["status", "--json", filter[0], filter[1]]);
        assert_eq!(
            data["counts"]["uncaptured_paths"], expected,
            "filter {filter:?}"
        );
        assert_eq!(data["counts"]["uncaptured_total"], 2, "filter {filter:?}");
        assert_eq!(data["preservation"], "uncaptured", "filter {filter:?}");
    }
}

#[test]
fn preservation_compares_working_bytes_not_index_dirt() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("seed");
    init_notes_scope(&repo);

    // A staged change the working tree no longer holds: the index is dirty
    // — recovery state — but the observed working bytes are in the
    // candidate, so preservation stays captured (SPEC 4.2 compares the
    // observed working content, never the index).
    repo.write_file("notes/a.md", b"# Two\n");
    repo.git_ok(["add", "--", "notes/a.md"]);
    repo.write_file("notes/a.md", b"# One\n");
    let output = margin(repo.env(), repo.path(), &["status", "--json"]);
    let data = data_of(&output, "a reverted working file over a staged edit");
    assert_eq!(data["preservation"], "captured");
    assert_eq!(data["operation"], "recovery_required", "the index is dirty");
    assert_eq!(data["counts"]["uncaptured_paths"], serde_json::json!([]));

    // The mirror: the staged entry matches the candidate while the working
    // file moved on, so the working content is uncaptured whatever the
    // index claims.
    repo.git_ok(["add", "--", "notes/a.md"]);
    repo.write_file("notes/a.md", b"# Two\n");
    let output = margin(repo.env(), repo.path(), &["status", "--json"]);
    let data = data_of(&output, "a moved-on working file over a staged revert");
    assert_eq!(data["preservation"], "uncaptured");
    assert_eq!(data["counts"]["uncaptured_total"], 1);
}

#[test]
fn text_mode_reports_the_three_states_in_one_line() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("seed");
    init_notes_scope(&repo);
    repo.write_file("notes/a.md", b"# One edited\n");
    let output = margin(repo.env(), repo.path(), &["status"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "margin: preservation=uncaptured review=starting_point operation=idle \
         branch=refs/heads/main\n"
    );
}
