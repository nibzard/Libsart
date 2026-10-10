//! Drift check between the canonical formal sources in `formal/` and the
//! copies embedded in SPEC.md Appendix A.
//!
//! SPEC 15.1 and A.1 make `formal/MarginReview.tla` and the five TLC
//! configurations the canonical extracted artifacts. SPEC 18.5 requires the
//! documentation embedding to be checked for drift. This test re-extracts
//! each Appendix A code block from SPEC.md and compares it byte for byte
//! with the canonical file, so editing either side without the other fails.

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

/// Each canonical file is anchored to its Appendix A heading and the info
/// string of the fenced block that carries its embedded copy.
const SOURCES: &[(&str, &str, &str)] = &[
    ("### A.2 MarginReview.tla", "tla", "formal/MarginReview.tla"),
    (
        "### A.3 MarginReview.cfg",
        "text",
        "formal/MarginReview.cfg",
    ),
    (
        "### A.4 MarginReviewLive.cfg",
        "text",
        "formal/MarginReviewLive.cfg",
    ),
    (
        "### A.5 MarginReviewNightly.cfg",
        "text",
        "formal/MarginReviewNightly.cfg",
    ),
    (
        "### A.6 MarginReviewCooperative.cfg",
        "text",
        "formal/MarginReviewCooperative.cfg",
    ),
    (
        "### A.7 MarginReviewUncooperative.cfg",
        "text",
        "formal/MarginReviewUncooperative.cfg",
    ),
];

/// Returns the first fenced code block after `heading`, or `None` when the
/// heading, the opening fence with the expected info string, or the closing
/// fence is missing.
fn embedded_block(spec: &str, heading: &str, info: &str) -> Option<String> {
    let mut lines = spec.lines().skip_while(|line| *line != heading);
    lines.next()?; // Drop the heading itself.
    let open = lines.find(|line| line.starts_with("```"))?;
    if open.strip_prefix("```") != Some(info) {
        return None;
    }
    let mut block = String::new();
    for line in lines {
        if line.starts_with("```") {
            return Some(block);
        }
        block.push_str(line);
        block.push('\n');
    }
    None // Unterminated block.
}

/// Describes the first difference between two texts with matching lines.
fn first_difference(embedded: &str, canonical: &str) -> String {
    let mut number = 0;
    let mut left = embedded.lines();
    let mut right = canonical.lines();
    loop {
        number += 1;
        match (left.next(), right.next()) {
            (Some(a), Some(b)) if a == b => continue,
            (Some(a), Some(b)) => {
                return format!("line {number}: SPEC.md has {a:?}, formal/ has {b:?}");
            }
            (Some(a), None) => return format!("line {number}: SPEC.md has extra line {a:?}"),
            (None, Some(b)) => return format!("line {number}: formal/ has extra line {b:?}"),
            (None, None) => {
                return format!(
                    "lines match; only trailing bytes differ ({} vs {} bytes)",
                    embedded.len(),
                    canonical.len()
                );
            }
        }
    }
}

#[test]
fn formal_sources_match_spec_appendix_a() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let spec_path = root.join("SPEC.md");
    let spec = fs::read_to_string(&spec_path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", spec_path.display()));

    let mut failures = Vec::new();
    for (heading, info, relative) in SOURCES {
        let path = root.join(relative);
        let canonical = fs::read_to_string(&path);
        match (embedded_block(&spec, heading, info), canonical) {
            (Some(embedded), Ok(canonical)) if embedded == canonical => {}
            (Some(embedded), Ok(canonical)) => {
                let mut message = String::new();
                let _ = write!(
                    message,
                    "{relative} differs from its Appendix A block: {}",
                    first_difference(&embedded, &canonical)
                );
                failures.push(message);
            }
            (None, _) => failures.push(format!(
                "{relative}: SPEC.md has no matching block after {heading}"
            )),
            (Some(_), Err(error)) => {
                failures.push(format!("cannot read {}: {error}", path.display()))
            }
        }
    }

    assert!(
        failures.is_empty(),
        "formal sources drifted from the specification:\n  {}",
        failures.join("\n  ")
    );
}
