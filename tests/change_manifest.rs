//! Real-Git tests for the canonical source patch manifest (SPEC 10.1-10.2,
//! task T023).
//!
//! Every case proves one manifest rule against the installed Git:
//!
//! - the fixed diff contract captures every scoped B-to-C change:
//!   additions, deletions, mode changes, whitespace, final newlines, and
//!   opaque binary files, keeping the exact patch bytes Git emitted
//! - the patch bytes equal what stock Git prints for the same trees, so
//!   the pinned settings distort nothing
//! - odd names stay literal through the header association
//! - repository and display configuration cannot change the contract
//! - full patch bytes produce the manifest, review, and change identities,
//!   which are stable for identical inputs and regenerate when a
//!   contributing input changes
//! - a stored entry that cannot join a comparison refuses with a typed
//!   error, and a broken patch stream never becomes a manifest

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use margin::app::{Identities, build_change_manifest, resolve_scope};
use margin::domain::{self, ChangeKind, ScopeInput, SourcePatchError};
use margin::git::Git;
use support::repo::{ObjectFormat, RepoOptions, TestRepo};

/// Adapter bound to `repo`'s working tree and isolated environment.
fn adapter_for(repo: &TestRepo) -> Git {
    Git::from_env(repo.env().vars().clone()).at(repo.path())
}

/// Resolves a scope that includes one root with Markdown and binary
/// suffixes, or panics with the error.
fn scope_descriptor(repo: &TestRepo, include: &[&str]) -> domain::ScopeDescriptor {
    let input = ScopeInput {
        include: include.iter().map(|root| root.to_string()).collect(),
        exclude: Vec::new(),
        suffixes: [".md", ".bin"]
            .iter()
            .map(|suffix| suffix.to_string())
            .collect(),
    };
    resolve_scope(&adapter_for(repo), &input)
        .unwrap_or_else(|error| panic!("the scope must resolve: {error}"))
        .descriptor
}

/// The manifest of `baseline` to `candidate` on `main`, or panics.
fn manifest_for(
    repo: &TestRepo,
    baseline: &str,
    candidate: &str,
    include: &[&str],
) -> margin::app::ChangeManifest {
    build_change_manifest(
        &adapter_for(repo),
        "refs/heads/main",
        baseline,
        candidate,
        &scope_descriptor(repo, include),
    )
    .unwrap_or_else(|error| panic!("the manifest must build: {error}"))
}

/// The changes of a manifest keyed by path.
fn by_path(manifest: &margin::app::ChangeManifest) -> BTreeMap<&str, &margin::app::ScopedChange> {
    manifest
        .changes
        .iter()
        .map(|change| (change.path.as_str(), change))
        .collect()
}

/// One record of stock `git diff-tree -r --raw -z`: the independent oracle
/// for presence, modes, and object identifiers.
struct RawRecord {
    mode_before: String,
    mode_after: String,
    oid_before: String,
    oid_after: String,
    status: String,
}

/// Reads stock raw records between two commits, keyed by path.
fn raw_records(repo: &TestRepo, baseline: &str, candidate: &str) -> BTreeMap<String, RawRecord> {
    let output = repo.git_ok([
        "diff-tree",
        "-r",
        "--raw",
        "-z",
        "--no-renames",
        "--no-commit-id",
        baseline,
        candidate,
    ]);
    let fields: Vec<&[u8]> = output
        .split(|&byte| byte == 0)
        .filter(|field| !field.is_empty())
        .collect();
    let mut records = BTreeMap::new();
    for pair in fields.chunks(2) {
        let meta = String::from_utf8(pair[0].to_vec()).expect("raw metadata is ASCII");
        let path = String::from_utf8(pair[1].to_vec()).expect("fixture paths are UTF-8");
        let parts: Vec<&str> = meta.split(' ').collect();
        assert!(parts.len() >= 5, "unexpected raw record {meta:?}");
        records.insert(
            path,
            RawRecord {
                mode_before: parts[0].trim_start_matches(':').to_string(),
                mode_after: parts[1].to_string(),
                oid_before: parts[2].to_string(),
                oid_after: parts[3].to_string(),
                status: parts[4].to_string(),
            },
        );
    }
    records
}

/// Stock-Git patch bytes between two commits. The flags name the documented
/// settings the contract pins; they are ordinary Git options computed
/// outside Margin (SPEC 16.3).
fn stock_patch(repo: &TestRepo, baseline: &str, candidate: &str) -> Vec<u8> {
    repo.git_ok([
        "-c",
        "core.quotePath=false",
        "-c",
        "diff.suppressBlankEmpty=false",
        "diff-tree",
        "-r",
        "-p",
        "--binary",
        "--no-commit-id",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--no-indent-heuristic",
        "--full-index",
        "--unified=3",
        baseline,
        candidate,
        "--",
        "content",
    ])
}

/// Writes the shared fixture of the byte-equality cases into `repo` and
/// returns the baseline and candidate commits: one prose edit, one binary
/// edit, one deletion, one addition with a mode flip, and one out-of-scope
/// edit.
fn write_comparison_fixture(repo: &TestRepo) -> (String, String) {
    repo.write_file("content/prose.md", b"one\ntwo\nthree\nfour\nfive\n");
    repo.write_file("content/data.bin", b"\x00\x01old\x00\n");
    repo.write_file("content/gone.md", b"delete me\n");
    repo.write_file("outside/notes.txt", b"out of scope\n");
    let baseline = repo.commit_all("baseline");

    repo.write_file("content/prose.md", b"one\ntwo\nTHREE\nfour\nfive\nsix\n");
    repo.write_file("content/data.bin", b"\x00\x02new\x00\n");
    repo.write_file("content/mode.md", b"mode flip\n");
    repo.set_mode("content/mode.md", 0o755);
    fs::remove_file(repo.file_path("content/gone.md")).expect("the fixture file is removed");
    repo.write_file("outside/notes.txt", b"changed out of scope\n");
    let candidate = repo.commit_all("candidate");
    (baseline, candidate)
}

/// A modified paragraph forms exactly one whole-file group with exact
/// ranges and verbatim patch bytes.
#[test]
fn prose_edit_yields_one_whole_file_group() {
    let repo = TestRepo::init();
    let (baseline, candidate) = write_comparison_fixture(&repo);
    let manifest = manifest_for(&repo, &baseline, &candidate, &["content"]);

    assert_eq!(manifest.changes.len(), 4, "every scoped change is listed");
    let changes = by_path(&manifest);
    let prose = changes["content/prose.md"];
    assert_eq!(prose.kind, ChangeKind::Modified);
    assert!(!prose.opaque);
    assert_eq!(prose.mode_before, domain::EntryMode::parse("100644"));
    assert_eq!(prose.mode_after, domain::EntryMode::parse("100644"));
    assert_eq!(prose.hunks.len(), 1);
    assert_eq!(prose.hunks[0].old_start, 1);
    assert_eq!(prose.hunks[0].old_count, 5);
    assert_eq!(prose.hunks[0].new_start, 1);
    assert_eq!(prose.hunks[0].new_count, 6);
    // The group is the complete original patch of the file, kept verbatim.
    assert_eq!(prose.group.patch_units.len(), 1);
    let bytes = &prose.group.patch_units[0];
    assert!(bytes.starts_with(b"diff --git a/content/prose.md b/content/prose.md\n"));
    assert!(has_needle(bytes, b"--- a/content/prose.md\n"));
    assert!(has_needle(bytes, b"-three\n+THREE\n"));
    assert!(bytes.ends_with(b"+six\n"));
}

/// The manifest covers the same paths, modes, object identifiers, and
/// presence transitions as stock `git diff-tree --raw`.
#[test]
fn entries_match_the_raw_oracle() {
    let repo = TestRepo::init();
    let (baseline, candidate) = write_comparison_fixture(&repo);
    let manifest = manifest_for(&repo, &baseline, &candidate, &["content"]);
    let oracle = raw_records(&repo, &baseline, &candidate);

    let changes = by_path(&manifest);
    for (path, record) in &oracle {
        if !path.starts_with("content/") {
            assert!(
                !changes.contains_key(path.as_str()),
                "{path} is out of scope"
            );
            continue;
        }
        let change = &changes[path.as_str()];
        let expected_kind = match record.status.as_str() {
            "A" => ChangeKind::Added,
            "D" => ChangeKind::Deleted,
            _ => ChangeKind::Modified,
        };
        assert_eq!(change.kind, expected_kind, "kind of {path}");
        assert_eq!(
            change.mode_before,
            domain::EntryMode::parse(&record.mode_before),
            "mode in B"
        );
        assert_eq!(
            change.mode_after,
            domain::EntryMode::parse(&record.mode_after),
            "mode in C"
        );
        assert_eq!(
            change.oid_before.as_deref(),
            present_oid(&record.oid_before),
            "blob of B for {path}"
        );
        assert_eq!(
            change.oid_after.as_deref(),
            present_oid(&record.oid_after),
            "blob of C for {path}"
        );
    }
    assert_eq!(changes.len(), 4);
    assert_eq!(changes["content/gone.md"].kind, ChangeKind::Deleted);
    assert_eq!(changes["content/mode.md"].kind, ChangeKind::Added);
}

/// Every captured patch byte equals what stock Git prints for the same
/// trees: the pinned settings add nothing and remove nothing.
#[test]
fn patch_bytes_match_stock_git() {
    let repo = TestRepo::init();
    let (baseline, candidate) = write_comparison_fixture(&repo);
    let manifest = manifest_for(&repo, &baseline, &candidate, &["content"]);

    let mut concatenated = Vec::new();
    for change in &manifest.changes {
        concatenated.extend_from_slice(&change.group.patch_units[0]);
    }
    assert_eq!(concatenated, stock_patch(&repo, &baseline, &candidate));
}

/// A mode-only change is captured without hunks, with both mode lines in
/// the patch bytes.
#[test]
fn mode_only_change_is_captured() {
    let repo = TestRepo::init();
    repo.write_file("content/mode.md", b"same bytes\n");
    let baseline = repo.commit_all("baseline");
    repo.set_mode("content/mode.md", 0o755);
    let candidate = repo.commit_all("candidate");

    let manifest = manifest_for(&repo, &baseline, &candidate, &["content"]);
    assert_eq!(manifest.changes.len(), 1);
    let change = &manifest.changes[0];
    assert_eq!(change.kind, ChangeKind::Modified);
    assert_eq!(change.mode_before, domain::EntryMode::parse("100644"));
    assert_eq!(change.mode_after, domain::EntryMode::parse("100755"));
    assert_eq!(change.oid_before, change.oid_after, "the content is equal");
    assert!(change.hunks.is_empty(), "a mode-only change has no hunks");
    let bytes = &change.group.patch_units[0];
    assert!(bytes.starts_with(b"diff --git a/content/mode.md b/content/mode.md\n"));
    assert!(has_needle(bytes, b"\nold mode 100644\nnew mode 100755\n"));
    assert!(!has_needle(bytes, b"@@"));
}

/// Whitespace-only edits and final-newline changes stay byte-exact,
/// including the no-newline marker.
#[test]
fn whitespace_and_final_newline_stay_exact() {
    let repo = TestRepo::init();
    repo.write_file("content/space.md", b"alpha\nbeta\n");
    repo.write_file("content/newline.md", b"kept\nlast\n");
    let baseline = repo.commit_all("baseline");

    repo.write_file("content/space.md", b"alpha  \nbeta\t\n");
    repo.write_file("content/newline.md", b"kept\nlast");
    let candidate = repo.commit_all("candidate");

    let manifest = manifest_for(&repo, &baseline, &candidate, &["content"]);
    let changes = by_path(&manifest);
    let space = changes["content/space.md"];
    assert_eq!(space.kind, ChangeKind::Modified);
    assert!(!space.opaque);
    assert_eq!(space.hunks.len(), 1, "a whitespace edit is still a change");
    assert!(has_needle(
        &space.group.patch_units[0],
        b"-alpha\n-beta\n+alpha  \n+beta\t\n"
    ));

    let newline = changes["content/newline.md"];
    assert!(has_needle(
        &newline.group.patch_units[0],
        b"\\ No newline at end of file"
    ));
}

/// An opaque binary change is a whole-file decision: no hunks, a complete
/// binary patch in the bytes. A binary addition works the same way.
#[test]
fn opaque_binary_changes_are_whole_file() {
    let repo = TestRepo::init();
    repo.write_file("content/data.bin", b"\x00\x01old\x00\n");
    let baseline = repo.commit_all("baseline");

    repo.write_file("content/data.bin", b"\x00\x02new\x00\n");
    repo.write_file("content/born.bin", b"\xff\xfe\x00born\n");
    let candidate = repo.commit_all("candidate");

    let manifest = manifest_for(&repo, &baseline, &candidate, &["content"]);
    let changes = by_path(&manifest);
    let data = changes["content/data.bin"];
    assert_eq!(data.kind, ChangeKind::Modified);
    assert!(data.opaque);
    assert!(data.hunks.is_empty());
    let bytes = &data.group.patch_units[0];
    assert!(has_needle(bytes, b"GIT binary patch"));
    assert!(has_needle(bytes, b"literal "));

    let born = changes["content/born.bin"];
    assert_eq!(born.kind, ChangeKind::Added);
    assert!(born.opaque);
    assert!(born.mode_before.is_none());
    assert_eq!(born.mode_after, domain::EntryMode::parse("100644"));
}

/// Rename detection stays off: one rename is one deletion plus one
/// addition (SPEC 3.2 rule 6).
#[test]
fn rename_is_deletion_plus_addition() {
    let repo = TestRepo::init();
    repo.write_file("content/old.md", b"moved text\n");
    let baseline = repo.commit_all("baseline");
    repo.git_ok(["mv", "content/old.md", "content/new.md"]);
    let candidate = repo.commit_all("candidate");

    let manifest = manifest_for(&repo, &baseline, &candidate, &["content"]);
    let changes = by_path(&manifest);
    assert_eq!(manifest.changes.len(), 2);
    assert_eq!(changes["content/old.md"].kind, ChangeKind::Deleted);
    assert_eq!(changes["content/new.md"].kind, ChangeKind::Added);
    assert_eq!(
        changes["content/new.md"].oid_after, changes["content/old.md"].oid_before,
        "the moved blob is the same object"
    );
}

/// The scope decides membership: excluded roots and out-of-scope paths
/// never join the manifest (SPEC 3.2 rule 4).
#[test]
fn scope_limits_the_manifest() {
    let repo = TestRepo::init();
    repo.write_file("content/kept.md", b"one\n");
    repo.write_file("content/private/secret.md", b"two\n");
    repo.write_file("outside/other.md", b"three\n");
    let baseline = repo.commit_all("baseline");

    repo.write_file("content/kept.md", b"ONE\n");
    repo.write_file("content/private/secret.md", b"TWO\n");
    repo.write_file("outside/other.md", b"THREE\n");
    let candidate = repo.commit_all("candidate");

    let input = ScopeInput {
        include: vec!["content/".to_string()],
        exclude: vec!["content/private/".to_string()],
        suffixes: vec![".md".to_string()],
    };
    let descriptor = resolve_scope(&adapter_for(&repo), &input)
        .unwrap_or_else(|error| panic!("the scope must resolve: {error}"))
        .descriptor;
    let manifest = build_change_manifest(
        &adapter_for(&repo),
        "refs/heads/main",
        &baseline,
        &candidate,
        &descriptor,
    )
    .unwrap_or_else(|error| panic!("the manifest must build: {error}"));

    let paths: Vec<&str> = manifest
        .changes
        .iter()
        .map(|change| change.path.as_str())
        .collect();
    assert_eq!(paths, vec!["content/kept.md"]);
}

/// Identical inputs produce identical identities; a changed contributing
/// input regenerates them (SPEC 8.6).
#[test]
fn identities_are_stable_and_input_sensitive() {
    let repo = TestRepo::init();
    repo.write_file("content/one.md", b"one\n");
    repo.write_file("content/two.md", b"two\n");
    repo.write_file("outside/other.md", b"out\n");
    let baseline = repo.commit_all("baseline");
    repo.write_file("content/one.md", b"ONE\n");
    repo.write_file("content/two.md", b"TWO\n");
    let candidate = repo.commit_all("candidate");
    repo.write_file("outside/other.md", b"changed\n");
    let later = repo.commit_all("later");

    let first = manifest_for(&repo, &baseline, &candidate, &["content"]);
    let second = manifest_for(&repo, &baseline, &candidate, &["content"]);
    assert_eq!(first.manifest_digest, second.manifest_digest);
    assert_eq!(first.review_id, second.review_id);
    assert_eq!(first.changes.len(), second.changes.len());
    for (left, right) in first.changes.iter().zip(&second.changes) {
        assert_eq!(left.change_id, right.change_id);
    }

    // An out-of-scope-only advance keeps the scoped manifest digest, but
    // the review identity still covers the new candidate.
    let advanced = manifest_for(&repo, &baseline, &later, &["content"]);
    assert_eq!(advanced.manifest_digest, first.manifest_digest);
    assert_ne!(advanced.review_id, first.review_id);

    // A scoped content change regenerates both.
    repo.write_file("content/one.md", b"one changed\n");
    let changed = repo.commit_all("changed");
    let third = manifest_for(&repo, &baseline, &changed, &["content"]);
    assert_ne!(third.manifest_digest, first.manifest_digest);
    assert_ne!(third.review_id, first.review_id);

    // Distinct paths never share a change identity.
    let ids: Vec<&str> = third
        .changes
        .iter()
        .map(|change| change.change_id.oid.as_str())
        .collect();
    let mut unique = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(ids.len(), unique.len());
}

/// The manifest digest covers the full patch bytes: rebuilding the digest
/// from the stock-Git sections reproduces it, and a whitespace-only edit
/// changes it.
#[test]
fn manifest_digest_covers_full_patch_bytes() {
    let repo = TestRepo::init();
    repo.write_file("content/prose.md", b"alpha\nbeta\n");
    let baseline = repo.commit_all("baseline");
    repo.write_file("content/prose.md", b"alpha\nbeta  \n");
    let candidate = repo.commit_all("candidate");

    let manifest = manifest_for(&repo, &baseline, &candidate, &["content"]);
    let oracle = stock_patch(&repo, &baseline, &candidate);
    let sections = domain::split_patch_stream(&oracle).expect("stock output splits");
    let groups: Vec<domain::ChangeGroup> = sections
        .iter()
        .map(|section| domain::ChangeGroup {
            path: "content/prose.md".to_string(),
            patch_units: vec![section.to_vec()],
        })
        .collect();
    let rebuilt = Identities::new(&adapter_for(&repo))
        .unwrap_or_else(|error| panic!("identities must read the repository: {error}"))
        .manifest(&groups)
        .unwrap_or_else(|error| panic!("the digest must hash: {error}"));
    assert_eq!(manifest.manifest_digest, rebuilt);

    // The same rendered text with different bytes is a different manifest.
    repo.write_file("content/prose.md", b"alpha\nbeta\t\n");
    let whitespace = repo.commit_all("whitespace");
    let other = manifest_for(&repo, &baseline, &whitespace, &["content"]);
    assert_ne!(other.manifest_digest, manifest.manifest_digest);
}

/// Unusual names stay literal through the pathspec and the header
/// association (SPEC 3.3).
#[test]
fn odd_paths_stay_literal() {
    let repo = TestRepo::init();
    let names = [
        "content/has space.md",
        "content/has \"quote\".md",
        "content/back\\slash.md",
        "content/tab\tname.md",
        "content/caf\u{00e9}.md",
        "content/glob*star.md",
        "content/range[xy].md",
        "content/--dash.md",
    ];
    for name in names {
        repo.write_file(name, b"before\n");
    }
    let newline_path = repo.path().join("content").join("new\nline.md");
    fs::write(&newline_path, b"before\n").expect("the fixture file is written");
    let baseline = repo.commit_all("baseline");

    for name in names {
        repo.write_file(name, b"after\n");
    }
    fs::write(&newline_path, b"after\n").expect("the fixture file is written");
    let candidate = repo.commit_all("candidate");

    let manifest = manifest_for(&repo, &baseline, &candidate, &["content"]);
    assert_eq!(manifest.changes.len(), names.len() + 1);
    let mut captured = names.to_vec();
    captured.push("content/new\nline.md");
    for name in captured {
        let change = by_path(&manifest)[name];
        assert_eq!(change.kind, ChangeKind::Modified, "{name} is captured");
        assert!(
            change.group.patch_units[0].starts_with(&domain::expected_diff_header(name)),
            "{name}'s patch starts with its exact header"
        );
    }
}

/// Repository and display configuration cannot change the diff contract:
/// hostile local settings still produce the clean repository's bytes
/// (SPEC 10.1).
#[test]
fn hostile_configuration_cannot_change_the_contract() {
    let hostile = TestRepo::init();
    let script = hostile.root().join("fake-diff.sh");
    fs::write(&script, b"#!/bin/sh\nexit 0\n").expect("the stub driver is written");
    make_executable(&script);
    for (key, value) in [
        ("diff.mnemonicPrefix", "true"),
        ("diff.noprefix", "true"),
        ("diff.srcPrefix", "SRC/"),
        ("diff.dstPrefix", "DST/"),
        ("diff.algorithm", "minimal"),
        ("diff.context", "7"),
        ("diff.interHunkContext", "2"),
        ("diff.indentHeuristic", "false"),
        ("diff.renames", "true"),
        ("diff.suppressBlankEmpty", "true"),
        ("core.quotePath", "true"),
        ("diff.relative", "true"),
        ("diff.external", script.to_str().unwrap()),
    ] {
        hostile.set_local_config(key, value);
    }

    let clean = TestRepo::init();
    let hostile_pair = write_comparison_fixture(&hostile);
    let clean_pair = write_comparison_fixture(&clean);

    let hostile_manifest = manifest_for(&hostile, &hostile_pair.0, &hostile_pair.1, &["content"]);
    let mut hostile_patch = Vec::new();
    for change in &hostile_manifest.changes {
        hostile_patch.extend_from_slice(&change.group.patch_units[0]);
    }
    assert_eq!(
        hostile_patch,
        stock_patch(&clean, &clean_pair.0, &clean_pair.1)
    );
    assert_eq!(hostile_manifest.changes.len(), 4);

    // The pins are the only reason the bytes stay fixed: with the hostile
    // settings active, plain Git prints different bytes for a blank
    // context line and a non-ASCII name.
    hostile.write_file("content/blank.md", b"one\n\ntwo\n");
    hostile.write_file("content/caf\u{00e9}.md", b"uni\n");
    let blank_base = hostile.commit_all("blank base");
    hostile.write_file("content/blank.md", b"one\n\ntwo\nthree\n");
    hostile.write_file("content/caf\u{00e9}.md", b"UNI\n");
    let blank_edit = hostile.commit_all("blank edit");

    let plain = hostile.git_ok([
        "diff-tree",
        "-r",
        "-p",
        "--binary",
        "--no-commit-id",
        "--no-ext-diff",
        "--no-renames",
        "--full-index",
        &blank_base,
        &blank_edit,
    ]);
    let pinned = manifest_for(&hostile, &blank_base, &blank_edit, &["content"]);
    let mut pinned_bytes = Vec::new();
    for change in &pinned.changes {
        pinned_bytes.extend_from_slice(&change.group.patch_units[0]);
    }
    assert_ne!(plain, pinned_bytes, "the hostile settings change plain Git");
    assert!(
        has_needle(&plain, b"caf\\303"),
        "plain Git quotes the non-ASCII name"
    );
    assert!(
        has_needle(&plain, b"\n\n") || !has_needle(&plain, b" \n"),
        "plain Git suppresses the blank context space"
    );
    assert!(
        has_needle(
            &pinned_bytes,
            &domain::expected_diff_header("content/caf\u{00e9}.md")
        ),
        "the pinned patch keeps the name literal"
    );
    assert!(
        has_needle(&pinned_bytes, b" \n"),
        "the pinned patch keeps the blank context space"
    );
}

/// A comparison with no scoped change yields an empty manifest with a
/// stable digest and a well-formed review identity.
#[test]
fn empty_change_set_yields_empty_manifest() {
    let repo = TestRepo::init();
    repo.write_file("content/kept.md", b"stable\n");
    let baseline = repo.commit_all("baseline");
    repo.write_file("outside/other.md", b"changed\n");
    let candidate = repo.commit_all("candidate");

    let manifest = manifest_for(&repo, &baseline, &candidate, &["content"]);
    assert!(manifest.changes.is_empty());
    let empty = Identities::new(&adapter_for(&repo))
        .unwrap_or_else(|error| panic!("identities must read the repository: {error}"))
        .manifest(&[])
        .unwrap_or_else(|error| panic!("the digest must hash: {error}"));
    assert_eq!(manifest.manifest_digest, empty);
    assert_eq!(
        manifest.review_id.id().split(':').next(),
        Some("sha1"),
        "the identity names its object format"
    );

    // Baseline equal to candidate is the same empty comparison.
    let equal = manifest_for(&repo, &baseline, &baseline, &["content"]);
    assert!(equal.changes.is_empty());
    assert_ne!(
        equal.review_id, manifest.review_id,
        "C is part of the identity"
    );
}

/// Both documented object formats work (SPEC 7.1, 16.5).
#[test]
fn sha256_repositories_work() {
    let repo = TestRepo::init_with(RepoOptions::new().with_object_format(ObjectFormat::Sha256));
    let (baseline, candidate) = write_comparison_fixture(&repo);
    let manifest = manifest_for(&repo, &baseline, &candidate, &["content"]);

    assert_eq!(manifest.changes.len(), 4);
    let changes = by_path(&manifest);
    let prose = changes["content/prose.md"];
    assert_eq!(prose.kind, ChangeKind::Modified);
    assert_eq!(prose.hunks.len(), 1);
    assert_eq!(prose.change_id.id().split(':').next(), Some("sha256"));
    assert!(changes["content/data.bin"].opaque);
}

/// Revisions must be validated canonical commit identifiers (SPEC 14.1).
#[test]
fn invalid_revisions_fail_typed() {
    let repo = TestRepo::init();
    repo.write_file("content/prose.md", b"one\n");
    let baseline = repo.commit_all("baseline");
    repo.write_file("content/prose.md", b"ONE\n");
    let candidate = repo.commit_all("candidate");
    let descriptor = scope_descriptor(&repo, &["content"]);
    let git = adapter_for(&repo);

    for (bad_baseline, bad_candidate) in [(&*baseline, "main"), ("HEAD", &*candidate)] {
        let error = build_change_manifest(
            &git,
            "refs/heads/main",
            bad_baseline,
            bad_candidate,
            &descriptor,
        )
        .expect_err("a non-canonical revision must fail");
        assert!(matches!(
            error,
            margin::app::ManifestError::InvalidRevision { .. }
        ));
        assert_eq!(error.public_code(), "INVALID_ARGUMENT");
    }
}

/// A stored symbolic link at a scoped path cannot join the comparison
/// (SPEC 3.2 rule 5).
#[test]
fn stored_symlink_blocks_the_manifest() {
    let repo = TestRepo::init();
    repo.write_file("content/prose.md", b"one\n");
    let baseline = repo.commit_all("baseline");
    std::os::unix::fs::symlink("prose.md", repo.file_path("content/link.md"))
        .expect("the symlink is created");
    let candidate = repo.commit_all("candidate");

    let error = build_change_manifest(
        &adapter_for(&repo),
        "refs/heads/main",
        &baseline,
        &candidate,
        &scope_descriptor(&repo, &["content"]),
    )
    .expect_err("a stored symlink must block the manifest");
    assert_eq!(error.public_code(), "UNSUPPORTED_REPOSITORY");
}

/// More changed paths than one pathspec batch still yield one complete
/// manifest (SPEC 13.3).
#[test]
fn many_paths_batch_through_processes() {
    let repo = TestRepo::init();
    for index in 0..300 {
        repo.write_file(&format!("content/f{index:03}.md"), b"before\n");
    }
    let baseline = repo.commit_all("baseline");
    for index in 0..300 {
        repo.write_file(&format!("content/f{index:03}.md"), b"after\n");
    }
    let candidate = repo.commit_all("candidate");

    let manifest = manifest_for(&repo, &baseline, &candidate, &["content"]);
    assert_eq!(manifest.changes.len(), 300);
    assert!(manifest.changes.iter().all(|change| {
        change.kind == ChangeKind::Modified
            && change.hunks.len() == 1
            && change.hunks[0].old_count == 1
            && change.hunks[0].new_count == 1
    }));
    assert!(
        manifest
            .changes
            .windows(2)
            .all(|pair| pair[0].path < pair[1].path)
    );
}

/// The parser refuses broken streams instead of guessing (SPEC 7.5).
#[test]
fn parser_rejects_broken_streams() {
    // Content before the first header.
    assert!(matches!(
        domain::split_patch_stream(b"junk\ndiff --git a/x b/x\n"),
        Err(SourcePatchError::LeadingContent { .. })
    ));
    // No header at all.
    assert!(matches!(
        domain::parse_source_patch(b"old mode 100644\n"),
        Err(SourcePatchError::MissingHeader)
    ));
    // A rename record is outside the contract.
    let renamed = b"diff --git a/x b/x\nsimilarity index 95%\nrename from x\nrename to y\n";
    assert!(matches!(
        domain::parse_source_patch(renamed),
        Err(SourcePatchError::UnexpectedHeader { .. })
    ));
    // A mode that is not a regular file.
    let symlink_mode = b"diff --git a/x b/x\nold mode 120000\nnew mode 100644\n";
    assert!(matches!(
        domain::parse_source_patch(symlink_mode),
        Err(SourcePatchError::UnsupportedMode { .. })
    ));
    // An unparsable hunk header.
    let bad_header = b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -a,b +1,2 @@\n+x\n";
    assert!(matches!(
        domain::parse_source_patch(bad_header),
        Err(SourcePatchError::MalformedHunkHeader { .. })
    ));
    // A hunk that ends before its counts are satisfied.
    let truncated = b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n-x\n+y\n";
    assert!(matches!(
        domain::parse_source_patch(truncated),
        Err(SourcePatchError::IncompleteHunk { .. })
    ));
    // A body line that is not a hunk line.
    let stray = b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-x\nstray\n+y\n";
    assert!(matches!(
        domain::parse_source_patch(stray),
        Err(SourcePatchError::IncompleteHunk { .. })
    ));
    // A binary region without a payload block.
    let bare_binary = b"diff --git a/x b/x\nindex aaa..bbb 100644\nGIT binary patch\n";
    assert!(matches!(
        domain::parse_source_patch(bare_binary),
        Err(SourcePatchError::MalformedBinary { .. })
    ));
    // Junk after a completed hunk.
    let trailing = b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-x\n+y\nold mode 100644\n";
    assert!(matches!(
        domain::parse_source_patch(trailing),
        Err(SourcePatchError::UnexpectedHeader { .. })
    ));
}

/// A well-formed multi-hunk patch parses with exact ranges and whole-line
/// byte regions.
#[test]
fn parser_accepts_a_well_formed_patch() {
    let patch = b"diff --git a/x.md b/x.md\nindex aaa..bbb 100644\n--- a/x.md\n+++ b/x.md\n\
@@ -1,3 +1,3 @@ section\n context\n-removed\n+added\n more\n\
@@ -20,2 +20,3 @@\n tail\n+extra\n tail2\n\\ No newline at end of file\n";
    let parsed = domain::parse_source_patch(patch).expect("the patch parses");
    assert_eq!(parsed.kind, ChangeKind::Modified);
    assert!(!parsed.opaque);
    assert_eq!(parsed.bytes, patch);
    assert_eq!(parsed.hunks.len(), 2);

    let first = &parsed.hunks[0];
    assert_eq!((first.old_start, first.old_count), (1, 3));
    assert_eq!((first.new_start, first.new_count), (1, 3));
    assert_eq!(
        &patch[first.offset..first.offset + first.length],
        b"@@ -1,3 +1,3 @@ section\n context\n-removed\n+added\n more\n"
    );
    assert_eq!(patch[first.offset + first.length - 1], b'\n');

    let second = &parsed.hunks[1];
    assert_eq!((second.old_start, second.old_count), (20, 2));
    assert_eq!((second.new_start, second.new_count), (20, 3));
    assert!(patch[second.offset..].starts_with(b"@@ -20,2 +20,3 @@"));
    // The no-newline marker is part of the complete hunk.
    assert_eq!(
        &patch[second.offset..second.offset + second.length],
        b"@@ -20,2 +20,3 @@\n tail\n+extra\n tail2\n\\ No newline at end of file\n"
    );
}

/// The raw oracle marks an absent side with an all-zero object ID; the
/// manifest expresses the same absence as `None`.
fn present_oid(oid: &str) -> Option<&str> {
    (!oid.chars().all(|digit| digit == '0')).then_some(oid)
}

/// Says whether `haystack` holds `needle` as a byte substring.
fn has_needle(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Makes the stub diff driver executable.
fn make_executable(path: &Path) {
    let metadata = fs::metadata(path).expect("the stub exists");
    let mut permissions = metadata.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("the stub mode is set");
}
