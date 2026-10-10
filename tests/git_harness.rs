//! Self-verification of the isolated real-Git test harness (SPEC 16.3).
//!
//! These tests prove the harness itself: one fresh repository per case, a
//! test-only home and Git configuration, a deterministic fixture identity,
//! and oracles that observe refs, index, trees, raw bytes, modes, and
//! out-of-scope sentinels through stock Git.

mod support;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use support::env;
use support::oracle;
use support::repo::{ObjectFormat, RepoOptions, TestRepo};

#[test]
fn fresh_repositories_are_independent_and_unborn() {
    let repo = TestRepo::init();
    assert!(
        repo.path().join(".git").is_dir(),
        "the repository needs a .git directory"
    );
    assert_eq!(
        oracle::head_oid(&repo),
        None,
        "a new repository has no commits"
    );
    assert!(repo.git_ok(["status", "--porcelain"]).is_empty());
    assert_eq!(
        support::text(&repo.git_ok(["rev-parse", "--show-object-format"])),
        "sha1"
    );

    let other = TestRepo::init();
    assert_ne!(
        repo.path(),
        other.path(),
        "each case gets its own repository"
    );
    assert_ne!(repo.home(), other.home(), "each case gets its own home");

    let developer_home_setting = std::env::var_os("HOME").unwrap_or_default();
    let developer_home = Path::new(&developer_home_setting);
    if !developer_home.as_os_str().is_empty() {
        assert_ne!(
            repo.home(),
            developer_home,
            "the test home must not be the developer home"
        );
    }
}

#[test]
fn configuration_comes_only_from_the_test_environment() {
    let repo = TestRepo::init();
    let listing = String::from_utf8(repo.git_ok(["config", "--list", "--show-origin"]))
        .expect("the configuration listing is UTF-8");

    let root = repo.root().to_path_buf();
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.clone());
    for line in listing.lines() {
        let origin = line.split('\t').next().unwrap_or(line);
        let reported = origin
            .strip_prefix("file:")
            .unwrap_or_else(|| panic!("unexpected configuration origin {origin:?} in {line:?}"));
        // Git prints the repository-local configuration relative to the work
        // tree; resolve it before comparing with the private root.
        let path = if Path::new(reported).is_absolute() {
            PathBuf::from(reported)
        } else {
            repo.path().join(reported)
        };
        assert!(
            path.starts_with(&root) || path.starts_with(&canonical_root),
            "configuration leaked from {reported:?}"
        );
    }

    let name = support::text(&repo.git_ok(["config", "--global", "--get", "user.name"]));
    assert_eq!(name, env::FIXTURE_USER_NAME);
    let email = support::text(&repo.git_ok(["config", "--get", "user.email"]));
    assert_eq!(email, env::FIXTURE_USER_EMAIL);
    assert!(
        repo.env().global_config().is_file(),
        "the fixture identity lives in the test-only global configuration"
    );
}

#[test]
fn inherited_git_variables_are_scrubbed() {
    let home = Path::new("/test-home-does-not-exist");
    let parent: Vec<(OsString, OsString)> = vec![
        ("GIT_DIR".into(), "/somewhere/.git".into()),
        ("GIT_WORK_TREE".into(), "/somewhere".into()),
        ("GIT_INDEX_FILE".into(), "/somewhere/index".into()),
        ("GIT_OBJECT_DIRECTORY".into(), "/somewhere/objects".into()),
        ("GIT_CONFIG_COUNT".into(), "1".into()),
        ("GIT_CONFIG_KEY_0".into(), "user.name".into()),
        ("GIT_CONFIG_VALUE_0".into(), "leaked".into()),
        ("EMAIL".into(), "leaked@example.invalid".into()),
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("TZ".into(), "Europe/Paris".into()),
    ];
    let vars = env::isolated_vars(parent, home);

    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_KEY_0",
        "GIT_CONFIG_VALUE_0",
        "EMAIL",
    ] {
        assert!(
            !vars.contains_key(OsStr::new(key)),
            "{key} must not reach a child Git"
        );
    }

    let home_setting = home.as_os_str().to_owned();
    let global_config = home.join(".gitconfig").into_os_string();
    let fixture_date = OsString::from(env::FIXTURE_DATE);
    let utc = OsString::from("UTC");
    let path = OsString::from("/usr/bin:/bin");
    assert_eq!(vars.get(OsStr::new("HOME")), Some(&home_setting));
    assert_eq!(
        vars.get(OsStr::new("GIT_CONFIG_GLOBAL")),
        Some(&global_config)
    );
    assert_eq!(vars.get(OsStr::new("GIT_AUTHOR_DATE")), Some(&fixture_date));
    assert_eq!(
        vars.get(OsStr::new("GIT_COMMITTER_DATE")),
        Some(&fixture_date)
    );
    assert_eq!(vars.get(OsStr::new("TZ")), Some(&utc));
    assert_eq!(
        vars.get(OsStr::new("PATH")),
        Some(&path),
        "unrelated variables survive"
    );
}

#[test]
fn fixture_identity_and_dates_are_deterministic() {
    let build = || {
        let repo = TestRepo::init();
        repo.write_file("content/article.md", b"first line\n");
        repo.commit_all("fixture pass")
    };
    let first = build();
    let second = build();
    assert_eq!(
        first, second,
        "the same steps in two fresh repositories must produce the same commit"
    );

    let repo = TestRepo::init();
    repo.write_file("content/article.md", b"first line\n");
    repo.commit_all("fixture pass");
    let header = String::from_utf8(repo.git_ok([
        "show",
        "-s",
        "--format=%an%n%ae%n%cn%n%ce%n%aI%n%cI",
        "HEAD",
    ]))
    .expect("the commit header is UTF-8");
    let expected = [
        env::FIXTURE_USER_NAME,
        env::FIXTURE_USER_EMAIL,
        env::FIXTURE_USER_NAME,
        env::FIXTURE_USER_EMAIL,
        "2026-01-01T00:00:00+00:00",
        "2026-01-01T00:00:00+00:00",
    ];
    let lines: Vec<&str> = header.lines().collect();
    assert_eq!(lines, expected.to_vec());
}

#[test]
fn sha256_repositories_are_supported() {
    let repo = TestRepo::init_with(RepoOptions::new().with_object_format(ObjectFormat::Sha256));
    assert_eq!(
        support::text(&repo.git_ok(["rev-parse", "--show-object-format"])),
        "sha256"
    );
    repo.write_file("content/one.md", b"one\n");
    let head = repo.commit_all("sha256 pass");
    assert_eq!(head.len(), 64, "a SHA-256 object ID has 64 hex digits");
    assert!(head.bytes().all(|byte| byte.is_ascii_hexdigit()));

    let entry = oracle::ls_tree(&repo, "HEAD")
        .into_iter()
        .find(|entry| entry.path == "content/one.md")
        .expect("the document is in the tree");
    assert_eq!(entry.oid.len(), 64);
    assert_eq!(oracle::blob(&repo, &entry.oid), b"one\n");
}

#[test]
fn oracles_read_refs_index_trees_bytes_and_modes() {
    let repo = TestRepo::init();
    repo.write_file("content/article.md", b"# Title\n\nbody\n");
    repo.write_file("assets/keep.bin", &[0x00, 0x9f, 0x92, 0x96]);
    let first = repo.commit_all("first pass");

    assert_eq!(
        oracle::refs(&repo).get("refs/heads/main"),
        Some(&first),
        "the branch must point at the fixture commit"
    );

    let tree = oracle::commit_tree(&repo, &first);
    assert_eq!(oracle::index_tree(&repo).as_deref(), Some(tree.as_str()));
    oracle::assert_index_matches_tree(&repo, "HEAD");

    let entries = oracle::ls_tree(&repo, "HEAD");
    let article = entries
        .iter()
        .find(|entry| entry.path == "content/article.md")
        .expect("the article is in the tree");
    assert_eq!(article.mode, "100644");
    assert_eq!(article.kind, "blob");
    assert_eq!(oracle::blob(&repo, &article.oid), b"# Title\n\nbody\n");
    let asset = entries
        .iter()
        .find(|entry| entry.path == "assets/keep.bin")
        .expect("the binary asset is in the tree");
    assert_eq!(oracle::blob(&repo, &asset.oid), [0x00, 0x9f, 0x92, 0x96]);
    assert_eq!(repo.read_file("content/article.md"), b"# Title\n\nbody\n");

    repo.set_mode("content/article.md", 0o755);
    assert_eq!(oracle::disk_mode(&repo, "content/article.md"), 0o755);
    let second = repo.commit_all("second pass");
    assert_ne!(second, first);
    let article = oracle::ls_tree(&repo, "HEAD")
        .into_iter()
        .find(|entry| entry.path == "content/article.md")
        .expect("the article is still in the tree");
    assert_eq!(
        article.mode, "100755",
        "the executable bit must be observable"
    );
}

#[test]
fn snapshots_compare_working_bytes_and_sentinels() {
    let repo = TestRepo::init();
    repo.write_file("content/a.md", b"a\n");
    repo.write_file("outside/sentinel.txt", b"sentinel\n");
    repo.commit_all("first pass");
    let before = oracle::snapshot(&repo);

    let repeat = oracle::snapshot(&repo);
    before.assert_unchanged(&repeat, "a second snapshot of an idle repository");

    repo.write_file("content/a.md", b"changed\n");
    let scoped = oracle::snapshot(&repo);
    before.assert_out_of_scope_unchanged(&scoped, &["content/"], "a scoped edit");
    assert_eq!(
        scoped.working_diff(&before),
        vec!["content/a.md".to_string()]
    );

    repo.write_file("outside/sentinel.txt", b"tampered\n");
    repo.write_file("outside/added.txt", b"added\n");
    let tampered = oracle::snapshot(&repo);
    assert_eq!(
        scoped.working_diff(&tampered),
        vec![
            "outside/added.txt".to_string(),
            "outside/sentinel.txt".to_string()
        ],
        "out-of-scope edits and additions must be detected"
    );

    fs::remove_file(repo.file_path("outside/added.txt"))
        .expect("the added sentinel can be removed");
    let removed = oracle::snapshot(&repo);
    assert!(
        tampered
            .working_diff(&removed)
            .contains(&"outside/added.txt".to_string()),
        "an out-of-scope removal must be detected"
    );

    repo.set_mode("outside/sentinel.txt", 0o600);
    let mode_changed = oracle::snapshot(&repo);
    assert!(
        removed
            .working_diff(&mode_changed)
            .contains(&"outside/sentinel.txt".to_string()),
        "an out-of-scope mode change must be detected"
    );
}

#[test]
fn out_of_scope_tree_entries_stay_comparable() {
    let repo = TestRepo::init();
    repo.write_file("content/a.md", b"a\n");
    repo.write_file("outside/keep.txt", b"keep\n");
    let first = repo.commit_all("first pass");

    repo.write_file("content/a.md", b"b\n");
    let second = repo.commit_all("scoped change");
    oracle::assert_out_of_scope_tree_unchanged(
        &repo,
        &first,
        &second,
        &["content/"],
        "a scoped commit",
    );
    assert!(oracle::out_of_scope_tree_diff(&repo, &first, &second, &["content/"]).is_empty());

    repo.write_file("outside/keep.txt", b"changed\n");
    let third = repo.commit_all("out-of-scope change");
    let differences = oracle::out_of_scope_tree_diff(&repo, &second, &third, &["content/"]);
    assert_eq!(differences.len(), 1, "one out-of-scope entry must differ");
    assert!(
        differences[0].starts_with("outside/keep.txt:"),
        "unexpected difference entry {}",
        differences[0]
    );
}

#[test]
fn custom_configuration_variants_are_dedicated() {
    let variant = TestRepo::init_with(
        RepoOptions::new()
            .with_local_config("core.autocrlf", "input")
            .with_global_config("diff.algorithm", "patience"),
    );
    assert_eq!(
        support::text(&variant.git_ok(["config", "--local", "--get", "core.autocrlf"])),
        "input"
    );
    assert_eq!(
        support::text(&variant.git_ok(["config", "--global", "--get", "diff.algorithm"])),
        "patience"
    );

    let plain = TestRepo::init();
    assert!(
        !plain
            .git(["config", "--local", "--get", "core.autocrlf"])
            .status
            .success(),
        "a default repository must not inherit variant configuration"
    );
    assert!(
        !plain
            .git(["config", "--get", "diff.algorithm"])
            .status
            .success(),
        "a default home must not inherit variant configuration"
    );
}

#[test]
fn environment_report_records_git_and_os_versions() {
    let report = support::environment_report();
    let (major, minor, _) = env::parse_git_version(&report.git_version)
        .unwrap_or_else(|| panic!("cannot parse the Git version {:?}", report.git_version));
    assert!(
        (major, minor) >= (2, 29),
        "the harness needs Git 2.29 or newer for SHA-256 repositories, found {}",
        report.git_version
    );
    assert!(!report.os.is_empty());
    assert!(!report.kernel.is_empty());

    let directory = std::env::var_os("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"));
    let path = directory.join("margin-test-environment.json");
    env::write_environment_record(report, &path);

    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("the record can be read back"))
            .expect("the record is valid JSON");
    assert_eq!(written["schema_version"], serde_json::json!(1));
    assert_eq!(
        written["git_version"],
        serde_json::json!(report.git_version)
    );
    assert_eq!(written["os"], serde_json::json!(report.os));
    assert_eq!(written["kernel"], serde_json::json!(report.kernel));
}
