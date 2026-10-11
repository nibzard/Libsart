//! Real-Git tests for the repository and storage policy guards
//! (SPEC 3.1 and 8.8, task T010).
//!
//! Every case proves one guard rule against the installed Git:
//!
//! - a clean supported repository passes every guard, in both object formats
//! - unsupported layouts return their specific blocker: bare, no working
//!   tree, linked worktrees, sparse checkout, shallow and partial clones,
//!   and a nested superproject
//! - in-progress merge, rebase, cherry-pick, revert, bisect, and sequencer
//!   states block, and the guard leaves them in place
//! - a detached `HEAD` and a staged or conflicted index block
//! - scoped index entries hidden by assume-unchanged or skip-worktree
//!   block with their own specific paths, an out-of-scope flag blocks
//!   nothing, and the inspection preserves the user's flags
//! - the identity storage profile rejects `core.autocrlf` and the `text`,
//!   `eol`, `filter`, `working-tree-encoding`, and `ident` attributes
//!   through Git's effective-attribute interface
//! - a disposable-index round trip proves that supported files stage to
//!   their raw bytes and that transformations change them
//! - the guards never touch refs, the real index, or working bytes

mod support;

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use margin::app::{
    MutationPolicyReport, PolicyBlocker, enforce_mutation_policy, resolve_scope,
    storage_policy_blockers, verify_identity_profile,
};
use margin::domain::ScopeInput;
use margin::git::Git;
use support::env::DEFAULT_BRANCH;
use support::repo::{ObjectFormat, RepoOptions, TestRepo};
use support::text;

/// Adapter bound to `repo`'s working tree and isolated environment.
fn adapter_for(repo: &TestRepo) -> Git {
    Git::from_env(repo.env().vars().clone()).at(repo.path())
}

/// Adapter bound to another directory inside the same isolated environment.
fn adapter_at(repo: &TestRepo, directory: &Path) -> Git {
    Git::from_env(repo.env().vars().clone()).at(directory)
}

/// Builds scope input from literal test values.
fn scope_input(include: &[&str], exclude: &[&str], suffixes: &[&str]) -> ScopeInput {
    ScopeInput {
        include: include.iter().map(|root| root.to_string()).collect(),
        exclude: exclude.iter().map(|root| root.to_string()).collect(),
        suffixes: suffixes.iter().map(|suffix| suffix.to_string()).collect(),
    }
}

/// Resolves the whole Markdown scope of `repo`.
fn whole_scope(repo: &TestRepo) -> margin::app::ResolvedScope {
    resolve_scope(
        &adapter_for(repo),
        &scope_input(&["content/"], &[], &[".md"]),
    )
    .unwrap_or_else(|error| panic!("the scope must resolve: {error}"))
}

/// Runs the full mutation guard and panics on inspection failure.
fn guard(git: &Git, scope: Option<&margin::app::ResolvedScope>) -> MutationPolicyReport {
    enforce_mutation_policy(git, scope)
        .unwrap_or_else(|error| panic!("the policy must inspect the repository: {error}"))
}

/// One committed Markdown fixture and an untracked companion.
fn seeded_repo() -> TestRepo {
    let repo = TestRepo::init();
    repo.write_file("content/a.md", b"alpha\r\nbeta\n");
    repo.write_file("other/outside.txt", b"outside\n");
    repo.commit_all("first pass");
    repo
}

/// Says whether `report` carries one blocker equal to `expected`.
fn carries(report: &MutationPolicyReport, expected: &PolicyBlocker) -> bool {
    report.blockers.iter().any(|blocker| blocker == expected)
}

/// Builds command arguments from literals followed by paths.
fn args(literals: &[&str], paths: &[&Path]) -> Vec<OsString> {
    literals
        .iter()
        .map(|literal| OsString::from(*literal))
        .chain(paths.iter().map(|path| path.as_os_str().to_owned()))
        .collect()
}

#[test]
fn clean_repositories_pass_every_guard() {
    for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
        let repo = TestRepo::init_with(RepoOptions::new().with_object_format(format));
        repo.write_file("content/crlf.md", b"a\r\nb\r\n");
        repo.write_file("content/no-newline.md", b"no final newline");
        repo.write_file("content/untracked.md", b"new\r\npass");
        repo.commit_all("first pass");

        let scope = whole_scope(&repo);
        let report = guard(&adapter_for(&repo), Some(&scope));
        assert!(
            report.ok(),
            "a clean {} repository must pass: {report:?}",
            format.as_str()
        );

        let round_trip = verify_identity_profile(&adapter_for(&repo), &scope)
            .expect("the identity round trip must inspect the repository");
        assert!(
            round_trip.is_empty(),
            "identity staging must preserve raw bytes: {round_trip:?}"
        );
    }
}

#[test]
fn bare_repositories_and_git_directories_block() {
    let repo = seeded_repo();

    let bare = repo.root().join("bare.git");
    repo.git_ok(args(&["init", "--quiet", "--bare"], &[&bare]));
    let report = guard(&adapter_at(&repo, &bare), None);
    assert_eq!(
        report.blockers,
        vec![PolicyBlocker::BareRepository],
        "a bare repository must block before any work-tree probe"
    );

    let git_dir = repo.path().join(".git");
    let report = guard(&adapter_at(&repo, &git_dir), None);
    assert_eq!(report.blockers, vec![PolicyBlocker::NotInWorkTree]);
}

#[test]
fn linked_worktrees_block_from_both_sides() {
    let repo = seeded_repo();
    let linked = repo.root().join("linked");
    repo.git_ok(args(
        &["worktree", "add", "--quiet", "-b", "linked"],
        &[&linked],
    ));

    for directory in [repo.path().to_path_buf(), linked] {
        let report = guard(&adapter_at(&repo, &directory), None);
        assert!(
            carries(&report, &PolicyBlocker::LinkedWorktree),
            "a linked worktree must block: {:?}",
            report.blockers
        );
    }
}

#[test]
fn sparse_checkout_blocks() {
    let repo = seeded_repo();
    repo.git_ok(["sparse-checkout", "set", "content"]);

    let report = guard(&adapter_for(&repo), None);
    assert!(
        carries(&report, &PolicyBlocker::SparseCheckout),
        "sparse checkout must block: {:?}",
        report.blockers
    );
}

#[test]
fn shallow_and_partial_clones_block() {
    let repo = seeded_repo();
    let url = format!("file://{}", repo.path().display());

    let shallow = repo.root().join("shallow");
    repo.git_ok(args(
        &["clone", "--quiet", "--depth", "1", &url],
        &[&shallow],
    ));
    let report = guard(&adapter_at(&repo, &shallow), None);
    assert_eq!(
        report.blockers,
        vec![PolicyBlocker::ShallowClone],
        "a shallow clone must block on its cut-off history"
    );

    let partial = repo.root().join("partial");
    repo.git_ok(args(
        &["clone", "--quiet", "--filter=blob:none", &url],
        &[&partial],
    ));
    let report = guard(&adapter_at(&repo, &partial), None);
    assert_eq!(
        report.blockers,
        vec![PolicyBlocker::PartialClone {
            remote: "origin".to_string()
        }],
        "a promisor remote must block as a partial clone"
    );
}

#[test]
fn a_nested_superproject_blocks() {
    let repo = seeded_repo();
    let outer = repo.root().join("outer");
    repo.git_ok(args(&["init", "--quiet"], &[&outer]));
    let inner = outer.join("inner");
    let url = format!("file://{}", repo.path().display());
    repo.git_ok(args(
        &["clone", "--quiet", "--no-hardlinks", &url],
        &[&inner],
    ));
    let in_outer = |arguments: &[&str]| -> Vec<OsString> {
        args(&["-C"], &[&outer])
            .into_iter()
            .chain(arguments.iter().map(|argument| OsString::from(*argument)))
            .collect()
    };
    repo.git_ok(in_outer(&["add", "--", "inner"]));
    repo.git_ok(in_outer(&["commit", "--quiet", "--message", "nested"]));

    let report = guard(&adapter_at(&repo, &inner), None);
    let nested = report
        .blockers
        .iter()
        .find_map(|blocker| match blocker {
            PolicyBlocker::NestedRepository { superproject } => Some(superproject.clone()),
            _ => None,
        })
        .expect("a gitlink in a superproject must block");
    assert!(
        nested.ends_with("outer"),
        "the blocker names the superproject: {nested}"
    );
}

/// Creates two branches that change the same file, so their merge,
/// cherry-pick, and rebase all conflict.
fn conflicting_branches(repo: &TestRepo) {
    repo.write_file("content/base.md", b"base\n");
    repo.commit_all("base");
    repo.git_ok(["checkout", "-q", "-b", "side"]);
    repo.write_file("content/base.md", b"side\n");
    repo.commit_all("side edit");
    repo.git_ok(["checkout", "-q", DEFAULT_BRANCH]);
    repo.write_file("content/base.md", b"main\n");
    repo.commit_all("main edit");
}

/// Says whether one in-progress-operation marker still exists after a guard.
fn marker_exists(repo: &TestRepo, marker: &str) -> bool {
    let raw = support::text(&repo.git_ok(["rev-parse", "--git-path", marker]));
    let path = Path::new(raw.as_str());
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo.path().join(path)
    };
    resolved.exists()
}

#[test]
fn an_unfinished_merge_blocks_and_survives_the_guard() {
    let repo = seeded_repo();
    conflicting_branches(&repo);
    repo.git_fail(["merge", "--no-commit", "--no-ff", "side"]);

    let scope = whole_scope(&repo);
    let report = guard(&adapter_for(&repo), Some(&scope));
    assert!(
        carries(&report, &PolicyBlocker::ActiveMerge),
        "a merge in progress must block: {:?}",
        report.blockers
    );
    assert!(
        carries(
            &report,
            &PolicyBlocker::UnresolvedConflict {
                count: 1,
                paths: vec![OsString::from("content/base.md")],
            }
        ),
        "the conflicted path must be named: {:?}",
        report.blockers
    );
    assert!(
        marker_exists(&repo, "MERGE_HEAD"),
        "the guard must leave the merge state in place"
    );
}

#[test]
fn unfinished_cherry_pick_rebase_and_revert_block() {
    let repo = seeded_repo();
    conflicting_branches(&repo);

    repo.git_fail(["cherry-pick", "side"]);
    let report = guard(&adapter_for(&repo), None);
    assert!(
        carries(&report, &PolicyBlocker::ActiveCherryPick),
        "a cherry-pick in progress must block: {:?}",
        report.blockers
    );
    assert!(marker_exists(&repo, "CHERRY_PICK_HEAD"));
    repo.git_ok(["cherry-pick", "--abort"]);

    repo.git_fail(["rebase", "side"]);
    let report = guard(&adapter_for(&repo), None);
    assert!(
        carries(&report, &PolicyBlocker::ActiveRebase),
        "a rebase in progress must block: {:?}",
        report.blockers
    );
    assert!(marker_exists(&repo, "rebase-merge"));
    repo.git_ok(["rebase", "--abort"]);

    repo.git_ok(["revert", "--no-commit", "HEAD"]);
    let report = guard(&adapter_for(&repo), None);
    assert!(
        carries(&report, &PolicyBlocker::ActiveRevert),
        "a revert in progress must block: {:?}",
        report.blockers
    );
    assert!(marker_exists(&repo, "REVERT_HEAD"));
}

#[test]
fn a_bisect_session_blocks() {
    let repo = seeded_repo();
    repo.git_ok(["bisect", "start"]);

    let report = guard(&adapter_for(&repo), None);
    assert!(
        carries(&report, &PolicyBlocker::ActiveBisect),
        "a bisect session must block: {:?}",
        report.blockers
    );
    assert!(marker_exists(&repo, "BISECT_LOG"));
}

#[test]
fn staged_changes_block_on_born_and_unborn_branches() {
    let repo = seeded_repo();
    repo.write_file("content/a.md", b"alpha\r\nchanged\n");
    repo.git_ok(["add", "--", "content/a.md"]);

    let report = guard(&adapter_for(&repo), None);
    assert_eq!(
        report.blockers,
        vec![PolicyBlocker::StagedChanges {
            count: 1,
            paths: vec![OsString::from("content/a.md")],
        }],
        "a staged change against HEAD must block with its path"
    );

    // Unborn branches compare the index against the empty tree, so any
    // staged entry blocks (SPEC 5.1).
    let unborn = TestRepo::init();
    unborn.write_file("content/first.md", b"first\n");
    unborn.git_ok(["add", "--", "content/first.md"]);
    let report = guard(&adapter_for(&unborn), None);
    assert_eq!(
        report.blockers,
        vec![PolicyBlocker::StagedChanges {
            count: 1,
            paths: vec![OsString::from("content/first.md")],
        }]
    );

    // An empty unborn repository is a supported initialization state.
    let empty = TestRepo::init();
    assert!(guard(&adapter_for(&empty), None).ok());
}

#[test]
fn a_detached_head_blocks() {
    let repo = seeded_repo();
    repo.git_ok(["checkout", "-q", "--detach"]);

    let report = guard(&adapter_for(&repo), None);
    assert_eq!(report.blockers, vec![PolicyBlocker::DetachedHead]);
}

#[test]
fn hidden_index_flags_block_scoped_mutations_only() {
    // Unsaved edits behind assume-unchanged and skip-worktree: Git itself
    // reports no change for either path, so the guard must name the flags
    // itself, one specific blocker per flag (SPEC 3.1, INV-08).
    let repo = seeded_repo();
    repo.write_file("content/b.md", b"beta\n");
    repo.commit_all("second pass");
    repo.write_file("content/a.md", b"alpha\r\nunsaved\n");
    repo.git_ok(["update-index", "--assume-unchanged", "--", "content/a.md"]);
    repo.git_ok(["update-index", "--skip-worktree", "--", "content/b.md"]);
    repo.write_file("content/b.md", b"beta revised\n");
    // Git itself confirms the suppression the flags cause.
    assert_eq!(
        text(&repo.git_ok(["status", "--porcelain"])),
        "",
        "stock Git reports no change behind either flag"
    );

    let adapter = adapter_for(&repo);
    let report = guard(&adapter, Some(&whole_scope(&repo)));
    assert_eq!(
        report.blockers,
        vec![
            PolicyBlocker::AssumeUnchangedEntries {
                count: 1,
                paths: vec![OsString::from("content/a.md")],
            },
            PolicyBlocker::SkipWorktreeEntries {
                count: 1,
                paths: vec![OsString::from("content/b.md")],
            },
        ],
        "both hiding flags block with their scoped paths"
    );
    assert!(
        !report.ok(),
        "a normal mutation may not proceed behind the flags"
    );
    let message = report.blockers[0].to_string();
    assert!(
        message.contains("assume-unchanged") && message.contains("content/a.md"),
        "the diagnosis names the flag and the path: {message}"
    );

    // A flag outside the scope hides no scoped content, so it blocks
    // nothing (SPEC 3.1: the writing scope decides what a capture must
    // see).
    repo.git_ok([
        "update-index",
        "--no-assume-unchanged",
        "--",
        "content/a.md",
    ]);
    repo.git_ok(["update-index", "--no-skip-worktree", "--", "content/b.md"]);
    repo.git_ok([
        "update-index",
        "--assume-unchanged",
        "--",
        "other/outside.txt",
    ]);
    let adapter = adapter_for(&repo);
    let report = guard(&adapter, Some(&whole_scope(&repo)));
    assert!(
        report.ok(),
        "an out-of-scope flag blocks nothing: {:?}",
        report.blockers
    );

    // The inspection preserves the user's flags exactly (SPEC 14.1).
    assert_eq!(
        text(&repo.git_ok(["ls-files", "-v", "--", "other/outside.txt"])),
        "h other/outside.txt",
        "the flag survives the guard untouched"
    );
}

#[test]
fn transformation_attributes_return_specific_blockers() {
    fn attribute_case(rule: &str, expected: PolicyBlocker) {
        let repo = seeded_repo();
        repo.write_file(".gitattributes", rule.as_bytes());
        let adapter = adapter_for(&repo);
        let scope = whole_scope(&repo);
        let blockers = storage_policy_blockers(&adapter, &scope)
            .expect("the storage policy must inspect the repository");
        assert_eq!(blockers, vec![expected], "rule: {rule}");
    }

    let path = || OsString::from("content/a.md");
    attribute_case(
        "*.md text=auto",
        PolicyBlocker::TextNormalization {
            path: path(),
            value: "auto".to_string(),
        },
    );
    attribute_case(
        "*.md text",
        PolicyBlocker::TextNormalization {
            path: path(),
            value: "set".to_string(),
        },
    );
    attribute_case(
        "*.md eol=crlf",
        PolicyBlocker::EolNormalization {
            path: path(),
            value: "crlf".to_string(),
        },
    );
    attribute_case(
        "*.md filter=margin-driver",
        PolicyBlocker::ContentFilter {
            path: path(),
            filter: "margin-driver".to_string(),
        },
    );
    attribute_case(
        "*.md working-tree-encoding=UTF-16",
        PolicyBlocker::WorkingTreeEncoding {
            path: path(),
            encoding: "UTF-16".to_string(),
        },
    );
    attribute_case("*.md ident", PolicyBlocker::IdentExpansion { path: path() });

    // Explicit non-text markings stay allowed (SPEC 8.8).
    for allowed in ["*.md -text\n", "*.md binary\n"] {
        let repo = seeded_repo();
        repo.write_file(".gitattributes", allowed.as_bytes());
        let blockers = storage_policy_blockers(&adapter_for(&repo), &whole_scope(&repo))
            .expect("the storage policy must inspect the repository");
        assert!(blockers.is_empty(), "rule {allowed:?} must stay allowed");
    }

    // Rules outside the scope do not act on scoped paths.
    let repo = seeded_repo();
    repo.write_file(".gitattributes", b"other/*.md text=auto\n");
    let blockers = storage_policy_blockers(&adapter_for(&repo), &whole_scope(&repo))
        .expect("the storage policy must inspect the repository");
    assert!(blockers.is_empty(), "an out-of-scope rule must not block");

    // .git/info/attributes is part of the effective attributes.
    let repo = seeded_repo();
    let info = repo.path().join(".git").join("info");
    fs::create_dir_all(&info).expect("the info directory can be created");
    fs::write(info.join("attributes"), b"*.md filter=info-filter\n")
        .expect("the info attributes can be written");
    let blockers = storage_policy_blockers(&adapter_for(&repo), &whole_scope(&repo))
        .expect("the storage policy must inspect the repository");
    assert_eq!(
        blockers,
        vec![PolicyBlocker::ContentFilter {
            path: OsString::from("content/a.md"),
            filter: "info-filter".to_string(),
        }]
    );
}

#[test]
fn autocrlf_conversion_blocks_and_effective_precedence_wins() {
    let repo = seeded_repo();
    // The harness global configuration sets core.autocrlf=false; a local
    // value is the effective one Git applies.
    repo.set_local_config("core.autocrlf", "input");
    let blockers = storage_policy_blockers(&adapter_for(&repo), &whole_scope(&repo))
        .expect("the storage policy must inspect the repository");
    assert_eq!(
        blockers,
        vec![PolicyBlocker::AutoCrlfConversion {
            value: "input".to_string()
        }]
    );

    repo.set_local_config("core.autocrlf", "false");
    let blockers = storage_policy_blockers(&adapter_for(&repo), &whole_scope(&repo))
        .expect("the storage policy must inspect the repository");
    assert!(blockers.is_empty(), "an effective false must stay allowed");
}

#[test]
fn identity_staging_preserves_exact_raw_bytes() {
    let repo = seeded_repo();
    repo.write_file("content/crlf.md", b"a\r\nb\r\n");
    repo.write_file("content/no-newline.md", b"no final newline");
    repo.write_file("content/untracked.md", b"untracked\r\nbody");
    let scope = whole_scope(&repo);

    let report = guard(&adapter_for(&repo), Some(&scope));
    assert!(report.ok(), "an identity repository must pass: {report:?}");
    let round_trip = verify_identity_profile(&adapter_for(&repo), &scope)
        .expect("the identity round trip must inspect the repository");
    assert!(round_trip.is_empty(), "raw bytes must stage unchanged");

    // Independent oracle: stock Git's own staging pipeline stores the same
    // bytes it reads, for CRLF, missing final newline, and untracked files.
    for relative in [
        "content/crlf.md",
        "content/no-newline.md",
        "content/untracked.md",
    ] {
        let raw = repo.git_stdin(
            ["hash-object", "-t", "blob", "--stdin"],
            &repo.read_file(relative),
        );
        let stored = support::text(&repo.git_ok(["hash-object", "--", relative]));
        assert_eq!(
            support::text(&raw),
            stored,
            "stock Git must store {relative} as its raw bytes"
        );
    }
}

#[test]
fn real_transformations_change_the_staged_bytes() {
    // A text rule converts the CRLF file, so both the attribute guard and
    // the byte round trip refuse.
    let repo = TestRepo::init();
    repo.write_file("content/crlf.md", b"a\r\nb\r\n");
    repo.commit_all("first pass");
    repo.write_file(".gitattributes", b"*.md text=auto\n");
    let scope = whole_scope(&repo);

    let blockers = storage_policy_blockers(&adapter_for(&repo), &scope)
        .expect("the storage policy must inspect the repository");
    assert_eq!(
        blockers,
        vec![PolicyBlocker::TextNormalization {
            path: OsString::from("content/crlf.md"),
            value: "auto".to_string(),
        }]
    );
    let round_trip = verify_identity_profile(&adapter_for(&repo), &scope)
        .expect("the identity round trip must inspect the repository");
    assert_eq!(
        round_trip,
        vec![PolicyBlocker::ByteTransformation {
            path: OsString::from("content/crlf.md")
        }],
        "Git's own staging must be caught changing the bytes"
    );
    // Independent oracle: stock Git stores normalized bytes for this file.
    let raw = repo.git_stdin(
        ["hash-object", "-t", "blob", "--stdin"],
        &repo.read_file("content/crlf.md"),
    );
    let stored = support::text(&repo.git_ok(["hash-object", "--", "content/crlf.md"]));
    assert_ne!(
        support::text(&raw),
        stored,
        "the oracle must confirm the transformation"
    );

    // core.autocrlf drives the same mismatch without any attribute.
    let config_repo = TestRepo::init();
    config_repo.write_file("content/crlf.md", b"a\r\nb\r\n");
    config_repo.commit_all("first pass");
    config_repo.set_local_config("core.autocrlf", "true");
    let config_scope = whole_scope(&config_repo);
    let round_trip = verify_identity_profile(&adapter_for(&config_repo), &config_scope)
        .expect("the identity round trip must inspect the repository");
    assert_eq!(
        round_trip,
        vec![PolicyBlocker::ByteTransformation {
            path: OsString::from("content/crlf.md")
        }],
        "an autocrlf conversion must change the staged bytes"
    );

    // A configured but inert normalization still violates the profile: an
    // LF-only file under text=auto blocks even though its bytes survive.
    let inert = TestRepo::init();
    inert.write_file("content/lf-only.md", b"a\nb\n");
    inert.commit_all("first pass");
    inert.write_file(".gitattributes", b"*.md text=auto\n");
    let inert_scope = whole_scope(&inert);
    let blockers = storage_policy_blockers(&adapter_for(&inert), &inert_scope)
        .expect("the storage policy must inspect the repository");
    assert_eq!(
        blockers,
        vec![PolicyBlocker::TextNormalization {
            path: OsString::from("content/lf-only.md"),
            value: "auto".to_string()
        },]
    );
    let round_trip = verify_identity_profile(&adapter_for(&inert), &inert_scope)
        .expect("the identity round trip must inspect the repository");
    assert!(
        round_trip.is_empty(),
        "an inert rule changes no bytes today; the profile guard still refuses"
    );
}

#[test]
fn the_guards_never_touch_the_repository() {
    let repo = seeded_repo();
    // Push the mtime forward so a lazy index refresh would change bytes.
    let file = fs::File::options()
        .write(true)
        .open(repo.file_path("content/a.md"))
        .expect("the tracked file can be opened");
    file.set_modified(SystemTime::now() + Duration::from_secs(120))
        .expect("the mtime can be moved");

    let scope = whole_scope(&repo);
    let index_path = repo.path().join(".git/index");
    let before = support::oracle::snapshot(&repo);
    let index_before = fs::read(&index_path).expect("the index can be read");

    let report = guard(&adapter_for(&repo), Some(&scope));
    assert!(report.ok());
    let round_trip = verify_identity_profile(&adapter_for(&repo), &scope)
        .expect("the identity round trip must inspect the repository");
    assert!(round_trip.is_empty());

    before.assert_unchanged(&support::oracle::snapshot(&repo), "policy guards");
    assert_eq!(
        fs::read(&index_path).expect("the index can be read"),
        index_before,
        "neither guard may rewrite the real index"
    );
    assert!(
        !repo.path().join(".git/index.lock").exists(),
        "no index lock may remain"
    );
}

#[test]
fn guards_report_every_blocker_at_once() {
    let repo = seeded_repo();
    // A clean no-commit merge both stages its result and leaves MERGE_HEAD,
    // so one report must name the operation and the index state together.
    repo.git_ok(["checkout", "-q", "-b", "side"]);
    repo.write_file("content/side.md", b"side\n");
    repo.commit_all("side edit");
    repo.git_ok(["checkout", "-q", DEFAULT_BRANCH]);
    repo.write_file("content/main.md", b"main\n");
    repo.commit_all("main edit");
    repo.git_ok(["merge", "--no-commit", "--no-ff", "side"]);

    let report = guard(&adapter_for(&repo), None);
    assert!(
        carries(&report, &PolicyBlocker::ActiveMerge),
        "the merge state must appear: {:?}",
        report.blockers
    );
    assert!(
        report
            .blockers
            .iter()
            .any(|blocker| matches!(blocker, PolicyBlocker::StagedChanges { count: 1, .. })),
        "the staged merge result must appear beside it: {:?}",
        report.blockers
    );
}
