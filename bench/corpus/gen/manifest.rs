//! The corpus manifest of SPEC 17.2: what a generator run recorded and
//! what any rerun must reproduce.
//!
//! The manifest splits into the reproducible record — seed, generator
//! revision, parameters, layout, graph shape, content hashes, expected
//! results, object format, and requested storage state — and the
//! `observed` section, which records what this environment and this Git
//! produced, including the `.git` byte total. A rerun with the same seed
//! and tools must reproduce the first part byte for byte.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::Serialize;

/// Bump when generation logic changes any corpus byte or identifier.
pub const GENERATOR_REVISION: &str = "margin-corpusgen/1";

/// Layout facts of one corpus (SPEC 17.2 "exact byte totals").
#[derive(Clone, Debug, Serialize)]
pub struct LayoutRecord {
    /// Scoped documents in the repository.
    pub documents: usize,
    /// Exact sum of the scoped document sizes at the candidate.
    pub total_document_bytes: u64,
    /// Largest scoped document.
    pub largest_document_bytes: u64,
    /// Total commits on the adopted branch.
    pub commits: usize,
    /// Documents changed between B and C.
    pub changed_documents: usize,
    /// The selected benchmark document.
    pub selected: SelectedRecord,
}

/// The selected document every selected-document metric uses.
#[derive(Clone, Debug, Serialize)]
pub struct SelectedRecord {
    /// Repository-relative path.
    pub path: String,
    /// Exact byte size.
    pub bytes: u64,
    /// Separated edit hunks since the baseline.
    pub changes: usize,
}

/// The commit-graph shape (SPEC 17.2 "commit graph", "distance to last
/// review", "empty-review count").
#[derive(Clone, Debug, Serialize)]
pub struct GraphRecord {
    /// Graph shape: `linear-first-parent`.
    pub shape: &'static str,
    /// Commit counts by kind.
    pub counts: CommitCounts,
    /// Commits after the last review marker.
    pub distance_to_last_review: usize,
    /// The candidate C.
    pub head: String,
    /// Tree of C.
    pub head_tree: String,
    /// The last review marker: the baseline B commit.
    pub baseline_commit: String,
    /// Tree of B.
    pub baseline_tree: String,
}

/// Commit counts by marker kind.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct CommitCounts {
    /// Root start markers.
    pub start: usize,
    /// External commits without Margin markers.
    pub external: usize,
    /// Draft markers.
    pub draft: usize,
    /// Review markers.
    pub review: usize,
    /// Restore markers.
    pub restore: usize,
    /// Empty keep-all review commits.
    pub empty_review: usize,
    /// Tree-changing selective review commits.
    pub selective_review: usize,
}

/// Content hashes of the corpus (SPEC 17.2 "content hashes").
#[derive(Clone, Debug, Serialize)]
pub struct ContentRecord {
    /// Root of the writing scope.
    pub scope_root: String,
    /// Digest of the scoped candidate entries under SPEC 8.6, Git hashing.
    pub scope_content_digest: String,
    /// One record per changed document, in path order.
    pub changed: Vec<ChangedDocument>,
}

/// One changed document between B and C.
#[derive(Clone, Debug, Serialize)]
pub struct ChangedDocument {
    /// Repository-relative path.
    pub path: String,
    /// Change kind; the corpora generate `modify` (C4 adds `add`/`delete`).
    pub kind: String,
    /// Edit classes applied, in order.
    pub classes: Vec<String>,
    /// Edits planned on this document.
    pub edits: usize,
    /// Hunks the fixed diff contract emits for this document.
    pub hunks: usize,
    /// Baseline-side byte size.
    pub before_bytes: u64,
    /// Candidate-side byte size.
    pub after_bytes: u64,
    /// Baseline-side blob object ID.
    pub before_blob: String,
    /// Candidate-side blob object ID.
    pub after_blob: String,
    /// Git hash of the exact patch bytes.
    pub patch_blob: String,
}

/// Expected review results (SPEC 17.2 "expected results"): the trees a
/// keep-all and a restore-all review must produce, and the change-set
/// totals.
#[derive(Clone, Debug, Serialize)]
pub struct ExpectedRecord {
    /// Tree of keeping every change: the candidate tree.
    pub keep_all_tree: String,
    /// Tree of restoring every change: the baseline tree.
    pub restore_all_tree: String,
    /// Changed documents of the B → C comparison.
    pub changed_documents: usize,
    /// Separated change hunks across the whole comparison.
    pub changed_groups: usize,
    /// Out-of-scope sentinel files history never touches.
    pub out_of_scope_sentinels: Vec<String>,
}

/// Requested storage state (SPEC 17.2 "object format", "storage packing
/// state").
#[derive(Clone, Debug, Serialize)]
pub struct StorageRecord {
    /// Repository object format.
    pub object_format: String,
    /// Requested packing state.
    pub packing_state: String,
    /// Whether the adopted `margin.*` settings were written.
    pub margin_settings: bool,
}

/// What this environment measured (never compared across environments,
/// always compared between same-environment reruns).
#[derive(Clone, Debug, Serialize)]
pub struct ObservedRecord {
    /// `git --version` of the generating environment.
    pub git_version: String,
    /// Loose objects after the requested packing step.
    pub loose_objects: u64,
    /// Objects in packs after the requested packing step.
    pub in_pack_objects: u64,
    /// Exact byte sum of every file under `.git`.
    pub git_dir_bytes: u64,
}

/// The Git-state variant extra of a C4 manifest.
#[derive(Clone, Debug, Serialize)]
pub struct VariantRecord {
    /// Variant name, such as `staged`.
    pub name: String,
    /// One-line description of the introduced state.
    pub description: String,
    /// Observable facts a benchmark or oracle can check independently.
    pub expected: BTreeMap<String, serde_json::Value>,
}

/// One complete corpus manifest.
#[derive(Clone, Debug, Serialize)]
pub struct Manifest {
    /// Manifest schema version.
    pub schema_version: u32,
    /// Corpus label, `C1` through `C4`.
    pub corpus: String,
    /// Generator revision; reruns must match it exactly.
    pub generator_revision: String,
    /// The seed this corpus derives from.
    pub seed: u64,
    /// The full parameter set, as JSON.
    pub parameters: serde_json::Value,
    /// Layout facts.
    pub layout: LayoutRecord,
    /// Graph shape.
    pub graph: GraphRecord,
    /// Content hashes.
    pub content: ContentRecord,
    /// Expected results.
    pub expected: ExpectedRecord,
    /// Requested storage state.
    pub storage: StorageRecord,
    /// The C4 variant extra, when this manifest describes a variant.
    pub variant: Option<VariantRecord>,
    /// Environment observations.
    pub observed: ObservedRecord,
}

impl Manifest {
    /// The manifest bytes that a rerun must reproduce exactly: the whole
    /// record without the environment-specific `observed` section.
    pub fn reproducible_bytes(&self) -> Vec<u8> {
        let mut value = serde_json::to_value(self).expect("the manifest serializes");
        if let serde_json::Value::Object(map) = &mut value {
            map.remove("observed");
        }
        serde_json::to_vec_pretty(&value).expect("the manifest serializes")
    }

    /// Writes `manifest.json` into `directory` and returns the bytes
    /// written.
    pub fn write(&self, directory: &Path) -> Vec<u8> {
        let body = serde_json::to_vec_pretty(self).expect("the manifest serializes");
        let path = directory.join("manifest.json");
        fs::write(&path, &body)
            .unwrap_or_else(|error| panic!("cannot write {}: {error}", path.display()));
        body
    }
}
