//! Real-Git tests for baseline selection (SPEC 8.5, task T013).
//!
//! Every case proves one selection rule against the installed Git:
//!
//! - the unfiltered first-parent walk stops at the nearest valid matching
//!   start or review marker, and it sees empty review commits that a
//!   path-limited log hides
//! - the baseline equals the candidate immediately after a start or review
//!   commit, and every review must record the baseline the walk selects at
//!   its own candidate, never a merely older marker
//! - a new start resets the anchor: the walk never crosses one to revive an
//!   older review, so scope A → B → A uses its newly established A anchor
//! - drafts, restores, ordinary external commits, and external merges leave
//!   the prior baseline unchanged
//! - unavailable history and history without a matching marker return
//!   `BASELINE_UNAVAILABLE`, never an arbitrary initial commit

mod support;

use margin::app::{
    BaselineError, BaselineUnavailable, Identities, commit_marker, resolve_scope, select_baseline,
};
use margin::domain::{
    CommitKind, CommitMetadata, DecisionMode, PlanIdentity, RestoreSource, ScopeInput,
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

/// Resolves the writing scope rooted at `include`.
fn scope_with_root(repo: &TestRepo, include: &str) -> margin::app::ResolvedScope {
    resolve_scope(&adapter_for(repo), &scope_input(&[include], &[], &[".md"]))
        .unwrap_or_else(|error| panic!("the scope must resolve: {error}"))
}

/// The prepared metadata map of one marker for `scope`.
fn metadata_for(
    scope: &margin::app::ResolvedScope,
    kind: CommitKind,
    candidate: &str,
    result_tree: &str,
) -> CommitMetadata {
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

/// The fingerprint of one deterministic plan identity.
fn plan_id(
    repo: &TestRepo,
    scope: &margin::app::ResolvedScope,
    operation: &str,
    request: &str,
    baseline: &str,
    candidate: &str,
    message: &str,
) -> String {
    Identities::new(&adapter_for(repo))
        .unwrap_or_else(|error| panic!("identities must read the repository: {error}"))
        .plan(&PlanIdentity {
            operation: operation.to_string(),
            request_id: request.to_string(),
            review_id: format!("sha1:{}", "0".repeat(40)),
            branch_ref: "refs/heads/main".to_string(),
            scope_id: scope.digest.oid.clone(),
            baseline_oid: baseline.to_string(),
            candidate_oid: candidate.to_string(),
            restore: Vec::new(),
            decision_mode: DecisionMode::KeepRemaining,
            message: message.to_string(),
        })
        .unwrap_or_else(|error| panic!("the plan must hash: {error}"))
        .id()
}

/// Stages nothing extra and commits one verified marker of `kind` on the
/// current index.
///
/// Reviews record `baseline`; restores record `source_revision`. The label
/// seeds the request ID, so repeated calls stay distinct receipts.
fn mark(
    repo: &TestRepo,
    scope: &margin::app::ResolvedScope,
    kind: CommitKind,
    label: &str,
    baseline: Option<&str>,
    source_revision: Option<&str>,
) -> margin::app::ValidMarker {
    let parent = support::text(&repo.git_ok(["rev-parse", "HEAD"]));
    let tree = support::text(&repo.git_ok(["write-tree"]));
    let mut prepared = metadata_for(scope, kind, &parent, &tree);
    // A request identifier holds letters, digits, and `.`, `_`, `:`, `-`
    // only (SPEC 7.3), while the label stays free text.
    let slug = label.replace(' ', "-");
    match kind {
        CommitKind::Review => {
            let baseline = baseline.unwrap_or(&parent);
            let request = format!("review-{slug}");
            prepared.baseline = Some(baseline.to_string());
            prepared.request = Some(request.clone());
            prepared.plan = Some(plan_id(
                repo, scope, "review", &request, baseline, &parent, label,
            ));
        }
        CommitKind::Restore => {
            let request = format!("restore-{slug}");
            prepared.request = Some(request.clone());
            prepared.plan = Some(plan_id(
                repo, scope, "restore", &request, &parent, &parent, label,
            ));
            prepared.source = Some(RestoreSource {
                path: "content/a.md".to_string(),
                revision: source_revision
                    .expect("a restore names its source revision")
                    .to_string(),
            });
        }
        CommitKind::Start | CommitKind::Draft => {}
    }
    commit_marker(&adapter_for(repo), &prepared, label)
        .unwrap_or_else(|error| panic!("the {} marker must verify: {error}", kind.as_str()))
}

/// Selects the baseline or panics with the failure.
fn selected(
    repo: &TestRepo,
    candidate: &str,
    scope: &margin::app::ResolvedScope,
) -> margin::app::ValidMarker {
    select_baseline(&adapter_for(repo), candidate, &scope.digest)
        .unwrap_or_else(|error| panic!("the baseline must select: {error}"))
}

/// A seeded repository with one committed out-of-scope sentinel.
fn seeded_repo() -> TestRepo {
    let repo = TestRepo::init();
    repo.write_file("other/sentinel.txt", b"untouched\n");
    repo.write_file("content/a.md", b"first words\n");
    repo.commit_all("seed");
    repo
}

/// Builds `seed → start → draft → keep-all review` and returns the
/// repository, its scope, and the three marker commits.
fn reviewed_repo() -> (TestRepo, margin::app::ResolvedScope, String, String, String) {
    let repo = seeded_repo();
    let scope = scope_with_root(&repo, "content/");

    // Adoption captures the already written draft on top of the seed.
    repo.write_file("content/a.md", b"first words\nsecond\n");
    repo.git_ok(["add", "--", "content/a.md"]);
    let start = mark(
        &repo,
        &scope,
        CommitKind::Start,
        "capture the opening",
        None,
        None,
    );

    // A draft revises the introduction.
    repo.write_file("content/a.md", b"first words\nsecond\nthird\n");
    repo.git_ok(["add", "--", "content/a.md"]);
    let draft = mark(
        &repo,
        &scope,
        CommitKind::Draft,
        "revise the tone",
        None,
        None,
    );

    // A keep-all review: an intentionally empty commit on the draft tree.
    let review = mark(
        &repo,
        &scope,
        CommitKind::Review,
        "keep every change",
        Some(&start.commit),
        None,
    );
    assert_eq!(
        support::text(&repo.git_ok([
            "diff-tree",
            "-r",
            "--name-only",
            "--no-commit-id",
            &review.commit
        ])),
        String::new(),
        "the keep-all review rewrote no content against its own parent"
    );

    (repo, scope, start.commit, draft.commit, review.commit)
}

#[test]
fn the_walk_stops_at_the_nearest_marker_and_sees_empty_commits() {
    let (repo, scope, start, draft, review) = reviewed_repo();
    // More writing after the review, still unreviewed.
    repo.write_file("content/a.md", b"first words\nsecond\nthird\nfourth\n");
    repo.git_ok(["add", "--", "content/a.md"]);
    let later = mark(
        &repo,
        &scope,
        CommitKind::Draft,
        "revise the ending",
        None,
        None,
    );

    // Selection at every candidate follows one rule: the nearest valid
    // start or review, the candidate itself included.
    let anchor = selected(&repo, &later.commit, &scope);
    assert_eq!(anchor.commit, review);
    assert_eq!(anchor.kind, CommitKind::Review);
    assert_eq!(anchor.scope_digest, scope.digest.id());
    assert_eq!(selected(&repo, &review, &scope).commit, review);
    assert_eq!(selected(&repo, &draft, &scope).commit, start);
    let anchored_start = selected(&repo, &start, &scope);
    assert_eq!(anchored_start.commit, start);
    assert_eq!(anchored_start.kind, CommitKind::Start);

    // Stock Git's own path-limited log hides the empty review commit; the
    // selection walk still finds it (SPEC 8.5).
    let path_history: Vec<String> =
        support::text(&repo.git_ok(["log", "--first-parent", "--format=%H", "--", "content/a.md"]))
            .lines()
            .map(str::to_string)
            .collect();
    assert!(
        path_history.contains(&later.commit),
        "the path log lists the latest draft"
    );
    assert!(
        !path_history.contains(&review),
        "the path log drops the empty review commit"
    );
}

#[test]
fn a_review_must_record_the_baseline_the_walk_selects() {
    let (repo, scope, start, _draft, review) = reviewed_repo();
    // A second review directly on the first: its candidate is the review
    // below it, so B equals C in the walk from that candidate (SPEC 8.4
    // rule 6, 8.5).
    let chained = mark(
        &repo,
        &scope,
        CommitKind::Review,
        "chained keep-all",
        Some(&review),
        None,
    );
    assert_eq!(chained.baseline.as_deref(), Some(review.as_str()));
    assert_eq!(
        selected(&repo, &chained.commit, &scope).commit,
        chained.commit
    );

    // A draft after that review, then a review that claims the older start
    // instead of the selected review. The prepared map is self-consistent,
    // so the commit itself verifies; only selection can refuse it.
    repo.write_file("content/a.md", b"first words\nsecond\nthird\nfourth\n");
    repo.git_ok(["add", "--", "content/a.md"]);
    mark(
        &repo,
        &scope,
        CommitKind::Draft,
        "revise the ending",
        None,
        None,
    );
    let wrong = mark(
        &repo,
        &scope,
        CommitKind::Review,
        "claims an older marker",
        Some(&start),
        None,
    );
    assert_eq!(wrong.baseline.as_deref(), Some(start.as_str()));

    // Rule 6 fails for the forged claim, so it confers nothing and the walk
    // continues to the real anchor.
    let anchor = selected(&repo, &wrong.commit, &scope);
    assert_eq!(anchor.commit, chained.commit);
    assert_eq!(anchor.kind, CommitKind::Review);
}

#[test]
fn a_review_without_any_anchor_below_it_cannot_anchor_the_scope() {
    let repo = seeded_repo();
    let scope = scope_with_root(&repo, "content/");
    // A review on top of history that holds no marker at all: nothing can
    // reproduce its recorded baseline.
    repo.write_file("content/a.md", b"first words\nsecond\n");
    repo.git_ok(["add", "--", "content/a.md"]);
    let orphan = mark(
        &repo,
        &scope,
        CommitKind::Review,
        "orphaned review",
        Some(&"0".repeat(40)),
        None,
    );

    let error = select_baseline(&adapter_for(&repo), &orphan.commit, &scope.digest)
        .expect_err("no anchor exists below the review");
    assert_eq!(error.public_code(), "BASELINE_UNAVAILABLE");
    assert_eq!(
        error.to_string(),
        "2 first-parent commits hold no valid matching start or review marker"
    );
}

#[test]
fn a_new_start_resets_the_anchor() {
    let (repo, scope, _start, _draft, review) = reviewed_repo();
    // An explicit new start on top of the reviewed history, then more
    // unreviewed writing.
    let fresh = mark(&repo, &scope, CommitKind::Start, "new start", None, None);
    repo.write_file("content/a.md", b"rewritten opening\n");
    repo.git_ok(["add", "--", "content/a.md"]);
    let after = mark(&repo, &scope, CommitKind::Draft, "revise again", None, None);

    // The walk stops at the new start and never revives the older review.
    let anchor = selected(&repo, &after.commit, &scope);
    assert_eq!(anchor.commit, fresh.commit);
    assert_eq!(anchor.kind, CommitKind::Start);
    assert_ne!(anchor.commit, review);
    assert_eq!(selected(&repo, &fresh.commit, &scope).commit, fresh.commit);
}

#[test]
fn a_foreign_start_blocks_older_approval_of_the_returned_scope() {
    let (repo, scope, _start, _draft, review) = reviewed_repo();
    // A second scope adopted on the same branch: its starting marker resets
    // the anchor for its own scope.
    repo.write_file("notes/n.md", b"one note\n");
    repo.git_ok(["add", "--", "notes/n.md"]);
    let notes = scope_with_root(&repo, "notes/");
    let notes_start = mark(
        &repo,
        &notes,
        CommitKind::Start,
        "adopt the notes",
        None,
        None,
    );

    // The notes scope anchors at its own start.
    assert_eq!(
        selected(&repo, &notes_start.commit, &notes).commit,
        notes_start.commit
    );

    // The writing scope now has no baseline: the walk stops at the foreign
    // start instead of crossing it to revive the older review (SPEC 8.2).
    let error = select_baseline(&adapter_for(&repo), &notes_start.commit, &scope.digest)
        .expect_err("a foreign start anchors this history");
    assert_eq!(error.public_code(), "BASELINE_UNAVAILABLE");
    match &error {
        BaselineError::Unavailable(BaselineUnavailable::ForeignStart {
            commit,
            scope_digest,
        }) => {
            assert_eq!(commit, &notes_start.commit);
            assert_eq!(scope_digest, &notes.digest.id());
        }
        other => panic!("the failure must name the foreign start: {other}"),
    }

    // Re-initializing the writing scope establishes its own new anchor. The
    // old approval stays dead: scope A → B → A uses the new A anchor.
    let back = mark(
        &repo,
        &scope,
        CommitKind::Start,
        "readopt writing",
        None,
        None,
    );
    let anchor = selected(&repo, &back.commit, &scope);
    assert_eq!(anchor.commit, back.commit);
    assert_eq!(anchor.kind, CommitKind::Start);
    assert_ne!(anchor.commit, review);
}

#[test]
fn ordinary_commits_and_external_merges_leave_the_prior_baseline() {
    let (repo, scope, _start, _draft, review) = reviewed_repo();
    // An ordinary external commit changes scoped content without markers.
    repo.write_file("content/a.md", b"external edit\n");
    let external = repo.commit_all("external edit");
    assert_eq!(selected(&repo, &external, &scope).commit, review);

    // A completed external merge: the walk follows the first parent only.
    let branch = support::text(&repo.git_ok(["branch", "--show-current"]));
    repo.git_ok(["checkout", "--quiet", "-b", "side"]);
    repo.write_file("other/branch.txt", b"side work\n");
    repo.commit_all("side work");
    repo.git_ok(["checkout", "--quiet", &branch]);
    repo.git_ok(["merge", "--quiet", "--no-ff", "side"]);
    let merged = support::text(&repo.git_ok(["rev-parse", "HEAD"]));
    // The merge commit really has two parents, and both are inspectable.
    repo.git_ok(["rev-parse", "--verify", "HEAD^2"]);
    assert_eq!(selected(&repo, &merged, &scope).commit, review);
}

#[test]
fn a_restore_never_advances_the_baseline() {
    let (repo, scope, start, _draft, review) = reviewed_repo();
    // Restore the seed content: a receipt commit, never approval.
    repo.write_file("content/a.md", b"first words\n");
    repo.git_ok(["add", "--", "content/a.md"]);
    let restored = mark(
        &repo,
        &scope,
        CommitKind::Restore,
        "restore the opening",
        None,
        Some(&start),
    );
    assert_eq!(restored.kind, CommitKind::Restore);
    assert_eq!(selected(&repo, &restored.commit, &scope).commit, review);
}

#[test]
fn history_without_markers_has_no_baseline() {
    let repo = seeded_repo();
    let scope = scope_with_root(&repo, "content/");
    let head = support::text(&repo.git_ok(["rev-parse", "HEAD"]));

    // Ordinary history anchors nothing, and the seed commit is never
    // mistaken for a reviewed state.
    let error = select_baseline(&adapter_for(&repo), &head, &scope.digest)
        .expect_err("ordinary history has no baseline");
    assert_eq!(error.public_code(), "BASELINE_UNAVAILABLE");
    match &error {
        BaselineError::Unavailable(BaselineUnavailable::Exhausted { walked }) => {
            assert_eq!(*walked, 1);
        }
        other => panic!("the failure must report the exhausted walk: {other}"),
    }
}

#[test]
fn shallow_history_returns_baseline_unavailable() {
    let (source, scope, _start, _draft, review) = reviewed_repo();
    assert_eq!(
        selected(&source, &review, &scope).commit,
        review,
        "the source selects its own review before the clone"
    );

    // A shallow clone cuts the history the walk depends on, even though the
    // review commit itself arrives byte for byte.
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
    let head = support::text(&shallow.git_ok(["rev-parse", "HEAD"]));

    let error = select_baseline(&adapter_for(&shallow), &head, &scope.digest)
        .expect_err("shallow history cannot anchor a scope");
    assert_eq!(error.public_code(), "BASELINE_UNAVAILABLE");
    assert_eq!(
        error.to_string(),
        "the history context depends on shallow clone history"
    );
}

#[test]
fn grafted_history_returns_baseline_unavailable() {
    let (repo, scope, _start, _draft, review) = reviewed_repo();
    // A grafts file reshapes the ancestry view — here the review is
    // re-parented onto the seed — even though every object stays untouched,
    // so no marker in the repository can anchor anything.
    let seed = support::text(&repo.git_ok(["rev-parse", "HEAD~3"]));
    std::fs::write(
        repo.path().join(".git/info/grafts"),
        format!("{review} {seed}\n"),
    )
    .unwrap();
    let error = select_baseline(&adapter_for(&repo), &review, &scope.digest)
        .expect_err("grafted history cannot anchor a scope");
    assert_eq!(error.public_code(), "BASELINE_UNAVAILABLE");
    assert_eq!(
        error.to_string(),
        "the history context depends on grafted history"
    );
}

#[test]
fn sha256_repositories_select_their_anchor() {
    let repo = TestRepo::init_with(RepoOptions::new().with_object_format(ObjectFormat::Sha256));
    repo.write_file("other/sentinel.txt", b"untouched\n");
    repo.write_file("content/a.md", b"sha256 words\n");
    repo.commit_all("seed");
    let scope = scope_with_root(&repo, "content/");

    repo.write_file("content/a.md", b"sha256 words\nmore\n");
    repo.git_ok(["add", "--", "content/a.md"]);
    let start = mark(&repo, &scope, CommitKind::Start, "wide start", None, None);
    let review = mark(
        &repo,
        &scope,
        CommitKind::Review,
        "wide keep-all",
        Some(&start.commit),
        None,
    );

    let anchor = selected(&repo, &review.commit, &scope);
    assert_eq!(anchor.commit, review.commit);
    assert_eq!(anchor.kind, CommitKind::Review);
    assert_eq!(anchor.commit.len(), 64);
    assert_eq!(anchor.scope_digest, scope.digest.id());
}
