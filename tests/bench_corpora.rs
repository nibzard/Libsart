//! Acceptance suite of task T007: the reproducible benchmark corpora of
//! SPEC 17.2.
//!
//! The generators under `bench/corpus/` create C1, C2, C3, and C4 from a
//! recorded seed, and each build writes a `manifest.json`. This suite
//! proves the acceptance criteria:
//!
//! - the full-scale definitions are exactly the sizes and histories of
//!   SPEC 17.2, and C4 covers every required Git-state class;
//! - every corpus rerun reproduces its manifest — the reproducible record
//!   byte for byte, and the observed storage state within one
//!   environment;
//! - the layout, graph shape, marker validity, exact byte totals, changed
//!   set, hunk counts, expected result trees, and recorded expectations
//!   hold against stock Git, never against the generator's own numbers.
//!
//! The default scale keeps the developer suite quick; set
//! `MARGIN_BENCH_FULL_C1=1`, `MARGIN_BENCH_FULL_C2=1`, or
//! `MARGIN_BENCH_FULL_C3=1` to run one corpus at the exact SPEC sizes
//! (C3 needs about 1 GiB plus its object store, and on the order of an
//! hour on a laptop-class machine).

mod support;

#[path = "../bench/corpus/gen/mod.rs"]
mod corpus;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use corpus::{Parameters, Variant};
use support::env::IsolatedEnv;
use tempfile::TempDir;

/// The fixed source-patch contract of SPEC 10.1, restated on purpose: an
/// oracle that imports the engine it checks proves nothing (SPEC 16.3).
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

/// The out-of-scope sentinels every corpus commits after its start
/// marker; stock Git must find them byte-identical at the candidate.
const SENTINELS: &[&str] = &["README.md", "assets/keep.bin", "settings/notes.txt"];

/// An independent stock-Git oracle over one generated repository. It
/// shares no code with the generator's runner: this file builds its own
/// commands from the isolated environment (SPEC 16.3).
struct Oracle {
    env: IsolatedEnv,
    repo: PathBuf,
}

impl Oracle {
    /// Builds the oracle for `repo` with a private, disposable home.
    fn at(repo: &Path, home: &Path) -> Self {
        fs::create_dir_all(home).expect("the oracle home exists");
        Oracle {
            env: IsolatedEnv::new(home),
            repo: repo.to_path_buf(),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        command.current_dir(&self.repo).args(args);
        for (key, _) in std::env::vars_os() {
            if key.as_encoded_bytes().starts_with(b"GIT_") || key == "EMAIL" {
                command.env_remove(&key);
            }
        }
        command.envs(self.env.vars());
        command.stdin(Stdio::null());
        command
    }

    /// Runs Git and returns the raw output.
    fn run(&self, args: &[&str]) -> Output {
        self.command(args)
            .output()
            .unwrap_or_else(|_error| panic!("cannot run git {} in tests", args.join(" ")))
    }

    /// Runs Git, asserts success, and returns decoded stdout.
    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "git {} failed in {}\nstderr: {}",
            args.join(" "),
            self.repo.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// One record of `git log` output per commit, newest first. Records
    /// end in an explicit `\x1e` sentinel because trailer elements carry
    /// their own newlines and would otherwise split in the middle.
    fn log(&self, format: &str) -> Vec<String> {
        let wrapped = format!("{format}\x1e");
        let raw = self.ok(&["log", "--first-parent", &format!("--format={wrapped}")]);
        raw.split('\x1e')
            .map(str::trim)
            .filter(|record| !record.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Runs the fixed contract patch of one path between two trees.
    fn patch(&self, base: &str, target: &str, path: &str) -> String {
        let mut argv: Vec<&str> = DIFF_TREE_PATCH_ARGV.to_vec();
        argv.push(base);
        argv.push(target);
        argv.push("--");
        argv.push(path);
        self.ok(&argv)
    }

    /// The exact working bytes of one repository-relative file.
    fn file(&self, relative: &str) -> Vec<u8> {
        fs::read(self.repo.join(relative))
            .unwrap_or_else(|error| panic!("cannot read {relative}: {error}"))
    }
}

/// Counts `@@` hunk headers of one patch.
fn hunks_of(patch: &str) -> usize {
    patch.lines().filter(|line| line.starts_with("@@ ")).count()
}

/// The parameters of one corpus at the scale this run uses: the test
/// scale by default, the exact SPEC sizes behind the opt-in variables.
fn parameters(full_variable: &str, base: Parameters) -> Parameters {
    if std::env::var(full_variable).as_deref() == Ok("1") {
        base
    } else {
        base.test_scale()
    }
}

/// Generates one corpus into a fresh temporary directory and returns it
/// with its oracle.
fn build(base: &Parameters) -> (TempDir, corpus::CorpusResult, Oracle) {
    let root = TempDir::new().expect("a temporary corpus root");
    let output = root.path().join("corpus");
    let built = corpus::generate(base, &output);
    let oracle = Oracle::at(&output.join("repo"), &root.path().join("oracle-home"));
    (root, built, oracle)
}

/// Generates one C4 variant into a fresh temporary directory.
fn build_variant(variant: Variant, base: &Parameters) -> (TempDir, corpus::CorpusResult, Oracle) {
    let root = TempDir::new().expect("a temporary variant root");
    let output = root.path().join(variant.name());
    let built = corpus::generate_variant(variant, base, &output);
    let oracle = Oracle::at(&output.join("repo"), &root.path().join("oracle-home"));
    (root, built, oracle)
}

/// Asserts that a second run of the same parameters reproduces the
/// manifest exactly, including the observed storage state.
fn assert_rerun_reproduces(built: &corpus::CorpusResult, base: &Parameters) {
    let second_root = TempDir::new().expect("a second temporary root");
    let second = corpus::generate(base, &second_root.path().join("corpus"));
    assert_eq!(
        built.manifest.reproducible_bytes(),
        second.manifest.reproducible_bytes(),
        "a rerun must reproduce the manifest of {}",
        base.corpus
    );
    assert_eq!(
        built.manifest.observed.git_dir_bytes, second.manifest.observed.git_dir_bytes,
        "a rerun must reproduce the .git storage size"
    );
    assert_eq!(
        built.manifest.observed.loose_objects,
        second.manifest.observed.loose_objects
    );
    assert_eq!(
        built.manifest.observed.in_pack_objects,
        second.manifest.observed.in_pack_objects
    );
}

/// The full-scale definitions are the SPEC 17.2 numbers, and C4 covers
/// every Git-state class the corpus definition requires.
#[test]
fn definitions_match_spec_17_2() {
    let c1 = Parameters::c1();
    assert_eq!(c1.documents, 100);
    assert_eq!(c1.commits, 200);
    assert_eq!(c1.changed_documents, 10);
    assert_eq!(c1.selected_changes, 10);
    assert_eq!(
        match c1.sizes {
            corpus::plan::SizePlan::Uniform { document_bytes } => document_bytes,
            _ => panic!("C1 is uniform"),
        } * c1.documents,
        100 * 20 * 1024
    );

    let c2 = Parameters::c2();
    let (total, max, changed_total, selected_bytes) = match c2.sizes {
        corpus::plan::SizePlan::ExactTotal {
            total,
            max,
            changed_total,
            selected_bytes,
        } => (total, max, changed_total, selected_bytes),
        _ => panic!("C2 uses an exact total"),
    };
    assert_eq!(c2.documents, 1000);
    assert_eq!(total, 100 * 1024 * 1024);
    assert_eq!(max, 1024 * 1024);
    assert_eq!(changed_total, 5 * 1024 * 1024);
    assert_eq!(selected_bytes, 100 * 1024);
    assert_eq!(c2.commits, 1000);
    assert_eq!(c2.changed_documents, 50);
    assert_eq!(c2.selected_changes, 50);

    let c3 = Parameters::c3();
    assert_eq!(c3.documents, 10_000);
    assert_eq!(
        match c3.sizes {
            corpus::plan::SizePlan::ExactTotal { total, .. } => total,
            _ => panic!("C3 uses an exact total"),
        },
        1024 * 1024 * 1024
    );
    assert_eq!(c3.commits, 10_000);
    assert_eq!(c3.pathological.mass_paragraph_bytes, 10 * 1024 * 1024);
    assert!(c3.pathological.repeated_line_documents >= 1);

    // C4 must cover staging, untracked/ignored files, add/delete, hooks,
    // conflicts, and storage-policy cases (SPEC 17.2).
    let names: BTreeSet<&str> = Variant::ALL.iter().map(|variant| variant.name()).collect();
    for required in [
        "staged",
        "untracked-ignored",
        "add-delete",
        "hooks",
        "conflict",
        "storage-policy",
    ] {
        assert!(names.contains(required), "C4 must include {required}");
    }
}

/// C1 at the default scale: reproducible, exact, linear, and structurally
/// valid for Margin's own marker recognizer.
#[test]
fn c1_generates_a_reproducible_daily_desk() {
    let base = parameters("MARGIN_BENCH_FULL_C1", Parameters::c1());
    let (_root, built, oracle) = build(&base);
    let manifest = &built.manifest;

    // The manifest self-declares its provenance.
    assert_eq!(manifest.corpus, "C1");
    assert_eq!(manifest.seed, base.seed);
    assert_eq!(
        manifest.generator_revision,
        corpus::GENERATOR_REVISION,
        "the manifest must name its generator revision"
    );
    assert_eq!(manifest.storage.object_format, "sha1");
    assert_eq!(manifest.storage.packing_state, base.packing.as_str());

    assert_conformant(&built, &oracle, &base, None);
    assert_classes_covered(manifest);
    assert_rerun_reproduces(&built, &base);
}

/// C2 at the default scale: the exact-total allocations hold.
#[test]
fn c2_generates_the_release_envelope() {
    let base = parameters("MARGIN_BENCH_FULL_C2", Parameters::c2());
    let (_root, built, oracle) = build(&base);
    let manifest = &built.manifest;

    let (total, max, changed_total, selected_bytes) = match base.sizes {
        corpus::plan::SizePlan::ExactTotal {
            total,
            max,
            changed_total,
            selected_bytes,
        } => (total, max as u64, changed_total, selected_bytes as u64),
        _ => unreachable!(),
    };
    assert_eq!(manifest.layout.total_document_bytes, total);
    assert!(
        manifest.layout.largest_document_bytes <= max,
        "no document may exceed the maximum"
    );
    let changed_total_measured: u64 = manifest
        .content
        .changed
        .iter()
        .map(|entry| entry.after_bytes)
        .sum();
    assert_eq!(changed_total_measured, changed_total);
    assert_eq!(
        manifest
            .content
            .changed
            .iter()
            .find(|entry| entry.path == manifest.layout.selected.path)
            .expect("the selected document is changed")
            .after_bytes,
        selected_bytes
    );
    assert_eq!(manifest.layout.selected.changes, base.selected_changes);

    assert_conformant(&built, &oracle, &base, None);
    assert_classes_covered(manifest);
    assert_rerun_reproduces(&built, &base);
}

/// C3 at the default scale: the pathological documents are included in
/// the exact total.
#[test]
fn c3_generates_the_stress_shape() {
    let base = parameters("MARGIN_BENCH_FULL_C3", Parameters::c3());
    let (_root, built, oracle) = build(&base);
    let manifest = &built.manifest;

    let (total, mass) = (
        match base.sizes {
            corpus::plan::SizePlan::ExactTotal { total, .. } => total,
            _ => unreachable!(),
        },
        base.pathological.mass_paragraph_bytes,
    );
    assert_eq!(manifest.layout.total_document_bytes, total);

    // The mass paragraph is exactly one line of its exact size.
    let mass_path = format!("{}/d01/doc-00001.md", manifest.content.scope_root);
    let mass_bytes = oracle.file(&mass_path);
    assert_eq!(mass_bytes.len(), mass);
    assert_eq!(
        mass_bytes.iter().filter(|byte| **byte == b'\n').count(),
        1,
        "the mass paragraph must be one line"
    );

    // The repeated-line documents hold their identical sentence over and
    // over, and one of them is part of the change set.
    let repeated = format!("{}/d02/doc-00002.md", manifest.content.scope_root);
    let repeated_bytes = oracle.file(&repeated);
    let first_line = repeated_bytes
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or(&[])
        .to_vec();
    assert!(first_line.len() > 40);
    assert!(
        repeated_bytes.len() as u64 >= base.pathological.repeated_line_bytes as u64,
        "the repeated-line document keeps its planned size"
    );

    assert_conformant(&built, &oracle, &base, None);
    assert_classes_covered(manifest);
    assert_rerun_reproduces(&built, &base);
}

/// Every C4 variant records expectations an independent Git oracle can
/// confirm, and one variant reruns reproducibly.
#[test]
fn c4_variants_record_independent_state() {
    let base = Parameters::c4_base().test_scale();
    for variant in Variant::ALL {
        let (_root, built, oracle) = build_variant(*variant, &base);
        let manifest = &built.manifest;
        assert_eq!(manifest.corpus, "C4");
        assert_eq!(
            manifest.variant.as_ref().expect("a variant record").name,
            variant.name()
        );
        assert_conformant(&built, &oracle, &base, Some(*variant));
        match variant {
            Variant::Staged => {
                let staged: Vec<String> =
                    manifest.variant.as_ref().unwrap().expected["staged_paths"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|value| value.as_str().unwrap().to_string())
                        .collect();
                assert_eq!(staged.len(), 1);
                let status = oracle.ok(&["status", "--porcelain"]);
                assert!(
                    status.starts_with("M  "),
                    "the staged edit must lead the porcelain status: {status}"
                );
                let diff_index = oracle.ok(&[
                    "diff-index",
                    "--name-only",
                    "--cached",
                    &manifest.graph.head,
                ]);
                assert_eq!(diff_index, staged[0]);
                assert_rerun_reproduces_variant(*variant, &base);
            }
            Variant::UntrackedIgnored => {
                let status = oracle.ok(&["status", "--porcelain", "--untracked-files=all"]);
                assert!(
                    status.contains("?? writing/untracked/extra-01.md")
                        && status.contains("?? writing/untracked/extra-02.md"),
                    "the untracked scoped files must be visible: {status}"
                );
                assert!(
                    !status.contains("writing/ignored/hidden.md"),
                    "the ignored scoped file must stay hidden: {status}"
                );
                let check = oracle.ok(&["check-ignore", "-q", "writing/ignored/hidden.md"]);
                assert_eq!(check, "");
            }
            Variant::AddDelete => {
                let changed: Vec<&str> = manifest
                    .content
                    .changed
                    .iter()
                    .map(|entry| entry.path.as_str())
                    .collect();
                assert!(changed.contains(&"writing/added/added-01.md"));
                assert!(changed.contains(&"writing/added/added-02.md"));
                let kinds: Vec<&str> = manifest
                    .content
                    .changed
                    .iter()
                    .map(|entry| entry.kind.as_str())
                    .collect();
                assert_eq!(kinds.iter().filter(|kind| **kind == "add").count(), 2);
                assert_eq!(kinds.iter().filter(|kind| **kind == "delete").count(), 1);
                let deleted: Vec<&str> = manifest
                    .content
                    .changed
                    .iter()
                    .filter(|entry| entry.kind == "delete")
                    .map(|entry| entry.path.as_str())
                    .collect();
                assert!(!oracle.repo.join(deleted[0]).exists());
            }
            Variant::Hooks => {
                for hook in ["pre-commit", "commit-msg"] {
                    let path = oracle.repo.join(".git/hooks").join(hook);
                    let metadata = fs::metadata(&path).expect("the hook exists");
                    use std::os::unix::fs::PermissionsExt;
                    assert!(metadata.permissions().mode() & 0o111 != 0, "{hook} runs");
                }
            }
            Variant::Conflict => {
                // One conflicted path reports its three index stages.
                let unmerged = oracle.ok(&["ls-files", "--unmerged"]);
                assert_eq!(unmerged.lines().count(), 3);
                let conflicted: BTreeSet<&str> = unmerged
                    .lines()
                    .map(|line| line.split('\t').nth(1).unwrap_or(line))
                    .collect();
                assert_eq!(conflicted.len(), 1);
                assert!(oracle.repo.join(".git/MERGE_HEAD").exists());
                let branch = oracle.ok(&["symbolic-ref", "--short", "HEAD"]);
                assert_eq!(branch, "main");
            }
            Variant::StoragePolicy => {
                let attribute = oracle.ok(&["check-attr", "text", "--", "writing/selected.md"]);
                assert_eq!(attribute, "writing/selected.md: text: auto");
            }
            Variant::Sha256 => {
                assert_eq!(manifest.storage.object_format, "sha256");
                let format = oracle.ok(&["rev-parse", "--show-object-format"]);
                assert_eq!(format, "sha256");
                assert_eq!(manifest.graph.head.len(), 64);
            }
            Variant::Loose => {
                assert_eq!(manifest.storage.packing_state, "loose");
                assert!(manifest.observed.loose_objects > 0);
                assert_eq!(manifest.observed.in_pack_objects, 0);
            }
        }
    }
}

/// A different seed produces a different corpus with the same shape: the
/// streams are seeded, not accidental constants.
#[test]
fn seeds_change_the_corpus() {
    let base = Parameters::c1().test_scale();
    let (_root, first, _oracle) = build(&base);
    let (_second_root, second, _oracle) = build(&base.with_seed(0x0B00_1E5E));
    assert_ne!(
        first.manifest.graph.head, second.manifest.graph.head,
        "a different seed must produce a different history"
    );
    assert_ne!(
        first.manifest.content.scope_content_digest,
        second.manifest.content.scope_content_digest
    );
    assert_eq!(
        first.manifest.layout.documents, second.manifest.layout.documents,
        "the shape stays parameterized"
    );
    assert_eq!(
        first.manifest.layout.commits,
        second.manifest.layout.commits
    );
}

/// The manually curated holdout set of SPEC 17.2 exists, is authored, and
/// is documented as excluded from optimization tuning.
#[test]
fn holdout_set_is_curated_and_excluded() {
    let readme = Path::new("bench/holdout/README.md");
    assert!(readme.exists(), "bench/holdout/README.md must exist");
    let text = fs::read_to_string(readme).expect("the holdout README is readable");
    let normalized: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        normalized.contains("not used to tune"),
        "the holdout protocol must state its exclusion from tuning"
    );
    let manifest = fs::read_to_string(Path::new("bench/holdout/manifest.json"))
        .expect("the holdout manifest exists");
    let parsed: serde_json::Value = serde_json::from_str(&manifest).expect("valid JSON");
    let pairs = parsed["pairs"].as_array().expect("a pairs array");
    assert!(pairs.len() >= 3, "the holdout set is small but real");
    for pair in pairs {
        for side in ["before", "after"] {
            let path = Path::new("bench/holdout").join(pair[side].as_str().expect(side));
            assert!(path.exists(), "{} must exist", path.display());
            assert!(
                fs::metadata(&path).unwrap().len() > 0,
                "{} holds content",
                path.display()
            );
        }
    }
}

/// The shared conformance oracle: layout, graph, markers, exact sizes,
/// changed set, hunks, and expected result trees, all through stock Git.
///
/// `variant` is `None` for the plain corpora; a variant's setup commits
/// add to the base commit budget by the recorded amount.
fn assert_conformant(
    built: &corpus::CorpusResult,
    oracle: &Oracle,
    base: &Parameters,
    variant: Option<Variant>,
) {
    let manifest = &built.manifest;
    let head = &manifest.graph.head;
    let baseline = &manifest.graph.baseline_commit;

    // Layout against independent Git.
    assert_eq!(oracle.ok(&["rev-parse", "HEAD"]), *head);
    let count: usize = oracle.ok(&["rev-list", "--count", "HEAD"]).parse().unwrap();
    assert_eq!(count, manifest.layout.commits);
    let extra = match variant {
        None => 0,
        // One setup commit each: the ignore rule, the add/delete draft,
        // the divergent main edit, and the storage-policy attributes.
        Some(Variant::UntrackedIgnored)
        | Some(Variant::AddDelete)
        | Some(Variant::Conflict)
        | Some(Variant::StoragePolicy) => 1,
        Some(_) => 0,
    };
    assert_eq!(
        count,
        base.commits + extra,
        "the commit budget is exact for the corpus and its variant"
    );
    assert_eq!(
        oracle
            .ok(&["rev-list", "--merges", "--count", "HEAD"])
            .parse::<usize>()
            .unwrap(),
        0,
        "the history is linear"
    );

    // Graph shape: every marker kind is where Git says it is.
    let lines = oracle.log("%H%x00%T%x00%P%x00%(trailers:key=Margin-Kind,valueonly)");
    assert_eq!(lines.len(), count);
    let mut starts = 0;
    let mut reviews = 0;
    let mut empty_reviews = 0;
    let mut externals = 0;
    let mut drafts_seen = 0;
    let mut distance = None;
    for (position, line) in lines.iter().enumerate() {
        let fields: Vec<&str> = line.split('\0').collect();
        assert_eq!(fields.len(), 4, "one parsed log line: {line}");
        let (commit, tree, parents, kind) = (fields[0], fields[1], fields[2], fields[3].trim());
        match kind {
            "" => externals += 1,
            "draft" => drafts_seen += 1,
            "start" => {
                starts += 1;
                assert!(parents.is_empty(), "the start marker is the root");
            }
            "review" => {
                reviews += 1;
                if distance.is_none() {
                    distance = Some(position);
                }
                let parent_tree = oracle.ok(&["rev-parse", &format!("{}^{{tree}}", parents)]);
                if tree == parent_tree {
                    empty_reviews += 1;
                }
            }
            other => panic!("unexpected Margin-Kind {other} in {commit}"),
        }
    }
    assert_eq!(externals, manifest.graph.counts.external);
    assert_eq!(drafts_seen, manifest.graph.counts.draft);
    assert_eq!(starts, 1);
    assert_eq!(reviews, manifest.graph.counts.review);
    assert_eq!(empty_reviews, manifest.graph.counts.empty_review);
    assert!(manifest.graph.counts.empty_review >= 1);
    assert!(manifest.graph.counts.selective_review >= 1);
    assert_eq!(
        distance.expect("a review exists"),
        manifest.graph.distance_to_last_review,
        "the distance to the last review counts commits after it"
    );
    assert_eq!(oracle.ok(&["rev-parse", baseline]), *baseline);
    assert_eq!(
        oracle.ok(&["rev-parse", &format!("{baseline}^{{tree}}")]),
        manifest.graph.baseline_tree
    );

    // Marker validity through the product's own recognizer: the corpus is
    // a Margin fixture, so its markers must parse as valid (SPEC 8.4).
    let adapter = margin_git(oracle);
    let commits = oracle.log("%H");
    let mut valid_markers = 0;
    for commit in &commits {
        if margin::app::validate_marker(&adapter, commit).is_ok() {
            valid_markers += 1;
        }
    }
    let expected_markers =
        manifest.graph.counts.start + manifest.graph.counts.draft + manifest.graph.counts.review;
    assert_eq!(
        valid_markers, expected_markers,
        "exactly the marker commits are structurally valid"
    );

    // Exact scoped sizes and the document set, from `git ls-tree -r -l`.
    let listed = oracle.ok(&["ls-tree", "-r", "-l", "HEAD"]);
    let scoped: Vec<&str> = listed
        .lines()
        .filter(|line| {
            line.split('\t')
                .nth(1)
                .is_some_and(|path| path.starts_with("writing/") && path.ends_with(".md"))
        })
        .collect();
    assert_eq!(scoped.len(), manifest.layout.documents);
    let total: u64 = scoped
        .iter()
        .map(|line| {
            line.split_whitespace()
                .nth(3)
                .expect("a size column")
                .parse::<u64>()
                .expect("a numeric size")
        })
        .sum();
    assert_eq!(total, manifest.layout.total_document_bytes);
    for line in scoped {
        let path = line.split('\t').nth(1).expect("a path");
        assert!(
            path.starts_with("writing/"),
            "every scoped document lives under the scope root"
        );
    }

    // The changed set and per-document hunk counts under the fixed
    // contract.
    let baseline_tree = &manifest.graph.baseline_tree;
    let head_tree = &manifest.graph.head_tree;
    let changed_names = oracle.ok(&[
        "diff-tree",
        "-r",
        "--name-only",
        "--no-commit-id",
        "-z",
        baseline_tree,
        head_tree,
    ]);
    let changed: Vec<String> = changed_names
        .split('\0')
        .filter(|name| !name.is_empty() && name.starts_with("writing/"))
        .map(str::to_string)
        .collect();
    let recorded: Vec<&str> = manifest
        .content
        .changed
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    assert_eq!(
        changed.iter().map(String::as_str).collect::<Vec<_>>(),
        recorded,
        "the recorded change set is the Git-observed one"
    );
    let mut hunks = 0;
    for entry in &manifest.content.changed {
        let patch = oracle.patch(baseline_tree, head_tree, &entry.path);
        assert_eq!(hunks_of(&patch), entry.hunks, "for {}", entry.path);
        assert_eq!(entry.before_blob.is_empty(), entry.kind == "add");
        assert_eq!(entry.after_blob.is_empty(), entry.kind == "delete");
        hunks += entry.hunks;
    }
    assert_eq!(hunks, manifest.expected.changed_groups);

    // The selected document: exact size, exact hunk count, one hunk per
    // separated edit.
    let selected = &manifest.layout.selected;
    let selected_entry = manifest
        .content
        .changed
        .iter()
        .find(|entry| entry.path == selected.path)
        .expect("the selected document is changed");
    assert_eq!(selected_entry.hunks, selected.changes);
    assert_eq!(
        oracle.file(&selected.path).len() as u64,
        selected.bytes,
        "the selected document holds its exact size"
    );

    // Expected result trees: keep-all is the candidate tree, restore-all
    // is the baseline tree (SPEC 17.2 "expected results").
    assert_eq!(manifest.expected.keep_all_tree, *head_tree);
    assert_eq!(manifest.expected.restore_all_tree, *baseline_tree);
    assert_ne!(head_tree, baseline_tree);

    // The out-of-scope sentinels are present and untouched by history.
    for sentinel in SENTINELS {
        assert!(
            listed.contains(sentinel),
            "{sentinel} is committed at the candidate"
        );
        let sentinel_head = oracle.ok(&["rev-parse", &format!("HEAD:{sentinel}")]);
        let sentinel_baseline = oracle.ok(&["rev-parse", &format!("{baseline}:{sentinel}")]);
        assert_eq!(sentinel_head, sentinel_baseline);
    }

    // The adopted settings make the repository operational for Margin:
    // the documented keys exist, the branch matches the adoption, and the
    // scope is the canonical descriptor of the corpus scope.
    let scope = oracle.ok(&["config", "--local", "margin.scope"]);
    let descriptor: serde_json::Value =
        serde_json::from_str(&scope).expect("margin.scope holds the canonical descriptor JSON");
    assert_eq!(descriptor["v"], 1);
    assert_eq!(descriptor["include"], serde_json::json!(["writing/"]));
    assert_eq!(descriptor["suffixes"], serde_json::json!([".md"]));
    assert_eq!(
        oracle.ok(&["config", "--local", "margin.branch"]),
        "main",
        "the adopted branch is recorded"
    );
    assert_eq!(
        oracle.ok(&["config", "--local", "margin.version"]),
        "1",
        "the settings version is recorded"
    );

    // The markers carry the same scope identity the settings describe:
    // canonicalizing the configured descriptor must yield the
    // `Margin-Scope` digest of the history's markers (SPEC 8.4 rule 2).
    let recorded_digest = oracle
        .log("%(trailers:key=Margin-Scope,valueonly)")
        .into_iter()
        .next()
        .expect("one marker exists");
    let parsed: margin::domain::ScopeDescriptor =
        serde_json::from_str(&scope).expect("the configured scope descriptor parses");
    let digest = format!(
        "{}:{}",
        manifest.storage.object_format,
        hash_with(oracle, &parsed.digest_input_bytes())
    );
    assert_eq!(digest.trim(), recorded_digest.trim());
}

/// Hashes `bytes` through the repository's own `git hash-object --stdin`.
fn hash_with(oracle: &Oracle, bytes: &[u8]) -> String {
    let mut command = oracle.command(&["hash-object", "--stdin"]);
    command.stdin(Stdio::piped()).stdout(Stdio::piped());
    let mut child = command.spawn().expect("spawn hash-object");
    use std::io::Write as _;
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(bytes)
        .expect("feed hash-object");
    let output = child.wait_with_output().expect("wait hash-object");
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A Margin Git adapter over the oracle's isolated environment, for the
/// product-side marker-validity check.
fn margin_git(oracle: &Oracle) -> margin::git::Git {
    let vars: Vec<(std::ffi::OsString, std::ffi::OsString)> = oracle
        .env
        .vars()
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    margin::git::Git::from_env(vars).at(&oracle.repo)
}

/// Rerun check for one variant.
fn assert_rerun_reproduces_variant(variant: Variant, base: &Parameters) {
    let root = TempDir::new().expect("a temporary variant root");
    let second = corpus::generate_variant(variant, base, &root.path().join(variant.name()));
    let first = corpus::generate_variant(variant, base, &root.path().join("first"));
    assert_eq!(
        first.manifest.reproducible_bytes(),
        second.manifest.reproducible_bytes(),
        "a rerun of the {} variant must reproduce its manifest",
        variant.name()
    );
    assert_eq!(
        first.manifest.observed.git_dir_bytes,
        second.manifest.observed.git_dir_bytes
    );
}

/// The edit classes of SPEC 17.2 — sparse, dense, pure reflow, claim, and
/// link-target changes — all appear in one corpus.
fn assert_classes_covered(manifest: &corpus::Manifest) {
    let classes: BTreeSet<&str> = manifest
        .content
        .changed
        .iter()
        .flat_map(|entry| entry.classes.iter().map(String::as_str))
        .collect();
    for required in [
        "sparse-word",
        "claim-value",
        "link-target",
        "pure-reflow",
        "dense-rewrite",
    ] {
        assert!(
            classes.contains(required),
            "the corpus must include {required} changes, found {classes:?}"
        );
    }
}
