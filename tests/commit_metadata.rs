//! Real-Git tests for commit metadata (SPEC 8.3-8.4, task T012).
//!
//! Every case proves one marker rule against the installed Git:
//!
//! - start, draft, review, and restore commits carry the required reserved
//!   trailers, and stock Git reads them back through its trailer tool
//! - structural validation checks exact trailer counts, the candidate
//!   parent, the full result tree, the scope digest, and out-of-scope
//!   equality, in both object formats
//! - forged and copied markers never validate, whatever they claim
//! - hooks stay in force: a rewritten or content-altering hook produces a
//!   commit that fails operation-time verification with `CONTRACT_VIOLATION`
//!   and confers no approval
//! - explicit messages cannot inject reserved trailers
//! - markers whose context depends on shallow history, grafts, or
//!   replacement objects do not validate

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;

use margin::app::{
    Identities, MarkerCommitError, MarkerError, MarkerInvalid, commit_marker, resolve_scope,
    validate_marker,
};
use margin::domain::{
    self, CommitKind, CommitMetadata, DecisionMode, PlanIdentity, RestoreSource, ScopeInput,
};
use margin::git::Git;
use support::repo::{ObjectFormat, RepoOptions, TestRepo};

/// Adapter bound to `repo`'s working tree and isolated environment.
fn adapter_for(repo: &TestRepo) -> Git {
    Git::from_env(repo.env().vars().clone()).at(repo.path())
}

/// Builds scope input from literal test values.
fn scope_input(include: &[&str], exclude: &[&str], suffixes: &[&str]) -> ScopeInput {
    ScopeInput {
        include: include.iter().map(|root| root.to_string()).collect(),
        exclude: exclude.iter().map(|root| root.to_string()).collect(),
        suffixes: suffixes.iter().map(|suffix| suffix.to_string()).collect(),
    }
}

/// The resolved writing scope used throughout this suite.
fn scope(repo: &TestRepo) -> margin::app::ResolvedScope {
    resolve_scope(
        &adapter_for(repo),
        &scope_input(&["content/"], &[], &[".md"]),
    )
    .unwrap_or_else(|error| panic!("the scope must resolve: {error}"))
}

/// The prepared metadata map with the common fields of every kind.
fn metadata(
    repo: &TestRepo,
    kind: CommitKind,
    candidate: &str,
    result_tree: &str,
) -> CommitMetadata {
    let scope = scope(repo);
    CommitMetadata {
        kind,
        scope_digest: scope.digest.id(),
        scope_spec: String::from_utf8(scope.descriptor.canonical_bytes()).unwrap(),
        candidate: candidate.to_string(),
        result_tree: result_tree.to_string(),
        baseline: None,
        request: None,
        plan: None,
        source: None,
    }
}

/// The plan fingerprint of one deterministic plan identity.
fn plan_id(repo: &TestRepo, operation: &str, request: &str, message: &str) -> String {
    let adapter = adapter_for(repo);
    let scope = scope(repo);
    let baseline = support::text(&repo.git_ok(["rev-parse", "HEAD"]));
    Identities::new(&adapter)
        .unwrap_or_else(|error| panic!("identities must read the repository: {error}"))
        .plan(&PlanIdentity {
            operation: operation.to_string(),
            request_id: request.to_string(),
            review_id: format!("sha1:{}", "0".repeat(40)),
            branch_ref: "refs/heads/main".to_string(),
            scope_id: scope.digest.oid,
            baseline_oid: baseline.clone(),
            candidate_oid: baseline,
            restore: Vec::new(),
            decision_mode: DecisionMode::KeepRemaining,
            message: message.to_string(),
        })
        .unwrap_or_else(|error| panic!("the plan must hash: {error}"))
        .id()
}

/// Validates one commit or panics with the finding.
fn valid(repo: &TestRepo, commit: &str) -> margin::app::ValidMarker {
    validate_marker(&adapter_for(repo), commit)
        .unwrap_or_else(|error| panic!("the marker must inspect: {error}"))
}

/// The finding of one invalid commit, or panics when it validates.
fn invalid(repo: &TestRepo, commit: &str) -> MarkerInvalid {
    match validate_marker(&adapter_for(repo), commit) {
        Err(MarkerError::Invalid(problem)) => problem,
        Err(other) => panic!("the marker must be inspectable: {other}"),
        Ok(marker) => panic!("the marker {commit} unexpectedly validates: {marker:?}"),
    }
}

/// Lists the trailers of one commit through stock Git's own trailer tool.
fn trailers_oracle(repo: &TestRepo, commit: &str) -> BTreeMap<String, String> {
    let message = repo.git_ok(["cat-file", "commit", commit]);
    let stdout = repo.git_stdin(["interpret-trailers", "--parse"], &message);
    let mut trailers = BTreeMap::new();
    for line in String::from_utf8_lossy(&stdout).lines() {
        let (token, value) = line
            .split_once(": ")
            .unwrap_or_else(|| panic!("git reports the trailer {line:?}"));
        trailers.insert(token.to_string(), value.to_string());
    }
    trailers
}

/// Writes one executable Git hook.
fn install_hook(repo: &TestRepo, name: &str, script: &str) {
    let path = repo.path().join(".git/hooks").join(name);
    fs::write(&path, script).unwrap_or_else(|e| panic!("cannot write the hook: {e}"));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|e| panic!("cannot chmod the hook: {e}"));
}

/// A seeded repository with one committed out-of-scope sentinel.
fn seeded_repo() -> TestRepo {
    let repo = TestRepo::init();
    repo.write_file("other/sentinel.txt", b"untouched\n");
    repo.write_file("content/a.md", b"first words\n");
    repo.commit_all("seed");
    repo
}

#[test]
fn start_markers_validate_on_an_unborn_branch() {
    let repo = seeded_repo();
    // A fresh start: an unborn branch carries no history at all. The orphan
    // checkout keeps the staged seed entries, so the index is emptied first
    // while the working files stay.
    repo.git_ok(["checkout", "--quiet", "--orphan", "writing"]);
    repo.git_ok(["rm", "--cached", "--quiet", "-r", "--", "."]);

    repo.write_file("content/a.md", b"first words\nsecond\n");
    repo.git_ok(["add", "--", "content/a.md"]);
    let tree = support::text(&repo.git_ok(["write-tree"]));
    let prepared = metadata(&repo, CommitKind::Start, domain::CANDIDATE_UNBORN, &tree);

    let marker = commit_marker(&adapter_for(&repo), &prepared, "capture the opening")
        .unwrap_or_else(|error| panic!("the start commit must verify: {error}"));

    // Stock Git sees the commit on the branch with the recorded trailers.
    let head = support::text(&repo.git_ok(["rev-parse", "HEAD"]));
    assert_eq!(marker.commit, head);
    assert_eq!(marker.kind, CommitKind::Start);
    assert_eq!(marker.candidate, domain::CANDIDATE_UNBORN);
    let trailers = trailers_oracle(&repo, &head);
    assert_eq!(
        trailers.get("Margin-Version").map(String::as_str),
        Some("1")
    );
    assert_eq!(
        trailers.get("Margin-Kind").map(String::as_str),
        Some("start")
    );
    assert_eq!(
        trailers.get("Margin-Result-Tree").map(String::as_str),
        Some(tree.as_str())
    );
    assert_eq!(
        trailers.get("Margin-Scope-Spec").map(String::as_str),
        Some(marker.scope_spec.as_str())
    );

    // The marker validates, and revalidates deterministically.
    let again = valid(&repo, &head);
    assert_eq!(again.commit, head);
    assert_eq!(again.result_tree, tree);
    assert_eq!(again.scope_digest, marker.scope_digest);
    // The starting tree holds only eligible scoped entries: the untracked
    // out-of-scope file never joined it.
    let entries = support::oracle::ls_tree(&repo, &tree);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, "content/a.md");
}

#[test]
fn draft_review_and_restore_markers_validate_in_sequence() {
    let repo = seeded_repo();
    let start_tree = support::text(&repo.git_ok(["rev-parse", "HEAD^{tree}"]));

    // Draft: a scoped edit committed on top of the seed commit.
    repo.write_file("content/a.md", b"first words\nsecond\n");
    repo.git_ok(["add", "--", "content/a.md"]);
    let parent = support::text(&repo.git_ok(["rev-parse", "HEAD"]));
    let tree = support::text(&repo.git_ok(["write-tree"]));
    let prepared = metadata(&repo, CommitKind::Draft, &parent, &tree);
    let draft = commit_marker(&adapter_for(&repo), &prepared, "")
        .unwrap_or_else(|error| panic!("the draft must verify: {error}"));

    // Review: a keep-all plan records an empty commit with the same tree.
    let review_tree = support::text(&repo.git_ok(["rev-parse", "HEAD^{tree}"]));
    assert_eq!(review_tree, tree);
    let mut review = metadata(&repo, CommitKind::Review, &draft.commit, &review_tree);
    review.baseline = Some(parent.clone());
    review.request = Some("review-introduction-01".to_string());
    review.plan = Some(plan_id(
        &repo,
        "review",
        "review-introduction-01",
        "keep everything",
    ));
    let reviewed = commit_marker(&adapter_for(&repo), &review, "keep all changes")
        .unwrap_or_else(|error| panic!("the review must verify: {error}"));
    assert_eq!(reviewed.kind, CommitKind::Review);
    assert_eq!(reviewed.baseline.as_deref(), Some(parent.as_str()));

    // Restore: the document returns to its seed content.
    repo.write_file("content/a.md", b"first words\n");
    repo.git_ok(["add", "--", "content/a.md"]);
    let restore_tree = support::text(&repo.git_ok(["write-tree"]));
    let mut restore = metadata(&repo, CommitKind::Restore, &reviewed.commit, &restore_tree);
    restore.request = Some("restore-ending-01".to_string());
    restore.plan = Some(plan_id(
        &repo,
        "restore",
        "restore-ending-01",
        "restore the ending",
    ));
    restore.source = Some(RestoreSource {
        path: "content/a.md".to_string(),
        revision: parent.clone(),
    });
    let restored = commit_marker(&adapter_for(&repo), &restore, "")
        .unwrap_or_else(|error| panic!("the restore must verify: {error}"));
    assert_eq!(restored.kind, CommitKind::Restore);

    // Every marker of the chain validates and keeps first-parent order.
    let chain: Vec<String> =
        String::from_utf8_lossy(&repo.git_ok(["log", "--first-parent", "--format=%H", "HEAD"]))
            .lines()
            .map(str::to_string)
            .collect();
    assert_eq!(chain[0], restored.commit);
    assert_eq!(chain[1], reviewed.commit);
    assert_eq!(chain[2], draft.commit);
    for commit in &chain[..3] {
        valid(&repo, commit);
    }

    // The keep-all review commit is intentionally empty, and out-of-scope
    // trees never moved across the whole sequence.
    assert_eq!(
        support::text(&repo.git_ok([
            "diff-tree",
            "-r",
            "--name-only",
            "--no-commit-id",
            &reviewed.commit
        ])),
        String::new(),
        "the keep-all review rewrote no content against its own parent"
    );
    support::oracle::assert_out_of_scope_tree_unchanged(
        &repo,
        &start_tree,
        &restored.commit,
        &["content"],
        "the marker sequence",
    );
    assert_eq!(repo.read_file("other/sentinel.txt"), b"untouched\n");
}

#[test]
fn sha256_repositories_carry_their_own_identity_width() {
    let repo = TestRepo::init_with(RepoOptions::new().with_object_format(ObjectFormat::Sha256));
    repo.write_file("content/a.md", b"sha256 words\n");
    repo.git_ok(["add", "--", "content/a.md"]);
    let tree = support::text(&repo.git_ok(["write-tree"]));
    let prepared = metadata(&repo, CommitKind::Start, domain::CANDIDATE_UNBORN, &tree);
    let marker = commit_marker(&adapter_for(&repo), &prepared, "wide start")
        .unwrap_or_else(|error| panic!("the sha256 start must verify: {error}"));

    // Every recorded identity names the sha256 format and full-width OIDs.
    assert!(marker.scope_digest.starts_with("sha256:"));
    assert_eq!(marker.scope_digest.len(), "sha256:".len() + 64);
    assert_eq!(marker.result_tree.len(), 64);
    assert_eq!(
        trailers_oracle(&repo, &marker.commit)
            .get("Margin-Scope")
            .map(String::as_str),
        Some(marker.scope_digest.as_str())
    );
    valid(&repo, &marker.commit);
}

/// Commits one forged marker with stock `git commit-tree`, which runs no
/// hooks and moves no branch: exactly what a forger would do.
fn forge(repo: &TestRepo, tree: &str, parents: &[&str], message: &[u8]) -> String {
    let mut argv: Vec<String> = vec!["commit-tree".to_string(), tree.to_string()];
    for parent in parents {
        argv.push("-p".to_string());
        argv.push((*parent).to_string());
    }
    let output = repo.git_stdin(
        argv.iter().map(String::as_str).collect::<Vec<&str>>(),
        message,
    );
    support::text(&output)
}

/// Builds one forged draft message from the prepared map, optionally
/// tampering trailer lines.
fn forged_draft_message(repo: &TestRepo, candidate: &str, tamper: Option<(&str, &str)>) -> Vec<u8> {
    let parent_tree = support::text(&repo.git_ok(["rev-parse", "HEAD^{tree}"]));
    let prepared = metadata(repo, CommitKind::Draft, candidate, &parent_tree);
    let mut message = String::from_utf8(prepared.commit_message("forged").unwrap()).unwrap();
    if let Some((from, to)) = tamper {
        message = message.replace(from, to);
    }
    message.into_bytes()
}

#[test]
fn forged_result_trees_and_copied_trailers_never_validate() {
    let repo = seeded_repo();
    let parent = support::text(&repo.git_ok(["rev-parse", "HEAD"]));

    // A marker whose recorded tree is not the actual tree.
    let wrong_tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string();
    let message = forged_draft_message(&repo, &parent, None);
    let forged = forge(&repo, &wrong_tree, &[&parent], &message);
    assert_eq!(
        invalid(&repo, &forged),
        MarkerInvalid::ResultTreeMismatch {
            trailer: support::text(&repo.git_ok(["rev-parse", &format!("{parent}^{{tree}}")])),
            actual: wrong_tree,
        }
    );

    // A structurally valid draft that also stages an out-of-scope file: the
    // recorded tree matches the smuggled tree, so only rule 5 can catch it.
    repo.write_file("other/foreign.md", b"smuggled\n");
    repo.git_ok(["add", "--", "other/foreign.md"]);
    let smuggle_tree = support::text(&repo.git_ok(["write-tree"]));
    let prepared = metadata(&repo, CommitKind::Draft, &parent, &smuggle_tree);
    let message = prepared.commit_message("forged").unwrap();
    let forged = forge(&repo, &smuggle_tree, &[&parent], &message);
    assert_eq!(
        invalid(&repo, &forged),
        MarkerInvalid::OutOfScopeChange {
            paths: vec!["other/foreign.md".to_string()],
        }
    );
    repo.git_ok(["reset", "--quiet", "HEAD", "--", "other/foreign.md"]);
    repo.git_ok(["checkout", "--quiet", "--", "other/sentinel.txt"]);
    fs::remove_file(repo.file_path("other/foreign.md")).unwrap();
}

#[test]
fn copied_trailer_counts_and_values_stay_invalid() {
    let repo = seeded_repo();
    let parent = support::text(&repo.git_ok(["rev-parse", "HEAD"]));
    let tree = support::text(&repo.git_ok(["rev-parse", "HEAD^{tree}"]));

    // A duplicated version trailer fails the exact-count rule.
    let message = forged_draft_message(&repo, &parent, None);
    let mut duplicated = message.clone();
    duplicated.extend_from_slice(b"Margin-Version: 1\n");
    let forged = forge(&repo, &tree, &[&parent], &duplicated);
    assert_eq!(
        invalid(&repo, &forged),
        MarkerInvalid::Trailers(domain::TrailerProblem::Duplicate {
            name: domain::TRAILER_VERSION,
            count: 2,
        })
    );

    // A future trailer contract version is not the supported one.
    let message = forged_draft_message(
        &repo,
        &parent,
        Some(("Margin-Version: 1", "Margin-Version: 2")),
    );
    let forged = forge(&repo, &tree, &[&parent], &message);
    assert_eq!(
        invalid(&repo, &forged),
        MarkerInvalid::Trailers(domain::TrailerProblem::UnsupportedVersion {
            found: "2".to_string(),
        })
    );

    // A scope digest that the recorded descriptor does not produce: flip one
    // hex digit, keeping the identity shape intact.
    let recorded = scope(&repo).digest.id();
    let flipped = format!(
        "{}{}",
        &recorded[..recorded.len() - 1],
        if recorded.ends_with('0') { '1' } else { '0' }
    );
    let message = forged_draft_message(&repo, &parent, Some((recorded.as_str(), flipped.as_str())));
    let forged = forge(&repo, &tree, &[&parent], &message);
    assert_eq!(
        invalid(&repo, &forged),
        MarkerInvalid::ScopeDigestMismatch {
            trailer: flipped.clone(),
            computed: recorded,
        }
    );

    // A review marker without its request receipt is not a review.
    let mut review = metadata(&repo, CommitKind::Review, &parent, &tree);
    review.baseline = Some(parent.clone());
    review.request = Some("review-1".to_string());
    review.plan = Some(plan_id(&repo, "review", "review-1", "keep"));
    let mut message = String::from_utf8(review.commit_message("m").unwrap()).unwrap();
    message = message
        .lines()
        .filter(|line| !line.starts_with("Margin-Request:"))
        .collect::<Vec<&str>>()
        .join("\n")
        + "\n";
    let forged = forge(&repo, &tree, &[&parent], message.as_bytes());
    assert_eq!(
        invalid(&repo, &forged),
        MarkerInvalid::Trailers(domain::TrailerProblem::Missing {
            name: domain::TRAILER_REQUEST,
        })
    );

    // A draft that claims a review baseline is refused by kind.
    let message = forged_draft_message(
        &repo,
        &parent,
        Some((
            "Margin-Candidate:",
            "Margin-Baseline: 0123\nMargin-Candidate:",
        )),
    );
    let forged = forge(&repo, &tree, &[&parent], &message);
    assert_eq!(
        invalid(&repo, &forged),
        MarkerInvalid::Trailers(domain::TrailerProblem::ForbiddenTrailer {
            name: domain::TRAILER_BASELINE,
            kind: CommitKind::Draft,
        })
    );
}

#[test]
fn candidate_and_parent_must_agree() {
    let repo = seeded_repo();
    repo.write_file("content/a.md", b"first words\nmore\n");
    let parent = repo.commit_all("second seed");
    let tree = support::text(&repo.git_ok(["rev-parse", "HEAD^{tree}"]));
    let seed = support::text(&repo.git_ok(["rev-parse", "HEAD~1"]));

    // The recorded candidate is not the actual parent.
    let message = forged_draft_message(&repo, &seed, None);
    let forged = forge(&repo, &tree, &[&parent], &message);
    assert_eq!(
        invalid(&repo, &forged),
        MarkerInvalid::CandidateMismatch {
            trailer: seed.clone(),
            parent: Some(parent.clone()),
        }
    );

    // A merge commit carries two parents, whatever its trailers say.
    let message = forged_draft_message(&repo, &parent, None);
    let forged = forge(&repo, &tree, &[&parent, &seed], &message);
    assert_eq!(
        invalid(&repo, &forged),
        MarkerInvalid::ParentCount { found: 2 }
    );

    // A parentless draft is not an initial start.
    let message = forged_draft_message(&repo, domain::CANDIDATE_UNBORN, None);
    let forged = forge(&repo, &tree, &[], &message);
    assert_eq!(invalid(&repo, &forged), MarkerInvalid::RootNotStart);
}

#[test]
fn ineligible_start_entries_block_the_root_case() {
    let repo = seeded_repo();

    // A root start whose tree holds an out-of-scope entry.
    repo.git_ok(["checkout", "--quiet", "--orphan", "writing"]);
    repo.git_ok(["rm", "--cached", "--quiet", "-r", "--", "."]);
    repo.git_ok(["add", "--", "other/sentinel.txt", "content/a.md"]);
    let tree = support::text(&repo.git_ok(["write-tree"]));
    let prepared = metadata(&repo, CommitKind::Start, domain::CANDIDATE_UNBORN, &tree);
    let message = prepared.commit_message("bad start").unwrap();
    let forged = forge(&repo, &tree, &[], &message);
    assert_eq!(
        invalid(&repo, &forged),
        MarkerInvalid::IneligibleStartEntry {
            path: "other/sentinel.txt".to_string(),
            why: "the entry lies outside the starting scope",
        }
    );

    // A stored symbolic link is not an eligible scoped file either.
    let repo = seeded_repo();
    repo.git_ok(["checkout", "--quiet", "--orphan", "writing"]);
    repo.git_ok(["rm", "--cached", "--quiet", "-r", "--", "."]);
    std::os::unix::fs::symlink("a.md", repo.file_path("content/link.md")).unwrap();
    repo.git_ok(["add", "--", "content/link.md"]);
    let tree = support::text(&repo.git_ok(["write-tree"]));
    let prepared = metadata(&repo, CommitKind::Start, domain::CANDIDATE_UNBORN, &tree);
    let message = prepared.commit_message("linked start").unwrap();
    let forged = forge(&repo, &tree, &[], &message);
    assert_eq!(
        invalid(&repo, &forged),
        MarkerInvalid::IneligibleStartEntry {
            path: "content/link.md".to_string(),
            why: "the entry is not a regular file",
        }
    );
}

/// One staged draft change ready to commit, with its prepared map.
fn staged_draft(repo: &TestRepo) -> (CommitMetadata, margin::app::ResolvedScope) {
    repo.write_file("content/a.md", b"first words\nedited\n");
    repo.git_ok(["add", "--", "content/a.md"]);
    let tree = support::text(&repo.git_ok(["write-tree"]));
    let parent = support::text(&repo.git_ok(["rev-parse", "HEAD"]));
    (
        metadata(repo, CommitKind::Draft, &parent, &tree),
        scope(repo),
    )
}

#[test]
fn hook_that_rewrites_reserved_trailers_fails_verification() {
    let repo = seeded_repo();
    // The commit-msg hook rewrites the recorded kind: the commit still
    // succeeds, so only the post-commit comparison can catch it.
    install_hook(
        &repo,
        "commit-msg",
        "#!/bin/sh\nsed -i.bak 's/^Margin-Kind: draft$/Margin-Kind: review/' \"$1\"\nrm -f \"$1.bak\"\nexit 0\n",
    );
    let (prepared, _) = staged_draft(&repo);

    let error = commit_marker(&adapter_for(&repo), &prepared, "passage")
        .expect_err("a rewritten marker must not verify");
    match &error {
        MarkerCommitError::Verification { commit, report } => {
            assert_eq!(*commit, support::text(&repo.git_ok(["rev-parse", "HEAD"])));
            assert!(
                !report.problems.is_empty() || !report.differences.is_empty(),
                "the verification reports what differed: {error}"
            );
        }
        other => panic!("the failure must be a verification: {other}"),
    }
    assert_eq!(error.public_code(), "CONTRACT_VIOLATION");

    // The commit exists in ordinary history and never validates.
    let head = support::text(&repo.git_ok(["rev-parse", "HEAD"]));
    assert!(matches!(invalid(&repo, &head), MarkerInvalid::Trailers(_)));
}

#[test]
fn hook_that_changes_content_fails_verification() {
    let repo = seeded_repo();
    // The pre-commit hook stages an extra out-of-scope file, so the actual
    // tree no longer equals the prepared result tree.
    install_hook(
        &repo,
        "pre-commit",
        "#!/bin/sh\necho smuggled > hook-file.txt\ngit add -- hook-file.txt\nexit 0\n",
    );
    let (prepared, _) = staged_draft(&repo);

    let error = commit_marker(&adapter_for(&repo), &prepared, "passage")
        .expect_err("content changed under the commit");
    match &error {
        MarkerCommitError::Verification { commit, report } => {
            // The recorded result tree names the prepared tree, so the
            // finding is a mismatch, never approval.
            assert!(
                report
                    .problems
                    .iter()
                    .any(|problem| matches!(problem, MarkerInvalid::ResultTreeMismatch { .. })),
                "the verification names the tree mismatch: {error}"
            );
            // The smuggled commit is the branch head.
            assert_eq!(commit, &support::text(&repo.git_ok(["rev-parse", "HEAD"])));
        }
        other => panic!("the failure must be a verification: {other}"),
    }
    assert_eq!(error.public_code(), "CONTRACT_VIOLATION");
    // The smuggled file is in the committed tree: nothing was deleted, and
    // the commit is ordinary history that never validates.
    assert_eq!(
        support::text(&repo.git_ok(["ls-tree", "-r", "--name-only", "HEAD"])),
        "content/a.md\nhook-file.txt\nother/sentinel.txt"
    );
    assert!(matches!(
        invalid(&repo, &support::text(&repo.git_ok(["rev-parse", "HEAD"]))),
        MarkerInvalid::ResultTreeMismatch { .. }
    ));
}

#[test]
fn hook_appending_unreserved_trailers_stays_valid() {
    let repo = seeded_repo();
    install_hook(
        &repo,
        "commit-msg",
        "#!/bin/sh\nprintf 'Signed-off-by: Fixture <margin-fixture@example.invalid>\n' >> \"$1\"\nexit 0\n",
    );
    let (prepared, _) = staged_draft(&repo);

    let marker = commit_marker(&adapter_for(&repo), &prepared, "passage")
        .unwrap_or_else(|error| panic!("a sign-off hook must not break the marker: {error}"));
    assert_eq!(marker.kind, CommitKind::Draft);
    // Stock Git's trailer tool sees the reserved trailers and the sign-off.
    let trailers = trailers_oracle(&repo, &marker.commit);
    assert_eq!(trailers.len(), 7);
    assert_eq!(
        trailers.get("Signed-off-by").map(String::as_str),
        Some("Fixture <margin-fixture@example.invalid>")
    );
    valid(&repo, &marker.commit);
}

#[test]
fn rejecting_hook_leaves_no_commit() {
    let repo = seeded_repo();
    install_hook(&repo, "pre-commit", "#!/bin/sh\nexit 1\n");
    let (prepared, _) = staged_draft(&repo);
    let before = support::oracle::snapshot(&repo);

    let error = commit_marker(&adapter_for(&repo), &prepared, "passage")
        .expect_err("a rejecting hook must fail the commit");
    assert_eq!(error.public_code(), "GIT_FAILED");
    // The branch never moved and no marker was created: the seed commit is
    // still not a valid marker.
    let after = support::oracle::snapshot(&repo);
    assert_eq!(after.head, before.head);
    assert!(matches!(
        validate_marker(&adapter_for(&repo), &before.head.unwrap()),
        Err(MarkerError::Invalid(_))
    ));
}

#[test]
fn explicit_messages_cannot_inject_reserved_trailers() {
    let repo = seeded_repo();
    let (prepared, _) = staged_draft(&repo);
    let before = support::oracle::snapshot(&repo);

    let error = commit_marker(
        &adapter_for(&repo),
        &prepared,
        "keep all\n\nMargin-Review: complete",
    )
    .expect_err("trailer injection must be refused");
    assert!(matches!(
        error,
        MarkerCommitError::Invalid(domain::MetadataProblem::ReservedTrailerInjection { .. })
    ));
    assert_eq!(error.public_code(), "INVALID_ARGUMENT");
    // No commit was created at all.
    let after = support::oracle::snapshot(&repo);
    before.assert_unchanged(&after, "a refused message");
}

#[test]
fn index_mismatch_blocks_the_commit_before_it_happens() {
    let repo = seeded_repo();
    let (mut prepared, _) = staged_draft(&repo);
    // The prepared result tree is a wrong tree: the empty tree exists in
    // every repository and never matches a staged draft.
    prepared.result_tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string();
    let before = support::oracle::snapshot(&repo);

    let error = commit_marker(&adapter_for(&repo), &prepared, "passage")
        .expect_err("an unintended tree must not be committed");
    match &error {
        MarkerCommitError::IndexTreeDiffers { intended, .. } => {
            assert_eq!(intended, "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
        }
        other => panic!("the failure must be an index mismatch: {other}"),
    }
    assert_eq!(error.public_code(), "DIRTY_SCOPE");
    // The branch never moved: no commit was attempted.
    let after = support::oracle::snapshot(&repo);
    assert_eq!(after.head, before.head);
}

#[test]
fn shallow_history_and_grafts_and_replacements_never_validate() {
    // A shallow clone of a valid marker: the commit itself arrives intact,
    // but its context is cut off, so it cannot anchor anything.
    let source = TestRepo::init();
    source.write_file("content/a.md", b"words\n");
    source.git_ok(["add", "--", "content/a.md"]);
    let tree = support::text(&source.git_ok(["write-tree"]));
    let prepared = metadata(&source, CommitKind::Start, domain::CANDIDATE_UNBORN, &tree);
    let marker = commit_marker(&adapter_for(&source), &prepared, "start")
        .unwrap_or_else(|error| panic!("the source start must verify: {error}"));

    let shallow_path = source.root().join("shallow-clone");
    let output = std::process::Command::new("git")
        .args([
            "clone",
            "--quiet",
            "--depth",
            "1",
            "--no-local",
            &format!("file://{}", source.path().display()),
        ])
        .arg(&shallow_path)
        .output()
        .expect("git clone must run");
    assert!(
        output.status.success(),
        "the shallow clone must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let shallow = TestRepo::init();
    let _ = std::fs::remove_dir_all(shallow.path());
    let _ = std::fs::rename(&shallow_path, shallow.path());
    assert_eq!(
        invalid(&shallow, &marker.commit),
        MarkerInvalid::UnavailableContext {
            what: "shallow clone history",
        }
    );

    // A grafts file reshapes history even though the object is untouched.
    let repo = seeded_repo();
    let parent = support::text(&repo.git_ok(["rev-parse", "HEAD"]));
    let tree = support::text(&repo.git_ok(["write-tree"]));
    let prepared = metadata(&repo, CommitKind::Draft, &parent, &tree);
    let message = prepared.commit_message("draft").unwrap();
    let forged = forge(&repo, &tree, &[&parent], &message);
    valid(&repo, &forged);
    fs::write(
        repo.path().join(".git/info/grafts"),
        format!("{parent} 0000000000000000000000000000000000000000\n"),
    )
    .unwrap();
    assert_eq!(
        invalid(&repo, &forged),
        MarkerInvalid::UnavailableContext {
            what: "grafted history",
        }
    );

    // One replacement ref makes the ordinary history view a substitution.
    let repo = seeded_repo();
    let parent = support::text(&repo.git_ok(["rev-parse", "HEAD"]));
    let tree = support::text(&repo.git_ok(["write-tree"]));
    let prepared = metadata(&repo, CommitKind::Draft, &parent, &tree);
    let message = prepared.commit_message("draft").unwrap();
    let forged = forge(&repo, &tree, &[&parent], &message);
    valid(&repo, &forged);
    repo.git_ok(["replace", "--graft", &forged]);
    assert_eq!(
        invalid(&repo, &forged),
        MarkerInvalid::UnavailableContext {
            what: "replacement objects",
        }
    );
}

#[test]
fn cached_marker_validation_tracks_context_changes() {
    let repo = seeded_repo();
    let parent = support::text(&repo.git_ok(["rev-parse", "HEAD"]));
    let tree = support::text(&repo.git_ok(["write-tree"]));
    let prepared = metadata(&repo, CommitKind::Draft, &parent, &tree);
    let commit = forge(
        &repo,
        &tree,
        &[&parent],
        &prepared.commit_message("draft").unwrap(),
    );
    let git = adapter_for(&repo);
    let mut walk = margin::app::HistoryWalk::new(&git);
    assert!(walk.validate_marker(&commit).is_ok());
    let grafts = repo.path().join(".git/info/grafts");
    fs::write(&grafts, "# changed context\n").unwrap();
    walk.forget_context();
    assert!(matches!(
        walk.validate_marker(&commit),
        Err(MarkerError::Invalid(MarkerInvalid::UnavailableContext {
            what: "grafted history"
        }))
    ));
    let mut initially_blocked = margin::app::HistoryWalk::new(&git);
    assert!(initially_blocked.validate_marker(&commit).is_err());
    fs::remove_file(grafts).unwrap();
    for current in [&mut walk, &mut initially_blocked] {
        current.forget_context();
        assert!(
            current.validate_marker(&commit).is_ok(),
            "a context refusal must not persist after context returns"
        );
    }
}
