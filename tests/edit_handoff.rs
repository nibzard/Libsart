//! Command-line editor handoff tests (SPEC 5.6, 12.5, task T047).
//!
//! Every case runs the built binary against a real Git repository, so the
//! claims are about processes, not intentions: `edit` passes one explicit
//! configured argv with the literal document path to a stand-in editor,
//! reports the editor's exit and the refreshed state in words, captures
//! one draft only while the stored policy enables it, and approves
//! nothing on any path. The handoff cases run on a pseudo-terminal,
//! because the handoff itself requires one; the typed refusals of the
//! interface are proven through pipes.

#![cfg(unix)]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use support::pty::Pty;
use support::repo::TestRepo;

/// How long any one wait in these tests may take.
const LIMIT: Duration = Duration::from_secs(20);

/// Runs the built binary in `cwd` with the isolated environment.
fn margin(env: &support::env::IsolatedEnv, cwd: &Path, args: &[&str]) -> Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_margin"));
    command
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (key, _) in std::env::vars_os() {
        if support::env::is_scrubbed(&key) {
            command.env_remove(&key);
        }
    }
    command.envs(env.vars());
    command
        .output()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"))
}

/// The child environment of one spawned command, plus any extra pairs the
/// case needs.
fn envs(repo: &TestRepo, extra: &[(&str, PathBuf)]) -> Vec<(String, String)> {
    let mut vars: Vec<(String, String)> = repo
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
    for (key, value) in extra {
        vars.push((key.to_string(), value.display().to_string()));
    }
    vars
}

/// Writes one stand-in editor script into `repo` and returns its path.
/// The script records the exact argv it received in `$MARGIN_EDIT_ARGV`,
/// appends one line to the document, and exits `status`. It lives at the
/// repository root, outside the adopted `notes/` scope, like the stand-in
/// editor of the terminal tests.
fn stand_in_editor(repo: &TestRepo, status: i32) -> PathBuf {
    let editor = repo.path().join("stand-in-editor.sh");
    std::fs::write(
        &editor,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$MARGIN_EDIT_ARGV\"\nprintf 'Edited in the \
             stand-in editor.\\n' >> \"$1\"\nexit {status}\n"
        )
        .as_bytes(),
    )
    .expect("the stand-in editor");
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755))
        .expect("the stand-in editor is executable");
    editor
}

/// One adopted repository whose configured editor is the working stand-in
/// that exits `status`.
fn adopted_repo(editor_status: i32, extra_flags: &[&str]) -> TestRepo {
    let repo = TestRepo::init();
    let editor = stand_in_editor(&repo, editor_status);
    repo.write_file("notes/a.md", b"# One\n\nFirst passage.\n");
    repo.commit_all("import");
    let editor_json = format!("[\"{}\"]", editor.display());
    let mut args: Vec<&str> = vec!["init", "--scope", "notes/"];
    args.extend_from_slice(extra_flags);
    args.extend_from_slice(&["--editor-argv-json", &editor_json]);
    let output = margin(repo.env(), repo.path(), &args);
    assert_eq!(
        output.status.code(),
        Some(0),
        "init must succeed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    repo
}

/// Spawns `margin` with `args` on a pseudo-terminal in `repo`.
fn spawn_margin(repo: &TestRepo, extra: &[(&str, PathBuf)], args: &[&str]) -> Pty {
    Pty::spawn(
        Path::new(env!("CARGO_BIN_EXE_margin")),
        args,
        &envs(repo, extra),
        repo.path(),
        110,
        32,
    )
    .expect("the command spawns on a pseudo-terminal")
}

/// The commits whose marker names one review, in Git's own words.
fn review_markers(repo: &TestRepo) -> Vec<String> {
    String::from_utf8_lossy(&repo.git_ok(["log", "--format=%H", "--grep=Margin-Kind: review"]))
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn escaping_paths_are_rejected_before_anything_runs() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let before = support::oracle::snapshot(&repo);
    // The one shared document-path validator answers each shape with its
    // own reason (SPEC 4.1), the same words `margin tui` reports.
    for (path, reason) in [
        ("/etc/passwd", "is absolute"),
        ("../outside.md", "escapes the repository"),
        ("notes/../../outside.md", "escapes the repository"),
        ("notes/", "names no one document"),
    ] {
        let output = margin(repo.env(), repo.path(), &["edit", "--path", path]);
        assert_eq!(output.status.code(), Some(2), "{path} is invalid input");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("INVALID_ARGUMENT"), "{path}: {stderr}");
        assert!(stderr.contains(reason), "{path}: {stderr}");
    }
    before.assert_unchanged(&support::oracle::snapshot(&repo), "rejected paths");
}

#[test]
fn edit_refuses_without_a_terminal_and_touches_nothing() {
    // The stored argv never needs to resolve: the refusal comes first, so
    // the editor is never launched.
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n\nFirst passage.\n");
    repo.commit_all("import");
    let editor_json = "[\"/no/such/margin-editor\"]";
    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "init",
            "--scope",
            "notes/",
            "--editor-argv-json",
            editor_json,
        ],
    );
    assert_eq!(output.status.code(), Some(0));
    let before = support::oracle::snapshot(&repo);
    let output = margin(repo.env(), repo.path(), &["edit", "--path", "notes/a.md"]);
    assert_eq!(output.status.code(), Some(4), "an unmet precondition");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("OPERATION_UNAVAILABLE"), "{stderr}");
    assert!(stderr.contains("terminal"), "{stderr}");
    before.assert_unchanged(&support::oracle::snapshot(&repo), "the refusal");
}

#[test]
fn edit_refuses_when_no_editor_is_configured() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n\nFirst passage.\n");
    repo.commit_all("import");
    let output = margin(repo.env(), repo.path(), &["init", "--scope", "notes/"]);
    assert_eq!(output.status.code(), Some(0));
    let mut pty = spawn_margin(&repo, &[], &["edit", "--path", "notes/a.md"]);
    assert!(
        pty.wait_for("OPERATION_UNAVAILABLE", LIMIT),
        "the refusal is typed: {}",
        String::from_utf8_lossy(pty.raw_output()),
    );
    assert!(
        pty.wait_for("no editor is configured", LIMIT),
        "the refusal states the reason: {}",
        String::from_utf8_lossy(pty.raw_output()),
    );
    assert!(
        pty.shows("--editor-argv-json"),
        "the refusal names the setup input: {}",
        String::from_utf8_lossy(pty.raw_output()),
    );
    let status = pty.wait_exit(LIMIT).expect("the command exits");
    assert_eq!(status.code(), Some(4), "an unmet precondition");
}

#[test]
fn edit_hands_the_literal_path_and_reports_the_refreshed_state() {
    let repo = adopted_repo(0, &[]);
    let head_before = support::oracle::head_oid(&repo);
    let reviews_before = review_markers(&repo);

    let mut pty = spawn_margin(
        &repo,
        &[("MARGIN_EDIT_ARGV", repo.path().join("editor-argv.txt"))],
        &["edit", "--path", "notes/a.md"],
    );
    assert!(
        pty.wait_for("editor returned; no checkpoint was made", LIMIT),
        "the exit is a refresh boundary, not a capture: {}",
        String::from_utf8_lossy(pty.raw_output()),
    );
    assert!(
        pty.wait_for("preservation=uncaptured", LIMIT),
        "the refreshed state is reported in the same words as status: {}",
        String::from_utf8_lossy(pty.raw_output()),
    );
    let status = pty.wait_exit(LIMIT).expect("the command exits");
    assert_eq!(status.code(), Some(0));

    // The editor received exactly the literal path of the document: one
    // argument, appended to the explicit argv (SPEC 12.5).
    let recorded = std::fs::read_to_string(repo.path().join("editor-argv.txt"))
        .expect("the editor recorded its argv");
    let expected = std::fs::canonicalize(repo.file_path("notes/a.md"))
        .unwrap_or_else(|_| repo.file_path("notes/a.md"));
    assert_eq!(
        recorded.trim(),
        expected.display().to_string(),
        "the editor's one argument is the literal document path",
    );

    // The editor's write stays the editor's: nothing committed, nothing
    // staged (SPEC 12.5).
    assert!(
        String::from_utf8_lossy(&repo.read_file("notes/a.md"))
            .contains("Edited in the stand-in editor."),
        "the editor's line is in the document",
    );
    assert_eq!(
        support::oracle::head_oid(&repo),
        head_before,
        "the handoff committed nothing",
    );
    support::oracle::assert_index_matches_tree(&repo, "HEAD");
    assert_eq!(
        review_markers(&repo),
        reviews_before,
        "nothing was approved"
    );
}

#[test]
fn edit_captures_one_draft_only_under_the_stored_policy() {
    let repo = adopted_repo(0, &["--capture-after-edit"]);
    let head_before = support::oracle::head_oid(&repo);
    let reviews_before = review_markers(&repo);

    let mut pty = spawn_margin(
        &repo,
        &[("MARGIN_EDIT_ARGV", repo.path().join("editor-argv.txt"))],
        &["edit", "--path", "notes/a.md"],
    );
    assert!(
        pty.wait_for("nothing was approved", LIMIT),
        "the capture states what it did not do: {}",
        String::from_utf8_lossy(pty.raw_output()),
    );
    let status = pty.wait_exit(LIMIT).expect("the command exits");
    assert_eq!(status.code(), Some(0));

    let head = support::oracle::head_oid(&repo).expect("the capture made one commit");
    assert_ne!(Some(head), head_before, "the policy enabled one capture",);
    let message_bytes = repo.git_ok(["log", "-1", "--format=%B"]);
    let message = String::from_utf8_lossy(&message_bytes);
    assert!(
        message.contains("Margin-Kind: draft"),
        "the capture is one ordinary draft commit: {message}",
    );
    // The captured draft holds the editor's line, and the working tree is
    // clean against it.
    let committed = repo.git_ok(["show", "HEAD:notes/a.md"]);
    assert!(
        String::from_utf8_lossy(&committed).contains("Edited in the stand-in editor."),
        "the commit carries the editor's line",
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
        "the editor exit approved nothing",
    );
}

#[test]
fn a_failing_editor_exit_never_captures_even_under_the_policy() {
    let repo = adopted_repo(3, &["--capture-after-edit"]);
    let head_before = support::oracle::head_oid(&repo);
    let reviews_before = review_markers(&repo);

    let mut pty = spawn_margin(
        &repo,
        &[("MARGIN_EDIT_ARGV", repo.path().join("editor-argv.txt"))],
        &["edit", "--path", "notes/a.md"],
    );
    assert!(
        pty.wait_for("editor exited with exit status: 3", LIMIT),
        "the editor's own exit is reported: {}",
        String::from_utf8_lossy(pty.raw_output()),
    );
    assert!(
        pty.wait_for("nothing was applied", LIMIT),
        "an abandoned pass applies nothing: {}",
        String::from_utf8_lossy(pty.raw_output()),
    );
    let status = pty.wait_exit(LIMIT).expect("the command exits");
    assert_eq!(status.code(), Some(0));

    // The editor's partial line stays an unsaved draft on disk.
    assert!(
        String::from_utf8_lossy(&repo.read_file("notes/a.md"))
            .contains("Edited in the stand-in editor."),
        "the editor's line is in the document",
    );
    assert_eq!(
        support::oracle::head_oid(&repo),
        head_before,
        "a failed editor pass captured nothing",
    );
    assert_eq!(review_markers(&repo), reviews_before);
}

#[test]
fn an_editor_that_cannot_start_is_one_typed_failure() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n\nFirst passage.\n");
    repo.commit_all("import");
    let editor_json = "[\"/no/such/margin-editor\"]";
    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "init",
            "--scope",
            "notes/",
            "--editor-argv-json",
            editor_json,
        ],
    );
    assert_eq!(output.status.code(), Some(0));
    let before = support::oracle::snapshot(&repo);

    let mut pty = spawn_margin(&repo, &[], &["edit", "--path", "notes/a.md"]);
    assert!(
        pty.wait_for("IO_FAILED", LIMIT),
        "the start failure is typed: {}",
        String::from_utf8_lossy(pty.raw_output()),
    );
    assert!(
        pty.wait_for("the editor could not start", LIMIT),
        "the start failure is stated: {}",
        String::from_utf8_lossy(pty.raw_output()),
    );
    let status = pty.wait_exit(LIMIT).expect("the command exits");
    assert_eq!(status.code(), Some(5), "an execution failure");
    before.assert_unchanged(&support::oracle::snapshot(&repo), "the start failure");
}
