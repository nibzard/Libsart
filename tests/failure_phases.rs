//! Real-Git failpoint and process tests for the failure phases of the
//! mutation operations (SPEC 16.6 and 9.6, task T037).
//!
//! Every case stops one operation at one observable boundary and proves the
//! three contracts of SPEC 16.6: the result reports the best-known effects
//! exactly, the repository stays recoverable, and no failure grants an
//! approval it did not earn.
//!
//! - the lock boundary: one busy mutation lock refuses the restoration and
//!   the resumption typed and without effects, and the release lets the
//!   identical request run (SPEC 16.6 item 1)
//! - the preview boundary: a failed disposable index stops the intended-tree
//!   proof of an application and of a restoration before any real write
//!   (SPEC 16.6 item 2)
//! - the application boundary: a permission failure inside the guarded
//!   apply path and one missing object fail typed, change nothing, and stay
//!   recoverable (SPEC 16.6 item 3)
//! - the commit boundary: a rejected or timed-out commit after a successful
//!   apply retains the proved result, reports it, and recovers through the
//!   resumption of SPEC 9.7 (SPEC 16.6 items 3 and 4)
//! - the hook boundary: a hook that edits the document before rejecting is
//!   reported with its exact effects, and a hook that rewrites the reserved
//!   trailers of a restoration leaves an ordinary commit that confers
//!   nothing (SPEC 16.6 item 5)
//! - the response boundary: an output limit that arrives after a published
//!   restoration commit still names that commit, one killed Margin process
//!   after the commit loses only its response while the receipt stays
//!   recoverable, and one interrupted process leaves the retained state of
//!   SPEC 9.6 behind (SPEC 16.6 items 6 and 7)
//! - the configuration boundary: one settings update over a locked
//!   configuration fails typed without a partial write, and one concurrent
//!   configuration writer never corrupts the file (SPEC 16.6 item 8)
//! - a write quota, the deterministic analog of a full disk in an isolated
//!   runner, fails one capture typed and leaves a recoverable workspace
//!   (SPEC 16.6: quota conditions)
//!
//! The neighboring suites hold the boundaries this one cross-references:
//! hooks, timeouts, and crashes of the checkpoint in `tests/checkpoint.rs`
//! and `tests/initialization.rs`, the partial application and the
//! concurrent writers of a review in `tests/review_apply.rs`, the retained
//! result of a resumption in `tests/review_resume.rs`, the receipt scans of
//! `tests/receipts.rs`, and the malformed output and process budgets of the
//! adapter in `tests/git_adapter.rs`.
//!
//! The oracles stay independent of the implementation (SPEC 16.3): every
//! state claim is checked with the installed Git, never with a second
//! Margin command that shares the parser under test.

mod support;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use margin::app::{
    BaselineStatus, CheckpointRequest, CommitEffect, DiffRequest, Effects, InitRequest,
    InspectRequest, ObservedEffect, OperationOutcome, Operations, RecoveryRequest, RestoreRequest,
    ReviewOutcome, ReviewRequest, validate_marker,
};
use margin::domain::{ReviewState, ScopeInput, SettingsUpdate};
use margin::git::{Git, Limits};
use serde_json::{Value, json};
use support::env::is_scrubbed;
use support::mutation::hold_mutation_lock;
use support::repo::TestRepo;
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

fn fixture() -> Fixture {
    let repo = TestRepo::init();
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

/// The change identity of one passage of `doc-a.md`: `passage` 0 is the
/// first in source order, found through the recorded source ranges.
fn group_of(pinned: &ReviewOutcome, passage: usize) -> String {
    let mut groups: Vec<&margin::app::ChangeDetail> = pinned
        .changes
        .iter()
        .filter(|change| change.path == "doc-a.md")
        .collect();
    assert_eq!(groups.len(), 2, "the document holds two passages");
    groups.sort_by_key(|change| change.ranges[0].old_start);
    groups[passage].change_id.clone()
}

/// One complete restoring review plan for `pinned`.
fn review_plan(pinned: &ReviewOutcome, request: &str) -> String {
    let restored = group_of(pinned, 0);
    json!({
        "schema_version": 1,
        "request_id": request,
        "review_id": pinned.review_id,
        "branch_ref": pinned.branch_ref,
        "scope_id": pinned.scope_id,
        "baseline_oid": pinned.baseline_oid,
        "candidate_oid": pinned.candidate_oid,
        "restore": [restored],
        "keep_remaining": true,
    })
    .to_string()
}

/// The workspace status both interfaces share, with the identity fields a
/// restoration plan pins to.
fn status_of(repo: &TestRepo) -> margin::app::WorkspaceInspection {
    let git = adapter_for(repo);
    let operations = Operations::new(&git);
    let status = operations.inspect_workspace(&InspectRequest {
        scope: None,
        path_filter: Vec::new(),
    });
    assert!(status.ok(), "the status query succeeds: {:?}", status.error);
    status.data.expect("a successful status carries data")
}

/// One restoration plan document for `status`: the document returns to the
/// bytes of the anchored baseline.
fn restore_plan(status: &margin::app::WorkspaceInspection, request: &str) -> String {
    let revision = match &status.baseline {
        BaselineStatus::Anchored(marker) => marker.commit.clone(),
        other => panic!("the fixture anchors one baseline: {other:?}"),
    };
    json!({
        "schema_version": 1,
        "request_id": request,
        "branch_ref": status.branch.clone().expect("an attached HEAD"),
        "scope_id": status.scope.digest,
        "candidate_oid": status.head.clone().expect("a born branch"),
        "path": "doc-a.md",
        "revision": revision,
    })
    .to_string()
}

/// Applies one review plan document through the operation layer.
fn apply(repo: &TestRepo, plan: &str) -> OperationOutcome<margin::app::ReviewDecision> {
    let git = adapter_for(repo);
    let operations = Operations::new(&git);
    operations.apply_review(&ReviewRequest {
        scope: None,
        plan: plan.as_bytes().to_vec(),
        dry_run: false,
    })
}

/// Resumes one review plan document through the operation layer.
fn resume(repo: &TestRepo, plan: &str) -> OperationOutcome<margin::app::ReviewDecision> {
    let git = adapter_for(repo);
    let operations = Operations::new(&git);
    operations.resume_review(&ReviewRequest {
        scope: None,
        plan: plan.as_bytes().to_vec(),
        dry_run: false,
    })
}

/// Restores one document through the operation layer.
fn restore(repo: &TestRepo, plan: &str) -> OperationOutcome<margin::app::RestoreOutcome> {
    let git = adapter_for(repo);
    let operations = Operations::new(&git);
    operations.restore_document(&RestoreRequest {
        scope: None,
        plan: plan.as_bytes().to_vec(),
        dry_run: false,
    })
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

/// The independent expected-tree oracle (SPEC 16.3): the candidate's
/// entries with `doc-a.md` replaced by `bytes`, written through
/// `git mktree`.
fn expected_tree_with_document(repo: &TestRepo, candidate: &str, bytes: &[u8]) -> String {
    let mut input = String::new();
    for entry in oracle::ls_tree(repo, candidate) {
        let record = if entry.path == "doc-a.md" {
            let oid =
                text(&repo.git_stdin(["hash-object", "-w", "--no-filters", "--stdin"], bytes));
            format!("100644 blob {oid}")
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
// The failpoint wrappers (SPEC 16.6)
// ---------------------------------------------------------------------------

/// The absolute path of the installed Git, resolved once per process.
fn real_git() -> &'static OsStr {
    static REAL_GIT: OnceLock<OsString> = OnceLock::new();
    REAL_GIT.get_or_init(|| {
        let output = Command::new("sh")
            .arg("-c")
            .arg("command -v git")
            .output()
            .expect("cannot locate the installed git");
        assert!(
            output.status.success(),
            "cannot locate the installed git: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        OsString::from(String::from_utf8_lossy(&output.stdout).trim())
    })
}

/// Writes one Git wrapper of `repo` that delegates every call to the
/// installed Git, with `script` between the shebang and the delegation.
/// `{real}` inside `script` names the installed Git.
fn write_git_wrapper(repo: &TestRepo, name: &str, script: &str) -> PathBuf {
    let path = repo.root().join(name);
    let body = script.replace("{real}", &real_git().to_string_lossy());
    fs::write(&path, body)
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", path.display()));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|error| panic!("cannot chmod {}: {error}", path.display()));
    path
}

/// One adapter whose wrapper fails every Git call that runs against a
/// disposable index (SPEC 16.6 item 2): the intended-tree proof cannot
/// finish, and the failure must not touch one real byte.
fn disposable_failure(repo: &TestRepo) -> Git {
    let script = write_git_wrapper(
        repo,
        "fail-disposable.sh",
        concat!(
            "#!/bin/sh\n",
            "# Failpoint (SPEC 16.6 item 2): fail every disposable-index call.\n",
            "if [ -n \"$GIT_INDEX_FILE\" ]; then\n",
            "  echo 'the disposable index failed' >&2\n",
            "  exit 1\n",
            "fi\n",
            "exec \"{real}\" \"$@\"\n",
        ),
    );
    Git::from_env(repo.env().vars().clone())
        .at(repo.path())
        .with_program(script.into_os_string())
}

/// The absolute path of the installed Margin executable, resolved once per
/// process from the Cargo build of this test.
fn margin_binary() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| PathBuf::from(env!("CARGO_BIN_EXE_margin")))
}

/// Prepares one child command of the installed executable with the isolated
/// environment of `repo`.
fn margin_command(repo: &TestRepo, args: &[&str]) -> Command {
    let mut command = Command::new(margin_binary());
    command
        .args(args)
        .current_dir(repo.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, _) in std::env::vars_os() {
        if is_scrubbed(&key) {
            command.env_remove(&key);
        }
    }
    command.envs(repo.env().vars());
    command
}

/// Runs the installed executable once in the repository.
fn margin(repo: &TestRepo, args: &[&str]) -> Output {
    margin_command(repo, args)
        .output()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"))
}

/// Runs the installed executable once in its own process group, so one test
/// can interrupt the whole group the way one terminal interrupt would.
fn margin_group(repo: &TestRepo, args: &[&str]) -> Child {
    let mut command = margin_command(repo, args);
    command.process_group(0);
    command
        .spawn()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"))
}

/// One `PATH` value whose `git` is one wrapper that delegates every call to
/// the installed Git and kills the Margin parent process after its first
/// successful real commit (SPEC 16.6 item 6): the commit is published and
/// the response dies with the process.
fn crashing_git_path(repo: &TestRepo) -> OsString {
    let dir = repo.root().join("shim");
    fs::create_dir_all(&dir)
        .unwrap_or_else(|error| panic!("cannot create {}: {error}", dir.display()));
    write_git_wrapper(
        repo,
        "shim/git",
        concat!(
            "#!/bin/sh\n",
            "# Failpoint (SPEC 16.6 item 6): delegate to the installed git, and\n",
            "# kill the Margin parent after one successful real commit, so the\n",
            "# published commit loses its response.\n",
            "if [ \"$1\" = \"commit\" ] && [ -z \"$GIT_INDEX_FILE\" ]; then\n",
            "  \"{real}\" \"$@\"\n",
            "  status=$?\n",
            "  if [ \"$status\" -eq 0 ]; then\n",
            "    kill -9 \"$PPID\"\n",
            "  fi\n",
            "  exit \"$status\"\n",
            "fi\n",
            "exec \"{real}\" \"$@\"\n",
        ),
    );
    let mut path = dir.into_os_string();
    path.push(":");
    if let Some(parent) = std::env::var_os("PATH") {
        path.push(parent);
    }
    path
}

/// Runs the installed executable once under one file-write quota of
/// `blocks` 512-byte blocks: the deterministic analog of one full disk in
/// an isolated runner (SPEC 16.6).
fn margin_under_quota(repo: &TestRepo, blocks: u32, args: &[&str]) -> Output {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(format!("ulimit -f {blocks}; exec \"$@\""))
        .arg("margin")
        .arg(margin_binary())
        .args(args)
        .current_dir(repo.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, _) in std::env::vars_os() {
        if is_scrubbed(&key) {
            command.env_remove(&key);
        }
    }
    command.envs(repo.env().vars());
    command
        .output()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"))
}

/// The number of first-parent commits `HEAD` carries, from stock Git.
fn commit_count(repo: &TestRepo) -> usize {
    text(&repo.git_ok(["rev-list", "--count", "--first-parent", "HEAD"]))
        .parse()
        .expect("one count")
}

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

/// Installs one hook of the repository.
fn install_hook(repo: &TestRepo, name: &str, script: &str) {
    let path = hook_path(repo, name);
    fs::write(&path, script)
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", path.display()));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|error| panic!("cannot chmod {}: {error}", path.display()));
}

/// Removes one hook of the repository, so one later commit may run.
fn remove_hook(repo: &TestRepo, name: &str) {
    let _ = fs::remove_file(hook_path(repo, name));
}

/// The recovery inspection of SPEC 9.7, through the operation layer.
fn doctor(repo: &TestRepo) -> margin::app::RecoveryInspection {
    let git = adapter_for(repo);
    let operations = Operations::new(&git);
    let result = operations.inspect_recovery(&RecoveryRequest {
        scope: Some(scope()),
    });
    assert!(
        result.ok(),
        "the recovery inspection succeeds: {:?}",
        result.error
    );
    result.data.expect("one report")
}

// ---------------------------------------------------------------------------
// The lock boundary (SPEC 16.6 item 1)
// ---------------------------------------------------------------------------

/// One busy mutation lock refuses the restoration typed and without
/// effects, the read-only recovery inspection still answers, and the
/// released lock lets the identical request run (SPEC 9.1 step 2, 16.6
/// item 1).
#[test]
fn a_busy_lock_refuses_the_restoration_and_releases() {
    let fixture = fixture();
    let plan = restore_plan(&status_of(&fixture.repo), "restore-lock-01");
    let before = oracle::snapshot(&fixture.repo);

    let held = hold_mutation_lock(&fixture.repo);
    assert_refusal(
        &restore(&fixture.repo, &plan),
        "BUSY",
        "one restoration over one held lock",
    );
    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "the refused restoration");

    // Queries hold no lock, so the recovery inspection still answers while
    // the mutation lock belongs to another operation.
    let report = doctor(&fixture.repo);
    assert_eq!(report.head.as_deref(), Some(fixture.candidate.as_str()));

    // The competing operation ends; the identical request then commits.
    drop(held);
    let outcome = restore(&fixture.repo, &plan);
    assert!(outcome.ok(), "the retry commits: {:?}", outcome.error);
    assert_eq!(outcome.effects.commit, CommitEffect::Created);
}

// ---------------------------------------------------------------------------
// The preview boundary (SPEC 16.6 item 2)
// ---------------------------------------------------------------------------

/// One failed disposable index stops the intended-tree proof of one review
/// application and of one restoration before any real write: both fail
/// typed, both report unchanged effects, and the repository keeps its
/// exact state (SPEC 9.1 step 5, 9.3, 16.6 item 2).
#[test]
fn a_failed_disposable_index_touches_no_real_state() {
    let fixture = fixture();
    // The pinned identities come from the plain adapter, so only the
    // operations under test run against the failing wrapper.
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let review = review_plan(&pinned, "review-preview-01");
    let restoration = restore_plan(&status_of(&fixture.repo), "restore-preview-01");
    let before = oracle::snapshot(&fixture.repo);

    let git = disposable_failure(&fixture.repo);
    let operations = Operations::new(&git);
    let apply = operations.apply_review(&ReviewRequest {
        scope: None,
        plan: review.as_bytes().to_vec(),
        dry_run: false,
    });
    assert!(!apply.ok(), "a broken proof must fail the application");
    assert_eq!(apply.error.as_ref().unwrap().code, "GIT_FAILED");
    assert_eq!(apply.effects, Effects::unchanged());
    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "the refused application");

    let restore = operations.restore_document(&RestoreRequest {
        scope: None,
        plan: restoration.as_bytes().to_vec(),
        dry_run: false,
    });
    assert!(!restore.ok(), "a broken proof must fail the restoration");
    assert_eq!(restore.error.as_ref().unwrap().code, "GIT_FAILED");
    assert_eq!(restore.effects, Effects::unchanged());
    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "the refused restoration");

    // No approval moved: the review stays pending below the candidate.
    let (review_state, baseline) = workspace_state(&fixture.repo);
    assert_eq!(review_state, ReviewState::Pending);
    match baseline {
        BaselineStatus::Anchored(marker) => assert_eq!(marker.commit, fixture.start),
        other => panic!("the start marker still anchors the scope: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The application boundary (SPEC 16.6 item 3)
// ---------------------------------------------------------------------------

/// One permission failure inside the guarded apply path of a restoration
/// fails typed, changes nothing, names its recovery, and stays recoverable
/// (SPEC 9.4 step 2, 9.6, 16.6: permission failure).
#[test]
fn a_permission_failure_during_the_apply_is_typed_and_recoverable() {
    let fixture = fixture();
    let plan = restore_plan(&status_of(&fixture.repo), "restore-permission-01");
    // The working directory stays unwritable for the apply: Git recreates a
    // patched file, so only the directory permission stops the write. The
    // tracked state is untouched, the guards still pass, and the disposable
    // proof lives outside the repository.
    let root = fixture.repo.path();
    let mut permissions = fs::metadata(root).unwrap().permissions();
    permissions.set_mode(0o555);
    fs::set_permissions(root, permissions).unwrap();
    let before = oracle::snapshot(&fixture.repo);

    let outcome = restore(&fixture.repo, &plan);
    assert!(!outcome.ok(), "an unwritable directory must fail the apply");
    let error = outcome.error.as_ref().unwrap();
    assert_eq!(error.code, "PARTIAL_MUTATION");
    assert!(
        error.message.contains("doc-a.md"),
        "the failure names the path: {}",
        error.message
    );
    assert!(
        error.message.contains("Permission denied"),
        "the failure names the permission: {}",
        error.message
    );
    assert_eq!(
        error.recovery_argv,
        vec!["margin", "status", "--json"],
        "the failure names the recovery: {:?}",
        error.recovery_argv
    );
    assert_eq!(outcome.effects, Effects::unchanged());
    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "the failed apply");

    // The writer restores write permission; the identical request commits.
    let mut permissions = fs::metadata(root).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(root, permissions).unwrap();
    let retry = restore(&fixture.repo, &plan);
    assert!(retry.ok(), "the retry commits: {:?}", retry.error);
    assert_eq!(retry.effects.commit, CommitEffect::Created);
    assert_eq!(
        fixture.repo.read_file("doc-a.md"),
        document("first original passage", "second original passage").as_bytes(),
        "the committed result holds the source bytes"
    );
}

/// One missing object of the candidate fails the application typed before
/// any mutation, and the restored object lets the identical request run
/// (SPEC 16.6: missing object).
#[test]
fn a_missing_object_fails_typed_and_preserves_the_state() {
    let fixture = fixture();
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = review_plan(&pinned, "review-missing-01");
    // The candidate's blob of the document leaves the object store.
    let blob = text(
        &fixture
            .repo
            .git_ok(["rev-parse", &format!("{}:doc-a.md", fixture.candidate)]),
    );
    let object = fixture
        .repo
        .path()
        .join(".git")
        .join("objects")
        .join(&blob[..2])
        .join(&blob[2..]);
    let moved = fixture.repo.root().join("moved-object");
    fs::rename(&object, &moved).unwrap();
    fixture.repo.git_fail(["cat-file", "-e", &blob]);
    let before = oracle::snapshot(&fixture.repo);

    let outcome = apply(&fixture.repo, &plan);
    assert!(!outcome.ok(), "a missing object must fail the application");
    let error = outcome.error.as_ref().unwrap();
    assert_eq!(error.code, "GIT_FAILED", "the typed failure: {error:?}");
    assert_eq!(outcome.effects, Effects::unchanged());
    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "the refused application");
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(fixture.candidate.as_str()),
        "no commit appeared"
    );

    // The object returns; the identical request applies.
    fs::rename(&moved, &object).unwrap();
    let retry = apply(&fixture.repo, &plan);
    assert!(retry.ok(), "the retry applies: {:?}", retry.error);
    assert_eq!(retry.effects.commit, CommitEffect::Created);
}

// ---------------------------------------------------------------------------
// The commit boundary (SPEC 16.6 items 3 and 4)
// ---------------------------------------------------------------------------

/// One rejected restoration commit retains the applied result: the index
/// holds the proved tree, the working bytes hold the restored document, and
/// the candidate stands. One retry stops on the retained staging, and the
/// guarded staged capture of SPEC 9.7 saves the result as one unreviewed
/// draft that approves nothing (SPEC 9.6: patch succeeds, commit fails).
#[test]
fn a_rejected_restoration_commit_retains_the_applied_result() {
    let fixture = fixture();
    let plan = restore_plan(&status_of(&fixture.repo), "restore-commit-01");
    install_hook(&fixture.repo, "pre-commit", "#!/bin/sh\nexit 1\n");
    let before = oracle::snapshot(&fixture.repo);

    let outcome = restore(&fixture.repo, &plan);
    assert!(!outcome.ok(), "a rejected commit must fail the restoration");
    assert_eq!(outcome.error.as_ref().unwrap().code, "GIT_FAILED");
    assert_eq!(outcome.effects.commit, CommitEffect::None);
    assert_eq!(outcome.effects.commit_oid, None);
    assert_eq!(outcome.effects.index, ObservedEffect::Changed);
    assert_eq!(outcome.effects.working_tree, ObservedEffect::Changed);
    let data = outcome.data.as_ref().expect("the observed state");
    assert_eq!(data.commit, None);

    // The retained state, read with stock Git: the index holds the proved
    // tree T, the working bytes hold the restored document, and the
    // candidate stays HEAD.
    assert_eq!(
        oracle::index_tree(&fixture.repo).as_deref(),
        Some(data.expected_tree.as_str()),
        "the real index holds the proved tree"
    );
    assert_eq!(
        fixture.repo.read_file("doc-a.md"),
        document("first original passage", "second original passage").as_bytes(),
        "the working tree holds the restored document"
    );
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(fixture.candidate.as_str()),
        "the candidate stands"
    );
    assert_ne!(
        before.index_tree,
        oracle::index_tree(&fixture.repo),
        "the index changed during the application"
    );

    // No receipt exists yet, so the decision stays unmade.
    let (review_state, baseline) = workspace_state(&fixture.repo);
    assert_eq!(review_state, ReviewState::Pending);
    match baseline {
        BaselineStatus::Anchored(marker) => assert_eq!(marker.commit, fixture.start),
        other => panic!("the start marker still anchors the scope: {other:?}"),
    }

    // The identical request does not force its staged result through the
    // guards: one retry stops on the staged state it finds (SPEC 9.2).
    assert_refusal(
        &restore(&fixture.repo, &plan),
        "STAGED_CHANGES",
        "one restoration over its own retained staging",
    );

    // The documented recovery (SPEC 9.7): the writer clears the hook, the
    // recovery inspection names the retained state, and the guarded staged
    // capture saves exactly that state as one unreviewed draft.
    remove_hook(&fixture.repo, "pre-commit");
    let report = doctor(&fixture.repo);
    assert_eq!(report.head.as_deref(), Some(fixture.candidate.as_str()));
    assert_eq!(
        report.index_tree.as_deref(),
        Some(data.expected_tree.as_str())
    );
    assert_eq!(report.staged_paths.total, 1, "one staged path is retained");
    let git = adapter_for(&fixture.repo);
    let operations = Operations::new(&git);
    let capture = operations.create_checkpoint(&CheckpointRequest {
        scope: Some(scope()),
        message: Some("recover the retained restoration".to_string()),
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
        capture.ok(),
        "the recovery capture commits: {:?}",
        capture.error
    );
    let draft = capture
        .data
        .as_ref()
        .unwrap()
        .commit
        .clone()
        .expect("the draft commit");
    assert_eq!(
        oracle::commit_tree(&fixture.repo, &draft),
        data.expected_tree,
        "the draft holds exactly the retained result"
    );
    assert_eq!(
        String::from_utf8_lossy(&fixture.repo.git_ok(["status", "--porcelain"])),
        "",
        "the retained state is saved"
    );

    // The draft confers no approval: the restored document equals the
    // bytes of the baseline, so the next review starts from the starting
    // point with nothing left to decide (SPEC 4.2, 8.5).
    let (review_state, baseline) = workspace_state(&fixture.repo);
    assert_eq!(review_state, ReviewState::StartingPoint);
    match baseline {
        BaselineStatus::Anchored(marker) => assert_eq!(marker.commit, fixture.start),
        other => panic!("the start marker still anchors the scope: {other:?}"),
    }
}

/// One rejected resumption commit keeps the retained result of the
/// interrupted application: nothing is rewritten, the refusal holds the
/// lock boundary, and the cleared hook lets the resumption commit the
/// proved index (SPEC 9.6, 9.7, 16.6 items 1 and 4).
#[test]
fn a_rejected_resumption_keeps_the_retained_result_and_recovers() {
    let fixture = fixture();
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = review_plan(&pinned, "review-resume-commit-01");

    // One interrupted application leaves the retained state of SPEC 9.6.
    install_hook(&fixture.repo, "pre-commit", "#!/bin/sh\nexit 1\n");
    let interrupted = apply(&fixture.repo, &plan);
    assert!(!interrupted.ok(), "the hook stops the application");
    let expected_tree = interrupted.data.as_ref().unwrap().expected_tree.clone();
    assert_eq!(
        oracle::index_tree(&fixture.repo).as_deref(),
        Some(expected_tree.as_str()),
        "the application retained the proved tree"
    );

    // The resumption commits only, so the rejected commit changes nothing:
    // the retained state stays exactly as the application left it.
    let before = oracle::snapshot(&fixture.repo);
    let refused = resume(&fixture.repo, &plan);
    assert!(!refused.ok(), "the hook stops the resumption");
    assert_eq!(refused.error.as_ref().unwrap().code, "GIT_FAILED");
    assert_eq!(refused.effects.commit, CommitEffect::None);
    assert_eq!(refused.effects.index, ObservedEffect::Unchanged);
    assert_eq!(refused.effects.working_tree, ObservedEffect::Unchanged);
    before.assert_unchanged(&oracle::snapshot(&fixture.repo), "the refused resumption");

    // One busy lock refuses the resumption of the retained state too.
    let held = hold_mutation_lock(&fixture.repo);
    assert_refusal(
        &resume(&fixture.repo, &plan),
        "BUSY",
        "one resumption over one held lock",
    );
    drop(held);

    // The writer clears the hook; the resumption commits the proved index
    // and the review is decided exactly once.
    remove_hook(&fixture.repo, "pre-commit");
    let count_before = commit_count(&fixture.repo);
    let outcome = resume(&fixture.repo, &plan);
    assert!(outcome.ok(), "the resumption commits: {:?}", outcome.error);
    let data = outcome.data.as_ref().unwrap();
    let commit = data.commit.clone().expect("the review commit");
    assert_eq!(outcome.effects.commit, CommitEffect::Created);
    assert_eq!(outcome.effects.commit_oid.as_deref(), Some(commit.as_str()));
    assert_eq!(commit_count(&fixture.repo), count_before + 1);
    assert_eq!(
        oracle::commit_tree(&fixture.repo, &commit),
        expected_tree,
        "the resumption commits the proved tree"
    );
    let (review_state, _) = workspace_state(&fixture.repo);
    assert_eq!(review_state, ReviewState::Reviewed);
}

/// One timed-out review commit loses its outcome: the response reports the
/// unknown commit and the retained state, and the explicit resumption of
/// SPEC 9.7 recovers the decision after the writer clears the hook and the
/// stale index lock (SPEC 9.6: process crashes before the response, 16.6:
/// subprocess timeout).
#[test]
fn a_timed_out_review_commit_reports_an_unknown_outcome_and_recovers() {
    let fixture = fixture();
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = review_plan(&pinned, "review-timeout-01");
    // One sleeping hook holds the commit until the tight budget kills it.
    install_hook(&fixture.repo, "pre-commit", "#!/bin/sh\nsleep 10\nexit 0\n");
    let restored = document("first original passage", "second revised passage");
    let expected_tree =
        expected_tree_with_document(&fixture.repo, &fixture.candidate, restored.as_bytes());

    let git = Git::from_env(fixture.repo.env().vars().clone())
        .at(fixture.repo.path())
        .with_limits(Limits {
            timeout: Duration::from_millis(600),
            max_output_bytes: 16 * 1024 * 1024,
        });
    let outcome = Operations::new(&git).apply_review(&ReviewRequest {
        scope: None,
        plan: plan.as_bytes().to_vec(),
        dry_run: false,
    });

    // The killed commit loses its outcome, which is never narrowed back to
    // a known value (SPEC 7.1, 9.6).
    assert!(!outcome.ok(), "the killed commit must fail the application");
    assert_eq!(outcome.error.as_ref().unwrap().code, "OUTCOME_UNKNOWN");
    assert_eq!(outcome.effects.commit, CommitEffect::Unknown);
    assert_eq!(outcome.effects.commit_oid, None);
    assert_eq!(outcome.effects.index, ObservedEffect::Changed);
    assert_eq!(outcome.effects.working_tree, ObservedEffect::Changed);
    let data = outcome.data.as_ref().expect("the observed state");
    assert_eq!(data.commit, None, "an unknown outcome is not a commit");
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(fixture.candidate.as_str()),
        "no commit was published"
    );
    assert_eq!(
        oracle::index_tree(&fixture.repo).as_deref(),
        Some(expected_tree.as_str()),
        "the retained index holds the proved tree"
    );

    // The review stays pending: an unknown outcome approves nothing.
    let (review_state, baseline) = workspace_state(&fixture.repo);
    assert_eq!(review_state, ReviewState::Pending);
    match baseline {
        BaselineStatus::Anchored(marker) => assert_eq!(marker.commit, fixture.start),
        other => panic!("the start marker still anchors the scope: {other:?}"),
    }

    // The writer clears the hook and the stale index lock the killed commit
    // left behind: Margin never deletes a Git lock file itself (SPEC 9.7).
    remove_hook(&fixture.repo, "pre-commit");
    let stale = fixture.repo.path().join(".git").join("index.lock");
    let _ = fs::remove_file(stale);

    let recovery = resume(&fixture.repo, &plan);
    assert!(
        recovery.ok(),
        "the resumption commits: {:?}",
        recovery.error
    );
    assert_eq!(recovery.effects.commit, CommitEffect::Created);
    assert_eq!(
        oracle::commit_tree(
            &fixture.repo,
            recovery.data.as_ref().unwrap().commit.as_deref().unwrap()
        ),
        expected_tree,
        "the resumption commits the proved tree"
    );
    let (review_state, _) = workspace_state(&fixture.repo);
    assert_eq!(review_state, ReviewState::Reviewed);
}

// ---------------------------------------------------------------------------
// The hook boundary (SPEC 16.6 item 5)
// ---------------------------------------------------------------------------

/// One hook that edits the document before it rejects is reported with its
/// exact effects: the staged restoration, the hook's working bytes, and no
/// commit. The writer discards the edit, and the guarded staged capture of
/// SPEC 9.7 saves the retained restoration as one unreviewed draft (SPEC
/// 9.2, 14.1, 16.6 item 5).
#[test]
fn a_restoration_hook_that_edits_then_rejects_is_reported() {
    let fixture = fixture();
    let plan = restore_plan(&status_of(&fixture.repo), "restore-hook-01");
    let restored = document("first original passage", "second original passage").into_bytes();
    install_hook(
        &fixture.repo,
        "pre-commit",
        "#!/bin/sh\nprintf 'hook bytes\\n' >> doc-a.md\nexit 1\n",
    );

    let outcome = restore(&fixture.repo, &plan);
    assert!(
        !outcome.ok(),
        "the rejecting hook must fail the restoration"
    );
    assert_eq!(outcome.error.as_ref().unwrap().code, "GIT_FAILED");
    assert_eq!(outcome.effects.commit, CommitEffect::None);
    assert_eq!(outcome.effects.commit_oid, None);
    assert_eq!(outcome.effects.index, ObservedEffect::Changed);
    assert_eq!(outcome.effects.working_tree, ObservedEffect::Changed);

    // The observed state, read with stock Git: the index holds the staged
    // restoration and the working file holds the hook's edit of it.
    let mut with_hook_bytes = restored.clone();
    with_hook_bytes.extend_from_slice(b"hook bytes\n");
    assert_eq!(fixture.repo.read_file("doc-a.md"), with_hook_bytes);
    assert_eq!(
        oracle::blob(
            &fixture.repo,
            &text(&fixture.repo.git_ok(["rev-parse", ":doc-a.md"]))
        ),
        restored,
        "the staged bytes are the content the hook read before its edit"
    );
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(fixture.candidate.as_str()),
        "the candidate stands"
    );

    // The documented recovery (SPEC 9.7): the writer clears the hook and
    // discards the hook's edit by returning the document to the bytes the
    // failed application staged, then the guarded staged capture saves that
    // state as one unreviewed draft.
    remove_hook(&fixture.repo, "pre-commit");
    fixture.repo.write_file("doc-a.md", &restored);
    let report = doctor(&fixture.repo);
    assert_eq!(report.head.as_deref(), Some(fixture.candidate.as_str()));
    assert_eq!(report.staged_paths.total, 1, "one staged path is retained");
    let git = adapter_for(&fixture.repo);
    let operations = Operations::new(&git);
    let capture = operations.create_checkpoint(&CheckpointRequest {
        scope: Some(scope()),
        message: Some("recover the retained restoration".to_string()),
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
        capture.ok(),
        "the recovery capture commits: {:?}",
        capture.error
    );
    let draft = capture
        .data
        .as_ref()
        .unwrap()
        .commit
        .clone()
        .expect("the draft commit");
    assert_eq!(
        oracle::commit_tree(&fixture.repo, &draft),
        oracle::index_tree(&fixture.repo).expect("the captured tree"),
        "the draft holds exactly the retained result"
    );
    assert_eq!(
        fixture.repo.read_file("doc-a.md"),
        restored,
        "the saved document holds the staged restoration"
    );
}

/// One hook that rewrites the reserved trailers of a restoration leaves an
/// ordinary commit that confers nothing: the failure reports the contract
/// violation with the created commit, and the workspace gains no approval
/// (SPEC 8.4, 9.6, 16.6 item 5).
#[test]
fn a_trailer_rewrite_during_a_restoration_is_a_contract_violation() {
    let fixture = fixture();
    let plan = restore_plan(&status_of(&fixture.repo), "restore-trailers-01");
    install_hook(
        &fixture.repo,
        "commit-msg",
        "#!/bin/sh\nprintf 'Margin-Kind: review\\n' >> \"$1\"\nexit 0\n",
    );

    let outcome = restore(&fixture.repo, &plan);
    assert!(!outcome.ok(), "a rewritten marker must fail verification");
    assert_eq!(outcome.error.as_ref().unwrap().code, "CONTRACT_VIOLATION");
    let created = oracle::head_oid(&fixture.repo).expect("the commit itself succeeded");
    assert_eq!(outcome.effects.commit, CommitEffect::Created);
    assert_eq!(
        outcome.effects.commit_oid.as_deref(),
        Some(created.as_str())
    );
    let data = outcome.data.as_ref().expect("the observed state");
    assert_eq!(data.commit.as_deref(), Some(created.as_str()));
    assert!(
        validate_marker(&adapter_for(&fixture.repo), &created).is_err(),
        "the ordinary commit confers nothing"
    );

    // No approval moved: the restored document equals the bytes of the
    // baseline, so the workspace sits at its starting point, anchored at
    // the same marker, and the ordinary commit approves nothing (SPEC 4.2).
    let (review_state, baseline) = workspace_state(&fixture.repo);
    assert_eq!(review_state, ReviewState::StartingPoint);
    match baseline {
        BaselineStatus::Anchored(marker) => assert_eq!(marker.commit, fixture.start),
        other => panic!("the start marker still anchors the scope: {other:?}"),
    }
    let report = doctor(&fixture.repo);
    assert!(report.head.is_some(), "the recovery inspection answers");
}

// ---------------------------------------------------------------------------
// The response boundary (SPEC 16.6 items 6 and 7)
// ---------------------------------------------------------------------------

/// One output limit that arrives after a published restoration commit
/// still names that commit, and the retry returns the receipt without a
/// second commit (SPEC 9.6: commit succeeds, response is lost).
#[test]
fn an_output_limit_after_the_restoration_commit_names_the_commit() {
    let fixture = fixture();
    let plan = restore_plan(&status_of(&fixture.repo), "restore-limit-01");
    // The commit-msg hook leaves the message alone and floods stderr, so
    // `git commit` publishes the commit and still exceeds the capture
    // budget.
    install_hook(
        &fixture.repo,
        "commit-msg",
        "#!/bin/sh\nhead -c 262144 /dev/zero >&2\nexit 0\n",
    );

    let git = Git::from_env(fixture.repo.env().vars().clone())
        .at(fixture.repo.path())
        .with_limits(Limits {
            timeout: Duration::from_millis(10_000),
            max_output_bytes: 64 * 1024,
        });
    let outcome = Operations::new(&git).restore_document(&RestoreRequest {
        scope: None,
        plan: plan.as_bytes().to_vec(),
        dry_run: false,
    });

    // The commit exists even though its process result was lost to the
    // output limit: the inspected head names it (SPEC 14.1, 9.6).
    assert!(!outcome.ok(), "the flooded capture must fail");
    assert_eq!(outcome.error.as_ref().unwrap().code, "LIMIT_EXCEEDED");
    let created = oracle::head_oid(&fixture.repo).expect("the commit itself succeeded");
    assert_eq!(outcome.effects.commit, CommitEffect::Created);
    assert_eq!(
        outcome.effects.commit_oid.as_deref(),
        Some(created.as_str())
    );
    let data = outcome.data.as_ref().expect("the observed state");
    assert_eq!(data.commit.as_deref(), Some(created.as_str()));
    validate_marker(&adapter_for(&fixture.repo), &created)
        .unwrap_or_else(|error| panic!("the receipt validates: {error}"));

    // The receipt precedes every present-day check, so the retry — the
    // flooding hook still installed — returns the receipt and commits
    // nothing (SPEC 8.7).
    let count_before = commit_count(&fixture.repo);
    let retry = restore(&fixture.repo, &plan);
    assert!(
        retry.ok(),
        "the retry reports the receipt: {:?}",
        retry.error
    );
    assert_eq!(retry.effects.commit, CommitEffect::ExistingReceipt);
    assert_eq!(
        retry.data.as_ref().unwrap().existing_receipt.as_deref(),
        Some(created.as_str())
    );
    assert_eq!(commit_count(&fixture.repo), count_before);
}

/// One Margin process killed after its commit loses only its response: the
/// published commit stays, the stock-Git state proves it, and the retry
/// returns the receipt without a second commit (SPEC 9.6: commit succeeds,
/// response is lost; 16.6 item 6 as one process test).
#[test]
fn a_process_crash_after_the_commit_loses_the_response_not_the_receipt() {
    let fixture = fixture();
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = review_plan(&pinned, "review-crash-01");
    let path = fixture.repo.root().join("plan.json");
    fs::write(&path, &plan).unwrap();
    let path = path.to_str().unwrap().to_string();
    let count_before = commit_count(&fixture.repo);

    // The installed executable runs with the crashing Git wrapper first in
    // its PATH: the published review commit kills the process before it can
    // write one envelope byte.
    let mut command = margin_command(&fixture.repo, &["review", "--input", &path, "--json"]);
    command.env("PATH", crashing_git_path(&fixture.repo));
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"));

    // The process died from the injected kill and wrote no envelope.
    assert_eq!(
        output.status.signal(),
        Some(9),
        "the process died from the injected kill"
    );
    assert!(
        output.stdout.is_empty(),
        "the killed process wrote no envelope: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    // The commit the process published stands, proven with stock Git and
    // the shared marker validator.
    let created = oracle::head_oid(&fixture.repo).expect("the published commit");
    assert_eq!(commit_count(&fixture.repo), count_before + 1);
    let marker = validate_marker(&adapter_for(&fixture.repo), &created)
        .unwrap_or_else(|error| panic!("the review marker validates: {error}"));
    assert_eq!(marker.kind, margin::domain::CommitKind::Review);

    // The retry returns the receipt before any stale check and commits
    // nothing (SPEC 8.7, 9.6).
    let retry = apply(&fixture.repo, &plan);
    assert!(
        retry.ok(),
        "the retry reports the receipt: {:?}",
        retry.error
    );
    assert_eq!(retry.effects.commit, CommitEffect::ExistingReceipt);
    assert_eq!(
        retry.data.as_ref().unwrap().existing_receipt.as_deref(),
        Some(created.as_str())
    );
    assert_eq!(commit_count(&fixture.repo), count_before + 1);
    let (review_state, _) = workspace_state(&fixture.repo);
    assert_eq!(review_state, ReviewState::Reviewed);
}

/// One interrupted application — the process group receives the terminal
/// interrupt while the commit hook sleeps — leaves the retained state of
/// SPEC 9.6 behind: no commit, the proved tree staged, the reviewed bytes
/// on disk, and one explicit resumption recovers the decision (SPEC 9.6:
/// process crashes before commit; 16.6: user interruption).
#[test]
fn an_interrupted_application_leaves_the_retained_state_recoverable() {
    let fixture = fixture();
    let pinned = pinned_review_of(&fixture.repo, &fixture.start);
    let plan = review_plan(&pinned, "review-interrupt-01");
    let path = fixture.repo.root().join("plan.json");
    fs::write(&path, &plan).unwrap();
    let path = path.to_str().unwrap().to_string();
    // One sleeping hook holds the commit open, so the interrupt arrives
    // between the apply and the commit.
    install_hook(&fixture.repo, "pre-commit", "#!/bin/sh\nsleep 30\nexit 0\n");

    let child = margin_group(&fixture.repo, &["review", "--input", &path, "--json"]);
    // The interrupt must land between the apply and the commit, so the test
    // waits until stock Git sees the staged result of the apply: the commit
    // itself waits inside the sleeping hook.
    let mut staged = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        let output = fixture
            .repo
            .git(["diff-index", "--cached", "--name-only", "HEAD"]);
        if output.status.success() && !text(&output.stdout).is_empty() {
            staged = true;
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        staged,
        "the application staged its result before the commit"
    );
    // One terminal delivers the interrupt to the whole foreground group, so
    // the shell builtin sends it to the negative process-group ID.
    let group = format!("-{}", child.id());
    let interrupt = Command::new("sh")
        .arg("-c")
        .arg(format!("kill -INT {group}"))
        .status()
        .expect("cannot send the interrupt");
    assert!(
        interrupt.success(),
        "the interrupt reached the process group"
    );
    let output = child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("cannot await the margin binary: {error}"));

    // The whole group died from the interrupt and no envelope was written.
    assert_eq!(
        output.status.signal(),
        Some(2),
        "the process died from the terminal interrupt"
    );
    assert!(
        output.stdout.is_empty(),
        "the interrupted process wrote no envelope: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    // The retained state, read with stock Git: the candidate stands, the
    // index holds the proved tree, and the working bytes hold the reviewed
    // result (SPEC 9.6).
    let expected_bytes = document("first original passage", "second revised passage");
    let expected_tree =
        expected_tree_with_document(&fixture.repo, &fixture.candidate, expected_bytes.as_bytes());
    assert_eq!(
        oracle::head_oid(&fixture.repo).as_deref(),
        Some(fixture.candidate.as_str()),
        "no commit was published"
    );
    assert_eq!(
        oracle::index_tree(&fixture.repo).as_deref(),
        Some(expected_tree.as_str()),
        "the interrupted application retained the proved tree"
    );
    assert_eq!(
        fixture.repo.read_file("doc-a.md"),
        expected_bytes.into_bytes(),
        "the working tree holds the reviewed result"
    );

    // The restart inspects and recovers explicitly: the recovery report
    // names the retained state, and the review stays unapproved.
    let report = doctor(&fixture.repo);
    assert_eq!(report.head.as_deref(), Some(fixture.candidate.as_str()));
    assert_eq!(report.index_tree.as_deref(), Some(expected_tree.as_str()));
    assert_eq!(report.staged_paths.total, 1, "one staged path is retained");
    let (review_state, baseline) = workspace_state(&fixture.repo);
    assert_eq!(review_state, ReviewState::Pending);
    match baseline {
        BaselineStatus::Anchored(marker) => assert_eq!(marker.commit, fixture.start),
        other => panic!("the start marker still anchors the scope: {other:?}"),
    }

    // The writer clears the hook and the stale index lock, then resumes:
    // the resumption commits the retained result exactly once (SPEC 9.7).
    remove_hook(&fixture.repo, "pre-commit");
    let stale = fixture.repo.path().join(".git").join("index.lock");
    let _ = fs::remove_file(stale);
    let count_before = commit_count(&fixture.repo);
    let recovery = resume(&fixture.repo, &plan);
    assert!(
        recovery.ok(),
        "the resumption commits: {:?}",
        recovery.error
    );
    assert_eq!(recovery.effects.commit, CommitEffect::Created);
    assert_eq!(commit_count(&fixture.repo), count_before + 1);
    assert_eq!(
        oracle::commit_tree(
            &fixture.repo,
            recovery.data.as_ref().unwrap().commit.as_deref().unwrap()
        ),
        expected_tree,
        "the resumption commits the proved tree"
    );
}

// ---------------------------------------------------------------------------
// The configuration boundary (SPEC 16.6 item 8)
// ---------------------------------------------------------------------------

/// One settings update over a locked configuration fails typed and claims
/// no write, the configuration stays parseable, and the cleared lock lets
/// the identical update run (SPEC 8.2, 16.6 item 8).
#[test]
fn a_settings_update_over_a_locked_configuration_fails_typed() {
    let fixture = fixture();
    // One stale Git configuration lock lets every read succeed while the
    // first settings write fails (SPEC 16.6 item 8): Git cannot take the
    // configuration lock, exactly as under one hostile filesystem.
    let lock = fixture.repo.path().join(".git").join("config.lock");
    fs::write(&lock, b"").unwrap();
    let update = SettingsUpdate {
        auto_checkpoint: Some(true),
        ..SettingsUpdate::default()
    };

    let outcome = Operations::new(&adapter_for(&fixture.repo)).update_settings(&update);
    assert!(!outcome.ok(), "a locked configuration must fail the update");
    let error = outcome.error.as_ref().unwrap();
    assert_eq!(error.code, "GIT_FAILED");
    assert!(
        error.message.contains("could not lock config file"),
        "the failure names the configuration lock: {}",
        error.message
    );
    assert_eq!(outcome.effects, Effects::unchanged());

    // The configuration parses, holds no new Margin key, and the identical
    // update runs once the stale lock is gone.
    assert_eq!(
        raw_config(&fixture.repo, "margin.autoCheckpoint"),
        None,
        "no setting was written"
    );
    fixture.repo.git_ok(["config", "--local", "--list"]);
    fs::remove_file(&lock).unwrap();
    let retry = Operations::new(&adapter_for(&fixture.repo)).update_settings(&update);
    assert!(retry.ok(), "the retry writes: {:?}", retry.error);
    assert_eq!(retry.effects.configuration, ObservedEffect::Changed);
    assert_eq!(
        raw_config(&fixture.repo, "margin.autoCheckpoint").as_deref(),
        Some("true")
    );
}

/// Reads one repository-local configuration value through stock Git.
/// `None` means the key is unset.
fn raw_config(repo: &TestRepo, key: &str) -> Option<String> {
    let output = repo.git(["config", "--local", "--get", key]);
    if output.status.success() {
        Some(text(&output.stdout))
    } else {
        None
    }
}

/// One concurrent configuration writer that edits an unrelated key while
/// Margin updates its settings never corrupts the file: both writes stand,
/// the configuration parses, and the update reports its exact effect (SPEC
/// 8.2, 16.6 item 8).
#[test]
fn a_settings_update_survives_one_concurrent_config_edit() {
    let fixture = fixture();
    let script = write_git_wrapper(
        &fixture.repo,
        "concurrent-config.sh",
        concat!(
            "#!/bin/sh\n",
            "# Failpoint (SPEC 16.6 item 8): one concurrent writer edits an\n",
            "# unrelated configuration key around every Margin config call.\n",
            "if [ \"$1\" = \"config\" ]; then\n",
            "  \"{real}\" config --local test.concurrent writer >/dev/null 2>&1\n",
            "fi\n",
            "exec \"{real}\" \"$@\"\n",
        ),
    );
    let git = Git::from_env(fixture.repo.env().vars().clone())
        .at(fixture.repo.path())
        .with_program(script.into_os_string());
    let update = SettingsUpdate {
        auto_checkpoint: Some(true),
        ..SettingsUpdate::default()
    };

    let outcome = Operations::new(&git).update_settings(&update);
    assert!(
        outcome.ok(),
        "the update survives the concurrent writer: {:?}",
        outcome.error
    );
    assert_eq!(outcome.effects.configuration, ObservedEffect::Changed);
    assert_eq!(
        raw_config(&fixture.repo, "margin.autoCheckpoint").as_deref(),
        Some("true"),
        "the Margin setting was written"
    );
    assert_eq!(
        text(
            &fixture
                .repo
                .git_ok(["config", "--local", "--get", "test.concurrent"])
        ),
        "writer",
        "the concurrent writer's key survives the Margin update"
    );
    fixture.repo.git_ok(["config", "--local", "--list"]);
}

// ---------------------------------------------------------------------------
// The write quota (SPEC 16.6: full-disk and quota conditions)
// ---------------------------------------------------------------------------

/// One capture under one file-write quota — the deterministic analog of a
/// full disk in an isolated runner — fails typed through one bounded
/// envelope, leaves no commit, and the unhampered retry captures the same
/// content (SPEC 9.6, 16.6).
#[test]
fn a_capture_under_a_write_quota_fails_typed_and_recovers() {
    let repo = TestRepo::init();
    for number in 1..=16 {
        repo.write_file(
            &format!("f{number:02}.md"),
            format!("# File {number}\n").as_bytes(),
        );
    }
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let init = operations.initialize_scope(&InitRequest {
        scope: Some(scope()),
        create_repository: false,
        branch: None,
        new_start: false,
        auto_checkpoint: None,
        capture_after_edit: None,
        editor_argv: None,
        dry_run: false,
    });
    assert!(init.ok(), "the fixture initializes: {:?}", init.error);
    let start = oracle::head_oid(&repo).expect("the start marker");
    repo.write_file("f01.md", b"# File 1 revised\n");

    // One 512-byte file ceiling breaks the first large write of the
    // capture, whatever boundary it reaches.
    let output = margin_under_quota(&repo, 1, &["checkpoint", "--json"]);
    assert!(
        !output.stdout.is_empty(),
        "the quota run answers with one envelope"
    );
    let envelope: Value = serde_json::from_slice(&output.stdout).expect("one parseable envelope");
    assert_eq!(envelope["ok"], false, "the quota run must fail: {envelope}");
    assert_eq!(
        envelope["error"]["code"], "GIT_FAILED",
        "the quota failure is one typed Git failure: {envelope}"
    );

    // The observed effects of the failed capture are exact: no commit was
    // created, the index kept the start tree, and the working files kept
    // their bytes (SPEC 7.1, 9.6).
    assert_eq!(envelope["effects"]["commit"], "none");
    assert_eq!(envelope["data"]["commit"], Value::Null);
    assert_eq!(
        oracle::head_oid(&repo).as_deref(),
        Some(start.as_str()),
        "no commit was published under the quota"
    );
    assert_eq!(repo.read_file("f01.md"), b"# File 1 revised\n");
    assert_eq!(
        text(&repo.git_ok(["diff-index", "--cached", "--name-only", "HEAD"])),
        "",
        "nothing was staged under the quota"
    );

    // The unhampered retry captures the same content (SPEC 9.6: correct
    // next recovery option).
    let retry = margin(&repo, &["checkpoint", "--json"]);
    assert_eq!(retry.status.code(), Some(0));
    let envelope: Value = serde_json::from_slice(&retry.stdout).expect("one parseable envelope");
    assert_eq!(envelope["ok"], true, "the retry captures: {envelope}");
    assert_eq!(envelope["data"]["created"], true);
}
