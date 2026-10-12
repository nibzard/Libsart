//! Real-Git tests for the recovery inspection (SPEC 9.7, task T034).
//!
//! Every case proves one contract of `doctor` against the installed Git:
//!
//! - the report observes the head, the valid baseline of the one walk, the
//!   index tree, the working-scope fingerprint, the staged paths, and the
//!   receipt state of history — each value checked against stock Git or the
//!   shared identities over stock-Git entry data, never against a second
//!   Margin parser
//! - the execution leaves refs, the index, and every working byte
//!   unchanged: `oracle::snapshot` proves the equality before and after,
//!   the observation the SPEC 15.7 `InspectRecovery` row requires, and the
//!   raw index bytes prove the query changed no index byte a tree ID alone
//!   cannot see — the tree oracle computes from one disposable index copy,
//!   so no oracle prepares the index cache first (task T062)
//! - staged content receives the bounded recovery argv of SPEC 9.7: argv
//!   arrays assembled from the report's own observations, never a shell
//!   script; a workspace with nothing staged receives none, and the
//!   suggested capture argv runs verbatim through the installed
//!   executable: it commits the inspected state as one unreviewed draft,
//!   consumes the staging, keeps the baseline, and refuses the moved
//!   state on its identical retry (task T036)
//! - a completed review and a completed restoration each leave the receipt
//!   history holds, and the report names it with its request identifier
//! - unresolved conflicts stop the index tree without stopping the query,
//!   and a history context no marker may depend on makes the receipt state
//!   unknown instead of absent
//! - outside a repository and without an adopted scope the query refuses
//!   typed with unchanged effects, and the installed executable reports the
//!   same contract in JSON and text modes

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use margin::app::{
    BaselineStatus, DiffRequest, Effects, Identities, InitRequest, OperationOutcome, Operations,
    RecoveryInspection, RecoveryRequest, ReviewRequest,
};
use margin::domain::{CommitKind, ContentEntry, EntryMode, ScopeInput};
use margin::git::Git;
use serde_json::{Value, json};
use support::env::{IsolatedEnv, is_scrubbed};
use support::repo::{RepoOptions, TestRepo};
use support::{oracle, text};

// ---------------------------------------------------------------------------
// The shared fixture
// ---------------------------------------------------------------------------

/// The original document body of the seeded workspace.
const ORIGINAL: &str = "original passage
second original passage
";

/// The revised document body of the seeded workspace.
const REVISED: &str = "revised passage
second revised passage
";

/// The seeded workspace: a start marker A and a committed candidate C, with
/// one out-of-scope note that changed between them, so the recovery report
/// has a real baseline, a real candidate, and out-of-scope sentinels.
struct Fixture {
    repo: TestRepo,
    /// The start marker the first-parent walk anchors at: A.
    start: String,
    /// The committed candidate: C.
    candidate: String,
}

fn fixture(options: RepoOptions) -> Fixture {
    let repo = TestRepo::init_with(options);
    repo.write_file("doc-a.md", ORIGINAL.as_bytes());
    repo.write_file("note.txt", b"the baseline note\n");
    let start = initialize(&repo);
    repo.write_file("doc-a.md", REVISED.as_bytes());
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

/// One recovery inspection with the stored scope of the workspace.
fn doctor(repo: &TestRepo) -> OperationOutcome<RecoveryInspection> {
    let git = adapter_for(repo);
    Operations::new(&git).inspect_recovery(&RecoveryRequest { scope: None })
}

/// Asserts that the raw bytes of the real index survived one doctor query
/// exactly (SPEC 16.3, task T062): `git write-tree` on the real index
/// rewrites its cached-tree extension even under `GIT_OPTIONAL_LOCKS=0`, so
/// a staged fixture captured before any `write-tree` ran makes this
/// comparison bite.
fn assert_index_bytes_unchanged(repo: &TestRepo, before: &[u8], context: &str) {
    assert!(
        oracle::index_bytes(repo) == before,
        "{context}: the real index changed its own bytes"
    );
}

/// The pinned comparison of the fixture, for plans a review needs.
fn pinned_review(repo: &TestRepo) -> margin::app::ReviewOutcome {
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
    outcome.data.expect("a successful diff carries data")
}

/// One complete keep-all plan document for `pinned`.
fn keep_all_plan(pinned: &margin::app::ReviewOutcome, request: &str) -> String {
    json!({
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
fn apply_review(repo: &TestRepo, plan: &str) -> OperationOutcome<margin::app::ReviewDecision> {
    let git = adapter_for(repo);
    Operations::new(&git).apply_review(&ReviewRequest {
        scope: None,
        plan: plan.as_bytes().to_vec(),
        dry_run: false,
    })
}

/// One restoration plan document (SPEC 7.4) for `path` at `revision`.
fn restore_plan(repo: &TestRepo, request: &str, path: &str, revision: &str) -> String {
    let status = {
        let git = adapter_for(repo);
        let operations = Operations::new(&git);
        let status = operations.inspect_workspace(&margin::app::InspectRequest {
            scope: None,
            path_filter: Vec::new(),
        });
        assert!(status.ok(), "the status query succeeds: {:?}", status.error);
        status.data.unwrap()
    };
    json!({
        "schema_version": 1,
        "request_id": request,
        "branch_ref": status.branch.expect("an attached HEAD"),
        "scope_id": status.scope.digest,
        "candidate_oid": status.head.expect("a born branch"),
        "path": path,
        "revision": revision,
    })
    .to_string()
}

/// Restores one document through the operation layer.
fn restore(repo: &TestRepo, plan: &str) -> OperationOutcome<margin::app::RestoreOutcome> {
    let git = adapter_for(repo);
    Operations::new(&git).restore_document(&margin::app::RestoreRequest {
        scope: None,
        plan: plan.as_bytes().to_vec(),
        dry_run: false,
    })
}

/// The working-scope fingerprint oracle (SPEC 8.6): the present scoped
/// paths hashed by stock Git, with the storage mode read from the disk, fed
/// through the shared identities. Only the canonical encoding is shared
/// with the implementation; the entries come from Git and the filesystem.
fn working_digest_of(repo: &TestRepo, paths: &[&str]) -> String {
    let git = adapter_for(repo);
    let identities =
        Identities::new(&git).unwrap_or_else(|error| panic!("identities must read: {error}"));
    let entries: Vec<ContentEntry> = paths
        .iter()
        .map(|path| {
            let oid = text(&repo.git_ok(["hash-object", "--no-filters", "--", path]));
            let metadata = fs::metadata(repo.file_path(path)).expect("the scoped file must exist");
            let mode = if metadata.permissions().mode() & 0o100 != 0 {
                EntryMode::Executable
            } else {
                EntryMode::Regular
            };
            ContentEntry {
                path: path.to_string(),
                mode,
                blob_oid: oid,
            }
        })
        .collect();
    identities
        .content(&entries)
        .expect("the entries must hash")
        .id()
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
    assert_eq!(
        stdout.matches('\n').count(),
        1,
        "{what}: exactly one envelope line, got: {stdout:?}"
    );
    serde_json::from_str(stdout.trim_end())
        .unwrap_or_else(|error| panic!("{what}: the envelope must parse: {error}: {stdout}"))
}

// ---------------------------------------------------------------------------
// The observed report
// ---------------------------------------------------------------------------

/// The report observes the recovery state through read-only Git calls, and
/// its execution changes no ref, no index entry, and no working byte
/// (SPEC 9.7; the SPEC 15.7 `InspectRecovery` observation).
#[test]
fn doctor_reports_recovery_state_without_writing() {
    let fixture = fixture(RepoOptions::new());
    let before = oracle::snapshot(&fixture.repo);
    let index_bytes = oracle::index_bytes(&fixture.repo);

    let outcome = doctor(&fixture.repo);
    assert!(
        outcome.ok(),
        "the recovery inspection succeeds: {:?}",
        outcome.error
    );
    assert!(outcome.warnings.is_empty());
    assert_eq!(outcome.effects, Effects::unchanged());
    let data = outcome.data.as_ref().unwrap();

    // Every observed value answers the stock-Git oracle of the same fact.
    assert_eq!(data.object_format.as_str(), "sha1");
    assert_eq!(data.head.as_deref(), Some(fixture.candidate.as_str()));
    assert_eq!(
        data.head.as_deref(),
        oracle::head_oid(&fixture.repo).as_deref()
    );
    match &data.baseline {
        BaselineStatus::Anchored(marker) => {
            assert_eq!(
                marker.commit, fixture.start,
                "the walk anchors at the start"
            );
            assert_eq!(marker.kind, CommitKind::Start);
        }
        other => panic!("the baseline must anchor at the start marker: {other:?}"),
    }
    assert_eq!(
        data.index_tree.as_deref(),
        oracle::index_tree(&fixture.repo).as_deref(),
        "the index tree comes from write-tree"
    );

    // The working-scope fingerprint: raw working bytes hashed by stock Git,
    // and stable across two inspections of the same state.
    assert_eq!(
        data.scoped_working_digest
            .as_ref()
            .map(|digest| digest.id()),
        Some(working_digest_of(&fixture.repo, &["doc-a.md"])),
        "the fingerprint covers exactly the scoped working files"
    );
    let again = doctor(&fixture.repo);
    assert!(again.ok(), "the second inspection succeeds");
    assert_eq!(
        again.data.as_ref().unwrap().scoped_working_digest,
        data.scoped_working_digest,
        "the same state reports the same fingerprint"
    );

    // Nothing is staged and no request completed: both are answers.
    assert_eq!(data.staged_paths.total, 0);
    assert!(data.staged_paths.paths.is_empty());
    assert!(matches!(data.receipt, margin::app::ReceiptPresence::Absent));
    assert!(
        data.recovery_argv.is_empty(),
        "a workspace with nothing staged recovers nothing: {:?}",
        data.recovery_argv
    );

    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "the doctor query");
    assert_index_bytes_unchanged(&fixture.repo, &index_bytes, "the doctor query");
}

/// The report observes the real index tree without changing one real index
/// byte (task T062, SPEC 16.3): the bytes are captured immediately after
/// the staging wrote them — before any oracle or query could run
/// `write-tree` and prepare the cached tree — and both the operation layer
/// and the installed executable must leave them exactly as they were,
/// while the reported tree still answers the stock-Git oracle.
#[test]
fn doctor_reports_the_index_tree_without_touching_the_index() {
    let fixture = fixture(RepoOptions::new());
    fixture.repo.write_file("doc-a.md", b"staged revision\n");
    fixture.repo.git_ok(["add", "--", "doc-a.md"]);

    // Immediately after the staging: the index cache is still unprepared,
    // so one careless inspection would rewrite the file.
    let staged = oracle::index_bytes(&fixture.repo);
    let before = oracle::snapshot(&fixture.repo);

    let outcome = doctor(&fixture.repo);
    assert!(
        outcome.ok(),
        "the recovery inspection succeeds: {:?}",
        outcome.error
    );
    assert_eq!(outcome.effects, Effects::unchanged());
    let data = outcome.data.as_ref().unwrap();
    assert_eq!(
        data.index_tree.as_deref(),
        oracle::index_tree(&fixture.repo).as_deref(),
        "the reported tree answers the read-only stock-Git oracle"
    );
    assert_eq!(data.staged_paths.total, 1, "the staged edit is observed");

    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "the doctor query");
    assert_index_bytes_unchanged(&fixture.repo, &staged, "the doctor query");

    // The installed executable holds the same contract in machine mode.
    let json = margin(
        fixture.repo.env(),
        fixture.repo.path(),
        &["doctor", "--json"],
    );
    assert_eq!(json.status.code(), Some(0));
    assert_index_bytes_unchanged(&fixture.repo, &staged, "the doctor executable");

    // The fixture stays honest: stock `write-tree` on the real index still
    // rewrites the bytes, so this regression cannot pass vacuously.
    fixture.repo.git_ok(["write-tree"]);
    assert!(
        oracle::index_bytes(&fixture.repo) != staged,
        "stock write-tree rewrites the unprepared index, so the fixture proves the contract"
    );
}

/// The staged delta carries its bounded path list, and the recovery argv is
/// assembled from the report's own observations as argv arrays (SPEC 9.7).
#[test]
fn doctor_reports_staged_paths_and_safe_recovery_argv() {
    let fixture = fixture(RepoOptions::new());
    // One staged edit, one staged addition, and one unstaged change on top:
    // the working fingerprint must cover the working bytes, not the index.
    fixture.repo.write_file("doc-a.md", b"staged revision\n");
    fixture.repo.write_file("doc-b.md", b"a second document\n");
    fixture.repo.git_ok(["add", "--", "doc-a.md", "doc-b.md"]);
    // The staged index cache is still unprepared here, before any oracle or
    // query runs.
    let staged = oracle::index_bytes(&fixture.repo);
    fixture
        .repo
        .write_file("doc-a.md", b"staged revision\nand more\n");

    let before = oracle::snapshot(&fixture.repo);
    let outcome = doctor(&fixture.repo);
    assert!(
        outcome.ok(),
        "the recovery inspection succeeds: {:?}",
        outcome.error
    );
    assert_eq!(outcome.effects, Effects::unchanged());
    let data = outcome.data.as_ref().unwrap();

    let head = oracle::head_oid(&fixture.repo).unwrap();
    let index_tree = oracle::index_tree(&fixture.repo).unwrap();
    assert_eq!(data.head.as_deref(), Some(head.as_str()));
    assert_eq!(data.index_tree.as_deref(), Some(index_tree.as_str()));
    assert_eq!(data.staged_paths.total, 2);
    assert_eq!(
        data.staged_paths
            .paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<String>>(),
        vec!["doc-a.md".to_string(), "doc-b.md".to_string()],
        "the staged paths match git diff-index --cached"
    );
    assert_eq!(
        data.scoped_working_digest
            .as_ref()
            .map(|digest| digest.id()),
        Some(working_digest_of(&fixture.repo, &["doc-a.md", "doc-b.md"])),
        "the fingerprint hashes the working bytes, not the staged index"
    );

    // The recovery argv: both documented recovery paths, as argv arrays the
    // writer can run — never one shell script (SPEC 7.5, 9.7).
    let expected_capture = vec![
        "margin".to_string(),
        "checkpoint".to_string(),
        "--include-staged".to_string(),
        "--expect-head".to_string(),
        head.clone(),
        "--expect-index".to_string(),
        index_tree.clone(),
        "--expect-working".to_string(),
        data.scoped_working_digest
            .as_ref()
            .expect("the digest is known")
            .id(),
        "--json".to_string(),
    ];
    let expected_resume = vec![
        "margin".to_string(),
        "review".to_string(),
        "--resume".to_string(),
        "--input".to_string(),
        "plan.json".to_string(),
        "--json".to_string(),
    ];
    assert_eq!(data.recovery_argv, vec![expected_capture, expected_resume]);
    for argv in &data.recovery_argv {
        assert_eq!(argv.first().map(String::as_str), Some("margin"));
    }

    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "the doctor query");
    assert_index_bytes_unchanged(&fixture.repo, &staged, "the doctor query");
}

/// The suggested recovery argv is executable exactly as printed: the
/// writer whose application left its proved result staged runs the capture
/// command the report assembles, and that command commits the inspected
/// state as one unreviewed draft — the visible preview and the capture it
/// previews are one contract (SPEC 9.7, task T036).
#[test]
fn the_suggested_recovery_argv_captures_the_staged_state() {
    let fixture = fixture(RepoOptions::new());
    // The retained state of one interrupted application: the staged edit
    // and the working bytes are equal, exactly as a failed apply left both.
    fixture.repo.write_file(
        "doc-a.md",
        b"revised passage\nsecond revised passage\nnew closing\n",
    );
    fixture.repo.git_ok(["add", "--", "doc-a.md"]);
    let staged = oracle::index_bytes(&fixture.repo);

    let outcome = doctor(&fixture.repo);
    assert!(
        outcome.ok(),
        "the recovery inspection succeeds: {:?}",
        outcome.error
    );
    let data = outcome.data.as_ref().unwrap();
    assert_eq!(data.staged_paths.total, 1);
    assert_index_bytes_unchanged(&fixture.repo, &staged, "the doctor query");
    let index_tree = data.index_tree.clone().expect("the inspected index tree");
    let argv = &data.recovery_argv[0];
    assert_eq!(argv.first().map(String::as_str), Some("margin"));

    // The executable runs the argv the report printed, verbatim.
    let args: Vec<&str> = argv.iter().skip(1).map(String::as_str).collect();
    let output = margin(fixture.repo.env(), fixture.repo.path(), &args);
    assert_eq!(
        output.status.code(),
        Some(0),
        "the suggested capture succeeds: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope = envelope_of(&output, "the suggested recovery capture");
    assert_eq!(envelope["command"], "checkpoint");
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["data"]["created"], true);
    assert_eq!(envelope["data"]["changed"], true);
    assert_eq!(envelope["effects"]["commit"], "created");
    assert_eq!(envelope["effects"]["index"], "changed");
    assert_eq!(envelope["effects"]["working_tree"], "unchanged");
    let commit = envelope["data"]["commit"]
        .as_str()
        .expect("the draft commit")
        .to_string();

    // The commit is the inspected state exactly, proven with stock Git:
    // the tree is the reported index tree, the staging is consumed, and
    // the working bytes never moved (SPEC 9.7).
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(commit.as_str())
    );
    assert_eq!(oracle::commit_tree(&fixture.repo, &commit), index_tree);
    assert_eq!(
        text(
            &fixture
                .repo
                .git_ok(["diff-index", "--cached", "--name-only", "HEAD"])
        ),
        "",
        "the staging is consumed"
    );
    assert_eq!(
        fixture.repo.read_file("doc-a.md"),
        b"revised passage\nsecond revised passage\nnew closing\n",
        "no working byte moved"
    );

    // The draft confers no approval and the report reflects the new
    // candidate: nothing is staged anymore, and the baseline a fresh
    // review needs still anchors at the start marker.
    let consumed = oracle::index_bytes(&fixture.repo);
    let after = doctor(&fixture.repo);
    assert!(
        after.ok(),
        "the recovery inspection after the capture: {:?}",
        after.error
    );
    let report = after.data.as_ref().unwrap();
    assert_eq!(report.staged_paths.total, 0);
    assert_eq!(report.recovery_argv, Vec::<Vec<String>>::new());
    match &report.baseline {
        BaselineStatus::Anchored(marker) => assert_eq!(marker.commit, fixture.start),
        other => panic!("the draft confers no approval: {other:?}"),
    }
    assert_index_bytes_unchanged(
        &fixture.repo,
        &consumed,
        "the doctor query after the capture",
    );

    // The identical argv on the moved state is one stale-context refusal:
    // the fingerprints pin one capture, and `doctor` reports the new state
    // to inspect next (SPEC 9.7).
    let retry = margin(fixture.repo.env(), fixture.repo.path(), &args);
    assert_eq!(retry.status.code(), Some(3), "the repeated capture refuses");
    let envelope = envelope_of(&retry, "the repeated recovery capture");
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["error"]["code"], "STALE_STATE");
    assert_eq!(envelope["effects"]["commit"], "none");
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(commit.as_str())
    );
}

/// A completed review leaves the receipt history holds; the report names
/// the verified commit with its request, and the baseline advances to it
/// while no staged content suggests recovery (SPEC 8.7, 9.7).
#[test]
fn doctor_reports_the_receipt_of_a_completed_review() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review(&fixture.repo);
    let plan = keep_all_plan(&pinned, "review-keep-all-01");
    let applied = apply_review(&fixture.repo, &plan);
    assert!(
        applied.ok(),
        "the keep-all review applies: {:?}",
        applied.error
    );
    let commit = applied
        .data
        .as_ref()
        .unwrap()
        .commit
        .clone()
        .expect("the application commits");

    let before = oracle::snapshot(&fixture.repo);
    let index_bytes = oracle::index_bytes(&fixture.repo);
    let outcome = doctor(&fixture.repo);
    assert!(
        outcome.ok(),
        "the recovery inspection succeeds: {:?}",
        outcome.error
    );
    assert_eq!(outcome.effects, Effects::unchanged());
    let data = outcome.data.as_ref().unwrap();

    match &data.receipt {
        margin::app::ReceiptPresence::Present(found) => {
            assert_eq!(found.commit, commit, "the report names the receipt commit");
            assert_eq!(found.kind, CommitKind::Review);
            assert_eq!(found.request, "review-keep-all-01");
            assert_eq!(
                found.plan.as_deref(),
                Some(fingerprint_of(&fixture.repo, &plan).as_str())
            );
        }
        other => panic!("the completed review must be present: {other:?}"),
    }
    match &data.baseline {
        BaselineStatus::Anchored(marker) => {
            assert_eq!(marker.commit, commit, "the baseline advanced to the review");
            assert_eq!(marker.kind, CommitKind::Review);
        }
        other => panic!("the baseline must anchor at the review: {other:?}"),
    }
    assert_eq!(data.staged_paths.total, 0);
    assert!(data.recovery_argv.is_empty());

    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "the doctor query");
    assert_index_bytes_unchanged(&fixture.repo, &index_bytes, "the doctor query");
}

/// A completed restoration leaves a receipt of its own kind while the
/// review baseline stays at its anchor (SPEC 8.7, 9.5, 9.7).
#[test]
fn doctor_reports_the_receipt_of_a_completed_restoration() {
    let fixture = fixture(RepoOptions::new());
    let plan = restore_plan(
        &fixture.repo,
        "restore-doc-a-01",
        "doc-a.md",
        &fixture.start,
    );
    let restored = restore(&fixture.repo, &plan);
    assert!(
        restored.ok(),
        "the restoration applies: {:?}",
        restored.error
    );
    let commit = restored
        .data
        .as_ref()
        .unwrap()
        .commit
        .clone()
        .expect("the restoration commits");

    let index_bytes = oracle::index_bytes(&fixture.repo);
    let outcome = doctor(&fixture.repo);
    assert!(
        outcome.ok(),
        "the recovery inspection succeeds: {:?}",
        outcome.error
    );
    let data = outcome.data.as_ref().unwrap();
    match &data.receipt {
        margin::app::ReceiptPresence::Present(found) => {
            assert_eq!(found.commit, commit);
            assert_eq!(found.kind, CommitKind::Restore);
            assert_eq!(found.request, "restore-doc-a-01");
        }
        other => panic!("the completed restoration must be present: {other:?}"),
    }
    // A restoration never anchors the scope: the baseline keeps the start.
    match &data.baseline {
        BaselineStatus::Anchored(marker) => {
            assert_eq!(marker.commit, fixture.start);
            assert_eq!(marker.kind, CommitKind::Start);
        }
        other => panic!("the baseline must keep the start: {other:?}"),
    }
    assert_eq!(outcome.effects, Effects::unchanged());
    assert_index_bytes_unchanged(&fixture.repo, &index_bytes, "the doctor query");
}

/// Unresolved conflict stages stop `write-tree`, and the report carries
/// that as one observation — `None` — instead of failing the query
/// (SPEC 9.7).
#[test]
fn doctor_reports_no_index_tree_while_conflicts_stop_it() {
    let fixture = fixture(RepoOptions::new());
    // One divergent branch whose merge leaves conflict stages behind.
    fixture
        .repo
        .git_ok(["switch", "--quiet", "--create", "side"]);
    fixture.repo.write_file("doc-a.md", b"the side revision\n");
    fixture.repo.commit_all("side edit");
    fixture.repo.git_ok(["switch", "--quiet", "main"]);
    fixture.repo.write_file("doc-a.md", b"the main revision\n");
    fixture.repo.commit_all("main edit");
    let merge = fixture.repo.git(["merge", "--quiet", "side"]);
    assert!(!merge.status.success(), "the merge must conflict");
    assert!(
        oracle::index_tree(&fixture.repo).is_none(),
        "stock Git cannot write the conflicted index as a tree"
    );

    let before = oracle::snapshot(&fixture.repo);
    let conflicted = oracle::index_bytes(&fixture.repo);
    let outcome = doctor(&fixture.repo);
    assert!(
        outcome.ok(),
        "the conflict stops the tree, not the query: {:?}",
        outcome.error
    );
    assert_eq!(outcome.effects, Effects::unchanged());
    let data = outcome.data.as_ref().unwrap();
    assert_eq!(data.index_tree, None, "conflict stages stop the index tree");
    assert!(data.head.is_some());

    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "the doctor query");
    assert_index_bytes_unchanged(&fixture.repo, &conflicted, "the doctor query");
}

/// A history context no marker may depend on stops the receipt scan, and
/// the report says unknown instead of absent (SPEC 8.4 rule 7, 8.7).
#[test]
fn doctor_reports_an_unknown_receipt_when_replacements_stop_validation() {
    let fixture = fixture(RepoOptions::new());
    let pinned = pinned_review(&fixture.repo);
    let plan = keep_all_plan(&pinned, "review-replaced-01");
    let applied = apply_review(&fixture.repo, &plan);
    assert!(
        applied.ok(),
        "the keep-all review applies: {:?}",
        applied.error
    );
    let review = applied
        .data
        .as_ref()
        .unwrap()
        .commit
        .clone()
        .expect("the application commits");

    // One replacement object makes every marker of this repository depend
    // on a context rule 7 excludes.
    fixture
        .repo
        .git_ok(["replace", "--", &review, &fixture.start]);

    let before = oracle::snapshot(&fixture.repo);
    let index_bytes = oracle::index_bytes(&fixture.repo);
    let outcome = doctor(&fixture.repo);
    assert!(
        outcome.ok(),
        "the query stays a diagnostic: {:?}",
        outcome.error
    );
    assert_eq!(outcome.effects, Effects::unchanged());
    let data = outcome.data.as_ref().unwrap();
    match &data.receipt {
        margin::app::ReceiptPresence::Unavailable { what } => {
            assert_eq!(*what, "replacement objects");
        }
        other => panic!("the receipt state must be unknown, not {other:?}"),
    }
    match &data.baseline {
        BaselineStatus::Unavailable(_) => {}
        other => panic!("the baseline must be unavailable, not {other:?}"),
    }

    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "the doctor query");
    assert_index_bytes_unchanged(&fixture.repo, &index_bytes, "the doctor query");
}

/// Outside a repository and inside one without an adopted scope the query
/// refuses typed with unchanged effects (SPEC 6.3, 7.5).
#[test]
fn doctor_refuses_typed_outside_a_repository_and_without_a_scope() {
    let outside = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let env = IsolatedEnv::new(home.path());
    let git = Git::from_env(env.vars().clone()).at(outside.path());
    let outcome = Operations::new(&git).inspect_recovery(&RecoveryRequest::default());
    assert!(!outcome.ok());
    assert_eq!(outcome.error.as_ref().unwrap().code, "NOT_INITIALIZED");
    assert_eq!(outcome.effects, Effects::unchanged());

    // A repository without stored Margin settings adopts no scope.
    let repo = TestRepo::init();
    repo.write_file("doc.md", b"# Nothing adopted\n");
    repo.commit_all("import");
    let refused = doctor(&repo);
    assert!(!refused.ok());
    assert_eq!(refused.error.as_ref().unwrap().code, "NOT_INITIALIZED");
    assert_eq!(refused.effects, Effects::unchanged());

    // The executable reports the same refusal through the envelope.
    let output = margin(&env, outside.path(), &["doctor", "--json"]);
    assert_eq!(
        output.status.code(),
        Some(4),
        "an unmet precondition exits 4"
    );
    let envelope = envelope_of(&output, "doctor outside a repository");
    assert_eq!(envelope["command"], "doctor");
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["error"]["code"], "NOT_INITIALIZED");
}

/// A detached `HEAD` stops mutations, not the diagnostic: `doctor` is the
/// command a writer runs while the workspace looks wrong, so it keeps
/// observing the same state the one walk selects (SPEC 9.7, 4.2).
#[test]
fn doctor_keeps_reporting_under_a_detached_head() {
    let fixture = fixture(RepoOptions::new());
    fixture.repo.git_ok(["switch", "--quiet", "--detach"]);

    let before = oracle::snapshot(&fixture.repo);
    let index_bytes = oracle::index_bytes(&fixture.repo);
    let outcome = doctor(&fixture.repo);
    assert!(
        outcome.ok(),
        "a detached head stops mutations, not the diagnostic: {:?}",
        outcome.error
    );
    assert_eq!(outcome.effects, Effects::unchanged());
    let data = outcome.data.as_ref().unwrap();
    assert_eq!(data.head.as_deref(), Some(fixture.candidate.as_str()));
    match &data.baseline {
        BaselineStatus::Anchored(marker) => assert_eq!(marker.commit, fixture.start),
        other => panic!("the walk still anchors the scope: {other:?}"),
    }
    assert!(matches!(data.receipt, margin::app::ReceiptPresence::Absent));

    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "the doctor query");
    assert_index_bytes_unchanged(&fixture.repo, &index_bytes, "the doctor query");
}

/// The installed executable reports the inspection in both modes, and
/// neither mode writes anything (SPEC 6.2, 7.1).
#[test]
fn the_executable_reports_doctor_in_both_modes() {
    let fixture = fixture(RepoOptions::new());
    let head = oracle::head_oid(&fixture.repo).unwrap();
    let before = oracle::snapshot(&fixture.repo);
    let index_bytes = oracle::index_bytes(&fixture.repo);

    let json = margin(
        fixture.repo.env(),
        fixture.repo.path(),
        &["doctor", "--json"],
    );
    assert_eq!(json.status.code(), Some(0));
    assert!(json.stderr.is_empty(), "machine mode writes no stderr");
    let envelope = envelope_of(&json, "doctor");
    assert_eq!(envelope["command"], "doctor");
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["error"], Value::Null);
    assert_eq!(envelope["effects"]["working_tree"], "unchanged");
    assert_eq!(envelope["effects"]["index"], "unchanged");
    assert_eq!(envelope["effects"]["commit"], "none");
    let data = &envelope["data"];
    assert_eq!(data["object_format"], "sha1");
    assert_eq!(data["head"], json!(head));
    assert_eq!(data["baseline"]["state"], "anchored");
    assert_eq!(data["baseline"]["commit"], json!(fixture.start));
    assert_eq!(data["index_tree"], json!(oracle::index_tree(&fixture.repo)));
    assert_eq!(data["staged_total"], 0);
    assert_eq!(data["receipt"]["state"], "absent");
    // The recovery argv is one array of argv arrays, empty here.
    assert_eq!(data["recovery_argv"], json!([]));

    let text = margin(fixture.repo.env(), fixture.repo.path(), &["doctor"]);
    assert_eq!(text.status.code(), Some(0));
    let line = String::from_utf8_lossy(&text.stdout);
    assert_eq!(line.matches('\n').count(), 1, "one bounded text line");
    assert!(
        line.starts_with("margin: head="),
        "the text view starts with the head: {line:?}"
    );
    assert!(line.contains(&head), "the text view names the head");
    assert!(line.contains("receipt=absent"));

    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "both doctor modes");
    assert_index_bytes_unchanged(&fixture.repo, &index_bytes, "both doctor modes");
}

// The plan fingerprint of one plan document, through the shared identities
// (SPEC 7.3, 8.6): the value the receipt records beside its request.
fn fingerprint_of(repo: &TestRepo, plan: &str) -> String {
    let parsed = margin::domain::ReviewPlan::parse(plan.as_bytes())
        .unwrap_or_else(|problem| panic!("the plan must parse: {problem}"));
    Identities::new(&adapter_for(repo))
        .unwrap_or_else(|error| panic!("identities must read the repository: {error}"))
        .plan(&parsed.identity())
        .unwrap_or_else(|error| panic!("the plan must hash: {error}"))
        .id()
}
