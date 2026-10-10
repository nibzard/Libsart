//! Real-Git tests for the ancestry-based editorial timeline (SPEC 5.5 and
//! 6.3, task T022).
//!
//! Every case proves one contract of `log` against the installed Git:
//!
//! - the timeline labels starts, drafts, reviews, and restorations from
//!   validated markers, in first-parent ancestry order — never clock order
//!   — and the page commits are the commits `git rev-list --first-parent`
//!   itself lists
//! - consecutive draft checkpoints collapse into one writing pass that
//!   counts them, while `--all-checkpoints` and `show --revision` keep
//!   every collapsed checkpoint reachable
//! - pages carry cursors bound to the walk and page contract; a moved
//!   head, an edited offset, another checkpoint policy, another filter, or
//!   another limit is one `STALE_CURSOR` refusal
//! - a short displayed log never hides an old baseline, and the empty
//!   review commit a path-limited `git log` drops stays in the timeline,
//!   because the walk itself never uses path filtering
//! - the `--path` presentation filter hides entries only; a filtered page
//!   still paginates and the next unfiltered query lists everything
//! - a forged or rule-breaking marker keeps its claimed kind with the
//!   reason it was ignored, and no baseline moves because of it
//! - the query leaves refs, index, and working bytes unchanged, works in
//!   both object formats, and an unadopted workspace reports
//!   `NOT_INITIALIZED`

mod support;

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use support::env::is_scrubbed;
use support::repo::TestRepo;

/// Runs the installed executable once with the isolated environment.
fn margin(env: &support::env::IsolatedEnv, cwd: &Path, args: &[&str]) -> Output {
    margin_stdin(env, cwd, args, b"")
}

/// Runs the installed executable once with `input` on standard input.
fn margin_stdin(
    env: &support::env::IsolatedEnv,
    cwd: &Path,
    args: &[&str],
    input: &[u8],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_margin"));
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, _) in std::env::vars_os() {
        if is_scrubbed(&key) {
            command.env_remove(&key);
        }
    }
    command.envs(env.vars());
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"));
    use std::io::Write as _;
    child
        .stdin
        .as_mut()
        .expect("stdin is piped")
        .write_all(input)
        .expect("the plan document reaches standard input");
    child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"))
}

/// Runs Git once with the isolated environment plus extra variables, so a
/// fixture can control the committer clock.
fn git_with(
    env: &support::env::IsolatedEnv,
    cwd: &Path,
    args: &[&str],
    extra: &[(&str, &str)],
) -> Output {
    let mut command = Command::new("git");
    command.args(args).current_dir(cwd);
    for (key, _) in std::env::vars_os() {
        if is_scrubbed(&key) {
            command.env_remove(&key);
        }
    }
    command.envs(env.vars());
    for (key, value) in extra {
        command.env(key, value);
    }
    command
        .output()
        .unwrap_or_else(|error| panic!("cannot run git: {error}"))
}

/// The exit class of one finished binary run.
fn exit_of(output: &Output) -> i32 {
    output.status.code().unwrap_or_else(|| {
        panic!("the binary died from a signal");
    })
}

/// Parses the one JSON envelope of a successful machine-mode run and checks
/// the envelope contract itself (SPEC 7.1).
fn data_of(output: &Output, what: &str) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.matches('\n').count(),
        1,
        "{what}: exactly one envelope line, got: {:?}",
        stdout.chars().take(400).collect::<String>()
    );
    let envelope: Value = serde_json::from_str(stdout.trim_end())
        .unwrap_or_else(|error| panic!("{what}: the envelope must parse: {error}"));
    assert_eq!(envelope["ok"], true, "{what}: the query succeeds");
    assert_eq!(envelope["error"], Value::Null, "{what}: no error");
    assert_eq!(
        envelope["effects"]["working_tree"], "unchanged",
        "{what}: a query writes no file"
    );
    assert_eq!(
        envelope["effects"]["index"], "unchanged",
        "{what}: a query writes no index entry"
    );
    assert_eq!(
        envelope["effects"]["commit"], "none",
        "{what}: a query creates no commit"
    );
    assert!(
        output.stderr.is_empty(),
        "{what}: machine mode writes no stderr"
    );
    envelope["data"].clone()
}

/// One failed envelope, parsed.
fn error_of(output: &Output, what: &str) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let envelope: Value = serde_json::from_str(stdout.trim_end())
        .unwrap_or_else(|error| panic!("{what}: the envelope must parse: {error}: {stdout}"));
    assert_eq!(envelope["ok"], false, "{what}: the query fails");
    envelope
}

/// The error code of one failed envelope.
fn code_of(envelope: &Value) -> &str {
    envelope["error"]["code"].as_str().unwrap()
}

/// Parses the one JSON envelope of one successful fixture mutation. The
/// effects belong to the mutation's own suite; the timeline tests only need
/// the created commits.
fn mutation_data_of(output: &Output, what: &str) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let envelope: Value = serde_json::from_str(stdout.trim_end())
        .unwrap_or_else(|error| panic!("{what}: the envelope must parse: {error}: {stdout}"));
    assert_eq!(envelope["ok"], true, "{what}: the mutation succeeds");
    envelope["data"].clone()
}

// ---------------------------------------------------------------------------
// The shared fixture
// ---------------------------------------------------------------------------

/// One whole writing session built through the installed executable: a
/// start marker, a writing pass of two checkpoints, an empty keep-all
/// review, and one historical restoration.
struct Session {
    repo: TestRepo,
    /// The start marker.
    start: String,
    /// The checkpoint commits, oldest first.
    checkpoints: Vec<String>,
    /// The empty review receipt.
    review: String,
    /// The restoration receipt.
    restore: String,
}

/// Initializes the `notes` scope through the executable.
fn init(repo: &TestRepo) -> String {
    let output = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes", "--json"],
    );
    assert_eq!(exit_of(&output), 0, "initialization succeeds");
    let data = mutation_data_of(&output, "initialization");
    data["start_commit"]
        .as_str()
        .expect("a start commit")
        .to_string()
}

/// Captures one checkpoint through the executable and returns its commit.
fn checkpoint(repo: &TestRepo, message: &str, path: &str, body: &[u8]) -> String {
    repo.write_file(path, body);
    let output = margin(
        repo.env(),
        repo.path(),
        &["checkpoint", "--message", message, "--json"],
    );
    assert_eq!(exit_of(&output), 0, "the checkpoint {message:?} succeeds");
    let data = mutation_data_of(&output, "a checkpoint");
    data["commit"]
        .as_str()
        .expect("a created commit")
        .to_string()
}

/// One ordinary external commit through stock Git, outside the scope.
fn ordinary(repo: &TestRepo, body: &[u8]) -> String {
    repo.write_file("note.txt", body);
    repo.git_ok(["add", "--", "note.txt"]);
    repo.commit_all("external edit")
}

/// Applies one complete keep-all review through the executable.
fn keep_all_review(repo: &TestRepo, request: &str) -> String {
    let output = margin(repo.env(), repo.path(), &["diff", "--json"]);
    assert_eq!(exit_of(&output), 0, "the diff query succeeds");
    let diff = data_of(&output, "the pinned review");
    let plan = json!({
        "schema_version": 1,
        "request_id": request,
        "review_id": diff["review_id"],
        "branch_ref": diff["branch_ref"],
        "scope_id": diff["scope_id"],
        "baseline_oid": diff["baseline_oid"],
        "candidate_oid": diff["candidate_oid"],
        "restore": [],
        "keep_remaining": true,
    });
    let output = margin_stdin(
        repo.env(),
        repo.path(),
        &["review", "--input", "-", "--json"],
        plan.to_string().as_bytes(),
    );
    assert_eq!(exit_of(&output), 0, "the keep-all review succeeds");
    let data = mutation_data_of(&output, "a decided review");
    data["commit"]
        .as_str()
        .expect("a receipt commit")
        .to_string()
}

/// Restores one document through the executable.
fn restore(repo: &TestRepo, request: &str, path: &str, revision: &str) -> String {
    let output = margin(repo.env(), repo.path(), &["status", "--json"]);
    assert_eq!(exit_of(&output), 0);
    let status = data_of(&output, "the workspace status");
    let plan = json!({
        "schema_version": 1,
        "request_id": request,
        "branch_ref": status["branch"],
        "scope_id": status["scope"]["digest"],
        "candidate_oid": status["head"],
        "path": path,
        "revision": revision,
    });
    let output = margin_stdin(
        repo.env(),
        repo.path(),
        &["restore", "--input", "-", "--json"],
        plan.to_string().as_bytes(),
    );
    assert_eq!(exit_of(&output), 0, "the restoration succeeds");
    let data = mutation_data_of(&output, "a restoration");
    data["commit"]
        .as_str()
        .expect("a receipt commit")
        .to_string()
}

/// Builds the shared session.
fn session() -> Session {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"original passage\n");
    repo.write_file("notes/b.md", b"b original\n");
    let start = init(&repo);
    let first = checkpoint(
        &repo,
        "Introduction revised",
        "notes/a.md",
        b"revised passage\n",
    );
    let second = checkpoint(&repo, "Tone revised", "notes/b.md", b"b revised\n");
    let review = keep_all_review(&repo, "req-t022-review-1");
    let restore_commit = restore(&repo, "req-t022-restore-1", "notes/a.md", &start);
    Session {
        repo,
        start,
        checkpoints: vec![first, second],
        review,
        restore: restore_commit,
    }
}

/// The commits of one timeline page.
fn page_commits(data: &Value) -> Vec<String> {
    data["entries"]
        .as_array()
        .expect("one entry array")
        .iter()
        .map(|entry| entry["commit"].as_str().expect("a commit").to_string())
        .collect()
}

/// The labels of one timeline page.
fn page_kinds(data: &Value) -> Vec<&str> {
    data["entries"]
        .as_array()
        .expect("one entry array")
        .iter()
        .map(|entry| entry["kind"].as_str().expect("a kind"))
        .collect()
}

/// The unfiltered first-parent ancestry Git itself lists, newest first.
fn ancestry(repo: &TestRepo) -> Vec<String> {
    String::from_utf8_lossy(&repo.git_ok(["rev-list", "--first-parent", "HEAD"]))
        .lines()
        .map(str::to_string)
        .collect()
}

/// One `git log --format` value per commit of a revision list.
fn log_values(repo: &TestRepo, format: &str, revisions: &[String]) -> Vec<String> {
    revisions
        .iter()
        .map(|commit| {
            support::text(&repo.git_ok(["log", "-n", "1", &format!("--format={format}"), commit]))
        })
        .collect()
}

/// Runs one `log` query through the executable and returns its data.
fn log_data(repo: &TestRepo, args: &[&str]) -> Value {
    let mut all = vec!["log", "--json"];
    all.extend_from_slice(args);
    let output = margin(repo.env(), repo.path(), &all);
    assert_eq!(exit_of(&output), 0, "the timeline query succeeds");
    data_of(&output, "the timeline")
}

// ---------------------------------------------------------------------------
// Labels and order
// ---------------------------------------------------------------------------

#[test]
fn the_timeline_labels_validated_markers_in_ancestry_order() {
    let session = session();
    let repo = &session.repo;

    let data = log_data(repo, &[]);
    assert_eq!(data["head"], ancestry(repo)[0], "the walk starts at HEAD");
    assert_eq!(data["all_checkpoints"], false);
    assert_eq!(data["complete"], true, "the whole timeline fits one page");
    assert_eq!(data["next_cursor"], Value::Null);

    // One entry per event: the newest first, the two consecutive drafts
    // collapsed into one writing pass carried by its newest checkpoint.
    assert_eq!(page_commits(&data).len(), 4);
    assert_eq!(
        page_commits(&data),
        [
            session.restore.clone(),
            session.review.clone(),
            session.checkpoints[1].clone(),
            session.start.clone()
        ],
        "ancestry order, newest first, drafts collapsed"
    );
    assert_eq!(page_kinds(&data), ["restore", "review", "draft", "start"]);
    let checkpoints: Vec<u64> = data["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["checkpoints"].as_u64().unwrap())
        .collect();
    assert_eq!(checkpoints, [1, 1, 2, 1], "the pass counts its checkpoints");

    // The reserved facts of the validated markers.
    let scope_id = data["scope_id"].as_str().unwrap().to_string();
    for entry in data["entries"].as_array().unwrap() {
        assert_eq!(entry["marker"]["scope"], scope_id, "one scope throughout");
        assert_eq!(entry["ignored_reason"], Value::Null);
    }
    let review = &data["entries"][1];
    assert_eq!(review["marker"]["baseline"], session.start.clone());
    let restore = &data["entries"][0];
    assert_eq!(restore["marker"]["source"]["revision"], session.start);
    assert_eq!(restore["marker"]["source"]["path"], "notes/a.md");
    // The collapsed pass carries its newest checkpoint's subject.
    assert_eq!(data["entries"][2]["subject"], "Tone revised");

    // Every timestamp is the exact value Git records, and the fixed fixture
    // clock cannot have ordered anything: order is ancestry alone.
    let times: Vec<i64> = data["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["committer_time"].as_i64().unwrap())
        .collect();
    assert_eq!(times.len(), 4);
    assert_eq!(
        times,
        log_values(repo, "%ct", &page_commits(&data))
            .iter()
            .map(|value| value.parse::<i64>().unwrap())
            .collect::<Vec<i64>>()
    );
}

#[test]
fn all_checkpoints_lists_every_collapsed_draft_again() {
    let session = session();
    let repo = &session.repo;

    // The default page collapsed the two checkpoints into one pass.
    let collapsed = log_data(repo, &[]);
    assert_eq!(collapsed["entries"].as_array().unwrap().len(), 4);

    // The checkpoint policy lists each draft again, in ancestry order.
    let expanded = log_data(repo, &["--all-checkpoints"]);
    assert_eq!(expanded["all_checkpoints"], true);
    assert_eq!(
        page_commits(&expanded),
        ancestry(repo),
        "every first-parent commit, including each checkpoint"
    );
    assert_eq!(
        page_kinds(&expanded),
        ["restore", "review", "draft", "draft", "start"]
    );
    for entry in expanded["entries"].as_array().unwrap() {
        assert_eq!(entry["checkpoints"], 1, "no checkpoint collapses");
    }

    // A collapsed checkpoint stays reachable through Margin itself: the
    // document reads at exactly the revision the collapse folded away.
    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/a.md",
            "--revision",
            &session.checkpoints[0],
            "--json",
        ],
    );
    assert_eq!(exit_of(&output), 0, "the folded checkpoint still resolves");
    let data = data_of(&output, "a collapsed checkpoint");
    assert_eq!(data["content"]["text"], "revised passage\n");
}

#[test]
fn pages_carry_bound_cursors_and_refuse_every_changed_context() {
    // One extra ordinary commit gives the timeline five entries: restore,
    // review, ordinary, pass, start.
    let session = session();
    let repo = &session.repo;
    ordinary(repo, b"an external note\n");

    let whole = log_data(repo, &["--limit", "200"]);
    assert_eq!(
        page_kinds(&whole),
        ["ordinary", "restore", "review", "draft", "start"]
    );

    // Page by two: the cursors thread the same entry sequence.
    let mut collected: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    for expected in [2usize, 2, 1] {
        let mut args: Vec<OsString> = vec!["--limit".into(), "2".into()];
        if let Some(cursor) = &cursor {
            args.push("--cursor".into());
            args.push(cursor.into());
        }
        let args: Vec<String> = args
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let page = log_data(repo, &refs);
        assert_eq!(
            page["entries"].as_array().unwrap().len(),
            expected,
            "the page holds {expected} entries"
        );
        collected.extend(page_commits(&page));
        let complete = page["complete"].as_bool().unwrap();
        assert_eq!(complete, expected != 2, "only the last page completes");
        cursor = page["next_cursor"].as_str().map(str::to_string);
        assert_eq!(cursor.is_none(), complete);
    }
    assert_eq!(
        collected,
        page_commits(&whole),
        "the pages tile the timeline"
    );

    // The identical request issues the identical cursor.
    let first = log_data(repo, &["--limit", "2"]);
    let repeat = log_data(repo, &["--limit", "2"]);
    assert_eq!(repeat["next_cursor"], first["next_cursor"]);

    // A cursor from the all-checkpoints policy does not page the collapsed
    // view, and neither does another limit.
    let expanded = log_data(repo, &["--all-checkpoints", "--limit", "2"]);
    let foreign = expanded["next_cursor"].as_str().unwrap().to_string();
    for (args, what) in [
        (vec!["--cursor", &foreign], "another checkpoint policy"),
        (
            vec!["--cursor", &foreign, "--all-checkpoints", "--limit", "3"],
            "another limit",
        ),
    ] {
        let output = margin(repo.env(), repo.path(), &{
            let mut all = vec!["log", "--json"];
            all.extend_from_slice(&args);
            all
        });
        assert_eq!(exit_of(&output), 3, "{what} is stale");
        assert_eq!(code_of(&error_of(&output, what)), "STALE_CURSOR");
    }

    // An edited offset demands a digest its editor cannot compute.
    let page = log_data(repo, &["--limit", "2"]);
    let issued = page["next_cursor"].as_str().unwrap().to_string();
    let (offset, digest) = issued.rsplit_once('-').unwrap();
    let edited = format!("{}-{digest}", offset.parse::<u64>().unwrap() + 1);
    let output = margin(
        repo.env(),
        repo.path(),
        &["log", "--json", "--limit", "2", "--cursor", &edited],
    );
    assert_eq!(exit_of(&output), 3, "an edited offset is stale");
    assert_eq!(
        code_of(&error_of(&output, "an edited cursor")),
        "STALE_CURSOR"
    );
    for malformed in ["0", "not-a-cursor", "1-short", "x-0-sha1:abc"] {
        let output = margin(
            repo.env(),
            repo.path(),
            &["log", "--json", "--limit", "2", "--cursor", malformed],
        );
        assert_eq!(exit_of(&output), 3, "{malformed:?} is malformed");
        assert_eq!(
            code_of(&error_of(&output, "a malformed cursor")),
            "STALE_CURSOR"
        );
    }

    // A new checkpoint moves the head, so every earlier cursor is stale.
    checkpoint(repo, "A newer pass", "notes/a.md", b"newer passage\n");
    let output = margin(
        repo.env(),
        repo.path(),
        &["log", "--json", "--limit", "2", "--cursor", &issued],
    );
    assert_eq!(exit_of(&output), 3, "a moved head is stale");
    assert_eq!(code_of(&error_of(&output, "a moved head")), "STALE_CURSOR");
}

#[test]
fn a_cursor_beyond_a_filtered_timeline_names_no_held_position() {
    let session = session();
    let repo = &session.repo;

    // The unfiltered timeline holds four entries; a filter that matches
    // only the start holds one. A real cursor into the fourth unfiltered
    // position cannot page the filtered walk.
    let unfiltered = log_data(repo, &["--limit", "3"]);
    let cursor = unfiltered["next_cursor"].as_str().unwrap().to_string();
    let filtered = log_data(repo, &["--path", "notes/zzz.md", "--limit", "3"]);
    assert_eq!(
        filtered["entries"].as_array().unwrap().len(),
        0,
        "no commit ever touched the filtered path"
    );
    assert_eq!(filtered["complete"], true);
    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "log",
            "--json",
            "--path",
            "notes/zzz.md",
            "--limit",
            "3",
            "--cursor",
            &cursor,
        ],
    );
    assert_eq!(exit_of(&output), 3, "the filter changes the page contract");

    // A position beyond the filtered timeline keeps its own digest only
    // when it was issued there, so the refusal is a stale one either way.
    let one_entry = log_data(repo, &["--path", "notes/b.md", "--limit", "1"]);
    let held = one_entry["next_cursor"].as_str().unwrap().to_string();
    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "log",
            "--json",
            "--path",
            "notes/b.md",
            "--limit",
            "1",
            "--cursor",
            &held,
        ],
    );
    assert_eq!(exit_of(&output), 0, "the issued position is held");
    let page = data_of(&output, "the last filtered page");
    assert_eq!(page_commits(&page), vec![session.start.clone()]);
    assert_eq!(page["complete"], true);
}

#[test]
fn a_short_page_keeps_baseline_discovery_whole() {
    let session = session();
    let repo = &session.repo;

    // One short page shows the newest entry only.
    let short = log_data(repo, &["--limit", "1"]);
    assert_eq!(page_commits(&short), vec![ancestry(repo)[0].clone()]);

    // The review marker the page never displayed still anchors the
    // workspace, because marker discovery is separate from presentation
    // (SPEC 6.3): a completed review advances the baseline to itself.
    let output = margin(repo.env(), repo.path(), &["status", "--json"]);
    assert_eq!(exit_of(&output), 0);
    let status = data_of(&output, "the workspace status");
    assert_eq!(status["baseline"]["state"], "anchored");
    assert_eq!(status["baseline"]["commit"], session.review.clone());
    assert_eq!(status["baseline"]["kind"], "review");

    let output = margin(repo.env(), repo.path(), &["diff", "--json"]);
    assert_eq!(exit_of(&output), 0, "the comparison still builds");
    let diff = data_of(&output, "the pinned review");
    assert_eq!(diff["baseline_oid"], session.review.clone());

    // The empty review commit changes no path, so a path-limited `git log`
    // hides it — the exact trap SPEC 8.5 names — while the Margin walk,
    // which takes no pathspec, keeps it.
    let path_history = String::from_utf8_lossy(&repo.git_ok([
        "log",
        "--first-parent",
        "--format=%H",
        "--",
        "notes/a.md",
    ]))
    .lines()
    .map(str::to_string)
    .collect::<Vec<String>>();
    assert!(
        !path_history.contains(&session.review),
        "a path-limited git log hides the empty review commit"
    );
    let data = log_data(repo, &[]);
    assert!(
        page_commits(&data).contains(&session.review),
        "the Margin timeline keeps the empty review commit visible"
    );
}

#[test]
fn the_path_filter_hides_entries_without_narrowing_the_walk() {
    let session = session();
    let repo = &session.repo;
    ordinary(repo, b"an external note\n");

    // Only the writing pass — through its second checkpoint — and the
    // start, which created notes/b.md, changed notes/b.md. The restoration
    // changed notes/a.md, the review changed nothing, and the external
    // commit stayed outside the scope.
    let filtered = log_data(repo, &["--path", "notes/b.md"]);
    assert_eq!(page_commits(&filtered).len(), 2);
    assert_eq!(page_kinds(&filtered), ["draft", "start"]);
    let pass = &filtered["entries"][0];
    assert_eq!(
        pass["checkpoints"], 2,
        "a pass survives the filter when any checkpoint matched"
    );

    // The filter never narrows the walk: the next unfiltered query still
    // lists every entry.
    let unfiltered = log_data(repo, &[]);
    assert_eq!(
        page_kinds(&unfiltered),
        ["ordinary", "restore", "review", "draft", "start"]
    );

    // A filtered page paginates within its own contract.
    let first = log_data(repo, &["--path", "notes/b.md", "--limit", "1"]);
    assert_eq!(page_kinds(&first), ["draft"]);
    assert_eq!(first["complete"], false);
    let cursor = first["next_cursor"].as_str().unwrap().to_string();
    let second = log_data(
        repo,
        &["--path", "notes/b.md", "--limit", "1", "--cursor", &cursor],
    );
    assert_eq!(page_commits(&second), vec![session.start.clone()]);
    assert_eq!(second["complete"], true);
}

/// Stages one forged body file outside the scope and returns the parent
/// commit and the staged tree the forgery will actually commit, so its
/// recorded structural claims can be made self-consistent.
fn stage_forged_body(repo: &TestRepo, body: &str) -> (String, String) {
    repo.write_file(body, b"a forged marker body\n");
    repo.git_ok(["add", "--", body]);
    (
        support::text(&repo.git_ok(["rev-parse", "HEAD"])),
        support::text(&repo.git_ok(["write-tree"])),
    )
}

/// Commits one prepared marker message verbatim through stock Git, with the
/// message file kept inside the Git directory so the working tree stays as
/// the fixture built it.
fn commit_with_message(repo: &TestRepo, message: &str) {
    let git_dir = support::text(&repo.git_ok(["rev-parse", "--absolute-git-dir"]));
    let file = Path::new(&git_dir).join("forged-message.txt");
    std::fs::write(&file, message.as_bytes())
        .unwrap_or_else(|error| panic!("cannot write the forged message: {error}"));
    repo.git_ok(["commit", "--quiet", "--file", &file.to_string_lossy()]);
}

#[test]
fn forged_markers_keep_their_claimed_kind_with_a_reason() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"original passage\n");
    let start = init(&repo);
    let draft = checkpoint(
        &repo,
        "Introduction revised",
        "notes/a.md",
        b"revised passage\n",
    );

    // The reserved trailer block of one real marker, to copy its scope
    // identity into forgeries.
    let start_only = [start.clone()];
    let real = log_values(&repo, "%B", &start_only).join("\n");
    let trailer = |line: &str| real.lines().any(|candidate| candidate.starts_with(line));
    assert!(trailer("Margin-Scope: "));
    assert!(trailer("Margin-Scope-Spec: "));
    let scope_digest = real
        .lines()
        .find(|line| line.starts_with("Margin-Scope: "))
        .unwrap()
        .strip_prefix("Margin-Scope: ")
        .unwrap()
        .to_string();
    let scope_spec = real
        .lines()
        .find(|line| line.starts_with("Margin-Scope-Spec: "))
        .unwrap()
        .strip_prefix("Margin-Scope-Spec: ")
        .unwrap()
        .to_string();
    // One forgery with a scope digest the descriptor does not hash to, and
    // one structurally perfect review that records another baseline than
    // the one selection reproduces at its candidate (SPEC 8.4 rule 6).
    // Both stage their body first, so every structural claim they record is
    // self-consistent and only the intended rule breaks.
    let (parent, tree) = stage_forged_body(&repo, "forged-digest.txt");
    let mut digest_forgery = String::from("a forged review\n\n");
    digest_forgery.push_str("Margin-Version: 1\n");
    digest_forgery.push_str("Margin-Kind: review\n");
    digest_forgery.push_str("Margin-Scope: sha1:0000000000000000000000000000000000000000\n");
    digest_forgery.push_str(&format!("Margin-Scope-Spec: {scope_spec}\n"));
    digest_forgery.push_str(&format!("Margin-Candidate: {parent}\n"));
    digest_forgery.push_str(&format!("Margin-Result-Tree: {tree}\n"));
    digest_forgery.push_str(&format!("Margin-Baseline: {start}\n"));
    digest_forgery.push_str("Margin-Review: complete\n");
    digest_forgery.push_str("Margin-Request: req-forged-digest\n");
    digest_forgery.push_str("Margin-Plan: sha1:1111111111111111111111111111111111111111\n");
    commit_with_message(&repo, &digest_forgery);

    let (baseline_parent, baseline_tree) = stage_forged_body(&repo, "forged-baseline.txt");
    let mut baseline_forgery = String::from("another forged review\n\n");
    baseline_forgery.push_str("Margin-Version: 1\n");
    baseline_forgery.push_str("Margin-Kind: review\n");
    baseline_forgery.push_str(&format!("Margin-Scope: {scope_digest}\n"));
    baseline_forgery.push_str(&format!("Margin-Scope-Spec: {scope_spec}\n"));
    baseline_forgery.push_str(&format!("Margin-Candidate: {baseline_parent}\n"));
    baseline_forgery.push_str(&format!("Margin-Result-Tree: {baseline_tree}\n"));
    baseline_forgery.push_str(&format!("Margin-Baseline: {draft}\n"));
    baseline_forgery.push_str("Margin-Review: complete\n");
    baseline_forgery.push_str("Margin-Request: req-forged-baseline\n");
    baseline_forgery.push_str("Margin-Plan: sha1:2222222222222222222222222222222222222222\n");
    commit_with_message(&repo, &baseline_forgery);

    let data = log_data(&repo, &[]);
    assert_eq!(
        page_kinds(&data),
        ["review", "review", "draft", "start"],
        "a claimed kind stays labeled while it is ignored"
    );
    let reason = |index: usize| {
        data["entries"][index]["ignored_reason"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    assert_eq!(data["entries"][0]["marker"], Value::Null);
    assert!(
        reason(0).contains("baseline"),
        "the rule 6 forgery names its rule, got: {}",
        reason(0)
    );
    assert_eq!(data["entries"][1]["marker"], Value::Null);
    assert!(
        reason(1).contains("scope digest"),
        "the digest forgery names its rule, got: {}",
        reason(1)
    );
    // The validated markers carry their facts and no reason.
    assert_eq!(data["entries"][2]["marker"]["scope"], scope_digest);
    assert_eq!(data["entries"][2]["ignored_reason"], Value::Null);
    assert_eq!(data["entries"][3]["commit"], start);

    // Neither forgery conferred anything: the baseline walk passed both
    // and still anchors the workspace at the start marker.
    let output = margin(repo.env(), repo.path(), &["status", "--json"]);
    assert_eq!(exit_of(&output), 0);
    let status = data_of(&output, "the workspace status");
    assert_eq!(status["baseline"]["state"], "anchored");
    assert_eq!(status["baseline"]["commit"], start);
}

#[test]
fn order_is_ancestry_even_against_the_committer_clock() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"original passage\n");
    let start = init(&repo);

    // Three ordinary external commits whose committer clocks deliberately
    // disagree with their ancestry: the newest is neither the oldest nor
    // the newest timestamp.
    let mut expected = Vec::new();
    for (body, seconds) in [
        (&b"first external\n"[..], 2_000_000_000i64),
        (b"second external\n", 1_000_000_000),
        (b"third external\n", 1_500_000_000),
    ] {
        repo.write_file("note.txt", body);
        repo.git_ok(["add", "--", "note.txt"]);
        let date = format!("@{seconds} +0000");
        let output = git_with(
            repo.env(),
            repo.path(),
            &["commit", "--quiet", "--message", "external edit"],
            &[
                ("GIT_AUTHOR_DATE", date.as_str()),
                ("GIT_COMMITTER_DATE", date.as_str()),
            ],
        );
        assert!(
            output.status.success(),
            "the fixture commit succeeds: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        expected.push((support::text(&repo.git_ok(["rev-parse", "HEAD"])), seconds));
    }

    let data = log_data(&repo, &[]);
    let commits: Vec<String> = expected.iter().map(|(commit, _)| commit.clone()).collect();
    let mut ordered = commits.clone();
    ordered.reverse();
    ordered.push(start);
    assert_eq!(page_commits(&data), ordered, "ancestry order, newest first");

    let times: Vec<i64> = data["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["committer_time"].as_i64().unwrap())
        .collect();
    assert_eq!(
        times,
        [1_500_000_000, 1_000_000_000, 2_000_000_000, times[3]],
        "the clock disagrees with the order, and the timeline keeps ancestry"
    );
    assert_eq!(
        times,
        log_values(&repo, "%ct", &page_commits(&data))
            .iter()
            .map(|value| value.parse::<i64>().unwrap())
            .collect::<Vec<i64>>(),
        "every timestamp is exactly what Git records"
    );
}

// ---------------------------------------------------------------------------
// Interface behavior
// ---------------------------------------------------------------------------

#[test]
fn the_text_view_lists_one_line_per_entry() {
    let session = session();
    let repo = &session.repo;
    let head = ancestry(&session.repo)[0].clone();

    let output = margin(repo.env(), repo.path(), &["log"]);
    assert_eq!(exit_of(&output), 0);
    let body = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(
        lines[0],
        format!("margin: timeline head={head} entries=4 complete=true"),
        "one summary line first"
    );
    assert_eq!(lines.len(), 5, "one line per entry");
    assert!(lines[1].starts_with("restore "), "got: {}", lines[1]);
    assert!(lines[2].starts_with("review "), "got: {}", lines[2]);
    assert!(
        lines[3].starts_with("draft ") && lines[3].ends_with("(+1 collapsed)"),
        "the pass states its collapse: {}",
        lines[3]
    );
    assert!(lines[3].contains("Tone revised"), "got: {}", lines[3]);
    assert!(lines[4].starts_with("start "), "got: {}", lines[4]);

    // Every checkpoint policy lists each draft without a collapse note.
    let output = margin(repo.env(), repo.path(), &["log", "--all-checkpoints"]);
    assert_eq!(exit_of(&output), 0);
    let body = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 6);
    assert_eq!(
        lines[3],
        format!("draft {} Tone revised", &session.checkpoints[1][..12])
    );
    assert_eq!(
        lines[4],
        format!(
            "draft {} Introduction revised",
            &session.checkpoints[0][..12]
        )
    );
}

#[test]
fn long_subjects_bound_their_entries() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"original passage\n");
    init(&repo);
    let long = "word ".repeat(80);
    checkpoint(&repo, &long, "notes/a.md", b"revised passage\n");

    let data = log_data(&repo, &[]);
    let entry = &data["entries"][0];
    assert_eq!(entry["kind"], "draft");
    assert_eq!(entry["subject_truncated"], true, "the subject is bounded");
    let subject = entry["subject"].as_str().unwrap();
    assert!(subject.chars().count() <= 200, "the bound holds");
    assert!(subject.ends_with('\u{2026}'), "the cut is stated");
    assert!(subject.starts_with("word word"), "the beginning survives");
}

#[test]
fn the_limit_bound_is_typed_invalid_input() {
    let session = session();
    let repo = &session.repo;
    for limit in ["0", "201", "nope"] {
        let output = margin(
            repo.env(),
            repo.path(),
            &["log", "--json", "--limit", limit],
        );
        assert_eq!(exit_of(&output), 2, "{limit:?} is outside 1 to 200");
        let envelope = error_of(&output, "an out-of-bound limit");
        assert_eq!(code_of(&envelope), "INVALID_ARGUMENT");
    }
    // The bounds themselves page fine.
    for limit in ["1", "200"] {
        let output = margin(
            repo.env(),
            repo.path(),
            &["log", "--json", "--limit", limit],
        );
        assert_eq!(exit_of(&output), 0, "{limit} is accepted");
    }
}

#[test]
fn an_unadopted_workspace_and_both_formats_behave_typed() {
    // No stored scope: the timeline reports the unadopted state.
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"before adoption\n");
    repo.commit_all("import");
    let output = margin(repo.env(), repo.path(), &["log", "--json"]);
    assert_eq!(exit_of(&output), 4);
    assert_eq!(
        code_of(&error_of(&output, "an unadopted workspace")),
        "NOT_INITIALIZED"
    );

    // The sha256 object format labels the same validated history.
    let repo = support::repo::TestRepo::init_with(
        support::repo::RepoOptions::new().with_object_format(support::repo::ObjectFormat::Sha256),
    );
    repo.write_file("notes/a.md", b"original passage\n");
    let start = init(&repo);
    let draft = checkpoint(
        &repo,
        "Introduction revised",
        "notes/a.md",
        b"revised passage\n",
    );
    let data = log_data(&repo, &[]);
    assert_eq!(data["object_format"], "sha256");
    assert_eq!(page_commits(&data), vec![draft.clone(), start]);
    assert_eq!(page_kinds(&data), ["draft", "start"]);
    assert_eq!(data["head"].as_str().unwrap().len(), 64);
}

#[test]
fn every_timeline_query_leaves_the_repository_unchanged() {
    let session = session();
    let repo = &session.repo;
    // Working bytes newer than every commit stay untouched by a query.
    repo.write_file("notes/a.md", b"uncaptured working bytes\n");
    let before = support::oracle::snapshot(repo);

    let page = log_data(repo, &["--limit", "2"]);
    let cursor = page["next_cursor"].as_str().unwrap().to_string();
    for args in [
        vec!["log", "--json"],
        vec!["log", "--json", "--all-checkpoints"],
        vec!["log", "--json", "--limit", "1"],
        vec!["log", "--json", "--path", "notes/b.md"],
        vec!["log"],
        vec!["log", "--json", "--limit", "2", "--cursor", &cursor],
    ] {
        let output = margin(repo.env(), repo.path(), &args);
        assert_eq!(exit_of(&output), 0, "{args:?}");
    }
    before.assert_unchanged(&support::oracle::snapshot(repo), "the timeline queries");
}
