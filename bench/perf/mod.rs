//! The process benchmark harness of SPEC 17.3-17.8.
//!
//! This crate-extension times the real executable against the seeded
//! corpora of SPEC 17.2 under the measurement boundaries of 17.3,
//! registers the runner environment of 17.6, samples and compares under
//! the policy of 17.7, and writes the result records of 17.8. It is a
//! development example, never a shipped executable; the runnable entry
//! point is the Cargo example `benchrun` (see `bench/README.md`).
//!
//! The modules share no code with the product: the oracles observe the
//! fixture through stock Git and raw bytes, so a Margin result is never
//! checked by Margin itself.

// The test suite and the example include this module from another
// crate; not every public item is used by the example binary.
#![allow(dead_code)]

pub mod envrec;
pub mod fixture;
pub mod hash;
pub mod metric;
pub mod oracle;
pub mod pty;
pub mod record;
pub mod resource;
pub mod runner;
pub mod samples;
pub mod stats;
pub mod timing;

/// The revision of this harness, recorded in every environment and
/// result record. Bump when measurement, reset, oracle, or statistics
/// behavior changes.
pub const HARNESS_REVISION: &str = "margin-benchrun/3";
