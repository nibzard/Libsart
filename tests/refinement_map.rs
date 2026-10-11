//! Drift checks between the formal model, the refinement map, the
//! implementation, and the error documentation (SPEC 15.7, task T038).
//!
//! SPEC 15.7 requires protocol changes to update the model, the mapping,
//! the executable tests, and the error documentation in the same change.
//! This suite enforces that rule mechanically:
//!
//! - The action inventory is re-derived from the disjunctions of `Next` in
//!   `formal/MarginReview.tla`, so adding, removing, or renaming a model
//!   action changes what the map must cover — exactly.
//! - Every mapped real Git test (`tests/<file>.rs::<function>`) must exist.
//! - Every trace record the map calls implementation-emitted must appear in
//!   the source, and every documented record must be one the checker knows.
//! - Every public error code the map names must exist in the
//!   implementation, so the reply-code map never documents a code the
//!   binary cannot return.
//!
//! The checks compare artifacts, never behavior; the behavior lives in
//! `tests/refinement_trace.rs`.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

/// The composed definitions whose disjunctions name every model action.
const COMPOSED: &[&str] = &["Next", "NormalNext", "Environment", "Commands", "Advance"];

/// The model's initial state predicate, which the map also covers.
const INITIAL: &str = "Init";

/// Returns the body of one top-level definition: the remainder of its
/// `name ==` line plus the indented lines that follow, until the next
/// definition at column zero.
fn definition_body(tla: &str, name: &str) -> Option<String> {
    let mut lines = tla.lines().skip_while(|line| {
        !line.starts_with(name) || !line[name.len()..].trim_start().starts_with("==")
    });
    let first = lines.next()?;
    let mut body = String::from(first.split_once("==")?.1);
    body.push('\n');
    for line in lines {
        if !line.is_empty() && !line.starts_with(char::is_whitespace) {
            break;
        }
        body.push_str(line);
        body.push('\n');
    }
    Some(body)
}

/// Extracts the leading identifier of one disjunct, after the parentheses
/// of a quantified group and one `\E x \in S :` prefix are stripped. The
/// domains hold no colon, so the first colon ends the quantifier.
fn leading_identifier(disjunct: &str) -> Option<String> {
    let mut text = disjunct.trim();
    while let Some(rest) = text.strip_prefix('(') {
        text = rest.trim_start();
    }
    if let Some(rest) = text.strip_prefix("\\E") {
        let colon = rest.find(':')?;
        text = rest[colon + 1..].trim();
        while let Some(rest) = text.strip_prefix('(') {
            text = rest.trim_start();
        }
    }
    let mut identifier = String::new();
    for character in text.chars() {
        if character.is_ascii_alphanumeric() {
            identifier.push(character);
        } else {
            break;
        }
    }
    if identifier.is_empty() || !identifier.starts_with(|c: char| c.is_ascii_uppercase()) {
        return None;
    }
    Some(identifier)
}

/// Derives every model action from the disjunctions of `Next`: the
/// parenthesized invocations inside the composed bodies, plus the bare
/// disjuncts of `Next` and `Advance`.
fn model_actions(tla: &str) -> BTreeSet<String> {
    let mut actions = BTreeSet::new();
    for name in COMPOSED {
        let body = definition_body(tla, name).unwrap_or_else(|| panic!("no {name} definition"));
        for disjunct in body.split("\\/") {
            // Invocations such as `Submit(r, b, c, rs, done, mode)` carry
            // their argument list into the disjunct.
            if let Some(identifier) = leading_identifier(disjunct) {
                let composed = COMPOSED.contains(&identifier.as_str());
                let domain_word = matches!(
                    identifier.as_str(),
                    "BOOLEAN" | "Scope" | "Trees" | "Paths" | "Values" | "RequestIds"
                );
                if !composed && !domain_word {
                    actions.insert(identifier);
                }
            }
        }
    }
    actions
}

/// The rows of one markdown table section: the first backticked token of
/// each row, with the whole row for the caller's column checks.
fn table_rows<'a>(document: &'a str, heading: &str) -> Vec<(String, &'a str)> {
    let section = section_of(document, heading);
    let mut rows = Vec::new();
    for line in section.lines() {
        let trimmed = line.trim_end();
        if !trimmed.starts_with("| `") {
            continue;
        }
        let rest = &trimmed[3..];
        let Some(end) = rest.find('`') else { continue };
        rows.push((rest[..end].to_string(), trimmed));
    }
    rows
}

/// The text of one `##` section.
fn section_of<'a>(document: &'a str, heading: &str) -> &'a str {
    let start = document
        .find(&format!("\n## {heading}\n"))
        .unwrap_or_else(|| panic!("the map has no {heading} section"))
        + heading.len()
        + 5;
    let end = document[start..]
        .find("\n## ")
        .map(|offset| start + offset)
        .unwrap_or(document.len());
    &document[start..end]
}

/// Every `tests/<file>.rs::<function>` reference in the map.
fn mapped_tests(document: &str) -> Vec<(String, String)> {
    let mut references = Vec::new();
    let mut offset = 0;
    while let Some(found) = document[offset..].find("tests/") {
        let at = offset + found;
        offset = at + 1;
        let rest = &document[at + "tests/".len()..];
        let file: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        let function = rest.strip_prefix(&format!("{file}.rs::"));
        let Some(function) = function else { continue };
        let function: String = function
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !file.is_empty() && !function.is_empty() {
            references.push((file, function));
        }
    }
    references
}

/// Every backticked public code of the map: one ALL-CAPS token of at least
/// four letters, digits, or underscores.
fn public_codes(document: &str) -> BTreeSet<String> {
    let mut codes = BTreeSet::new();
    let mut remainder = document;
    while let Some(found) = remainder.find('`') {
        let after = &remainder[found + 1..];
        let Some(end) = after.find('`') else {
            break;
        };
        let token = &after[..end];
        if token.len() >= 4
            && token
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            && token.starts_with(|c: char| c.is_ascii_uppercase())
        {
            codes.insert(token.to_string());
        }
        remainder = &after[end + 1..];
    }
    codes
}

/// Reads one repository file, skipping the checks when the artifact tree
/// is absent so ordinary library builds stay unaffected.
fn repository_file(name: &str) -> Option<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(name);
    fs::read_to_string(path).ok()
}

#[test]
fn the_map_covers_every_model_action_exactly() {
    let Some(tla) = repository_file("formal/MarginReview.tla") else {
        return;
    };
    let Some(map) = repository_file("formal/refinement.md") else {
        panic!("formal/refinement.md is missing");
    };

    let mut expected = model_actions(&tla);
    expected.insert(INITIAL.to_string());

    let mapped: BTreeSet<String> = table_rows(&map, "The action map")
        .into_iter()
        .map(|(action, _)| {
            // `Checkpoint(FALSE)` and `Submit(r, ...)` map by their base
            // name; the row states the arguments the model carries.
            action
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
        })
        .collect();

    let missing: Vec<&String> = expected.difference(&mapped).collect();
    let invented: Vec<&String> = mapped.difference(&expected).collect();
    assert!(
        missing.is_empty() && invented.is_empty(),
        "the action map and formal/MarginReview.tla drifted: unmapped actions \
         {missing:?}, actions without one model action {invented:?}"
    );
}

#[test]
fn the_trace_contract_names_every_model_action_once() {
    let Some(tla) = repository_file("formal/MarginReview.tla") else {
        return;
    };
    let Some(map) = repository_file("formal/refinement.md") else {
        panic!("formal/refinement.md is missing");
    };

    let mut expected = model_actions(&tla);
    expected.insert(INITIAL.to_string());

    let rows = table_rows(&map, "Trace records");
    let documented: BTreeSet<String> = rows.iter().map(|(name, _)| name.clone()).collect();
    assert_eq!(
        documented, expected,
        "the trace contract must document exactly the model actions"
    );

    // Every record the map calls implementation-emitted exists in the
    // source; every record the map documents is one the checker knows.
    let Some(source) = repository_file("src/app.rs") else {
        return;
    };
    let Some(checker) = repository_file("tests/support/model.rs") else {
        panic!("tests/support/model.rs is missing");
    };
    for (name, row) in &rows {
        assert!(
            checker.contains(&format!("\"{name}\"")),
            "the checker does not know the record {name}"
        );
        if row.contains("implementation") {
            assert!(
                source.contains(&format!("\"{name}\"")),
                "src/app.rs never emits the record {name} the map promises"
            );
        }
    }
}

#[test]
fn every_mapped_real_git_test_exists() {
    let Some(map) = repository_file("formal/refinement.md") else {
        panic!("formal/refinement.md is missing");
    };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    let mut failures = Vec::new();
    for (file, function) in mapped_tests(&map) {
        let path = root.join("tests").join(format!("{file}.rs"));
        let body = fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!("cannot read the mapped test file tests/{file}.rs: {error}")
        });
        let declared = body.contains(&format!("fn {function}("));
        if !declared {
            failures.push(format!("tests/{file} holds no fn {function}"));
        }
    }
    assert!(
        failures.is_empty(),
        "the refinement map names tests that do not exist:\n  {}",
        failures.join("\n  ")
    );
}

#[test]
fn every_documented_public_code_exists_in_the_implementation() {
    let Some(map) = repository_file("formal/refinement.md") else {
        panic!("formal/refinement.md is missing");
    };
    let Some(app) = repository_file("src/app.rs") else {
        return;
    };
    let Some(cli) = repository_file("src/cli.rs") else {
        return;
    };

    let failures: Vec<String> = public_codes(&map)
        .into_iter()
        .filter(|code| {
            !app.contains(&format!("\"{code}\"")) && !cli.contains(&format!("\"{code}\""))
        })
        .map(|code| format!("the map documents {code}, which no operation returns"))
        .collect();
    assert!(
        failures.is_empty(),
        "the reply-code map drifted from the implementation:\n  {}",
        failures.join("\n  ")
    );
}
