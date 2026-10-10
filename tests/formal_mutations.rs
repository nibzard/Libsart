//! Drift check between the canonical formal model and its mutation patches.
//!
//! SPEC 15.6 and Appendix A.9 require one documented model mutation per
//! deliberate defect, and `formal/run-mutations.sh` applies those patches to
//! `formal/MarginReview.tla` before each TLC run. A patch that no longer
//! applies would turn its mutation case into a broken test, so this suite
//! applies every patch listed in `formal/mutations/cases.tsv` to a copy of
//! the canonical model and fails when either side changed alone.
//!
//! The suite never runs TLC: the mutation gate is `formal/run-mutations.sh`,
//! whose record lives in `formal/evidence/mutations/`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

/// One row of `formal/mutations/cases.tsv`.
struct Case {
    id: String,
    patches: Vec<String>,
    config: String,
    expect: String,
    trace_action: String,
}

/// Parses the manifest, skipping its comment lines. Returns one error
/// message per malformed row.
fn read_cases(path: &Path) -> Result<Vec<Case>, Vec<String>> {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    let mut cases = Vec::new();
    let mut errors = Vec::new();
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != 5 {
            errors.push(format!(
                "line {}: expected 5 tab-separated fields",
                number + 1
            ));
            continue;
        }
        cases.push(Case {
            id: fields[0].to_string(),
            patches: if fields[1] == "-" {
                Vec::new()
            } else {
                fields[1].split(' ').map(str::to_string).collect()
            },
            config: fields[2].to_string(),
            expect: fields[3].to_string(),
            trace_action: fields[4].to_string(),
        });
    }
    if cases.is_empty() {
        errors.push("no cases listed".into());
    }
    if errors.is_empty() {
        Ok(cases)
    } else {
        Err(errors)
    }
}

/// Applies `patches` in order to `MarginReview.tla` inside `dir` with the
/// same `patch` invocation the mutation gate uses.
fn apply_patches(dir: &Path, patches: &[String], root: &Path) -> Result<(), String> {
    for name in patches {
        let patch_path = root.join("formal/mutations").join(name);
        let content = fs::read_to_string(&patch_path)
            .map_err(|error| format!("cannot read {}: {error}", patch_path.display()))?;
        for header in ["--- a/MarginReview.tla", "+++ b/MarginReview.tla"] {
            if !content.contains(header) {
                return Err(format!("{name} must only touch MarginReview.tla"));
            }
        }
        let file = fs::File::open(&patch_path)
            .map_err(|error| format!("cannot open {}: {error}", patch_path.display()))?;
        let status = Command::new("patch")
            .arg("-p1")
            .arg("--batch")
            .arg("--fuzz=0")
            .arg("--silent")
            .current_dir(dir)
            .stdin(file)
            .status()
            .map_err(|error| format!("cannot run patch: {error}"))?;
        if !status.success() {
            return Err(format!(
                "{name} does not apply to the current MarginReview.tla"
            ));
        }
    }
    Ok(())
}

#[test]
fn mutation_patches_still_apply_to_the_canonical_model() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let manifest = root.join("formal/mutations/cases.tsv");
    let model = fs::read_to_string(root.join("formal/MarginReview.tla"))
        .expect("canonical formal/MarginReview.tla is readable");
    let cases =
        read_cases(&manifest).unwrap_or_else(|errors| panic!("malformed cases.tsv: {errors:?}"));

    let mut failures = Vec::new();
    for case in &cases {
        if case.patches.is_empty() {
            continue;
        }
        let dir = tempdir().expect("temporary directory");
        let target = dir.path().join("MarginReview.tla");
        fs::write(&target, &model).expect("copy of the canonical model");

        if let Err(message) = apply_patches(dir.path(), &case.patches, &root) {
            failures.push(format!("{}: {message}", case.id));
            continue;
        }
        let mutant = fs::read_to_string(&target).expect("mutated model");
        if mutant == model {
            failures.push(format!("{}: patches changed nothing", case.id));
        }
    }

    assert!(
        failures.is_empty(),
        "mutation patches drifted from formal/MarginReview.tla:\n  {}",
        failures.join("\n  ")
    );
}

#[test]
fn mutation_cases_reference_real_configs_and_detectors() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let manifest = root.join("formal/mutations/cases.tsv");
    let cases =
        read_cases(&manifest).unwrap_or_else(|errors| panic!("malformed cases.tsv: {errors:?}"));

    let mut failures = Vec::new();
    for case in &cases {
        let config_path = root.join("formal").join(format!("{}.cfg", case.config));
        let config = fs::read_to_string(&config_path)
            .unwrap_or_else(|_| panic!("missing config {}", config_path.display()));

        if let Some(property) = case.expect.strip_prefix("violated:") {
            // The host configuration must actually check the designated
            // detector; otherwise TLC could never report it. It appears as
            // its own line under INVARIANTS or behind the PROPERTY keyword.
            if !config
                .lines()
                .any(|line| line.split_whitespace().any(|word| word == property))
            {
                failures.push(format!(
                    "{}: {}.cfg does not check {}",
                    case.id, case.config, property
                ));
            }
        } else if case.expect != "pass" {
            failures.push(format!(
                "{}: expected must be pass or violated:<property>, got {}",
                case.id, case.expect
            ));
        }

        if case.expect == "pass" && case.trace_action != "-" {
            failures.push(format!(
                "{}: a passing case cannot expect a counterexample action",
                case.id
            ));
        }
        if case.expect != "pass" && case.trace_action == "-" {
            failures.push(format!(
                "{}: a violated case must name the action in its counterexample",
                case.id
            ));
        }
        if case.id.contains(char::is_whitespace) {
            failures.push(format!("{}: case ids cannot contain whitespace", case.id));
        }
    }

    assert!(
        failures.is_empty(),
        "mutation cases reference incomplete expectations:\n  {}",
        failures.join("\n  ")
    );
}
