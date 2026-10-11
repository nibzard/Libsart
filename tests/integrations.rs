//! Agent integration tests (SPEC 12, 16.2, task T048).
//!
//! Every case runs the built binary against a real Git repository and
//! checks the outcome with stock Git, so the claims are about processes,
//! not intentions: `hook claude` reads one bounded upstream payload,
//! captures one ordinary draft only while the stored policy enables it,
//! answers only in the upstream hook protocol, never emits a response
//! that continues the agent after a Stop, and never approves anything on
//! any path. A missing, malformed, or interrupted event captures and
//! approves nothing.

#![cfg(unix)]

mod support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use serde_json::Value;
use support::pty::Pty;
use support::repo::TestRepo;

/// How long any one wait in these tests may take.
const LIMIT: Duration = Duration::from_secs(20);

/// The largest payload the adapter accepts; oversized probes use it.
const MAX_PAYLOAD_BYTES: usize = 128 * 1024;

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Builds one command of the built binary with the scrubbed environment.
fn margin_command(
    env: &support::env::IsolatedEnv,
    cwd: &Path,
    args: &[&str],
) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_margin"));
    command
        .args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (key, _) in std::env::vars_os() {
        if support::env::is_scrubbed(&key) {
            command.env_remove(&key);
        }
    }
    command.envs(env.vars());
    command
}

/// Runs the built binary with no standard input.
fn margin(env: &support::env::IsolatedEnv, cwd: &Path, args: &[&str]) -> Output {
    let mut command = margin_command(env, cwd, args);
    command.stdin(std::process::Stdio::null());
    command
        .output()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"))
}

/// Pipes one hook payload into `margin` with the given arguments.
fn hook_args(env: &support::env::IsolatedEnv, cwd: &Path, args: &[&str], payload: &[u8]) -> Output {
    let mut command = margin_command(env, cwd, args);
    command.stdin(std::process::Stdio::piped());
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"));
    {
        let mut stdin = child.stdin.take().expect("piped standard input");
        stdin
            .write_all(payload)
            .expect("the payload fits one pipe write");
    }
    child.wait_with_output().expect("the hook command finishes")
}

/// Pipes one hook payload into `margin hook claude` in `cwd`.
fn hook(env: &support::env::IsolatedEnv, cwd: &Path, payload: &[u8]) -> Output {
    hook_args(env, cwd, &["hook", "claude"], payload)
}

/// Reads one hook fixture and points its workspace at `workspace`.
/// `@@WORKSPACE@@` stands for the directory the event must name.
fn payload(fixture: &str, workspace: &Path) -> Vec<u8> {
    let path = project_root().join("tests/fixtures/hooks").join(fixture);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read the fixture {fixture}: {error}"));
    text.replace("@@WORKSPACE@@", &workspace.display().to_string())
        .into_bytes()
}

/// One adopted repository whose automatic-checkpoint policy is `auto`.
fn adopted_repo(auto: bool) -> TestRepo {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n\nFirst passage.\n");
    repo.commit_all("import");
    let mut args: Vec<&str> = vec!["init", "--scope", "notes/"];
    if auto {
        args.push("--auto-checkpoint");
    }
    let output = margin(repo.env(), repo.path(), &args);
    assert_eq!(
        output.status.code(),
        Some(0),
        "init must succeed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    repo
}

/// Leaves one eligible edit in the adopted scope. Each pass writes new
/// bytes, so a later boundary has something left to capture.
fn edit_scope(repo: &TestRepo, pass: usize) {
    repo.write_file(
        "notes/a.md",
        format!("# One\n\nFirst passage.\n\nPass {pass} of the writing.\n").as_bytes(),
    );
}

/// Stages the eligible edit, so the shared checkpoint guard refuses the
/// boundary capture with `STAGED_CHANGES`.
fn stage_scope_edit(repo: &TestRepo) {
    repo.git_ok(["add", "notes/a.md"]);
}

/// The commits whose marker names one review, in Git's own words.
fn review_markers(repo: &TestRepo) -> Vec<String> {
    String::from_utf8_lossy(&repo.git_ok(["log", "--format=%H", "--grep=Margin-Kind: review"]))
        .lines()
        .map(str::to_string)
        .collect()
}

/// Decodes one JSON response the adapter printed.
fn response_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("the response is one JSON object: {error}"))
}

#[test]
fn stop_captures_the_completed_pass_while_the_agent_stops() {
    let repo = adopted_repo(true);
    edit_scope(&repo, 1);
    let head_before = support::oracle::head_oid(&repo);
    let reviews_before = review_markers(&repo);

    let output = hook(repo.env(), repo.path(), &payload("stop.json", repo.path()));
    assert_eq!(output.status.code(), Some(0), "the hook succeeds");
    assert!(
        output.stdout.is_empty(),
        "the response is minimal: no output lets the agent stop (no decision, no \
         continuation context): {}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(
        output.stderr.is_empty(),
        "a successful capture is silent: {}",
        String::from_utf8_lossy(&output.stderr),
    );

    let head = support::oracle::head_oid(&repo).expect("the capture made one commit");
    assert_ne!(Some(head), head_before, "the policy enabled one capture");
    let message = String::from_utf8_lossy(&repo.git_ok(["log", "-1", "--format=%B"])).into_owned();
    assert!(
        message.contains("Margin-Kind: draft"),
        "the boundary capture is one ordinary unreviewed draft: {message}",
    );
    assert!(
        message.contains("Completed writing pass"),
        "the commit reason is the generic boundary reason, never a prompt or message body: \
         {message}",
    );
    let dirty = repo.git(["status", "--porcelain", "--", "notes/"]);
    assert!(
        dirty.status.success() && dirty.stdout.is_empty(),
        "the capture left the scope clean: {}",
        String::from_utf8_lossy(&dirty.stdout),
    );
    assert_eq!(
        review_markers(&repo),
        reviews_before,
        "a boundary capture approves nothing",
    );
}

#[test]
fn an_immediate_duplicate_stop_is_one_harmless_no_op() {
    let repo = adopted_repo(true);
    edit_scope(&repo, 1);
    let first = hook(repo.env(), repo.path(), &payload("stop.json", repo.path()));
    assert_eq!(first.status.code(), Some(0));
    let head_after_first = support::oracle::head_oid(&repo);

    // The delayed or repeated duplicate boundary carries identical
    // content, so the unchanged checkpoint is an explicit no-op (SPEC
    // 12.3).
    let second = hook(repo.env(), repo.path(), &payload("stop.json", repo.path()));
    assert_eq!(second.status.code(), Some(0), "a duplicate succeeds");
    assert!(second.stdout.is_empty(), "a duplicate answers nothing");
    assert_eq!(
        support::oracle::head_oid(&repo),
        head_after_first,
        "the duplicate created no second commit",
    );
}

#[test]
fn a_continued_stop_never_blocks_the_agent() {
    let repo = adopted_repo(true);
    edit_scope(&repo, 1);
    let head_before = support::oracle::head_oid(&repo);

    // `stop_hook_active` is true when upstream already continued the agent
    // from a stop hook. Margin never emits a blocking decision, so this
    // Stop still captures and still lets the agent stop; no loop can form.
    let output = hook(
        repo.env(),
        repo.path(),
        &payload("stop-continued.json", repo.path()),
    );
    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stdout.is_empty(),
        "no decision, no exit 2, no continuation context: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert_ne!(
        support::oracle::head_oid(&repo),
        head_before,
        "the continued Stop still captured the pass",
    );
}

#[test]
fn a_refused_stop_capture_reports_without_continuing() {
    let repo = adopted_repo(true);
    edit_scope(&repo, 1);
    stage_scope_edit(&repo);
    let head_before = support::oracle::head_oid(&repo);
    let staged_before = repo.git_ok(["status", "--porcelain"]);
    let reviews_before = review_markers(&repo);

    // The staged change is the concurrent-writer indication: the shared
    // guard refuses the capture (SPEC 12.2).
    let output = hook(repo.env(), repo.path(), &payload("stop.json", repo.path()));
    assert_eq!(output.status.code(), Some(0), "the agent still stops");
    let response = response_json(&output);
    let object = response.as_object().expect("the response is one object");
    assert_eq!(
        object.len(),
        1,
        "the response carries exactly one field: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(
        object.contains_key("systemMessage"),
        "the report is one non-blocking systemMessage: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    let message = object["systemMessage"].as_str().unwrap_or_default();
    assert!(
        message.contains("Margin could not save the writing pass"),
        "the report names the failure: {message}",
    );
    assert!(message.contains("STAGED_CHANGES"), "typed: {message}");
    assert!(
        message.contains("Nothing was approved"),
        "the report states what it did not do: {message}",
    );
    assert_eq!(
        support::oracle::head_oid(&repo),
        head_before,
        "the refused capture committed nothing",
    );
    assert_eq!(
        repo.git_ok(["status", "--porcelain"]),
        staged_before,
        "the refused capture changed nothing",
    );
    assert_eq!(
        review_markers(&repo),
        reviews_before,
        "nothing was approved"
    );
}

#[test]
fn prompt_submit_captures_outstanding_content_and_allows_the_prompt() {
    let repo = adopted_repo(true);
    edit_scope(&repo, 1);
    let head_before = support::oracle::head_oid(&repo);
    let reviews_before = review_markers(&repo);

    let output = hook(
        repo.env(),
        repo.path(),
        &payload("user-prompt-submit.json", repo.path()),
    );
    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stdout.is_empty(),
        "a successful pre-start capture blocks nothing: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    let head = support::oracle::head_oid(&repo).expect("the capture made one commit");
    assert_ne!(Some(head), head_before);
    let message = String::from_utf8_lossy(&repo.git_ok(["log", "-1", "--format=%B"])).into_owned();
    assert!(
        message.contains("Captured before the next prompt"),
        "the generic boundary reason names the boundary: {message}",
    );
    // The prompt itself is fixture data; it must never reach the commit
    // title (SPEC 12.3).
    assert!(
        !message.contains("Tighten the introduction"),
        "no prompt content reached the commit: {message}",
    );
    assert_eq!(review_markers(&repo), reviews_before);
}

#[test]
fn a_refused_prompt_capture_blocks_the_prompt_with_the_supported_response() {
    let repo = adopted_repo(true);
    edit_scope(&repo, 1);
    stage_scope_edit(&repo);
    let head_before = support::oracle::head_oid(&repo);
    let staged_before = repo.git_ok(["status", "--porcelain"]);

    // Preservation is required and cannot be provided, so the adapter uses
    // the one supported pre-start blocking response (SPEC 12.2) rather
    // than claim preservation it did not provide.
    let output = hook(
        repo.env(),
        repo.path(),
        &payload("user-prompt-submit.json", repo.path()),
    );
    assert_eq!(output.status.code(), Some(0), "JSON output applies");
    let response = response_json(&output);
    let object = response.as_object().expect("the response is one object");
    assert_eq!(
        object.len(),
        2,
        "the block carries decision and reason only: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert_eq!(object["decision"], "block");
    let reason = object["reason"].as_str().unwrap_or_default();
    assert!(reason.contains("STAGED_CHANGES"), "typed: {reason}");
    assert!(
        reason.contains("Nothing was approved"),
        "the block approves nothing: {reason}",
    );
    assert_eq!(
        support::oracle::head_oid(&repo),
        head_before,
        "the blocked prompt committed nothing",
    );
    assert_eq!(
        repo.git_ok(["status", "--porcelain"]),
        staged_before,
        "the blocked prompt changed nothing",
    );
}

#[test]
fn boundaries_are_silent_when_the_policy_is_off() {
    let repo = adopted_repo(false);
    edit_scope(&repo, 1);
    let head_before = support::oracle::head_oid(&repo);

    // First use defaults to off, so every boundary row answers "manual
    // preservation": silent, no capture, no claim (SPEC 12.2).
    for fixture in ["stop.json", "user-prompt-submit.json", "session-end.json"] {
        let output = hook(repo.env(), repo.path(), &payload(fixture, repo.path()));
        assert_eq!(output.status.code(), Some(0), "{fixture} succeeds");
        assert!(output.stdout.is_empty(), "{fixture} prints nothing");
        assert!(output.stderr.is_empty(), "{fixture} is silent");
    }
    assert_eq!(
        support::oracle::head_oid(&repo),
        head_before,
        "a disabled policy captures nothing",
    );
}

#[test]
fn outside_any_repository_the_adapter_is_silent() {
    let repo = TestRepo::init();
    // The private root of the test harness holds no repository of its own.
    let outside = repo.root().to_path_buf();

    let output = hook(repo.env(), &outside, &payload("stop.json", &outside));
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty(), "no output");
    assert!(output.stderr.is_empty(), "no claim of preservation");
}

#[test]
fn an_uninitialized_repository_keeps_preservation_manual() {
    let repo = TestRepo::init();
    edit_scope(&repo, 1);
    let head_before = support::oracle::head_oid(&repo);

    // A plain Git repository Margin never adopted keeps first-use
    // defaults: both capture policies off, so the adapter is silent.
    let output = hook(repo.env(), repo.path(), &payload("stop.json", repo.path()));
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(
        support::oracle::head_oid(&repo),
        head_before,
        "no adoption happened implicitly",
    );
}

#[test]
fn session_boundaries_capture_best_effort() {
    let repo = adopted_repo(true);
    let head_before = support::oracle::head_oid(&repo);

    // Each session boundary is one best-effort ordinary checkpoint: the
    // capture happens, but no output reaches the agent, because these
    // events discard or ignore JSON output (SPEC 12.2).
    let mut previous = head_before;
    for (pass, (fixture, label)) in [
        ("session-start.json", "startup"),
        ("stop-failure.json", "failure"),
        ("session-end.json", "end"),
    ]
    .into_iter()
    .enumerate()
    {
        edit_scope(&repo, pass + 1);
        let output = hook(repo.env(), repo.path(), &payload(fixture, repo.path()));
        assert_eq!(output.status.code(), Some(0), "{label} succeeds");
        assert!(
            output.stdout.is_empty(),
            "{label} prints no protocol output"
        );
        let head = support::oracle::head_oid(&repo).expect("the boundary captured");
        assert_ne!(previous, Some(head.clone()), "{label} captured the pass");
        previous = Some(head);
        let message =
            String::from_utf8_lossy(&repo.git_ok(["log", "-1", "--format=%B"])).into_owned();
        assert!(
            message.contains("Captured at a session boundary"),
            "{label} uses the generic session reason: {message}",
        );
    }
}

#[test]
fn a_failed_session_boundary_is_best_effort_only() {
    let repo = adopted_repo(true);
    edit_scope(&repo, 1);
    stage_scope_edit(&repo);
    let head_before = support::oracle::head_oid(&repo);

    // The capture fails, but the boundary stays best effort: success exit,
    // no protocol output, one stderr diagnostic, and the next status or
    // startup inspection stays authoritative (SPEC 12.2).
    let output = hook(
        repo.env(),
        repo.path(),
        &payload("session-end.json", repo.path()),
    );
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty(), "no output reaches the agent");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("margin hook: best-effort capture failed"),
        "one bounded diagnostic for the debug log: {stderr}",
    );
    assert_eq!(
        support::oracle::head_oid(&repo),
        head_before,
        "the failed capture committed nothing",
    );
}

/// One payload that must be refused before any capture: every row keeps
/// the repository exactly as it was, because a missing or interrupted
/// event never implies capture or approval (SPEC 12.3).
#[test]
fn missing_or_unsupported_events_never_capture_or_approve() {
    let repo = adopted_repo(true);
    edit_scope(&repo, 1);
    let head_before = support::oracle::head_oid(&repo);
    let reviews_before = review_markers(&repo);

    let here = repo.path().display().to_string();
    let oversized = {
        let mut bytes = br#"{"hook_event_name":"Stop","cwd":"","pad":""#.to_vec();
        bytes.resize(MAX_PAYLOAD_BYTES + 1, b'a');
        bytes
    };
    let cases: &[(&str, Vec<u8>)] = &[
        ("empty", Vec::new()),
        (
            "truncated",
            format!(r#"{{"hook_event_name":"Stop","cwd":"{here}""#).into_bytes(),
        ),
        ("not an object", b"[1, 2, 3]".to_vec()),
        (
            "no event name",
            format!(r#"{{"cwd":"{here}"}}"#).into_bytes(),
        ),
        (
            "unsupported event",
            format!(r#"{{"hook_event_name":"PreToolUse","cwd":"{here}"}}"#).into_bytes(),
        ),
        (
            "event name of the wrong type",
            format!(r#"{{"hook_event_name":7,"cwd":"{here}"}}"#).into_bytes(),
        ),
        ("no workspace", br#"{"hook_event_name":"Stop"}"#.to_vec()),
        (
            "stop_hook_active of the wrong type",
            format!(r#"{{"hook_event_name":"Stop","cwd":"{here}","stop_hook_active":"yes"}}"#)
                .into_bytes(),
        ),
        ("oversized", oversized),
    ];
    for (label, bytes) in cases {
        let output = hook(repo.env(), repo.path(), bytes);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{label}: one non-blocking refusal, never the blocking exit 2",
        );
        assert!(
            output.stdout.is_empty(),
            "{label}: no protocol output claims anything",
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.starts_with("margin hook: "),
            "{label}: one bounded stderr line: {stderr}",
        );
        assert!(
            stderr.lines().count() == 1 && stderr.ends_with('\n'),
            "{label}: exactly one line: {stderr}",
        );
        assert_eq!(
            support::oracle::head_oid(&repo),
            head_before,
            "{label}: nothing was captured",
        );
        assert_eq!(
            review_markers(&repo),
            reviews_before,
            "{label}: nothing was approved"
        );
    }
}

#[test]
fn a_workspace_mismatch_refuses_without_touching_the_named_repository() {
    let repo = adopted_repo(true);
    edit_scope(&repo, 1);
    let head_before = support::oracle::head_oid(&repo);

    // The payload names the harness root, not the served repository; the
    // adapter must not run its capture from another workspace's event.
    let output = hook(repo.env(), repo.path(), &payload("stop.json", repo.root()));
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("nothing was captured or approved"),
        "the refusal states its bounds: {stderr}",
    );
    assert_eq!(
        support::oracle::head_oid(&repo),
        head_before,
        "the served repository was not touched",
    );
}

#[test]
fn the_json_flag_never_prints_an_envelope_into_the_protocol() {
    let repo = adopted_repo(true);

    // `--json` belongs to the Margin envelope, which the hook channel must
    // never carry (SPEC 6.2), so the flag is a typed refusal on stderr.
    let output = hook_args(
        repo.env(),
        repo.path(),
        &["hook", "claude", "--json"],
        &payload("stop.json", repo.path()),
    );
    assert_eq!(output.status.code(), Some(2), "invalid input");
    assert!(
        output.stdout.is_empty(),
        "no envelope entered the protocol channel",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("INVALID_ARGUMENT"), "{stderr}");
    assert!(stderr.contains("--json"), "{stderr}");
}

#[test]
fn a_manual_run_without_piped_input_refuses() {
    let repo = adopted_repo(true);
    let head_before = support::oracle::head_oid(&repo);

    // A terminal standard input carries no event; the refusal is immediate
    // and the manual run is told what upstream expects.
    let envs: Vec<(String, String)> = repo
        .env()
        .vars()
        .iter()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect();
    let mut pty = Pty::spawn(
        Path::new(env!("CARGO_BIN_EXE_margin")),
        &["hook", "claude"],
        &envs,
        repo.path(),
        110,
        32,
    )
    .expect("the command spawns on a pseudo-terminal");
    assert!(
        pty.wait_for("no hook payload was provided on standard input", LIMIT),
        "the refusal explains the missing event: {}",
        String::from_utf8_lossy(pty.raw_output()),
    );
    let status = pty.wait_exit(LIMIT).expect("the command exits");
    assert_eq!(status.code(), Some(1));
    assert_eq!(
        support::oracle::head_oid(&repo),
        head_before,
        "the missing event captured nothing",
    );
}
