//! Real-Git tests for scoped draft checkpoints (SPEC 9.2, task T020).
//!
//! Every case proves one contract of the capture against the installed Git:
//!
//! - one checkpoint commits exactly the eligible additions, edits, mode
//!   changes, and deletions from a globally clean index, and the created
//!   commit verifies against stock Git: parent, full tree, bytes, modes,
//!   and the reserved draft trailers
//! - unchanged scoped content is the explicit no-op of `created:false`
//!   with the current candidate and no effects
//! - out-of-scope working files and tree entries stay unchanged, and the
//!   capture confers no approval: the baseline a review needs stays the
//!   anchor the walk selected before the draft, including below an
//!   ordinary external commit
//! - a dry run reports the intended tree without writing; an invalid
//!   explicit message refuses before anything is staged
//! - a checkpoint runs only on the adopted branch: another branch — even
//!   at the same commit — refuses `BRANCH_CHANGED` before anything is
//!   staged, a detached `HEAD` and an unadopted workspace keep their own
//!   refusals, and one explicit init adopts the new branch
//! - a hook that rejects the commit leaves the staged changes with the
//!   candidate anchor and exact effects, and the next checkpoint stops on
//!   that staged state; a hook that rewrites the reserved trailers produces
//!   an ordinary commit reported as a contract violation
//! - a capture failure reports the observed repository state: a staging
//!   failure and a hook that edits working files before rejecting report
//!   the inspected index, working tree, and head; a killed commit process
//!   keeps the outcome unknown while an output-limit failure reached after
//!   the commit still names the created commit
//! - a writer who changes a file after it was staged sees a truthful
//!   warning about newer uncaptured content, never a claim that those
//!   bytes are saved
//! - the shared mutation lock of SPEC 9.1 step 2 serializes the capture:
//!   one held lock refuses `BUSY` without intended effects — through the
//!   operation layer and through the installed executable — and every
//!   return path, refusal or failure included, releases the lock for the
//!   next operation
//! - the precondition recheck of SPEC 9.1 step 6 refuses before staging
//!   when `HEAD`, the branch, the index, the scoped bytes, or a path kind
//!   moved while the capture was prepared: the deterministic process seam
//!   injects one concurrent writer at the intended-tree proof, and nothing
//!   is staged on refusal
//! - an unborn branch and an uninitialized repository refuse without
//!   writing, and both object formats capture drafts

mod support;

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use margin::app::{
    BaselineStatus, CheckpointRequest, CommitEffect, Effects, InitRequest, InspectRequest,
    ObservedEffect, OperationOutcome, Operations, RepositoryEffect, resolve_scope, select_baseline,
    validate_marker,
};
use margin::domain::{CommitKind, ScopeInput};
use margin::git::{Git, Limits};
use support::env::{IsolatedEnv, is_scrubbed};
use support::mutation::{SeamPass, hold_mutation_lock, lock_path, seam_adapter};
use support::repo::{ObjectFormat, RepoOptions, TestRepo};
use support::{oracle, text};

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
        dry_run: false,
    }
}

/// Initializes the `notes` scope and returns the start commit that anchors
/// it.
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
        .as_ref()
        .and_then(|data| data.start_commit.clone())
        .expect("first use creates one start commit")
}

/// Writes one executable Git hook.
fn install_hook(repo: &TestRepo, name: &str, script: &str) {
    let path = repo.path().join(".git/hooks").join(name);
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Reads one repository-local configuration value through Git itself.
fn raw_config(repo: &TestRepo, key: &str) -> Option<String> {
    let output = repo.git(["config", "--local", "--get", key]);
    if output.status.success() {
        Some(text(&output.stdout))
    } else {
        None
    }
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

/// The reserved `Margin-` trailer lines of one commit, from stock Git.
fn trailers_of(repo: &TestRepo, commit: &str) -> Vec<String> {
    String::from_utf8_lossy(&repo.git_ok(["cat-file", "commit", commit]))
        .lines()
        .filter(|line| line.starts_with("Margin-"))
        .map(str::to_string)
        .collect()
}

/// The tree mode Git records for the permission bits on disk.
fn tree_mode_of(repo: &TestRepo, relative: &str) -> &'static str {
    if oracle::disk_mode(repo, relative) & 0o100 != 0 {
        "100755"
    } else {
        "100644"
    }
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

#[test]
fn checkpoints_commit_exactly_the_eligible_changes() {
    let repo = TestRepo::init();
    repo.write_file("notes/edited.md", b"# Edited\n");
    repo.write_file("notes/deleted.md", b"# Gone\n");
    repo.write_file("notes/mode.md", b"# Mode\n");
    repo.write_file("other/sentinel.txt", b"out of scope\n");
    repo.commit_all("import");
    let start = initialize(&repo);
    let parent = oracle::head_oid(&repo).unwrap();

    // One edit, one eligible untracked addition, one deletion, and one pure
    // mode change are all eligible scoped changes (SPEC 9.2).
    repo.write_file("notes/edited.md", b"# Edited twice\n");
    repo.write_file("notes/added.md", b"# Added\n");
    repo.write_file("notes/odd name.md", b"# Odd\n");
    fs::remove_file(repo.file_path("notes/deleted.md")).unwrap();
    repo.set_mode("notes/mode.md", 0o755);
    // An ignored scoped path stays outside the eligible set.
    repo.write_file("notes/.ignore-entry.md", b"ignored\n");
    fs::write(repo.file_path("notes/.gitignore"), ".ignore-entry.md\n").unwrap();
    let before = oracle::snapshot(&repo);

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let scope = scope_input(&["notes"], &[], &[".md"]);
    let result =
        operations.create_checkpoint(&checkpoint_of(&scope, Some("tighten the introduction")));

    assert!(
        result.ok(),
        "the checkpoint must succeed: {:?}",
        result.error
    );
    assert!(result.warnings.is_empty());
    let data = result.data.as_ref().unwrap();
    let commit = data.commit.as_deref().expect("a draft commit");
    assert!(data.changed);
    assert_eq!(data.candidate, commit);
    assert!(!data.dry_run);
    assert_eq!(
        data.paths,
        vec![
            OsString::from("notes/added.md"),
            OsString::from("notes/deleted.md"),
            OsString::from("notes/edited.md"),
            OsString::from("notes/mode.md"),
            OsString::from("notes/odd name.md"),
        ],
        "the captured set is the exact eligible change list in path order"
    );
    assert_eq!(oracle::head_oid(&repo).as_deref(), Some(commit));

    // Parent, tree, and trailers verify against stock Git.
    let marker = validate_marker(&git, commit)
        .unwrap_or_else(|error| panic!("the draft must validate: {error}"));
    assert_eq!(marker.kind, CommitKind::Draft);
    assert_eq!(marker.candidate, parent);
    let tree = oracle::commit_tree(&repo, commit);
    assert_eq!(tree, marker.result_tree);
    assert_eq!(data.expected_tree.as_deref(), Some(tree.as_str()));
    let resolved = resolve_scope(&git, &scope).unwrap();
    let mut trailers = trailers_of(&repo, commit);
    trailers.sort();
    let mut expected = vec![
        "Margin-Kind: draft".to_string(),
        format!("Margin-Candidate: {parent}"),
        format!("Margin-Result-Tree: {tree}"),
        format!("Margin-Scope: {}", resolved.digest.id()),
        format!(
            "Margin-Scope-Spec: {}",
            String::from_utf8_lossy(&resolved.descriptor.canonical_bytes())
        ),
        "Margin-Version: 1".to_string(),
    ];
    expected.sort();
    assert_eq!(trailers, expected, "the draft carries exactly its trailers");
    let raw = String::from_utf8_lossy(&repo.git_ok(["cat-file", "commit", commit])).to_string();
    let message = raw
        .split_once("\n\n")
        .map(|(_, message)| message)
        .unwrap_or("");
    assert!(
        message.starts_with("tighten the introduction\n\nMargin-"),
        "the explicit message is the free-text subject, got: {message:?}"
    );

    // Bytes and modes: every scoped tree entry equals the working file.
    let all_entries = oracle::ls_tree(&repo, commit);
    let entries: Vec<&oracle::TreeEntry> = all_entries
        .iter()
        .filter(|entry| oracle::in_scope(&entry.path, &["notes"]))
        .collect();
    let mut paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
    paths.sort();
    assert_eq!(
        paths,
        vec![
            "notes/added.md",
            "notes/edited.md",
            "notes/mode.md",
            "notes/odd name.md",
        ],
        "the deletion left the tree, the ignored entry stayed out, and the\
         \nnon-matching ignore file stays outside the suffix scope"
    );
    for entry in entries {
        assert_eq!(entry.kind, "blob");
        assert_eq!(
            entry.mode,
            tree_mode_of(&repo, &entry.path),
            "the mode of {} is the working mode",
            entry.path
        );
        assert_eq!(
            oracle::blob(&repo, &entry.oid),
            repo.read_file(&entry.path),
            "the stored bytes of {} are the raw working bytes",
            entry.path
        );
    }
    assert!(
        !repo
            .git([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{commit}:notes/deleted.md")
            ])
            .status
            .success(),
        "the deletion is committed"
    );

    // Nothing out of scope moved, in the tree or on disk, and the index
    // holds exactly the committed tree.
    oracle::assert_out_of_scope_tree_unchanged(&repo, &parent, commit, &["notes"], "checkpoint");
    oracle::assert_index_matches_tree(&repo, commit);
    before.assert_out_of_scope_unchanged(&oracle::snapshot(&repo), &["notes"], "checkpoint");

    // Approval stays unchanged: the walk still anchors at the start marker,
    // and one draft never reads as reviewed (SPEC 8.1).
    let anchor = select_baseline(&git, commit, &resolved.digest)
        .unwrap_or_else(|error| panic!("the baseline must select: {error}"));
    assert_eq!(anchor.commit, start);
    assert_eq!(anchor.kind, CommitKind::Start);
    let status = operations.inspect_workspace(&InspectRequest {
        scope: None,
        path_filter: Vec::new(),
    });
    assert!(status.ok());
    let observed = status.data.as_ref().unwrap();
    assert_eq!(observed.preservation.as_str(), "captured");
    assert_eq!(observed.review.as_str(), "pending");

    // Effects report the staged capture and the created commit (SPEC 7.1).
    assert_eq!(result.effects.index, ObservedEffect::Changed);
    assert_eq!(result.effects.commit, CommitEffect::Created);
    assert_eq!(result.effects.commit_oid.as_deref(), Some(commit));
    assert_eq!(result.effects.working_tree, ObservedEffect::Unchanged);
    assert_eq!(result.effects.repository, RepositoryEffect::Unchanged);
    assert_eq!(result.effects.configuration, ObservedEffect::Unchanged);
    assert_eq!(result.effects.integration, ObservedEffect::Unchanged);
}

#[test]
fn unchanged_content_is_the_documented_no_op() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let start = initialize(&repo);
    let before = oracle::snapshot(&repo);

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.create_checkpoint(&checkpoint_of(
        &scope_input(&["notes"], &[], &[".md"]),
        None,
    ));

    // Unchanged content returns `created:false`, the current candidate, and
    // no effects (SPEC 9.2).
    assert!(result.ok(), "the no-op must succeed");
    assert!(result.warnings.is_empty());
    let data = result.data.as_ref().unwrap();
    assert_eq!(data.commit, None);
    assert!(!data.changed);
    assert_eq!(data.candidate, start);
    assert!(data.paths.is_empty());
    assert!(!data.dry_run);
    assert_eq!(data.expected_tree, None);
    assert_eq!(result.effects, Effects::unchanged());
    before.assert_unchanged(&oracle::snapshot(&repo), "an unchanged checkpoint");

    // The next checkpoint of the same content stays a no-op: an unchanged
    // checkpoint creates no commit and no receipt (SPEC 8.7).
    let again = operations.create_checkpoint(&checkpoint_of(
        &scope_input(&["notes"], &[], &[".md"]),
        Some("explicit message"),
    ));
    assert!(again.ok());
    let data = again.data.as_ref().unwrap();
    assert_eq!(data.commit, None);
    assert!(!data.changed);
    assert_eq!(data.candidate, start);
}

#[test]
fn out_of_scope_dirt_stays_out_of_the_capture() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.write_file("other/sentinel.txt", b"out of scope\n");
    repo.commit_all("import");
    initialize(&repo);
    let parent = oracle::head_oid(&repo).unwrap();

    // A scoped edit plus one modified and one untracked out-of-scope file:
    // only the scoped edit belongs to the capture.
    repo.write_file("notes/a.md", b"# One revised\n");
    repo.write_file("other/sentinel.txt", b"out of scope, edited\n");
    repo.write_file("other/extra.txt", b"untracked outside\n");
    let sentinel_before = repo.read_file("other/sentinel.txt");

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.create_checkpoint(&checkpoint_of(
        &scope_input(&["notes"], &[], &[".md"]),
        None,
    ));

    assert!(
        result.ok(),
        "the checkpoint must succeed: {:?}",
        result.error
    );
    let data = result.data.as_ref().unwrap();
    assert_eq!(
        data.paths,
        vec![OsString::from("notes/a.md")],
        "only the scoped path is captured"
    );
    let commit = data.commit.as_deref().unwrap();

    // The out-of-scope tree entries are exactly the parent's, and the
    // out-of-scope working bytes are untouched, still uncaptured by Margin.
    oracle::assert_out_of_scope_tree_unchanged(&repo, &parent, commit, &["notes"], "checkpoint");
    assert_eq!(repo.read_file("other/sentinel.txt"), sentinel_before);
    assert_eq!(
        repo.read_file("other/extra.txt"),
        b"untracked outside\n",
        "the untracked out-of-scope file stays on disk"
    );
    assert_eq!(
        text(&repo.git_ok(["diff-index", "--cached", "--name-only", "HEAD"])),
        "",
        "nothing out of scope was staged"
    );
    assert!(
        text(&repo.git_ok(["diff-files", "--name-only"])).contains("other/sentinel.txt"),
        "the out-of-scope edit stays a working-tree difference"
    );
}

#[test]
fn a_draft_after_an_ordinary_commit_keeps_the_prior_baseline() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.write_file("other/sentinel.txt", b"out of scope\n");
    repo.commit_all("import");
    let start = initialize(&repo);

    // An ordinary external commit is a new candidate and leaves the prior
    // baseline unchanged (SPEC 8.5); the next draft captures on top of it.
    repo.write_file("other/sentinel.txt", b"out of scope, moved\n");
    repo.commit_all("an ordinary external commit");
    let parent = oracle::head_oid(&repo).unwrap();
    repo.write_file("notes/a.md", b"# One revised\n");

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let scope = scope_input(&["notes"], &[], &[".md"]);
    let result = operations.create_checkpoint(&checkpoint_of(&scope, None));

    assert!(
        result.ok(),
        "the checkpoint must succeed: {:?}",
        result.error
    );
    let data = result.data.as_ref().unwrap();
    let commit = data.commit.as_deref().unwrap();
    let marker = validate_marker(&git, commit)
        .unwrap_or_else(|error| panic!("the draft must validate: {error}"));
    assert_eq!(marker.candidate, parent);
    oracle::assert_out_of_scope_tree_unchanged(&repo, &parent, commit, &["notes"], "checkpoint");

    // The walk still anchors at the start marker below the ordinary commit:
    // the new draft is a candidate, never approval (SPEC 8.5).
    let resolved = resolve_scope(&git, &scope).unwrap();
    let anchor = select_baseline(&git, commit, &resolved.digest)
        .unwrap_or_else(|error| panic!("the baseline must select: {error}"));
    assert_eq!(anchor.commit, start);
    let status = operations.inspect_workspace(&InspectRequest {
        scope: None,
        path_filter: Vec::new(),
    });
    assert!(status.ok());
    let observed = status.data.as_ref().unwrap();
    match &observed.baseline {
        BaselineStatus::Anchored(anchor) => assert_eq!(anchor.commit, start),
        other => panic!("the start marker must still anchor: {other:?}"),
    }
    assert_eq!(observed.review.as_str(), "pending");
}

#[test]
fn a_dry_run_reports_the_intended_tree_without_writing() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    initialize(&repo);
    repo.write_file("notes/a.md", b"# One revised\n");
    repo.write_file("notes/added.md", b"# Added\n");
    let before = oracle::snapshot(&repo);

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let scope = scope_input(&["notes"], &[], &[".md"]);
    let mut request = checkpoint_of(&scope, Some("draft wording"));
    request.dry_run = true;
    let result = operations.create_checkpoint(&request);

    assert!(result.ok(), "the dry run must succeed: {:?}", result.error);
    let data = result.data.as_ref().unwrap();
    assert!(data.dry_run);
    assert!(data.changed);
    assert_eq!(data.commit, None);
    let intended = data.expected_tree.clone().expect("an intended tree");
    before.assert_unchanged(&oracle::snapshot(&repo), "a checkpoint dry run");

    // The intended tree is what the real capture then commits (SPEC 6.4).
    let real = operations.create_checkpoint(&checkpoint_of(&scope, Some("draft wording")));
    assert!(real.ok());
    let committed = real.data.as_ref().unwrap().commit.clone().unwrap();
    assert_eq!(
        oracle::commit_tree(&repo, &committed),
        intended,
        "the dry run predicted the committed tree"
    );
}

#[test]
fn an_invalid_message_refuses_before_anything_is_staged() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    initialize(&repo);
    repo.write_file("notes/a.md", b"# One revised\n");
    let before = oracle::snapshot(&repo);

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.create_checkpoint(&checkpoint_of(
        &scope_input(&["notes"], &[], &[".md"]),
        Some("text\nMargin-Kind: review"),
    ));

    // A message that injects a reserved trailer can never become a commit,
    // and the refusal stages nothing (SPEC 7.3).
    assert_refusal(&result, "INVALID_ARGUMENT", "an injecting message");
    assert!(
        result
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("reserved Margin trailer"),
        "the diagnosis names the injection"
    );
    before.assert_unchanged(&oracle::snapshot(&repo), "a refused checkpoint");
}

#[test]
fn a_branch_switch_after_adoption_refuses_the_checkpoint() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    initialize(&repo);
    // Another branch at the same commit: the head object ID cannot tell the
    // two apart, only the configured branch name can (SPEC 8.2, INV-08).
    let start = oracle::head_oid(&repo).unwrap();
    repo.git_ok(["switch", "--quiet", "--create", "other"]);
    assert_eq!(
        oracle::head_oid(&repo).as_deref(),
        Some(start.as_str()),
        "the other branch starts at the same commit"
    );
    repo.write_file("notes/a.md", b"# One revised\n");
    let before = oracle::snapshot(&repo);

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let scope = scope_input(&["notes"], &[], &[".md"]);
    let result = operations.create_checkpoint(&checkpoint_of(&scope, None));

    // The mismatch refuses `BRANCH_CHANGED` with nothing written: no
    // staging, no commit, no settings change, and no branch switch
    // (SPEC 9.1 step 4, 8.2).
    assert_refusal(&result, "BRANCH_CHANGED", "a checkpoint on another branch");
    let error = result.error.as_ref().unwrap();
    assert!(
        error.message.contains("other") && error.message.contains("main"),
        "the diagnosis names both branches: {}",
        error.message
    );
    assert_eq!(
        error.recovery_argv,
        vec![
            "margin".to_string(),
            "init".to_string(),
            "--json".to_string()
        ],
        "the recovery names the explicit adoption path"
    );
    assert_eq!(
        text(&repo.git_ok(["symbolic-ref", "--quiet", "HEAD"])),
        "refs/heads/other",
        "the checkpoint never switches the branch itself"
    );
    assert_eq!(oracle::head_oid(&repo).as_deref(), Some(start.as_str()));
    assert_eq!(
        text(&repo.git_ok(["diff-index", "--cached", "--name-only", "HEAD"])),
        "",
        "the refusal staged nothing"
    );
    assert_eq!(
        raw_config(&repo, "margin.branch").as_deref(),
        Some("main"),
        "the adoption stays what it was"
    );
    assert_eq!(
        repo.read_file("notes/a.md"),
        b"# One revised\n",
        "the edit stays on disk"
    );
    before.assert_unchanged(
        &oracle::snapshot(&repo),
        "a refused foreign-branch checkpoint",
    );

    // A dry run passes through the same shared guard and writes nothing.
    let mut dry = checkpoint_of(&scope, None);
    dry.dry_run = true;
    let dry_run = operations.create_checkpoint(&dry);
    assert_refusal(&dry_run, "BRANCH_CHANGED", "a foreign-branch dry run");

    // One explicit init on the new branch adopts it; the checkpoint then
    // captures the same edit there (SPEC 8.2).
    let adoption = operations.initialize_scope(&first_use(&scope));
    assert!(
        adoption.ok(),
        "the explicit init adopts the branch: {:?}",
        adoption.error
    );
    assert_eq!(
        raw_config(&repo, "margin.branch").as_deref(),
        Some("other"),
        "the adoption now records the new branch"
    );
    let retry = operations.create_checkpoint(&checkpoint_of(&scope, None));
    assert!(
        retry.ok(),
        "the checkpoint follows the adoption: {:?}",
        retry.error
    );
    let data = retry.data.as_ref().unwrap();
    let commit = data.commit.as_deref().unwrap();
    assert_eq!(oracle::head_oid(&repo).as_deref(), Some(commit));
    assert_eq!(
        oracle::blob(
            &repo,
            &text(&repo.git_ok(["rev-parse", &format!("{commit}:notes/a.md")]))
        ),
        b"# One revised\n",
        "the adopted branch now holds the edit"
    );
}

#[test]
fn detached_head_and_unadopted_workspaces_keep_their_refusals() {
    // A detached HEAD keeps the repository policy's own refusal; the
    // adopted-branch comparison never replaces it (SPEC 3.1, 9.1 step 4).
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    initialize(&repo);
    repo.write_file("notes/a.md", b"# One revised\n");
    repo.git_ok(["checkout", "--quiet", "--detach"]);
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.create_checkpoint(&checkpoint_of(
        &scope_input(&["notes"], &[], &[".md"]),
        None,
    ));
    assert_refusal(&result, "UNSUPPORTED_REPOSITORY", "a detached checkpoint");
    assert!(
        result.error.as_ref().unwrap().message.contains("detached"),
        "the diagnosis names the detached HEAD"
    );

    // A workspace without adoption settings keeps the uninitialized
    // refusal; the branch guard adds no second vocabulary for it.
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let before = oracle::snapshot(&repo);
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.create_checkpoint(&CheckpointRequest {
        scope: None,
        message: None,
        include_staged: false,
        dry_run: false,
    });
    assert_refusal(
        &result,
        "NOT_INITIALIZED",
        "a checkpoint without adoption settings",
    );
    assert_eq!(
        result.error.as_ref().unwrap().recovery_argv,
        vec![
            "margin".to_string(),
            "init".to_string(),
            "--json".to_string()
        ]
    );
    before.assert_unchanged(&oracle::snapshot(&repo), "an unadopted checkpoint");
}

#[test]
fn a_rejecting_hook_leaves_staged_changes_with_exact_effects() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let start = initialize(&repo);
    repo.write_file("notes/a.md", b"# One revised\n");
    install_hook(&repo, "pre-commit", "#!/bin/sh\nexit 1\n");

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.create_checkpoint(&checkpoint_of(
        &scope_input(&["notes"], &[], &[".md"]),
        None,
    ));

    // The commit failed, so the staged changes remain and the response
    // reports the candidate anchor and the exact effects (SPEC 9.2).
    assert!(!result.ok(), "a rejecting hook must fail the checkpoint");
    assert_eq!(result.error.as_ref().unwrap().code, "GIT_FAILED");
    assert_eq!(result.effects.index, ObservedEffect::Changed);
    assert_eq!(result.effects.commit, CommitEffect::None);
    assert_eq!(result.effects.commit_oid, None);
    let data = result.data.as_ref().expect("the observed state");
    assert_eq!(data.commit, None);
    assert!(data.changed);
    assert_eq!(data.candidate, start);
    assert_eq!(oracle::head_oid(&repo).as_deref(), Some(start.as_str()));
    assert_eq!(
        text(&repo.git_ok(["diff-index", "--cached", "--name-only", "HEAD"])),
        "notes/a.md",
        "the capture stayed staged"
    );

    // Later checkpoints stop on that staged state instead of consuming it.
    let retry = operations.create_checkpoint(&checkpoint_of(
        &scope_input(&["notes"], &[], &[".md"]),
        None,
    ));
    assert_refusal(&retry, "STAGED_CHANGES", "a checkpoint over staged state");
}

#[test]
fn a_hook_that_rewrites_trailers_reports_a_contract_violation() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    initialize(&repo);
    repo.write_file("notes/a.md", b"# One revised\n");
    install_hook(
        &repo,
        "commit-msg",
        "#!/bin/sh\nprintf 'Margin-Kind: review\\n' >> \"$1\"\nexit 0\n",
    );

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.create_checkpoint(&checkpoint_of(
        &scope_input(&["notes"], &[], &[".md"]),
        None,
    ));

    // The commit exists but its reserved trailers were rewritten, so it is
    // ordinary history that confers no approval (SPEC 9.6).
    assert!(!result.ok(), "a rewritten marker must fail verification");
    assert_eq!(
        result.error.as_ref().unwrap().code,
        "CONTRACT_VIOLATION",
        "the hook-rewritten commit is reported"
    );
    let created = oracle::head_oid(&repo).expect("the commit itself succeeded");
    assert_eq!(result.effects.commit, CommitEffect::Created);
    assert_eq!(result.effects.commit_oid.as_deref(), Some(created.as_str()));
    assert!(validate_marker(&git, &created).is_err());
    let data = result.data.as_ref().expect("the observed state");
    assert_eq!(data.commit.as_deref(), Some(created.as_str()));
}

#[test]
fn a_writer_change_after_staging_is_reported_not_saved() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    initialize(&repo);
    repo.write_file("notes/a.md", b"# One revised\n");
    // The pre-commit hook runs after staging: the writer changes the file
    // and creates a new scoped path while the checkpoint commits the staged
    // bytes.
    install_hook(
        &repo,
        "pre-commit",
        "#!/bin/sh\nprintf 'newer bytes' >> notes/a.md\nprintf '# Late\\n' > notes/late.md\nexit 0\n",
    );

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.create_checkpoint(&checkpoint_of(
        &scope_input(&["notes"], &[], &[".md"]),
        None,
    ));

    assert!(
        result.ok(),
        "the captured commit stands: {:?}",
        result.error
    );
    let data = result.data.as_ref().unwrap();
    let commit = data.commit.as_deref().unwrap();
    // The committed bytes are the staged ones; the newer bytes and the late
    // file stayed on disk, and the response warns instead of calling them
    // saved (SPEC 9.2).
    assert_eq!(
        oracle::blob(
            &repo,
            &text(&repo.git_ok(["rev-parse", &format!("{commit}:notes/a.md")]))
        ),
        b"# One revised\n"
    );
    assert_eq!(repo.read_file("notes/a.md"), b"# One revised\nnewer bytes");
    assert_eq!(
        repo.read_file("notes/late.md"),
        b"# Late\n",
        "the late file exists on disk"
    );
    assert_eq!(result.warnings.len(), 1, "one newer-content warning");
    let warning = &result.warnings[0];
    assert_eq!(warning.code, "DIRTY_SCOPE");
    assert!(
        warning.message.contains("notes/a.md") && warning.message.contains("notes/late.md"),
        "the warning names both unsaved paths: {}",
        warning.message
    );
    let status = operations.inspect_workspace(&InspectRequest {
        scope: None,
        path_filter: Vec::new(),
    });
    assert!(status.ok());
    assert_eq!(
        status.data.as_ref().unwrap().preservation.as_str(),
        "uncaptured",
        "the newer bytes are honestly not saved"
    );
}

#[test]
fn unborn_and_uninitialized_workspaces_refuse_without_writing() {
    // An unborn branch supplies no candidate, and only a start marker may
    // be a root commit (SPEC 8.4 rule 3).
    let unborn = TestRepo::init();
    unborn.write_file("notes/a.md", b"# One\n");
    let git = adapter_for(&unborn);
    let operations = Operations::new(&git);
    let result = operations.create_checkpoint(&checkpoint_of(
        &scope_input(&["notes"], &[], &[".md"]),
        None,
    ));
    assert_refusal(&result, "BASELINE_UNAVAILABLE", "an unborn checkpoint");

    // A repository without a stored scope is not initialized.
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.create_checkpoint(&CheckpointRequest {
        scope: None,
        message: None,
        include_staged: false,
        dry_run: false,
    });
    assert_refusal(&result, "NOT_INITIALIZED", "an unconfigured checkpoint");

    // The guarded recovery capture of SPEC 9.7 arrives with task T036.
    initialize(&repo);
    let recovery = operations.create_checkpoint(&CheckpointRequest {
        scope: None,
        message: None,
        include_staged: true,
        dry_run: false,
    });
    assert_refusal(
        &recovery,
        "OPERATION_UNAVAILABLE",
        "the staged-content recovery capture",
    );
}

#[test]
fn checkpoints_work_in_sha256_repositories() {
    let repo = TestRepo::init_with(RepoOptions::new().with_object_format(ObjectFormat::Sha256));
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    initialize(&repo);
    repo.write_file("notes/a.md", b"# One revised\n");

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.create_checkpoint(&checkpoint_of(
        &scope_input(&["notes"], &[], &[".md"]),
        None,
    ));

    assert!(
        result.ok(),
        "a sha256 capture must succeed: {:?}",
        result.error
    );
    let data = result.data.as_ref().unwrap();
    let commit = data.commit.as_deref().unwrap();
    let marker = validate_marker(&git, commit)
        .unwrap_or_else(|error| panic!("the sha256 draft must validate: {error}"));
    assert_eq!(marker.kind, CommitKind::Draft);
    let trailers = trailers_of(&repo, commit);
    assert!(
        trailers
            .iter()
            .any(|line| line.starts_with("Margin-Scope: sha256:")),
        "the scope digest names the object format"
    );
    assert_eq!(
        oracle::blob(
            &repo,
            &text(&repo.git_ok(["rev-parse", &format!("{commit}:notes/a.md")]))
        ),
        b"# One revised\n"
    );
}

#[test]
fn the_cli_checkpoint_reports_one_envelope() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    initialize(&repo);
    repo.write_file("notes/a.md", b"# One revised\n");

    // One machine-mode capture: `created`, the commit, and the effects.
    let output = margin(
        repo.env(),
        repo.path(),
        &["checkpoint", "--message", "shorten", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));
    let envelope: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one valid envelope");
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["command"], "checkpoint");
    assert_eq!(envelope["data"]["created"], true);
    assert_eq!(envelope["data"]["changed"], true);
    assert_eq!(envelope["data"]["dry_run"], false);
    assert_eq!(envelope["effects"]["commit"], "created");
    assert_eq!(
        envelope["effects"]["commit_oid"],
        oracle::head_oid(&repo).expect("the created commit")
    );
    assert_eq!(envelope["effects"]["index"], "changed");
    assert_eq!(envelope["effects"]["working_tree"], "unchanged");

    // The repeated call is the documented no-op of unchanged content.
    let repeat = margin(repo.env(), repo.path(), &["checkpoint", "--json"]);
    assert_eq!(repeat.status.code(), Some(0));
    let envelope: serde_json::Value =
        serde_json::from_slice(&repeat.stdout).expect("one valid envelope");
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["data"]["created"], false);
    assert_eq!(envelope["data"]["changed"], false);
    assert_eq!(envelope["data"]["commit"], serde_json::Value::Null);
    assert_eq!(envelope["effects"]["commit"], "none");
}

#[test]
fn the_cli_checkpoint_on_another_branch_reports_the_refusal() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    initialize(&repo);
    repo.git_ok(["switch", "--quiet", "--create", "other"]);
    repo.write_file("notes/a.md", b"# One revised\n");
    let before = oracle::snapshot(&repo);

    // One machine-mode refusal of the stale-context exit class: exit 3,
    // `BRANCH_CHANGED`, and the unchanged effects of a command that wrote
    // nothing (SPEC 7.5).
    let output = margin(
        repo.env(),
        repo.path(),
        &["checkpoint", "--message", "draft wording", "--json"],
    );
    assert_eq!(output.status.code(), Some(3));
    let envelope: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one valid envelope");
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["command"], "checkpoint");
    assert_eq!(envelope["error"]["code"], "BRANCH_CHANGED");
    assert_eq!(
        envelope["error"]["recovery_argv"],
        serde_json::json!(["margin", "init", "--json"])
    );
    for (field, expected) in [
        ("working_tree", "unchanged"),
        ("index", "unchanged"),
        ("commit", "none"),
        ("configuration", "unchanged"),
        ("repository", "unchanged"),
    ] {
        assert_eq!(envelope["effects"][field], expected, "effects {field}");
    }
    assert!(
        output.stderr.is_empty(),
        "machine mode writes no stderr, got: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    before.assert_unchanged(&oracle::snapshot(&repo), "the refused CLI checkpoint");
    assert_eq!(
        raw_config(&repo, "margin.branch").as_deref(),
        Some("main"),
        "the stored adoption survives the refusal"
    );
}

#[test]
fn a_staging_failure_reports_the_inspected_state() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let start = initialize(&repo);
    repo.write_file("notes/a.md", b"# One revised\n");
    // One stale index lock lets every guard pass but breaks the first real
    // staging write.
    let lock = repo.path().join(".git").join("index.lock");
    fs::write(&lock, b"").unwrap();

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.create_checkpoint(&checkpoint_of(
        &scope_input(&["notes"], &[], &[".md"]),
        None,
    ));

    // The staging failed, so the reported effects come from inspecting the
    // repository: nothing was staged, no commit exists, and the working
    // files are untouched (SPEC 9.1 step 7).
    assert!(!result.ok(), "a broken staging must fail the capture");
    assert_eq!(result.error.as_ref().unwrap().code, "GIT_FAILED");
    assert!(
        result
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("index.lock"),
        "the failure names the index lock: {:?}",
        result.error
    );
    assert_eq!(result.effects.index, ObservedEffect::Unchanged);
    assert_eq!(result.effects.commit, CommitEffect::None);
    assert_eq!(result.effects.commit_oid, None);
    assert_eq!(result.effects.working_tree, ObservedEffect::Unchanged);
    assert_eq!(oracle::head_oid(&repo).as_deref(), Some(start.as_str()));
    fs::remove_file(&lock).unwrap();
    assert_eq!(
        text(&repo.git_ok(["diff-index", "--cached", "--name-only", "HEAD"])),
        "",
        "the failed staging wrote no index entry"
    );

    // With the lock gone, the same checkpoint captures the edit.
    let retry = operations.create_checkpoint(&checkpoint_of(
        &scope_input(&["notes"], &[], &[".md"]),
        None,
    ));
    assert!(retry.ok(), "the retry must succeed: {:?}", retry.error);
    assert_eq!(retry.effects.commit, CommitEffect::Created);
}

#[test]
fn a_hook_that_edits_working_files_before_rejecting_is_reported() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let start = initialize(&repo);
    repo.write_file("notes/a.md", b"# One revised\n");
    // The pre-commit hook runs after staging: it edits the working file and
    // then rejects the commit.
    install_hook(
        &repo,
        "pre-commit",
        "#!/bin/sh\nprintf 'hook bytes\\n' >> notes/a.md\nexit 1\n",
    );

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.create_checkpoint(&checkpoint_of(
        &scope_input(&["notes"], &[], &[".md"]),
        None,
    ));

    // The working-file change the hook made is observed and reported, the
    // staged content stays, and no commit exists (SPEC 7.1, 14.1, 9.2).
    assert!(!result.ok(), "a rejecting hook must fail the checkpoint");
    assert_eq!(result.error.as_ref().unwrap().code, "GIT_FAILED");
    assert_eq!(result.effects.working_tree, ObservedEffect::Changed);
    assert_eq!(result.effects.index, ObservedEffect::Changed);
    assert_eq!(result.effects.commit, CommitEffect::None);
    assert_eq!(result.effects.commit_oid, None);
    let data = result.data.as_ref().expect("the observed state");
    assert_eq!(data.commit, None);
    assert_eq!(data.candidate, start);
    assert_eq!(repo.read_file("notes/a.md"), b"# One revised\nhook bytes\n");
    assert_eq!(
        oracle::blob(&repo, &text(&repo.git_ok(["rev-parse", ":notes/a.md"]))),
        b"# One revised\n",
        "the staged bytes are the content the hook read before its edit"
    );
    assert_eq!(oracle::head_oid(&repo).as_deref(), Some(start.as_str()));
}

#[test]
fn a_killed_commit_process_keeps_the_outcome_unknown() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let start = initialize(&repo);
    repo.write_file("notes/a.md", b"# One revised\n");
    install_hook(&repo, "pre-commit", "#!/bin/sh\nsleep 10\nexit 0\n");

    // One tight time budget kills `git commit` while its hook sleeps.
    let git = Git::from_env(repo.env().vars().clone())
        .at(repo.path())
        .with_limits(Limits {
            timeout: Duration::from_millis(600),
            max_output_bytes: 16 * 1024 * 1024,
        });
    let operations = Operations::new(&git);
    let result = operations.create_checkpoint(&checkpoint_of(
        &scope_input(&["notes"], &[], &[".md"]),
        None,
    ));

    // A killed mutation process loses its outcome: the commit effect stays
    // unknown even though the head, the index, and the working files were
    // inspected afterwards (SPEC 7.1, 9.6).
    assert!(!result.ok(), "a killed commit must fail the checkpoint");
    assert_eq!(result.error.as_ref().unwrap().code, "OUTCOME_UNKNOWN");
    assert_eq!(result.effects.commit, CommitEffect::Unknown);
    assert_eq!(result.effects.commit_oid, None);
    assert_eq!(result.effects.index, ObservedEffect::Changed);
    assert_eq!(result.effects.working_tree, ObservedEffect::Unchanged);
    let data = result.data.as_ref().expect("the observed state");
    assert_eq!(data.commit, None, "an unknown outcome is not a commit");
    assert_eq!(oracle::head_oid(&repo).as_deref(), Some(start.as_str()));

    // The staged content stayed behind, so the next checkpoint stops on it.
    assert_eq!(
        text(&repo.git_ok(["diff-index", "--cached", "--name-only", "HEAD"])),
        "notes/a.md",
        "the capture stayed staged"
    );
    let retry = Operations::new(&adapter_for(&repo)).create_checkpoint(&checkpoint_of(
        &scope_input(&["notes"], &[], &[".md"]),
        None,
    ));
    assert_refusal(&retry, "STAGED_CHANGES", "a checkpoint over staged state");
}

#[test]
fn an_output_limit_failure_after_the_commit_reports_it() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    initialize(&repo);
    repo.write_file("notes/a.md", b"# One revised\n");
    // The commit-msg hook leaves the message alone but floods stderr, so
    // `git commit` creates the commit and still exceeds the capture limit.
    install_hook(
        &repo,
        "commit-msg",
        "#!/bin/sh\nhead -c 262144 /dev/zero >&2\nexit 0\n",
    );

    let git = Git::from_env(repo.env().vars().clone())
        .at(repo.path())
        .with_limits(Limits {
            timeout: Duration::from_millis(10_000),
            max_output_bytes: 64 * 1024,
        });
    let operations = Operations::new(&git);
    let result = operations.create_checkpoint(&checkpoint_of(
        &scope_input(&["notes"], &[], &[".md"]),
        None,
    ));

    // The commit exists even though its process result was lost to the
    // output limit: the inspected head names it with its object ID (SPEC
    // 9.6, 14.1).
    assert!(!result.ok(), "the flooded capture must fail");
    assert_eq!(result.error.as_ref().unwrap().code, "LIMIT_EXCEEDED");
    let created = oracle::head_oid(&repo).expect("the commit itself succeeded");
    assert_eq!(result.effects.commit, CommitEffect::Created);
    assert_eq!(result.effects.commit_oid.as_deref(), Some(created.as_str()));
    let data = result.data.as_ref().expect("the observed state");
    assert_eq!(data.commit.as_deref(), Some(created.as_str()));
    assert_eq!(result.effects.index, ObservedEffect::Changed);
    assert_eq!(result.effects.working_tree, ObservedEffect::Unchanged);
    let marker = validate_marker(&git, &created)
        .unwrap_or_else(|error| panic!("the draft validates: {error}"));
    assert_eq!(marker.kind, CommitKind::Draft);
    assert_eq!(
        oracle::blob(
            &repo,
            &text(&repo.git_ok(["rev-parse", &format!("{created}:notes/a.md")]))
        ),
        b"# One revised\n"
    );
}

// ---------------------------------------------------------------------------
// The shared mutation lock (SPEC 9.1 step 2)
// ---------------------------------------------------------------------------

#[test]
fn a_held_lock_refuses_the_checkpoint_as_busy() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let start = initialize(&repo);
    repo.write_file("notes/a.md", b"# One revised\n");

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    // One competing Margin operation holds the operating-system lock of
    // this Git directory, taken through stock Git's own lock path.
    assert_eq!(
        lock_path(&repo),
        repo.path().join(".git").join("margin.lock"),
        "the lock file lives in the Git directory (SPEC 8.2)"
    );
    let held = hold_mutation_lock(&repo);
    let before = oracle::snapshot(&repo);
    let result = operations.create_checkpoint(&checkpoint_of(
        &scope_input(&["notes"], &[], &[".md"]),
        None,
    ));

    // The refusal carries the retryable `BUSY` of SPEC 7.5 and the
    // unchanged effects of an operation that wrote nothing.
    assert!(!result.ok(), "a held lock must refuse the capture");
    let error = result.error.as_ref().unwrap();
    assert_eq!(error.code, "BUSY");
    assert!(
        error.retryable,
        "an identical retry is safe once the owner finishes"
    );
    assert_eq!(result.effects, Effects::unchanged());
    assert!(
        result.data.is_none(),
        "a refused operation reports no outcome data"
    );
    before.assert_unchanged(&oracle::snapshot(&repo), "the refused checkpoint");
    assert_eq!(
        text(&repo.git_ok(["diff-index", "--cached", "--name-only", "HEAD"])),
        "",
        "nothing was staged"
    );

    // The lock releases with the competing operation, so the identical
    // capture then succeeds.
    drop(held);
    let retry = operations.create_checkpoint(&checkpoint_of(
        &scope_input(&["notes"], &[], &[".md"]),
        None,
    ));
    assert!(retry.ok(), "the retry must succeed: {:?}", retry.error);
    let commit = retry.data.as_ref().unwrap().commit.clone().unwrap();
    assert_eq!(oracle::head_oid(&repo).as_deref(), Some(commit.as_str()));
    assert_ne!(commit, start);
}

#[test]
fn the_cli_checkpoint_waits_for_a_held_mutation_lock() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    initialize(&repo);
    repo.write_file("notes/a.md", b"# One revised\n");

    let held = hold_mutation_lock(&repo);
    let before = oracle::snapshot(&repo);
    let output = margin(
        repo.env(),
        repo.path(),
        &["checkpoint", "--message", "draft wording", "--json"],
    );

    // One machine-mode refusal of the precondition exit class: exit 4,
    // `BUSY`, one retryable error, and the unchanged effects of a command
    // that wrote nothing (SPEC 7.5, 9.1 step 2).
    assert_eq!(output.status.code(), Some(4));
    let envelope: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one valid envelope");
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["command"], "checkpoint");
    assert_eq!(envelope["error"]["code"], "BUSY");
    assert_eq!(envelope["error"]["retryable"], true);
    for (field, expected) in [
        ("working_tree", "unchanged"),
        ("index", "unchanged"),
        ("commit", "none"),
        ("configuration", "unchanged"),
        ("repository", "unchanged"),
    ] {
        assert_eq!(envelope["effects"][field], expected, "effects {field}");
    }
    assert!(
        output.stderr.is_empty(),
        "machine mode writes no stderr, got: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    before.assert_unchanged(&oracle::snapshot(&repo), "the refused CLI checkpoint");
    assert_eq!(
        raw_config(&repo, "margin.branch").as_deref(),
        Some("main"),
        "the adoption settings survive the refusal"
    );

    // Queries hold no lock: `status` answers while the mutation lock is
    // held elsewhere (SPEC 9.1 step 2, INV-12).
    let status = margin(repo.env(), repo.path(), &["status", "--json"]);
    assert_eq!(status.status.code(), Some(0));

    // The competing operation ends; the same command line then captures.
    drop(held);
    let retry = margin(
        repo.env(),
        repo.path(),
        &["checkpoint", "--message", "draft wording", "--json"],
    );
    assert_eq!(retry.status.code(), Some(0));
    let retry_envelope: serde_json::Value =
        serde_json::from_slice(&retry.stdout).expect("one valid envelope");
    assert_eq!(retry_envelope["ok"], true);
    assert_eq!(retry_envelope["effects"]["commit"], "created");
}

#[test]
fn a_refusal_releases_the_lock_for_the_next_operation() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    initialize(&repo);
    repo.write_file("notes/a.md", b"# One revised\n");

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    // One refusal that writes nothing, then one observed failure that
    // leaves staged content behind: both return paths must release the
    // lock, because the following operation runs in this same process and
    // would contend with any guard left open (SPEC 9.1 step 8).
    let refused = operations.create_checkpoint(&checkpoint_of(
        &scope_input(&["notes"], &[], &[".md"]),
        Some("text\nMargin-Kind: review"),
    ));
    assert_refusal(&refused, "INVALID_ARGUMENT", "an injecting message");

    let lock = repo.path().join(".git").join("index.lock");
    fs::write(&lock, b"").unwrap();
    let failed = operations.create_checkpoint(&checkpoint_of(
        &scope_input(&["notes"], &[], &[".md"]),
        None,
    ));
    assert_eq!(failed.error.as_ref().unwrap().code, "GIT_FAILED");
    fs::remove_file(&lock).unwrap();

    let captured = operations.create_checkpoint(&checkpoint_of(
        &scope_input(&["notes"], &[], &[".md"]),
        None,
    ));
    assert!(
        captured.ok(),
        "the next operation must not be BUSY: {:?}",
        captured.error
    );
}

// ---------------------------------------------------------------------------
// The precondition recheck (SPEC 9.1 step 6)
// ---------------------------------------------------------------------------

/// Runs one checkpoint whose concurrent writer — injected at the pass
/// `pass` of the capture — performs `action`, and returns the result.
fn checkpoint_with_seam_writer(
    repo: &TestRepo,
    pass: SeamPass,
    action: &str,
) -> margin::app::OperationOutcome<margin::app::CheckpointOutcome> {
    let git = seam_adapter(repo, pass, action);
    let operations = Operations::new(&git);
    operations.create_checkpoint(&checkpoint_of(
        &scope_input(&["notes"], &[], &[".md"]),
        Some("draft wording"),
    ))
}

#[test]
fn a_head_that_moves_before_staging_refuses_without_writing() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let start = initialize(&repo);
    repo.write_file("notes/a.md", b"# One revised\n");

    let result = checkpoint_with_seam_writer(
        &repo,
        SeamPass::FirstDisposable,
        "git commit --quiet --allow-empty --message concurrent-writer",
    );

    // The prepared candidate is no longer HEAD, so the capture refuses
    // before staging; the concurrent commit stands untouched (SPEC 9.1
    // step 6, 16.5 stale state).
    assert!(!result.ok(), "a moved HEAD must refuse the capture");
    let error = result.error.as_ref().unwrap();
    assert_eq!(error.code, "STALE_STATE");
    assert!(
        error.message.contains("nothing was staged"),
        "the refusal names its own pass: {}",
        error.message
    );
    assert_eq!(result.effects, Effects::unchanged());
    let concurrent = oracle::head_oid(&repo).expect("the concurrent commit stands");
    assert_ne!(concurrent, start);
    assert!(
        result.error.as_ref().unwrap().message.contains(&concurrent),
        "the refusal names the observed HEAD: {}",
        error.message
    );
    assert_eq!(
        text(&repo.git_ok(["diff-index", "--cached", "--name-only", "HEAD"])),
        "",
        "nothing was staged over the concurrent commit"
    );
    assert_eq!(
        repo.read_file("notes/a.md"),
        b"# One revised\n",
        "the writer's edit stays unstaged"
    );
}

#[test]
fn a_branch_that_switches_before_staging_refuses_without_writing() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let start = initialize(&repo);
    repo.write_file("notes/a.md", b"# One revised\n");

    let result = checkpoint_with_seam_writer(
        &repo,
        SeamPass::FirstDisposable,
        "git switch --quiet --create other-branch",
    );

    // The draft would land on a branch Margin never adopted, so the
    // recheck refuses `BRANCH_CHANGED` before staging (SPEC 8.2, INV-08).
    assert!(!result.ok(), "a switched branch must refuse the capture");
    let error = result.error.as_ref().unwrap();
    assert_eq!(error.code, "BRANCH_CHANGED");
    assert_eq!(result.effects, Effects::unchanged());
    assert_eq!(error.recovery_argv, vec!["margin", "init", "--json"]);
    assert_eq!(
        text(&repo.git_ok(["symbolic-ref", "--short", "HEAD"])),
        "other-branch",
        "the writer's switch stands"
    );
    assert_eq!(
        text(&repo.git_ok(["diff-index", "--cached", "--name-only", "HEAD"])),
        "",
        "nothing was staged"
    );
    assert_eq!(
        oracle::head_oid(&repo).as_deref(),
        Some(start.as_str()),
        "the branch moved but no commit appeared"
    );
    assert_eq!(
        repo.read_file("notes/a.md"),
        b"# One revised\n",
        "the writer's edit stays unstaged"
    );
}

#[test]
fn an_index_that_changes_before_staging_refuses_without_writing() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.write_file("other.txt", b"out of scope\n");
    repo.commit_all("import");
    let start = initialize(&repo);
    repo.write_file("notes/a.md", b"# One revised\n");
    // One out-of-scope edit the concurrent writer stages mid-capture; an
    // edit that matches the candidate would stage nothing at all.
    repo.write_file("other.txt", b"out of scope, edited\n");

    let result =
        checkpoint_with_seam_writer(&repo, SeamPass::FirstDisposable, "git add -- other.txt");

    // A staged entry that arrived while the capture was prepared refuses
    // before staging (SPEC 9.1 step 6, 9.2): Margin never consumes a
    // pre-existing staged selection.
    assert!(!result.ok(), "a staged entry must refuse the capture");
    let error = result.error.as_ref().unwrap();
    assert_eq!(error.code, "STAGED_CHANGES");
    assert_eq!(result.effects, Effects::unchanged());
    assert_eq!(
        text(&repo.git_ok(["diff-index", "--cached", "--name-only", "HEAD"])),
        "other.txt",
        "the writer's staged entry stands, and nothing else was staged"
    );
    assert_eq!(oracle::head_oid(&repo).as_deref(), Some(start.as_str()));
}

#[test]
fn scoped_bytes_that_change_before_staging_refuse_without_writing() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let start = initialize(&repo);
    repo.write_file("notes/a.md", b"# One revised\n");

    // The writer edits after the intended-tree proof closed and before the
    // precondition recheck, so the proof itself stays consistent and the
    // recheck alone sees the newer bytes.
    let result = checkpoint_with_seam_writer(
        &repo,
        SeamPass::ProofTail,
        "printf ' writer bytes' >> notes/a.md",
    );

    // The enumerated capture no longer matches the scoped working content,
    // so the capture refuses before staging and never calls the writer's
    // newer bytes saved (SPEC 9.1 step 6, 9.2).
    assert!(!result.ok(), "moved scoped bytes must refuse the capture");
    let error = result.error.as_ref().unwrap();
    assert_eq!(error.code, "DIRTY_SCOPE");
    assert!(
        error.message.contains("notes/a.md"),
        "the refusal names the moved path: {}",
        error.message
    );
    assert!(
        error.message.contains("nothing was staged"),
        "the refusal names its own pass: {}",
        error.message
    );
    assert_eq!(result.effects, Effects::unchanged());
    assert_eq!(
        repo.read_file("notes/a.md"),
        b"# One revised\n writer bytes",
        "the writer's change stays in the working tree"
    );
    assert_eq!(
        text(&repo.git_ok(["diff-index", "--cached", "--name-only", "HEAD"])),
        "",
        "nothing was staged"
    );
    assert_eq!(oracle::head_oid(&repo).as_deref(), Some(start.as_str()));
}

#[test]
fn a_path_kind_that_changes_before_staging_refuses_without_writing() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let start = initialize(&repo);
    repo.write_file("notes/a.md", b"# One revised\n");

    let result = checkpoint_with_seam_writer(
        &repo,
        SeamPass::FirstDisposable,
        "rm notes/a.md && ln -s /dev/null notes/a.md",
    );

    // A scoped path that became a symbolic link fails the path-safety
    // rules of the one resolver, and the capture refuses before staging
    // (SPEC 14.3, 3.2 rule 5).
    assert!(!result.ok(), "a changed path kind must refuse the capture");
    let error = result.error.as_ref().unwrap();
    assert_eq!(error.code, "UNSUPPORTED_REPOSITORY");
    assert!(
        error.message.contains("notes/a.md"),
        "the refusal names the blocked path: {}",
        error.message
    );
    assert_eq!(result.effects, Effects::unchanged());
    assert_eq!(
        text(&repo.git_ok(["diff-index", "--cached", "--name-only", "HEAD"])),
        "",
        "nothing was staged"
    );
    assert_eq!(oracle::head_oid(&repo).as_deref(), Some(start.as_str()));
}
