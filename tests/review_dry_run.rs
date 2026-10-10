//! Real-Git tests for review dry-run result trees (SPEC 6.4, 9.3, T029).
//!
//! Every case proves one contract of the intended result against the
//! installed Git:
//!
//! - keep-all intends the candidate's own tree, and a review with nothing
//!   pending intends it too
//! - restore-all and every partial selection intend the exact recorded
//!   result: the expected bytes come from construction-level line
//!   replacement, the expected trees from `git hash-object` plus
//!   `git mktree`, and an out-of-scope change between B and C stays with
//!   the candidate, never with the baseline
//! - the dry run preserves the real repository: refs, HEAD, the index,
//!   working bytes and modes, the commit count, and the approval state the
//!   baseline walk selects
//! - scoped working differences, staged changes, a foreign branch, an
//!   unknown change identity, and a stale review refuse with their typed
//!   codes and unchanged effects
//! - the installed executable reports one envelope in JSON mode and one
//!   line in text mode, and both object formats work
//!
//! The oracles stay independent of the implementation (SPEC 16.3): trees
//! come from `git rev-parse`, `git ls-tree`, and `git mktree`; blob
//! identities from `git hash-object`; the repository state from
//! `git for-each-ref`, `git write-tree`, `git status`, and raw disk bytes.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use margin::app::{
    BaselineStatus, ChangeDetail, DiffRequest, Effects, InitRequest, InspectRequest,
    OperationOutcome, Operations, ReviewDecision, ReviewOutcome, ReviewRequest,
};
use margin::domain::{ReviewState, ScopeInput};
use margin::git::Git;
use serde_json::{Value, json};
use support::env::{IsolatedEnv, is_scrubbed};
use support::repo::{ObjectFormat, RepoOptions, TestRepo};
use support::{c0, oracle, text};

// ---------------------------------------------------------------------------
// The shared fixture
// ---------------------------------------------------------------------------

/// One document with two independently reviewable passages, each inside its
/// own gap paragraph: `passage(0)` sits between the upper and lower gaps.
fn document(title: &str, first: &str, second: &str) -> String {
    let token = title.to_lowercase().replace(' ', "-");
    let mut out = format!("# {title}\n\n");
    for line in 1..=6 {
        out.push_str(&format!("gap {token} upper {line}\n"));
    }
    out.push_str(&format!("\n{first}\n"));
    for line in 1..=6 {
        out.push_str(&format!("gap {token} lower {line}\n"));
    }
    out.push_str(&format!("\n{second}\n"));
    out
}

/// The four sides of the fixture: each document revises both passages, one
/// by growing and one by shrinking, so every partial selection has its own
/// exact expected bytes.
struct Fixture {
    repo: TestRepo,
    /// The start marker the first-parent walk anchors at: B.
    start: String,
    /// The committed candidate: C.
    candidate: String,
    before_a: String,
    after_a: String,
    before_b: String,
    after_b: String,
}

/// The independent expected-tree oracle: the candidate's entries with
/// `replacements` applied, written through `git mktree`.
fn expected_tree(
    repo: &TestRepo,
    candidate: &str,
    replacements: &BTreeMap<&str, Option<(String, String)>>,
) -> String {
    let mut input = String::new();
    for entry in oracle::ls_tree(repo, candidate) {
        let record = match replacements.get(entry.path.as_str()) {
            Some(Some((mode, oid))) => format!("{mode} blob {oid}"),
            Some(None) => continue,
            None => format!("{} {} {}", entry.mode, entry.kind, entry.oid),
        };
        input.push_str(&record);
        input.push('\t');
        input.push_str(&entry.path);
        input.push('\n');
    }
    text(&repo.git_stdin(["mktree"], input.as_bytes()))
}

/// Hashes expected bytes through stock Git and returns their blob OID.
fn blob_of(repo: &TestRepo, bytes: &[u8]) -> String {
    text(&repo.git_stdin(["hash-object", "-w", "--no-filters", "--stdin"], bytes))
}

/// The `mode oid` replacement for one expected document.
fn replacement(repo: &TestRepo, bytes: &[u8]) -> Option<(String, String)> {
    Some(("100644".to_string(), blob_of(repo, bytes)))
}

/// Builds the fixture: two scoped documents with two passages each and one
/// out-of-scope note that changes between B and C, so no restoration may
/// trade the note's candidate entry for the baseline's absence.
fn fixture(options: RepoOptions) -> Fixture {
    let repo = TestRepo::init_with(options);
    let before_a = document("Doc A", "first original passage", "second original passage");
    let after_a = document(
        "Doc A",
        "first revised passage\nwith two lines",
        "second revised passage",
    );
    let before_b = document(
        "Doc B",
        "first original paragraph",
        "second original paragraph",
    );
    let after_b = document(
        "Doc B",
        "first revised paragraph",
        "second revised paragraph with two lines",
    );
    repo.write_file("doc-a.md", before_a.as_bytes());
    repo.write_file("doc-b.md", before_b.as_bytes());
    repo.write_file("note.txt", b"the baseline note\n");
    let start = initialize(&repo);
    repo.write_file("doc-a.md", after_a.as_bytes());
    repo.write_file("doc-b.md", after_b.as_bytes());
    repo.write_file("note.txt", b"the candidate note\n");
    let candidate = repo.commit_all("writing pass");
    Fixture {
        repo,
        start,
        candidate,
        before_a,
        after_a,
        before_b,
        after_b,
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

/// The pinned comparison of the fixture: four change groups, one per
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
    data
}

/// The change identity of one passage of one document: `passage` 0 is the
/// first in source order, found through the recorded source ranges.
fn group_of(pinned: &ReviewOutcome, path: &str, passage: usize) -> String {
    let mut groups: Vec<&ChangeDetail> = pinned
        .changes
        .iter()
        .filter(|change| change.path == path)
        .collect();
    assert_eq!(groups.len(), 2, "{path} holds two passages");
    groups.sort_by_key(|change| change.ranges[0].old_start);
    groups[passage].change_id.clone()
}

/// Runs one dry run of `restore` against the pinned comparison and returns
/// the typed outcome.
fn decide(
    repo: &TestRepo,
    pinned: &ReviewOutcome,
    restore: &[String],
) -> OperationOutcome<ReviewDecision> {
    let git = adapter_for(repo);
    let operations = Operations::new(&git);
    let plan = json!({
        "schema_version": 1,
        "request_id": "dry-run-decision-01",
        "review_id": pinned.review_id,
        "branch_ref": pinned.branch_ref,
        "scope_id": pinned.scope_id,
        "baseline_oid": pinned.baseline_oid,
        "candidate_oid": pinned.candidate_oid,
        "restore": restore,
        "keep_remaining": true,
    })
    .to_string();
    operations.validate_review(&ReviewRequest {
        scope: None,
        plan: plan.into_bytes(),
        dry_run: true,
    })
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
// Intended trees
// ---------------------------------------------------------------------------

/// Keep-all intends the candidate's own tree and reports the candidate's
/// scoped result (SPEC 9.3).
#[test]
fn keep_all_intends_the_candidate_tree_and_writes_nothing() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let before = oracle::snapshot(&fixture.repo);

    let outcome = decide(&fixture.repo, &pinned, &[]);
    assert!(outcome.ok(), "keep-all validates: {:?}", outcome.error);
    let data = outcome.data.as_ref().unwrap();
    assert!(data.dry_run, "the operation is the dry run");
    assert_eq!(data.restored, Vec::<String>::new());
    assert_eq!(data.kept, Some(4), "every group stays");
    assert_eq!(data.paths, Vec::<String>::new(), "nothing is rewritten");
    assert_eq!(
        data.expected_tree,
        oracle::commit_tree(&fixture.repo, &fixture.candidate),
        "keep-all intends the candidate tree"
    );
    assert_eq!(
        data.result_scope_digest, pinned.candidate_scope_digest,
        "the scoped result of keep-all is the candidate's scoped content"
    );
    assert_eq!(outcome.effects, Effects::unchanged());
    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "a keep-all dry run");
}

/// A review with nothing pending still validates a complete keep-all plan
/// and intends the candidate tree (SPEC 6.4, 9.3).
#[test]
fn a_review_with_nothing_pending_intends_the_candidate_tree() {
    let fixture = fixture(RepoOptions::new());
    // Discard the writing pass: HEAD becomes the start marker again.
    fixture
        .repo
        .git_ok(["reset", "--quiet", "--hard", &fixture.start]);
    let head = oracle::head_oid(&fixture.repo).expect("HEAD is born");
    assert_eq!(head, fixture.start);
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    assert_eq!(pinned.scope_totals.groups, 0, "nothing is pending");

    let outcome = decide(&fixture.repo, &pinned, &[]);
    assert!(
        outcome.ok(),
        "an empty review validates: {:?}",
        outcome.error
    );
    let data = outcome.data.as_ref().unwrap();
    assert_eq!(data.kept, Some(0));
    assert_eq!(data.restored, Vec::<String>::new());
    assert_eq!(
        data.expected_tree,
        oracle::commit_tree(&fixture.repo, &fixture.start),
        "the intended tree is the anchored tree itself"
    );
}

/// Restore-all recomposes the baseline's scoped entries while the
/// out-of-scope change stays with the candidate (SPEC 9.3, INV-07).
#[test]
fn restore_all_intends_the_recomposed_scope_with_the_candidate_neighbors() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let all: Vec<String> = pinned
        .changes
        .iter()
        .map(|change| change.change_id.clone())
        .collect();
    let before = oracle::snapshot(&fixture.repo);

    let outcome = decide(&fixture.repo, &pinned, &all);
    assert!(outcome.ok(), "restore-all validates: {:?}", outcome.error);
    let data = outcome.data.as_ref().unwrap();
    assert_eq!(data.restored.len(), 4);
    assert_eq!(data.kept, Some(0));
    assert_eq!(
        data.paths,
        vec!["doc-a.md".to_string(), "doc-b.md".to_string()]
    );

    let mut replacements = BTreeMap::new();
    replacements.insert(
        "doc-a.md",
        replacement(&fixture.repo, fixture.before_a.as_bytes()),
    );
    replacements.insert(
        "doc-b.md",
        replacement(&fixture.repo, fixture.before_b.as_bytes()),
    );
    let wanted = expected_tree(&fixture.repo, &fixture.candidate, &replacements);
    assert_eq!(
        data.expected_tree, wanted,
        "restore-all intends B's documents"
    );
    // The note is absent at B and changed at C: the intended tree is
    // neither tree alone, so the out-of-scope change provably stays.
    assert_ne!(
        data.expected_tree,
        oracle::commit_tree(&fixture.repo, &fixture.start),
        "the out-of-scope note must not vanish with the scoped restoration"
    );
    assert_ne!(
        data.result_scope_digest, pinned.candidate_scope_digest,
        "restoring every passage changes the scoped result"
    );
    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "a restore-all dry run");
}

/// Every partial selection intends the exact recorded passages: expected
/// bytes by construction, expected trees through the mktree oracle (SPEC
/// 9.3, 10.6).
#[test]
fn partial_selections_intend_the_exact_recorded_passages() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let a_first = group_of(&pinned, "doc-a.md", 0);
    let a_second = group_of(&pinned, "doc-a.md", 1);
    let b_first = group_of(&pinned, "doc-b.md", 0);
    let b_second = group_of(&pinned, "doc-b.md", 1);

    let reverted_a_first = document("Doc A", "first original passage", "second revised passage");
    let reverted_a_second = document(
        "Doc A",
        "first revised passage\nwith two lines",
        "second original passage",
    );
    let reverted_b_first = document(
        "Doc B",
        "first original paragraph",
        "second revised paragraph with two lines",
    );
    let reverted_b_second = document(
        "Doc B",
        "first revised paragraph",
        "second original paragraph",
    );

    let cases: Vec<(&str, Vec<String>, String, String)> = vec![
        (
            "the first passage of doc-a",
            vec![a_first.clone()],
            reverted_a_first.clone(),
            fixture.after_b.clone(),
        ),
        (
            "the second passage of doc-a",
            vec![a_second.clone()],
            reverted_a_second.clone(),
            fixture.after_b.clone(),
        ),
        (
            "one passage of each document",
            vec![a_second.clone(), b_first.clone()],
            reverted_a_second.clone(),
            reverted_b_first,
        ),
        (
            "the other passage of each document",
            vec![a_first.clone(), b_second.clone()],
            reverted_a_first,
            reverted_b_second,
        ),
    ];
    for (what, restore, expected_a, expected_b) in cases {
        let outcome = decide(&fixture.repo, &pinned, &restore);
        assert!(outcome.ok(), "{what} validates: {:?}", outcome.error);
        let data = outcome.data.as_ref().unwrap();
        let mut replacements = BTreeMap::new();
        replacements.insert(
            "doc-a.md",
            replacement(&fixture.repo, expected_a.as_bytes()),
        );
        replacements.insert(
            "doc-b.md",
            replacement(&fixture.repo, expected_b.as_bytes()),
        );
        assert_eq!(
            data.expected_tree,
            expected_tree(&fixture.repo, &fixture.candidate, &replacements),
            "{what}: the intended tree is the exact recorded result"
        );
        // The decisions return in manifest order, whatever the plan order.
        let decided: Vec<String> = pinned
            .changes
            .iter()
            .filter(|change| restore.contains(&change.change_id))
            .map(|change| change.change_id.clone())
            .collect();
        assert_eq!(data.restored, decided, "{what}: manifest order");
        assert_eq!(
            data.kept,
            Some(4 - decided.len()),
            "{what}: the rest is kept"
        );
    }
}

// ---------------------------------------------------------------------------
// Preserved state
// ---------------------------------------------------------------------------

/// The dry run preserves refs, HEAD, the real index, working files, the
/// commit count, and the approval state (SPEC 6.4, 9.3).
#[test]
fn the_dry_run_preserves_every_real_dimension() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let all: Vec<String> = pinned
        .changes
        .iter()
        .map(|change| change.change_id.clone())
        .collect();
    let before = oracle::snapshot(&fixture.repo);
    let commits_before = fixture.repo.git_ok(["rev-list", "--first-parent", "HEAD"]);

    let outcome = decide(&fixture.repo, &pinned, &all);
    assert!(outcome.ok(), "{:?}", outcome.error);
    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "a dry run");

    // Stock Git: the index still matches HEAD's tree and nothing is dirty.
    oracle::assert_index_matches_tree(&fixture.repo, "HEAD");
    assert_eq!(text(&fixture.repo.git_ok(["status", "--porcelain"])), "");
    assert_eq!(
        fixture.repo.git_ok(["rev-list", "--first-parent", "HEAD"]),
        commits_before,
        "no commit joined the history"
    );

    // The approval state: the walk still anchors at the start marker and
    // the review stays pending, so nothing was approved (SPEC 4.2, 6.4).
    let git = adapter_for(&fixture.repo);
    let operations = Operations::new(&git);
    let status = operations.inspect_workspace(&InspectRequest {
        scope: None,
        path_filter: Vec::new(),
    });
    assert!(status.ok(), "{:?}", status.error);
    let data = status.data.as_ref().unwrap();
    assert_eq!(data.head.as_deref(), Some(fixture.candidate.as_str()));
    assert_eq!(data.review, ReviewState::Pending);
    match &data.baseline {
        BaselineStatus::Anchored(marker) => assert_eq!(marker.commit, fixture.start),
        other => panic!("the start marker still anchors the scope: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Typed refusals
// ---------------------------------------------------------------------------

/// Scoped working differences refuse the dry run before anything is
/// computed as actionable (SPEC 9.3).
#[test]
fn scoped_working_differences_refuse_the_dry_run() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let all: Vec<String> = pinned
        .changes
        .iter()
        .map(|change| change.change_id.clone())
        .collect();

    // An uncaptured scoped edit: newer bytes belong to the next checkpoint.
    fixture.repo.write_file(
        "doc-a.md",
        fixture.after_a.replace("gap", "edited gap").as_bytes(),
    );
    let dirty = oracle::snapshot(&fixture.repo);
    let outcome = decide(&fixture.repo, &pinned, &all);
    assert_refusal(&outcome, "DIRTY_SCOPE", "uncaptured scoped content");
    dirty.assert_unchanged(&oracle::snapshot(&fixture.repo), "a refused dry run");

    // Through the executable: the unmet precondition is exit class 4.
    let plan = plan_file(&fixture, "dirty.json", &all, &pinned);
    let (exit, code, envelope) = refusal(
        &fixture.repo,
        &["review", "--input", &plan, "--dry-run", "--json"],
    );
    assert_eq!(exit, 4);
    assert_eq!(code, "DIRTY_SCOPE");
    assert_eq!(envelope["effects"]["working_tree"], "unchanged");
    assert_eq!(envelope["effects"]["commit"], "none");
    dirty.assert_unchanged(&oracle::snapshot(&fixture.repo), "a refused executable run");
}

/// Staged changes and a foreign branch refuse with the shared guard codes
/// (SPEC 6.4, 9.1 step 4).
#[test]
fn staged_changes_and_a_foreign_branch_refuse_the_dry_run() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let all: Vec<String> = pinned
        .changes
        .iter()
        .map(|change| change.change_id.clone())
        .collect();

    // A staged out-of-scope entry still blocks: the application commits
    // through the real index.
    fixture
        .repo
        .write_file("note.txt", b"staged candidate note\n");
    fixture.repo.git_ok(["add", "note.txt"]);
    let staged = decide(&fixture.repo, &pinned, &all);
    assert_refusal(&staged, "STAGED_CHANGES", "a staged entry");
    fixture
        .repo
        .git_ok(["restore", "--quiet", "--staged", "note.txt"]);
    fixture.repo.write_file("note.txt", b"the candidate note\n");

    // A branch other than the adopted one, even at the same commit. The
    // plan is rebuilt against the branch the workspace is on, so the
    // adopted-branch guard of the shared inspection is what refuses.
    fixture
        .repo
        .git_ok(["switch", "--quiet", "--create", "other"]);
    let switched_pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let switched_all: Vec<String> = switched_pinned
        .changes
        .iter()
        .map(|change| change.change_id.clone())
        .collect();
    let switched = decide(&fixture.repo, &switched_pinned, &switched_all);
    assert_refusal(&switched, "BRANCH_CHANGED", "a foreign branch");
}

/// An unknown identity and a stale review refuse before any construction
/// (SPEC 9.3).
#[test]
fn unknown_and_stale_decisions_refuse_without_writing() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);

    // One identity the pinned review never issued.
    let stranger = pinned.changes[0].change_id.replace('0', "f");
    let unknown = decide(&fixture.repo, &pinned, std::slice::from_ref(&stranger));
    assert_refusal(&unknown, "UNKNOWN_CHANGE", "an unknown identity");
    assert!(
        unknown.error.as_ref().unwrap().message.contains(&stranger),
        "the refusal names the identity"
    );

    // The writer moved the candidate: the pinned comparison the plan names
    // is gone, so the plan is stale rather than silently reused.
    let all: Vec<String> = pinned
        .changes
        .iter()
        .map(|change| change.change_id.clone())
        .collect();
    fixture.repo.write_file(
        "doc-a.md",
        fixture.after_a.replace("passage", "section").as_bytes(),
    );
    fixture.repo.commit_all("another writing pass");
    let moved = oracle::snapshot(&fixture.repo);
    let stale = decide(&fixture.repo, &pinned, &all);
    assert_refusal(&stale, "STALE_REVIEW", "a moved candidate");
    moved.assert_unchanged(&oracle::snapshot(&fixture.repo), "a refused stale dry run");
}

// ---------------------------------------------------------------------------
// The executable and both object formats
// ---------------------------------------------------------------------------

/// The installed executable reports one envelope in JSON mode and one line
/// in text mode (SPEC 6.4, 7.1).
#[test]
fn the_executable_reports_the_dry_run_once() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let all: Vec<String> = pinned
        .changes
        .iter()
        .map(|change| change.change_id.clone())
        .collect();
    let plan = plan_file(&fixture, "plan.json", &all, &pinned);

    let output = margin(
        fixture.repo.env(),
        fixture.repo.path(),
        &["review", "--input", &plan, "--dry-run", "--json"],
    );
    let envelope = envelope_of(&output, "a dry run through the executable");
    assert_eq!(envelope["command"], "review");
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["data"]["dry_run"], true);
    assert_eq!(envelope["data"]["restored"].as_array().unwrap().len(), 4);
    let mut replacements = BTreeMap::new();
    replacements.insert(
        "doc-a.md",
        replacement(&fixture.repo, fixture.before_a.as_bytes()),
    );
    replacements.insert(
        "doc-b.md",
        replacement(&fixture.repo, fixture.before_b.as_bytes()),
    );
    assert_eq!(
        envelope["data"]["expected_tree"],
        expected_tree(&fixture.repo, &fixture.candidate, &replacements),
        "the envelope carries the intended tree"
    );
    assert_eq!(envelope["effects"]["working_tree"], "unchanged");
    assert_eq!(envelope["effects"]["index"], "unchanged");
    assert_eq!(envelope["effects"]["commit"], "none");
    assert_eq!(envelope["effects"]["repository"], "unchanged");
    assert_eq!(envelope["effects"]["configuration"], "unchanged");

    let output = margin(
        fixture.repo.env(),
        fixture.repo.path(),
        &["review", "--input", &plan, "--dry-run"],
    );
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("margin:"), "one text line: {stdout}");
    assert!(stdout.contains(&expected_tree(
        &fixture.repo,
        &fixture.candidate,
        &replacements
    )));
    assert!(stdout.ends_with('\n'));
}

/// Both documented object formats construct their intended trees (SPEC
/// 16.5).
#[test]
fn dry_runs_work_in_sha256_repositories() {
    let fixture = fixture(RepoOptions::new().with_object_format(ObjectFormat::Sha256));
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let all: Vec<String> = pinned
        .changes
        .iter()
        .map(|change| change.change_id.clone())
        .collect();
    let outcome = decide(&fixture.repo, &pinned, &all);
    assert!(outcome.ok(), "{:?}", outcome.error);
    let data = outcome.data.as_ref().unwrap();
    let mut replacements = BTreeMap::new();
    replacements.insert(
        "doc-a.md",
        replacement(&fixture.repo, fixture.before_a.as_bytes()),
    );
    replacements.insert(
        "doc-b.md",
        replacement(&fixture.repo, fixture.before_b.as_bytes()),
    );
    assert_eq!(
        data.expected_tree,
        expected_tree(&fixture.repo, &fixture.candidate, &replacements)
    );
}

// ---------------------------------------------------------------------------
// The frozen corpus
// ---------------------------------------------------------------------------

/// Writes one side of a corpus fixture into the working tree, with the
/// recorded executable bit and absence.
fn write_side(repo: &TestRepo, path: &str, side: &c0::Side) {
    match side.bytes() {
        Some(content) => {
            let target = repo.write_file(path, content);
            let current = fs::metadata(&target)
                .unwrap_or_else(|error| panic!("cannot stat {}: {error}", target.display()))
                .permissions()
                .mode();
            let wanted = side.mode().unwrap_or(0);
            fs::set_permissions(
                &target,
                fs::Permissions::from_mode((current & !0o111) | (wanted & 0o111)),
            )
            .unwrap_or_else(|error| panic!("cannot chmod {}: {error}", target.display()));
        }
        None => {
            let target = repo.file_path(path);
            if target.exists() {
                fs::remove_file(&target)
                    .unwrap_or_else(|error| panic!("cannot remove {}: {error}", target.display()));
            }
        }
    }
}

/// The storage mode Git records for the permission bits on disk.
fn git_mode(mode: u32) -> &'static str {
    if mode & 0o111 == 0 {
        "100644"
    } else {
        "100755"
    }
}

/// Proves one sampled corpus fixture: restoring its first recorded group
/// alone intends the exact recorded restoration, against the mktree oracle
/// (SPEC 16.7, 10.6).
fn check_corpus_fixture(fixture: &c0::Fixture) {
    let repo = TestRepo::init();
    write_side(&repo, fixture.path, &fixture.before);
    let start = initialize(&repo);
    write_side(&repo, fixture.path, &fixture.after);
    let candidate = repo.commit_all("candidate");

    let pinned = pinned_review_of(&repo, &start);
    let groups: Vec<&ChangeDetail> = pinned
        .changes
        .iter()
        .filter(|change| change.path == fixture.path)
        .collect();
    assert_eq!(
        groups.len(),
        pinned.changes.len(),
        "{}: every group belongs to the fixture document",
        fixture.id
    );
    let support::c0::Expectation::Groups { groups: recorded } = &fixture.expectation else {
        panic!(
            "{}: only independent-passage fixtures are sampled",
            fixture.id
        );
    };
    assert_eq!(
        groups.len(),
        recorded.len(),
        "{}: the manifest groups as recorded",
        fixture.id
    );

    // The dry run of the first group alone.
    let restore = vec![groups[0].change_id.clone()];
    let outcome = decide(&repo, &pinned, &restore);
    assert!(
        outcome.ok(),
        "{}: the dry run validates: {:?}",
        fixture.id,
        outcome.error
    );
    let data = outcome.data.as_ref().unwrap();
    let mut replacements = BTreeMap::new();
    replacements.insert(
        fixture.path,
        Some((
            git_mode(fixture.after.mode().unwrap_or(0)).to_string(),
            blob_of(&repo, recorded[0].restore.bytes()),
        )),
    );
    assert_eq!(
        data.expected_tree,
        expected_tree(&repo, &candidate, &replacements),
        "{}: the intended tree is the recorded restoration",
        fixture.id
    );
}

/// A deterministic sample of the corpus's multi-group fixtures reaches the
/// dry run, spanning the content classes of SPEC 16.7 without replaying the
/// full restoration property of task T025.
#[test]
fn corpus_fixtures_restore_their_recorded_group_through_the_dry_run() {
    let multi: Vec<&c0::Fixture> = support::c0::iter()
        .filter(|fixture| {
            matches!(&fixture.expectation, c0::Expectation::Groups { groups } if groups.len() >= 2)
        })
        .collect();
    assert!(
        multi.len() >= 12,
        "the corpus holds many multi-group fixtures"
    );
    let sample: Vec<&&c0::Fixture> = multi.iter().step_by(2).collect();
    for fixture in sample {
        check_corpus_fixture(fixture);
    }
}

// ---------------------------------------------------------------------------
// Executable helpers
// ---------------------------------------------------------------------------

/// Writes one complete plan document beside the repository, so no plan
/// file ever joins the working tree.
fn plan_file(fixture: &Fixture, name: &str, restore: &[String], pinned: &ReviewOutcome) -> String {
    let document = json!({
        "schema_version": 1,
        "request_id": "dry-run-decision-01",
        "review_id": pinned.review_id,
        "branch_ref": pinned.branch_ref,
        "scope_id": pinned.scope_id,
        "baseline_oid": pinned.baseline_oid,
        "candidate_oid": pinned.candidate_oid,
        "restore": restore,
        "keep_remaining": true,
    })
    .to_string();
    let path = fixture.repo.root().join(name);
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
