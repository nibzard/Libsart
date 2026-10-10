//! Real-Git tests for the safe process adapter (SPEC 14.1, task T008).
//!
//! Every case proves one adapter rule against the installed Git or a fake
//! program at the process seam:
//!
//! - argv with unusual names and NUL-delimited staging protocols
//! - literal pathspecs, both object formats, bounded time and output
//! - typed failures for nonzero exits, hangs, floods, and malformed output
//! - read-only calls that avoid optional index writes and lazy fetches
//! - refused inherited environment redirection and explicit overrides

mod support;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime};

use margin::git::{Git, GitError, Mode, ObjectFormat, StageOutcome};
use support::repo::{ObjectFormat as HarnessFormat, RepoOptions, TestRepo};

/// Adapter bound to `repo`'s working tree and isolated environment.
fn adapter_for(repo: &TestRepo) -> Git {
    Git::from_env(repo.env().vars().clone()).at(repo.path())
}

/// Writes an executable fake `git` named `name` with the shell `body`.
fn fake_git(repo: &TestRepo, name: &str, body: &str) -> PathBuf {
    let directory = repo.root().join("fake-bin");
    fs::create_dir_all(&directory).expect("the fake bin directory can be created");
    let path = directory.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("the fake git can be written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .expect("the fake git can be made executable");
    path
}

/// Runs stock Git with the isolated harness environment, minus `without`.
fn git_without(repo: &TestRepo, cwd: &Path, args: &[&str], without: &[&str]) -> Output {
    let mut command = Command::new("git");
    command.current_dir(cwd).args(args).stdin(Stdio::null());
    for (key, value) in repo.env().vars() {
        command.env(key, value);
    }
    for key in without {
        command.env_remove(key);
    }
    command.output().expect("stock git can run")
}

/// Index entries of the adapter, keyed and compared as plain tuples.
fn adapter_index(adapter: &Git) -> BTreeMap<OsString, (String, String, u32)> {
    adapter
        .ls_files_stage()
        .expect("the adapter can list the index")
        .into_iter()
        .map(|entry| (entry.path, (entry.mode, entry.oid, entry.stage)))
        .collect()
}

/// Index entries of stock Git through the harness oracle.
fn oracle_index(repo: &TestRepo) -> BTreeMap<OsString, (String, String, u32)> {
    support::oracle::ls_files(repo)
        .into_iter()
        .map(|entry| {
            (
                OsString::from(entry.path),
                (entry.mode, entry.oid, entry.stage),
            )
        })
        .collect()
}

/// Names that stress argv, quoting, pathspec magic, and path parsing.
const UNUSUAL_NAMES: &[&str] = &[
    "-lead-dash.md",
    "--flag-like.md",
    "spa ce.md",
    "quote\"and'quote.md",
    "new\nline.md",
    "tab\tname.md",
    "star*.md",
    "starX.md",
    ":(glob)**.md",
    "back\\slash.md",
    "semi;colon.md",
    "é🦀-ünïcode.md",
    "HEAD",
    "content/nested/deep name.md",
];

#[test]
fn argv_and_nul_protocols_handle_unusual_names() {
    let repo = TestRepo::init();
    for name in UNUSUAL_NAMES {
        repo.write_file(name, b"v1\n");
    }
    repo.commit_all("first pass");

    let adapter = adapter_for(&repo);
    let listed = adapter_index(&adapter);
    let expected = oracle_index(&repo);
    assert_eq!(
        listed, expected,
        "the adapter and stock Git must list the same index"
    );
    assert_eq!(listed.len(), UNUSUAL_NAMES.len());
    assert!(listed.contains_key(&OsString::from("new\nline.md")));
    assert!(listed.contains_key(&OsString::from(":(glob)**.md")));

    // Exact staging through the NUL stdin protocol: one modification, one
    // deletion, one addition. Every other entry stays untouched.
    repo.write_file("spa ce.md", b"v2 with spaces\n");
    fs::remove_file(repo.file_path("new\nline.md")).expect("the newline name can be removed");
    repo.write_file("added\tby adapter.md", b"new\n");
    let staged: Vec<OsString> = vec![
        OsString::from("spa ce.md"),
        OsString::from("new\nline.md"),
        OsString::from("added\tby adapter.md"),
    ];
    assert_eq!(
        adapter.stage_paths(&staged).ok(),
        Some(StageOutcome::Staged)
    );

    let after = adapter_index(&adapter);
    assert_eq!(after, oracle_index(&repo));
    assert!(
        !after.contains_key(&OsString::from("new\nline.md")),
        "the deletion is staged"
    );
    assert!(after.contains_key(&OsString::from("added\tby adapter.md")));
    let modified = &after[&OsString::from("spa ce.md")];
    assert_eq!(modified.0, "100644");
    assert_eq!(
        support::oracle::blob(&repo, &modified.1),
        b"v2 with spaces\n".to_vec(),
        "the staged blob holds the exact raw bytes"
    );

    // An empty resolved staging list is a no-op, never a whole-repository
    // default (SPEC 14.1): the index is unchanged and no process is needed.
    let before_empty = adapter_index(&adapter);
    assert_eq!(adapter.stage_paths(&[]).ok(), Some(StageOutcome::Noop));
    assert_eq!(adapter_index(&adapter), before_empty);
}

#[test]
fn literal_pathspecs_never_glob_or_apply_magic() {
    let repo = TestRepo::init();
    repo.write_file("star*.md", b"literal\n");
    repo.write_file("starX.md", b"glob sibling\n");
    repo.write_file(":(glob)**.md", b"magic\n");
    repo.write_file("HEAD", b"a file, not a revision\n");
    repo.commit_all("first pass");

    let adapter = adapter_for(&repo);
    let listing = |pathspec: &str| {
        let stdout = adapter
            .run_ok(Mode::Query, ["ls-files", "--", pathspec])
            .expect("the literal query succeeds");
        String::from_utf8(stdout).expect("the listing is UTF-8")
    };
    assert_eq!(listing("star*.md"), "star*.md\n", "no glob expansion");
    assert_eq!(
        listing(":(glob)**.md"),
        ":(glob)**.md\n",
        "no pathspec magic"
    );
    assert_eq!(listing("HEAD"), "HEAD\n", "the path after -- is data");

    // Control: without literal pathspecs, Git globs `star*.md` to two files,
    // so the assertion above really discriminates.
    let control = repo.git(["ls-files", "--", "star*.md"]);
    assert_eq!(
        String::from_utf8_lossy(&control.stdout).lines().count(),
        2,
        "the control must show default globbing"
    );
}

#[test]
fn queries_leave_a_stat_stale_index_untouched() {
    let repo = TestRepo::init();
    repo.write_file("content/article.md", b"body\n");
    repo.commit_all("first pass");

    // Push the file mtime forward so the cached stat data is stale.
    let file = fs::File::options()
        .write(true)
        .open(repo.file_path("content/article.md"))
        .expect("the tracked file can be opened");
    file.set_modified(SystemTime::now() + Duration::from_secs(120))
        .expect("the mtime can be moved");

    let index_path = repo.path().join(".git/index");
    let before = fs::read(&index_path).expect("the index can be read");

    let adapter = adapter_for(&repo);
    let stdout = adapter
        .run_ok(Mode::Query, ["status", "--porcelain"])
        .expect("the bounded query succeeds");
    assert!(
        stdout.is_empty(),
        "the content is unchanged, only the stat data is stale"
    );

    let after_query = fs::read(&index_path).expect("the index can be read");
    assert_eq!(
        before, after_query,
        "a query must not rewrite the real index (GIT_OPTIONAL_LOCKS=0)"
    );

    // Control: the same status without GIT_OPTIONAL_LOCKS=0 rewrites the
    // index, so the assertion above really detects optional writes.
    let control = git_without(
        &repo,
        repo.path(),
        &["status", "--porcelain"],
        &["GIT_OPTIONAL_LOCKS"],
    );
    assert!(control.status.success());
    let after_control = fs::read(&index_path).expect("the index can be read");
    assert_ne!(
        before, after_control,
        "the control must show that plain status refreshes the index"
    );
}

#[test]
fn changed_path_listings_report_without_rewriting_the_index() {
    for format in [HarnessFormat::Sha1, HarnessFormat::Sha256] {
        let repo = TestRepo::init_with(RepoOptions::new().with_object_format(format));
        repo.write_file("spa ce.md", b"v1\n");
        repo.write_file("new\nline.md", b"v1\n");
        let base = repo.commit_all("first pass");

        // One modified tracked name, one new untracked name: only the
        // modified one differs from the index, because `diff-files` never
        // lists untracked paths (SPEC 6.3 status keeps untracked paths a
        // separate question).
        repo.write_file("spa ce.md", b"v2\n");
        repo.write_file("ignored by index.md", b"new\n");
        let adapter = adapter_for(&repo);
        let index_path = repo.path().join(".git/index");
        let before = fs::read(&index_path).expect("the index can be read");
        assert_eq!(
            adapter.changed_against_index().unwrap(),
            vec![OsString::from("spa ce.md")]
        );

        // The working-tree comparison runs in query mode, so the stat cache
        // is read and never rewritten, even for the changed path.
        assert_eq!(
            fs::read(&index_path).expect("the index can be read"),
            before,
            "diff-files must not refresh the index (GIT_OPTIONAL_LOCKS=0)"
        );

        // The committed comparison names every changed path without rename
        // detection: a rename is a deletion plus an addition (SPEC 3.2
        // rule 6).
        let target = repo.commit_all("second pass");
        let mut changed = adapter.changed_between_commits(&base, &target).unwrap();
        changed.sort();
        assert_eq!(
            changed,
            vec![
                OsString::from("ignored by index.md"),
                OsString::from("spa ce.md")
            ]
        );
        // Equal trees compare equal, whatever the object format is.
        assert!(
            adapter
                .changed_between_commits(&target, &target)
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn both_object_formats_report_and_resolve() {
    for format in [HarnessFormat::Sha1, HarnessFormat::Sha256] {
        let repo = TestRepo::init_with(RepoOptions::new().with_object_format(format));
        repo.write_file("spa ce.md", b"body\n");
        let head = repo.commit_all("first pass");

        let adapter = adapter_for(&repo);
        let expected = match format {
            HarnessFormat::Sha1 => ObjectFormat::Sha1,
            HarnessFormat::Sha256 => ObjectFormat::Sha256,
        };
        assert_eq!(adapter.object_format().ok(), Some(expected));

        // Revision resolution uses the end-of-options boundary and validates
        // the full object ID without assuming one width (SPEC 7.1).
        let oid = adapter
            .resolve_commit(head.as_ref())
            .expect("HEAD resolves to a commit");
        let width = match format {
            HarnessFormat::Sha1 => 40,
            HarnessFormat::Sha256 => 64,
        };
        assert_eq!(oid.len(), width);
        assert!(
            oid.bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        );
        assert_eq!(oid, head);

        // A tree is not a commit, and a dash-leading string is neither an
        // option nor a revision.
        assert!(matches!(
            adapter.resolve_commit(OsString::from("HEAD^{tree}").as_os_str()),
            Err(GitError::Exit { .. })
        ));
        assert!(matches!(
            adapter.resolve_commit(OsString::from("-nonsense").as_os_str()),
            Err(GitError::Exit { .. })
        ));

        // The NUL staging protocol also works on SHA-256 repositories.
        repo.write_file("spa ce.md", b"changed\n");
        let staged: Vec<OsString> = vec![OsString::from("spa ce.md")];
        assert_eq!(
            adapter.stage_paths(&staged).ok(),
            Some(StageOutcome::Staged)
        );
        let after = adapter_index(&adapter);
        assert_eq!(after, oracle_index(&repo));
        let entry = &after[&OsString::from("spa ce.md")];
        assert_eq!(entry.0, "100644");
        assert_eq!(
            entry.1.len(),
            width,
            "the object ID width follows the format"
        );
    }
}

#[test]
fn time_budgets_kill_a_hung_child() {
    let repo = TestRepo::init();
    let program = fake_git(&repo, "git", "exec sleep 30");
    let adapter = Git::from_env(repo.env().vars().clone())
        .with_program(program)
        .with_limits(limits(300, 64 * 1024));

    let started = Instant::now();
    let error = adapter
        .run(Mode::Query, ["--version"])
        .expect_err("a hung child must fail");
    let elapsed = started.elapsed();
    assert!(
        matches!(error, GitError::Timeout { .. }),
        "expected a timeout, found {error:?}"
    );
    assert_eq!(error.public_code(), "OUTCOME_UNKNOWN");
    assert!(!error.retryable());
    assert!(
        elapsed < Duration::from_secs(10),
        "the kill happened near the budget, not after the sleep"
    );
}

#[test]
fn output_is_bounded() {
    let repo = TestRepo::init();

    // 256 KiB of output against a 64 KiB limit is a typed failure.
    let flood = fake_git(&repo, "git", "head -c 262144 /dev/zero");
    let adapter = Git::from_env(repo.env().vars().clone())
        .with_program(flood)
        .with_limits(limits(10_000, 64 * 1024));
    let error = adapter
        .run(Mode::Query, ["--version"])
        .expect_err("a flood must fail");
    match &error {
        GitError::OutputLimit { limit, .. } => assert_eq!(*limit, 64 * 1024),
        other => panic!("expected an output limit, found {other:?}"),
    }
    assert_eq!(error.public_code(), "LIMIT_EXCEEDED");

    // Exactly the limit still succeeds: the bound is inclusive.
    let edge = fake_git(&repo, "git", "head -c 65536 /dev/zero");
    let adapter = Git::from_env(repo.env().vars().clone())
        .with_program(edge)
        .with_limits(limits(10_000, 64 * 1024));
    let output = adapter
        .run(Mode::Query, ["--version"])
        .expect("the exact limit succeeds");
    assert_eq!(output.stdout.len(), 64 * 1024);
}

#[test]
fn malformed_git_output_is_typed() {
    let repo = TestRepo::init();

    let garbage = fake_git(
        &repo,
        "git",
        "case \"$1\" in
           ls-files) printf 'garbage record without a tab\\n' ;;
           rev-parse) printf 'not-an-oid\\n' ;;
         esac",
    );
    let adapter = Git::from_env(repo.env().vars().clone())
        .at(repo.path())
        .with_program(garbage);
    let error = adapter
        .ls_files_stage()
        .expect_err("a tabless record cannot parse");
    assert!(
        matches!(error, GitError::MalformedOutput { .. }),
        "expected malformed output, found {error:?}"
    );
    assert_eq!(error.public_code(), "CONTRACT_VIOLATION");

    let error = adapter
        .resolve_commit(OsString::from("HEAD").as_os_str())
        .expect_err("a non-hex object ID cannot parse");
    assert!(matches!(error, GitError::MalformedOutput { .. }));

    // A structurally valid record with a non-lowercase object ID is also
    // malformed: OIDs are full lowercase hex (SPEC 7.1).
    let upper = fake_git(
        &repo,
        "git",
        "printf '100644 ABCDEF0123456789ABCDEF0123456789ABCDEF01 0\\tpath.md\\0'",
    );
    let adapter = Git::from_env(repo.env().vars().clone())
        .at(repo.path())
        .with_program(upper);
    assert!(matches!(
        adapter.ls_files_stage(),
        Err(GitError::MalformedOutput { .. })
    ));
}

#[test]
fn child_environment_is_fixed_by_mode() {
    let repo = TestRepo::init();
    let reporter = fake_git(&repo, "git", "env");
    // The harness itself pins GIT_OPTIONAL_LOCKS for hermetic Git runs, so
    // remove it here: the test must observe what the adapter adds, not what
    // the test environment already carries.
    let mut vars = repo.env().vars().clone();
    vars.remove(&OsString::from("GIT_OPTIONAL_LOCKS"));
    let dump = |mode: Mode| {
        let adapter = Git::from_env(vars.clone()).with_program(reporter.clone());
        let output = adapter
            .run(mode, ["--version"])
            .expect("the env dump succeeds");
        String::from_utf8(output.stdout)
            .expect("the dump is UTF-8")
            .lines()
            .map(|line| line.split_once('=').expect("one KEY=VALUE per line"))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect::<BTreeMap<_, _>>()
    };

    let query = dump(Mode::Query);
    assert_eq!(
        query.get("GIT_OPTIONAL_LOCKS").map(String::as_str),
        Some("0")
    );
    assert_eq!(
        query.get("GIT_NO_LAZY_FETCH").map(String::as_str),
        Some("1")
    );
    assert_eq!(
        query.get("GIT_LITERAL_PATHSPECS").map(String::as_str),
        Some("1")
    );
    assert_eq!(
        query.get("GIT_TERMINAL_PROMPT").map(String::as_str),
        Some("0")
    );
    assert_eq!(
        query.get("HOME").map(String::as_str),
        Some(repo.home().to_str().expect("the test home is UTF-8")),
        "the ordinary environment stays in force"
    );

    let mutate = dump(Mode::Mutate);
    assert!(
        !mutate.contains_key("GIT_OPTIONAL_LOCKS"),
        "only queries pin GIT_OPTIONAL_LOCKS"
    );
    assert_eq!(
        mutate.get("GIT_NO_LAZY_FETCH").map(String::as_str),
        Some("1")
    );
    assert_eq!(
        mutate.get("GIT_LITERAL_PATHSPECS").map(String::as_str),
        Some("1")
    );
}

#[test]
fn stdin_is_closed_unless_a_protocol_feeds_it() {
    let repo = TestRepo::init();
    let program = fake_git(
        &repo,
        "git",
        "if read -r line; then echo \"got:$line\"; else echo eof; fi",
    );
    let adapter = Git::from_env(repo.env().vars().clone()).with_program(program);

    let plain = adapter
        .run(Mode::Query, ["--version"])
        .expect("the plain call succeeds");
    assert_eq!(String::from_utf8_lossy(&plain.stdout).trim(), "eof");

    let fed = adapter
        .run_stdin(Mode::Query, ["--version"], b"hello\n")
        .expect("the fed call succeeds");
    assert_eq!(String::from_utf8_lossy(&fed.stdout).trim(), "got:hello");
}

#[test]
fn inherited_redirection_is_refused_and_overrides_are_explicit() {
    let repo = TestRepo::init();
    repo.write_file("content/article.md", b"body\n");
    repo.commit_all("first pass");

    let mut vars = repo.env().vars().clone();
    vars.insert(OsString::from("GIT_DIR"), OsString::from("/somewhere/.git"));
    let adapter = Git::from_env(vars).at(repo.path());
    let error = adapter
        .run_ok(Mode::Query, ["rev-parse", "HEAD"])
        .expect_err("redirection is refused");
    match &error {
        GitError::UnexpectedEnvironment { vars } => {
            assert_eq!(vars, &vec![OsString::from("GIT_DIR")]);
        }
        other => panic!("unexpected error {other:?}"),
    }
    assert_eq!(error.public_code(), "UNSUPPORTED_REPOSITORY");

    // Margin's own disposable index is an explicit override, not inherited
    // redirection: write-tree reads it, and the real index stays intact.
    // Stock Git prepares a valid empty disposable index (SPEC 9.3).
    let disposable = repo.root().join("disposable-index");
    let mut prepare = Command::new("git");
    prepare
        .current_dir(repo.path())
        .args(["read-tree", "--empty"])
        .stdin(Stdio::null());
    for (key, value) in repo.env().vars() {
        prepare.env(key, value);
    }
    prepare.env("GIT_INDEX_FILE", &disposable);
    assert!(
        prepare
            .status()
            .expect("stock git can prepare an empty index")
            .success(),
        "the disposable index could not be prepared"
    );
    let index_path = repo.path().join(".git/index");
    let before = fs::read(&index_path).expect("the real index can be read");

    let mut adapter = adapter_for(&repo);
    adapter.set_env_override("GIT_INDEX_FILE", disposable.as_os_str().to_os_string());
    let empty_tree = adapter
        .run_ok(Mode::Query, ["write-tree"])
        .expect("the disposable index writes the empty tree");
    let expected = support::text(&repo.git_stdin(["hash-object", "-t", "tree", "--stdin"], b""));
    assert_eq!(support::text(&empty_tree), expected);

    let after = fs::read(&index_path).expect("the real index can be read");
    assert_eq!(
        before, after,
        "a disposable index must never alias the real one"
    );
}

#[test]
fn nonzero_exits_are_typed() {
    let repo = TestRepo::init();
    let adapter = adapter_for(&repo);

    let error = adapter
        .run_ok(
            Mode::Query,
            [
                "rev-parse",
                "--verify",
                "--end-of-options",
                "no-such-rev^{commit}",
            ],
        )
        .expect_err("an unknown revision is a nonzero exit");
    match &error {
        GitError::Exit { exit_code, .. } => assert_eq!(*exit_code, Some(128)),
        other => panic!("unexpected error {other:?}"),
    }
    assert_eq!(error.public_code(), "GIT_FAILED");

    // The raw runner reports the same exit without turning it into an error.
    let raw = adapter
        .run(
            Mode::Query,
            [
                "rev-parse",
                "--verify",
                "--end-of-options",
                "no-such-rev^{commit}",
            ],
        )
        .expect("the raw runner returns the output");
    assert_eq!(raw.exit_code, Some(128));
    assert!(!raw.success());
    assert!(!raw.stderr.is_empty());
}

#[test]
fn lazy_fetch_is_disabled_in_partial_clones() {
    let repo =
        TestRepo::init_with(RepoOptions::new().with_local_config("uploadpack.allowFilter", "true"));
    repo.write_file("content.md", b"body\n");
    repo.write_file("other.md", b"other\n");
    repo.commit_all("first pass");
    let blob = support::text(&repo.git_ok(["rev-parse", "HEAD:content.md"]));

    let destination = repo.root().join("partial");
    let url = format!("file://{}", repo.path().display());
    let clone = git_without(
        &repo,
        repo.root(),
        &[
            "clone",
            "--quiet",
            "--no-checkout",
            "--no-local",
            "--filter=blob:none",
            &url,
            destination.to_str().expect("the destination is UTF-8"),
        ],
        &[],
    );
    assert!(
        clone.status.success(),
        "the partial clone failed: {}",
        String::from_utf8_lossy(&clone.stderr)
    );

    // The adapter must refuse the missing object instead of fetching it.
    let adapter = Git::from_env(repo.env().vars().clone()).at(&destination);
    let error = adapter
        .run_ok(Mode::Query, ["cat-file", "blob", blob.as_str()])
        .expect_err("a promisor blob must not be fetched implicitly");
    assert!(matches!(error, GitError::Exit { .. }), "found {error:?}");

    // Control: without GIT_NO_LAZY_FETCH the same read fetches on demand,
    // so the assertion above really pins the disabled lazy fetch.
    let control = git_without(
        &repo,
        &destination,
        &["cat-file", "-e", blob.as_str()],
        &["GIT_NO_LAZY_FETCH"],
    );
    assert!(
        control.status.success(),
        "the control must fetch the blob on demand: {}",
        String::from_utf8_lossy(&control.stderr)
    );
}

/// Builds small limits for the process seam.
fn limits(timeout_ms: u64, max_output_bytes: usize) -> margin::git::Limits {
    margin::git::Limits {
        timeout: Duration::from_millis(timeout_ms),
        max_output_bytes,
    }
}

#[test]
fn version_reporting_matches_the_harness_record() {
    let repo = TestRepo::init();
    let adapter = adapter_for(&repo);
    let version = adapter
        .git_version()
        .expect("the installed Git reports a version");
    assert_eq!(version, support::environment_report().git_version);
}
