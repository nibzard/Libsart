//! Validates the C0 Markdown correctness corpus against stock Git.
//!
//! SPEC 16.7 and 17.2 define C0 as at least 60 hand-reviewed source-pair
//! fixtures with recorded expectations. This suite checks the corpus itself,
//! not the Margin implementation:
//!
//! - size, identity, and metadata quality of the fixture set;
//! - coverage of every content class that SPEC 16.7 lists;
//! - grouping expectations that partition complete canonical hunks;
//! - whole-file fallbacks for the conservative classes of SPEC 10.3;
//! - and, through real repositories and stock Git only, that every recorded
//!   source change, keep-all result, restore-all result, single-group
//!   restoration, and group composition reproduces exactly.
//!
//! The corpus oracle never imports Margin code: the diff contract is
//! restated here, and every comparison runs through the installed Git and
//! raw disk bytes (SPEC 16.3).

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;

use support::c0::{self, Content, Expectation, FileChange, Fixture, Side};
use support::repo::TestRepo;
use support::text;

/// The fixed source-patch contract of SPEC 10.1. Duplicated from the engine
/// settings on purpose: an oracle that imports the implementation it checks
/// proves nothing (SPEC 16.3). Drift between the two lists is a
/// specification defect, not a test convenience.
const DIFF_TREE_PATCH_ARGV: &[&str] = &[
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
    "--diff-algorithm=myers",
    "--inter-hunk-context=0",
    "--full-index",
    "--src-prefix=a/",
    "--dst-prefix=b/",
    "--no-relative",
    "--unified=3",
];

/// Out-of-scope sentinel committed beside every fixture document. It never
/// changes, so every restoration must leave it byte-identical (SPEC 16.3).
const SEED_PATH: &str = "seed.md";
const SEED: &[u8] = b"out-of-scope sentinel for the C0 corpus\n";

#[test]
fn corpus_has_at_least_60_reviewed_pairs() {
    assert!(
        c0::count() >= 60,
        "SPEC 16.7 requires at least 60 fixtures; the corpus has {}",
        c0::count()
    );

    let mut ids = BTreeSet::new();
    for fixture in c0::iter() {
        assert!(
            ids.insert(fixture.id),
            "duplicate fixture id {}",
            fixture.id
        );
        assert!(!fixture.title.is_empty(), "{}: empty title", fixture.id);
        assert!(
            !fixture.classes.is_empty(),
            "{}: no content classes",
            fixture.id
        );
        assert!(!fixture.path.is_empty(), "{}: empty path", fixture.id);
        assert!(
            !fixture.path.contains('/'),
            "{}: fixture paths stay single-segment so expected trees stay flat",
            fixture.id
        );
        assert!(
            !fixture.reader.is_empty(),
            "{}: the reader-visible difference is missing",
            fixture.id
        );
        assert!(
            fixture.reader.len() >= 40,
            "{}: the reader note is too thin to review",
            fixture.id
        );
        match &fixture.expectation {
            Expectation::Groups { groups } => {
                assert!(
                    !groups.is_empty(),
                    "{}: a grouping expectation needs at least one group",
                    fixture.id
                );
                for group in *groups {
                    assert!(
                        !group.reason.is_empty(),
                        "{}: a group without a reason cannot explain its size",
                        fixture.id
                    );
                }
            }
            Expectation::WholeFile { reason } => assert!(
                !reason.is_empty(),
                "{}: a whole-file fallback needs a reason",
                fixture.id
            ),
        }
    }
}

#[test]
fn corpus_covers_every_required_class() {
    let covered: BTreeSet<&str> = c0::iter().flat_map(|f| f.classes.iter().copied()).collect();
    let vocabulary: BTreeSet<&str> = c0::REQUIRED_CLASSES
        .iter()
        .chain(c0::EXTRA_CLASSES)
        .copied()
        .collect();

    let unknown: Vec<&str> = covered.difference(&vocabulary).copied().collect();
    assert!(
        unknown.is_empty(),
        "classes outside the recorded vocabulary: {}",
        unknown.join(", ")
    );

    let missing: Vec<&str> = c0::REQUIRED_CLASSES
        .iter()
        .filter(|class| !covered.contains(**class))
        .copied()
        .collect();
    assert!(
        missing.is_empty(),
        "SPEC 16.7 content classes without a fixture: {}",
        missing.join(", ")
    );
}

#[test]
fn grouping_expectations_partition_complete_hunks() {
    for fixture in c0::iter() {
        let total = fixture.changes.hunks.len();
        let Expectation::Groups { groups } = &fixture.expectation else {
            continue;
        };
        let mut seen = BTreeSet::new();
        for group in *groups {
            assert!(
                !group.hunks.is_empty(),
                "{}: an empty group cannot be a passage",
                fixture.id
            );
            let mut previous = 0;
            for &number in group.hunks {
                assert!(
                    number >= 1 && number <= total,
                    "{}: hunk {number} outside 1..={total}",
                    fixture.id
                );
                assert!(
                    number > previous,
                    "{}: group hunks must ascend without repeats",
                    fixture.id
                );
                assert!(
                    seen.insert(number),
                    "{}: hunk {number} belongs to two groups",
                    fixture.id
                );
                previous = number;
            }
        }
        assert!(
            seen.len() == total,
            "{}: groups cover {} of {total} hunks; a group must hold complete \
             hunks and together they must cover the whole patch",
            fixture.id,
            seen.len()
        );
    }
}

#[test]
fn whole_file_fallbacks_are_recorded_for_conservative_classes() {
    // SPEC 10.3 makes reference-link and footnote relations, opaque binary
    // changes, file creation and deletion, and file-mode changes whole-file
    // decisions. The corpus must record them as such, never as passages.
    for class in [
        "reference-links",
        "footnotes",
        "opaque-bytes",
        "file-presence",
        "file-mode",
    ] {
        for fixture in c0::iter().filter(|f| f.classes.contains(&class)) {
            assert!(
                matches!(fixture.expectation, Expectation::WholeFile { .. }),
                "{}: class {class} must be a whole-file decision",
                fixture.id
            );
        }
    }
    let fallbacks = c0::iter()
        .filter(|f| matches!(f.expectation, Expectation::WholeFile { .. }))
        .count();
    assert!(
        fallbacks >= 10,
        "the corpus records only {fallbacks} whole-file fallbacks"
    );
}

#[test]
fn recorded_expectations_reproduce_through_stock_git() {
    let mut failures = Vec::new();
    for fixture in c0::iter() {
        let FixtureRepo {
            repo,
            before,
            after,
        } = fixture_repo(fixture);
        if let Err(context) = check_fixture(&repo, fixture, &before, &after) {
            failures.push(format!("{}: {context}", fixture.id));
        }
    }
    assert!(
        failures.is_empty(),
        "corpus expectations did not reproduce through stock Git:\n  {}",
        failures.join("\n  ")
    );
}

/// Writes one side of a fixture into the working tree.
fn write_side(repo: &TestRepo, path: &str, side: &Side) {
    let target = repo.file_path(path);
    match side {
        Side::Present { content, mode } => {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .unwrap_or_else(|error| panic!("cannot create {}: {error}", parent.display()));
            }
            fs::write(&target, content.bytes())
                .unwrap_or_else(|error| panic!("cannot write {}: {error}", target.display()));
            fs::set_permissions(&target, fs::Permissions::from_mode(*mode))
                .unwrap_or_else(|error| panic!("cannot chmod {}: {error}", target.display()));
        }
        Side::Absent => {
            if target.exists() {
                fs::remove_file(&target)
                    .unwrap_or_else(|error| panic!("cannot remove {}: {error}", target.display()));
            }
        }
    }
}

/// Stages everything and creates one fixture commit; returns its object ID.
fn commit_side(repo: &TestRepo, message: &str) -> String {
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", message]);
    text(&repo.git_ok(["rev-parse", "HEAD"]))
}

/// A fresh repository holding the committed B and C sides of a fixture.
struct FixtureRepo {
    repo: TestRepo,
    before: String,
    after: String,
}

/// Builds a fresh repository with the fixture pair committed as B and C.
fn fixture_repo(fixture: &Fixture) -> FixtureRepo {
    let repo = TestRepo::init();
    repo.write_file(SEED_PATH, SEED);
    write_side(&repo, fixture.path, &fixture.before);
    let before = commit_side(&repo, "fixture before");
    write_side(&repo, fixture.path, &fixture.after);
    let after = commit_side(&repo, "fixture after");
    FixtureRepo {
        repo,
        before,
        after,
    }
}

/// The canonical B-to-C patch of `path` under the fixed contract.
fn canonical_patch(repo: &TestRepo, before: &str, after: &str, path: &str) -> Vec<u8> {
    let mut argv: Vec<String> = DIFF_TREE_PATCH_ARGV
        .iter()
        .map(|argument| argument.to_string())
        .collect();
    argv.push(before.to_string());
    argv.push(after.to_string());
    argv.push("--".to_string());
    argv.push(path.to_string());
    repo.git_ok(argv)
}

/// The parsed shape of one single-file patch.
struct ParsedPatch {
    path: Vec<u8>,
    change: FileChange,
    binary: bool,
    hunks: Vec<(u32, u32, u32, u32)>,
    header_lines: Vec<Vec<u8>>,
    hunk_lines: Vec<Vec<Vec<u8>>>,
}

/// Finds the last occurrence of `needle` in `haystack`.
fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    (0..haystack.len())
        .rev()
        .find(|&index| haystack[index..].starts_with(needle))
}

/// Parses `-<start>[,<count>]` from a hunk header token.
fn parse_range(token: &[u8], what: &str) -> (u32, u32) {
    let body = token
        .strip_prefix(b"-")
        .or_else(|| token.strip_prefix(b"+"))
        .unwrap_or_else(|| {
            panic!("{what}: hunk range {token:?} has no side marker");
        });
    let number = |bytes: &[u8], part: &str| {
        text(bytes)
            .parse()
            .unwrap_or_else(|error| panic!("{what}: bad {part} {bytes:?}: {error}"))
    };
    match body.iter().position(|&byte| byte == b',') {
        Some(index) => (
            number(&body[..index], "start"),
            number(&body[index + 1..], "count"),
        ),
        None => (number(body, "start"), 1),
    }
}

/// Parses a single-file patch into its recorded shape. Hunk bodies stay
/// verbatim so group patches can be rebuilt byte for byte.
fn parse_patch(patch: &[u8], what: &str) -> ParsedPatch {
    let lines: Vec<&[u8]> = patch.split(|&byte| byte == b'\n').collect();
    let first = lines.first().copied().unwrap_or(&[]);
    let prefix = b"diff --git a/";
    assert!(
        first.starts_with(prefix),
        "{what}: the patch does not begin with a diff --git line"
    );
    let rest = &first[prefix.len()..];
    let separator = rfind(rest, b" b/")
        .unwrap_or_else(|| panic!("{what}: no b/ path in {:?}", String::from_utf8_lossy(rest)));
    let (path, second) = (&rest[..separator], &rest[separator + 3..]);
    assert_eq!(
        path, second,
        "{what}: the two path halves of the header disagree"
    );

    let mut change = FileChange::Modified;
    let mut binary = false;
    let mut header_lines: Vec<Vec<u8>> = Vec::new();
    let mut hunks: Vec<(u32, u32, u32, u32)> = Vec::new();
    let mut hunk_lines: Vec<Vec<Vec<u8>>> = Vec::new();
    let mut in_header = true;
    for line in lines.iter().skip(1) {
        if line.starts_with(b"diff --git ") {
            panic!("{what}: more than one file in the patch");
        }
        if line.starts_with(b"@@ ") {
            in_header = false;
            let core_end = rfind(line, b" @@").unwrap_or_else(|| {
                panic!(
                    "{what}: unterminated hunk header {:?}",
                    String::from_utf8_lossy(line)
                )
            });
            let mut tokens = line[3..core_end].split(|&byte| byte == b' ');
            let old_token = tokens.next().unwrap_or(&[]);
            let new_token = tokens.next().unwrap_or(&[]);
            assert!(
                tokens.next().is_none(),
                "{what}: extra fields in a hunk header"
            );
            let old = parse_range(old_token, what);
            let new = parse_range(new_token, what);
            hunks.push((old.0, old.1, new.0, new.1));
            hunk_lines.push(vec![line.to_vec()]);
        } else if *line == b"GIT binary patch" {
            binary = true;
            in_header = false;
            header_lines.push(line.to_vec());
        } else if line.starts_with(b"new file mode") {
            change = FileChange::Added;
            header_lines.push(line.to_vec());
        } else if line.starts_with(b"deleted file mode") {
            change = FileChange::Deleted;
            header_lines.push(line.to_vec());
        } else if line.starts_with(b"old mode") {
            if matches!(change, FileChange::Modified) {
                change = FileChange::ModeOnly;
            }
            header_lines.push(line.to_vec());
        } else if in_header {
            header_lines.push(line.to_vec());
        } else if let Some(body) = hunk_lines.last_mut() {
            body.push(line.to_vec());
        }
    }
    if matches!(change, FileChange::ModeOnly) && !hunks.is_empty() {
        change = FileChange::Modified;
    }
    ParsedPatch {
        path: path.to_vec(),
        change,
        binary,
        hunks,
        header_lines,
        hunk_lines,
    }
}

/// Rebuilds the sub-patch holding exactly `hunks`, verbatim.
fn group_patch(parsed: &ParsedPatch, hunks: &[usize]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in &parsed.header_lines {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    for &number in hunks {
        for line in &parsed.hunk_lines[number - 1] {
            out.extend_from_slice(line);
            out.push(b'\n');
        }
    }
    out
}

/// Resets index and working tree to `commit`.
fn reset(repo: &TestRepo, commit: &str) {
    repo.git_ok(["reset", "--hard", "--quiet", commit]);
}

/// Reverse-applies `patch` through the guarded index path of stock Git.
fn reverse_apply(repo: &TestRepo, patch: &[u8]) {
    repo.git_stdin(
        ["apply", "--reverse", "--index", "--whitespace=nowarn", "-"],
        patch,
    );
}

/// Hashes `bytes` into the repository without any conversion filter.
fn blob_oid(repo: &TestRepo, bytes: &[u8]) -> String {
    text(&repo.git_stdin(["hash-object", "-w", "--no-filters", "--stdin"], bytes))
}

/// Builds the expected result tree from the sentinel plus an optional
/// document entry, through `git mktree`.
fn expected_tree(repo: &TestRepo, fixture: &Fixture, document: Option<(&[u8], u32)>) -> String {
    let mut entries: BTreeMap<&str, String> = BTreeMap::new();
    entries.insert(
        SEED_PATH,
        format!("100644 blob {}\t{}", blob_oid(repo, SEED), SEED_PATH),
    );
    if let Some((bytes, mode)) = document {
        let git_mode = if mode & 0o111 == 0 {
            "100644"
        } else {
            "100755"
        };
        entries.insert(
            fixture.path,
            format!(
                "{git_mode} blob {}\t{}",
                blob_oid(repo, bytes),
                fixture.path
            ),
        );
    }
    let mut input = String::new();
    for line in entries.values() {
        input.push_str(line);
        input.push('\n');
    }
    text(&repo.git_stdin(["mktree"], input.as_bytes()))
}

/// Reads the working-tree document: exact bytes and the executable bit Git
/// tracks. The umask governs the remaining bits, so wider comparison would
/// test the environment instead of the contract.
fn working_document(repo: &TestRepo, path: &str) -> Option<(Vec<u8>, u32)> {
    let target = repo.file_path(path);
    let metadata = fs::symlink_metadata(&target).ok()?;
    Some((
        fs::read(&target)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", target.display())),
        metadata.permissions().mode() & 0o111,
    ))
}

/// Asserts that the working document matches `side` exactly.
fn assert_working(
    repo: &TestRepo,
    fixture: &Fixture,
    side: &Side,
    what: &str,
) -> Result<(), String> {
    match (
        working_document(repo, fixture.path),
        side.bytes(),
        side.mode(),
    ) {
        (Some((bytes, exec)), Some(expected), Some(mode)) => {
            if bytes != expected {
                return Err(format!(
                    "{what}: working bytes differ from the expected side"
                ));
            }
            if exec != (mode & 0o111) {
                return Err(format!(
                    "{what}: the executable bit differs from the expected side"
                ));
            }
        }
        (None, None, None) => {}
        (observed, expected, _) => {
            return Err(format!(
                "{what}: presence differs (observed {observed:?}, expected {expected:?})"
            ));
        }
    }
    if repo.read_file(SEED_PATH) != SEED {
        return Err(format!("{what}: the out-of-scope sentinel changed"));
    }
    Ok(())
}

/// Asserts that the index tree equals the expected tree of `document`.
fn assert_tree(
    repo: &TestRepo,
    fixture: &Fixture,
    document: Option<(&[u8], u32)>,
    what: &str,
) -> Result<(), String> {
    let actual = text(&repo.git_ok(["write-tree"]));
    let expected = expected_tree(repo, fixture, document);
    if actual != expected {
        return Err(format!(
            "{what}: index tree {actual} is not the expected tree {expected}"
        ));
    }
    Ok(())
}

/// The document entry of a side: bytes plus executable bit, or `None`.
fn side_document(side: &Side) -> Option<(&'static [u8], u32)> {
    side.bytes()
        .map(|bytes| (bytes, side.mode().unwrap_or(0) & 0o111))
}

/// Runs every recorded expectation of one fixture through stock Git.
fn check_fixture(
    repo: &TestRepo,
    fixture: &Fixture,
    before: &str,
    after: &str,
) -> Result<(), String> {
    // Commit fidelity: the committed blobs are exactly the fixture bytes,
    // which also anchors keep-all: C is the no-change result.
    for (side, commit, label) in [(&fixture.before, before, "B"), (&fixture.after, after, "C")] {
        match side.bytes() {
            Some(bytes) => {
                let committed =
                    repo.git_ok(["cat-file", "blob", &format!("{commit}:{}", fixture.path)]);
                if committed != bytes {
                    return Err(format!(
                        "the {label} commit does not hold the exact fixture bytes"
                    ));
                }
            }
            None => {
                let output = repo.git(["cat-file", "blob", &format!("{commit}:{}", fixture.path)]);
                if output.status.success() {
                    return Err(format!(
                        "the {label} commit holds a document that should be absent"
                    ));
                }
            }
        }
    }
    reset(repo, after);
    assert_working(repo, fixture, &fixture.after, "keep-all")?;
    assert_tree(repo, fixture, side_document(&fixture.after), "keep-all")?;

    // Recorded source changes.
    let patch = canonical_patch(repo, before, after, fixture.path);
    let parsed = parse_patch(&patch, fixture.id);
    if parsed.path != fixture.path.as_bytes() {
        return Err(format!(
            "the canonical patch names {:?}, not {:?}",
            String::from_utf8_lossy(&parsed.path),
            fixture.path
        ));
    }
    if parsed.change != fixture.changes.change {
        return Err(format!(
            "the canonical change is {:?}, recorded {:?}",
            parsed.change, fixture.changes.change
        ));
    }
    if parsed.binary != fixture.changes.binary {
        return Err(format!(
            "binary patch flag is {}, recorded {}",
            parsed.binary, fixture.changes.binary
        ));
    }
    let recorded: Vec<(u32, u32, u32, u32)> = fixture
        .changes
        .hunks
        .iter()
        .map(|hunk| {
            (
                hunk.old_start,
                hunk.old_count,
                hunk.new_start,
                hunk.new_count,
            )
        })
        .collect();
    if parsed.hunks != recorded {
        return Err(format!(
            "canonical hunks {recorded:?} differ from the recorded {:?}",
            parsed.hunks
        ));
    }

    // Restore-all: reversing the whole patch must return B exactly.
    reset(repo, after);
    reverse_apply(repo, &patch);
    assert_working(repo, fixture, &fixture.before, "restore-all")?;
    assert_tree(repo, fixture, side_document(&fixture.before), "restore-all")?;

    let Expectation::Groups { groups } = &fixture.expectation else {
        return Ok(());
    };
    for group in *groups {
        reset(repo, after);
        reverse_apply(repo, &group_patch(&parsed, group.hunks));
        assert_working_document(repo, fixture, &group.restore, "group restore")?;
        assert_tree(
            repo,
            fixture,
            Some((group.restore.bytes(), exec_of(&fixture.after))),
            "group restore",
        )?;
    }

    // Composition: restoring every group, latest hunk positions first so
    // the recorded line numbers stay valid, must recompose B exactly.
    if groups.len() > 1 {
        reset(repo, after);
        let mut order: Vec<&c0::Group> = groups.iter().collect();
        order.sort_by_key(|group| group.hunks[group.hunks.len() - 1]);
        order.reverse();
        for group in order {
            reverse_apply(repo, &group_patch(&parsed, group.hunks));
        }
        assert_working(repo, fixture, &fixture.before, "group composition")?;
        assert_tree(
            repo,
            fixture,
            side_document(&fixture.before),
            "group composition",
        )?;
    }
    Ok(())
}

/// The executable bit of a side; zero when the side is absent.
fn exec_of(side: &Side) -> u32 {
    side.mode().unwrap_or(0) & 0o111
}

/// Asserts that the working document equals recorded restoration content.
fn assert_working_document(
    repo: &TestRepo,
    fixture: &Fixture,
    restore: &Content,
    what: &str,
) -> Result<(), String> {
    let expected_exec = exec_of(&fixture.after);
    match working_document(repo, fixture.path) {
        Some((bytes, exec)) => {
            if bytes != restore.bytes() {
                return Err(format!(
                    "{what}: the restored bytes differ from the recorded result"
                ));
            }
            if exec != expected_exec {
                return Err(format!("{what}: the restored executable bit differs"));
            }
        }
        None => {
            return Err(format!(
                "{what}: the document disappeared during restoration"
            ));
        }
    }
    if repo.read_file(SEED_PATH) != SEED {
        return Err(format!("{what}: the out-of-scope sentinel changed"));
    }
    Ok(())
}
