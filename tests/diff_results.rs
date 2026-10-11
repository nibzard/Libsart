//! Installed-binary and real-Git tests for bounded pinned diff results
//! (SPEC 6.3, 7.2, task T026).
//!
//! Every case proves one contract of the `diff` query against the installed
//! Git and the real executable:
//!
//! - the review identity, manifest digest, and totals cover the whole
//!   scope, whatever path filter narrowed the displayed detail
//! - detail pages obey the 50-group or 256-KiB default bound, whichever is
//!   reached first, expose omissions and cursors, and page through every
//!   group without losing or repeating one
//! - a cursor is tied to the review identity and the page contract: a
//!   changed comparison, contract, or hand edit is `STALE_CURSOR`
//! - one `--change` selection returns exactly that group; an unknown
//!   identity is `UNKNOWN_CHANGE`
//! - `--working` previews exactly the tree one checkpoint would create,
//!   moves no ref, index, or working file, and stays non-actionable
//! - `--working` still previews an edit that assume-unchanged hides from
//!   Git's own change reports, and the preview preserves the flag
//! - a scope with no candidate or no anchor reports the typed precondition
//!   failure, never a fabricated comparison

mod support;

use std::path::Path;
use std::process::{Command, Output, Stdio};

use margin::app::{DiffRequest, Operations, build_grouped_manifest, resolve_scope};
use margin::domain::ScopeInput;
use margin::git::Git;
use serde_json::Value;
use support::env::is_scrubbed;
use support::repo::TestRepo;
use support::text;

/// Runs the installed executable once with the isolated environment.
fn margin(env: &support::env::IsolatedEnv, cwd: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_margin"));
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, _) in std::env::vars_os() {
        if is_scrubbed(&key) {
            command.env_remove(&key);
        }
    }
    command.envs(env.vars());
    command
        .output()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"))
}

/// The exit class of one finished binary run.
fn exit_of(output: &Output) -> i32 {
    output.status.code().unwrap_or_else(|| {
        panic!("the binary died from a signal");
    })
}

/// Parses the one JSON envelope of a machine-mode run (SPEC 7.1).
fn envelope_of(output: &Output, what: &str) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let envelope: Value = serde_json::from_str(stdout.trim_end())
        .unwrap_or_else(|error| panic!("{what}: the envelope must parse: {error}: {stdout}"));
    assert_eq!(envelope["schema_version"], 1, "{what}");
    envelope
}

/// The `data` object of one successful run.
fn data_of(env: &support::env::IsolatedEnv, cwd: &Path, args: &[&str]) -> Value {
    let output = margin(env, cwd, args);
    let what = format!("{args:?}");
    assert_eq!(exit_of(&output), 0, "{what}: the query must succeed");
    let envelope = envelope_of(&output, &what);
    assert_eq!(envelope["ok"], true, "{what}: {envelope}");
    assert_eq!(envelope["effects"]["working_tree"], "unchanged", "{what}");
    assert_eq!(envelope["effects"]["index"], "unchanged", "{what}");
    assert_eq!(envelope["effects"]["commit"], "none", "{what}");
    envelope["data"].clone()
}

/// The typed error of one failed run.
fn error_of(env: &support::env::IsolatedEnv, cwd: &Path, args: &[&str]) -> (i32, String) {
    let output = margin(env, cwd, args);
    let what = format!("{args:?}");
    let envelope = envelope_of(&output, &what);
    let code = envelope["error"]["code"].as_str().unwrap_or("").to_string();
    (exit_of(&output), code)
}

/// Adapter bound to `repo`'s working tree and isolated environment.
fn adapter_for(repo: &TestRepo) -> Git {
    Git::from_env(repo.env().vars().clone()).at(repo.path())
}

/// The whole-scope identity an independent grouped-manifest build produces
/// for `baseline` to `candidate` on the adopted branch, plus its totals.
fn independent_identity(
    repo: &TestRepo,
    baseline: &str,
    candidate: &str,
) -> (String, String, usize, usize) {
    let git = adapter_for(repo);
    let scope = resolve_scope(
        &git,
        &ScopeInput {
            include: vec!["notes".to_string()],
            exclude: Vec::new(),
            suffixes: vec![".md".to_string()],
        },
    )
    .unwrap_or_else(|error| panic!("the scope must resolve: {error}"));
    let manifest = build_grouped_manifest(
        &git,
        "refs/heads/main",
        baseline,
        candidate,
        &scope.descriptor,
    )
    .unwrap_or_else(|error| panic!("the manifest must build: {error}"));
    let groups = manifest.groups().count();
    (
        manifest.review_id.id(),
        manifest.manifest_digest.id(),
        manifest.files.len(),
        groups,
    )
}

/// Initializes one workspace over `notes` and returns the start marker that
/// anchors the scope.
fn init_margin(repo: &TestRepo) -> String {
    let output = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes", "--json"],
    );
    assert_eq!(exit_of(&output), 0, "initialization must succeed");
    let envelope = envelope_of(&output, "init");
    assert_eq!(envelope["ok"], true, "init: {envelope}");
    text(&repo.git_ok(["rev-parse", "HEAD"]))
}

/// Writes one changed paragraph into a scoped file.
fn reparagraph(repo: &TestRepo, path: &str, words: &str) {
    repo.write_file(path, format!("# Title\n\n{words}\n\nClosing.\n").as_bytes());
}

/// Commits every change as one ordinary fixture commit and returns the new
/// `HEAD`.
fn commit(repo: &TestRepo, message: &str) -> String {
    repo.git_ok(["add", "--all"]);
    repo.git_ok(["commit", "--quiet", "--message", message]);
    text(&repo.git_ok(["rev-parse", "HEAD"]))
}

/// The change identities of one detail page, in page order.
fn ids_of(changes: &Value) -> Vec<String> {
    changes
        .as_array()
        .unwrap()
        .iter()
        .map(|change| change["change_id"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn whole_scope_identity_and_totals_survive_the_path_filter() {
    let repo = TestRepo::init();
    reparagraph(&repo, "notes/a.md", "Original a.");
    reparagraph(&repo, "notes/b.md", "Original b.");
    repo.write_file("other.txt", b"out-of-scope sentinel\n");
    let baseline = init_margin(&repo);

    // Three independent pending files: two paragraph edits and one addition.
    reparagraph(&repo, "notes/a.md", "Changed a.");
    reparagraph(&repo, "notes/b.md", "Changed b.");
    reparagraph(&repo, "notes/sub/c.md", "New c.");
    let candidate = commit(&repo, "writing pass");

    let (review_id, manifest_digest, files, groups) =
        independent_identity(&repo, &baseline, &candidate);

    // The filtered page hides detail only: every identity and total stays
    // whole-scope (SPEC 6.3).
    let filtered = data_of(
        repo.env(),
        repo.path(),
        &["diff", "--path", "notes/a.md", "--json"],
    );
    let shown: Vec<&str> = filtered["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|change| change["path"].as_str().unwrap())
        .collect();
    assert_eq!(shown, vec!["notes/a.md"], "only the filtered path shows");
    assert_eq!(filtered["scope_totals"]["files"], files as u64);
    assert_eq!(filtered["scope_totals"]["groups"], groups as u64);
    assert_eq!(filtered["review_id"], review_id);
    assert_eq!(filtered["manifest_digest"], manifest_digest);
    assert_eq!(filtered["baseline_oid"], baseline);
    assert_eq!(filtered["candidate_oid"], candidate);
    assert_eq!(filtered["omitted_detail_count"], (groups - 1) as u64);
    assert_eq!(filtered["complete"], true, "one filtered group fits");
    assert_eq!(filtered["next_cursor"], Value::Null);
    assert_eq!(filtered["working"], false);
    assert_eq!(filtered["actionable"], true);
    assert!(
        filtered["candidate_scope_digest"]
            .as_str()
            .unwrap()
            .starts_with("sha1:"),
        "the scoped digest is one full fingerprint"
    );

    // The unfiltered diff of the same comparison carries the same identity
    // and totals with every group.
    let whole = data_of(repo.env(), repo.path(), &["diff", "--json"]);
    assert_eq!(whole["review_id"], review_id);
    assert_eq!(whole["manifest_digest"], manifest_digest);
    assert_eq!(whole["scope_totals"]["groups"], groups as u64);
    assert_eq!(whole["changes"].as_array().unwrap().len(), groups);
    assert_eq!(whole["omitted_detail_count"], 0);
    assert_eq!(whole["complete"], true);

    // Stock Git agrees that exactly three scoped files changed.
    let changed: Vec<String> = String::from_utf8_lossy(&repo.git_ok([
        "diff-tree",
        "-r",
        "--name-only",
        "--no-commit-id",
        "-z",
        &baseline,
        &candidate,
    ]))
    .split('\0')
    .filter(|path| path.starts_with("notes/"))
    .map(str::to_string)
    .collect();
    assert_eq!(changed.len(), files, "the oracle counts the same files");
    assert_eq!(
        changed,
        vec![
            "notes/a.md".to_string(),
            "notes/b.md".to_string(),
            "notes/sub/c.md".to_string()
        ],
        "the paths match the manifest order"
    );
}

#[test]
fn detail_pages_obey_the_default_group_bound_and_continue() {
    let repo = TestRepo::init();
    for number in 0..60 {
        let name = format!("notes/f{number:02}.md");
        reparagraph(&repo, &name, &format!("Original {number}."));
    }
    let baseline = init_margin(&repo);
    for number in 0..60 {
        let name = format!("notes/f{number:02}.md");
        reparagraph(&repo, &name, &format!("Changed {number}."));
    }
    let candidate = commit(&repo, "sixty edits");
    let (_, _, _, groups) = independent_identity(&repo, &baseline, &candidate);
    assert!(groups >= 60, "each edited file is at least one group");

    // The default page carries at most 50 groups (SPEC 6.3).
    let first = data_of(repo.env(), repo.path(), &["diff", "--json"]);
    let first_ids = ids_of(&first["changes"]);
    assert_eq!(first_ids.len(), 50, "the default bound is 50 groups");
    assert_eq!(first["scope_totals"]["groups"], groups as u64);
    assert_eq!(first["complete"], false);
    assert_eq!(first["omitted_detail_count"], (groups - 50) as u64);
    let cursor = first["next_cursor"].as_str().unwrap().to_string();
    assert!(!cursor.is_empty(), "an incomplete page issues one cursor");

    // The cursor continues through the rest without repeating a group. The
    // omitted count stays whole-scope: it counts every group this response
    // does not carry, including the previous pages' groups.
    let second = data_of(
        repo.env(),
        repo.path(),
        &["diff", "--cursor", &cursor, "--json"],
    );
    let second_ids = ids_of(&second["changes"]);
    assert_eq!(second_ids.len(), groups - 50);
    assert_eq!(second["complete"], true);
    assert_eq!(second["next_cursor"], Value::Null);
    assert_eq!(second["omitted_detail_count"], 50);

    // Both pages together are exactly one unbounded walk of the manifest.
    let whole = data_of(
        repo.env(),
        repo.path(),
        &["diff", "--limit", "200", "--json"],
    );
    let mut joined = first_ids;
    joined.extend(second_ids);
    assert_eq!(joined, ids_of(&whole["changes"]));

    // An explicit limit tightens the page and the omission count follows.
    let tight = data_of(
        repo.env(),
        repo.path(),
        &["diff", "--limit", "10", "--json"],
    );
    assert_eq!(ids_of(&tight["changes"]).len(), 10);
    assert_eq!(tight["complete"], false);
    assert_eq!(tight["omitted_detail_count"], (groups - 10) as u64);
}

#[test]
fn the_byte_bound_stops_a_page_and_truncates_presentations() {
    let repo = TestRepo::init();
    let before_word = "x".repeat(160_000);
    let after_word = "y".repeat(160_000);
    for name in ["notes/one.md", "notes/two.md"] {
        repo.write_file(
            name,
            format!("# Title\n\n{before_word}\n\nClosing.\n").as_bytes(),
        );
    }
    let baseline = init_margin(&repo);
    for name in ["notes/one.md", "notes/two.md"] {
        repo.write_file(
            name,
            format!("# Title\n\n{after_word}\n\nClosing.\n").as_bytes(),
        );
    }
    let candidate = commit(&repo, "huge paragraphs");

    // One presentation already fills most of the 256-KiB page budget, so
    // the byte bound, not the 50-group bound, stops this page.
    let first = data_of(repo.env(), repo.path(), &["diff", "--json"]);
    let changes = first["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1, "the byte bound is reached first");
    assert_eq!(first["complete"], false);
    let cursor = first["next_cursor"].as_str().unwrap().to_string();

    // The detail content itself stays inside the bound, and the truncated
    // excerpts report what they omitted (SPEC 7.2).
    let serialized = serde_json::to_string(changes).unwrap();
    assert!(
        serialized.len() <= 256 * 1024,
        "the detail page stays inside 256 KiB, got {}",
        serialized.len()
    );
    for side in ["before", "after"] {
        let excerpt = &changes[0]["presentation"][side];
        assert_eq!(excerpt["truncated"], true, "{side} is truncated");
        assert_eq!(excerpt["total_bytes"], 160_001);
        let shown = excerpt["text"].as_str().unwrap();
        assert!(shown.len() <= 64 * 1024, "{side} is bounded");
    }

    // The second page still makes progress on the same comparison.
    let second = data_of(
        repo.env(),
        repo.path(),
        &["diff", "--cursor", &cursor, "--json"],
    );
    assert_eq!(second["changes"].as_array().unwrap().len(), 1);
    assert_eq!(second["complete"], true);

    // Both pages covered both huge groups without losing one.
    let (_, _, files, _) = independent_identity(&repo, &baseline, &candidate);
    assert_eq!(files, 2);
}

#[test]
fn one_groups_own_hints_and_ranges_never_break_the_page_bound() {
    let repo = TestRepo::init();
    // One file whose single passage pairs thousands of reliable word
    // hints: the detail of that one group alone would dwarf a page.
    let mut before = String::from("# Title\n\n");
    let mut after = String::from("# Title\n\n");
    for number in 0..4000 {
        before.push_str(&format!("Line shared {number} old-{number:06} tail.\n\n"));
        after.push_str(&format!("Line shared {number} new-{number:06} tail.\n\n"));
    }
    repo.write_file("notes/many.md", before.as_bytes());
    init_margin(&repo);
    repo.write_file("notes/many.md", after.as_bytes());
    commit(&repo, "thousands of paired edits");

    let page = data_of(repo.env(), repo.path(), &["diff", "--json"]);
    let changes = page["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1, "one passage group holds every pairing");
    let serialized = serde_json::to_string(changes).unwrap();
    assert!(
        serialized.len() <= 256 * 1024,
        "one group's detail stays bounded, got {}",
        serialized.len()
    );
    let presentation = &changes[0]["presentation"];
    assert!(
        presentation["word_hints_omitted"].as_u64().unwrap() > 0,
        "the omitted hint count is explicit, not silent"
    );
    assert!(
        presentation["word_hints"].as_array().unwrap().len() < 4000,
        "the carried hints fit the group budget"
    );
}

#[test]
fn cursors_reject_changed_context_and_edits() {
    let repo = TestRepo::init();
    for number in 0..60 {
        let name = format!("notes/f{number:02}.md");
        reparagraph(&repo, &name, &format!("Original {number}."));
    }
    init_margin(&repo);
    for number in 0..60 {
        let name = format!("notes/f{number:02}.md");
        reparagraph(&repo, &name, &format!("Changed {number}."));
    }
    commit(&repo, "sixty edits");

    let first = data_of(repo.env(), repo.path(), &["diff", "--json"]);
    let cursor = first["next_cursor"].as_str().unwrap().to_string();
    assert_eq!(first["complete"], false);

    // A new commit moves the candidate, so the pinned review changed.
    reparagraph(&repo, "notes/f00.md", "Changed again.");
    commit(&repo, "one more edit");
    let (exit, code) = error_of(
        repo.env(),
        repo.path(),
        &["diff", "--cursor", &cursor, "--json"],
    );
    assert_eq!((exit, code.as_str()), (3, "STALE_CURSOR"));

    // A fresh cursor of the new review still fails after a hand-edited
    // offset: the digest covers the position it was issued at.
    let fresh = data_of(repo.env(), repo.path(), &["diff", "--json"]);
    let fresh_cursor = fresh["next_cursor"].as_str().unwrap().to_string();
    let (offset, digest) = fresh_cursor.rsplit_once('-').unwrap();
    let tampered = format!("{}-{}", offset.parse::<u64>().unwrap() + 1, digest);
    let (exit, code) = error_of(
        repo.env(),
        repo.path(),
        &["diff", "--cursor", &tampered, "--json"],
    );
    assert_eq!((exit, code.as_str()), (3, "STALE_CURSOR"));

    // Malformed cursors never panic or page.
    for bad in ["", "not-a-cursor", "50"] {
        let (exit, code) = error_of(
            repo.env(),
            repo.path(),
            &["diff", "--cursor", bad, "--json"],
        );
        assert_eq!((exit, code.as_str()), (3, "STALE_CURSOR"), "{bad:?}");
    }

    // The page contract is part of the cursor: another filter or limit is a
    // different contract (SPEC 6.3).
    let (exit, code) = error_of(
        repo.env(),
        repo.path(),
        &["diff", "--cursor", &cursor, "--limit", "60", "--json"],
    );
    assert_eq!((exit, code.as_str()), (3, "STALE_CURSOR"));
    let (exit, code) = error_of(
        repo.env(),
        repo.path(),
        &[
            "diff",
            "--cursor",
            &cursor,
            "--path",
            "notes/f01.md",
            "--json",
        ],
    );
    assert_eq!((exit, code.as_str()), (3, "STALE_CURSOR"));
}

#[test]
fn one_change_selection_and_unknown_changes() {
    let repo = TestRepo::init();
    reparagraph(&repo, "notes/a.md", "Original a.");
    reparagraph(&repo, "notes/b.md", "Original b.");
    let baseline = init_margin(&repo);
    reparagraph(&repo, "notes/a.md", "Changed a.");
    reparagraph(&repo, "notes/b.md", "Changed b.");
    let candidate = commit(&repo, "writing pass");
    let (review_id, _, _, groups) = independent_identity(&repo, &baseline, &candidate);

    let whole = data_of(repo.env(), repo.path(), &["diff", "--json"]);
    let wanted = ids_of(&whole["changes"])[1].clone();

    // One named group is the whole page; every other group is omitted.
    let selected = data_of(
        repo.env(),
        repo.path(),
        &["diff", "--change", &wanted, "--json"],
    );
    let changes = selected["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["change_id"], wanted);
    assert_eq!(selected["review_id"], review_id);
    assert_eq!(selected["complete"], true);
    assert_eq!(selected["omitted_detail_count"], (groups - 1) as u64);

    // An unknown identity is one typed invalid input, and the selection
    // conflicts with the page controls of the same command.
    let (exit, code) = error_of(
        repo.env(),
        repo.path(),
        &[
            "diff",
            "--change",
            "sha1:0000000000000000000000000000000000000000",
            "--json",
        ],
    );
    assert_eq!((exit, code.as_str()), (2, "UNKNOWN_CHANGE"));
    let (exit, code) = error_of(
        repo.env(),
        repo.path(),
        &[
            "diff",
            "--change",
            &wanted,
            "--path",
            "notes/a.md",
            "--json",
        ],
    );
    assert_eq!((exit, code.as_str()), (2, "INVALID_ARGUMENT"));
}

#[test]
fn working_previews_are_non_actionable_and_write_nothing() {
    let repo = TestRepo::init();
    reparagraph(&repo, "notes/a.md", "Original a.");
    reparagraph(&repo, "notes/b.md", "Original b.");
    repo.write_file("other.txt", b"out-of-scope sentinel\n");
    let baseline = init_margin(&repo);

    // Uncaptured working content: the committed comparison is empty while
    // the preview carries it (SPEC 4.2, 6.2).
    reparagraph(&repo, "notes/a.md", "Working a.");
    let before = support::oracle::snapshot(&repo);

    let committed = data_of(repo.env(), repo.path(), &["diff", "--json"]);
    assert_eq!(committed["changes"].as_array().unwrap().len(), 0);
    assert_eq!(committed["actionable"], true);

    let preview = data_of(repo.env(), repo.path(), &["diff", "--working", "--json"]);
    assert_eq!(preview["working"], true);
    assert_eq!(preview["actionable"], false, "a preview never approves");
    let changes = preview["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1, "only the working change shows");
    assert_eq!(changes[0]["path"], "notes/a.md");
    assert_eq!(changes[0]["kind"], "modified");
    assert_eq!(preview["baseline_oid"], baseline);
    assert_eq!(preview["complete"], true);
    // The preview repeats the same change identity when nothing moves.
    let preview_again = data_of(repo.env(), repo.path(), &["diff", "--working", "--json"]);
    assert_eq!(preview_again["review_id"], preview["review_id"]);

    // The preview candidate is one tree object holding exactly the working
    // content one checkpoint would stage (stock Git oracle).
    let candidate = preview["candidate_oid"].as_str().unwrap();
    assert_eq!(
        text(&repo.git_ok(["cat-file", "-t", candidate])),
        "tree",
        "the preview candidate is the preview tree"
    );
    let working_oid = text(&repo.git_ok(["hash-object", "notes/a.md"]));
    let record = String::from_utf8_lossy(&repo.git_ok([
        "ls-tree",
        "-r",
        "-z",
        candidate,
        "--",
        "notes/a.md",
    ]))
    .to_string();
    assert!(
        record
            .split('\t')
            .next()
            .is_some_and(|meta| meta.contains(&working_oid)),
        "the preview tree stores the working bytes: {record:?}"
    );

    // The query left the repository exactly as it was.
    before.assert_unchanged(&support::oracle::snapshot(&repo), "diff --working");
}

#[test]
fn working_previews_see_content_hidden_by_index_flags() {
    // The preview of SPEC 6.2 `--working` must carry an edit that
    // assume-unchanged hides from every Git change report, because the
    // preview tree is what one checkpoint would stage (T061, SPEC 4.2).
    let repo = TestRepo::init();
    reparagraph(&repo, "notes/a.md", "Original a.");
    let baseline = init_margin(&repo);
    reparagraph(&repo, "notes/a.md", "Working a.");
    repo.git_ok(["update-index", "--assume-unchanged", "--", "notes/a.md"]);
    assert_eq!(
        text(&repo.git_ok(["status", "--porcelain"])),
        "",
        "stock Git reports no change behind the flag"
    );
    let before = support::oracle::snapshot(&repo);

    let preview = data_of(repo.env(), repo.path(), &["diff", "--working", "--json"]);
    assert_eq!(preview["working"], true);
    assert_eq!(preview["actionable"], false, "a preview never approves");
    let changes = preview["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1, "the hidden working change shows");
    assert_eq!(changes[0]["path"], "notes/a.md");
    assert_eq!(changes[0]["kind"], "modified");
    assert_eq!(preview["baseline_oid"], baseline);

    // The preview tree stores the exact working bytes, proved by stock Git
    // itself.
    let candidate = preview["candidate_oid"].as_str().unwrap();
    let working_oid = text(&repo.git_ok(["hash-object", "notes/a.md"]));
    let record = String::from_utf8_lossy(&repo.git_ok([
        "ls-tree",
        "-r",
        "-z",
        candidate,
        "--",
        "notes/a.md",
    ]))
    .to_string();
    assert!(
        record
            .split('\t')
            .next()
            .is_some_and(|meta| meta.contains(&working_oid)),
        "the preview tree stores the hidden working bytes: {record:?}"
    );

    // The preview observes only: the flag and the edit survive it.
    before.assert_unchanged(&support::oracle::snapshot(&repo), "a flagged preview");
    assert_eq!(
        text(&repo.git_ok(["ls-files", "-v", "--", "notes/a.md"])),
        "h notes/a.md",
        "the preview preserves the user's flag"
    );
}

#[test]
fn diff_needs_a_scope_and_an_anchor() {
    // No stored scope: the typed uninitialized state, never a comparison.
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# A\n");
    repo.commit_all("import");
    let (exit, code) = error_of(repo.env(), repo.path(), &["diff", "--json"]);
    assert_eq!((exit, code.as_str()), (4, "NOT_INITIALIZED"));

    // A stored scope whose branch is unborn has no candidate at all.
    init_margin(&repo);
    repo.git_ok(["switch", "--orphan", "fresh"]);
    let (exit, code) = error_of(repo.env(), repo.path(), &["diff", "--json"]);
    assert_eq!((exit, code.as_str()), (4, "BASELINE_UNAVAILABLE"));

    // The page bounds are validated at the interface (SPEC 6.3: 1-200).
    repo.git_ok(["switch", "main"]);
    for limit in ["0", "201"] {
        let output = margin(
            repo.env(),
            repo.path(),
            &["diff", "--limit", limit, "--json"],
        );
        assert_eq!(exit_of(&output), 2, "limit {limit} is invalid");
        let envelope = envelope_of(&output, "an invalid limit");
        assert_eq!(envelope["error"]["code"], "INVALID_ARGUMENT");
    }
}

#[test]
fn the_operation_layer_serves_the_same_pages() {
    // The CLI only serializes; the shared operation of SPEC 13.2 builds the
    // identical page, so the TUI and hooks can call it directly.
    let repo = TestRepo::init();
    reparagraph(&repo, "notes/a.md", "Original a.");
    reparagraph(&repo, "notes/b.md", "Original b.");
    let baseline = init_margin(&repo);
    reparagraph(&repo, "notes/a.md", "Changed a.");
    reparagraph(&repo, "notes/b.md", "Changed b.");
    let candidate = commit(&repo, "writing pass");
    let (review_id, manifest_digest, files, groups) =
        independent_identity(&repo, &baseline, &candidate);

    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let outcome = operations.build_review(&DiffRequest {
        scope: None,
        path_filter: vec!["notes".to_string()],
        change: None,
        cursor: None,
        limit: Some(1),
        working: false,
    });
    assert!(outcome.ok(), "the operation must succeed");
    let data = outcome.data.as_ref().unwrap();
    assert_eq!(data.review_id, review_id);
    assert_eq!(data.manifest_digest, manifest_digest);
    assert_eq!(data.scope_totals.files, files);
    assert_eq!(data.scope_totals.groups, groups);
    assert_eq!(
        data.changes.len(),
        1,
        "the limit applies to the whole scope"
    );
    assert_eq!(data.changes[0].path, "notes/a.md", "manifest order holds");
    assert_eq!(data.omitted_detail_count, groups - 1);
    assert!(!data.complete, "one group of two remains");
    assert!(data.next_cursor.is_some());

    // Continuing through the operation layer reaches every group.
    let second = operations.build_review(&DiffRequest {
        scope: None,
        path_filter: vec!["notes".to_string()],
        change: None,
        cursor: data.next_cursor.clone(),
        limit: Some(1),
        working: false,
    });
    assert!(second.ok(), "the second page must succeed");
    let rest = second.data.as_ref().unwrap();
    assert!(rest.complete);
    assert_eq!(rest.changes[0].path, "notes/b.md");
    assert_ne!(
        rest.changes[0].change_id, data.changes[0].change_id,
        "the pages differ"
    );
    assert_eq!(outcome.effects, margin::app::Effects::unchanged());
}
