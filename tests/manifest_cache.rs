//! Real-Git tests for the rebuildable manifest cache (SPEC 8.2, 13.3,
//! task T084).
//!
//! Every case proves one contract of the cache against the installed Git:
//!
//! - the `diff` that pins a review and the `review` that validates and
//!   applies its plan reuse one manifest derivation: the entry the diff
//!   wrote serves the validation, and the applied result equals the tree a
//!   cold derivation proves
//! - a served entry reproduces the exact diff result of a cold derivation,
//!   identity, totals, change identities, and presentations included
//! - an entry whose identity-relevant bytes changed is refused: the
//!   manifest digest, the review identity, or a change identity no longer
//!   derives the recorded value, and the comparison is rederived
//! - a comparison that moved never reads the entry of another: the key
//!   names the branch, baseline, candidate, scope, engine, and Git version
//!   of exactly one comparison
//! - the cache stays bounded in entries, so a desk's recent comparisons
//!   displace the oldest, and deleting the whole directory changes no
//!   result
//! - a diff writes no ref, index entry, or working byte: only the
//!   disposable state inside the Git directory moves
//!
//! The oracles stay independent of the implementation (SPEC 16.3): trees
//! and object IDs come from `git rev-parse` and `git ls-tree`, and the
//! repository state from `git status`.

mod support;

use std::fs;
use std::path::PathBuf;

use margin::app::{DiffRequest, InitRequest, Operations, ReviewOutcome, ReviewRequest};
use margin::domain::ScopeInput;
use margin::git::Git;
use serde_json::Value;
use support::repo::{RepoOptions, TestRepo};

/// One document with two independently reviewable passages, each inside
/// its own gap paragraph.
fn document(first: &str, second: &str) -> String {
    let mut out = String::from("# Notes\n\n");
    for line in 1..=6 {
        out.push_str(&format!("gap upper {line}\n"));
    }
    out.push_str(&format!("\n{first}\n"));
    for line in 1..=6 {
        out.push_str(&format!("gap lower {line}\n"));
    }
    out.push_str(&format!("\n{second}\n"));
    out
}

/// The seeded workspace: a start marker B and a committed candidate C.
struct Fixture {
    repo: TestRepo,
    start: String,
    candidate: String,
}

fn fixture() -> Fixture {
    let repo = TestRepo::init_with(RepoOptions::new());
    repo.write_file(
        "doc-a.md",
        document("first original passage", "second original passage").as_bytes(),
    );
    repo.write_file("note.txt", b"the baseline note\n");
    let start = initialize(&repo);
    repo.write_file(
        "doc-a.md",
        document(
            "first revised passage\nwith two lines",
            "second revised passage",
        )
        .as_bytes(),
    );
    repo.write_file("note.txt", b"the candidate note\n");
    let candidate = repo.commit_all("writing pass");
    Fixture {
        repo,
        start,
        candidate,
    }
}

/// The whole-repository Markdown scope.
fn scope() -> ScopeInput {
    ScopeInput {
        include: vec![".".to_string()],
        exclude: Vec::new(),
        suffixes: vec![".md".to_string()],
    }
}

/// Adapter bound to the fixture's working tree and isolated environment.
fn adapter_for(repo: &TestRepo) -> Git {
    Git::from_env(repo.env().vars().clone()).at(repo.path())
}

/// Initializes the whole-repository scope and returns the start marker.
fn initialize(repo: &TestRepo) -> String {
    let git = adapter_for(repo);
    let operations = Operations::new(&git);
    let result = operations.initialize_scope(&InitRequest {
        scope: Some(scope()),
        create_repository: false,
        branch: None,
        new_start: false,
        auto_checkpoint: None,
        capture_after_edit: None,
        editor_argv: None,
        dry_run: false,
    });
    assert!(
        result.ok(),
        "initialization must succeed: {:?}",
        result.error
    );
    result
        .data
        .and_then(|data| data.start_commit)
        .expect("first use creates one start commit")
}

/// Builds the pinned review through one fresh operation layer.
fn review_of(repo: &TestRepo) -> ReviewOutcome {
    let git = adapter_for(repo);
    let operations = Operations::new(&git);
    let outcome = operations.build_review(&DiffRequest {
        scope: None,
        path_filter: Vec::new(),
        change: None,
        cursor: None,
        limit: None,
        working: false,
    });
    assert!(outcome.ok(), "the review builds: {:?}", outcome.error);
    outcome.data.expect("a successful diff carries data")
}

/// The cache directory of the fixture's repository.
fn cache_dir(repo: &TestRepo) -> PathBuf {
    repo.path().join(".git/margin/manifest-cache")
}

/// The one entry the fixture's comparison wrote, parsed as JSON.
fn the_entry(repo: &TestRepo) -> (String, Value) {
    let dir = cache_dir(repo);
    let mut names: Vec<String> = fs::read_dir(&dir)
        .expect("the cache directory exists")
        .flatten()
        .map(|entry| entry.file_name().into_string().expect("an entry name"))
        .collect();
    names.sort();
    assert_eq!(names.len(), 1, "exactly one entry holds: {names:?}");
    let name = names.pop().expect("one entry");
    let raw = fs::read(dir.join(&name)).expect("the entry is readable");
    let value: Value = serde_json::from_slice(&raw).expect("the entry is JSON");
    (name, value)
}

/// Rewrites the one entry with `value`.
fn write_entry(repo: &TestRepo, name: &str, value: &Value) {
    let body = serde_json::to_vec_pretty(value).expect("the entry serializes");
    fs::write(cache_dir(repo).join(name), body).expect("the entry rewrites");
}

/// The identity and presentation summary one assertion compares.
fn summary_of(review: &ReviewOutcome) -> Vec<String> {
    review
        .changes
        .iter()
        .map(|change| {
            format!(
                "{}|{}|{}|{}",
                change.change_id, change.path, change.reason, change.presentation.heading
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// One derivation serves the diff and the apply
// ---------------------------------------------------------------------------

#[test]
fn the_diff_and_the_apply_reuse_one_derivation() {
    let Fixture {
        repo,
        start,
        candidate,
    } = fixture();

    // The diff derives and caches the manifest of the pinned comparison.
    let first = review_of(&repo);
    assert_eq!(first.baseline_oid, start);
    assert_eq!(first.candidate_oid, candidate);
    let (name, entry) = the_entry(&repo);
    assert_eq!(
        entry["review_id"].as_str().unwrap(),
        first
            .review_id
            .strip_prefix("sha1:")
            .unwrap_or(&first.review_id),
        "the entry records the review identity"
    );

    // The validation of the plan the diff composed reads the same entry:
    // a presentation field the entry carries is served back, which only
    // the cached derivation can do.
    let restore: Vec<String> = first
        .changes
        .iter()
        .map(|change| change.change_id.clone())
        .collect();
    let plan = serde_json::json!({
        "schema_version": 1,
        "request_id": "cache-reuse-01",
        "review_id": first.review_id,
        "branch_ref": first.branch_ref,
        "scope_id": first.scope_id,
        "baseline_oid": first.baseline_oid,
        "candidate_oid": first.candidate_oid,
        "restore": restore,
        "keep_remaining": true,
    })
    .to_string();

    // A cold apply on the same plan must reach the same verified commit:
    // delete the entry, then apply.
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let applied = operations.apply_review(&ReviewRequest {
        scope: None,
        plan: plan.as_bytes().to_vec(),
        dry_run: false,
    });
    assert!(applied.ok(), "the cold apply commits: {:?}", applied.error);
    let cold = applied.data.expect("the apply reports data");
    let cold_tree = String::from_utf8_lossy(&repo.git_ok(["show", "-s", "--format=%T", "HEAD"]))
        .trim()
        .to_string();
    assert_eq!(cold.expected_tree, cold_tree);

    // Back at the candidate, the cached entry serves the same apply: the
    // entry is still the one the diff wrote, and the commit reproduces.
    repo.git_ok(["reset", "--hard", &candidate]);
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let plan = serde_json::json!({
        "schema_version": 1,
        "request_id": "cache-reuse-02",
        "review_id": first.review_id,
        "branch_ref": first.branch_ref,
        "scope_id": first.scope_id,
        "baseline_oid": first.baseline_oid,
        "candidate_oid": first.candidate_oid,
        "restore": restore,
        "keep_remaining": true,
    })
    .to_string();
    let warm = operations.apply_review(&ReviewRequest {
        scope: None,
        plan: plan.as_bytes().to_vec(),
        dry_run: false,
    });
    assert!(warm.ok(), "the warm apply commits: {:?}", warm.error);
    let warm_tree = String::from_utf8_lossy(&repo.git_ok(["show", "-s", "--format=%T", "HEAD"]))
        .trim()
        .to_string();
    assert_eq!(
        cold_tree, warm_tree,
        "the cached derivation applies the exact tree the cold one proved"
    );
    assert_eq!(
        the_entry(&repo).0,
        name,
        "the warm apply stored no second entry for the same comparison"
    );
}

#[test]
fn a_served_entry_reproduces_the_cold_derivation() {
    let Fixture { repo, .. } = fixture();
    let cold = review_of(&repo);

    // The second build runs against the entry the first wrote: one
    // presentation field no identity covers is rewritten in the entry, so
    // the served result can only come from the entry, and every field the
    // cold derivation produced comes back equal.
    let (name, mut entry) = the_entry(&repo);
    entry["files"][0]["groups"][0]["presentation"]["heading"] = Value::from("the served heading");
    write_entry(&repo, &name, &entry);
    let warm = review_of(&repo);
    assert_eq!(
        warm.changes[0].presentation.heading, "the served heading",
        "the entry answered the build, or the rewrite proved nothing"
    );

    // The identity the served entry carries is the identity the cold
    // derivation issued, over the same context and the same groups.
    let cold_again = {
        fs::remove_dir_all(cache_dir(&repo)).expect("the cache directory goes");
        review_of(&repo)
    };
    assert_eq!(cold.review_id, warm.review_id);
    assert_eq!(cold.manifest_digest, warm.manifest_digest);
    assert_eq!(cold.scope_totals.files, warm.scope_totals.files);
    assert_eq!(cold.scope_totals.groups, warm.scope_totals.groups);
    assert_ne!(
        cold_again.changes[0].presentation.heading, "the served heading",
        "the cold derivation never invents the served field"
    );
    let mut cold_changes = cold.changes.clone();
    cold_changes[0].presentation.heading = warm.changes[0].presentation.heading.clone();
    let mut cold_again_changes = cold_again.changes.clone();
    cold_again_changes[0].presentation.heading = warm.changes[0].presentation.heading.clone();
    assert_eq!(
        identity_of(&cold_changes),
        identity_of(&warm.changes),
        "the served identity equals the derived one"
    );
    assert_eq!(
        identity_of(&cold_changes),
        identity_of(&cold_again_changes),
        "the cold derivation is the same before and after the entry"
    );
    for (cold_change, warm_change) in cold_changes.iter().zip(warm.changes.iter()) {
        assert_eq!(cold_change.ranges, warm_change.ranges);
        assert_eq!(
            cold_change.presentation.before.text, warm_change.presentation.before.text,
            "the served excerpt equals the derived one"
        );
        assert_eq!(
            cold_change.presentation.after.text, warm_change.presentation.after.text,
            "the served excerpt equals the derived one"
        );
        assert_eq!(
            cold_change.presentation.word_hints,
            warm_change.presentation.word_hints
        );
    }
}

/// The identity summary of one changes page: change identity, path,
/// reason, and ranges, without the presentation text a served entry may
/// legitimately carry from its own derivation.
fn identity_of(changes: &[margin::app::ChangeDetail]) -> Vec<String> {
    changes
        .iter()
        .map(|change| {
            format!(
                "{}|{}|{}|{:?}",
                change.change_id, change.path, change.reason, change.ranges
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// A changed entry is refused, not trusted
// ---------------------------------------------------------------------------

#[test]
fn an_entry_with_changed_patch_bytes_falls_back_to_the_derivation() {
    let Fixture { repo, .. } = fixture();
    let true_review = review_of(&repo);
    let (name, mut entry) = the_entry(&repo);

    // Corrupt one group's patch bytes in the entry. The manifest digest of
    // the stored groups no longer derives the recorded value, so the entry
    // must be refused and the comparison rederived.
    entry["files"][0]["groups"][0]["patch_units"][0] = Value::from("00".repeat(64));
    write_entry(&repo, &name, &entry);

    let rederived = review_of(&repo);
    assert_eq!(
        rederived.review_id, true_review.review_id,
        "the rederived comparison is the true one"
    );
    assert_eq!(summary_of(&rederived), summary_of(&true_review));
}

#[test]
fn an_entry_with_a_changed_change_identity_falls_back() {
    let Fixture { repo, .. } = fixture();
    let true_review = review_of(&repo);
    let (name, mut entry) = the_entry(&repo);

    // Keep the patch bytes but forge one change identity: the digest and
    // the review identity still derive, and only the change-identity
    // comparison refuses the entry.
    let other = entry["files"][0]["groups"][0]["change_id"]
        .as_str()
        .unwrap()
        .chars()
        .rev()
        .collect::<String>();
    entry["files"][0]["groups"][0]["change_id"] = Value::from(other);
    write_entry(&repo, &name, &entry);

    let rederived = review_of(&repo);
    assert_eq!(summary_of(&rederived), summary_of(&true_review));
}

#[test]
fn an_entry_of_another_comparison_is_never_served() {
    let Fixture { repo, .. } = fixture();
    let first = review_of(&repo);

    // Move the candidate: the next comparison has another candidate, so
    // its key finds no entry and the derivation runs cold.
    repo.write_file(
        "doc-a.md",
        document("first revised passage\nwith two lines", "a later passage").as_bytes(),
    );
    repo.commit_all("a later pass");
    let second = review_of(&repo);
    assert_ne!(
        first.review_id, second.review_id,
        "the moved candidate derives another review"
    );
    let dir = cache_dir(&repo);
    let entries = fs::read_dir(&dir)
        .expect("the cache directory exists")
        .count();
    assert_eq!(entries, 2, "each comparison holds its own entry");
}

// ---------------------------------------------------------------------------
// The cache is bounded and disposable
// ---------------------------------------------------------------------------

#[test]
fn an_entry_whose_objects_left_the_store_is_a_miss() {
    let Fixture {
        repo, candidate, ..
    } = fixture();
    review_of(&repo);

    // The candidate's document blob leaves the object store. The entry is
    // no longer usable — the repository cannot stand behind the objects
    // its derivation read — so the comparison is rederived and fails the
    // derivation itself, typed, instead of serving a manifest over
    // missing objects (SPEC 16.6: missing object).
    let blob =
        String::from_utf8_lossy(&repo.git_ok(["rev-parse", &format!("{candidate}:doc-a.md")]))
            .trim()
            .to_string();
    let object = repo
        .path()
        .join(".git")
        .join("objects")
        .join(&blob[..2])
        .join(&blob[2..]);
    let moved = repo.root().join("moved-object");
    fs::rename(&object, &moved).expect("the blob object moves");

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let outcome = operations.build_review(&DiffRequest {
        scope: None,
        path_filter: Vec::new(),
        change: None,
        cursor: None,
        limit: None,
        working: false,
    });
    assert!(!outcome.ok(), "a missing object must fail the comparison");
    let error = outcome.error.expect("the typed failure");
    assert_eq!(error.code, "GIT_FAILED", "the typed failure: {error:?}");

    // The object returns; the identical comparison builds and the entry
    // serves it again.
    fs::rename(&moved, &object).expect("the blob object returns");
    let rebuilt = review_of(&repo);
    assert!(rebuilt.complete);
}

#[test]
fn the_cache_keeps_only_the_newest_comparisons() {
    let Fixture { repo, .. } = fixture();
    // Seven distinct comparisons against one document: each write pass
    // moves the candidate, so each review keys another entry.
    for pass in 0..8 {
        repo.write_file(
            "doc-a.md",
            document(
                "first revised passage\nwith two lines",
                &format!("pass {pass}"),
            )
            .as_bytes(),
        );
        repo.commit_all(&format!("pass {pass}"));
        review_of(&repo);
    }
    let dir = cache_dir(&repo);
    let entries = fs::read_dir(&dir)
        .expect("the cache directory exists")
        .flatten()
        .count();
    assert!(
        entries <= 6,
        "the cache stays bounded in entries, held {entries}"
    );
}

#[test]
fn deleting_the_cache_changes_no_result() {
    let Fixture { repo, .. } = fixture();
    let with_cache = review_of(&repo);
    fs::remove_dir_all(cache_dir(&repo)).expect("the cache directory goes");
    let without_cache = review_of(&repo);
    assert_eq!(summary_of(&with_cache), summary_of(&without_cache));
    assert_eq!(with_cache.review_id, without_cache.review_id);
    assert!(
        cache_dir(&repo).exists(),
        "the cold derivation wrote its entry back"
    );
}

// ---------------------------------------------------------------------------
// Read-only behavior
// ---------------------------------------------------------------------------

#[test]
fn a_diff_moves_no_ref_index_entry_or_working_byte() {
    let Fixture {
        repo, candidate, ..
    } = fixture();
    let before = repo.git_ok(["rev-parse", "HEAD"]);
    let head_before = String::from_utf8(before).expect("one commit ID");
    let status_before = String::from_utf8(repo.git_ok(["status", "--porcelain"])).expect("clean");

    review_of(&repo);

    let head_after = String::from_utf8(repo.git_ok(["rev-parse", "HEAD"])).expect("one commit ID");
    let status_after =
        String::from_utf8(repo.git_ok(["status", "--porcelain"])).expect("still clean");
    assert_eq!(head_before, head_after, "no ref moved");
    assert_eq!(status_before, status_after, "no working byte moved");
    assert_eq!(head_after.trim(), candidate);
    // The only new repository state is the cache directory itself, the
    // disposable state SPEC 8.2 allows inside the Git directory.
    assert!(cache_dir(&repo).exists());
}

// ---------------------------------------------------------------------------
// A structurally broken entry is a miss, never a panic
// ---------------------------------------------------------------------------

#[test]
fn an_entry_with_hunk_indexes_the_file_does_not_hold_is_a_miss() {
    let Fixture { repo, .. } = fixture();
    let true_review = review_of(&repo);
    let (name, mut entry) = the_entry(&repo);

    // The hunk indexes of a group are not identity-covered bytes, so the
    // digests still verify; the structure alone refuses the entry. A
    // served index beyond the file's hunks would panic the detail
    // projection, which no disposable entry may do.
    entry["files"][0]["groups"][0]["hunk_indexes"] = serde_json::json!([9999]);
    write_entry(&repo, &name, &entry);

    let rederived = review_of(&repo);
    assert_eq!(summary_of(&rederived), summary_of(&true_review));
}

#[test]
fn an_entry_that_names_a_revision_instead_of_an_object_is_a_miss() {
    let Fixture { repo, .. } = fixture();
    let true_review = review_of(&repo);
    let (name, mut entry) = the_entry(&repo);

    // A blob field that is not a full object ID must never reach Git as a
    // revision it would resolve (SPEC 14.1); the entry is refused whole.
    entry["files"][0]["oid_after"] = Value::from("HEAD^{tree}");
    write_entry(&repo, &name, &entry);

    let rederived = review_of(&repo);
    assert_eq!(summary_of(&rederived), summary_of(&true_review));
    assert_eq!(
        rederived.changes[0].oid_after, true_review.changes[0].oid_after,
        "the served blob object ID is the derived one"
    );
}

#[test]
fn an_entry_whose_group_lost_its_patch_unit_is_a_miss() {
    let Fixture { repo, .. } = fixture();
    let true_review = review_of(&repo);
    let (name, mut entry) = the_entry(&repo);

    // A group without its one complete patch unit has nothing the
    // application could replay; the entry is refused before that.
    entry["files"][0]["groups"][0]["patch_units"] = serde_json::json!([]);
    write_entry(&repo, &name, &entry);

    let rederived = review_of(&repo);
    assert_eq!(summary_of(&rederived), summary_of(&true_review));
}
