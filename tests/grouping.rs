//! Real-Git tests for the safe Markdown passage grouping (SPEC 10.2-10.5,
//! task T024).
//!
//! The C0 corpus of SPEC 16.7 is the oracle: its recorded groupings and
//! whole-file fallbacks were authored independently of this code, and every
//! case runs through the installed Git. These tests prove that
//!
//! - every fixture's grouping reproduces the reviewed expectation: the
//!   same passages of complete hunks, or the same whole-file fallback;
//! - each issued group patch restores exactly the recorded per-group bytes
//!   through stock Git, independent of Margin's own validation;
//! - the manifest, review, and change identities derive from the grouped
//!   patch units, arrive only after grouping, and stay stable for equal
//!   inputs;
//! - grouping is a query: refs, the index, and working files never move.

mod support;

use std::collections::BTreeMap;
use std::fs;

use margin::app::{Identities, build_change_manifest, build_grouped_manifest, resolve_scope};
use margin::domain::ScopeInput;
use margin::git::Git;
use support::c0::{self, Content, Expectation, Fixture, Side};
use support::repo::{ObjectFormat, RepoOptions, TestRepo};
use support::text;

/// Adapter bound to `repo`'s working tree and isolated environment.
fn adapter_for(repo: &TestRepo) -> Git {
    Git::from_env(repo.env().vars().clone()).at(repo.path())
}

/// Resolves one whole-repository Markdown scope.
fn scope_descriptor(repo: &TestRepo) -> margin::domain::ScopeDescriptor {
    let input = ScopeInput {
        include: vec![".".to_string()],
        exclude: Vec::new(),
        suffixes: vec![".md".to_string()],
    };
    resolve_scope(&adapter_for(repo), &input)
        .unwrap_or_else(|error| panic!("the scope must resolve: {error}"))
        .descriptor
}

/// The grouped manifest of `baseline` to `candidate` on `main`.
fn grouped_for(repo: &TestRepo, baseline: &str, candidate: &str) -> margin::app::GroupedManifest {
    build_grouped_manifest(
        &adapter_for(repo),
        "refs/heads/main",
        baseline,
        candidate,
        &scope_descriptor(repo),
    )
    .unwrap_or_else(|error| panic!("the grouped manifest must build: {error}"))
}

/// Writes one side of a fixture into the working tree.
fn write_side(repo: &TestRepo, path: &str, side: &Side) {
    let target = repo.file_path(path);
    match side {
        Side::Present { content, mode } => {
            fs::write(&target, content.bytes())
                .unwrap_or_else(|error| panic!("cannot write {}: {error}", target.display()));
            set_exec(&target, *mode);
        }
        Side::Absent => {
            if target.exists() {
                fs::remove_file(&target)
                    .unwrap_or_else(|error| panic!("cannot remove {}: {error}", target.display()));
            }
        }
    }
}

/// Applies only the executable bit Git tracks.
fn set_exec(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let metadata = fs::metadata(path).unwrap_or_else(|error| panic!("stat: {error}"));
    let mut permissions = metadata.permissions();
    let current = permissions.mode();
    permissions.set_mode((current & !0o111) | (mode & 0o111));
    fs::set_permissions(path, permissions).unwrap_or_else(|error| panic!("chmod: {error}"));
}

/// A fresh repository with the fixture pair committed as B and C.
struct FixtureRepo {
    repo: TestRepo,
    before: String,
    after: String,
}

/// Builds the fixture repository, with an out-of-scope sentinel that never
/// changes.
fn fixture_repo(fixture: &Fixture) -> FixtureRepo {
    let repo = TestRepo::init();
    repo.write_file("seed.md", b"out-of-scope sentinel for grouping\n");
    write_side(&repo, fixture.path, &fixture.before);
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "fixture before"]);
    let before = text(&repo.git_ok(["rev-parse", "HEAD"]));
    write_side(&repo, fixture.path, &fixture.after);
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "fixture after"]);
    let after = text(&repo.git_ok(["rev-parse", "HEAD"]));
    FixtureRepo {
        repo,
        before,
        after,
    }
}

/// Every corpus fixture groups exactly as reviewed (SPEC 10.3, 16.7).
#[test]
fn corpus_grouping_matches_reviewed_expectations() {
    let mut failures = Vec::new();
    for fixture in c0::iter() {
        let FixtureRepo {
            repo,
            before,
            after,
        } = fixture_repo(fixture);
        let manifest = grouped_for(&repo, &before, &after);
        if let Err(context) = check_expectation(&manifest, fixture) {
            failures.push(format!("{}: {context}", fixture.id));
        }
    }
    assert!(
        failures.is_empty(),
        "grouping did not match the reviewed corpus:\n  {}",
        failures.join("\n  ")
    );
}

/// Compares one manifest against one fixture's recorded expectation.
fn check_expectation(
    manifest: &margin::app::GroupedManifest,
    fixture: &Fixture,
) -> Result<(), String> {
    let files: BTreeMap<&str, &margin::app::GroupedFile> = manifest
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();
    let file = files
        .get(fixture.path)
        .ok_or_else(|| format!("the manifest holds no group for {:?}", fixture.path))?;
    if files.len() != 1 {
        return Err(format!(
            "the manifest holds {} files, exactly the fixture document expected",
            files.len()
        ));
    }

    match &fixture.expectation {
        Expectation::WholeFile { .. } => {
            let reason = file.whole_file_reason.as_ref().ok_or_else(|| {
                format!(
                    "expected a whole-file decision, found {} passages",
                    file.groups.len()
                )
            })?;
            if file.groups.len() != 1 {
                return Err("a whole-file decision is exactly one group".to_string());
            }
            if reason.is_empty() {
                return Err("the whole-file fallback carries no reason".to_string());
            }
            Ok(())
        }
        Expectation::Groups { groups } => {
            if file.whole_file_reason.is_some() {
                return Err(format!(
                    "expected {} independent passages, found the whole-file fallback {:?}",
                    groups.len(),
                    file.whole_file_reason
                ));
            }
            if file.groups.len() != groups.len() {
                return Err(format!(
                    "expected {} passages, found {}",
                    groups.len(),
                    file.groups.len()
                ));
            }
            for (found, expected) in file.groups.iter().zip(groups.iter()) {
                let found_hunks: Vec<usize> =
                    found.hunk_indexes.iter().map(|index| index + 1).collect();
                let expected_hunks: Vec<usize> = expected.hunks.to_vec();
                if found_hunks != expected_hunks {
                    return Err(format!(
                        "expected hunks {expected_hunks:?}, found {found_hunks:?}"
                    ));
                }
                if found.reason.is_empty() {
                    return Err("a passage carries no reason".to_string());
                }
            }
            Ok(())
        }
    }
}

/// Each issued group patch restores the recorded per-group bytes through
/// stock Git alone (SPEC 10.1, 16.4): the oracle here is `git read-tree`,
/// `git apply --cached --reverse`, and `git cat-file`, never Margin.
#[test]
fn group_patches_reproduce_recorded_restorations() {
    let mut failures = Vec::new();
    for fixture in
        c0::iter().filter(|fixture| matches!(fixture.expectation, Expectation::Groups { .. }))
    {
        let FixtureRepo {
            repo,
            before,
            after,
        } = fixture_repo(fixture);
        let manifest = grouped_for(&repo, &before, &after);
        let file = manifest
            .files
            .iter()
            .find(|file| file.path == fixture.path)
            .expect("the fixture document is grouped");
        let Expectation::Groups { groups } = &fixture.expectation else {
            unreachable!("filtered to grouped fixtures");
        };
        for (group, expected) in file.groups.iter().zip(groups.iter()) {
            let restored = stock_restore(&repo, &after, fixture.path, &group.group.patch_units[0]);
            match restored {
                Some(bytes) if bytes == expected.restore.bytes() => {}
                Some(bytes) => failures.push(format!(
                    "{}: group {:?} restored {:?}, recorded {:?}",
                    fixture.id,
                    group.hunk_indexes,
                    String::from_utf8_lossy(&bytes),
                    String::from_utf8_lossy(expected.restore.bytes())
                )),
                None => failures.push(format!(
                    "{}: group {:?} did not restore through stock Git",
                    fixture.id, group.hunk_indexes
                )),
            }
        }
    }
    assert!(
        failures.is_empty(),
        "group patches did not reproduce the recorded restorations:\n  {}",
        failures.join("\n  ")
    );
}

/// Reverse-applies one group patch onto C through the stock index and
/// returns the resulting document bytes.
fn stock_restore(repo: &TestRepo, after: &str, path: &str, patch: &[u8]) -> Option<Vec<u8>> {
    repo.git_ok(["read-tree", after]);
    repo.git_stdin(
        ["apply", "--cached", "--reverse", "--whitespace=nowarn", "-"],
        patch,
    );
    let listing = repo.git_ok(["ls-files", "-s", "--", path]);
    let line = listing.split(|&byte| byte == b'\n').next()?;
    let oid = line.split(|&byte| byte == b' ').nth(1)?;
    let oid = std::str::from_utf8(oid).ok()?.trim().to_string();
    Some(repo.git_ok(["cat-file", "blob", &oid]))
}

/// Two distant paragraph edits form two passages, and every identity
/// derives from the grouped units (SPEC 8.6, 10.2 steps 6-7).
#[test]
fn identities_arrive_after_grouping() {
    let repo = TestRepo::init();
    let document = concat!(
        "# Notes\n",
        "\n",
        "first paragraph original\n",
        "filler 01\n",
        "filler 02\n",
        "filler 03\n",
        "filler 04\n",
        "filler 05\n",
        "filler 06\n",
        "filler 07\n",
        "filler 08\n",
        "filler 09\n",
        "filler 10\n",
        "second paragraph original\n",
    );
    repo.write_file("notes.md", document.as_bytes());
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "before"]);
    let before = text(&repo.git_ok(["rev-parse", "HEAD"]));

    let edited = document
        .replace("first paragraph original", "first paragraph revised")
        .replace("second paragraph original", "second paragraph revised");
    repo.write_file("notes.md", edited.as_bytes());
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "after"]);
    let after = text(&repo.git_ok(["rev-parse", "HEAD"]));

    let manifest = grouped_for(&repo, &before, &after);
    assert_eq!(manifest.files.len(), 1);
    let file = &manifest.files[0];
    assert!(
        file.whole_file_reason.is_none(),
        "two distant edits are independent"
    );
    assert_eq!(file.groups.len(), 2);
    assert_eq!(file.groups[0].hunk_indexes, vec![0]);
    assert_eq!(file.groups[1].hunk_indexes, vec![1]);

    // The passages carry distinct change identities, and the manifest
    // digest is the digest of exactly the grouped patch units.
    assert_ne!(file.groups[0].change_id, file.groups[1].change_id);
    let units: Vec<margin::domain::ChangeGroup> =
        manifest.groups().map(|group| group.group.clone()).collect();
    let rebuilt = Identities::new(&adapter_for(&repo))
        .unwrap_or_else(|error| panic!("identities must read the repository: {error}"))
        .manifest(&units)
        .unwrap_or_else(|error| panic!("the digest must hash: {error}"));
    assert_eq!(rebuilt, manifest.manifest_digest);

    // The whole-file manifest of T023 covers one whole-file unit, so its
    // digest and review identity differ: grouping changed the actionable
    // manifest, and the identities followed it.
    let whole = build_change_manifest(
        &adapter_for(&repo),
        "refs/heads/main",
        &before,
        &after,
        &scope_descriptor(&repo),
    )
    .unwrap_or_else(|error| panic!("the whole-file manifest must build: {error}"));
    assert_ne!(manifest.manifest_digest, whole.manifest_digest);
    assert_ne!(manifest.review_id, whole.review_id);

    // Identical inputs reproduce every identity.
    let repeat = grouped_for(&repo, &before, &after);
    assert_eq!(repeat.manifest_digest, manifest.manifest_digest);
    assert_eq!(repeat.review_id, manifest.review_id);
    assert_eq!(
        repeat
            .groups()
            .map(|group| group.change_id.clone())
            .collect::<Vec<_>>(),
        manifest
            .groups()
            .map(|group| group.change_id.clone())
            .collect::<Vec<_>>()
    );

    // A changed scoped input regenerates them.
    repo.write_file(
        "notes.md",
        edited
            .replace("second paragraph revised", "second rewritten")
            .as_bytes(),
    );
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "changed"]);
    let changed = text(&repo.git_ok(["rev-parse", "HEAD"]));
    let regenerated = grouped_for(&repo, &before, &changed);
    assert_ne!(regenerated.manifest_digest, manifest.manifest_digest);
    assert_ne!(regenerated.review_id, manifest.review_id);
}

/// One long list edited at both ends forms two independent passages of
/// complete hunks, each carrying the list as its structure (SPEC 10.3).
#[test]
fn one_long_list_forms_independent_item_passages() {
    let repo = TestRepo::init();
    let items: Vec<String> = (1..=24).map(|index| format!("- item {index:02}")).collect();
    repo.write_file("list.md", format!("{}\n", items.join("\n")).as_bytes());
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "before"]);
    let before = text(&repo.git_ok(["rev-parse", "HEAD"]));

    let mut edited = items.clone();
    edited[1] = "- item 02 revised".to_string();
    edited[22] = "- item 23 revised".to_string();
    repo.write_file("list.md", format!("{}\n", edited.join("\n")).as_bytes());
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "after"]);
    let after = text(&repo.git_ok(["rev-parse", "HEAD"]));

    let manifest = grouped_for(&repo, &before, &after);
    let file = &manifest.files[0];
    assert!(file.whole_file_reason.is_none());
    // Two hunks of one list, two passages: the distant items are separate
    // decisions, each labeled by the list it belongs to (SPEC 10.3).
    assert!(file.hunks.len() >= 2, "the two edits form separate hunks");
    assert_eq!(file.groups.len(), 2);
    assert_eq!(file.groups[0].hunk_indexes, vec![0]);
    assert_eq!(file.groups[1].hunk_indexes, vec![1]);
    assert!(file.groups[0].reason.contains("list"));
}

/// Grouping is a query: refs, the index, and working files never move
/// (SPEC 7.6, 10.2).
#[test]
fn grouping_never_touches_repository_state() {
    let repo = TestRepo::init();
    repo.write_file("a.md", b"# A\n\none original line\n");
    repo.write_file("b.md", b"plain filler of b\n");
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "before"]);
    let before = text(&repo.git_ok(["rev-parse", "HEAD"]));
    repo.write_file("a.md", b"# A\n\none revised line\n");
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "after"]);
    let after = text(&repo.git_ok(["rev-parse", "HEAD"]));

    let head = text(&repo.git_ok(["rev-parse", "HEAD"]));
    let index = repo.git_ok(["ls-files", "-s"]);
    let working_a = repo.read_file("a.md").to_vec();
    let working_b = repo.read_file("b.md").to_vec();
    let status = repo.git_ok(["status", "--porcelain"]);

    let manifest = grouped_for(&repo, &before, &after);
    assert_eq!(manifest.files.len(), 1);

    assert_eq!(text(&repo.git_ok(["rev-parse", "HEAD"])), head);
    assert_eq!(repo.git_ok(["ls-files", "-s"]), index);
    assert_eq!(repo.read_file("a.md").to_vec(), working_a);
    assert_eq!(repo.read_file("b.md").to_vec(), working_b);
    assert_eq!(repo.git_ok(["status", "--porcelain"]), status);
}

/// Both documented object formats group identically (SPEC 7.1, 16.5).
#[test]
fn sha256_repositories_group_the_same_way() {
    let repo = TestRepo::init_with(RepoOptions::new().with_object_format(ObjectFormat::Sha256));
    let fixture = c0::by_id("moved-paragraph").expect("the fixture exists");
    let (before, after) = commit_fixture_in(fixture, &repo);
    let manifest = grouped_for(&repo, &before, &after);
    let file = &manifest.files[0];
    assert!(file.whole_file_reason.is_none());
    assert_eq!(file.groups.len(), 1, "the move is one coupled decision");
    assert_eq!(file.groups[0].hunk_indexes, vec![0, 1]);
    assert_eq!(
        file.groups[0].change_id.id().split(':').next(),
        Some("sha256")
    );
}

/// Revisions must be validated canonical commit identifiers (SPEC 14.1).
#[test]
fn invalid_revisions_fail_typed() {
    let repo = TestRepo::init();
    repo.write_file("doc.md", b"one\n");
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "before"]);
    let before = text(&repo.git_ok(["rev-parse", "HEAD"]));
    repo.write_file("doc.md", b"ONE\n");
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "after"]);
    let after = text(&repo.git_ok(["rev-parse", "HEAD"]));
    let scope = scope_descriptor(&repo);

    let error = build_grouped_manifest(
        &adapter_for(&repo),
        "refs/heads/main",
        "HEAD",
        &after,
        &scope,
    )
    .expect_err("a non-canonical revision must fail");
    assert!(matches!(
        error,
        margin::app::ManifestError::InvalidRevision { .. }
    ));
    assert_eq!(error.public_code(), "INVALID_ARGUMENT");

    let error = build_grouped_manifest(
        &adapter_for(&repo),
        "refs/heads/main",
        &before,
        "main",
        &scope,
    )
    .expect_err("a non-canonical revision must fail");
    assert!(matches!(
        error,
        margin::app::ManifestError::InvalidRevision { .. }
    ));
}

/// Commits one fixture inside an existing repository and returns the two
/// revisions; the sha256 case cannot use [`fixture_repo`] because that
/// always initializes a fresh sha1 repository.
fn commit_fixture_in(fixture: &Fixture, repo: &TestRepo) -> (String, String) {
    repo.write_file("seed.md", b"out-of-scope sentinel for grouping\n");
    write_side(repo, fixture.path, &fixture.before);
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "fixture before"]);
    let before = text(&repo.git_ok(["rev-parse", "HEAD"]));
    write_side(repo, fixture.path, &fixture.after);
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "fixture after"]);
    let after = text(&repo.git_ok(["rev-parse", "HEAD"]));
    (before, after)
}

/// An added file, a deleted file, a mode change, and a binary change are
/// each one whole-file decision whose presentation exposes the transition
/// (SPEC 10.3-10.4).
#[test]
fn presence_and_mode_transitions_are_whole_file() {
    let repo = TestRepo::init();
    repo.write_file("kept.md", b"stable neighbor\n");
    repo.write_file("gone.md", b"an old document\n");
    repo.write_file("flip.md", b"same bytes\n");
    repo.write_file("data.bin", b"\x00\x01old\x00\n");
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "before"]);
    let before = text(&repo.git_ok(["rev-parse", "HEAD"]));

    repo.write_file("born.md", b"a fresh document\n");
    fs::remove_file(repo.file_path("gone.md")).expect("the fixture file is removed");
    set_exec(&repo.file_path("flip.md"), 0o755);
    repo.write_file("data.bin", b"\x00\x02new\x00\n");
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "after"]);
    let after = text(&repo.git_ok(["rev-parse", "HEAD"]));

    // The Markdown scope covers only .md paths; the binary neighbor stays
    // out, which is what the suffix rules of SPEC 3.2 decide.
    let manifest = grouped_for(&repo, &before, &after);
    let files: BTreeMap<&str, &margin::app::GroupedFile> = manifest
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();
    assert_eq!(files.len(), 3, "born, gone, and flip changed");

    let born = files["born.md"];
    assert_eq!(born.kind, margin::domain::ChangeKind::Added);
    assert_eq!(born.groups.len(), 1);
    assert!(
        born.whole_file_reason
            .as_deref()
            .unwrap()
            .contains("file creation")
    );
    // The full-file preview exposes the present side.
    assert_eq!(
        born.groups[0].presentation.after.bytes,
        b"a fresh document\n"
    );
    assert!(born.groups[0].presentation.before.bytes.is_empty());

    let gone = files["gone.md"];
    assert_eq!(gone.kind, margin::domain::ChangeKind::Deleted);
    assert!(
        gone.whole_file_reason
            .as_deref()
            .unwrap()
            .contains("file deletion")
    );
    assert_eq!(
        gone.groups[0].presentation.before.bytes,
        b"an old document\n"
    );

    let flip = files["flip.md"];
    assert!(
        flip.whole_file_reason
            .as_deref()
            .unwrap()
            .contains("file-mode change")
    );
    assert_eq!(flip.mode_before.map(|mode| mode.as_str()), Some("100644"));
    assert_eq!(flip.mode_after.map(|mode| mode.as_str()), Some("100755"));
}

/// Word hints expose a link destination change and keep the label visible;
/// a whitespace-only edit stays counted and flagged (SPEC 10.4-10.5).
#[test]
fn presentation_exposes_links_and_whitespace() {
    let repo = TestRepo::init();
    let filler: Vec<&str> = vec![
        "filler 01",
        "filler 02",
        "filler 03",
        "filler 04",
        "filler 05",
        "filler 06",
        "filler 07",
        "filler 08",
        "filler 09",
        "filler 10",
    ];
    let document: Vec<String> = vec![
        "# Guide".to_string(),
        String::new(),
        "See the [changelog](/old-road.md) soon.".to_string(),
        String::new(),
        filler.join("\n"),
        String::new(),
        "verse line".to_string(),
        "next line".to_string(),
    ];
    let edited: Vec<String> = vec![
        "# Guide".to_string(),
        String::new(),
        "See the [changelog](/new-road.md) soon.".to_string(),
        String::new(),
        filler.join("\n"),
        String::new(),
        "verse line ".to_string(),
        "next line\t".to_string(),
    ];
    repo.write_file("doc.md", format!("{}\n", document.join("\n")).as_bytes());
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "before"]);
    let before = text(&repo.git_ok(["rev-parse", "HEAD"]));
    repo.write_file("doc.md", format!("{}\n", edited.join("\n")).as_bytes());
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "after"]);
    let after = text(&repo.git_ok(["rev-parse", "HEAD"]));

    let manifest = grouped_for(&repo, &before, &after);
    let file = &manifest.files[0];
    assert_eq!(
        file.groups.len(),
        2,
        "the two distant regions are independent"
    );

    let link = &file.groups[0];
    assert!(
        link.presentation.heading.contains("Guide"),
        "the section stays in the heading"
    );
    assert!(
        link.presentation.link_change,
        "the destination change is exposed"
    );
    assert_eq!(link.presentation.word_hints.len(), 1);
    let hint = &link.presentation.word_hints[0];
    assert!(
        hint.prefix.contains("[changelog]("),
        "the label stays in context"
    );
    assert_eq!(hint.removed, "old");
    assert_eq!(hint.added, "new");
    assert_eq!(
        link.presentation.before.bytes,
        b"See the [changelog](/old-road.md) soon.\n"
    );

    let verse = &file.groups[1];
    assert!(verse.presentation.whitespace_only);
    assert!(verse.presentation.invisible_change);
    // The exact bytes keep the trailing space and tab the reader cannot
    // see; the reveal helper makes them visible.
    assert_eq!(
        verse.presentation.after.bytes,
        b"verse line \nnext line\t\n"
    );
    assert_eq!(
        margin::markdown::reveal_source("next line\t"),
        "next line\u{21e5}"
    );
}

/// The recorded content classes of one opaque side: bytes that are not
/// valid UTF-8 are one whole-file decision with an exact-bytes
/// presentation (SPEC 10.4).
#[test]
fn non_utf8_content_is_opaque() {
    let repo = TestRepo::init();
    repo.write_file("doc.md", b"caf\xe9 central\n");
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "before"]);
    let before = text(&repo.git_ok(["rev-parse", "HEAD"]));
    repo.write_file("doc.md", b"caf\xe9 ouvert\n");
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "after"]);
    let after = text(&repo.git_ok(["rev-parse", "HEAD"]));

    let manifest = grouped_for(&repo, &before, &after);
    let file = &manifest.files[0];
    let reason = file.whole_file_reason.as_deref().expect("opaque content");
    assert!(reason.contains("not valid UTF-8"));
    assert_eq!(file.groups.len(), 1);
    assert_eq!(
        file.groups[0].presentation.before.bytes,
        b"caf\xe9 central\n"
    );
    assert_eq!(file.groups[0].presentation.after.bytes, b"caf\xe9 ouvert\n");
    assert!(file.groups[0].presentation.word_hints.is_empty());
}

/// Keeps the unused-content helper honest: the corpus type carries opaque
/// bytes as raw data.
#[test]
fn corpus_content_covers_opaque_bytes() {
    let fixture = c0::by_id("opaque-binary-nul").expect("the fixture exists");
    assert!(matches!(
        fixture.before,
        Side::Present {
            content: Content::Opaque(_),
            ..
        }
    ));
}
