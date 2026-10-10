//! Real-Git tests for scope initialization (SPEC 5.1, 6.4, 8.2, tasks T017
//! and T018).
//!
//! Every case proves one contract of `init` against the installed Git:
//!
//! - an existing repository is adopted on its current branch, and the
//!   eligible scoped working content, untracked files and deletions
//!   included, is captured in one valid `start` marker that changes no
//!   out-of-scope entry and may be empty when the tree is already saved
//! - an unborn branch produces exactly one zero-parent `start` marker with
//!   `Margin-Candidate: unborn` whose tree holds only eligible scoped
//!   entries
//! - the local settings persist only after the commit verifies, so a
//!   configuration failure reports the partial success with its commit, and
//!   the retry adopts that validated marker without committing a second one
//! - repeating initialization with the same anchored scope changes only the
//!   supplied settings and adds no start commit; a changed scope, a missing
//!   anchor, or `--new-start` creates a fresh anchor, and scope A → B → A
//!   never revives the older approval
//! - `--create-repo` creates the repository through ordinary `git init`,
//!   names the initial branch, never creates an identity, and its dry run
//!   proposes the scope and configuration without creating `.git`
//! - a dry run over an existing repository reports the intended tree and
//!   the planned settings without changing refs, index, or working bytes
//! - staged changes and a wrong expected branch refuse without writing
//! - an explicit init on another branch adopts it: the validated anchor
//!   stands, only the stored branch changes, and no second start appears
//! - a capture failure reports the observed repository state: a hook that
//!   appends a second reserved trailer leaves a created commit that the
//!   response names with its object ID while withholding every adoption
//!   setting; a rejecting hook, a hook that edits working files, and a
//!   staging failure report the inspected index, working tree, and head
//!   instead of assuming them
//! - the shared mutation lock of SPEC 9.1 step 2 serializes first use,
//!   repeat settings, and `--create-repo` adoption of an existing
//!   repository: one held lock refuses `BUSY` without intended effects,
//!   and the retry after release completes. Creation itself is the
//!   documented bootstrap boundary — no Git directory exists to lock until
//!   `git init` returns.
//! - the intended starting tree is proved in one disposable index before
//!   the real index is staged, and the real staged tree must equal it
//!   before the commit runs: a writer whose bytes land between the proof
//!   and the staging leaves no start marker, no settings, and the staged
//!   divergence reported honestly. The precondition recheck refuses before
//!   staging when the scoped bytes or `HEAD` moved while the capture was
//!   prepared.

mod support;

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

use margin::app::{
    BaselineError, BaselineStatus, BaselineUnavailable, CommitEffect, Effects, Identities,
    InitRequest, InspectRequest, ObservedEffect, OperationOutcome, Operations, RepositoryEffect,
    TREE_REASON_ALREADY_ANCHORED, TREE_REASON_REPOSITORY_NOT_CREATED, commit_marker, resolve_scope,
    select_baseline, validate_marker,
};
use margin::domain::{CommitKind, CommitMetadata, DecisionMode, PlanIdentity, ScopeInput};
use margin::git::Git;
use support::env::{
    DEFAULT_BRANCH, FIXTURE_USER_EMAIL, FIXTURE_USER_NAME, IsolatedEnv, is_scrubbed,
};
use support::mutation::{SeamPass, hold_mutation_lock, seam_adapter};
use support::repo::{RepoOptions, TestRepo};
use support::{oracle, text};

/// Adapter bound to `repo`'s working tree and isolated environment.
fn adapter_for(repo: &TestRepo) -> Git {
    Git::from_env(repo.env().vars().clone()).at(repo.path())
}

/// Adapter bound to one fresh folder and its isolated environment.
fn adapter_for_folder(folder: &FreshFolder) -> Git {
    Git::from_env(folder.env().vars().clone()).at(folder.folder())
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

/// Resolves the writing scope rooted at one include root.
fn resolved_scope(repo: &TestRepo, include: &str) -> margin::app::ResolvedScope {
    resolve_scope(&adapter_for(repo), &scope_input(&[include], &[], &[".md"]))
        .unwrap_or_else(|error| panic!("the scope must resolve: {error}"))
}

/// Commits one valid keep-all review marker for `scope` on the current
/// index, recording `baseline`, through the shared marker writer.
fn review_marker(
    repo: &TestRepo,
    scope: &margin::app::ResolvedScope,
    label: &str,
    baseline: &str,
) -> String {
    let git = adapter_for(repo);
    let parent = text(&repo.git_ok(["rev-parse", "HEAD"]));
    let tree = text(&repo.git_ok(["write-tree"]));
    let request = format!("review-{}", label.replace(' ', "-"));
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
            message: label.to_string(),
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
        label,
    )
    .unwrap_or_else(|error| panic!("the review marker must verify: {error}"));
    marker.commit
}

/// Selects the baseline for `scope` at `candidate` and returns its commit.
fn anchored_at(repo: &TestRepo, candidate: &str, scope: &margin::app::ResolvedScope) -> String {
    select_baseline(&adapter_for(repo), candidate, &scope.digest)
        .unwrap_or_else(|error| panic!("the baseline must select: {error}"))
        .commit
}

/// The commit `HEAD` points at.
fn head(repo: &TestRepo) -> String {
    oracle::head_oid(repo).expect("a born branch")
}

/// Writes one executable Git hook.
fn install_hook(repo: &TestRepo, name: &str, script: &str) {
    let path = repo.path().join(".git/hooks").join(name);
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// The keys of the settings one operation applied, in write order.
fn applied_keys(result: &OperationOutcome<margin::app::InitializationOutcome>) -> Vec<String> {
    result
        .data
        .as_ref()
        .expect("a completed initialization")
        .settings
        .iter()
        .map(|setting| setting.key.clone())
        .collect()
}

/// Asserts one typed refusal: not successful, one primary code, and the
/// unchanged effects of an operation that wrote nothing.
fn assert_refusal<T>(result: &OperationOutcome<T>, code: &str, what: &str) {
    assert!(!result.ok(), "{what} must not succeed");
    let error = result.error.as_ref().unwrap();
    assert_eq!(error.code, code, "{what} reports the wrong code");
    assert_eq!(result.effects, Effects::unchanged(), "{what} wrote nothing");
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

/// The raw bytes of one commit object.
fn commit_bytes(repo: &TestRepo, commit: &str) -> Vec<u8> {
    repo.git_ok(["cat-file", "commit", commit])
}

/// One fresh folder outside any repository, with its own isolated home.
///
/// `margin init --create-repo` runs here, so the folder becomes a repository
/// only when the operation itself creates one. The test-only global
/// configuration carries the fixture identity unless the case needs it
/// missing (SPEC 5.1).
struct FreshFolder {
    #[allow(dead_code)]
    base: TempDir,
    folder: PathBuf,
    env: IsolatedEnv,
}

/// Configuration written to the test-only global file for every folder.
const FOLDER_GLOBAL_CONFIG: &[(&str, &str)] = &[
    ("init.defaultBranch", DEFAULT_BRANCH),
    ("commit.gpgsign", "false"),
    ("core.autocrlf", "false"),
    ("gc.auto", "0"),
];

impl FreshFolder {
    /// One folder whose global configuration carries the fixture identity.
    fn with_identity() -> Self {
        Self::new(true)
    }

    /// One folder whose global configuration holds no Git identity.
    fn without_identity() -> Self {
        Self::new(false)
    }

    fn new(identity: bool) -> Self {
        let base = TempDir::new()
            .unwrap_or_else(|error| panic!("cannot create a temporary root: {error}"));
        let home = base.path().join("home");
        fs::create_dir_all(&home)
            .unwrap_or_else(|error| panic!("cannot create {}: {error}", home.display()));
        let env = IsolatedEnv::new(&home);
        let mut entries: Vec<(&str, &str)> = FOLDER_GLOBAL_CONFIG.to_vec();
        if identity {
            entries.extend([
                ("user.name", FIXTURE_USER_NAME),
                ("user.email", FIXTURE_USER_EMAIL),
            ]);
        }
        for (key, value) in entries {
            let mut command = Command::new("git");
            command
                .args(["config", "--file"])
                .arg(env.global_config())
                .arg(key)
                .arg(value);
            scrub(&mut command);
            let output = command
                .output()
                .unwrap_or_else(|error| panic!("cannot write the test configuration: {error}"));
            assert!(
                output.status.success(),
                "cannot set {key} in the test configuration: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let folder = base.path().join("folder");
        fs::create_dir_all(&folder)
            .unwrap_or_else(|error| panic!("cannot create {}: {error}", folder.display()));
        Self { base, folder, env }
    }

    /// The folder `margin init --create-repo` would run in.
    fn folder(&self) -> &Path {
        &self.folder
    }

    /// The isolated environment of the folder.
    fn env(&self) -> &IsolatedEnv {
        &self.env
    }

    /// Writes raw bytes to a folder-relative file and returns its path.
    fn write_file(&self, relative: &str, bytes: &[u8]) -> PathBuf {
        let path = self.folder.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("cannot create {}: {error}", parent.display()));
        }
        fs::write(&path, bytes).unwrap_or_else(|error| panic!("cannot write the file: {error}"));
        path
    }

    /// Runs stock Git in the folder and returns the raw process output.
    fn git<I, S>(&self, args: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new("git");
        command.args(args).current_dir(&self.folder);
        scrub(&mut command);
        command
            .envs(self.env.vars())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap_or_else(|error| panic!("cannot run git: {error}"))
    }

    /// Runs stock Git in the folder, asserts a zero exit, and returns stdout.
    fn git_ok<I, S>(&self, args: I) -> Vec<u8>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.git(args);
        assert!(
            output.status.success(),
            "git failed in {}: {}",
            self.folder.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }
}

/// Reads one local configuration value of a folder repository.
fn raw_folder_config(folder: &FreshFolder, key: &str) -> Option<String> {
    let output = folder.git(["config", "--local", "--get", key]);
    if output.status.success() {
        Some(text(&output.stdout))
    } else {
        None
    }
}

/// Lists the paths of one tree-ish in a folder repository.
fn folder_tree_paths(folder: &FreshFolder, treeish: &str) -> Vec<String> {
    let stdout = folder.git_ok(["ls-tree", "-r", "-z", treeish]);
    stdout
        .split(|&byte| byte == 0)
        .filter(|record| !record.is_empty())
        .map(|record| {
            let separator = record
                .iter()
                .position(|&byte| byte == b'\t')
                .expect("one ls-tree record");
            String::from_utf8_lossy(&record[separator + 1..]).into_owned()
        })
        .collect()
}

/// Removes every inherited variable the harness scrubs from child processes.
fn scrub(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        if is_scrubbed(&key) {
            command.env_remove(&key);
        }
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
    scrub(&mut command);
    command.envs(env.vars());
    command
        .output()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"))
}

/// Decodes one JSON envelope from the executable.
fn envelope_of(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).expect("one valid JSON envelope")
}

#[test]
fn first_use_captures_scoped_content_in_a_valid_start_commit() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.write_file("other.txt", b"out of scope\n");
    repo.commit_all("import");
    repo.write_file("notes/gone.md", b"# Gone\n");
    repo.commit_all("add gone");
    // Working edits, an eligible untracked file, and an unstaged deletion
    // are all part of the captured starting point (SPEC 5.1, 3.2).
    repo.write_file("notes/a.md", b"# One revised\n");
    repo.write_file("notes/draft.md", b"# Draft\n");
    fs::remove_file(repo.file_path("notes/gone.md")).unwrap();
    let parent = oracle::head_oid(&repo).unwrap();

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let scope = scope_input(&["notes"], &[], &[".md"]);
    let before = oracle::snapshot(&repo);
    let result = operations.initialize_scope(&first_use(&scope));

    assert!(result.ok(), "initialization must succeed");
    let data = result.data.as_ref().unwrap();
    let start = data.start_commit.as_deref().expect("a start commit");
    assert!(!data.dry_run);
    assert!(!data.repository_created);
    assert_eq!(data.tree_reason, None);

    // The marker is one structurally valid start commit whose parent is the
    // prior candidate and whose tree is the intended result tree.
    let marker = validate_marker(&git, start)
        .unwrap_or_else(|error| panic!("the start marker must validate: {error}"));
    assert_eq!(marker.kind, CommitKind::Start);
    assert_eq!(marker.candidate, parent);
    assert_eq!(oracle::commit_tree(&repo, start), marker.result_tree);
    let bytes = commit_bytes(&repo, start);
    let raw = String::from_utf8_lossy(&bytes);
    assert!(raw.contains(&format!("parent {parent}")));
    assert!(raw.contains("Margin-Kind: start"));
    assert!(
        !raw.contains("Margin-Candidate: unborn"),
        "a born branch records its parent as the candidate"
    );
    // The scoped content is exactly the eligible working content.
    assert_eq!(
        oracle::blob(
            &repo,
            &text(&repo.git_ok(["rev-parse", &format!("{start}:notes/a.md")]))
        ),
        b"# One revised\n"
    );
    assert_eq!(
        oracle::blob(
            &repo,
            &text(&repo.git_ok(["rev-parse", &format!("{start}:notes/draft.md")]))
        ),
        b"# Draft\n"
    );
    assert!(
        !repo
            .git([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{start}:notes/gone.md")
            ])
            .status
            .success(),
        "the unstaged deletion left the starting tree"
    );
    // Nothing out of scope moved, in the tree or on disk.
    oracle::assert_out_of_scope_tree_unchanged(&repo, &parent, start, &["notes"], "init");
    oracle::assert_index_matches_tree(&repo, start);
    before.assert_out_of_scope_unchanged(&oracle::snapshot(&repo), &["notes"], "init");
    assert_eq!(
        oracle::head_oid(&repo).as_deref(),
        Some(start),
        "the adopted branch carries the start commit"
    );

    // The settings persist after the verified commit and first use captures
    // nothing automatically (SPEC 6.2, 8.2).
    assert_eq!(raw_config(&repo, "margin.version").as_deref(), Some("1"));
    assert_eq!(raw_config(&repo, "margin.branch").as_deref(), Some("main"));
    let stored = raw_config(&repo, "margin.scope").unwrap();
    assert_eq!(
        stored,
        String::from_utf8(
            resolve_scope(&git, &scope)
                .unwrap()
                .descriptor
                .canonical_bytes()
        )
        .unwrap()
    );
    assert_eq!(raw_config(&repo, "margin.autoCheckpoint"), None);
    assert_eq!(raw_config(&repo, "margin.captureAfterEdit"), None);
    let applied: Vec<String> = data.settings.iter().map(|s| s.key.clone()).collect();
    assert_eq!(
        applied,
        vec!["margin.version", "margin.branch", "margin.scope"]
    );

    // The stored scope anchors the workspace through the shared walk.
    let inspection = operations.inspect_workspace(&InspectRequest {
        scope: None,
        path_filter: Vec::new(),
    });
    assert!(inspection.ok());
    match &inspection.data.as_ref().unwrap().baseline {
        BaselineStatus::Anchored(anchor) => assert_eq!(anchor.commit, start),
        other => panic!("the start marker must anchor the scope: {other:?}"),
    }

    // Effects report the staged capture, the created commit, and the
    // settings writes, with the working bytes untouched (SPEC 7.1).
    assert_eq!(result.effects.index, ObservedEffect::Changed);
    assert_eq!(result.effects.commit, CommitEffect::Created);
    assert_eq!(result.effects.commit_oid.as_deref(), Some(start));
    assert_eq!(result.effects.working_tree, ObservedEffect::Unchanged);
    assert_eq!(result.effects.repository, RepositoryEffect::Unchanged);
    assert_eq!(result.effects.configuration, ObservedEffect::Changed);
    assert_eq!(result.effects.integration, ObservedEffect::Unchanged);
}

#[test]
fn an_already_saved_tree_yields_an_empty_start_commit() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let parent = oracle::head_oid(&repo).unwrap();

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.initialize_scope(&first_use(&scope_input(&["notes"], &[], &[".md"])));

    assert!(result.ok(), "an already saved tree still anchors");
    let data = result.data.as_ref().unwrap();
    let start = data.start_commit.as_deref().unwrap();
    assert_eq!(
        oracle::commit_tree(&repo, start),
        oracle::commit_tree(&repo, &parent),
        "the starting commit may be empty when the tree is already saved"
    );
    assert_eq!(oracle::head_oid(&repo).as_deref(), Some(start));
    assert_eq!(raw_config(&repo, "margin.branch").as_deref(), Some("main"));
}

#[test]
fn unborn_initialization_creates_the_root_start_marker() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.write_file("notes/deep/draft.md", b"# Draft\n");
    repo.write_file("readme.txt", b"out of scope\n");

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let before = oracle::snapshot(&repo);
    let result = operations.initialize_scope(&first_use(&scope_input(&["notes"], &[], &[".md"])));

    assert!(result.ok(), "unborn initialization must succeed");
    let data = result.data.as_ref().unwrap();
    let start = data.start_commit.as_deref().expect("a start commit");

    // Exactly one zero-parent start marker whose tree holds only eligible
    // scoped entries (SPEC 5.1).
    let bytes = commit_bytes(&repo, start);
    let raw = String::from_utf8_lossy(&bytes);
    assert!(!raw.contains("\nparent "), "the marker has no parents");
    assert!(raw.contains("Margin-Candidate: unborn"));
    assert!(raw.contains("Margin-Kind: start"));
    let entries = oracle::ls_tree(&repo, start);
    let mut paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, vec!["notes/a.md", "notes/deep/draft.md"]);
    validate_marker(&git, start).unwrap_or_else(|error| panic!("the marker validates: {error}"));
    assert_eq!(oracle::head_oid(&repo).as_deref(), Some(start));
    oracle::assert_index_matches_tree(&repo, start);
    before.assert_out_of_scope_unchanged(&oracle::snapshot(&repo), &["notes"], "unborn init");

    assert_eq!(raw_config(&repo, "margin.branch").as_deref(), Some("main"));
    assert_eq!(raw_config(&repo, "margin.version").as_deref(), Some("1"));
    assert_eq!(result.effects.repository, RepositoryEffect::Unchanged);
    assert_eq!(result.effects.commit, CommitEffect::Created);
}

#[test]
fn an_unborn_empty_scope_anchors_the_empty_tree() {
    let repo = TestRepo::init();
    fs::create_dir_all(repo.file_path("empty")).unwrap();
    repo.write_file("readme.txt", b"out of scope\n");
    let empty_tree = text(&repo.git_stdin(["mktree"], b""));

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.initialize_scope(&first_use(&scope_input(&["empty"], &[], &[".md"])));

    assert!(result.ok(), "an empty scope still anchors");
    let data = result.data.as_ref().unwrap();
    let start = data.start_commit.as_deref().unwrap();
    assert_eq!(oracle::commit_tree(&repo, start), empty_tree);
    assert!(oracle::ls_tree(&repo, start).is_empty());
    assert_eq!(raw_config(&repo, "margin.branch").as_deref(), Some("main"));
}

#[test]
fn dry_run_reports_the_plan_without_writing() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.write_file("other.txt", b"out of scope\n");
    repo.commit_all("import");
    repo.write_file("notes/a.md", b"# One revised\n");
    repo.write_file("notes/draft.md", b"# Draft\n");

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let scope = scope_input(&["notes"], &[], &[".md"]);
    let request = InitRequest {
        dry_run: true,
        ..first_use(&scope)
    };
    let before = oracle::snapshot(&repo);
    let result = operations.initialize_scope(&request);

    assert!(result.ok(), "a dry run succeeds");
    let data = result.data.as_ref().unwrap();
    assert!(data.dry_run);
    assert_eq!(data.start_commit, None, "a dry run commits nothing");
    assert!(!data.repository_created);
    assert_eq!(data.tree_reason, None);
    let planned = data.expected_tree.as_deref().expect("an intended tree");
    assert_eq!(result.effects, Effects::unchanged());
    before.assert_unchanged(&oracle::snapshot(&repo), "a dry run");

    // The planned settings and tree are what the real initialization applies.
    let applied = operations.initialize_scope(&first_use(&scope));
    assert!(applied.ok());
    assert_eq!(
        applied.data.as_ref().unwrap().settings,
        data.settings,
        "the dry run reports the exact planned settings"
    );
    let start = applied.data.as_ref().unwrap().start_commit.clone().unwrap();
    assert_eq!(
        oracle::commit_tree(&repo, &start),
        planned,
        "the dry run predicted the intended tree"
    );
}

#[test]
fn create_repo_dry_run_proposes_without_creating() {
    let folder = FreshFolder::with_identity();
    folder.write_file("content/a.md", b"# One\n");
    folder.write_file("draft.txt", b"out of scope\n");

    let git = adapter_for_folder(&folder);
    let operations = Operations::new(&git);
    let scope = scope_input(&["."], &[], &[".md"]);
    let mut request = first_use(&scope);
    request.create_repository = true;
    request.dry_run = true;
    let result = operations.initialize_scope(&request);

    assert!(result.ok(), "the proposal is one successful dry run");
    let data = result.data.as_ref().unwrap();
    assert!(data.dry_run);
    assert_eq!(data.start_commit, None);
    assert_eq!(data.expected_tree, None);
    assert_eq!(data.tree_reason, Some(TREE_REASON_REPOSITORY_NOT_CREATED));
    let proposed = data.proposed.as_ref().expect("the exact proposal");
    assert!(proposed.create_repository);
    assert_eq!(proposed.scope.include, vec![".".to_string()]);
    assert_eq!(proposed.scope.suffixes, vec![".md".to_string()]);
    assert_eq!(proposed.scope.exclude, Vec::<String>::new());
    assert_eq!(proposed.branch, None);
    assert_eq!(result.effects, Effects::unchanged());
    assert!(
        !folder.folder().join(".git").exists(),
        "a dry run must not create the target .git"
    );
    assert_eq!(
        fs::read(folder.folder().join("content/a.md")).unwrap(),
        b"# One\n"
    );
}

#[test]
fn create_repo_initializes_the_repository_and_captures() {
    let folder = FreshFolder::with_identity();
    folder.write_file("content/a.md", b"# One\n");
    folder.write_file("draft.txt", b"out of scope\n");

    let git = adapter_for_folder(&folder);
    let operations = Operations::new(&git);
    let mut request = first_use(&scope_input(&["."], &[], &[".md"]));
    request.create_repository = true;
    let result = operations.initialize_scope(&request);

    assert!(result.ok(), "creation and capture must succeed");
    let data = result.data.as_ref().unwrap();
    assert!(data.repository_created, "the repository was created");
    let start = data.start_commit.as_deref().unwrap();
    assert_eq!(result.effects.repository, RepositoryEffect::Created);

    // The created repository carries one root start marker over the unborn
    // branch, holding only the eligible scoped entries.
    let bytes = folder.git_ok(["cat-file", "commit", start]);
    let raw = String::from_utf8_lossy(&bytes);
    assert!(raw.contains("Margin-Candidate: unborn"));
    assert!(raw.contains("Margin-Kind: start"));
    assert_eq!(folder_tree_paths(&folder, start), vec!["content/a.md"]);
    assert!(folder.folder().join(".git").exists());
    assert_eq!(
        fs::read(folder.folder().join("content/a.md")).unwrap(),
        b"# One\n",
        "creation rewrites no working bytes"
    );
    assert_eq!(
        fs::read(folder.folder().join("draft.txt")).unwrap(),
        b"out of scope\n"
    );
    assert_eq!(
        raw_folder_config(&folder, "margin.branch").as_deref(),
        Some(DEFAULT_BRANCH)
    );
    assert!(raw_folder_config(&folder, "margin.scope").is_some());
}

#[test]
fn create_repo_names_the_initial_branch() {
    let folder = FreshFolder::with_identity();
    folder.write_file("content/a.md", b"# One\n");

    let git = adapter_for_folder(&folder);
    let operations = Operations::new(&git);
    let mut request = first_use(&scope_input(&["."], &[], &[".md"]));
    request.create_repository = true;
    request.branch = Some("writing".to_string());
    let result = operations.initialize_scope(&request);

    assert!(result.ok());
    assert!(result.data.as_ref().unwrap().repository_created);
    assert_eq!(
        text(&folder.git_ok(["symbolic-ref", "HEAD"])),
        "refs/heads/writing",
        "the created repository uses the named initial branch"
    );
    assert_eq!(
        raw_folder_config(&folder, "margin.branch").as_deref(),
        Some("writing")
    );
}

#[test]
fn missing_identity_is_an_actionable_setup_error() {
    let folder = FreshFolder::without_identity();
    folder.write_file("content/a.md", b"# One\n");

    let git = adapter_for_folder(&folder);
    let operations = Operations::new(&git);
    let mut request = first_use(&scope_input(&["."], &[], &[".md"]));
    request.create_repository = true;
    let result = operations.initialize_scope(&request);

    // The repository was created and that effect is reported, but no commit
    // exists: Margin never invents an author (SPEC 5.1, 6.4).
    assert!(!result.ok(), "a missing identity must refuse");
    let error = result.error.as_ref().unwrap();
    assert_eq!(error.code, "MISSING_IDENTITY");
    assert!(error.message.contains("user.name"));
    assert_eq!(result.effects.repository, RepositoryEffect::Created);
    assert_eq!(result.effects.commit, CommitEffect::None);
    assert!(result.data.is_none());
    assert!(folder.folder().join(".git").exists());
    assert!(
        !folder
            .git(["rev-parse", "--verify", "--quiet", "HEAD"])
            .status
            .success(),
        "no commit was created"
    );
    assert_eq!(raw_folder_config(&folder, "margin.branch"), None);

    // The executable maps the refusal to the unmet-precondition class.
    let second = FreshFolder::without_identity();
    second.write_file("content/a.md", b"# One\n");
    let output = margin(
        second.env(),
        second.folder(),
        &["init", "--create-repo", "--scope", ".", "--json"],
    );
    assert_eq!(output.status.code(), Some(4));
    let envelope = envelope_of(&output);
    assert_eq!(envelope["error"]["code"], "MISSING_IDENTITY");
    assert_eq!(envelope["effects"]["repository"], "created");
    assert_eq!(envelope["effects"]["commit"], "none");
}

#[test]
fn existing_staged_changes_block_setup() {
    // A staged change against HEAD blocks adoption of a born repository.
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    repo.write_file("notes/a.md", b"# One revised\n");
    repo.git_ok(["add", "notes/a.md"]);
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let before = oracle::snapshot(&repo);

    let result = operations.initialize_scope(&first_use(&scope_input(&["notes"], &[], &[".md"])));
    assert_refusal(&result, "STAGED_CHANGES", "setup over staged changes");
    assert!(
        result
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("staged change")
    );
    before.assert_unchanged(&oracle::snapshot(&repo), "the refusal");

    // On an unborn branch any staged entry counts (SPEC 5.1).
    let unborn = TestRepo::init();
    unborn.write_file("notes/a.md", b"# One\n");
    unborn.git_ok(["add", "notes/a.md"]);
    let git = adapter_for(&unborn);
    let operations = Operations::new(&git);
    let result = operations.initialize_scope(&first_use(&scope_input(&["notes"], &[], &[".md"])));
    assert_refusal(
        &result,
        "STAGED_CHANGES",
        "unborn setup over staged entries",
    );
}

#[test]
fn the_expected_branch_asserts_the_adopted_branch() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let scope = scope_input(&["notes"], &[], &[".md"]);

    // Margin adopts the current branch and never switches it (SPEC 5.1).
    let mut mismatched = first_use(&scope);
    mismatched.branch = Some("writing".to_string());
    let before = oracle::snapshot(&repo);
    let result = operations.initialize_scope(&mismatched);
    assert_refusal(&result, "BRANCH_CHANGED", "a wrong expected branch");
    before.assert_unchanged(&oracle::snapshot(&repo), "the branch assertion");

    // Without the assertion the current branch is adopted and recorded.
    let adopted = operations.initialize_scope(&first_use(&scope));
    assert!(adopted.ok());
    assert_eq!(raw_config(&repo, "margin.branch").as_deref(), Some("main"));

    // A repository on another branch records that branch, not a fixed name.
    let other = TestRepo::init_with(RepoOptions::new().with_initial_branch("writing"));
    other.write_file("notes/a.md", b"# One\n");
    other.commit_all("import");
    let git = adapter_for(&other);
    let operations = Operations::new(&git);
    let adopted = operations.initialize_scope(&first_use(&scope));
    assert!(adopted.ok());
    assert_eq!(
        raw_config(&other, "margin.branch").as_deref(),
        Some("writing")
    );
}

#[test]
fn explicit_init_on_another_branch_adopts_it() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let scope = scope_input(&["notes"], &[], &[".md"]);
    let first = operations.initialize_scope(&first_use(&scope));
    assert!(first.ok());
    let start = first.data.as_ref().unwrap().start_commit.clone().unwrap();

    // Another branch at the same commit, with an unsaved edit beside it:
    // the anchor still validates, so one explicit init adopts the new
    // branch as the repeat initialization of the same scope (SPEC 8.2).
    repo.git_ok(["switch", "--quiet", "--create", "other"]);
    repo.write_file("notes/a.md", b"# One revised\n");
    let before = oracle::snapshot(&repo);
    let adoption = operations.initialize_scope(&InitRequest {
        scope: None,
        ..first_use(&scope)
    });
    assert!(
        adoption.ok(),
        "the init adopts the new branch: {:?}",
        adoption.error
    );
    let data = adoption.data.as_ref().unwrap();
    assert_eq!(
        data.start_commit, None,
        "the validated anchor is adopted, not committed again"
    );
    assert_eq!(data.tree_reason, Some(TREE_REASON_ALREADY_ANCHORED));
    assert_eq!(
        applied_keys(&adoption),
        vec!["margin.branch"],
        "only the stored branch changes"
    );
    assert_eq!(
        raw_config(&repo, "margin.branch").as_deref(),
        Some("other"),
        "the adoption records the new branch"
    );
    assert_eq!(head(&repo), start, "the anchor commit still stands");
    before.assert_unchanged(&oracle::snapshot(&repo), "the branch adoption");
    // Effects: one configuration write, nothing else (SPEC 7.1).
    assert_eq!(adoption.effects, {
        let mut expected = Effects::unchanged();
        expected.observe_configuration(ObservedEffect::Changed);
        expected
    });
    // The walk still anchors at the start marker the adoption kept.
    assert_eq!(
        anchored_at(&repo, &start, &resolved_scope(&repo, "notes")),
        start
    );
}

#[test]
fn repeat_initialization_updates_only_supplied_settings() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let scope = scope_input(&["notes"], &[], &[".md"]);

    let first = operations.initialize_scope(&first_use(&scope));
    assert!(first.ok());
    let start = first.data.as_ref().unwrap().start_commit.clone().unwrap();

    // Writing continues after the anchor, so the repeat must find its anchor
    // below newer ordinary history, not only at HEAD.
    repo.write_file("notes/a.md", b"# Two\n");
    let external = repo.commit_all("external edit");
    let before = oracle::snapshot(&repo);

    // The repeat names the stored scope and one setup setting (SPEC 8.2).
    let repeat = operations.initialize_scope(&InitRequest {
        scope: None,
        auto_checkpoint: Some(true),
        ..first_use(&scope)
    });
    assert!(repeat.ok(), "the repeat must succeed");
    let data = repeat.data.as_ref().unwrap();
    assert_eq!(data.start_commit, None, "no start commit is added");
    assert!(!data.dry_run);
    assert_eq!(data.tree_reason, Some(TREE_REASON_ALREADY_ANCHORED));
    assert_eq!(data.expected_tree, None);
    assert_eq!(
        applied_keys(&repeat),
        vec!["margin.autoCheckpoint"],
        "only the supplied setting changes"
    );
    assert_eq!(
        raw_config(&repo, "margin.autoCheckpoint").as_deref(),
        Some("true")
    );

    // The stored branch, scope, and version keep their stored values, no
    // commit appears, and the anchor below the external edit still stands.
    assert_eq!(raw_config(&repo, "margin.version").as_deref(), Some("1"));
    assert_eq!(raw_config(&repo, "margin.branch").as_deref(), Some("main"));
    let stored_scope = raw_config(&repo, "margin.scope").unwrap();
    let canonical = String::from_utf8(
        resolve_scope(&git, &scope)
            .unwrap()
            .descriptor
            .canonical_bytes(),
    )
    .unwrap();
    assert_eq!(stored_scope, canonical);
    assert_eq!(head(&repo), external, "no commit was added");
    before.assert_unchanged(&oracle::snapshot(&repo), "the repeat");
    let inspection = operations.inspect_workspace(&InspectRequest {
        scope: None,
        path_filter: Vec::new(),
    });
    assert!(inspection.ok());
    match &inspection.data.as_ref().unwrap().baseline {
        BaselineStatus::Anchored(marker) => {
            assert_eq!(marker.commit, start);
            assert_eq!(marker.kind, CommitKind::Start);
        }
        other => panic!("the repeat must preserve the anchor: {other:?}"),
    }

    // Effects: one configuration write, nothing else (SPEC 7.1).
    assert_eq!(repeat.effects, {
        let mut expected = Effects::unchanged();
        expected.observe_configuration(ObservedEffect::Changed);
        expected
    });
}

#[test]
fn a_repeat_that_supplies_nothing_writes_nothing() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let scope = scope_input(&["notes"], &[], &[".md"]);
    assert!(operations.initialize_scope(&first_use(&scope)).ok());

    // Omitted settings keep their values, so the repeat is one explicit
    // no-op that writes not even the version stamp (SPEC 8.2).
    let before = oracle::snapshot(&repo);
    let repeat = operations.initialize_scope(&InitRequest {
        scope: None,
        ..first_use(&scope)
    });
    assert!(repeat.ok(), "an empty repeat is one explicit no-op");
    let data = repeat.data.as_ref().unwrap();
    assert_eq!(data.start_commit, None);
    assert!(data.settings.is_empty());
    assert_eq!(repeat.effects, Effects::unchanged());
    before.assert_unchanged(&oracle::snapshot(&repo), "the no-op repeat");
    assert_eq!(raw_config(&repo, "margin.version").as_deref(), Some("1"));
}

#[test]
fn a_repeat_dry_run_plans_only_the_supplied_settings() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let scope = scope_input(&["notes"], &[], &[".md"]);
    assert!(operations.initialize_scope(&first_use(&scope)).ok());

    let before = oracle::snapshot(&repo);
    let repeat = operations.initialize_scope(&InitRequest {
        scope: None,
        auto_checkpoint: Some(true),
        capture_after_edit: Some(false),
        dry_run: true,
        ..first_use(&scope)
    });
    assert!(repeat.ok(), "a repeat dry run succeeds");
    let data = repeat.data.as_ref().unwrap();
    assert!(data.dry_run);
    assert_eq!(data.start_commit, None);
    assert_eq!(data.expected_tree, None);
    assert_eq!(data.tree_reason, Some(TREE_REASON_ALREADY_ANCHORED));
    assert_eq!(
        applied_keys(&repeat),
        vec!["margin.autoCheckpoint", "margin.captureAfterEdit"]
    );
    assert_eq!(repeat.effects, Effects::unchanged());
    before.assert_unchanged(&oracle::snapshot(&repo), "the repeat dry run");
    assert_eq!(raw_config(&repo, "margin.autoCheckpoint"), None);
    assert_eq!(raw_config(&repo, "margin.captureAfterEdit"), None);

    // The real repeat applies exactly the planned writes.
    let applied = operations.initialize_scope(&InitRequest {
        scope: None,
        auto_checkpoint: Some(true),
        capture_after_edit: Some(false),
        ..first_use(&scope)
    });
    assert!(applied.ok());
    assert_eq!(applied_keys(&applied), applied_keys(&repeat));
}

#[test]
fn new_start_forces_a_fresh_anchor() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let scope = scope_input(&["notes"], &[], &[".md"]);

    let first = operations.initialize_scope(&first_use(&scope));
    assert!(first.ok());
    let start = first.data.as_ref().unwrap().start_commit.clone().unwrap();

    // New eligible content and an explicit --new-start: the anchor resets
    // without claiming approval for the older one (SPEC 8.2).
    repo.write_file("notes/b.md", b"# Two\n");
    let before = oracle::snapshot(&repo);
    let forced = operations.initialize_scope(&InitRequest {
        scope: None,
        new_start: true,
        ..first_use(&scope)
    });
    assert!(forced.ok(), "a forced new anchor must be created");
    let data = forced.data.as_ref().unwrap();
    let fresh = data.start_commit.as_deref().expect("a fresh start commit");
    assert_eq!(data.tree_reason, None);
    validate_marker(&git, fresh)
        .unwrap_or_else(|error| panic!("the fresh start marker validates: {error}"));
    assert_eq!(head(&repo), fresh);
    assert_ne!(fresh, start);
    assert_eq!(
        text(&repo.git_ok(["rev-parse", &format!("{fresh}^")])),
        start,
        "the fresh start commits on top of the old anchor"
    );

    // The walk anchors at the fresh start and the old one stays reachable
    // below it; the adoption rewrites the stored branch and scope.
    let resolved = resolved_scope(&repo, "notes");
    assert_eq!(anchored_at(&repo, fresh, &resolved), fresh);
    let inspection = operations.inspect_workspace(&InspectRequest {
        scope: None,
        path_filter: Vec::new(),
    });
    match &inspection.data.as_ref().unwrap().baseline {
        BaselineStatus::Anchored(marker) => assert_eq!(marker.commit, fresh),
        other => panic!("the fresh start must anchor the scope: {other:?}"),
    }
    assert_eq!(
        applied_keys(&forced),
        vec!["margin.branch", "margin.scope"],
        "a forced anchor records the adoption again"
    );
    assert_eq!(forced.effects.commit, CommitEffect::Created);
    assert_eq!(forced.effects.commit_oid.as_deref(), Some(fresh));
    assert_eq!(forced.effects.index, ObservedEffect::Changed);
    before.assert_out_of_scope_unchanged(&oracle::snapshot(&repo), &["notes"], "new start");
}

#[test]
fn a_changed_scope_creates_its_own_anchor() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.write_file("content/c.md", b"# Content\n");
    repo.commit_all("import");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);

    // Scope A anchors first; adopting scope B then creates its own start
    // marker on top (SPEC 8.2, 3.2 rule 7).
    let first = operations.initialize_scope(&first_use(&scope_input(&["notes"], &[], &[".md"])));
    assert!(first.ok());
    let start_a = first.data.as_ref().unwrap().start_commit.clone().unwrap();

    let second = operations.initialize_scope(&first_use(&scope_input(&["content"], &[], &[".md"])));
    assert!(second.ok(), "the changed scope reinitializes");
    let start_b = second.data.as_ref().unwrap().start_commit.clone().unwrap();
    assert_ne!(start_b, start_a);
    assert_eq!(
        text(&repo.git_ok(["rev-parse", &format!("{start_b}^")])),
        start_a,
        "the scope-B start commits on top of the scope-A anchor"
    );

    // Scope B anchors at its own start, and scope A no longer anchors at
    // all: the walk stops at the foreign start instead of crossing it.
    let scope_a = resolved_scope(&repo, "notes");
    let scope_b = resolved_scope(&repo, "content");
    assert_eq!(anchored_at(&repo, &start_b, &scope_b), start_b);
    let error = select_baseline(&git, &start_b, &scope_a.digest)
        .expect_err("a foreign start anchors this history");
    assert!(matches!(
        &error,
        BaselineError::Unavailable(BaselineUnavailable::ForeignStart { commit, .. })
            if commit == &start_b
    ));

    // The stored scope is the one the last adoption recorded.
    assert_eq!(
        raw_config(&repo, "margin.scope").unwrap(),
        String::from_utf8(scope_b.descriptor.canonical_bytes()).unwrap()
    );
}

#[test]
fn a_to_b_to_a_uses_the_new_anchor_not_the_old_approval() {
    let repo = TestRepo::init();
    repo.write_file("other/sentinel.txt", b"untouched\n");
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let scope_a_input = scope_input(&["notes"], &[], &[".md"]);

    // Scope A anchors and is explicitly reviewed: the review is the old
    // approval the later scope changes must never revive.
    let first = operations.initialize_scope(&first_use(&scope_a_input));
    assert!(first.ok());
    let start_a = first.data.as_ref().unwrap().start_commit.clone().unwrap();
    repo.write_file("notes/a.md", b"# One revised\n");
    repo.git_ok(["add", "--", "notes/a.md"]);
    let review = review_marker(&repo, &resolved_scope(&repo, "notes"), "keep all", &start_a);
    assert_eq!(head(&repo), review);
    let scope_a = resolved_scope(&repo, "notes");
    assert_eq!(
        anchored_at(&repo, &review, &scope_a),
        review,
        "the review anchors scope A before the scope changes"
    );

    // Scope B: a changed scope creates its own start above the review.
    repo.write_file("extra/e.md", b"# Extra\n");
    let second = operations.initialize_scope(&first_use(&scope_input(&["extra"], &[], &[".md"])));
    assert!(second.ok());
    let start_b = second.data.as_ref().unwrap().start_commit.clone().unwrap();
    assert!(matches!(
        select_baseline(&git, &start_b, &scope_a.digest),
        Err(BaselineError::Unavailable(
            BaselineUnavailable::ForeignStart { .. }
        ))
    ));

    // Scope A again: the walk stops at the scope-B start, so this
    // initialization creates a fresh A anchor instead of adopting the old
    // one, and the old review stays dead (SPEC 8.2).
    let third = operations.initialize_scope(&first_use(&scope_a_input));
    assert!(third.ok(), "returning to scope A reinitializes");
    let start_a2 = third.data.as_ref().unwrap().start_commit.clone().unwrap();
    assert_eq!(
        text(&repo.git_ok(["rev-parse", &format!("{start_a2}^")])),
        start_b
    );
    let anchor = anchored_at(&repo, &head(&repo), &scope_a);
    assert_eq!(anchor, start_a2);
    assert_ne!(anchor, review, "the old approval is never revived");
    assert_ne!(anchor, start_a, "the old start is never revived");
    validate_marker(&git, &start_a2)
        .unwrap_or_else(|error| panic!("the fresh A start validates: {error}"));

    // The stored scope is A again, and the review stays ordinary reachable
    // history below the fresh anchor.
    assert_eq!(
        raw_config(&repo, "margin.scope").unwrap(),
        String::from_utf8(scope_a.descriptor.canonical_bytes()).unwrap()
    );
    repo.git_ok(["merge-base", "--is-ancestor", &review, &start_a2]);
}

#[test]
fn a_missing_anchor_reinitializes_explicitly() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    let import = repo.commit_all("import");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let scope = scope_input(&["notes"], &[], &[".md"]);
    assert!(operations.initialize_scope(&first_use(&scope)).ok());

    // A branch reset drops the anchor out of first-parent history, so the
    // stored scope no longer anchors anything (SPEC 8.5); reinitialization
    // must create a fresh anchor rather than fail or fall back.
    repo.git_ok(["reset", "--quiet", "--hard", &import]);
    assert!(matches!(
        select_baseline(&git, &head(&repo), &resolved_scope(&repo, "notes").digest),
        Err(BaselineError::Unavailable(
            BaselineUnavailable::Exhausted { .. }
        ))
    ));

    let again = operations.initialize_scope(&InitRequest {
        scope: None,
        ..first_use(&scope)
    });
    assert!(again.ok(), "a missing anchor reinitializes explicitly");
    let data = again.data.as_ref().unwrap();
    let fresh = data.start_commit.as_deref().expect("a fresh start commit");
    assert_eq!(head(&repo), fresh);
    assert_eq!(
        text(&repo.git_ok(["rev-parse", &format!("{fresh}^")])),
        import
    );
    let resolved = resolved_scope(&repo, "notes");
    assert_eq!(anchored_at(&repo, fresh, &resolved), fresh);
    validate_marker(&git, fresh)
        .unwrap_or_else(|error| panic!("the fresh start validates: {error}"));
}

#[test]
fn retry_after_a_settings_failure_adopts_the_validated_marker() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let scope = scope_input(&["notes"], &[], &[".md"]);

    // The first attempt verifies its start commit but cannot persist any
    // setting, so it reports the partial success with the commit.
    let lock = repo.path().join(".git").join("config.lock");
    fs::write(&lock, b"").unwrap();
    let failed = operations.initialize_scope(&first_use(&scope));
    assert!(!failed.ok());
    let start = failed
        .data
        .as_ref()
        .and_then(|data| data.start_commit.clone())
        .expect("the failure reports the verified commit");
    assert_eq!(failed.effects.commit_oid.as_deref(), Some(start.as_str()));
    fs::remove_file(&lock).unwrap();

    // The retry adopts that validated matching marker instead of committing
    // a second one, and persists the settings that never landed (SPEC 8.2).
    let before = oracle::snapshot(&repo);
    let retry = operations.initialize_scope(&first_use(&scope));
    assert!(retry.ok(), "the retry must succeed");
    let data = retry.data.as_ref().unwrap();
    assert_eq!(data.start_commit, None, "no second start is committed");
    assert_eq!(data.tree_reason, Some(TREE_REASON_ALREADY_ANCHORED));
    assert_eq!(
        applied_keys(&retry),
        vec!["margin.version", "margin.branch", "margin.scope"]
    );
    assert_eq!(head(&repo), start, "the verified marker still stands");
    before.assert_unchanged(&oracle::snapshot(&repo), "the retry");
    assert_eq!(raw_config(&repo, "margin.branch").as_deref(), Some("main"));
    assert!(raw_config(&repo, "margin.scope").is_some());

    let inspection = operations.inspect_workspace(&InspectRequest {
        scope: None,
        path_filter: Vec::new(),
    });
    assert!(inspection.ok());
    match &inspection.data.as_ref().unwrap().baseline {
        BaselineStatus::Anchored(marker) => assert_eq!(marker.commit, start),
        other => panic!("the adopted marker anchors the scope: {other:?}"),
    }
}

#[test]
fn a_settings_failure_reports_the_verified_commit() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);

    // One stale Git configuration lock lets every read succeed while the
    // first settings write fails, after the start commit already verified.
    let lock = repo.path().join(".git").join("config.lock");
    fs::write(&lock, b"").unwrap();
    let result = operations.initialize_scope(&first_use(&scope_input(&["notes"], &[], &[".md"])));

    // The start commit verified, the settings could not persist, and the
    // partial success shows the commit (SPEC 8.2).
    assert!(!result.ok());
    let data = result.data.as_ref().expect("the observed state");
    let start = data.start_commit.as_deref().expect("the verified commit");
    assert_eq!(result.effects.commit, CommitEffect::Created);
    assert_eq!(result.effects.commit_oid.as_deref(), Some(start));
    assert_eq!(result.effects.index, ObservedEffect::Changed);
    let error = result.error.as_ref().unwrap();
    assert!(
        error.message.contains("could not lock config file"),
        "the failure names the configuration lock: {:?}",
        error.message
    );
    validate_marker(&git, start).unwrap_or_else(|error| panic!("the marker validates: {error}"));

    fs::remove_file(&lock).unwrap();
    assert_eq!(
        raw_config(&repo, "margin.branch"),
        None,
        "no setting was stored"
    );
    assert_eq!(oracle::head_oid(&repo).as_deref(), Some(start));
}

#[test]
fn the_executable_initializes_through_the_shared_operation() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.write_file("notes/draft.md", b"# Draft\n");
    repo.commit_all("import");

    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "init",
            "--scope",
            "notes/",
            "--auto-checkpoint",
            "--editor-argv-json",
            "[\"nvim\",\"--\"]",
            "--json",
        ],
    );
    assert_eq!(output.status.code(), Some(0), "init succeeds");
    let envelope = envelope_of(&output);
    assert_eq!(envelope["command"], "init");
    assert_eq!(envelope["ok"], true);
    assert!(envelope["data"]["start_commit"].is_string());
    assert_eq!(envelope["data"]["repository_created"], false);
    assert_eq!(envelope["data"]["dry_run"], false);
    assert_eq!(envelope["effects"]["commit"], "created");
    assert_eq!(envelope["effects"]["index"], "changed");
    assert_eq!(envelope["effects"]["configuration"], "changed");
    let settings: Vec<String> = envelope["data"]["settings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["key"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        settings,
        vec![
            "margin.version",
            "margin.branch",
            "margin.scope",
            "margin.autoCheckpoint",
            "margin.editorArgv"
        ]
    );
    assert_eq!(
        raw_config(&repo, "margin.autoCheckpoint").as_deref(),
        Some("true")
    );
    assert_eq!(
        raw_config(&repo, "margin.editorArgv").as_deref(),
        Some("[\"nvim\",\"--\"]")
    );

    // The first-use default without the flag stays off (SPEC 6.2).
    let other = TestRepo::init();
    other.write_file("notes/a.md", b"# One\n");
    let output = margin(
        other.env(),
        other.path(),
        &["init", "--scope", "notes/", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(envelope_of(&output)["ok"], true);
    assert_eq!(raw_config(&other, "margin.autoCheckpoint"), None);
}

#[test]
fn the_executable_repeats_initialization_without_a_new_commit() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");

    let first = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes/", "--json"],
    );
    assert_eq!(first.status.code(), Some(0), "first use initializes");
    let start = envelope_of(&first)["data"]["start_commit"]
        .as_str()
        .expect("a start commit")
        .to_string();

    // The repeat names only one setup setting, so it changes no history.
    let repeat = margin(
        repo.env(),
        repo.path(),
        &["init", "--auto-checkpoint", "--json"],
    );
    assert_eq!(repeat.status.code(), Some(0), "the repeat succeeds");
    let envelope = envelope_of(&repeat);
    assert_eq!(envelope["command"], "init");
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["data"]["start_commit"], serde_json::Value::Null);
    assert_eq!(envelope["data"]["tree_reason"], "already_anchored");
    assert_eq!(envelope["data"]["expected_tree"], serde_json::Value::Null);
    assert_eq!(envelope["effects"]["commit"], "none");
    assert_eq!(envelope["effects"]["index"], "unchanged");
    assert_eq!(envelope["effects"]["configuration"], "changed");
    let settings: Vec<String> = envelope["data"]["settings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["key"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(settings, vec!["margin.autoCheckpoint"]);
    assert_eq!(
        text(&repo.git_ok(["rev-parse", "HEAD"])),
        start,
        "the repeat added no commit"
    );
    assert_eq!(
        raw_config(&repo, "margin.autoCheckpoint").as_deref(),
        Some("true")
    );
}

#[test]
fn init_without_a_repository_or_scope_fails_typed() {
    let folder = FreshFolder::with_identity();
    folder.write_file("content/a.md", b"# One\n");

    // Without --create-repo, a folder inside no repository is one typed
    // uninitialized state, and nothing is written.
    let output = margin(
        folder.env(),
        folder.folder(),
        &["init", "--scope", ".", "--json"],
    );
    assert_eq!(output.status.code(), Some(4));
    let envelope = envelope_of(&output);
    assert_eq!(envelope["error"]["code"], "NOT_INITIALIZED");
    assert_eq!(envelope["effects"]["repository"], "unchanged");
    assert!(!folder.folder().join(".git").exists());

    // Inside a repository, a first use without any scope cannot adopt one.
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let output = margin(repo.env(), repo.path(), &["init", "--json"]);
    assert_eq!(output.status.code(), Some(4));
    let envelope = envelope_of(&output);
    assert_eq!(envelope["error"]["code"], "NOT_INITIALIZED");
    assert!(envelope["error"]["recovery_argv"].is_array());
}

#[test]
fn an_invalid_start_marker_reports_the_created_commit() {
    let repo = TestRepo::init();
    repo.write_file("readme.txt", b"out of scope\n");
    repo.commit_all("import");
    repo.write_file("notes/draft.md", b"# Draft\n");
    // One commit-msg hook appends a second reserved trailer: Git creates
    // the commit, but its metadata no longer matches the prepared map.
    install_hook(
        &repo,
        "commit-msg",
        "#!/bin/sh\nprintf 'Margin-Kind: draft\\n' >> \"$1\"\nexit 0\n",
    );

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.initialize_scope(&first_use(&scope_input(&["notes"], &[], &[".md"])));

    // The rewritten marker is one contract violation, and the created commit
    // is reported with its object ID even though verification failed (SPEC
    // 7.1, 9.6).
    assert!(!result.ok(), "a rewritten marker must fail verification");
    assert_eq!(
        result.error.as_ref().unwrap().code,
        "CONTRACT_VIOLATION",
        "the hook-rewritten marker is the primary error"
    );
    let created = oracle::head_oid(&repo).expect("the commit itself succeeded");
    assert_eq!(result.effects.commit, CommitEffect::Created);
    assert_eq!(result.effects.commit_oid.as_deref(), Some(created.as_str()));
    let data = result.data.as_ref().expect("the observed state");
    assert_eq!(data.start_commit.as_deref(), Some(created.as_str()));
    assert!(validate_marker(&git, &created).is_err());

    // The partial state is preserved and the settings adoption is withheld
    // (SPEC 8.2, 9.6): no margin.* key was written above the invalid marker.
    assert_eq!(result.effects.index, ObservedEffect::Changed);
    assert_eq!(result.effects.working_tree, ObservedEffect::Unchanged);
    assert_eq!(result.effects.configuration, ObservedEffect::Unchanged);
    assert_eq!(result.effects.repository, RepositoryEffect::Unchanged);
    assert!(data.settings.is_empty());
    assert_eq!(raw_config(&repo, "margin.version"), None);
    assert_eq!(raw_config(&repo, "margin.branch"), None);
    assert_eq!(raw_config(&repo, "margin.scope"), None);

    // Stock Git confirms the commit, its duplicate trailer, and the retained
    // scoped content.
    let raw = String::from_utf8_lossy(&commit_bytes(&repo, &created)).to_string();
    assert!(raw.contains("Margin-Kind: start\n"));
    assert!(
        raw.contains("Margin-Kind: draft\n"),
        "the hook trailer is part of the commit: {raw:?}"
    );
    assert_eq!(
        oracle::blob(
            &repo,
            &text(&repo.git_ok(["rev-parse", &format!("{created}:notes/draft.md")]))
        ),
        b"# Draft\n",
        "the created commit retains the working content"
    );
    assert_eq!(
        text(&repo.git_ok(["diff-index", "--cached", "--name-only", &created])),
        "",
        "the staging became the created commit"
    );

    // The executable reports the same observed state in one JSON envelope.
    let second = TestRepo::init();
    second.write_file("readme.txt", b"out of scope\n");
    second.commit_all("import");
    second.write_file("notes/draft.md", b"# Draft\n");
    install_hook(
        &second,
        "commit-msg",
        "#!/bin/sh\nprintf 'Margin-Kind: draft\\n' >> \"$1\"\nexit 0\n",
    );
    let output = margin(
        second.env(),
        second.path(),
        &["init", "--scope", "notes/", "--json"],
    );
    assert_eq!(output.status.code(), Some(5));
    let envelope = envelope_of(&output);
    assert_eq!(envelope["error"]["code"], "CONTRACT_VIOLATION");
    assert_eq!(envelope["effects"]["commit"], "created");
    assert_eq!(
        envelope["effects"]["commit_oid"],
        oracle::head_oid(&second).expect("the created commit")
    );
    assert_eq!(
        envelope["data"]["start_commit"],
        oracle::head_oid(&second).expect("the created commit")
    );
    assert_eq!(
        envelope["data"]["settings"].as_array().map(Vec::len),
        Some(0)
    );
    assert_eq!(envelope["effects"]["configuration"], "unchanged");
    assert_eq!(
        raw_config(&second, "margin.branch"),
        None,
        "no setting was adopted"
    );
}

#[test]
fn a_rejecting_hook_leaves_staged_content_and_withholds_settings() {
    let repo = TestRepo::init();
    repo.write_file("readme.txt", b"out of scope\n");
    repo.commit_all("import");
    repo.write_file("notes/draft.md", b"# Draft\n");
    let parent = oracle::head_oid(&repo).unwrap();
    install_hook(&repo, "pre-commit", "#!/bin/sh\nexit 1\n");

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.initialize_scope(&first_use(&scope_input(&["notes"], &[], &[".md"])));

    // The commit never ran, so the staged content stays and the inspected
    // effects report it; no setting is adopted above the failure (SPEC 9.2,
    // 8.2).
    assert!(!result.ok(), "a rejecting hook must fail the capture");
    assert_eq!(result.error.as_ref().unwrap().code, "GIT_FAILED");
    assert_eq!(result.effects.commit, CommitEffect::None);
    assert_eq!(result.effects.commit_oid, None);
    assert_eq!(result.effects.index, ObservedEffect::Changed);
    assert_eq!(result.effects.working_tree, ObservedEffect::Unchanged);
    assert_eq!(result.effects.configuration, ObservedEffect::Unchanged);
    let data = result.data.as_ref().expect("the observed state");
    assert_eq!(data.start_commit, None);
    assert!(data.settings.is_empty());
    assert_eq!(oracle::head_oid(&repo).as_deref(), Some(parent.as_str()));
    assert_eq!(
        text(&repo.git_ok(["diff-index", "--cached", "--name-only", "HEAD"])),
        "notes/draft.md",
        "the scoped content stayed staged"
    );
    assert_eq!(raw_config(&repo, "margin.branch"), None);
    assert_eq!(raw_config(&repo, "margin.scope"), None);

    // The next initialization stops on that staged state instead of
    // consuming it.
    let retry = operations.initialize_scope(&first_use(&scope_input(&["notes"], &[], &[".md"])));
    assert_refusal(&retry, "STAGED_CHANGES", "init over its own staged state");
}

#[test]
fn a_hook_that_edits_working_files_is_reported_as_changed() {
    let repo = TestRepo::init();
    repo.write_file("readme.txt", b"out of scope\n");
    repo.commit_all("import");
    repo.write_file("notes/draft.md", b"# Draft\n");
    // The pre-commit hook runs after staging: it edits the working file and
    // then rejects the commit.
    install_hook(
        &repo,
        "pre-commit",
        "#!/bin/sh\nprintf 'hook bytes\\n' >> notes/draft.md\nexit 1\n",
    );

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.initialize_scope(&first_use(&scope_input(&["notes"], &[], &[".md"])));

    // The working-file change the hook made is observed and reported, never
    // silently dropped (SPEC 7.1, 14.1).
    assert!(!result.ok(), "a rejecting hook must fail the capture");
    assert_eq!(result.error.as_ref().unwrap().code, "GIT_FAILED");
    assert_eq!(result.effects.working_tree, ObservedEffect::Changed);
    assert_eq!(result.effects.index, ObservedEffect::Changed);
    assert_eq!(result.effects.commit, CommitEffect::None);
    assert_eq!(repo.read_file("notes/draft.md"), b"# Draft\nhook bytes\n");
    assert_eq!(
        oracle::blob(&repo, &text(&repo.git_ok(["rev-parse", ":notes/draft.md"]))),
        b"# Draft\n",
        "the staged bytes are the content the hook read before its edit"
    );
    assert_eq!(raw_config(&repo, "margin.branch"), None);
}

#[test]
fn a_staging_failure_reports_the_inspected_index() {
    let repo = TestRepo::init();
    repo.write_file("readme.txt", b"out of scope\n");
    repo.commit_all("import");
    repo.write_file("notes/draft.md", b"# Draft\n");
    let parent = oracle::head_oid(&repo).unwrap();
    // One stale index lock lets every guard pass but breaks the first real
    // staging write.
    let lock = repo.path().join(".git").join("index.lock");
    fs::write(&lock, b"").unwrap();

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.initialize_scope(&first_use(&scope_input(&["notes"], &[], &[".md"])));

    // The staging failed, and the reported effects come from inspecting the
    // repository, not from the intended plan: nothing was staged and no
    // setting was adopted (SPEC 9.1 step 7, 8.2).
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
    assert_eq!(result.effects.working_tree, ObservedEffect::Unchanged);
    assert_eq!(result.effects.configuration, ObservedEffect::Unchanged);
    assert!(result.data.is_none());
    assert_eq!(oracle::head_oid(&repo).as_deref(), Some(parent.as_str()));
    fs::remove_file(&lock).unwrap();
    assert_eq!(
        text(&repo.git_ok(["diff-index", "--cached", "--name-only", "HEAD"])),
        "",
        "the failed staging wrote no index entry"
    );
    assert_eq!(raw_config(&repo, "margin.branch"), None);

    // With the lock gone, the same initialization succeeds.
    let retry = operations.initialize_scope(&first_use(&scope_input(&["notes"], &[], &[".md"])));
    assert!(retry.ok(), "the retry must succeed: {:?}", retry.error);
    assert_eq!(retry.effects.commit, CommitEffect::Created);
    assert_eq!(raw_config(&repo, "margin.branch").as_deref(), Some("main"));
}

#[test]
fn an_unborn_capture_failure_reports_the_observed_state() {
    let repo = TestRepo::init();
    repo.write_file("notes/draft.md", b"# Draft\n");
    repo.write_file("readme.txt", b"out of scope\n");
    install_hook(&repo, "pre-commit", "#!/bin/sh\nexit 1\n");

    // An unborn branch has no anchor commit, so the failed capture is
    // observed against the empty starting state (SPEC 9.1 step 7).
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let result = operations.initialize_scope(&first_use(&scope_input(&["notes"], &[], &[".md"])));

    assert!(!result.ok(), "a rejecting hook must fail the capture");
    assert_eq!(result.error.as_ref().unwrap().code, "GIT_FAILED");
    assert_eq!(result.effects.commit, CommitEffect::None);
    assert_eq!(result.effects.commit_oid, None);
    assert_eq!(result.effects.index, ObservedEffect::Changed);
    assert_eq!(result.effects.working_tree, ObservedEffect::Unchanged);
    assert_eq!(result.effects.configuration, ObservedEffect::Unchanged);
    assert!(
        !repo
            .git(["rev-parse", "--verify", "--quiet", "HEAD"])
            .status
            .success(),
        "no commit was created on the unborn branch"
    );
    let staged = text(&repo.git_ok(["ls-files", "--stage"]));
    assert!(
        staged.contains("0\tnotes/draft.md"),
        "the scoped content stayed staged: {staged:?}"
    );
    assert_eq!(
        oracle::blob(&repo, &text(&repo.git_ok(["rev-parse", ":notes/draft.md"]))),
        b"# Draft\n",
        "the staged bytes are the working content"
    );
    assert_eq!(raw_config(&repo, "margin.branch"), None);
}

#[test]
fn a_created_repository_survives_a_failed_capture() {
    let folder = FreshFolder::with_identity();
    folder.write_file("content/a.md", b"# One\n");
    // One global hooks path with a rejecting hook: the repository the
    // operation creates inherits it, so the capture fails after the
    // repository write already happened.
    let hooks = folder.base.path().join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-commit");
    fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    let mut command = Command::new("git");
    command
        .args(["config", "--file"])
        .arg(folder.env().global_config())
        .arg("core.hooksPath")
        .arg(&hooks);
    scrub(&mut command);
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("cannot write the test configuration: {error}"));
    assert!(
        output.status.success(),
        "cannot set the test hooks path: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let git = adapter_for_folder(&folder);
    let operations = Operations::new(&git);
    let mut request = first_use(&scope_input(&["."], &[], &[".md"]));
    request.create_repository = true;
    let result = operations.initialize_scope(&request);

    // The repository write stays, the failed capture is reported from
    // inspection, and no setting is adopted (SPEC 6.4, 8.2, 9.1 step 7).
    assert!(!result.ok(), "a rejecting hook must fail the capture");
    assert_eq!(result.error.as_ref().unwrap().code, "GIT_FAILED");
    assert_eq!(result.effects.repository, RepositoryEffect::Created);
    assert_eq!(result.effects.commit, CommitEffect::None);
    assert_eq!(result.effects.commit_oid, None);
    assert_eq!(result.effects.index, ObservedEffect::Changed);
    assert_eq!(result.effects.working_tree, ObservedEffect::Unchanged);
    assert_eq!(result.effects.configuration, ObservedEffect::Unchanged);
    let data = result.data.as_ref().expect("the observed state");
    assert_eq!(data.start_commit, None);
    assert!(data.repository_created, "the created repository is kept");
    assert!(data.settings.is_empty());
    assert!(folder.folder().join(".git").exists());
    assert!(
        !folder
            .git(["rev-parse", "--verify", "--quiet", "HEAD"])
            .status
            .success(),
        "no commit was created"
    );
    assert_eq!(
        fs::read(folder.folder().join("content/a.md")).unwrap(),
        b"# One\n",
        "the working content is retained on disk"
    );
    assert_eq!(raw_folder_config(&folder, "margin.branch"), None);
}

// ---------------------------------------------------------------------------
// The shared mutation lock (SPEC 9.1 step 2)
// ---------------------------------------------------------------------------

#[test]
fn a_held_lock_refuses_first_use_initialization_as_busy() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");

    // One competing Margin operation holds the lock of this Git directory,
    // so the first use refuses `BUSY` and writes nothing (SPEC 9.1 step 2).
    let held = hold_mutation_lock(&repo);
    let before = oracle::snapshot(&repo);
    let output = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes", "--suffix", ".md", "--json"],
    );
    assert_eq!(output.status.code(), Some(4));
    let envelope: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one valid envelope");
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["command"], "init");
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
    before.assert_unchanged(&oracle::snapshot(&repo), "the refused initialization");
    assert!(
        oracle::head_oid(&repo).is_none(),
        "no start commit appeared"
    );
    assert_eq!(raw_config(&repo, "margin.scope"), None);
    assert!(
        output.stderr.is_empty(),
        "machine mode writes no stderr, got: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The competing operation ends; the same command line then anchors.
    drop(held);
    let retry = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes", "--suffix", ".md", "--json"],
    );
    assert_eq!(retry.status.code(), Some(0));
    let retry_envelope: serde_json::Value =
        serde_json::from_slice(&retry.stdout).expect("one valid envelope");
    assert_eq!(retry_envelope["ok"], true);
    assert_eq!(retry_envelope["data"]["repository_created"], false);
    assert!(oracle::head_oid(&repo).is_some(), "the start commit exists");
    assert_eq!(raw_config(&repo, "margin.branch").as_deref(), Some("main"));
}

#[test]
fn a_held_lock_refuses_repeat_initialization_settings_as_busy() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let start = operations
        .initialize_scope(&first_use(&scope_input(&["notes"], &[], &[".md"])))
        .data
        .and_then(|data| data.start_commit)
        .expect("first use anchors the scope");

    // The repeat path changes only settings, and it is one Margin write of
    // the repository: it waits for the lock and refuses `BUSY` first
    // (SPEC 9.1 step 2, 8.2).
    let held = hold_mutation_lock(&repo);
    let output = margin(
        repo.env(),
        repo.path(),
        &["init", "--auto-checkpoint", "--json"],
    );
    assert_eq!(output.status.code(), Some(4));
    let envelope: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one valid envelope");
    assert_eq!(envelope["error"]["code"], "BUSY");
    assert_eq!(envelope["effects"]["configuration"], "unchanged");
    assert_eq!(raw_config(&repo, "margin.autoCheckpoint"), None);
    assert_eq!(
        oracle::head_oid(&repo).as_deref(),
        Some(start.as_str()),
        "no second start commit appeared"
    );

    drop(held);
    let retry = margin(
        repo.env(),
        repo.path(),
        &["init", "--auto-checkpoint", "--json"],
    );
    assert_eq!(retry.status.code(), Some(0));
    assert_eq!(
        raw_config(&repo, "margin.autoCheckpoint").as_deref(),
        Some("true"),
        "the retry applies the setting"
    );
    assert_eq!(
        oracle::head_oid(&repo).as_deref(),
        Some(start.as_str()),
        "the repeat still adds no start commit"
    );
}

#[test]
fn create_repo_adoption_waits_for_the_mutation_lock() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");

    // Bootstrap boundary: creating a repository cannot hold the lock,
    // because the Git directory does not exist until `git init` returns.
    // Adopting a repository that exists is serialized like every other
    // write, so `--create-repo` over an existing repository refuses `BUSY`
    // instead of racing the competing operation (SPEC 5.1, 9.1 step 2).
    let held = hold_mutation_lock(&repo);
    let before = oracle::snapshot(&repo);
    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "init",
            "--create-repo",
            "--scope",
            "notes",
            "--suffix",
            ".md",
            "--json",
        ],
    );
    assert_eq!(output.status.code(), Some(4));
    let envelope: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one valid envelope");
    assert_eq!(envelope["error"]["code"], "BUSY");
    assert_eq!(envelope["data"], serde_json::Value::Null);
    before.assert_unchanged(&oracle::snapshot(&repo), "the refused adoption");
    assert_eq!(raw_config(&repo, "margin.scope"), None);

    // The competing operation ends; the same adoption then completes.
    drop(held);
    let retry = margin(
        repo.env(),
        repo.path(),
        &[
            "init",
            "--create-repo",
            "--scope",
            "notes",
            "--suffix",
            ".md",
            "--json",
        ],
    );
    assert_eq!(retry.status.code(), Some(0));
    assert_eq!(
        raw_config(&repo, "margin.branch").as_deref(),
        Some("main"),
        "the released adoption completes"
    );
}

// ---------------------------------------------------------------------------
// The intended start tree and its recheck (SPEC 9.1 steps 5 and 6)
// ---------------------------------------------------------------------------

#[test]
fn scoped_bytes_that_move_before_init_staging_refuse_without_writing() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");

    // The writer edits while the initialization is prepared — injected at
    // the disposable proof of the intended start tree — and the recheck
    // refuses before the real index is staged (SPEC 9.1 step 6).
    let git = seam_adapter(
        &repo,
        SeamPass::SecondDisposable,
        "printf ' writer bytes' >> notes/a.md",
    );
    let operations = Operations::new(&git);
    let result = operations.initialize_scope(&first_use(&scope_input(&["notes"], &[], &[".md"])));

    assert!(
        !result.ok(),
        "moved scoped bytes must refuse the initialization"
    );
    let error = result.error.as_ref().unwrap();
    assert_eq!(error.code, "DIRTY_SCOPE");
    assert!(
        error.message.contains("notes/a.md"),
        "the refusal names the moved path: {}",
        error.message
    );
    assert_eq!(result.effects, Effects::unchanged());
    assert!(
        oracle::head_oid(&repo).is_none(),
        "no start commit appeared"
    );
    assert_eq!(text(&repo.git_ok(["ls-files"])), "", "nothing was staged");
    assert_eq!(
        repo.read_file("notes/a.md"),
        b"# One\n writer bytes",
        "the writer's change stays in the working tree"
    );
    assert_eq!(raw_config(&repo, "margin.scope"), None);
}

#[test]
fn a_head_that_moves_before_init_staging_refuses_without_writing() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    repo.write_file("notes/a.md", b"# One revised\n");

    // A concurrent commit on the same branch invalidates the candidate the
    // start marker names, so the recheck refuses before staging
    // (SPEC 9.1 step 6, 16.5 stale state).
    let git = seam_adapter(
        &repo,
        SeamPass::SecondDisposable,
        "git commit --quiet --allow-empty --message concurrent-writer",
    );
    let operations = Operations::new(&git);
    let result = operations.initialize_scope(&first_use(&scope_input(&["notes"], &[], &[".md"])));

    assert!(!result.ok(), "a moved HEAD must refuse the initialization");
    let error = result.error.as_ref().unwrap();
    assert_eq!(error.code, "STALE_STATE");
    let concurrent = oracle::head_oid(&repo).expect("the concurrent commit stands");
    assert!(
        error.message.contains(&concurrent),
        "the refusal names the observed HEAD: {}",
        error.message
    );
    assert_eq!(result.effects, Effects::unchanged());
    assert_eq!(
        text(&repo.git_ok(["diff-index", "--cached", "--name-only", "HEAD"])),
        "",
        "nothing was staged over the concurrent commit"
    );
    assert_eq!(raw_config(&repo, "margin.scope"), None);
}

#[test]
fn an_init_staging_that_diverges_from_the_intended_tree_refuses_before_committing() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");

    // The writer edits between the recheck and the real staging, so the
    // real staged tree no longer equals the proved intended tree: the
    // shared marker writer refuses before committing and the partial state
    // is reported, never adopted (SPEC 9.1 steps 5 and 7, 9.6).
    let git = seam_adapter(
        &repo,
        SeamPass::Real,
        "printf ' writer bytes' >> notes/a.md",
    );
    let operations = Operations::new(&git);
    let result = operations.initialize_scope(&first_use(&scope_input(&["notes"], &[], &[".md"])));

    assert!(
        !result.ok(),
        "a divergent staging must refuse the initialization"
    );
    let error = result.error.as_ref().unwrap();
    assert_eq!(error.code, "DIRTY_SCOPE");
    assert!(
        error.message.contains("intended result tree"),
        "the refusal names the tree divergence: {}",
        error.message
    );
    assert!(
        oracle::head_oid(&repo).is_none(),
        "no start commit appeared"
    );
    let data = result.data.as_ref().expect("the observed state");
    assert_eq!(data.start_commit, None);
    assert!(
        data.settings.is_empty(),
        "the adoption settings are withheld"
    );

    // The staged content stayed behind with the observed effects of the
    // failed capture (SPEC 9.2, 9.6): the index holds the newer bytes the
    // writer produced, the working tree changed, and nothing else moved.
    assert_eq!(
        text(&repo.git_ok(["ls-files"])),
        "notes/a.md",
        "the divergent staging stayed in the index"
    );
    assert_eq!(
        oracle::blob(&repo, &text(&repo.git_ok(["rev-parse", ":notes/a.md"]))),
        b"# One\n writer bytes",
        "the staged bytes are the content the writer left"
    );
    assert_eq!(result.effects.index, ObservedEffect::Changed);
    assert_eq!(result.effects.commit, CommitEffect::None);
    assert_eq!(result.effects.working_tree, ObservedEffect::Changed);
    assert_eq!(result.effects.configuration, ObservedEffect::Unchanged);
    assert_eq!(raw_config(&repo, "margin.scope"), None);
}
