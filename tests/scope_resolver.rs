//! Real-Git tests for the one scope resolver (SPEC 3.2, task T009).
//!
//! Every case proves one resolver rule against the installed Git:
//!
//! - the canonical descriptor and its Git-hashed digest are stable
//! - tracked, eligible untracked, ignored, and deleted paths are classified
//! - unusual names stay literal through argv and NUL protocols
//! - escaping, absolute, NUL, and symlinked roots are rejected as input
//! - symlinks, submodules, nested repositories, and non-UTF-8 paths inside
//!   the scope become actionable blockers instead of silent exclusions
//! - resolution only observes the repository

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::time::{Duration, SystemTime};

use margin::app::{
    ResolvedScope, ScopeBlocker, ScopeResolutionError, ScopedPathState, resolve_scope,
};
use margin::domain::{SCOPE_DIGEST_DOMAIN, ScopeInput};
use margin::git::Git;
use support::repo::{ObjectFormat, RepoOptions, TestRepo};

/// Names that stress argv, quoting, pathspec magic, and byte matching.
const UNUSUAL_NAMES: &[&str] = &[
    "-lead-dash.md",
    "--flag-like.md",
    "spa ce.md",
    "quote\"'quote.md",
    "new\nline.md",
    "tab\tname.md",
    "star*.md",
    ":(glob)**.md",
    "back\\slash.md",
    "é🦀-ünïcode.md",
    "HEAD.md",
    "semi;colon.md",
    "content/nested/deep name.md",
];

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

/// Resolves a scope and panics with the error on failure.
fn resolve(
    repo: &TestRepo,
    include: &[&str],
    exclude: &[&str],
    suffixes: &[&str],
) -> ResolvedScope {
    resolve_scope(&adapter_for(repo), &scope_input(include, exclude, suffixes))
        .unwrap_or_else(|error| panic!("the scope must resolve: {error}"))
}

/// The stock-Git object ID of `bytes`, computed outside Margin.
fn hash_object_oracle(repo: &TestRepo, bytes: &[u8]) -> String {
    support::text(&repo.git_stdin(["hash-object", "-t", "blob", "--stdin"], bytes))
}

/// NUL-delimited raw path records from stock Git output.
fn raw_records(bytes: &[u8]) -> Vec<OsString> {
    bytes
        .split(|&byte| byte == 0)
        .filter(|record| !record.is_empty())
        .map(|record| OsString::from_vec(record.to_vec()))
        .collect()
}

/// Creates a symbolic link at a repository-relative path.
fn link(repo: &TestRepo, relative: &str, target: &str) {
    let path = repo.file_path(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the link parent can be created");
    }
    symlink(target, &path).unwrap_or_else(|error| panic!("cannot link {relative}: {error}"));
}

#[test]
fn canonical_descriptor_and_digest_are_stable() {
    let repo = TestRepo::init();
    repo.write_file("content/article.md", b"body\n");
    repo.commit_all("first pass");

    let resolved = resolve(
        &repo,
        &["content/", "content/", "content/deep/", "./content"],
        &["content/private/", "content/private/x/"],
        &[".MD", ".md", ".markdown"],
    );
    // Redundant roots and suffixes collapse into one canonical descriptor
    // with the exact field order of the specification example.
    assert_eq!(
        String::from_utf8(resolved.descriptor.canonical_bytes()).unwrap(),
        "{\"v\":1,\"include\":[\"content/\"],\"exclude\":[\"content/private/\"],\
         \"suffixes\":[\".markdown\",\".md\"],\"path_policy\":\"literal-utf8-v1\",\
         \"storage_policy\":\"identity-v1\"}"
    );
    // The digest is Git's own object ID of the domain-separated bytes.
    let mut oracle_input = SCOPE_DIGEST_DOMAIN.to_vec();
    oracle_input.extend_from_slice(&resolved.descriptor.canonical_bytes());
    assert_eq!(
        resolved.digest.oid,
        hash_object_oracle(&repo, &oracle_input)
    );
    assert_eq!(
        resolved.digest.id(),
        format!("sha1:{}", resolved.digest.oid)
    );

    let repeat = resolve(
        &repo,
        &["content/"],
        &["content/private/"],
        &[".md", ".markdown"],
    );
    assert_eq!(repeat.descriptor, resolved.descriptor);
    assert_eq!(repeat.digest, resolved.digest);

    // A changed contract input changes the digest.
    let changed = resolve(&repo, &["content/"], &["content/private/"], &[".md"]);
    assert_ne!(changed.digest.oid, resolved.digest.oid);
    assert_ne!(changed.descriptor, resolved.descriptor);
}

#[test]
fn one_scope_spelled_many_ways_yields_one_digest() {
    let repo = TestRepo::init();
    repo.write_file("content/article.md", b"body\n");
    repo.commit_all("first pass");

    let mut digests = BTreeSet::new();
    for spelling in ["content", "content/", "./content/", "content//./"] {
        let resolved = resolve(&repo, &[spelling], &[], &[".md"]);
        assert_eq!(resolved.descriptor.include, vec!["content/".to_string()]);
        digests.insert(resolved.digest.oid);
    }
    assert_eq!(digests.len(), 1, "every spelling must produce one digest");

    for spelling in [".", "./"] {
        let resolved = resolve(&repo, &[spelling], &[], &[".md"]);
        assert_eq!(resolved.descriptor.include, vec!["./".to_string()]);
    }
}

#[test]
fn scoped_paths_cover_tracked_untracked_ignored_and_deleted() {
    let repo = TestRepo::init();
    repo.write_file("content/a.md", b"a\n");
    repo.write_file("content/tracked-ignored.md", b"tracked\n");
    repo.write_file("content/gone.md", b"gone\n");
    repo.write_file("content/dir/vanish.md", b"vanish\n");
    repo.write_file("content/note.txt", b"wrong suffix\n");
    repo.write_file("other/out.md", b"outside\n");
    repo.git_ok([
        "add",
        "--",
        "content/a.md",
        "content/tracked-ignored.md",
        "content/gone.md",
        "content/dir/vanish.md",
        "content/note.txt",
        "other/out.md",
    ]);
    repo.commit_all("first pass");

    // The ignore rules arrive after the files were tracked, so
    // tracked-ignored.md stays tracked while an identical untracked file
    // is excluded (SPEC 3.2 rule 3).
    repo.write_file(
        ".gitignore",
        b"content/tracked-ignored.md\ncontent/ignored-new.md\n",
    );
    repo.git_ok(["add", "--", ".gitignore"]);
    repo.commit_all("ignore rules");

    repo.write_file("content/u.md", b"untracked\n");
    repo.write_file("content/udir/u2.md", b"untracked dir\n");
    repo.write_file("content/ignored-new.md", b"ignored\n");
    fs::remove_file(repo.file_path("content/gone.md")).expect("the tracked file can be removed");
    fs::remove_dir_all(repo.file_path("content/dir")).expect("the tracked dir can be removed");

    let resolved = resolve(&repo, &["content/"], &[], &[".md"]);
    let expected: BTreeMap<OsString, ScopedPathState> = BTreeMap::from([
        (OsString::from("content/a.md"), ScopedPathState::Tracked),
        // The ignore rule arrived after tracking; the file stays in scope
        // (SPEC 3.2 rule 3).
        (
            OsString::from("content/tracked-ignored.md"),
            ScopedPathState::Tracked,
        ),
        (
            OsString::from("content/gone.md"),
            ScopedPathState::TrackedDeleted,
        ),
        (
            OsString::from("content/dir/vanish.md"),
            ScopedPathState::TrackedDeleted,
        ),
        (OsString::from("content/u.md"), ScopedPathState::Untracked),
        (
            OsString::from("content/udir/u2.md"),
            ScopedPathState::Untracked,
        ),
    ]);
    assert_eq!(
        resolved.paths, expected,
        "exactly the scoped path classes remain"
    );
    assert!(resolved.blockers.is_empty());

    // Independent oracles: stock Git listings filtered by the same rules.
    let tracked: BTreeSet<OsString> = raw_records(&repo.git_ok(["ls-files", "-z"]))
        .into_iter()
        .filter(|path| {
            let bytes = path.as_encoded_bytes();
            bytes.starts_with(b"content/") && bytes.ends_with(b".md")
        })
        .collect();
    assert_eq!(
        resolved
            .paths
            .keys()
            .filter(|path| {
                !matches!(resolved.paths.get(*path), Some(ScopedPathState::Untracked))
            })
            .cloned()
            .collect::<BTreeSet<_>>(),
        tracked,
        "the tracked side must equal stock git ls-files"
    );
    let untracked = raw_records(&repo.git_ok(["ls-files", "--others", "--exclude-standard", "-z"]));
    assert_eq!(
        untracked,
        vec![
            OsString::from("content/u.md"),
            OsString::from("content/udir/u2.md")
        ],
        "the oracle must show the ignored and suffix-filtered files excluded"
    );

    // An exact include root never force-adds an ignored untracked file.
    let exact = resolve(&repo, &["content/ignored-new.md"], &[], &[".md"]);
    assert!(exact.paths.is_empty());
    // The same root on the tracked-ignored file keeps it in scope.
    let tracked_exact = resolve(&repo, &["content/tracked-ignored.md"], &[], &[".md"]);
    assert_eq!(
        tracked_exact.paths.keys().cloned().collect::<Vec<_>>(),
        vec![OsString::from("content/tracked-ignored.md")]
    );
}

#[test]
fn oddly_named_paths_resolve_literally() {
    let repo = TestRepo::init();
    for name in UNUSUAL_NAMES {
        repo.write_file(name, b"v1\n");
    }
    repo.git_ok(["add", "--all"]);
    repo.commit_all("odd names");

    let resolved = resolve(&repo, &["./"], &[], &[".md"]);
    let expected: BTreeSet<OsString> = UNUSUAL_NAMES.iter().map(OsString::from).collect();
    assert_eq!(
        resolved.paths.keys().cloned().collect::<BTreeSet<_>>(),
        expected
    );
    assert!(
        resolved
            .paths
            .values()
            .all(|state| matches!(state, ScopedPathState::Tracked))
    );

    // An exact-file root selects one unusual name without reinterpretation.
    let exact = resolve(&repo, &["new\nline.md"], &[], &[".md"]);
    assert_eq!(
        exact.paths.keys().cloned().collect::<Vec<_>>(),
        vec![OsString::from("new\nline.md")]
    );
    // A leading dash stays data, never an option.
    let dashed = resolve(&repo, &["-lead-dash.md"], &[], &[".md"]);
    assert_eq!(
        dashed.paths.keys().cloned().collect::<Vec<_>>(),
        vec![OsString::from("-lead-dash.md")]
    );
}

#[test]
fn escaping_absolute_empty_and_nul_roots_are_rejected() {
    let repo = TestRepo::init();
    repo.write_file("content/a.md", b"a\n");
    repo.commit_all("first pass");
    let adapter = adapter_for(&repo);

    let rejects = [
        ("../outside", "escapes with .."),
        ("a/../b", "escapes from the middle"),
        ("/etc", "is absolute"),
        ("", "is empty"),
        ("a\0b", "contains NUL"),
    ];
    for (root, why) in rejects {
        let error = resolve_scope(&adapter, &scope_input(&[root], &[], &[".md"])).expect_err(why);
        assert_eq!(error.public_code(), "INVALID_ARGUMENT");
    }

    // Exclude roots face the same rules, and one bad root fails the scope.
    let error = resolve_scope(
        &adapter,
        &scope_input(&["content/"], &["../escape"], &[".md"]),
    )
    .expect_err("an escaping exclude must fail");
    assert_eq!(error.public_code(), "INVALID_ARGUMENT");

    // Non-UTF-8 input is rejected with its raw bytes.
    let bad = OsString::from_vec(b"\xff.md".to_vec());
    let error = ScopeInput::from_os(&[bad], &[], &[]).expect_err("non-UTF-8 input must fail");
    assert_eq!(
        error,
        margin::domain::ScopeError::NonUtf8 {
            kind: "include root",
            bytes: b"\xff.md".to_vec()
        }
    );

    // A scope with no include root is invalid.
    let error = resolve_scope(&adapter, &scope_input(&[], &[], &[".md"]))
        .expect_err("an empty include list must fail");
    assert!(matches!(
        error,
        ScopeResolutionError::Invalid(margin::domain::ScopeError::EmptyInclude)
    ));
}

#[test]
fn symlinked_roots_and_components_are_rejected() {
    let repo = TestRepo::init();
    repo.write_file("content/a.md", b"a\n");
    repo.write_file("target.md", b"target\n");
    repo.commit_all("first pass");
    link(&repo, "alias", "content");
    link(&repo, "link.md", "target.md");

    for root in ["alias/", "alias/a.md", "link.md"] {
        let error = resolve_scope(&adapter_for(&repo), &scope_input(&[root], &[], &[".md"]))
            .expect_err("a symlinked root must fail");
        assert!(
            matches!(
                error,
                ScopeResolutionError::Invalid(margin::domain::ScopeError::SymlinkRoot { .. })
            ),
            "unexpected error for {root}: {error}"
        );
        assert_eq!(error.public_code(), "INVALID_ARGUMENT");
    }

    // A symlinked exclude root fails the same way.
    let error = resolve_scope(
        &adapter_for(&repo),
        &scope_input(&["content/"], &["alias/"], &[".md"]),
    )
    .expect_err("a symlinked exclude must fail");
    assert!(matches!(
        error,
        ScopeResolutionError::Invalid(margin::domain::ScopeError::SymlinkRoot { .. })
    ));
}

#[test]
fn symlinks_inside_the_scope_are_blockers() {
    let repo = TestRepo::init();
    repo.write_file("content/a.md", b"a\n");
    // A tracked symbolic link has mode 120000 in the index.
    link(&repo, "content/tlink.md", "../target.md");
    repo.git_ok(["add", "--", "content/tlink.md"]);
    repo.write_file("content/sub/b.md", b"b\n");
    repo.git_ok(["add", "--", "content/a.md", "content/sub/b.md"]);
    repo.commit_all("first pass");

    // Untracked symlink, tracked symlink, and a tracked file whose ancestor
    // directory becomes a symlink: all are blockers, never scoped paths.
    link(&repo, "content/link.md", "a.md");
    fs::remove_dir_all(repo.file_path("content/sub")).expect("the tracked dir can be removed");
    link(&repo, "content/sub", "../elsewhere");

    let resolved = resolve(&repo, &["content/"], &[], &[".md"]);
    assert_eq!(
        resolved.blockers,
        vec![
            ScopeBlocker::SymlinkPath {
                path: OsString::from("content/link.md")
            },
            ScopeBlocker::SymlinkPath {
                path: OsString::from("content/sub/b.md")
            },
            ScopeBlocker::SymlinkPath {
                path: OsString::from("content/tlink.md")
            },
        ],
        "blockers are sorted by path bytes and name every symlink"
    );
    assert_eq!(
        resolved.paths.keys().cloned().collect::<Vec<_>>(),
        vec![OsString::from("content/a.md")],
        "only the regular tracked file stays actionable"
    );
}

#[test]
fn nested_repositories_and_submodules_are_blockers() {
    let repo = TestRepo::init();
    repo.write_file("content/a.md", b"a\n");
    repo.write_file("content/private/keep.md", b"keep\n");
    repo.write_file("top.md", b"top\n");
    repo.commit_all("first pass");

    // An untracked nested repository inside the scope.
    repo.git_ok(["init", "--quiet", "content/nested"]);
    // A nested repository under an excluded root is not actionable.
    repo.git_ok(["init", "--quiet", "content/private/hidden"]);
    // A gitlink in the index is how a submodule looks to stock Git.
    let head = support::text(&repo.git_ok(["rev-parse", "HEAD"]));
    repo.git_ok([
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{head},content/sub"),
    ]);
    // The include root itself is a nested repository.
    repo.git_ok(["init", "--quiet", "topnest"]);

    let resolved = resolve(&repo, &["./", "topnest/"], &["content/private/"], &[".md"]);
    assert_eq!(
        resolved.blockers,
        vec![
            ScopeBlocker::NestedRepository {
                path: OsString::from("content/nested")
            },
            ScopeBlocker::Submodule {
                path: OsString::from("content/sub")
            },
            ScopeBlocker::NestedRepository {
                path: OsString::from("topnest")
            },
        ]
    );
    // Stock Git sees the same gitlink.
    let stage = support::text(&repo.git_ok(["ls-files", "--stage", "content/sub"]));
    assert!(
        stage.starts_with("160000 "),
        "the oracle must see a gitlink: {stage}"
    );

    // Without the nested repository in scope boundaries, no blocker fires.
    let narrow = resolve(&repo, &["top.md"], &[], &[".md"]);
    assert!(narrow.blockers.is_empty());
}

#[test]
fn non_utf8_scoped_paths_are_blockers() {
    let repo = TestRepo::init();
    repo.write_file("good/g.md", b"good\n");
    repo.commit_all("first pass");

    let raw = OsString::from_vec(b"\xff.md".to_vec());
    let path = repo.path().join(&raw);
    fs::write(&path, b"opaque\n").expect("the non-UTF-8 file can be written");
    repo.git_ok(vec!["add".into(), "--".into(), raw.clone()]);

    let resolved = resolve(&repo, &["./"], &[], &[".md"]);
    assert_eq!(
        resolved.blockers,
        vec![ScopeBlocker::NonUtf8Path {
            path: b"\xff.md".to_vec()
        }]
    );
    assert_eq!(
        resolved.paths.keys().cloned().collect::<Vec<_>>(),
        vec![OsString::from("good/g.md")]
    );

    // A scope that does not cover the path needs no blocker.
    let narrow = resolve(&repo, &["good/"], &[], &[".md"]);
    assert!(narrow.blockers.is_empty());
}

#[test]
fn digests_are_object_format_qualified() {
    for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
        let repo = TestRepo::init_with(RepoOptions::new().with_object_format(format));
        repo.write_file("content/a.md", b"a\n");
        repo.commit_all("first pass");

        let resolved = resolve(&repo, &["content/"], &[], &[".md"]);
        let width = match format {
            ObjectFormat::Sha1 => 40,
            ObjectFormat::Sha256 => 64,
        };
        assert_eq!(resolved.digest.oid.len(), width);
        assert_eq!(
            resolved.digest.id(),
            format!("{}:{}", format.as_str(), resolved.digest.oid)
        );
        assert_eq!(
            resolved.digest.oid,
            hash_object_oracle(&repo, &resolved.descriptor.digest_input_bytes()),
            "the digest must be Git's own hash in this repository"
        );
        if format == ObjectFormat::Sha1 {
            let other =
                TestRepo::init_with(RepoOptions::new().with_object_format(ObjectFormat::Sha256));
            other.write_file("content/a.md", b"a\n");
            other.commit_all("first pass");
            let sha256 = resolve(&other, &["content/"], &[], &[".md"]);
            assert_ne!(sha256.digest.oid, resolved.digest.oid);
        }
    }
}

#[test]
fn resolution_observes_without_touching_the_repository() {
    let repo = TestRepo::init();
    repo.write_file("content/article.md", b"body\n");
    repo.commit_all("first pass");
    // Push the mtime forward so the cached index stat data is stale.
    let file = fs::File::options()
        .write(true)
        .open(repo.file_path("content/article.md"))
        .expect("the tracked file can be opened");
    file.set_modified(SystemTime::now() + Duration::from_secs(120))
        .expect("the mtime can be moved");

    let before = support::oracle::snapshot(&repo);
    let index_path = repo.path().join(".git/index");
    let index_before = fs::read(&index_path).expect("the index can be read");

    let resolved = resolve(&repo, &["content/"], &[], &[".md"]);
    assert_eq!(resolved.paths.len(), 1);

    before.assert_unchanged(&support::oracle::snapshot(&repo), "scope resolution");
    assert_eq!(
        fs::read(&index_path).expect("the index can be read"),
        index_before,
        "resolution must not rewrite the real index"
    );
}

#[test]
fn unborn_repositories_and_absent_roots_resolve() {
    let repo = TestRepo::init();
    repo.write_file("content/u.md", b"untracked\n");

    let resolved = resolve(&repo, &["content/"], &[], &[".md"]);
    assert_eq!(
        resolved.paths.keys().cloned().collect::<Vec<_>>(),
        vec![OsString::from("content/u.md")]
    );
    assert!(matches!(
        resolved.paths[&OsString::from("content/u.md")],
        ScopedPathState::Untracked
    ));
    assert_eq!(
        resolved.digest.oid.len(),
        40,
        "hash-object works without a single commit"
    );

    // Absent roots keep their canonical form: the user's spelling decides
    // between a future directory and an exact file path.
    let dir_spelled = resolve(&repo, &["missing/"], &[], &[".md"]);
    assert_eq!(dir_spelled.descriptor.include, vec!["missing/".to_string()]);
    assert!(dir_spelled.paths.is_empty());
    let file_spelled = resolve(&repo, &["missing"], &[], &[".md"]);
    assert_eq!(file_spelled.descriptor.include, vec!["missing".to_string()]);
    assert_ne!(dir_spelled.digest.oid, file_spelled.digest.oid);
}

#[test]
fn file_and_directory_roots_are_annotated_from_disk() {
    let repo = TestRepo::init();
    repo.write_file("README.md", b"readme\n");
    repo.write_file("content/a.md", b"a\n");
    repo.commit_all("first pass");

    // An existing file root never gains a trailing slash; an existing
    // directory root always has one, whatever the user wrote.
    let resolved = resolve(&repo, &["README.md", "content"], &[], &[".md"]);
    assert_eq!(
        resolved.descriptor.include,
        vec!["README.md".to_string(), "content/".to_string()]
    );
    assert_eq!(
        resolved.paths.keys().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from([OsString::from("README.md"), OsString::from("content/a.md")])
    );

    // A root below an existing file can never match and is rejected.
    let error = resolve_scope(
        &adapter_for(&repo),
        &scope_input(&["README.md/deeper/"], &[], &[".md"]),
    )
    .expect_err("a root below a file must fail");
    assert!(matches!(
        error,
        ScopeResolutionError::Invalid(margin::domain::ScopeError::RootBelowNonDirectory { .. })
    ));
}

#[test]
fn excluded_directories_subtract_from_the_scope() {
    let repo = TestRepo::init();
    repo.write_file("content/a.md", b"a\n");
    repo.write_file("content/private/p.md", b"p\n");
    repo.write_file("content/secret.md", b"s\n");
    repo.write_file("content/private/deep/d.md", b"d\n");
    repo.commit_all("first pass");

    let resolved = resolve(
        &repo,
        &["content/"],
        &["content/private/", "content/secret.md"],
        &[".md"],
    );
    assert_eq!(
        resolved.paths.keys().cloned().collect::<Vec<_>>(),
        vec![OsString::from("content/a.md")],
        "directory and exact-file excludes both subtract"
    );
}

/// Proves that the adapter used by the resolver reports the same top-level
/// directory stock Git uses, for repositories with unusual roots.
#[test]
fn toplevel_matches_stock_git() {
    let repo = TestRepo::init();
    repo.write_file("a.md", b"a\n");
    let adapter = adapter_for(&repo);
    let toplevel = adapter.toplevel().expect("the working tree root resolves");
    let expected = support::text(&repo.git_ok(["rev-parse", "--show-toplevel"]));
    assert_eq!(toplevel, Path::new(expected.as_str()).to_path_buf());
}
