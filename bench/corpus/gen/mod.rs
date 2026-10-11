//! Seeded generators for the reproducible benchmark corpora of SPEC 17.2.
//!
//! Four corpora are defined:
//!
//! - **C1 — daily desk**: 100 documents of 20 KiB, 200 commits, 10 changed
//!   documents, a selected 20 KiB document with 10 changes.
//! - **C2 — release envelope**: 1,000 documents totaling exactly 100 MiB,
//!   1,000 commits, 50 changed documents totaling 5 MiB, a selected
//!   100 KiB document with 50 changes, maximum file 1 MiB.
//! - **C3 — stress**: 10,000 documents totaling 1 GiB, 10,000 commits, an
//!   included 10 MiB single-paragraph document and repeated-line cases.
//! - **C4 — Git-state variants**: independent C1-shaped repositories with
//!   staging, untracked/ignored files, add/delete, hooks, conflicts,
//!   storage-policy, object-format, and packing cases.
//!
//! Every corpus is a pure function of its seed: the same parameters
//! produce byte-identical documents, edits, commit messages, and object
//! identifiers, recorded in `manifest.json` beside the repository. The
//! `tests/bench_corpora.rs` suite reruns generation and proves it.
//!
//! The runnable entry point is the Cargo example `corpusgen`; see
//! `bench/README.md` for the full protocol.

// The test suite includes this module from another crate; not every
// public item is used by the example binary.
#![allow(dead_code)]
#![allow(unused_imports)]

pub mod build;
pub mod edits;
pub mod manifest;
pub mod plan;
pub mod rng;
pub mod runner;
pub mod text;

pub use build::{CorpusResult, Variant, generate, generate_variant};
pub use manifest::{GENERATOR_REVISION, Manifest};
pub use plan::{HistoryLayout, Packing, Parameters};
pub use runner::ObjectFormat;

use std::path::Path;

/// Generates every C4 Git-state variant under `root`, one subdirectory
/// per variant, each with its own repository and manifest.
pub fn generate_all_variants(base: &Parameters, root: &Path) -> Vec<CorpusResult> {
    Variant::ALL
        .iter()
        .map(|variant| generate_variant(*variant, base, &root.join(variant.name())))
        .collect()
}
