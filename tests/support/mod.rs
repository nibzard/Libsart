//! Shared harness for integration tests that run real Git.
//!
//! SPEC 16.3 requires one fresh temporary repository per integration case,
//! a test-only home and Git configuration, a deterministic fixture identity,
//! and independent oracles. This module supplies those pieces:
//!
//! - [`repo`] creates isolated repositories and runs the installed Git.
//! - [`env`] scrubs the child environment and records tool versions.
//! - [`oracle`] compares refs, index, trees, raw bytes, and file modes.
//!
//! The harness calls only stock Git and the standard library. Tests use it
//! so that a Margin result is checked by Git itself, never by a second
//! Margin command that shares the same parser (SPEC 16.3).

// Later slices use more of this harness than the self-verification suite.
#![allow(dead_code)]

pub mod env;
pub mod model;
pub mod mutation;
pub mod oracle;
pub mod repo;

/// The pseudo-terminal harness of the terminal-lifecycle tests (SPEC
/// 11.5): Unix only, because a pseudo-terminal is a Unix device.
#[cfg(unix)]
pub mod pty;

/// The C0 Markdown correctness corpus (SPEC 16.7). The corpus test and the
/// later restoration slice (T025) share these fixtures, so they live beside
/// the harness instead of inside one test binary.
#[path = "../fixtures/c0/mod.rs"]
pub mod c0;

// Not every test binary reads the environment record; the ones that do
// keep this re-export alive, so silence the per-binary unused warning.
#[allow(unused_imports)]
pub use env::environment_report;

/// Decodes Git stdout as UTF-8 text without the trailing newline.
pub(crate) fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}
