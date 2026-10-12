//! Corpus construction: documents, history, Git-state variants, packing,
//! and measurement (SPEC 17.2).
//!
//! One build produces a repository plus its manifest:
//!
//! ```text
//! <output>/repo          the corpus repository
//! <output>/gen-home      private fixture HOME
//! <output>/manifest.json the reproducible record
//! ```
//!
//! The history is linear: a root `start` marker holding only the scoped
//! documents, one external commit with out-of-scope sentinels, history
//! drafts broken by keep-all and selective reviews, and one draft per
//! changed document after the last review. Every marker carries the
//! reserved trailers of SPEC 8.3, assembled through the product's own
//! metadata types, and every commit runs through stock Git with fixed
//! identity and dates, so object identifiers are pure functions of the
//! seed.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use margin::domain::{
    self, CommitKind, CommitMetadata, ContentEntry, EntryMode, RootEntry, ScopeDescriptor,
};

use super::edits::{self, EditKind};
use super::manifest::{
    ChangedDocument, CommitCounts, ContentRecord, ExpectedRecord, GENERATOR_REVISION, GraphRecord,
    LayoutRecord, Manifest, ObservedRecord, SelectedRecord, StorageRecord, VariantRecord,
};
use super::plan::{
    DocumentPlanItem, Flavor, HistoryLayout, Packing, Parameters, SCOPE_ROOT, SCOPE_SUFFIX,
    plan_documents,
};
use super::rng::Rng;
use super::runner::{BRANCH, ObjectFormat, Runner};
use super::text;

/// The fixed source-patch contract of SPEC 10.1, restated from the engine
/// settings: the generator and the manifest measurements must not drift
/// from the comparison contract the product itself pins.
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

/// Out-of-scope sentinels every corpus commits after the start marker.
/// History never touches them again (SPEC 16.3 sentinel discipline).
const SENTINELS: &[(&str, &[u8])] = &[
    ("README.md", b"Corpus repository for Margin benchmarks.\n"),
    (
        "assets/keep.bin",
        &[0x89, b'P', b'N', b'G', 0x00, 0x01, 0x02, 0xFF],
    ),
    ("settings/notes.txt", b"out-of-scope plain sentinel\n"),
];

/// The repeated sentence of the pathological repeated-line documents. It
/// carries a claim and swap-pair words so single-line edits apply.
const REPEATED_LINE: &str =
    "The tally records 3 of 4 repeated lines, and the quick guide keeps the same words again. ";

/// The C4 Git-state variants (SPEC 17.2: staging, untracked/ignored
/// files, add/delete, hooks, conflicts, and storage-policy cases, plus
/// object-format and packing variants of the same shape).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    /// A scoped edit sits staged in the index.
    Staged,
    /// Untracked scoped files beside ignored ones.
    UntrackedIgnored,
    /// One draft commit adds and deletes scoped documents.
    AddDelete,
    /// Executable pre-commit and commit-msg hooks.
    Hooks,
    /// An in-progress merge with one conflicted document.
    Conflict,
    /// A `.gitattributes` line that breaks the identity storage profile.
    StoragePolicy,
    /// The C4 shape in the SHA-256 object format.
    Sha256,
    /// The C4 shape without the packing step.
    Loose,
}

impl Variant {
    /// All variants, in manifest order.
    pub const ALL: &'static [Variant] = &[
        Variant::Staged,
        Variant::UntrackedIgnored,
        Variant::AddDelete,
        Variant::Hooks,
        Variant::Conflict,
        Variant::StoragePolicy,
        Variant::Sha256,
        Variant::Loose,
    ];

    /// The manifest name.
    pub fn name(self) -> &'static str {
        match self {
            Variant::Staged => "staged",
            Variant::UntrackedIgnored => "untracked-ignored",
            Variant::AddDelete => "add-delete",
            Variant::Hooks => "hooks",
            Variant::Conflict => "conflict",
            Variant::StoragePolicy => "storage-policy",
            Variant::Sha256 => "sha256",
            Variant::Loose => "loose",
        }
    }

    /// One line describing the introduced state.
    pub fn describe(self) -> &'static str {
        match self {
            Variant::Staged => "one scoped edit is staged in the index",
            Variant::UntrackedIgnored => {
                "two untracked scoped files exist beside one ignored scoped file"
            }
            Variant::AddDelete => "a draft commit adds two and deletes one scoped document",
            Variant::Hooks => "executable pre-commit and commit-msg hooks are installed",
            Variant::Conflict => "an in-progress merge holds one conflicted document",
            Variant::StoragePolicy => "committed .gitattributes apply text=auto to the scope",
            Variant::Sha256 => "the same shape stored in the sha256 object format",
            Variant::Loose => "the same shape with objects left loose",
        }
    }

    /// The parameter adjustment the variant needs before generation.
    fn adjust(self, base: &Parameters) -> Parameters {
        match self {
            Variant::Sha256 => base.clone().with_object_format(ObjectFormat::Sha256),
            Variant::Loose => base.clone().with_packing(Packing::Loose),
            _ => base.clone(),
        }
    }
}

/// One built corpus: its output directory and manifest.
pub struct CorpusResult {
    /// The directory holding `repo/` and `manifest.json`.
    pub root: PathBuf,
    /// The written manifest.
    pub manifest: Manifest,
}

/// Generates one corpus of C1, C2, or C3 shape.
pub fn generate(parameters: &Parameters, output: &Path) -> CorpusResult {
    parameters.validate();
    build(parameters, output, None)
}

/// Generates one C4 Git-state variant from `base`.
pub fn generate_variant(variant: Variant, base: &Parameters, output: &Path) -> CorpusResult {
    let mut parameters = variant.adjust(base);
    parameters.corpus = "C4";
    parameters.validate();
    build(&parameters, output, Some(variant))
}

fn build(parameters: &Parameters, output: &Path, variant: Option<Variant>) -> CorpusResult {
    if output.exists() {
        panic!(
            "the corpus output {} already exists; the generator never overwrites",
            output.display()
        );
    }
    fs::create_dir_all(output)
        .unwrap_or_else(|error| panic!("cannot create {}: {error}", output.display()));
    let builder = Builder::start(parameters, output);
    let manifest = builder.build_corpus(variant);
    manifest.write(output);
    CorpusResult {
        root: output.to_path_buf(),
        manifest,
    }
}

/// The working state of one document.
struct DocState {
    plan: DocumentPlanItem,
    /// Body lines (prose) or the whole document (single-line flavors).
    lines: Vec<String>,
    /// Post-review edit classes, for the manifest.
    applied: Vec<EditKind>,
}

/// One scoped tree entry with its blob size.
struct ScopedEntry {
    path: String,
    oid: String,
    size: u64,
}

/// Everything the build tracks while it constructs history.
struct Builder {
    runner: Runner,
    parameters: Parameters,
    layout: HistoryLayout,
    scope_spec: String,
    scope_digest: String,
    docs: Vec<DocState>,
    counts: CommitCounts,
    /// The current baseline marker: the start commit, later the newest
    /// review commit.
    baseline_commit: String,
    /// The document of the newest history draft and its pre-draft lines,
    /// which a selective review restores — in the file and in this state,
    /// so no later edit can silently re-apply a reverted change.
    last_draft: Option<(usize, Vec<String>)>,
    review_ordinal: usize,
    draft_ordinal: usize,
    /// Expected observations the C4 variant records in its manifest.
    variant_expected: BTreeMap<String, serde_json::Value>,
}

impl Builder {
    fn start(parameters: &Parameters, output: &Path) -> Self {
        let runner = Runner::init(output, parameters.object_format);
        let descriptor = ScopeDescriptor::assemble(
            &[RootEntry {
                components: vec![SCOPE_ROOT.to_string()],
                is_dir_root: true,
            }],
            &[],
            &[SCOPE_SUFFIX.to_string()],
        )
        .expect("the corpus scope assembles");
        let scope_spec = String::from_utf8(descriptor.canonical_bytes())
            .expect("the canonical descriptor is UTF-8");
        let scope_digest = format!(
            "{}:{}",
            parameters.object_format.as_str(),
            runner.hash_object(&descriptor.digest_input_bytes())
        );
        Builder {
            layout: HistoryLayout::of(parameters),
            runner,
            scope_spec,
            scope_digest,
            docs: Vec::new(),
            counts: CommitCounts {
                start: 0,
                external: 0,
                draft: 0,
                review: 0,
                restore: 0,
                empty_review: 0,
                selective_review: 0,
            },
            parameters: parameters.clone(),
            baseline_commit: String::new(),
            last_draft: None,
            review_ordinal: 0,
            draft_ordinal: 0,
            variant_expected: BTreeMap::new(),
        }
    }

    /// Allowed edit kinds of one document.
    fn allowed_kinds(plan: &DocumentPlanItem) -> Vec<EditKind> {
        if plan.flavor.is_single_line() {
            edits::single_line_kinds()
        } else {
            edits::prose_kinds(plan.flavor.is_wrapped())
        }
    }

    /// Materializes document `index` at its first version.
    fn materialize(&self, index: usize, plan: DocumentPlanItem) -> DocState {
        let mut rng = Rng::seeded(self.parameters.seed ^ (0xD0C_0000_0000 + index as u64));
        let lines = match plan.flavor {
            Flavor::ProseLong | Flavor::ProseWrapped => {
                let draft = text::draft(&mut rng, body_target(plan.bytes));
                text::document_body(&draft, plan.flavor.is_wrapped())
            }
            Flavor::LongParagraph | Flavor::HardWrapped => {
                // Both members of the pair render the same sentences, so
                // the corpus holds a long paragraph and its hard-wrapped
                // equivalent (SPEC 17.2).
                let pair_seed = self.parameters.seed
                    ^ (0xABBA_0000_0000_0000 + pair_ordinal(index, &self.parameters) as u64);
                let mut pair_rng = Rng::seeded(pair_seed);
                let draft = text::draft(&mut pair_rng, body_target(plan.bytes));
                text::document_body(&draft, plan.flavor.is_wrapped())
            }
            Flavor::MassParagraph => vec![text::mass_line(&mut rng, plan.bytes)],
            Flavor::RepeatedLines => text::repeated_lines(REPEATED_LINE, plan.bytes),
        };
        DocState {
            plan,
            lines,
            applied: Vec::new(),
        }
    }

    /// Renders the current version of one document at its exact size.
    fn render(&self, index: usize) -> Vec<u8> {
        render_doc(&self.docs[index])
    }

    /// Writes and stages one document, returning the resulting index tree.
    fn stage(&mut self, index: usize) -> String {
        let bytes = self.render(index);
        let path = self.docs[index].plan.path.clone();
        self.runner.write(&path, &bytes);
        self.runner.ok(["add", "--", &path]);
        self.index_tree()
    }

    /// The tree of the current index.
    fn index_tree(&self) -> String {
        self.runner.text(["write-tree"])
    }

    /// Commits the current index as one Margin marker (SPEC 8.3).
    #[allow(clippy::too_many_arguments)]
    fn commit_marker(
        &mut self,
        kind: CommitKind,
        candidate: &str,
        result_tree: &str,
        baseline: Option<&str>,
        request: Option<String>,
        plan: Option<String>,
        subject: &str,
        allow_empty: bool,
    ) -> String {
        let metadata = CommitMetadata {
            kind,
            scope_digest: self.scope_digest.clone(),
            scope_spec: self.scope_spec.clone(),
            candidate: candidate.to_string(),
            result_tree: result_tree.to_string(),
            baseline: baseline.map(str::to_string),
            request,
            plan,
            source: None,
        };
        let message = metadata
            .commit_message(subject)
            .expect("the corpus metadata is valid");
        self.runner.commit_message(&message, allow_empty)
    }

    /// The plan-fingerprint identity of one review, hashed through Git
    /// exactly like the product's identities (SPEC 8.6).
    fn review_fingerprint(&self, ordinal: usize, baseline: &str, head: &str, tree: &str) -> String {
        let mut input = domain::PLAN_FINGERPRINT_DOMAIN.to_vec();
        input.extend_from_slice(
            format!("bench-review\0{ordinal}\0{baseline}\0{head}\0{tree}\0").as_bytes(),
        );
        format!(
            "{}:{}",
            self.parameters.object_format.as_str(),
            self.runner.hash_object(&input)
        )
    }

    /// One ordinary draft: edit one ordinary document and commit it as a
    /// draft marker. The mass paragraph stays out of the history loop, so
    /// a 10 MiB rewrite never happens per commit.
    fn history_draft(&mut self, rng: &mut Rng) {
        let ordinary: Vec<usize> = (0..self.docs.len())
            .filter(|index| self.docs[*index].plan.flavor != Flavor::MassParagraph)
            .collect();
        let index = ordinary[rng.below(ordinary.len())];
        let before = self.docs[index].lines.clone();
        let planned = if rng.chance(50) { 2 } else { 1 };
        let allowed = Self::allowed_kinds(&self.docs[index].plan);
        edits::apply(&mut self.docs[index].lines, rng, planned, &allowed);
        let tree = self.stage(index);
        self.draft_ordinal += 1;
        let subject = format!("bench draft {}: writing pass", self.draft_ordinal);
        let candidate = self.runner.head();
        self.commit_marker(
            CommitKind::Draft,
            &candidate,
            &tree,
            None,
            None,
            None,
            &subject,
            false,
        );
        self.counts.draft += 1;
        self.last_draft = Some((index, before));
    }

    /// One review: keep-all (an empty commit) or selective (restoring the
    /// previous draft's document state).
    fn review(&mut self) {
        self.review_ordinal += 1;
        let baseline = self.baseline_commit.clone();
        let selective = self.review_ordinal % 2 == 1 && self.last_draft.is_some();
        let head = self.runner.head();
        let request = format!(
            "bench-{}-r{:04}",
            self.parameters.corpus, self.review_ordinal
        );
        let subject = if selective {
            format!("bench review {}: selective restore", self.review_ordinal)
        } else {
            format!("bench review {}: keep all", self.review_ordinal)
        };
        let (tree, allow_empty) = if selective {
            let (index, before) = self.last_draft.take().expect("checked above");
            self.docs[index].lines = before;
            let bytes = self.render(index);
            let path = self.docs[index].plan.path.clone();
            self.runner.write(&path, &bytes);
            self.runner.ok(["add", "--", &path]);
            (self.index_tree(), false)
        } else {
            (self.runner.tree_of("HEAD"), true)
        };
        let plan = self.review_fingerprint(self.review_ordinal, &baseline, &head, &tree);
        let commit = self.commit_marker(
            CommitKind::Review,
            &head,
            &tree,
            Some(&baseline),
            Some(request),
            Some(plan),
            &subject,
            allow_empty,
        );
        self.counts.review += 1;
        if selective {
            self.counts.selective_review += 1;
        } else {
            self.counts.empty_review += 1;
        }
        self.baseline_commit = commit;
    }

    /// One external commit without Margin trailers.
    fn external_commit(&mut self, subject: &str) -> String {
        self.counts.external += 1;
        let message = format!("{subject}\n");
        self.runner.commit_message(message.as_bytes(), false)
    }

    /// Applies one document's planned post-review edits and commits the
    /// draft.
    fn post_review_draft(&mut self, rng: &mut Rng, index: usize) {
        let planned = self.docs[index].plan.edits;
        let allowed = Self::allowed_kinds(&self.docs[index].plan);
        let applied = edits::apply(&mut self.docs[index].lines, rng, planned, &allowed);
        self.docs[index].applied = applied;
        let tree = self.stage(index);
        self.draft_ordinal += 1;
        let subject = format!(
            "bench draft {}: {}",
            self.draft_ordinal, self.docs[index].plan.path
        );
        let candidate = self.runner.head();
        self.commit_marker(
            CommitKind::Draft,
            &candidate,
            &tree,
            None,
            None,
            None,
            &subject,
            false,
        );
        self.counts.draft += 1;
    }

    /// Writes the adopted `margin.*` settings (SPEC 8.2), exactly the keys
    /// an explicit init of this scope would store.
    fn write_settings(&mut self) {
        self.runner.config("margin.version", "1");
        self.runner.config("margin.branch", BRANCH);
        self.runner.config("margin.scope", &self.scope_spec);
    }

    /// Applies the requested packing state.
    ///
    /// The repack runs single-threaded on purpose: Git's parallel delta
    /// search picks different delta bases run to run, which would make the
    /// pack bytes — and with them the recorded `.git` size — differ between
    /// two runs of the same seed. One thread makes the packed state a pure
    /// function of the object set.
    fn pack(&mut self) {
        if self.parameters.packing == Packing::Packed {
            self.runner
                .ok(["-c", "pack.threads=1", "repack", "-a", "-d", "--quiet"]);
            self.runner.ok(["prune-packed"]);
            self.runner.ok(["pack-refs", "--all", "--prune"]);
        }
    }

    /// Runs the whole corpus construction for one parameter set.
    fn build_corpus(mut self, variant: Option<Variant>) -> Manifest {
        let parameters = self.parameters.clone();

        // Documents and the root start marker: its tree holds only the
        // eligible scoped entries (SPEC 5.1).
        let plan = plan_documents(&parameters);
        self.docs = plan
            .items
            .into_iter()
            .enumerate()
            .map(|(index, item)| self.materialize(index, item))
            .collect();
        for index in 0..self.docs.len() {
            let bytes = self.render(index);
            let path = self.docs[index].plan.path.clone();
            self.runner.write(&path, &bytes);
        }
        self.runner.ok(["add", "--", SCOPE_ROOT]);
        let tree = self.index_tree();
        let start = self.commit_marker(
            CommitKind::Start,
            "unborn",
            &tree,
            None,
            None,
            None,
            "bench start: adopt the writing scope",
            false,
        );
        self.counts.start += 1;
        self.baseline_commit = start;

        // Out-of-scope sentinels through one external commit.
        for (path, bytes) in SENTINELS {
            self.runner.write(path, bytes);
            self.runner.ok(["add", "--", path]);
        }
        self.external_commit("bench: add out-of-scope sentinels");

        // History: leading drafts, then reviews between evenly spaced
        // drafts, so the final history commit is the last review.
        let mut rng = Rng::seeded(parameters.seed ^ 0x051E_D1C7);
        let layout = self.layout;
        let leading = layout.leading_drafts();
        let gap = layout.review_gap();
        let mut reviews_emitted = 0;
        for step in 0..layout.history_drafts {
            self.history_draft(&mut rng);
            let passed = step + 1;
            let at_boundary = passed > leading
                && (passed - leading).is_multiple_of(gap)
                && passed - leading <= gap * layout.reviews;
            let at_end = passed == layout.history_drafts;
            if (at_boundary || at_end) && reviews_emitted < layout.reviews {
                self.review();
                reviews_emitted += 1;
            }
        }
        assert_eq!(reviews_emitted, layout.reviews, "the review layout broke");

        // Post-review drafts: exactly the changed documents.
        for index in 0..self.docs.len() {
            if self.docs[index].plan.changed {
                self.post_review_draft(&mut rng, index);
            }
        }

        // The C4 Git-state variant mutation.
        if let Some(variant) = variant {
            self.apply_variant(variant);
        }

        self.write_settings();
        self.pack();
        self.measure(variant)
    }

    /// Introduces one Git-state variant after the base history.
    fn apply_variant(&mut self, variant: Variant) {
        match variant {
            Variant::Staged => {
                let index = self.first_changed_prose();
                let mut rng = Rng::seeded(self.parameters.seed ^ 0x57A_06ED);
                let allowed = Self::allowed_kinds(&self.docs[index].plan);
                edits::apply(&mut self.docs[index].lines, &mut rng, 1, &allowed);
                let bytes = self.render(index);
                let path = self.docs[index].plan.path.clone();
                self.runner.write(&path, &bytes);
                self.runner.ok(["add", "--", &path]);
                self.variant_expected
                    .insert("staged_paths".into(), serde_json::json!([path]));
                self.variant_expected.insert(
                    "staged_index_differs_from_head".into(),
                    serde_json::json!(true),
                );
            }
            Variant::UntrackedIgnored => {
                self.runner.write(".gitignore", b"writing/ignored/\n");
                self.runner.ok(["add", "--", ".gitignore"]);
                self.external_commit("bench: ignore an inner scope directory");
                self.runner.write(
                    "writing/untracked/extra-01.md",
                    b"Untracked scoped document one.\n",
                );
                self.runner.write(
                    "writing/untracked/extra-02.md",
                    b"Untracked scoped document two.\n",
                );
                self.runner.write(
                    "writing/ignored/hidden.md",
                    b"Ignored scoped document, never eligible.\n",
                );
                self.variant_expected.insert(
                    "untracked_scoped".into(),
                    serde_json::json!([
                        "writing/untracked/extra-01.md",
                        "writing/untracked/extra-02.md",
                    ]),
                );
                self.variant_expected.insert(
                    "ignored_scoped".into(),
                    serde_json::json!(["writing/ignored/hidden.md"]),
                );
            }
            Variant::AddDelete => {
                self.runner
                    .write("writing/added/added-01.md", &added_document(1));
                self.runner
                    .write("writing/added/added-02.md", &added_document(2));
                let deleted = self.first_unchanged();
                let deleted_path = self.docs[deleted].plan.path.clone();
                self.runner.remove(&deleted_path);
                self.runner.ok(["add", "-A", "--", SCOPE_ROOT]);
                let tree = self.index_tree();
                self.draft_ordinal += 1;
                let candidate = self.runner.head();
                self.commit_marker(
                    CommitKind::Draft,
                    &candidate,
                    &tree,
                    None,
                    None,
                    None,
                    "bench draft: add and delete documents",
                    false,
                );
                self.counts.draft += 1;
                self.variant_expected.insert(
                    "added_documents".into(),
                    serde_json::json!(["writing/added/added-01.md", "writing/added/added-02.md",]),
                );
                self.variant_expected.insert(
                    "deleted_documents".into(),
                    serde_json::json!([deleted_path]),
                );
            }
            Variant::Hooks => {
                for hook in ["pre-commit", "commit-msg"] {
                    let path = self.runner.repo().join(".git/hooks").join(hook);
                    fs::write(&path, "#!/bin/sh\nexit 0\n")
                        .unwrap_or_else(|error| panic!("cannot write {hook}: {error}"));
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                        .unwrap_or_else(|error| panic!("cannot chmod {hook}: {error}"));
                }
                self.variant_expected.insert(
                    "hooks".into(),
                    serde_json::json!(["commit-msg", "pre-commit"]),
                );
            }
            Variant::Conflict => {
                let index = self.first_unchanged();
                let path = self.docs[index].plan.path.clone();
                let original = self.docs[index].lines.clone();
                let middle = middle_line(&original);
                let side = conflicting_line(&original[middle], ConflictSide::Side);
                let main = conflicting_line(&original[middle], ConflictSide::Main);
                self.runner.ok(["checkout", "--quiet", "-b", "bench-side"]);
                let plan = self.docs[index].plan.clone();
                let side_bytes = render_lines(&plan, &side_line_set(&original, middle, side));
                self.runner.write(&path, &side_bytes);
                self.runner.ok(["add", "--", &path]);
                // The side commit lives off the adopted branch, so it never
                // joins the counted first-parent chain.
                let message = b"bench: side edit\n";
                self.runner.commit_message(message, false);
                self.runner.ok(["checkout", "--quiet", BRANCH]);
                let main_bytes = render_lines(&plan, &side_line_set(&original, middle, main));
                self.runner.write(&path, &main_bytes);
                self.runner.ok(["add", "--", &path]);
                self.external_commit("bench: main edit");
                let output = self.runner.run(["merge", "bench-side"]);
                assert!(
                    !output.status.success(),
                    "the fixture merge must conflict over {path}"
                );
                self.variant_expected
                    .insert("unmerged_paths".into(), serde_json::json!([path]));
                self.variant_expected
                    .insert("merge_in_progress".into(), serde_json::json!(true));
                self.variant_expected
                    .insert("adopted_branch".into(), serde_json::json!(BRANCH));
            }
            Variant::StoragePolicy => {
                self.runner.write(".gitattributes", b"*.md text=auto\n");
                self.runner.ok(["add", "--", ".gitattributes"]);
                self.external_commit("bench: apply a text storage policy");
                self.variant_expected.insert(
                    "attributes".into(),
                    serde_json::json!({
                        "path": "writing/selected.md",
                        "attr": "text",
                        "value": "auto",
                    }),
                );
                self.variant_expected.insert(
                    "identity_profile_supported".into(),
                    serde_json::json!(false),
                );
            }
            Variant::Sha256 | Variant::Loose => {
                // Pure parameter variants: the shape itself is the state.
            }
        }
    }

    /// The first changed ordinary prose document.
    fn first_changed_prose(&self) -> usize {
        (0..self.docs.len())
            .find(|index| {
                let plan = &self.docs[*index].plan;
                plan.changed && !plan.flavor.is_single_line()
            })
            .expect("a changed prose document exists")
    }

    /// The first document outside the change set.
    fn first_unchanged(&self) -> usize {
        (0..self.docs.len())
            .find(|index| {
                let plan = &self.docs[*index].plan;
                !plan.changed && !plan.flavor.is_single_line()
            })
            .expect("an unchanged prose document exists")
    }

    /// The scoped blob entries of `tree` with their exact sizes, from
    /// `git ls-tree -r -l -z`.
    fn scoped_entries(&self, tree: &str) -> Vec<ScopedEntry> {
        let raw =
            String::from_utf8_lossy(&self.runner.feed(["ls-tree", "-r", "-l", "-z", tree], b""))
                .into_owned();
        let mut entries = Vec::new();
        for record in raw.split('\0') {
            if record.is_empty() {
                continue;
            }
            let Some((info, path)) = record.split_once('\t') else {
                continue;
            };
            let fields: Vec<&str> = info.split_whitespace().collect();
            if fields.len() == 4 && fields[1] == "blob" && path.starts_with(SCOPE_ROOT) {
                entries.push(ScopedEntry {
                    path: path.to_string(),
                    oid: fields[2].to_string(),
                    size: fields[3].parse().unwrap_or(0),
                });
            }
        }
        entries
    }

    /// The blob of `path` in `tree`, when present.
    fn blob_at(&self, tree: &str, path: &str) -> Option<String> {
        let raw =
            String::from_utf8_lossy(&self.runner.feed(["ls-tree", "-z", tree, "--", path], b""))
                .into_owned();
        for record in raw.split('\0') {
            if record.is_empty() {
                continue;
            }
            let Some((info, _)) = record.split_once('\t') else {
                continue;
            };
            let fields: Vec<&str> = info.split_whitespace().collect();
            if fields.len() >= 3 && fields[1] == "blob" {
                return Some(fields[2].to_string());
            }
        }
        None
    }

    /// The size of one object.
    fn object_size(&self, oid: &str) -> u64 {
        self.runner
            .text(["cat-file", "-s", oid])
            .parse()
            .unwrap_or_else(|error| panic!("cannot read the size of {oid}: {error}"))
    }

    /// The exact patch bytes of one path between two trees under the
    /// fixed contract of SPEC 10.1.
    fn patch_of(&self, base: &str, target: &str, path: &str) -> Vec<u8> {
        let mut argv: Vec<String> = DIFF_TREE_PATCH_ARGV
            .iter()
            .map(|argument| (*argument).to_string())
            .collect();
        argv.push(base.to_string());
        argv.push(target.to_string());
        argv.push("--".to_string());
        argv.push(path.to_string());
        self.runner.ok(argv)
    }

    /// Measures the finished repository and assembles the manifest.
    fn measure(&self, variant: Option<Variant>) -> Manifest {
        let runner = &self.runner;
        let head = runner.head();
        let head_tree = runner.tree_of(&head);
        let baseline_commit = self.baseline_commit.clone();
        let baseline_tree = runner.tree_of(&baseline_commit);

        // Commit count, linearity, and distance to the last review, from
        // Git itself.
        let listed = runner.text(["rev-list", "HEAD"]);
        let commit_list: Vec<&str> = listed.lines().collect();
        let merges: usize = runner
            .text(["rev-list", "--merges", "--count", "HEAD"])
            .parse()
            .unwrap_or_default();
        assert_eq!(merges, 0, "the corpus history must be linear");
        let distance = commit_list
            .iter()
            .position(|commit| *commit == baseline_commit)
            .expect("the baseline is reachable");
        let counted =
            self.counts.start + self.counts.external + self.counts.draft + self.counts.review;
        assert_eq!(
            commit_list.len(),
            counted,
            "the measured commit count must match the generator's bookkeeping"
        );

        // Scoped entries and exact sizes. The add-delete variant is the
        // one build that changes the scoped document set after planning.
        let entries = self.scoped_entries(&head_tree);
        let added = variant == Some(Variant::AddDelete);
        let expected_documents = if added {
            self.docs.len() + 1
        } else {
            self.docs.len()
        };
        assert_eq!(
            entries.len(),
            expected_documents,
            "the candidate tree must hold exactly the scoped documents"
        );
        for doc in &self.docs {
            if let Some(entry) = entries.iter().find(|entry| entry.path == doc.plan.path) {
                assert_eq!(
                    entry.size as usize, doc.plan.bytes,
                    "{} must hold its exact planned size",
                    doc.plan.path
                );
            } else {
                assert!(
                    added,
                    "{} vanished from the candidate tree outside the add-delete variant",
                    doc.plan.path
                );
            }
        }
        let total_bytes: u64 = entries.iter().map(|entry| entry.size).sum();
        let largest = entries.iter().map(|entry| entry.size).max().unwrap_or(0);
        let digest_entries: Vec<ContentEntry> = entries
            .iter()
            .map(|entry| ContentEntry {
                path: entry.path.clone(),
                mode: EntryMode::Regular,
                blob_oid: entry.oid.clone(),
            })
            .collect();
        let scope_content_digest = format!(
            "{}:{}",
            self.parameters.object_format.as_str(),
            runner.hash_object(&domain::content_digest_input_bytes(&digest_entries))
        );

        // Changed documents from the real B → C tree comparison.
        let changed_raw = runner.text([
            "diff-tree".to_string(),
            "-r".to_string(),
            "--name-only".to_string(),
            "--no-commit-id".to_string(),
            "-z".to_string(),
            baseline_tree.clone(),
            head_tree.clone(),
        ]);
        // The review comparison covers the scope; out-of-scope changes
        // such as variant setup files never enter it.
        let changed_paths: Vec<String> = split_nul(&changed_raw)
            .into_iter()
            .filter(|path| path.starts_with(SCOPE_ROOT))
            .collect();
        if variant.is_none() {
            let mut planned: Vec<&str> = self
                .docs
                .iter()
                .filter(|doc| doc.plan.changed)
                .map(|doc| doc.plan.path.as_str())
                .collect();
            planned.sort_unstable();
            assert_eq!(
                changed_paths, planned,
                "the B -> C comparison must hold exactly the planned change set"
            );
        }

        let bookkeeping: BTreeMap<&str, &DocState> = self
            .docs
            .iter()
            .map(|doc| (doc.plan.path.as_str(), doc))
            .collect();
        let mut changed = Vec::new();
        for path in &changed_paths {
            let before_blob = self.blob_at(&baseline_tree, path);
            let after_blob = self.blob_at(&head_tree, path);
            let before_bytes = before_blob
                .as_ref()
                .map(|oid| self.object_size(oid))
                .unwrap_or(0);
            let after_bytes = after_blob
                .as_ref()
                .map(|oid| self.object_size(oid))
                .unwrap_or(0);
            let kind = match (&before_blob, &after_blob) {
                (None, Some(_)) => "add",
                (Some(_), None) => "delete",
                _ => "modify",
            };
            let patch = self.patch_of(&baseline_tree, &head_tree, path);
            let hunks = count_hunks(&patch);
            let doc = bookkeeping.get(path.as_str());
            let edits = doc.map(|doc| doc.applied.len()).unwrap_or(0);
            let classes: Vec<String> = doc
                .map(|doc| {
                    doc.applied
                        .iter()
                        .map(|kind| kind.label().to_string())
                        .collect()
                })
                .unwrap_or_default();
            if kind == "modify"
                && let Some(doc) = doc
                && doc.plan.changed
            {
                // Variant-introduced modifications (the conflict fixture's
                // divergent edit) carry no post-review edit bookkeeping.
                assert_eq!(
                    hunks, edits,
                    "the fixed contract must emit one hunk per edit of {path}"
                );
            }
            changed.push(ChangedDocument {
                path: path.clone(),
                kind: kind.to_string(),
                classes,
                edits,
                hunks,
                before_bytes,
                after_bytes,
                before_blob: before_blob.unwrap_or_default(),
                after_blob: after_blob.unwrap_or_default(),
                patch_blob: format!(
                    "{}:{}",
                    self.parameters.object_format.as_str(),
                    runner.hash_object(&patch)
                ),
            });
        }
        let changed_groups: usize = changed.iter().map(|entry| entry.hunks).sum();
        let selected = self
            .docs
            .iter()
            .find(|doc| doc.plan.selected)
            .expect("the selected document exists");
        let selected_entry = changed
            .iter()
            .find(|entry| entry.path == selected.plan.path)
            .expect("the selected document is changed");

        let counts = count_objects(&runner.text(["count-objects", "-v"]));
        let git_version = String::from_utf8_lossy(&runner.ok(["--version"]))
            .trim()
            .to_string();
        let git_dir_bytes = dir_bytes(&runner.repo().join(".git"));

        Manifest {
            schema_version: 1,
            corpus: self.parameters.corpus.to_string(),
            generator_revision: GENERATOR_REVISION.to_string(),
            seed: self.parameters.seed,
            parameters: serde_json::to_value(&self.parameters).expect("parameters serialize"),
            layout: LayoutRecord {
                documents: entries.len(),
                total_document_bytes: total_bytes,
                largest_document_bytes: largest,
                commits: commit_list.len(),
                changed_documents: changed.len(),
                selected: SelectedRecord {
                    path: selected.plan.path.clone(),
                    bytes: selected.plan.bytes as u64,
                    changes: selected_entry.hunks,
                },
            },
            graph: GraphRecord {
                shape: "linear-first-parent",
                counts: self.counts,
                distance_to_last_review: distance,
                head,
                head_tree,
                baseline_commit,
                baseline_tree,
            },
            content: ContentRecord {
                scope_root: SCOPE_ROOT.to_string(),
                scope_content_digest,
                changed,
            },
            expected: ExpectedRecord {
                keep_all_tree: runner.tree_of(&runner.head()),
                restore_all_tree: runner.tree_of(&self.baseline_commit),
                changed_documents: changed_paths.len(),
                changed_groups,
                out_of_scope_sentinels: SENTINELS
                    .iter()
                    .map(|(path, _)| path.to_string())
                    .collect(),
            },
            storage: StorageRecord {
                object_format: self.parameters.object_format.as_str().to_string(),
                packing_state: self.parameters.packing.as_str().to_string(),
                margin_settings: true,
            },
            variant: variant.map(|variant| VariantRecord {
                name: variant.name().to_string(),
                description: variant.describe().to_string(),
                expected: self.variant_expected.clone(),
            }),
            observed: ObservedRecord {
                git_version,
                loose_objects: counts.loose,
                in_pack_objects: counts.in_pack,
                git_dir_bytes,
            },
        }
    }
}

/// Splits NUL-delimited Git output into trimmed nonempty strings.
fn split_nul(raw: &str) -> Vec<String> {
    raw.split('\0')
        .filter(|record| !record.is_empty())
        .map(str::to_string)
        .collect()
}

/// Counts `@@` hunk headers in one patch.
fn count_hunks(patch: &[u8]) -> usize {
    patch
        .split(|byte| *byte == b'\n')
        .filter(|line| line.starts_with(b"@@ "))
        .count()
}

/// The parsed output of `git count-objects -v`.
struct ObjectCounts {
    loose: u64,
    in_pack: u64,
}

/// Parses `git count-objects -v`, whose fields use `name: value` lines.
fn count_objects(raw: &str) -> ObjectCounts {
    let mut counts = ObjectCounts {
        loose: 0,
        in_pack: 0,
    };
    for line in raw.lines() {
        if let Some(value) = line.strip_prefix("count:") {
            counts.loose = value.trim().parse().unwrap_or(0);
        }
        if let Some(value) = line.strip_prefix("in-pack:") {
            counts.in_pack = value.trim().parse().unwrap_or(0);
        }
    }
    counts
}

/// The exact byte sum of every regular file under `directory`.
fn dir_bytes(directory: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![directory.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(read) = fs::read_dir(&current) else {
            continue;
        };
        for entry in read.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                stack.push(entry.path());
            } else if metadata.is_file() {
                total += metadata.len();
            }
        }
    }
    total
}

/// The body byte budget of one prose document: the exact size minus the
/// deterministic tail reserve.
fn body_target(bytes: usize) -> usize {
    bytes - (bytes / 8).clamp(64, 2048)
}

/// The equivalent-pair ordinal of document `index`.
fn pair_ordinal(index: usize, parameters: &Parameters) -> usize {
    let mut first = 1;
    if parameters.pathological.mass_paragraph_bytes > 0 {
        first += 1;
    }
    first += parameters.pathological.repeated_line_documents;
    index.saturating_sub(first) / 2
}

/// Renders one document at its exact recorded size.
fn render_doc(doc: &DocState) -> Vec<u8> {
    render_lines(&doc.plan, &doc.lines)
}

/// Renders `lines` of one planned document to its exact recorded size.
fn render_lines(plan: &DocumentPlanItem, lines: &[String]) -> Vec<u8> {
    let bytes = plan.bytes;
    let mut content = String::new();
    for line in lines {
        content.push_str(line);
        content.push('\n');
    }
    if plan.flavor.is_single_line() {
        assert_eq!(
            content.len(),
            bytes,
            "the single-line document {} must keep its exact size",
            plan.path
        );
        return content.into_bytes();
    }
    let tail = text::Tail::render(content.len(), bytes);
    content.push_str(&tail);
    assert_eq!(
        content.len(),
        bytes,
        "the document {} must keep its exact size",
        plan.path
    );
    content.into_bytes()
}

/// The middle non-blank line index of `lines`.
fn middle_line(lines: &[String]) -> usize {
    let non_blank: Vec<usize> = (0..lines.len())
        .filter(|index| !lines[*index].trim().is_empty())
        .collect();
    non_blank[non_blank.len() / 2]
}

/// Which side of the conflict fixture a line variant belongs to.
enum ConflictSide {
    Side,
    Main,
}

/// Produces one same-length variant of `line`: the chosen side replaces a
/// punctuation mark, so both variants differ from each other and from the
/// original at the same position.
fn conflicting_line(line: &str, side: ConflictSide) -> String {
    let mut variant = line.to_string();
    let pairs = [('.', '!', '?'), (',', ';', ':')];
    for (mark, side_mark, main_mark) in pairs {
        if let Some(at) = variant.find(mark) {
            let replacement = match side {
                ConflictSide::Side => side_mark,
                ConflictSide::Main => main_mark,
            };
            variant.replace_range(at..at + 1, &replacement.to_string());
            return variant;
        }
    }
    // No punctuation: flip the case of the first lowercase letter.
    if let Some(at) = variant.bytes().position(|byte| byte.is_ascii_lowercase()) {
        let bytes = unsafe { variant.as_bytes_mut() };
        if matches!(side, ConflictSide::Side) {
            bytes[at] = bytes[at].to_ascii_uppercase();
        }
    }
    variant
}

/// Rebuilds the line set with `line` replaced at `at`.
fn side_line_set(lines: &[String], at: usize, line: String) -> Vec<String> {
    let mut replaced = lines.to_vec();
    replaced[at] = line;
    replaced
}

/// The fixed content of one added document of the add-delete variant.
fn added_document(ordinal: usize) -> Vec<u8> {
    let mut content = String::new();
    content.push_str(&format!("# Added bench document {ordinal}\n\n"));
    let filler = "Added documents exercise the whole-file addition groups of a review. ";
    while content.len() + filler.len() <= 2 * 1024 {
        content.push_str(filler);
    }
    content.push('\n');
    content.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunk_counting_reads_headers() {
        let patch = b"--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n context\n-old\n+new\n@@ -20,3 +20,3 @@\n c\n-o\n+n\n";
        assert_eq!(count_hunks(patch), 2);
    }

    #[test]
    fn conflicting_lines_differ_same_length() {
        let line = "The quick guide shows the tally again.";
        let side = conflicting_line(line, ConflictSide::Side);
        let main = conflicting_line(line, ConflictSide::Main);
        assert_eq!(side.len(), line.len());
        assert_eq!(main.len(), line.len());
        assert_ne!(side, main);
        assert_ne!(side, line);
        assert_ne!(main, line);
    }
}
