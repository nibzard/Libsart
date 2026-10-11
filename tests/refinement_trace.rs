//! Trace-conformance tests for the refinement map (SPEC 15.7, task T038).
//!
//! Every scenario runs the real implementation over a real Git repository
//! with the test-only model-action trace of SPEC 15.7 turned on, then hands
//! the whole trace — the records the implementation emitted and the records
//! the harness appended for the environment — to the independent checker
//! in `tests/support/model.rs`. The checker replays the sequence against
//! the allowed transitions of `formal/MarginReview.tla`, so a protocol
//! change that leaves the model behind fails here even though every action
//! keeps its name.
//!
//! The scenarios cover the model actions one protocol run can take:
//!
//! - `Init`, `Checkpoint(FALSE)`, and the reply-only Submit branches:
//!   receipt and conflict
//! - `Submit` in apply mode with `ApplyGroup`, `UpdateIndex`, `Publish`,
//!   and `Respond`, including the keep-all path that skips the apply
//! - `Fail` at the commit boundary, then `InspectRecovery` and
//!   `Checkpoint(TRUE)` over the retained state: the modeled recovery
//! - `Crash` and `Restart` around one killed process, recovered through
//!   `InspectRecovery` and either the resume (`Submit` in resume mode) or
//!   the receipt retry
//! - the uncooperative writer of SPEC 15.6: one external third-value write
//!   inside the apply interval trips the `NoUnseenOverwrite` monitor and
//!   stops publication
//!
//! The negative tests at the end prove the checker itself rejects what the
//! model forbids, because shared naming alone is not refinement.
//!
//! The state claims stay independent of the implementation (SPEC 16.3):
//! every commit, tree, and byte the scenarios assert is checked with the
//! installed Git, never with a second Margin command.

mod support;

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::Duration;

use margin::app::{
    ChangeDetail, CheckpointRequest, DiffRequest, InitRequest, ModelTrace, Operations,
    RecoveryRequest, ReviewOutcome, ReviewRequest, validate_marker,
};
use margin::domain::{CommitKind, ScopeInput};
use margin::git::Git;
use serde_json::{Value, json};
use support::repo::TestRepo;
use support::{model, oracle, text};

// ---------------------------------------------------------------------------
// The fixture and the traced operation layer
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

/// The whole-repository Markdown scope.
fn scope() -> ScopeInput {
    ScopeInput {
        include: vec![".".to_string()],
        exclude: Vec::new(),
        suffixes: vec![".md".to_string()],
    }
}

/// The trace file every operation of one scenario appends to.
fn trace_path(repo: &TestRepo) -> PathBuf {
    repo.root().join("model-trace.jsonl")
}

/// Adapter bound to the fixture's working tree and isolated environment.
fn adapter_for(repo: &TestRepo) -> Git {
    Git::from_env(repo.env().vars().clone()).at(repo.path())
}

/// The operation layer with its trace attached, over `git`.
fn operations_with<'a>(git: &'a Git, trace: &Path) -> Operations<'a> {
    Operations::with_model_trace(git, ModelTrace::to_file(trace))
}

/// Initializes the whole-repository scope and returns the start marker.
fn initialize(repo: &TestRepo, trace: &Path) -> String {
    let git = adapter_for(repo);
    let result = operations_with(&git, trace).initialize_scope(&InitRequest {
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

/// Checkpoints the current scoped content and returns the draft commit.
fn checkpoint(repo: &TestRepo, trace: &Path) -> String {
    let git = adapter_for(repo);
    let result = operations_with(&git, trace).create_checkpoint(&CheckpointRequest {
        scope: None,
        message: None,
        include_staged: false,
        expect_head: None,
        expect_index: None,
        expect_working: None,
        dry_run: false,
    });
    assert!(result.ok(), "the checkpoint commits: {:?}", result.error);
    result
        .data
        .and_then(|data| data.commit)
        .expect("changed content creates one draft")
}

/// The pinned comparison of the workspace, with the identity fields a plan
/// pins to. The query runs untraced: one observation, no model action.
fn pinned_review(repo: &TestRepo) -> ReviewOutcome {
    let git = adapter_for(repo);
    let outcome = Operations::new(&git).build_review(&DiffRequest {
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

/// The change identities of the two passages of `doc-a.md`, in source
/// order, found through the recorded source ranges.
fn groups_of(pinned: &ReviewOutcome) -> Vec<String> {
    let mut groups: Vec<&ChangeDetail> = pinned
        .changes
        .iter()
        .filter(|change| change.path == "doc-a.md")
        .collect();
    assert_eq!(groups.len(), 2, "the document holds two passages");
    groups.sort_by_key(|change| change.ranges[0].old_start);
    groups
        .iter()
        .map(|change| change.change_id.clone())
        .collect()
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

/// Applies one plan document through the traced operation layer.
fn apply(
    repo: &TestRepo,
    trace: &Path,
    plan: &str,
) -> margin::app::OperationOutcome<margin::app::ReviewDecision> {
    let git = adapter_for(repo);
    operations_with(&git, trace).apply_review(&ReviewRequest {
        scope: None,
        plan: plan.as_bytes().to_vec(),
        dry_run: false,
    })
}

/// Resumes one retained plan through the traced operation layer.
fn resume(
    repo: &TestRepo,
    trace: &Path,
    plan: &str,
) -> margin::app::OperationOutcome<margin::app::ReviewDecision> {
    let git = adapter_for(repo);
    operations_with(&git, trace).resume_review(&ReviewRequest {
        scope: None,
        plan: plan.as_bytes().to_vec(),
        dry_run: false,
    })
}

/// Runs the traced recovery inspection and returns its report.
fn doctor(
    repo: &TestRepo,
    trace: &Path,
) -> margin::app::OperationOutcome<margin::app::RecoveryInspection> {
    let git = adapter_for(repo);
    operations_with(&git, trace).inspect_recovery(&RecoveryRequest { scope: None })
}

/// One scenario's shared start: the anchor, one cooperative external edit,
/// and one draft checkpoint over it. Returns the start marker and the
/// candidate.
fn seeded_draft(repo: &TestRepo, trace: &Path) -> (String, String) {
    repo.write_file(
        "doc-a.md",
        document("first original passage", "second original passage").as_bytes(),
    );
    let start = initialize(repo, trace);
    repo.write_file(
        "doc-a.md",
        document(
            "first revised passage\nwith two lines",
            "second revised passage",
        )
        .as_bytes(),
    );
    note_external_write(repo, trace, "doc-a.md", false);
    let candidate = checkpoint(repo, trace);
    (start, candidate)
}

// ---------------------------------------------------------------------------
// The trace and its harness records
// ---------------------------------------------------------------------------

/// Reads every record of one trace file, in order.
fn records(path: &Path) -> Vec<Value> {
    let body = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("cannot read the trace {}: {error}", path.display()));
    body.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("one malformed trace record {line:?}: {error}"))
        })
        .collect()
}

/// The action names of one trace file, in order.
fn actions(path: &Path) -> Vec<String> {
    records(path)
        .iter()
        .map(|record| {
            record["action"]
                .as_str()
                .expect("every record names its action")
                .to_string()
        })
        .collect()
}

/// The one record of one action name.
fn one_record<'a>(path: &Path, records: &'a [Value], action: &str) -> &'a Value {
    let found: Vec<&Value> = records
        .iter()
        .filter(|record| record["action"].as_str() == Some(action))
        .collect();
    assert_eq!(
        found.len(),
        1,
        "the trace {path:?} holds exactly one {action} record: {found:?}"
    );
    found[0]
}

/// Checks one whole trace against the model and returns its outcome.
fn conforming(path: &Path, cooperative: bool) -> model::Outcome {
    let body = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("cannot read the trace {}: {error}", path.display()));
    model::check(&body, cooperative)
        .unwrap_or_else(|message| panic!("the trace conforms to the model: {message}"))
}

/// Appends one harness record to the trace.
fn append_record(path: &Path, record: Value) {
    let mut line = record.to_string();
    line.push('\n');
    let mut file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .unwrap_or_else(|error| panic!("cannot append to the trace: {error}"));
    file.write_all(line.as_bytes())
        .unwrap_or_else(|error| panic!("cannot append to the trace: {error}"));
}

/// Records one process death the killed process cannot report itself
/// (`Crash`, SPEC 15.7).
fn note_crash(path: &Path) {
    append_record(path, json!({"action": "Crash"}));
}

/// Records one later process starting over the same repository (`Restart`,
/// SPEC 15.7).
fn note_restart(path: &Path) {
    append_record(path, json!({"action": "Restart"}));
}

/// Records one external write of `relative`, with the blob identity stock
/// Git computes for the bytes now on disk (`ExternalWrite`, SPEC 15.7).
/// `unseen` states that the written value differs from both the candidate
/// and the intended result of the active review: the third value of the
/// adversarial trace in SPEC 15.6.
fn note_external_write(repo: &TestRepo, path: &Path, relative: &str, unseen: bool) {
    let blob = text(&repo.git_ok(["hash-object", "--", relative]));
    append_record(
        path,
        json!({"action": "ExternalWrite", "paths": {relative: blob}, "unseen": unseen}),
    );
}

// ---------------------------------------------------------------------------
// The executable helpers of the process scenarios
// ---------------------------------------------------------------------------

/// The absolute path of the installed Margin executable.
fn margin_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_margin"))
}

/// Prepares one child command of the installed executable with the isolated
/// environment of `repo` and the model trace turned on.
fn margin_traced(repo: &TestRepo, trace: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(margin_binary());
    command
        .args(args)
        .current_dir(repo.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("MARGIN_MODEL_TRACE", trace);
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"GIT_") || key == "EMAIL" {
            command.env_remove(&key);
        }
    }
    command.envs(repo.env().vars());
    command
}

/// Writes one executable Git hook.
fn install_hook(repo: &TestRepo, name: &str, script: &str) {
    let path = repo.path().join(".git/hooks").join(name);
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Resolves the installed Git with `command -v git`, the same lookup the
/// shell of a hook or wrapper performs.
fn which_git() -> String {
    let output = Command::new("sh")
        .arg("-c")
        .arg("command -v git")
        .output()
        .expect("cannot locate the installed git");
    assert!(
        output.status.success(),
        "command -v git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    text(&output.stdout)
}

/// One `PATH` value whose `git` delegates every call to the installed Git
/// and kills the Margin parent after its first successful real commit, so
/// the published commit loses its response (SPEC 16.6 item 6).
fn crashing_git_path(repo: &TestRepo) -> std::ffi::OsString {
    let real = which_git();
    let dir = repo.root().join("shim");
    fs::create_dir_all(&dir).unwrap();
    let script = dir.join("git");
    fs::write(
        &script,
        format!(
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
            real = real,
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let mut path = dir.into_os_string();
    path.push(":");
    if let Some(parent) = std::env::var_os("PATH") {
        path.push(parent);
    }
    path
}

/// Writes the plan of one scenario to one file and returns its path.
fn plan_file(repo: &TestRepo, name: &str, plan: &str) -> String {
    let path = repo.root().join(name);
    fs::write(&path, plan).unwrap();
    path.to_str().unwrap().to_string()
}

/// Commits in the repository, counted with stock Git.
fn commit_count(repo: &TestRepo) -> usize {
    text(&repo.git_ok(["rev-list", "--count", "HEAD"]))
        .parse()
        .unwrap()
}

// ---------------------------------------------------------------------------
// Checkpoint and keep-all review
// ---------------------------------------------------------------------------

/// One keep-all review over one draft checkpoint follows the model exactly:
/// the anchor, one draft, one accepted submission that skips the apply, one
/// publication, and one response that names the receipt (SPEC 15.7).
#[test]
fn keep_all_publication_follows_the_model_phases() {
    let repo = TestRepo::init();
    let trace = trace_path(&repo);
    let (start, candidate) = seeded_draft(&repo, &trace);
    assert_eq!(
        candidate,
        oracle::head_oid(&repo).expect("the draft stands")
    );

    let pinned = pinned_review(&repo);
    assert_eq!(pinned.baseline_oid, start, "B is the anchor");
    let plan = plan_document(&pinned, "review-keep-all-01", &[], None);
    let outcome = apply(&repo, &trace, &plan);
    assert!(outcome.ok(), "the keep-all applies: {:?}", outcome.error);

    // The exact model sequence, ending in one idle workspace.
    assert_eq!(
        actions(&trace),
        [
            "Init",
            "ExternalWrite",
            "Checkpoint",
            "Submit",
            "Publish",
            "Respond"
        ],
        "one keep-all run takes exactly the modeled transitions"
    );
    let all = records(&trace);
    let submit = one_record(&trace, &all, "Submit");
    let publish = one_record(&trace, &all, "Publish");
    let respond = one_record(&trace, &all, "Respond");
    assert_eq!(submit["outcome"], json!("accepted"), "the plan is accepted");
    assert_eq!(submit["mode"], json!("apply"));
    assert_eq!(submit["head"], json!(candidate), "the submission pins C");
    assert_eq!(submit["baseline"], json!(start), "the submission pins B");
    assert_eq!(
        submit["restore"],
        json!([]),
        "keep-all restores nothing, so the model skips the apply phases"
    );
    assert_eq!(
        publish["parent"],
        json!(candidate),
        "the review commits over C"
    );
    let review = oracle::head_oid(&repo).expect("the review stands");
    assert_eq!(publish["head"], json!(review), "the publication is HEAD");
    assert_eq!(
        publish["tree"],
        json!(oracle::commit_tree(&repo, &review)),
        "the publication carries Git's own tree identity"
    );
    assert_eq!(
        respond["receipt"],
        json!(review),
        "the response names its receipt"
    );

    let outcome = conforming(&trace, true);
    assert!(!outcome.unseen_overwrite, "no third value intervened");
}

// ---------------------------------------------------------------------------
// Restoring review
// ---------------------------------------------------------------------------

/// One restoring review reports one `ApplyGroup` per selected group, then
/// the index result, then the publication that commits exactly that tree
/// (SPEC 15.7 `ApplyGroup` / `UpdateIndex` / `Publish`).
#[test]
fn restoring_application_applies_each_group_before_publication() {
    let repo = TestRepo::init();
    let trace = trace_path(&repo);
    let (start, candidate) = seeded_draft(&repo, &trace);

    let pinned = pinned_review(&repo);
    let restored = groups_of(&pinned);
    let plan = plan_document(&pinned, "review-restore-01", &restored, None);
    let outcome = apply(&repo, &trace, &plan);
    assert!(
        outcome.ok(),
        "the restoring plan applies: {:?}",
        outcome.error
    );

    let names = actions(&trace);
    let position = |action: &str| {
        names
            .iter()
            .position(|name| name == action)
            .unwrap_or_else(|| panic!("the trace holds one {action}: {names:?}"))
    };
    assert!(position("Submit") < position("ApplyGroup"));
    assert_eq!(
        names.iter().filter(|name| *name == "ApplyGroup").count(),
        2,
        "one record per selected group: {names:?}"
    );
    assert!(position("ApplyGroup") < position("UpdateIndex"));
    assert!(position("UpdateIndex") < position("Publish"));
    assert!(position("Publish") < position("Respond"));

    let all = records(&trace);
    let submit = one_record(&trace, &all, "Submit");
    let applied: Vec<String> = all
        .iter()
        .filter(|record| record["action"].as_str() == Some("ApplyGroup"))
        .map(|record| {
            record["group"]
                .as_str()
                .expect("the group identity")
                .to_string()
        })
        .collect();
    let selected: Vec<String> = submit["restore"]
        .as_array()
        .expect("the selected groups")
        .iter()
        .map(|group| group.as_str().unwrap().to_string())
        .collect();
    let mut sorted_applied = applied.clone();
    sorted_applied.sort();
    let mut sorted_selected = selected.clone();
    sorted_selected.sort();
    assert_eq!(
        sorted_applied, sorted_selected,
        "the applied groups are the selected groups"
    );
    for record in &all {
        if record["action"].as_str() == Some("ApplyGroup") {
            assert_eq!(
                record["head"],
                json!(candidate),
                "the candidate stays pinned"
            );
        }
    }
    let update = one_record(&trace, &all, "UpdateIndex");
    let publish = one_record(&trace, &all, "Publish");
    let review = oracle::head_oid(&repo).expect("the review stands");
    assert_eq!(update["tree"], json!(oracle::commit_tree(&repo, &review)));
    assert_eq!(
        publish["tree"], update["tree"],
        "the commit is the proved index result"
    );
    assert_eq!(publish["parent"], json!(candidate));
    assert_eq!(submit["baseline"], json!(start));
    assert!(!conforming(&trace, true).unseen_overwrite);
}

// ---------------------------------------------------------------------------
// Reply-only submissions
// ---------------------------------------------------------------------------

/// The receipt and conflict branches of `Submit` answer one already decided
/// request without one new transition, and one inspection over the idle
/// workspace is one stuttering observation (SPEC 15.7 `Submit`,
/// `InspectRecovery`).
#[test]
fn receipt_and_conflict_replies_stay_reply_only() {
    let repo = TestRepo::init();
    let trace = trace_path(&repo);
    seeded_draft(&repo, &trace);

    let pinned = pinned_review(&repo);
    let plan = plan_document(&pinned, "review-reply-01", &[], None);
    assert!(
        apply(&repo, &trace, &plan).ok(),
        "the first decision applies"
    );
    let review = oracle::head_oid(&repo).expect("the review stands");
    let after_decision = commit_count(&repo);

    // One read-only inspection over the idle workspace: a stutter the model
    // permits, and one record the checker accepts without a phase change.
    assert!(doctor(&repo, &trace).ok(), "the inspection succeeds");

    // The exact retry returns the receipt as one reply, with no new commit.
    let retry = apply(&repo, &trace, &plan);
    assert!(
        retry.ok(),
        "the retry reports the receipt: {:?}",
        retry.error
    );
    assert_eq!(commit_count(&repo), after_decision);

    // The same request identity under another plan conflicts, again with no
    // new commit.
    let other = plan_document(&pinned, "review-reply-01", &[], Some("another message"));
    let conflict = apply(&repo, &trace, &other);
    assert!(!conflict.ok(), "the reused identity conflicts");
    assert_eq!(
        conflict.error.as_ref().unwrap().code,
        "REQUEST_ID_CONFLICT",
        "the conflict keeps its public code"
    );
    assert_eq!(commit_count(&repo), after_decision);

    let all = records(&trace);
    let replies: Vec<&Value> = all
        .iter()
        .filter(|record| record["action"].as_str() == Some("Submit"))
        .collect();
    assert_eq!(replies.len(), 3, "one accepted, one receipt, one conflict");
    assert_eq!(replies[1]["outcome"], json!("receipt"));
    assert_eq!(
        replies[1]["receipt"],
        json!(review),
        "the retry names its receipt"
    );
    assert_eq!(replies[2]["outcome"], json!("conflict"));
    assert_eq!(
        actions(&trace).last().map(String::as_str),
        Some("Submit"),
        "replies end the run"
    );
    assert!(!conforming(&trace, true).unseen_overwrite);
}

// ---------------------------------------------------------------------------
// Failure, inspection, and the guarded staged capture
// ---------------------------------------------------------------------------

/// One application that fails at its commit boundary takes `Fail`, and the
/// modeled recovery returns through one read-only inspection and one guarded
/// staged capture: `Checkpoint(TRUE)` over exactly the inspected state
/// (SPEC 15.7 `Fail`, `InspectRecovery`, `Checkpoint(TRUE)`).
#[test]
fn a_failed_application_recovers_through_inspection_and_a_guarded_capture() {
    let repo = TestRepo::init();
    let trace = trace_path(&repo);
    let (_, candidate) = seeded_draft(&repo, &trace);

    let pinned = pinned_review(&repo);
    let restored = groups_of(&pinned);
    let plan = plan_document(&pinned, "review-fail-01", &restored, None);

    // The rejecting hook stops the application at its commit boundary, so
    // the accepted submission fails inside its protocol (SPEC 16.6 item 4).
    install_hook(&repo, "pre-commit", "#!/bin/sh\nexit 1\n");
    let failed = apply(&repo, &trace, &plan);
    assert!(!failed.ok(), "the rejecting hook stops the application");
    fs::remove_file(repo.path().join(".git/hooks/pre-commit")).unwrap();
    assert_eq!(
        oracle::head_oid(&repo).as_deref(),
        Some(candidate.as_str()),
        "the failure published nothing"
    );

    // The modeled recovery: one read-only inspection first, then the
    // guarded capture of exactly the inspected staged state.
    let inspected = doctor(&repo, &trace);
    assert!(
        inspected.ok(),
        "the inspection succeeds: {:?}",
        inspected.error
    );
    let report = inspected.data.expect("the recovery report");
    let head = report.head.expect("a born branch holds a head");
    let index_tree = report
        .index_tree
        .expect("an unconflicted index holds a tree");
    let working = report
        .scoped_working_digest
        .expect("a hashable scope holds a fingerprint")
        .id();

    let git = adapter_for(&repo);
    let captured = operations_with(&git, &trace).create_checkpoint(&CheckpointRequest {
        scope: None,
        message: None,
        include_staged: true,
        expect_head: Some(head),
        expect_index: Some(index_tree),
        expect_working: Some(working),
        dry_run: false,
    });
    assert!(
        captured.ok(),
        "the guarded capture commits the inspected state: {:?}",
        captured.error
    );

    assert_eq!(
        actions(&trace),
        [
            "Init",
            "ExternalWrite",
            "Checkpoint",
            "Submit",
            "ApplyGroup",
            "ApplyGroup",
            "UpdateIndex",
            "Fail",
            "InspectRecovery",
            "Checkpoint",
        ],
        "the failed protocol recovers through inspection and one guarded capture"
    );
    let all = records(&trace);
    let checkpoints: Vec<&Value> = all
        .iter()
        .filter(|record| record["action"].as_str() == Some("Checkpoint"))
        .collect();
    assert_eq!(checkpoints[0]["include_staged"], json!(false), "the draft");
    assert_eq!(
        checkpoints[1]["include_staged"],
        json!(true),
        "the recovery capture"
    );
    assert_eq!(checkpoints[1]["parent"], json!(candidate));
    let draft = checkpoints[1]["head"].as_str().unwrap();
    assert_eq!(draft, oracle::head_oid(&repo).expect("the capture stands"));
    assert_eq!(
        checkpoints[1]["tree"],
        json!(oracle::commit_tree(&repo, draft)),
        "the captured tree is Git's own identity"
    );
    assert!(!conforming(&trace, true).unseen_overwrite);
}

// ---------------------------------------------------------------------------
// Process death and the resume
// ---------------------------------------------------------------------------

/// One process interrupted between its application and its commit leaves
/// the trace at `UpdateIndex`; the harness records the death and the
/// restart, the inspection returns the workspace to idle, and the exact
/// resume publishes and responds: the modeled path back (SPEC 15.7
/// `Crash`, `Restart`, resume submission).
#[test]
fn an_interrupted_process_resumes_through_inspection() {
    let repo = TestRepo::init();
    let trace = trace_path(&repo);
    let (_, candidate) = seeded_draft(&repo, &trace);

    let pinned = pinned_review(&repo);
    let restored = groups_of(&pinned);
    let plan = plan_document(&pinned, "review-crash-01", &restored, None);
    let path = plan_file(&repo, "plan.json", &plan);

    // One sleeping hook holds the commit open, so the interrupt lands
    // between the application and the commit (SPEC 16.6: user
    // interruption). The hook marks its own start outside the working tree,
    // so the test interrupts exactly inside the commit it delays.
    let hook_marker = repo.root().join("hook-running");
    install_hook(
        &repo,
        "pre-commit",
        &format!(
            "#!/bin/sh\nprintf ok > '{}'\nsleep 30\nexit 0\n",
            hook_marker.display()
        ),
    );
    let mut command = margin_traced(&repo, &trace, &["review", "--input", &path, "--json"]);
    command.process_group(0);
    let child = command
        .spawn()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"));

    // The interrupt must land inside the commit the sleeping hook holds,
    // after the application staged and verified its whole result.
    let mut inside_commit = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if hook_marker.exists() {
            inside_commit = true;
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        inside_commit,
        "the commit reached its hook after the verified application"
    );
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
    assert_eq!(
        output.status.signal(),
        Some(2),
        "the process died from the terminal interrupt"
    );
    assert_eq!(
        oracle::head_oid(&repo).as_deref(),
        Some(candidate.as_str()),
        "the crash published nothing"
    );

    // The harness records what the killed process cannot: its death and the
    // process that follows it (SPEC 15.7).
    fs::remove_file(repo.path().join(".git/hooks/pre-commit")).unwrap();
    note_crash(&trace);
    note_restart(&trace);

    // The modeled order after one restart: inspect, then resume.
    assert!(doctor(&repo, &trace).ok(), "the inspection succeeds");
    let resumed = resume(&repo, &trace, &plan);
    assert!(
        resumed.ok(),
        "the exact retained result resumes: {:?}",
        resumed.error
    );

    assert_eq!(
        actions(&trace),
        [
            "Init",
            "ExternalWrite",
            "Checkpoint",
            "Submit",
            "ApplyGroup",
            "ApplyGroup",
            "UpdateIndex",
            "Crash",
            "Restart",
            "InspectRecovery",
            "Submit",
            "Publish",
            "Respond",
        ],
        "the interrupted run recovers exactly along the modeled path"
    );
    let all = records(&trace);
    let submissions: Vec<&Value> = all
        .iter()
        .filter(|record| record["action"].as_str() == Some("Submit"))
        .collect();
    assert_eq!(submissions[1]["mode"], json!("resume"), "the resume mode");
    assert_eq!(
        submissions[1]["head"],
        json!(candidate),
        "the resume pins C"
    );
    let review = oracle::head_oid(&repo).expect("the review stands");
    let publish = one_record(&trace, &all, "Publish");
    assert_eq!(publish["parent"], json!(candidate));
    assert_eq!(publish["head"], json!(review));
    assert!(!conforming(&trace, true).unseen_overwrite);
}

/// One process killed after its publication loses only its response: the
/// harness completes the `Publish` record from the durable commit that
/// stock Git and the shared marker validator observe, records the death and
/// the restart, and the retry answers with the receipt and no second commit
/// (SPEC 15.7 `Publish`, `Respond`, `Crash`).
#[test]
fn a_process_killed_after_publication_loses_only_its_response() {
    let repo = TestRepo::init();
    let trace = trace_path(&repo);
    let (_, candidate) = seeded_draft(&repo, &trace);

    let pinned = pinned_review(&repo);
    let restored = groups_of(&pinned);
    let plan = plan_document(&pinned, "review-loss-01", &restored, None);
    let path = plan_file(&repo, "plan.json", &plan);
    let count_before = commit_count(&repo);

    // The installed executable runs with the crashing Git wrapper first in
    // its PATH: the published review commit kills the process before it can
    // verify the commit or write one envelope byte.
    let mut command = margin_traced(&repo, &trace, &["review", "--input", &path, "--json"]);
    command.env("PATH", crashing_git_path(&repo));
    let output: Output = command
        .output()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"));
    assert_eq!(
        output.status.signal(),
        Some(9),
        "the process died from the kill"
    );
    assert!(
        output.stdout.is_empty(),
        "the killed process wrote no envelope: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    // The durable commit stands, proven with stock Git and the shared
    // marker validator; the killed process recorded nothing about it.
    let review = oracle::head_oid(&repo).expect("the published commit");
    assert_eq!(commit_count(&repo), count_before + 1);
    let marker = validate_marker(&adapter_for(&repo), &review)
        .unwrap_or_else(|error| panic!("the review marker validates: {error}"));
    assert_eq!(marker.kind, CommitKind::Review);
    let request = marker.request.expect("one review names its request");
    assert_eq!(
        actions(&trace).last().map(String::as_str),
        Some("UpdateIndex"),
        "the killed process recorded everything before its commit"
    );

    // The model's Publish is atomic; the implementation publishes through
    // Git and then verifies. The harness completes the record from the
    // durable state Git itself observes — the same observation the retry
    // performs — then records the death and the restart.
    append_record(
        &trace,
        json!({
            "action": "Publish",
            "request": request,
            "head": review,
            "parent": candidate,
            "tree": marker.result_tree,
        }),
    );
    note_crash(&trace);
    note_restart(&trace);

    assert!(doctor(&repo, &trace).ok(), "the inspection succeeds");
    let retry = apply(&repo, &trace, &plan);
    assert!(
        retry.ok(),
        "the retry reports the receipt: {:?}",
        retry.error
    );
    assert_eq!(commit_count(&repo), count_before + 1, "no second commit");

    let all = records(&trace);
    let reply = all
        .iter()
        .find(|record| {
            record["action"].as_str() == Some("Submit") && record["outcome"] == json!("receipt")
        })
        .expect("the retry answers with the receipt");
    assert_eq!(reply["receipt"], json!(review));
    assert!(!conforming(&trace, true).unseen_overwrite);
}

// ---------------------------------------------------------------------------
// The uncooperative writer
// ---------------------------------------------------------------------------

/// Feeds `bytes` to stock Git's `hash-object --stdin` and returns its blob
/// identity, so the harness never trusts Margin's own hashing.
fn blob_of(repo: &TestRepo, bytes: &[u8]) -> String {
    text(&repo.git_stdin(["hash-object", "--stdin"], bytes))
}

/// The adversarial trace of SPEC 15.6 under the real implementation: one
/// external write of a third value inside the apply interval trips the
/// `NoUnseenOverwrite` monitor, and the application stops at `Fail`; it
/// never publishes over content no side of the review held (SPEC 15.7
/// `ExternalWrite`, `NoUnseenOverwrite`).
#[test]
fn an_uncooperative_writer_stops_the_publication() {
    let repo = TestRepo::init();
    let trace = trace_path(&repo);
    let (_, candidate) = seeded_draft(&repo, &trace);

    let pinned = pinned_review(&repo);
    let restored = groups_of(&pinned);
    let plan = plan_document(&pinned, "review-unseen-01", &restored, None);

    // The unseen value: neither the candidate's bytes nor the reviewed
    // result's bytes, named by the blob identity stock Git computes.
    let unseen_bytes = document("first unseen value", "second unseen value");
    let blob = blob_of(&repo, unseen_bytes.as_bytes());
    let candidate_blob = text(&repo.git_ok(["rev-parse", &(candidate.to_string() + ":doc-a.md")]));
    assert_ne!(blob, candidate_blob, "the third value differs from C");

    // One wrapper delegates every call to the installed Git and, between
    // the two real unit applications, writes the unseen value and records
    // the environment action in the same trace (SPEC 15.6 adversarial
    // trace; 16.6 failpoint seam).
    let script = repo.root().join("unseen-seam.sh");
    let counter = repo.root().join("unseen-seam-count");
    let unseen_file = repo.root().join("unseen-value.md");
    fs::write(&unseen_file, unseen_bytes.as_bytes()).unwrap();
    fs::write(
        &script,
        concat!(
            "#!/bin/sh\n",
            "# Uncooperative-writer seam (SPEC 15.6, 15.7): delegate to the\n",
            "# installed git, and between the two real unit applications of a\n",
            "# restoring review, write one third value no side of the review\n",
            "# holds, recording the environment action in the model trace.\n",
            "count=0\n",
            "if [ -f \"$MARGIN_SEAM_COUNT\" ]; then\n",
            "  count=$(cat \"$MARGIN_SEAM_COUNT\")\n",
            "fi\n",
            "fire=0\n",
            "if [ -z \"$GIT_INDEX_FILE\" ] && [ \"$3\" = apply ]; then\n",
            "  case \" $* \" in\n",
            "    *\" --check \"*|*\" --cached \"*) ;;\n",
            "    *)\n",
            "      count=$((count + 1))\n",
            "      printf '%s' \"$count\" > \"$MARGIN_SEAM_COUNT\"\n",
            "      if [ \"$count\" -eq 2 ]; then fire=1; fi\n",
            "      ;;\n",
            "  esac\n",
            "fi\n",
            "if [ \"$fire\" = 1 ]; then\n",
            "  printf '%s\\n' ",
            "'{\"action\":\"ExternalWrite\",\"paths\":{\"doc-a.md\":\"BLOB\"},\"unseen\":true}'",
            " >> \"$MARGIN_MODEL_TRACE\"\n",
            // Git reads the patch from the wrapper's standard input, so the
            // action copies one prepared file instead of reading it.
            "  cp \"$MARGIN_SEAM_UNSEEN\" doc-a.md\n",
            "fi\n",
            "exec git \"$@\"\n",
        )
        .replace("BLOB", &blob),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

    let mut vars = repo.env().vars().clone();
    vars.insert(
        std::ffi::OsString::from("MARGIN_SEAM_COUNT"),
        counter.into_os_string(),
    );
    vars.insert(
        std::ffi::OsString::from("MARGIN_SEAM_UNSEEN"),
        unseen_file.into_os_string(),
    );
    vars.insert(
        std::ffi::OsString::from("MARGIN_MODEL_TRACE"),
        trace.as_os_str().to_os_string(),
    );
    let git = Git::from_env(vars)
        .at(repo.path())
        .with_program(script.into_os_string());
    let outcome = operations_with(&git, &trace).apply_review(&ReviewRequest {
        scope: None,
        plan: plan.as_bytes().to_vec(),
        dry_run: false,
    });
    assert!(
        !outcome.ok(),
        "the application over one unseen write must not succeed"
    );

    let names = actions(&trace);
    assert!(
        !names
            .iter()
            .any(|name| name == "Publish" || name == "Respond"),
        "no publication and no response follow the unseen write: {names:?}"
    );
    assert_eq!(names.last().map(String::as_str), Some("Fail"));
    assert_eq!(
        oracle::head_oid(&repo).as_deref(),
        Some(candidate.as_str()),
        "the candidate stands; nothing was published"
    );
    assert_eq!(
        repo.read_file("doc-a.md"),
        unseen_bytes.into_bytes(),
        "the unseen value stays on disk as retained recovery state"
    );

    // Under cooperation the same trace is one environment violation; the
    // model's cooperative run never permits it. Without cooperation, the
    // monitor trips and the conformance holds because the protocol stopped.
    let body = fs::read_to_string(&trace).unwrap();
    assert!(
        model::check(&body, true).is_err(),
        "a cooperative environment never writes inside the apply interval"
    );
    let verdict = model::check(&body, false)
        .unwrap_or_else(|message| panic!("the uncooperative trace conforms: {message}"));
    assert!(
        verdict.unseen_overwrite,
        "the monitor observed the third value"
    );
}

// ---------------------------------------------------------------------------
// The checker itself
// ---------------------------------------------------------------------------

/// The checker rejects what the model forbids, so shared action names alone
/// cannot pass the conformance check (SPEC 15.7).
#[test]
fn the_checker_rejects_sequences_the_model_forbids() {
    let init = r#"{"action":"Init","head":"i1","tree":"t1"}"#;
    let checkpoint =
        r#"{"action":"Checkpoint","include_staged":false,"head":"c1","parent":"i1","tree":"t2"}"#;
    let submit = |request: &str, head: &str, baseline: &str, restore: &[&str]| {
        let groups: Vec<String> = restore.iter().map(|group| format!("\"{group}\"")).collect();
        format!(
            concat!(
                r#"{{"action":"Submit","mode":"apply","outcome":"accepted","request":"{request}","#,
                r#""head":"{head}","baseline":"{baseline}","restore":[{groups}]}}"#
            ),
            request = request,
            head = head,
            baseline = baseline,
            groups = groups.join(","),
        )
    };
    let apply_group = |request: &str, group: &str, head: &str| {
        format!(
            r#"{{"action":"ApplyGroup","request":"{request}","group":"{group}","head":"{head}"}}"#
        )
    };
    let update_index = |request: &str, head: &str, tree: &str| {
        format!(
            r#"{{"action":"UpdateIndex","request":"{request}","head":"{head}","tree":"{tree}"}}"#
        )
    };
    let publish = |request: &str, head: &str, parent: &str, tree: &str| {
        format!(
            r#"{{"action":"Publish","request":"{request}","head":"{head}","parent":"{parent}","tree":"{tree}"}}"#
        )
    };
    let respond = |request: &str, receipt: &str| {
        format!(r#"{{"action":"Respond","request":"{request}","receipt":"{receipt}"}}"#)
    };
    let unseen_write =
        r#"{"action":"ExternalWrite","paths":{"doc.md":"b9"},"unseen":true}"#.to_string();

    // One complete conforming run, the control.
    let control = [
        init.to_string(),
        checkpoint.to_string(),
        submit("r1", "c1", "i1", &["g1"]),
        apply_group("r1", "g1", "c1"),
        update_index("r1", "c1", "t3"),
        publish("r1", "v1", "c1", "t3"),
        respond("r1", "v1"),
    ]
    .join("\n");
    assert!(model::check(&control, true).is_ok());

    let rejects = |name: &str, trace: String, expected: &str| {
        let error = model::check(&trace, true).unwrap_err();
        assert!(
            error.contains(expected),
            "{name} must be rejected because of {expected}, but the checker said: {error}"
        );
    };

    // Publication without one accepted submission.
    rejects(
        "one publish without one submission",
        [init.to_string(), publish("r1", "v1", "i1", "t2")].join("\n"),
        "Publish",
    );
    // One second receipt for one request.
    rejects(
        "one second receipt for one request",
        [
            init.to_string(),
            submit("r1", "i1", "i1", &[]),
            publish("r1", "v1", "i1", "t2"),
            respond("r1", "v1"),
            submit("r1", "v1", "v1", &[]),
        ]
        .join("\n"),
        "RequestAtMostOnce",
    );
    // One accepted submission for one request that already holds a receipt.
    rejects(
        "one acceptance over one existing receipt",
        [
            init.to_string(),
            submit("r1", "i1", "i1", &[]),
            publish("r1", "v1", "i1", "t2"),
            respond("r1", "v1"),
            submit("r1", "v1", "v1", &[]),
            publish("r1", "v2", "v1", "t3"),
            respond("r1", "v2"),
        ]
        .join("\n"),
        "RequestAtMostOnce",
    );
    // One checkpoint inside the active protocol.
    rejects(
        "one checkpoint during the apply",
        [
            init.to_string(),
            submit("r1", "i1", "i1", &["g1"]),
            checkpoint.to_string(),
            apply_group("r1", "g1", "i1"),
        ]
        .join("\n"),
        "idle",
    );
    // One accepted submission pinned to one baseline the trace never
    // reached.
    rejects(
        "one stale baseline acceptance",
        [
            init.to_string(),
            checkpoint.to_string(),
            submit("r1", "c1", "b0", &[]),
        ]
        .join("\n"),
        "BaselineIsReviewed",
    );
    // One response that names another commit.
    rejects(
        "one response naming another commit",
        [
            init.to_string(),
            submit("r1", "i1", "i1", &[]),
            publish("r1", "v1", "i1", "t2"),
            respond("r1", "other"),
        ]
        .join("\n"),
        "SuccessHasReceipt",
    );
    // One cooperative external write inside the active protocol.
    rejects(
        "one cooperative write during the apply",
        [
            init.to_string(),
            submit("r1", "i1", "i1", &["g1"]),
            unseen_write.clone(),
            apply_group("r1", "g1", "i1"),
        ]
        .join("\n"),
        "cooperative",
    );
    // One index publication before every group applied.
    rejects(
        "one index update before the groups completed",
        [
            init.to_string(),
            submit("r1", "i1", "i1", &["g1", "g2"]),
            apply_group("r1", "g1", "i1"),
            update_index("r1", "i1", "t3"),
        ]
        .join("\n"),
        "UpdateIndex",
    );
    // One publication over a different tree than the application verified.
    rejects(
        "one publication of another tree",
        [
            init.to_string(),
            submit("r1", "i1", "i1", &["g1"]),
            apply_group("r1", "g1", "i1"),
            update_index("r1", "i1", "t3"),
            publish("r1", "v1", "i1", "t4"),
        ]
        .join("\n"),
        "verified",
    );
    // One unseen write followed by one publication: the uncooperative
    // monitor must stop it.
    let over_unseen = [
        init.to_string(),
        submit("r1", "i1", "i1", &["g1"]),
        unseen_write,
        apply_group("r1", "g1", "i1"),
        update_index("r1", "i1", "t3"),
        publish("r1", "v1", "i1", "t3"),
    ]
    .join("\n");
    let error = model::check(&over_unseen, false).unwrap_err();
    assert!(
        error.contains("NoUnseenOverwrite"),
        "the publication over the unseen value must fail on the monitor: {error}"
    );
    // One restart that skips the crash it follows.
    rejects(
        "one restart without one crash",
        [init.to_string(), r#"{"action":"Restart"}"#.to_string()].join("\n"),
        "down",
    );
    // One failure outside the active protocol.
    rejects(
        "one failure from one idle workspace",
        [
            init.to_string(),
            r#"{"action":"Fail","request":"r1"}"#.to_string(),
        ]
        .join("\n"),
        "Fail",
    );
}
