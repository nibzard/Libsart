//! Exact restoration proofs for safe change groups (SPEC 10.6, 16.4, T025).
//!
//! The grouping engine of T024 issues one patch unit per safe group. This
//! suite proves those units restore exactly, before any review interface
//! exists to apply them:
//!
//! - for every C0 fixture, keep-all, restore-all, and every subset of the
//!   fixture's groups restore the exact recorded bytes, presence, and mode;
//! - for generated documents, every subset of up to eight independent groups
//!   restores the exact expected result, and larger group sets restore a
//!   stable sample of keep and restore combinations;
//! - no source change disappears: the groups partition the complete patch,
//!   and restoring every group recomposes the baseline tree exactly;
//! - out-of-scope entries stay with the candidate tree, in both object
//!   formats.
//!
//! The oracle is independent of the implementation (SPEC 16.3). Expected
//! bytes come from the frozen corpus data, from line arithmetic over the
//! recorded hunk ranges, or from the generated region construction; expected
//! trees come from `git mktree` over `git hash-object` results; the
//! applications themselves run through stock Git (`git read-tree`,
//! `git apply --cached --reverse`, `git write-tree`), never through a Margin
//! parser.
//!
//! Set `MARGIN_RESTORATION_CASES` and `MARGIN_RESTORATION_LARGE_CASES` to
//! raise the generated case budgets of SPEC 16.4 for a CI tier. Every case
//! is a fixed function of its printed seed, so a failure reproduces exactly.

mod support;

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::thread;

use margin::app::{GroupedFile, GroupedManifest, build_grouped_manifest, resolve_scope};
use margin::domain::ScopeInput;
use margin::git::Git;
use support::c0::{self, Expectation, Fixture, Group, HunkRange, Side};
use support::oracle::ls_files;
use support::repo::{ObjectFormat, RepoOptions, TestRepo};
use support::text;

/// Out-of-scope sentinel committed beside every fixture document. It never
/// changes, so every restoration must leave its entry identical (SPEC 16.3).
/// The `.txt` suffix also keeps it outside the Markdown scope.
const SENTINEL_PATH: &str = "sentinel.txt";
const SENTINEL: &[u8] = b"out-of-scope sentinel for restoration proofs\n";

/// The guarded index application of one patch unit: the same explicit
/// whitespace policy the engine contract fixes (SPEC 9.3, 14.1), so the
/// oracle never applies with fuzzy matching the engine would refuse.
const APPLY_ARGV: &[&str] = &[
    "-c",
    "apply.ignoreWhitespace=false",
    "apply",
    "--cached",
    "--reverse",
    "--whitespace=nowarn",
    "-",
];

/// The default case budget of the small-manifest property when no
/// environment override is present. The fixed leading shapes always run;
/// CI tiers raise the budget through `MARGIN_RESTORATION_CASES` to the
/// case counts of SPEC 16.4.
const DEFAULT_CASES: usize = 48;

/// The default case budget of the larger-manifest property when no
/// environment override is present. CI tiers raise it through
/// `MARGIN_RESTORATION_LARGE_CASES`.
const DEFAULT_LARGE_CASES: usize = 8;

/// The seed salt of every generated case; the case index completes the seed.
const SEED_SALT: u64 = 0x5EC0_2525_0000_0000;

/// One mask with alternating groups restored, used as the sampled tree
/// oracle case for every manifest with more than two groups.
const ALTERNATING_MASK: usize = 0b0101_0101;

// ---------------------------------------------------------------------------
// Stock-Git application and tree oracles
// ---------------------------------------------------------------------------

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
fn grouped_for(repo: &TestRepo, baseline: &str, candidate: &str) -> GroupedManifest {
    build_grouped_manifest(
        &adapter_for(repo),
        "refs/heads/main",
        baseline,
        candidate,
        &scope_descriptor(repo),
    )
    .unwrap_or_else(|error| panic!("the grouped manifest must build: {error}"))
}

/// Wraps `build_grouped_manifest` into a message for the parallel workers.
fn try_build(repo: &TestRepo, before: &str, after: &str) -> Result<GroupedManifest, String> {
    build_grouped_manifest(
        &adapter_for(repo),
        "refs/heads/main",
        before,
        after,
        &scope_descriptor(repo),
    )
    .map_err(|error| format!("the grouped manifest did not build: {error}"))
}

/// Seeds the index with the candidate tree, reverse-applies each patch unit
/// through the guarded index path, and returns the resulting index entries
/// per path as `mode oid`. The units of one file arrive latest source
/// position first, which keeps every recorded line number valid.
fn apply_subset(repo: &TestRepo, candidate: &str, units: &[&[u8]]) -> BTreeMap<String, String> {
    repo.git_ok(["read-tree", candidate]);
    for unit in units {
        repo.git_stdin(APPLY_ARGV, unit);
    }
    index_entries(repo)
}

/// The index entries of `git ls-files --stage` as `path -> "mode oid"`.
fn index_entries(repo: &TestRepo) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for entry in ls_files(repo) {
        assert_eq!(entry.stage, 0, "no conflicted stage may survive: {entry:?}");
        let previous = map.insert(entry.path.clone(), format!("{} {}", entry.mode, entry.oid));
        assert!(
            previous.is_none(),
            "the index holds two entries for {}",
            entry.path
        );
    }
    map
}

/// Writes the current index as a tree and returns its object ID.
fn write_index_tree(repo: &TestRepo) -> String {
    text(&repo.git_ok(["write-tree"]))
}

/// The tree of one commit, through `git rev-parse`.
fn commit_tree(repo: &TestRepo, commit: &str) -> String {
    text(&repo.git_ok(["rev-parse", &format!("{commit}^{{tree}}")]))
}

/// Hashes every distinct blob once and returns `bytes -> object ID`. The
/// expected trees of all subsets of one case derive from this one batch, so
/// the subset checks stay independent of how many subsets run.
fn hash_blobs(repo: &TestRepo, blobs: &[Vec<u8>]) -> HashMap<Vec<u8>, String> {
    let mut unique: HashMap<Vec<u8>, PathBuf> = HashMap::new();
    for blob in blobs {
        if !unique.contains_key(blob) {
            let index = unique.len();
            let path = repo.root().join(format!("oracle-{index}.blob"));
            fs::write(&path, blob)
                .unwrap_or_else(|error| panic!("cannot write {}: {error}", path.display()));
            unique.insert(blob.clone(), path);
        }
    }
    if unique.is_empty() {
        return HashMap::new();
    }
    let mut argv: Vec<String> = vec![
        "hash-object".to_string(),
        "-w".to_string(),
        "--no-filters".to_string(),
        "--".to_string(),
    ];
    let mut order: Vec<Vec<u8>> = Vec::new();
    for (blob, path) in unique.iter() {
        argv.push(path.to_string_lossy().into_owned());
        order.push(blob.clone());
    }
    let stdout = repo.git_ok(argv);
    let oids: Vec<String> = String::from_utf8_lossy(&stdout)
        .lines()
        .map(|line| line.trim().to_string())
        .collect();
    assert_eq!(
        oids.len(),
        order.len(),
        "git hash-object returned {} object IDs for {} files",
        oids.len(),
        order.len()
    );
    order.into_iter().zip(oids).collect()
}

/// Builds one tree from `mode oid` index entries per path through
/// `git mktree`, naming each object a blob. This is the independent
/// expected-tree oracle.
fn mktree(repo: &TestRepo, entries: &BTreeMap<String, String>) -> String {
    let mut input = String::new();
    for (path, entry) in entries {
        // The index entries hold `mode oid`; a tree record also names the
        // object type.
        let record = match entry.split_once(' ') {
            Some((mode, oid)) => format!("{mode} blob {oid}"),
            None => entry.clone(),
        };
        input.push_str(&record);
        input.push('\t');
        input.push_str(path);
        input.push('\n');
    }
    text(&repo.git_stdin(["mktree"], input.as_bytes()))
}

/// The storage mode string of one Unix mode: only the executable bit changes
/// what Git stores.
fn git_mode(mode: u32) -> String {
    if mode & 0o111 == 0 {
        "100644".to_string()
    } else {
        "100755".to_string()
    }
}

/// The storage mode of one side; `100644` when the side is absent.
fn side_mode(side: &Side) -> String {
    git_mode(side.mode().unwrap_or(0))
}

// ---------------------------------------------------------------------------
// The expected-result oracles
// ---------------------------------------------------------------------------

/// Splits exact bytes into lines that keep their terminator, so one final
/// unterminated line stays distinct from a terminated one (SPEC 10.5).
fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, &byte) in bytes.iter().enumerate() {
        if byte == b'\n' {
            lines.push(&bytes[start..=index]);
            start = index + 1;
        }
    }
    if start < bytes.len() {
        lines.push(&bytes[start..]);
    }
    lines
}

/// Rebuilds the exact bytes of one partial restoration: the C side with the
/// selected recorded hunks reverted to the B side. Pure line arithmetic over
/// the frozen `@@` ranges. A hunk window carries its context lines, which
/// are equal on both sides, so whole windows splice in and out. Only the
/// last line of a file can be unterminated, and only the last hunk can reach
/// it, so splicing keeps every terminator exact.
fn revert_hunks(before: &[u8], after: &[u8], hunks: &[HunkRange], selected: &[usize]) -> Vec<u8> {
    let old = split_lines(before);
    let new = split_lines(after);
    let mut picked: Vec<&HunkRange> = selected.iter().map(|&number| &hunks[number - 1]).collect();
    picked.sort_by_key(|hunk| hunk.new_start);
    let mut out: Vec<u8> = Vec::new();
    let mut cursor = 0;
    for hunk in picked {
        let start = (hunk.new_start.max(1) - 1) as usize;
        let end = (start + hunk.new_count as usize).min(new.len());
        for line in &new[cursor.min(start)..start] {
            out.extend_from_slice(line);
        }
        let old_start = (hunk.old_start.max(1) - 1) as usize;
        let old_end = (old_start + hunk.old_count as usize).min(old.len());
        for line in &old[old_start..old_end] {
            out.extend_from_slice(line);
        }
        cursor = end;
    }
    for line in &new[cursor.min(new.len())..] {
        out.extend_from_slice(line);
    }
    out
}

/// One expected document state of one subset restoration.
struct Variant {
    /// Exact expected bytes of the document.
    bytes: Vec<u8>,
    /// Expected storage mode.
    mode: String,
    /// Whether the document is present in the result.
    present: bool,
}

/// The expected variants of one fixture: index `mask` runs over the file's
/// groups in manifest order, with one bit set meaning the group is restored.
fn fixture_variants(fixture: &Fixture, group_count: usize) -> Vec<Variant> {
    let recorded: &[Group] = match &fixture.expectation {
        Expectation::Groups { groups } => groups,
        Expectation::WholeFile { .. } => &[],
    };
    (0..(1usize << group_count))
        .map(|mask| {
            let selected: Vec<usize> = (0..group_count)
                .filter(|index| mask & (1 << index) != 0)
                .collect();
            if selected.is_empty() {
                Variant {
                    bytes: fixture.after.bytes().unwrap_or_default().to_vec(),
                    mode: side_mode(&fixture.after),
                    present: fixture.after.bytes().is_some(),
                }
            } else if selected.len() == group_count {
                Variant {
                    bytes: fixture.before.bytes().unwrap_or_default().to_vec(),
                    mode: side_mode(&fixture.before),
                    present: fixture.before.bytes().is_some(),
                }
            } else {
                // A partial restoration of a readable modified file: revert
                // the selected groups' recorded hunks by line arithmetic and
                // keep the candidate presence and mode.
                let mut hunks: Vec<usize> = Vec::new();
                for &index in &selected {
                    hunks.extend_from_slice(recorded[index].hunks);
                }
                Variant {
                    bytes: revert_hunks(
                        fixture.before.bytes().unwrap_or_default(),
                        fixture.after.bytes().unwrap_or_default(),
                        fixture.changes.hunks,
                        &hunks,
                    ),
                    mode: side_mode(&fixture.after),
                    present: fixture.after.bytes().is_some(),
                }
            }
        })
        .collect()
}

/// The expected variants of one fixed draft: the candidate lines with each
/// selected token block returned to its baseline block. The blocks are
/// contiguous runs of token-prefixed lines in document order.
fn variants_by_token(before: &[String], after: &[String], tokens: &[&str]) -> Vec<Variant> {
    let block = |lines: &[String], token: &str| -> Vec<String> {
        lines
            .iter()
            .filter(|line| line.starts_with(token))
            .cloned()
            .collect()
    };
    let mut variants = Vec::new();
    for mask in 0..(1usize << tokens.len()) {
        let mut lines: Vec<String> = Vec::new();
        let mut cursor = 0;
        for (index, token) in tokens.iter().enumerate() {
            let start = after[cursor..]
                .iter()
                .position(|line| line.starts_with(token))
                .expect("the token block is in the candidate")
                + cursor;
            lines.extend(after[cursor..start].iter().cloned());
            if mask & (1 << index) != 0 {
                lines.extend(block(before, token));
            } else {
                lines.extend(block(after, token));
            }
            cursor = start + block(after, token).len();
        }
        lines.extend(after[cursor..].iter().cloned());
        let mut bytes = String::new();
        for line in &lines {
            bytes.push_str(line);
            bytes.push('\n');
        }
        variants.push(Variant {
            bytes: bytes.into_bytes(),
            mode: "100644".to_string(),
            present: true,
        });
    }
    variants
}

// ---------------------------------------------------------------------------
// The shared subset proof
// ---------------------------------------------------------------------------

/// One document of a case under proof: its path, its groups inside the
/// manifest, its groups' file, and the expected variant per local mask.
struct ProofDocument {
    /// Repository-relative document path.
    path: String,
    /// Zero-based offset of the file's first group in mask bit order.
    offset: usize,
    /// The file's independent groups.
    groups: usize,
    /// Index of the file inside the manifest.
    file_index: usize,
    /// Expected variant per local mask over the file's groups.
    variants: Vec<Variant>,
}

/// Proves every mask of one case: applies the selected patch units through
/// stock Git, compares the resulting index entries with the expected
/// entries, and compares keep-all, restore-all, singleton, and one
/// alternating tree against `git mktree`, the candidate tree, and the
/// baseline tree. Returns one message per failure.
fn prove_masks(
    repo: &TestRepo,
    manifest: &GroupedManifest,
    documents: &[ProofDocument],
    baseline: &str,
    candidate: &str,
    masks: &[usize],
    context: &str,
) -> Vec<String> {
    let total: usize = documents.iter().map(|document| document.groups).sum();
    let mut failures = Vec::new();

    // Every candidate entry no proof document owns, read once: the sentinel
    // and any out-of-scope change stay with the candidate, so every subset
    // must reproduce these entries exactly (SPEC 16.3, INV-07).
    repo.git_ok(["read-tree", candidate]);
    let document_paths: Vec<&str> = documents
        .iter()
        .map(|document| document.path.as_str())
        .collect();
    let fixed: BTreeMap<String, String> = index_entries(repo)
        .into_iter()
        .filter(|(path, _)| !document_paths.contains(&path.as_str()))
        .collect();
    assert!(
        fixed.contains_key(SENTINEL_PATH),
        "{context}: the sentinel is missing from the candidate"
    );

    // The same entries of the baseline: when they are equal, no out-of-scope
    // change separates the two revisions and restore-all must recompose the
    // baseline tree itself; otherwise the out-of-scope change stays with the
    // candidate and only the mktree oracle applies.
    repo.git_ok(["read-tree", baseline]);
    let baseline_fixed: BTreeMap<String, String> = index_entries(repo)
        .into_iter()
        .filter(|(path, _)| !document_paths.contains(&path.as_str()))
        .collect();
    let out_of_scope_unchanged = baseline_fixed == fixed;

    // Every distinct expected blob, hashed in one batch.
    let blobs: Vec<Vec<u8>> = documents
        .iter()
        .flat_map(|document| {
            document
                .variants
                .iter()
                .map(|variant| variant.bytes.clone())
        })
        .collect();
    let oids = hash_blobs(repo, &blobs);

    // The patch units per file, in manifest order; application runs one
    // file's units latest source position first.
    let units: Vec<Vec<Vec<u8>>> = manifest
        .files
        .iter()
        .map(|file| {
            file.groups
                .iter()
                .map(|group| group.group.patch_units[0].clone())
                .collect()
        })
        .collect();

    for &mask in masks {
        let mut expected: BTreeMap<String, String> = fixed.clone();
        let mut applied: Vec<&[u8]> = Vec::new();
        for document in documents {
            let local = (mask >> document.offset) & ((1 << document.groups) - 1);
            let variant = &document.variants[local];
            if variant.present {
                let oid = &oids[&variant.bytes];
                expected.insert(document.path.clone(), format!("{} {}", variant.mode, oid));
            }
            for index in (0..document.groups).rev() {
                if local & (1 << index) != 0 {
                    applied.push(&units[document.file_index][index]);
                }
            }
        }
        let observed = apply_subset(repo, candidate, &applied);
        if observed != expected {
            let mut paths: Vec<&String> = observed.keys().chain(expected.keys()).collect();
            paths.sort();
            paths.dedup();
            let differences: Vec<String> = paths
                .into_iter()
                .filter_map(|path| match (observed.get(path), expected.get(path)) {
                    (Some(found), Some(wanted)) if found == wanted => None,
                    (Some(found), Some(wanted)) => {
                        Some(format!("{path}: index holds {found}, expected {wanted}"))
                    }
                    (Some(found), None) => Some(format!("{path}: unexpected {found}")),
                    (None, Some(wanted)) => Some(format!("{path}: missing {wanted}")),
                    (None, None) => unreachable!("the key set supplies each path"),
                })
                .collect();
            failures.push(format!(
                "{context}: mask {mask:#b} of {total} groups restored {} units, but the index \
                 disagrees:\n    {}",
                applied.len(),
                differences.join("\n    ")
            ));
            continue;
        }
        // Tree oracles: keep-all equals the candidate tree, restore-all the
        // mktree oracle over the candidate's out-of-scope entries and the
        // restored documents, and each singleton and one alternating mask
        // equal the mktree oracle. With no out-of-scope change between the
        // revisions, restore-all must equal the baseline tree itself.
        let restored = mask.count_ones() as usize;
        if restored == 0 || restored == total || restored == 1 || mask == ALTERNATING_MASK {
            let tree = write_index_tree(repo);
            let mut wanted = mktree(repo, &expected);
            if restored == 0 {
                wanted = commit_tree(repo, candidate);
            } else if restored == total && out_of_scope_unchanged {
                wanted = commit_tree(repo, baseline);
            }
            if tree != wanted {
                failures.push(format!(
                    "{context}: mask {mask:#b} tree {tree} is not the expected tree {wanted}"
                ));
            }
        }
    }
    failures
}

// ---------------------------------------------------------------------------
// The corpus fixture proof
// ---------------------------------------------------------------------------

/// Writes one side of a fixture into the working tree.
fn write_side(repo: &TestRepo, path: &str, side: &Side) {
    let target = repo.file_path(path);
    match side {
        Side::Present { content, mode } => {
            fs::write(&target, content.bytes())
                .unwrap_or_else(|error| panic!("cannot write {}: {error}", target.display()));
            let current = fs::metadata(&target)
                .unwrap_or_else(|error| panic!("cannot stat {}: {error}", target.display()))
                .permissions()
                .mode();
            fs::set_permissions(
                &target,
                fs::Permissions::from_mode((current & !0o111) | (mode & 0o111)),
            )
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

/// Builds a fresh repository with the fixture pair committed as B and C.
fn fixture_repo(fixture: &Fixture) -> (TestRepo, String, String) {
    let repo = TestRepo::init();
    repo.write_file(SENTINEL_PATH, SENTINEL);
    write_side(&repo, fixture.path, &fixture.before);
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "fixture before"]);
    let before = text(&repo.git_ok(["rev-parse", "HEAD"]));
    write_side(&repo, fixture.path, &fixture.after);
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "fixture after"]);
    let after = text(&repo.git_ok(["rev-parse", "HEAD"]));
    (repo, before, after)
}

/// Proves one fixture: the manifest groups exactly as recorded, the groups
/// partition the complete patch, and every subset restores exactly.
fn check_fixture(fixture: &Fixture) -> Vec<String> {
    let context = format!("fixture {}", fixture.id);
    let (repo, before, after) = fixture_repo(fixture);
    let manifest = match try_build(&repo, &before, &after) {
        Ok(manifest) => manifest,
        Err(error) => return vec![format!("{context}: {error}")],
    };
    let files: BTreeMap<&str, &GroupedFile> = manifest
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();
    let Some(file) = files.get(fixture.path) else {
        return vec![format!(
            "{context}: the manifest holds no group for the fixture document"
        )];
    };
    if files.len() != 1 {
        return vec![format!(
            "{context}: the manifest holds {} files, exactly the fixture document expected",
            files.len()
        )];
    }

    let mut failures = Vec::new();
    let group_count = file.groups.len();
    let recorded = match &fixture.expectation {
        Expectation::Groups { groups } => groups.len(),
        Expectation::WholeFile { .. } => 1,
    };
    if group_count != recorded {
        failures.push(format!(
            "{context}: {group_count} groups, the recorded expectation holds {recorded}"
        ));
        return failures;
    }

    // Complete change accounting: the groups partition the file's hunks, so
    // no source change can disappear from the manifest (SPEC 10.6).
    let mut covered: Vec<usize> = file
        .groups
        .iter()
        .flat_map(|group| group.hunk_indexes.iter().copied())
        .collect();
    covered.sort();
    let complete: Vec<usize> = (0..file.hunks.len()).collect();
    if covered != complete {
        failures.push(format!(
            "{context}: the groups cover {covered:?} of the hunks 0..{}",
            file.hunks.len()
        ));
    }
    if file.hunks.len() != fixture.changes.hunks.len() {
        failures.push(format!(
            "{context}: {} hunks, the recorded changes hold {}",
            file.hunks.len(),
            fixture.changes.hunks.len()
        ));
    }

    let variants = fixture_variants(fixture, group_count);
    // The recorded per-group restorations must agree with the line
    // arithmetic, and restoring one group alone must change the candidate.
    if let Expectation::Groups { groups } = &fixture.expectation {
        let candidate_bytes = fixture.after.bytes().unwrap_or_default();
        for (index, group) in groups.iter().enumerate() {
            if group_count > 1 && variants[1 << index].bytes != group.restore.bytes() {
                failures.push(format!(
                    "{context}: group {} restored by line arithmetic differs from the recorded \
                     bytes",
                    index + 1
                ));
            }
            if group.restore.bytes() == candidate_bytes {
                failures.push(format!(
                    "{context}: restoring group {} alone changes nothing",
                    index + 1
                ));
            }
        }
    }

    let document = ProofDocument {
        path: fixture.path.to_string(),
        offset: 0,
        groups: group_count,
        file_index: 0,
        variants,
    };
    let masks: Vec<usize> = (0..(1usize << group_count)).collect();
    failures.extend(prove_masks(
        &repo,
        &manifest,
        &[document],
        &before,
        &after,
        &masks,
        &context,
    ));
    failures
}

/// Every corpus fixture restores every subset of its groups exactly (SPEC
/// 10.6): keep-all, restore-all, and each partial selection, against the
/// frozen recorded results and stock-Git trees.
#[test]
fn corpus_fixtures_restore_every_subset_exactly() {
    let total = c0::count();
    let failures = run_parallel(total, |index| {
        check_fixture(c0::iter().nth(index).expect("the fixture exists"))
    });
    assert!(
        failures.is_empty(),
        "fixture restorations disagreed with the oracles:\n  {}",
        failures.join("\n  ")
    );
}

// ---------------------------------------------------------------------------
// Generated property cases
// ---------------------------------------------------------------------------

/// One deterministic SplitMix64 generator (SPEC 16.4: stable seeds, so every
/// case and every failure reproduce exactly).
struct Rng(u64);

impl Rng {
    /// The generator of one case: the salted seed.
    fn new(seed: u64) -> Self {
        Rng(seed)
    }

    /// The next pseudorandom 64-bit value.
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// One pseudorandom value below `bound`.
    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    /// True with probability `numerator` of `denominator`.
    fn chance(&mut self, numerator: u64, denominator: u64) -> bool {
        self.next() % denominator < numerator
    }
}

/// Ordinary prose words of the generator vocabulary.
const PLAIN_WORDS: &[&str] = &[
    "harbor", "lantern", "meadow", "quartz", "signal", "timber", "compass", "ember", "fjord",
    "willow", "cinder", "marble", "ripple", "thicket",
];

/// Unicode words of the generator vocabulary: accents, non-Latin scripts,
/// combining marks, and emoji (SPEC 16.4).
const UNICODE_WORDS: &[&str] = &[
    "naïve",
    "żółć",
    "日本語の記録",
    "Σαλπίσματα",
    "e\u{301}cole",
    "μῆνιν",
    "良好",
    "🫧",
    "🙂",
];

/// One generated region edit: what the candidate holds at `start_in_after`,
/// and the baseline block that restores it.
struct RegionEdit {
    start_in_after: usize,
    new_len: usize,
    old_block: Vec<String>,
}

/// One generated document pair: the baseline lines, the candidate lines, the
/// line separator, and the region edits between them.
struct GeneratedDoc {
    path: String,
    before: Vec<String>,
    after: Vec<String>,
    separator: String,
    regions: Vec<RegionEdit>,
}

/// One generated line: unique prose that no other line of the case repeats,
/// occasionally carrying trailing spaces or a tab (SPEC 10.5).
fn generated_line(rng: &mut Rng, token: &str, salt: usize) -> String {
    let words = 3 + rng.below(5);
    let mut line = String::new();
    for _ in 0..words {
        let word = if rng.chance(1, 3) {
            UNICODE_WORDS[rng.below(UNICODE_WORDS.len())]
        } else {
            PLAIN_WORDS[rng.below(PLAIN_WORDS.len())]
        };
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    line.push(' ');
    line.push_str(token);
    line.push_str(&salt.to_string());
    if rng.chance(1, 5) {
        line.push_str("  ");
    } else if rng.chance(1, 10) {
        line.push('\t');
    }
    line
}

/// Generates one document pair with `regions` independent paragraph edits.
/// One gap paragraph of six lines plus two blank lines separates every
/// region, so each region forms exactly one complete hunk and the regions
/// are independent passages (SPEC 10.3). One region edit replaces one whole
/// paragraph: either a fresh block of a new length, or the same paragraph
/// with one interior line rewritten, so the restored block stays contiguous
/// on both sides.
fn generate_doc(
    rng: &mut Rng,
    token: &str,
    path: &str,
    regions: usize,
    crlf: bool,
) -> GeneratedDoc {
    let separator = if crlf { "\r\n" } else { "\n" }.to_string();
    let mut before: Vec<String> = vec![format!("# Notes of {token}"), String::new()];
    let mut starts: Vec<usize> = Vec::new();
    for region in 0..regions {
        for gap in 0..6 {
            before.push(format!("gap {token} {region} {gap}"));
        }
        before.push(String::new());
        starts.push(before.len());
        for _ in 0..(4 + rng.below(5)) {
            let salt = rng.next() as usize;
            before.push(generated_line(rng, &format!("o{token}r{region}"), salt));
        }
        before.push(String::new());
    }

    let paragraph_length = |start: usize| -> usize {
        before[start..]
            .iter()
            .take_while(|line| !line.is_empty())
            .count()
    };

    let mut after: Vec<String> = Vec::new();
    let mut edits: Vec<RegionEdit> = Vec::new();
    // Both cursors walk in step: `before_cursor` through the baseline, the
    // growing `after` through the candidate, so blocks of differing lengths
    // never misalign the next region.
    let mut before_cursor = 0usize;
    for (region, &start) in starts.iter().enumerate() {
        after.extend(before[before_cursor..start].iter().cloned());
        let old_block: Vec<String> = before[start..start + paragraph_length(start)].to_vec();
        let new_block: Vec<String> = if rng.chance(3, 4) {
            // A fresh block of a new length.
            (0..(1 + rng.below(7)))
                .map(|line| generated_line(rng, &format!("n{token}r{region}"), line))
                .collect()
        } else {
            // The same paragraph with one interior line rewritten.
            let position = 1 + rng.below(old_block.len() - 2);
            let mut new: Vec<String> = old_block[..position].to_vec();
            for line in 0..(1 + rng.below(2)) {
                new.push(generated_line(
                    rng,
                    &format!("n{token}r{region}p{position}"),
                    line,
                ));
            }
            new.extend(old_block[position + 1..].iter().cloned());
            new
        };
        if old_block == new_block {
            panic!("generated region {region} of {path} holds an unchanged block");
        }
        let start_in_after = after.len();
        after.extend(new_block.iter().cloned());
        before_cursor = start + old_block.len();
        edits.push(RegionEdit {
            start_in_after,
            new_len: new_block.len(),
            old_block,
        });
    }
    after.extend(before[before_cursor..].iter().cloned());

    GeneratedDoc {
        path: path.to_string(),
        before,
        after,
        separator,
        regions: edits,
    }
}

/// Renders one side of a generated document: the lines joined by the
/// document separator, with the final separator present.
fn render(doc: &GeneratedDoc, lines: &[String]) -> Vec<u8> {
    let mut out = String::new();
    for line in lines {
        out.push_str(line);
        out.push_str(&doc.separator);
    }
    out.into_bytes()
}

/// The expected bytes of one local mask: the candidate lines with each
/// selected region's block restored to the baseline block. This is the
/// construction-level oracle, independent of Git and of Margin.
fn restore_regions(doc: &GeneratedDoc, local: usize) -> Vec<u8> {
    let mut lines: Vec<String> = Vec::new();
    let mut cursor = 0;
    for (index, region) in doc.regions.iter().enumerate() {
        lines.extend(doc.after[cursor..region.start_in_after].iter().cloned());
        if local & (1 << index) != 0 {
            lines.extend(region.old_block.iter().cloned());
        } else {
            lines.extend(
                doc.after[region.start_in_after..region.start_in_after + region.new_len]
                    .iter()
                    .cloned(),
            );
        }
        cursor = region.start_in_after + region.new_len;
    }
    lines.extend(doc.after[cursor..].iter().cloned());
    render(doc, &lines)
}

/// The fixed leading case shapes: one file with eight regions in both line
/// endings, then two and three files, so a full eight-group enumeration and
/// the multi-file shapes always run regardless of the case budget.
const FIXED_SHAPES: &[&[(bool, usize)]] = &[
    &[(false, 8)],
    &[(true, 8)],
    &[(false, 4), (false, 4)],
    &[(false, 3), (true, 3), (false, 2)],
];

/// One case shape: per file, whether it uses carriage returns and how many
/// independent regions it holds. The first cases replay the fixed shapes;
/// the rest split a random total inside `min_groups` to `max_groups`.
fn case_shape(
    rng: &mut Rng,
    index: usize,
    min_groups: usize,
    max_groups: usize,
) -> Vec<(bool, usize)> {
    if index < FIXED_SHAPES.len() {
        let fixed: Vec<(bool, usize)> = FIXED_SHAPES[index].to_vec();
        let total: usize = fixed.iter().map(|(_, regions)| regions).sum();
        if total >= min_groups && total <= max_groups {
            return fixed;
        }
    }
    let total = min_groups + rng.below(max_groups - min_groups + 1);
    // Every file needs at least one region, or it holds no change at all.
    let file_count = (1 + rng.below(3)).min(total);
    let mut shape = Vec::new();
    let mut remaining = total;
    for file in 0..file_count {
        let left = file_count - file;
        let count = if left == 1 {
            remaining
        } else {
            let slack = remaining.saturating_sub(left - 1).max(1);
            1 + rng.below(slack)
        };
        remaining -= count;
        shape.push((rng.chance(1, 4), count));
    }
    shape
}

/// Proves one generated case end to end and returns one message per failure.
fn check_generated_case(
    seed: u64,
    index: usize,
    min_groups: usize,
    max_groups: usize,
    sample: bool,
    options: RepoOptions,
) -> Vec<String> {
    let mut rng = Rng::new(seed);
    let shape = case_shape(&mut rng, index, min_groups, max_groups);
    let total: usize = shape.iter().map(|(_, regions)| regions).sum();
    let context = format!("case {index} seed {seed} of {total} groups");
    let mut docs: Vec<GeneratedDoc> = Vec::new();
    for (file, &(crlf, regions)) in shape.iter().enumerate() {
        let token = format!("c{index}f{file}");
        docs.push(generate_doc(
            &mut rng,
            &token,
            &format!("draft-{file}.md"),
            regions,
            crlf,
        ));
    }

    let repo = TestRepo::init_with(options);
    repo.write_file(SENTINEL_PATH, SENTINEL);
    for doc in &docs {
        repo.write_file(&doc.path, &render(doc, &doc.before));
    }
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "generated before"]);
    let before = text(&repo.git_ok(["rev-parse", "HEAD"]));
    for doc in &docs {
        repo.write_file(&doc.path, &render(doc, &doc.after));
    }
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "generated after"]);
    let after = text(&repo.git_ok(["rev-parse", "HEAD"]));

    let manifest = match try_build(&repo, &before, &after) {
        Ok(manifest) => manifest,
        Err(error) => return vec![format!("{context}: {error}")],
    };

    let mut failures = Vec::new();
    let files: BTreeMap<&str, &GroupedFile> = manifest
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();
    if files.len() != docs.len() {
        return vec![format!(
            "{context}: the manifest holds {} files, the case wrote {}",
            files.len(),
            docs.len()
        )];
    }

    let mut documents: Vec<ProofDocument> = Vec::new();
    let mut offset = 0;
    for doc in &docs {
        let file = files[doc.path.as_str()];
        if let Some(reason) = &file.whole_file_reason {
            failures.push(format!(
                "{context}: {} fell back to the whole file: {reason}",
                doc.path
            ));
            continue;
        }
        if file.groups.len() != doc.regions.len() {
            failures.push(format!(
                "{context}: {} holds {} groups, the case generated {} regions",
                doc.path,
                file.groups.len(),
                doc.regions.len()
            ));
            continue;
        }
        if file
            .groups
            .iter()
            .any(|group| group.hunk_indexes.len() != 1)
        {
            failures.push(format!(
                "{context}: a group of {} does not hold exactly one complete hunk",
                doc.path
            ));
            continue;
        }
        // Complete change accounting: the groups partition the file's hunks.
        let mut covered: Vec<usize> = file
            .groups
            .iter()
            .flat_map(|group| group.hunk_indexes.iter().copied())
            .collect();
        covered.sort();
        let complete: Vec<usize> = (0..file.hunks.len()).collect();
        if covered != complete {
            failures.push(format!(
                "{context}: the groups of {} cover {covered:?} of the hunks 0..{}",
                doc.path,
                file.hunks.len()
            ));
        }
        let variants: Vec<Variant> = (0..(1usize << doc.regions.len()))
            .map(|local| Variant {
                bytes: restore_regions(doc, local),
                mode: "100644".to_string(),
                present: true,
            })
            .collect();
        documents.push(ProofDocument {
            path: doc.path.clone(),
            offset,
            groups: doc.regions.len(),
            file_index: manifest
                .files
                .iter()
                .position(|other| other.path == doc.path)
                .expect("the file is in the manifest"),
            variants,
        });
        offset += doc.regions.len();
    }
    if !failures.is_empty() {
        return failures;
    }

    // No generated region changes nothing, and restoring every region of a
    // document recomposes its baseline bytes exactly.
    for document in &documents {
        let doc = docs
            .iter()
            .find(|doc| doc.path == document.path)
            .expect("the document is paired");
        if document.variants[(1 << document.groups) - 1].bytes != render(doc, &doc.before) {
            failures.push(format!(
                "{context}: restoring every region of {} does not recompose the baseline",
                doc.path
            ));
        }
        for local in 0..document.groups {
            if document.variants[1 << local].bytes == document.variants[0].bytes {
                failures.push(format!(
                    "{context}: restoring region {local} of {} alone changes nothing",
                    doc.path
                ));
            }
        }
    }

    let masks: Vec<usize> = if sample {
        sampled_masks(&mut rng, offset)
    } else {
        (0..(1usize << offset)).collect()
    };
    failures.extend(prove_masks(
        &repo, &manifest, &documents, &before, &after, &masks, &context,
    ));
    failures
}

/// The stable subset sample of one larger case: keep-all, restore-all, every
/// singleton, every adjacent pair, both all-but-one masks, and twenty seeded
/// combinations (SPEC 16.4: generated combinations with stable seeds).
fn sampled_masks(rng: &mut Rng, total: usize) -> Vec<usize> {
    let mut masks: Vec<usize> = vec![0, (1 << total) - 1];
    for index in 0..total {
        masks.push(1 << index);
    }
    for index in 1..total {
        masks.push((1 << index) | (1 << (index - 1)));
    }
    masks.push((1 << total) - 2);
    masks.push((1 << total) - 1 - (1 << (total - 1)));
    for _ in 0..20 {
        masks.push((rng.next() as usize) & ((1 << total) - 1));
    }
    masks.sort();
    masks.dedup();
    masks
}

/// Generated manifests of up to eight independent groups restore every keep
/// and restore combination exactly (SPEC 10.6, 16.4).
#[test]
fn generated_manifests_restore_every_subset() {
    let cases = case_budget("MARGIN_RESTORATION_CASES", DEFAULT_CASES).max(4);
    let failures = run_parallel(cases, |index| {
        check_generated_case(
            SEED_SALT.wrapping_add(index as u64),
            index,
            2,
            8,
            false,
            RepoOptions::new(),
        )
    });
    assert!(
        failures.is_empty(),
        "generated restorations disagreed with the oracles:\n  {}",
        failures.join("\n  ")
    );
}

/// Generated manifests with more than eight groups restore a stable sample
/// of keep and restore combinations exactly (SPEC 16.4: larger sets use
/// generated combinations with stable seeds).
#[test]
fn larger_generated_manifests_restore_sampled_subsets() {
    let cases = case_budget("MARGIN_RESTORATION_LARGE_CASES", DEFAULT_LARGE_CASES).max(2);
    let failures = run_parallel(cases, |index| {
        check_generated_case(
            SEED_SALT ^ 0x00A1_1CE5_0000_0000 ^ index as u64,
            index + 1000,
            9,
            12,
            true,
            RepoOptions::new(),
        )
    });
    assert!(
        failures.is_empty(),
        "sampled restorations disagreed with the oracles:\n  {}",
        failures.join("\n  ")
    );
}

/// Reads one case budget from the environment, or returns `fallback`.
fn case_budget(name: &str, fallback: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(fallback)
}

// ---------------------------------------------------------------------------
// Fixed cases: eight groups, out-of-scope persistence, object format
// ---------------------------------------------------------------------------

/// Builds the baseline lines of one flagship draft: two region paragraphs,
/// each behind a six-line gap paragraph.
fn flagship_before(title: &str, gap: &str, blocks: &[(&str, usize)]) -> Vec<String> {
    let mut lines = vec![title.to_string(), String::new()];
    for &(token, count) in blocks {
        for line in 0..6 {
            lines.push(format!("{gap} {token} {line}"));
        }
        lines.push(String::new());
        for line in 0..count {
            lines.push(format!("{token} original {line}"));
        }
        lines.push(String::new());
    }
    lines
}

/// Replaces the contiguous block of `token`-prefixed lines with `count`
/// revised lines.
fn flagship_revise(lines: &[String], token: &str, count: usize) -> Vec<String> {
    let start = lines
        .iter()
        .position(|line| line.starts_with(token))
        .unwrap_or_else(|| panic!("the {token} block is missing"));
    let end = start
        + lines
            .iter()
            .skip(start)
            .take_while(|line| line.starts_with(token))
            .count();
    let mut out: Vec<String> = lines[..start].to_vec();
    for line in 0..count {
        out.push(format!("{token} revised {line}"));
    }
    out.extend(lines[end..].iter().cloned());
    out
}

/// Eight independent groups across six files enumerate every one of their
/// 256 keep and restore combinations (SPEC 10.6): two modified documents
/// with two passages each, an added file, a deleted file, a mode change, and
/// an opaque content change.
#[test]
fn eight_independent_groups_enumerate_every_subset() {
    let repo = TestRepo::init();
    repo.write_file(SENTINEL_PATH, SENTINEL);
    let before_a = flagship_before("# Flagship A", "gap a", &[("first a", 5), ("second a", 5)]);
    let after_a = flagship_revise(&flagship_revise(&before_a, "first a", 3), "second a", 1);
    let before_b = flagship_before("# Flagship B", "gap b", &[("first b", 6), ("second b", 6)]);
    let after_b = flagship_revise(&flagship_revise(&before_b, "first b", 7), "second b", 2);
    let join = |lines: &[String]| -> String {
        let mut out = String::new();
        for line in lines {
            out.push_str(line);
            out.push('\n');
        }
        out
    };

    repo.write_file("draft-a.md", join(&before_a).as_bytes());
    repo.write_file("draft-b.md", join(&before_b).as_bytes());
    repo.write_file("gone.md", b"an old document of the flagship case\n");
    repo.write_file("flip.md", b"same bytes of the flagship case\n");
    repo.write_file("opaque.md", b"caf\xe9 original of the flagship\n");
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "flagship before"]);
    let before = text(&repo.git_ok(["rev-parse", "HEAD"]));

    repo.write_file("draft-a.md", join(&after_a).as_bytes());
    repo.write_file("draft-b.md", join(&after_b).as_bytes());
    repo.write_file("born.md", b"a fresh document of the flagship case\n");
    fs::remove_file(repo.file_path("gone.md")).expect("gone.md exists");
    repo.set_mode("flip.md", 0o755);
    repo.write_file("opaque.md", b"caf\xe9 revised of the flagship\n");
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "flagship after"]);
    let after = text(&repo.git_ok(["rev-parse", "HEAD"]));

    let manifest = grouped_for(&repo, &before, &after);
    let files: BTreeMap<&str, &GroupedFile> = manifest
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();
    let expected_counts = [
        ("born.md", 1usize),
        ("draft-a.md", 2),
        ("draft-b.md", 2),
        ("flip.md", 1),
        ("gone.md", 1),
        ("opaque.md", 1),
    ];
    let mut failures = Vec::new();
    for (path, groups) in expected_counts {
        match files.get(path) {
            Some(file) if file.groups.len() == groups => {}
            Some(file) => failures.push(format!(
                "{path} holds {} groups, {groups} expected",
                file.groups.len()
            )),
            None => failures.push(format!("{path} is missing from the manifest")),
        }
    }
    assert!(
        failures.is_empty(),
        "the flagship manifest does not hold eight independent groups:\n  {}",
        failures.join("\n  ")
    );

    // The expected documents, in manifest order, with their group offsets.
    let documents = vec![
        ProofDocument {
            path: "born.md".to_string(),
            offset: 0,
            groups: 1,
            file_index: 0,
            variants: vec![
                Variant {
                    bytes: b"a fresh document of the flagship case\n".to_vec(),
                    mode: "100644".to_string(),
                    present: true,
                },
                Variant {
                    bytes: Vec::new(),
                    mode: "100644".to_string(),
                    present: false,
                },
            ],
        },
        ProofDocument {
            path: "draft-a.md".to_string(),
            offset: 1,
            groups: 2,
            file_index: 1,
            variants: variants_by_token(&before_a, &after_a, &["first a", "second a"]),
        },
        ProofDocument {
            path: "draft-b.md".to_string(),
            offset: 3,
            groups: 2,
            file_index: 2,
            variants: variants_by_token(&before_b, &after_b, &["first b", "second b"]),
        },
        ProofDocument {
            path: "flip.md".to_string(),
            offset: 5,
            groups: 1,
            file_index: 3,
            variants: vec![
                Variant {
                    bytes: b"same bytes of the flagship case\n".to_vec(),
                    mode: "100755".to_string(),
                    present: true,
                },
                Variant {
                    bytes: b"same bytes of the flagship case\n".to_vec(),
                    mode: "100644".to_string(),
                    present: true,
                },
            ],
        },
        ProofDocument {
            path: "gone.md".to_string(),
            offset: 6,
            groups: 1,
            file_index: 4,
            variants: vec![
                Variant {
                    bytes: Vec::new(),
                    mode: "100644".to_string(),
                    present: false,
                },
                Variant {
                    bytes: b"an old document of the flagship case\n".to_vec(),
                    mode: "100644".to_string(),
                    present: true,
                },
            ],
        },
        ProofDocument {
            path: "opaque.md".to_string(),
            offset: 7,
            groups: 1,
            file_index: 5,
            variants: vec![
                Variant {
                    bytes: b"caf\xe9 revised of the flagship\n".to_vec(),
                    mode: "100644".to_string(),
                    present: true,
                },
                Variant {
                    bytes: b"caf\xe9 original of the flagship\n".to_vec(),
                    mode: "100644".to_string(),
                    present: true,
                },
            ],
        },
    ];
    let total: usize = documents.iter().map(|document| document.groups).sum();
    assert_eq!(total, 8, "the flagship holds eight groups");

    let masks: Vec<usize> = (0..(1usize << total)).collect();
    let failures = prove_masks(
        &repo, &manifest, &documents, &before, &after, &masks, "flagship",
    );
    assert!(
        failures.is_empty(),
        "the flagship subsets disagreed with the oracles:\n  {}",
        failures.join("\n  ")
    );
}

/// Out-of-scope changes stay with the candidate tree in every subset (SPEC
/// 10.3, INV-07): a scoped document with two groups changes beside an
/// unscoped note, and no restoration may rewrite the note's entry.
#[test]
fn out_of_scope_changes_stay_with_the_candidate() {
    let repo = TestRepo::init();
    repo.write_file(SENTINEL_PATH, SENTINEL);
    repo.write_file("note.txt", b"the baseline note\n");
    let before_doc = concat!(
        "# Notes with a neighbor\n",
        "\n",
        "gap line 01\n",
        "gap line 02\n",
        "gap line 03\n",
        "gap line 04\n",
        "gap line 05\n",
        "gap line 06\n",
        "\n",
        "first original passage line\n",
        "\n",
        "gap line 07\n",
        "gap line 08\n",
        "gap line 09\n",
        "gap line 10\n",
        "gap line 11\n",
        "gap line 12\n",
        "\n",
        "second original passage line\n",
    );
    let after_doc = before_doc
        .replace(
            "first original passage line",
            "first revised passage line\nwith two lines",
        )
        .replace("second original passage line\n", "second revised passage");
    repo.write_file("doc.md", before_doc.as_bytes());
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "before"]);
    let before = text(&repo.git_ok(["rev-parse", "HEAD"]));
    repo.write_file("doc.md", after_doc.as_bytes());
    repo.write_file("note.txt", b"the candidate note\n");
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", "after"]);
    let after = text(&repo.git_ok(["rev-parse", "HEAD"]));

    let manifest = grouped_for(&repo, &before, &after);
    assert_eq!(
        manifest.files.len(),
        1,
        "only the scoped document is grouped"
    );
    let file = &manifest.files[0];
    assert_eq!(file.groups.len(), 2, "the two passages are independent");

    // The note entry of the candidate tree enters every expected index, so
    // prove_masks itself proves the out-of-scope isolation; the variant
    // order follows the mask order: keep-all, each passage, restore-all.
    let variants = vec![
        after_doc.clone(),
        after_doc.replace(
            "first revised passage line\nwith two lines",
            "first original passage line",
        ),
        after_doc.replace("second revised passage", "second original passage line\n"),
        before_doc.to_string(),
    ];
    let documents = vec![ProofDocument {
        path: "doc.md".to_string(),
        offset: 0,
        groups: 2,
        file_index: 0,
        variants: variants
            .into_iter()
            .map(|bytes| Variant {
                bytes: bytes.into_bytes(),
                mode: "100644".to_string(),
                present: true,
            })
            .collect(),
    }];
    let masks: Vec<usize> = (0..4).collect();
    let failures = prove_masks(
        &repo,
        &manifest,
        &documents,
        &before,
        &after,
        &masks,
        "out-of-scope",
    );
    assert!(
        failures.is_empty(),
        "the out-of-scope case disagreed with the oracles:\n  {}",
        failures.join("\n  ")
    );

    // The full restoration recomposes the baseline document, but not the
    // baseline tree: the out-of-scope change stays with the candidate.
    repo.git_ok(["read-tree", &after]);
    for group in file.groups.iter().rev() {
        repo.git_stdin(APPLY_ARGV, &group.group.patch_units[0]);
    }
    let tree = write_index_tree(&repo);
    assert_ne!(
        tree,
        commit_tree(&repo, &before),
        "an out-of-scope change must not vanish with a scoped restoration"
    );
    let entries = index_entries(&repo);
    let oid_of = |path: &str| -> String {
        entries[path]
            .split(' ')
            .nth(1)
            .unwrap_or_else(|| panic!("the {path} entry holds a mode and an object ID"))
            .to_string()
    };
    assert_eq!(
        support::oracle::blob(&repo, &oid_of("doc.md")),
        before_doc.as_bytes(),
        "the scoped document must recompose its baseline bytes"
    );
    assert_eq!(
        support::oracle::blob(&repo, &oid_of("note.txt")),
        b"the candidate note\n",
        "the out-of-scope note must keep its candidate bytes"
    );
}

/// Both documented object formats restore exactly (SPEC 16.5): the same
/// generated proof under SHA-256, with no fixed object-ID width anywhere in
/// the application or the oracles.
#[test]
fn sha256_repositories_restore_exactly() {
    let failures = check_generated_case(
        SEED_SALT ^ 0x5A25_6000_0000_0000,
        900,
        4,
        4,
        false,
        RepoOptions::new().with_object_format(ObjectFormat::Sha256),
    );
    assert!(
        failures.is_empty(),
        "the SHA-256 restoration disagreed with the oracles:\n  {}",
        failures.join("\n  ")
    );
}

// ---------------------------------------------------------------------------
// Parallel case driver
// ---------------------------------------------------------------------------

/// Runs `case` for every index in bounded parallel threads and returns one
/// sorted message per failure or panic. The failures stay ordered, so a
/// rerun prints the same list.
fn run_parallel<F>(cases: usize, case: F) -> Vec<String>
where
    F: Fn(usize) -> Vec<String> + Send + Sync,
{
    let workers = thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(2)
        .clamp(1, 4);
    let case = &case;
    let mut failures = Vec::new();
    thread::scope(|scope| {
        let mut handles = Vec::new();
        for worker in 0..workers {
            handles.push(scope.spawn(move || {
                let mut messages = Vec::new();
                let mut index = worker;
                while index < cases {
                    match catch_unwind(AssertUnwindSafe(|| case(index))) {
                        Ok(mut case_failures) => messages.append(&mut case_failures),
                        Err(panic) => {
                            let detail = if let Some(message) = panic.downcast_ref::<&str>() {
                                message.to_string()
                            } else if let Some(message) = panic.downcast_ref::<String>() {
                                message.clone()
                            } else {
                                "an unknown panic".to_string()
                            };
                            messages.push(format!("case {index} panicked: {detail}"));
                        }
                    }
                    index += workers;
                }
                messages
            }));
        }
        for handle in handles {
            match handle.join() {
                Ok(mut messages) => failures.append(&mut messages),
                Err(_) => failures.push("a worker thread panicked".to_string()),
            }
        }
    });
    failures.sort();
    failures
}
