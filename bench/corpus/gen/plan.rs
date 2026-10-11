//! Corpus definitions, scaling, history layout, and document plans
//! (SPEC 17.2).
//!
//! The full-scale parameters are the exact sizes and histories of SPEC
//! 17.2; the test-scale parameters run the same code paths small enough
//! for the developer suite. Everything derives from a recorded seed, so a
//! rerun reproduces the same documents, edits, and commit graph.

use serde::Serialize;

/// Storage packing state requested for a corpus (SPEC 17.2: setup and
/// packing happen outside timing and the state is recorded).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Packing {
    /// Every object packed: `git repack -a -d`, packed refs.
    Packed,
    /// Objects stay loose exactly as the commits left them.
    Loose,
}

impl Packing {
    /// The manifest spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Packing::Packed => "packed",
            Packing::Loose => "loose",
        }
    }
}

/// How document sizes are allocated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SizePlan {
    /// Every document has the same exact byte size (C1).
    Uniform {
        /// Exact bytes per document.
        document_bytes: usize,
    },
    /// Documents fill one exact total (C2, C3).
    ExactTotal {
        /// Exact sum of all document sizes.
        total: u64,
        /// Largest allowed document.
        max: usize,
        /// Exact sum of the changed documents' candidate-side sizes.
        changed_total: u64,
        /// Exact size of the selected document.
        selected_bytes: usize,
    },
}

/// The pathological documents C3 must include (SPEC 17.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct PathologicalPlan {
    /// Size of the single-paragraph document: one line of this many bytes.
    pub mass_paragraph_bytes: usize,
    /// How many repeated-line documents to include.
    pub repeated_line_documents: usize,
    /// Size of each repeated-line document.
    pub repeated_line_bytes: usize,
}

impl PathologicalPlan {
    /// No pathological documents.
    pub const NONE: PathologicalPlan = PathologicalPlan {
        mass_paragraph_bytes: 0,
        repeated_line_documents: 0,
        repeated_line_bytes: 0,
    };
}

/// One corpus definition. Field values at full scale are the SPEC 17.2
/// numbers; see [`Parameters::c1`], [`Parameters::c2`],
/// [`Parameters::c3`].
#[derive(Clone, Debug, Serialize)]
pub struct Parameters {
    /// Corpus label, such as `C1`.
    pub corpus: &'static str,
    /// The seed every document and edit stream derives from.
    pub seed: u64,
    /// Number of scoped documents.
    pub documents: usize,
    /// Document size allocation.
    pub sizes: SizePlan,
    /// Total commits on the adopted branch, every kind included.
    pub commits: usize,
    /// Documents changed between the baseline B and the candidate C.
    pub changed_documents: usize,
    /// Edit hunks applied to the selected document after the last review.
    pub selected_changes: usize,
    /// Roughly this many history drafts precede each review.
    pub review_cadence: usize,
    /// Long-paragraph documents with a hard-wrapped equivalent document.
    pub long_paragraph_pairs: usize,
    /// Pathological documents (C3).
    pub pathological: PathologicalPlan,
    /// Requested storage packing state.
    pub packing: Packing,
    /// Repository object format.
    pub object_format: super::runner::ObjectFormat,
}

/// Default seeds: the `marg` prefix plus the corpus ordinal, so every
/// corpus restarts identically from its recorded default.
fn default_seed(corpus: u64) -> u64 {
    0x6D61_7267_0000_0000 | corpus
}

/// One KiB, for readable size arithmetic.
const KIB: usize = 1024;

impl Parameters {
    /// C1 — daily desk: 100 documents of 20 KiB, 200 commits, 10 changed
    /// documents, a selected 20 KiB document with 10 changes (SPEC 17.2).
    pub fn c1() -> Self {
        Parameters {
            corpus: "C1",
            seed: default_seed(1),
            documents: 100,
            sizes: SizePlan::Uniform {
                document_bytes: 20 * KIB,
            },
            commits: 200,
            changed_documents: 10,
            selected_changes: 10,
            review_cadence: 18,
            long_paragraph_pairs: 4,
            pathological: PathologicalPlan::NONE,
            packing: Packing::Packed,
            object_format: super::runner::ObjectFormat::Sha1,
        }
    }

    /// C2 — release envelope: 1,000 documents totaling exactly 100 MiB,
    /// 1,000 commits, 50 changed documents totaling 5 MiB, a selected
    /// 100 KiB document with 50 changes, maximum file 1 MiB (SPEC 17.2).
    pub fn c2() -> Self {
        Parameters {
            corpus: "C2",
            seed: default_seed(2),
            documents: 1000,
            sizes: SizePlan::ExactTotal {
                total: 100 * KIB as u64 * KIB as u64,
                max: KIB * KIB,
                changed_total: 5 * KIB as u64 * KIB as u64,
                selected_bytes: 100 * KIB,
            },
            commits: 1000,
            changed_documents: 50,
            selected_changes: 50,
            review_cadence: 18,
            long_paragraph_pairs: 6,
            pathological: PathologicalPlan::NONE,
            packing: Packing::Packed,
            object_format: super::runner::ObjectFormat::Sha1,
        }
    }

    /// C3 — stress: 10,000 documents totaling exactly 1 GiB, 10,000
    /// commits, an included 10 MiB single-paragraph document and
    /// repeated-line pathological cases (SPEC 17.2).
    ///
    /// The changed set holds 100 documents: the mass paragraph, the eight
    /// repeated-line documents, the selected document, and 91 ordinary
    /// documents. SPEC fixes the corpus totals, not the C3 change set, so
    /// the manifest records this composition as the definition.
    pub fn c3() -> Self {
        let mass = 10 * 1024 * KIB;
        let repeated_documents = 8;
        let repeated_bytes = 64 * KIB;
        let changed = 100;
        let selected_bytes = 128 * KIB;
        let total = 1024 * KIB as u64 * KIB as u64;
        let changed_total = mass as u64
            + (repeated_documents * repeated_bytes) as u64
            + (changed - repeated_documents) as u64 * selected_bytes as u64;
        Parameters {
            corpus: "C3",
            seed: default_seed(3),
            documents: 10_000,
            sizes: SizePlan::ExactTotal {
                total,
                max: total as usize,
                changed_total,
                selected_bytes,
            },
            commits: 10_000,
            changed_documents: changed,
            selected_changes: 10,
            review_cadence: 18,
            long_paragraph_pairs: 4,
            pathological: PathologicalPlan {
                mass_paragraph_bytes: mass,
                repeated_line_documents: repeated_documents,
                repeated_line_bytes: repeated_bytes,
            },
            packing: Packing::Packed,
            object_format: super::runner::ObjectFormat::Sha1,
        }
    }

    /// The C1-shaped base of the C4 Git-state variants (SPEC 17.2 calls
    /// them "independent C1/C2 variants").
    pub fn c4_base() -> Self {
        let mut base = Parameters::c1();
        base.corpus = "C4";
        base.seed = default_seed(4);
        base
    }

    /// The developer-suite scale of one corpus: small enough to generate
    /// in seconds while keeping every code path, exact total, hunk
    /// separation, and history shape of the full definition.
    pub fn test_scale(&self) -> Self {
        match self.corpus {
            "C2" => Parameters {
                corpus: "C2",
                seed: self.seed,
                documents: 40,
                sizes: SizePlan::ExactTotal {
                    total: 4 * 1024 * KIB as u64,
                    max: KIB * KIB,
                    changed_total: 640 * KIB as u64,
                    selected_bytes: 10 * KIB,
                },
                commits: 60,
                changed_documents: 8,
                selected_changes: 12,
                review_cadence: 8,
                long_paragraph_pairs: 2,
                pathological: PathologicalPlan::NONE,
                packing: self.packing,
                object_format: self.object_format,
            },
            "C3" => {
                let mass = 32 * KIB;
                let repeated_documents = 2;
                let repeated_bytes = 8 * KIB;
                let rest_bytes = 16 * KIB;
                let rest_documents = 12;
                let changed = 6;
                let changed_total = mass as u64
                    + (repeated_documents * repeated_bytes) as u64
                    + (changed - repeated_documents) as u64 * rest_bytes as u64;
                let total = changed_total + (rest_documents as u64) * (rest_bytes as u64);
                Parameters {
                    corpus: "C3",
                    seed: self.seed,
                    documents: 1
                        + repeated_documents
                        + (changed - 1 - repeated_documents)
                        + rest_documents,
                    sizes: SizePlan::ExactTotal {
                        total,
                        max: total as usize,
                        changed_total,
                        selected_bytes: rest_bytes,
                    },
                    commits: 36,
                    changed_documents: changed,
                    selected_changes: 8,
                    review_cadence: 8,
                    long_paragraph_pairs: 1,
                    pathological: PathologicalPlan {
                        mass_paragraph_bytes: mass,
                        repeated_line_documents: repeated_documents,
                        repeated_line_bytes: repeated_bytes,
                    },
                    packing: self.packing,
                    object_format: self.object_format,
                }
            }
            "C4" => Parameters {
                corpus: "C4",
                seed: self.seed,
                documents: 8,
                sizes: SizePlan::Uniform {
                    document_bytes: 6 * KIB,
                },
                commits: 24,
                changed_documents: 4,
                selected_changes: 6,
                review_cadence: 6,
                long_paragraph_pairs: 1,
                pathological: PathologicalPlan::NONE,
                packing: self.packing,
                object_format: self.object_format,
            },
            _ => Parameters {
                corpus: "C1",
                seed: self.seed,
                documents: 12,
                sizes: SizePlan::Uniform {
                    document_bytes: 16 * KIB,
                },
                commits: 44,
                changed_documents: 5,
                selected_changes: 10,
                review_cadence: 8,
                long_paragraph_pairs: 2,
                pathological: PathologicalPlan::NONE,
                packing: self.packing,
                object_format: self.object_format,
            },
        }
    }

    /// Overrides the seed.
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    /// Overrides the packing state.
    pub fn with_packing(mut self, packing: Packing) -> Self {
        self.packing = packing;
        self
    }

    /// Overrides the object format.
    pub fn with_object_format(mut self, format: super::runner::ObjectFormat) -> Self {
        self.object_format = format;
        self
    }

    /// Checks the invariants the generator relies on.
    pub fn validate(&self) {
        assert!(self.documents >= 6, "a corpus needs at least six documents");
        assert!(
            self.changed_documents >= 2,
            "a corpus needs at least two changed documents"
        );
        assert!(
            self.changed_documents <= self.documents,
            "more changed documents than documents"
        );
        assert!(
            self.selected_changes >= 2,
            "the selected document needs edits"
        );
        assert!(
            self.long_paragraph_pairs * 2
                + 1
                + if self.pathological.mass_paragraph_bytes > 0 {
                    1
                } else {
                    0
                }
                + self.pathological.repeated_line_documents
                <= self.documents,
            "no room for the planned special documents"
        );
        let layout = HistoryLayout::of(self);
        assert_eq!(
            layout.total(),
            self.commits,
            "the history layout must fill the commit budget exactly"
        );
        if let SizePlan::ExactTotal {
            total,
            changed_total,
            selected_bytes,
            ..
        } = &self.sizes
        {
            assert!(
                *changed_total < *total,
                "the changed documents cannot cover the whole corpus"
            );
            assert!(*selected_bytes as u64 <= *changed_total);
        }
    }
}

/// The commit composition of one corpus history.
///
/// The history is linear and first-parent only. It opens with the scoped
/// start marker and one external sentinel commit, continues with history
/// drafts broken by evenly spaced reviews, and closes with one draft per
/// changed document — so the distance from the candidate C to the last
/// review equals the changed-document count and the B → C comparison is
/// exactly the planned change set.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct HistoryLayout {
    /// Root start markers.
    pub start: usize,
    /// External, non-marker commits (sentinels, variant setup).
    pub external: usize,
    /// Draft markers before the last review.
    pub history_drafts: usize,
    /// Review markers, keep-all and selective.
    pub reviews: usize,
    /// Draft markers after the last review, one per changed document.
    pub post_review_drafts: usize,
}

impl HistoryLayout {
    /// Derives the layout from `parameters`.
    pub fn of(parameters: &Parameters) -> Self {
        let post = parameters.changed_documents;
        let budget = parameters.commits - post - 2;
        assert!(budget >= 2, "no room for history drafts and reviews");
        let reviews = (budget / (parameters.review_cadence + 1))
            .max(1)
            .min(budget - 1);
        let history_drafts = budget - reviews;
        HistoryLayout {
            start: 1,
            external: 1,
            history_drafts,
            reviews,
            post_review_drafts: post,
        }
    }

    /// Total commits of the layout.
    pub fn total(&self) -> usize {
        self.start + self.external + self.history_drafts + self.reviews + self.post_review_drafts
    }

    /// Drafts before the first review, so the last review always lands
    /// directly before the post-review drafts.
    pub fn leading_drafts(&self) -> usize {
        let gap = self.review_gap();
        self.history_drafts - gap * self.reviews
    }

    /// Drafts between consecutive reviews.
    pub fn review_gap(&self) -> usize {
        self.history_drafts / self.reviews
    }
}

/// The rendering shape of one planned document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Flavor {
    /// Ordinary prose, one paragraph per line.
    ProseLong,
    /// Ordinary prose, hard-wrapped at 72 columns.
    ProseWrapped,
    /// The long-paragraph member of an equivalent pair.
    LongParagraph,
    /// The hard-wrapped member of an equivalent pair.
    HardWrapped,
    /// One single line of the whole document size (C3).
    MassParagraph,
    /// One sentence repeated to the whole document size (C3).
    RepeatedLines,
}

impl Flavor {
    /// Whether the document renders paragraphs hard-wrapped.
    pub fn is_wrapped(self) -> bool {
        matches!(self, Flavor::ProseWrapped | Flavor::HardWrapped)
    }

    /// Whether the document is exactly one line.
    pub fn is_single_line(self) -> bool {
        matches!(self, Flavor::MassParagraph | Flavor::RepeatedLines)
    }
}

/// One planned document.
#[derive(Clone, Debug)]
pub struct DocumentPlanItem {
    /// Repository-relative path.
    pub path: String,
    /// Exact byte size at every revision.
    pub bytes: usize,
    /// Rendering shape.
    pub flavor: Flavor,
    /// Whether the document belongs to the B → C change set.
    pub changed: bool,
    /// Whether this is the selected benchmark document.
    pub selected: bool,
    /// Post-review edit count (changed documents only).
    pub edits: usize,
}

/// The planned document set with exact sizes.
pub struct DocumentPlan {
    /// Every document in index order.
    pub items: Vec<DocumentPlanItem>,
}

/// Even, deterministic size allocation: `total` split across `count`
/// documents, each at most `max`, with the remainder spread over the
/// first documents.
fn allocate_even(count: usize, total: u64, max: usize) -> Vec<usize> {
    assert!(count > 0, "cannot allocate across zero documents");
    assert!(
        total <= max as u64 * count as u64,
        "the total exceeds count * max; the allocation is impossible"
    );
    let base = (total / count as u64) as usize;
    let mut remainder = (total % count as u64) as usize;
    let mut sizes = vec![base; count];
    if remainder > 0 {
        let headroom = max - base;
        assert!(headroom > 0, "the remainder has no headroom under the cap");
        for size in &mut sizes {
            if remainder == 0 {
                break;
            }
            let add = headroom.min(remainder);
            *size += add;
            remainder -= add;
        }
    }
    assert_eq!(
        sizes.iter().map(|size| *size as u64).sum::<u64>(),
        total,
        "the allocation must fill the exact total"
    );
    sizes
}

/// The scope root of every corpus repository.
pub const SCOPE_ROOT: &str = "writing";

/// The suffix of the scope.
pub const SCOPE_SUFFIX: &str = ".md";

/// The repository-relative path of document `index`.
///
/// The selected document sits at the top of the scope; the others spread
/// across eight subdirectories so ordinary tree operations see realistic
/// depth.
fn document_path(index: usize) -> String {
    if index == 0 {
        return format!("{SCOPE_ROOT}/selected.md");
    }
    let directory = index % 8;
    format!("{SCOPE_ROOT}/d{directory:02}/doc-{index:05}.md")
}

/// Plans the document set: paths, exact sizes, flavors, and edit counts.
///
/// Index 0 is the selected document. The pathological documents follow,
/// then the long/wrapped equivalent pairs, then ordinary prose. The first
/// `changed_documents` indices form the B → C change set, so the
/// pathological cases are always part of the comparison.
pub fn plan_documents(parameters: &Parameters) -> DocumentPlan {
    let mut flavors = vec![Flavor::ProseWrapped; parameters.documents];
    let mut next = 1;
    if parameters.pathological.mass_paragraph_bytes > 0 {
        flavors[next] = Flavor::MassParagraph;
        next += 1;
    }
    for slot in flavors
        .iter_mut()
        .skip(next)
        .take(parameters.pathological.repeated_line_documents)
    {
        *slot = Flavor::RepeatedLines;
    }
    next += parameters.pathological.repeated_line_documents;
    for pair in 0..parameters.long_paragraph_pairs {
        let long = next + pair * 2;
        let wrapped = long + 1;
        assert!(
            wrapped < parameters.documents,
            "no room for the long/wrapped pairs"
        );
        flavors[long] = Flavor::LongParagraph;
        flavors[wrapped] = Flavor::HardWrapped;
    }
    // Every third remaining document stays unwrapped.
    for (index, flavor) in flavors.iter_mut().enumerate() {
        if *flavor == Flavor::ProseWrapped && index % 3 == 2 {
            *flavor = Flavor::ProseLong;
        }
    }

    let sizes: Vec<usize> = match &parameters.sizes {
        SizePlan::Uniform { document_bytes } => vec![*document_bytes; parameters.documents],
        SizePlan::ExactTotal {
            total,
            max,
            changed_total,
            selected_bytes,
        } => {
            let changed = parameters.changed_documents;
            let rest = parameters.documents - changed;
            let pathological = &parameters.pathological;
            let mut sizes = vec![0usize; parameters.documents];
            // Fixed sizes: the selected document, then the pathological
            // documents, all inside the changed set.
            sizes[0] = *selected_bytes;
            let mut fixed = 1;
            let mut changed_fixed = 1usize;
            if pathological.mass_paragraph_bytes > 0 {
                sizes[fixed] = pathological.mass_paragraph_bytes;
                fixed += 1;
                changed_fixed += 1;
            }
            assert!(
                pathological.repeated_line_documents + fixed <= changed,
                "the pathological documents must sit inside the changed set"
            );
            for _ in 0..pathological.repeated_line_documents {
                sizes[fixed] = pathological.repeated_line_bytes;
                fixed += 1;
                changed_fixed += 1;
            }
            let fixed_total: u64 = sizes[..fixed].iter().map(|size| *size as u64).sum();
            let changed_rest_total = *changed_total - fixed_total;
            let changed_rest = changed - changed_fixed;
            let rest_total = *total - *changed_total;
            let changed_sizes = allocate_even(changed_rest, changed_rest_total, *max);
            let rest_sizes = allocate_even(rest, rest_total, *max);
            let mut fill = 0;
            for size in changed_sizes {
                while sizes[fill] != 0 {
                    fill += 1;
                }
                sizes[fill] = size;
            }
            for size in rest_sizes {
                while sizes[fill] != 0 {
                    fill += 1;
                }
                sizes[fill] = size;
            }
            sizes
        }
    };

    let items = flavors
        .into_iter()
        .zip(sizes)
        .enumerate()
        .map(|(index, (flavor, bytes))| {
            let changed = index < parameters.changed_documents;
            let selected = index == 0;
            DocumentPlanItem {
                path: document_path(index),
                bytes,
                flavor,
                changed,
                selected,
                edits: if changed {
                    if selected {
                        parameters.selected_changes
                    } else if flavor == Flavor::MassParagraph {
                        // One physical line: one edit, one hunk.
                        1
                    } else {
                        2
                    }
                } else {
                    0
                },
            }
        })
        .collect();
    DocumentPlan { items }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_definitions_match_spec_17_2() {
        let c1 = Parameters::c1();
        assert_eq!(c1.documents, 100);
        assert_eq!(
            c1.sizes,
            SizePlan::Uniform {
                document_bytes: 20 * 1024
            }
        );
        assert_eq!(c1.commits, 200);
        assert_eq!(c1.changed_documents, 10);
        assert_eq!(c1.selected_changes, 10);

        let c2 = Parameters::c2();
        assert_eq!(c2.documents, 1000);
        assert_eq!(
            c2.sizes,
            SizePlan::ExactTotal {
                total: 100 * 1024 * 1024,
                max: 1024 * 1024,
                changed_total: 5 * 1024 * 1024,
                selected_bytes: 100 * 1024,
            }
        );
        assert_eq!(c2.commits, 1000);
        assert_eq!(c2.changed_documents, 50);
        assert_eq!(c2.selected_changes, 50);

        let c3 = Parameters::c3();
        assert_eq!(c3.documents, 10_000);
        assert_eq!(c3.commits, 10_000);
        assert_eq!(c3.pathological.mass_paragraph_bytes, 10 * 1024 * 1024);
        assert!(c3.pathological.repeated_line_documents >= 1);
        assert_eq!(
            match c3.sizes {
                SizePlan::ExactTotal { total, .. } => total,
                _ => unreachable!(),
            },
            1024 * 1024 * 1024
        );
    }

    #[test]
    fn layouts_fill_their_budgets() {
        for parameters in [
            Parameters::c1(),
            Parameters::c2(),
            Parameters::c3(),
            Parameters::c4_base(),
        ] {
            parameters.validate();
            let layout = HistoryLayout::of(&parameters);
            assert_eq!(layout.total(), parameters.commits);
            assert!(layout.reviews >= 1);
            assert!(layout.review_gap() >= 1);
            assert_eq!(layout.post_review_drafts, parameters.changed_documents);
        }
    }

    #[test]
    fn test_scales_keep_their_invariants() {
        for parameters in [
            Parameters::c1(),
            Parameters::c2(),
            Parameters::c3(),
            Parameters::c4_base(),
        ] {
            let scaled = parameters.test_scale();
            scaled.validate();
            assert_eq!(scaled.corpus, parameters.corpus);
            assert_eq!(scaled.seed, parameters.seed);
        }
    }

    #[test]
    fn exact_totals_allocate_exactly() {
        for parameters in [Parameters::c2(), Parameters::c3()] {
            let scaled = parameters.test_scale();
            let plan = plan_documents(&scaled);
            let (total, changed_total, selected_bytes) = match scaled.sizes {
                SizePlan::ExactTotal {
                    total,
                    changed_total,
                    selected_bytes,
                    ..
                } => (total, changed_total, selected_bytes),
                _ => unreachable!(),
            };
            assert_eq!(
                plan.items.iter().map(|item| item.bytes as u64).sum::<u64>(),
                total
            );
            assert_eq!(
                plan.items
                    .iter()
                    .filter(|item| item.changed)
                    .map(|item| item.bytes as u64)
                    .sum::<u64>(),
                changed_total
            );
            assert_eq!(plan.items[0].bytes, selected_bytes);
            assert!(plan.items[0].selected);
        }
    }
}
