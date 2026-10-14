//! Installed-binary tests for the CLI envelope and command discovery
//! (SPEC 6, 7, and 16.8, tasks T015 and T039).
//!
//! Every case runs the real executable through an intact pipe with the
//! isolated harness environment, then checks the one JSON envelope, the
//! exit class, and the absence of terminal decoration. These tests cover
//! the interface contract; the operation semantics behind each command
//! arrive with their own tasks.
//!
//! The T039 cases complete the conformance families of SPEC 16.8 that the
//! command suites exercise per command: plans on standard input, the
//! execution-failure exit class, output into a consumer that closed its
//! pipe, execution with no terminal at all, and the example walkthrough of
//! `examples/README.md`. The cursor, output-limit, and raw-export families
//! live with their commands in `diff_results`, `editorial_log`, and
//! `show_document`.

mod support;

use std::fs;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use support::env::{IsolatedEnv, environment_report, is_scrubbed};
use support::repo::TestRepo;

/// Runs the installed executable once with the isolated environment.
fn margin(env: &IsolatedEnv, cwd: &Path, args: &[&str]) -> Output {
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

/// Parses the one JSON envelope of a machine-mode run and checks the
/// envelope contract itself (SPEC 7.1).
fn envelope_of(output: &Output, what: &str) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.matches('\n').count(),
        1,
        "{what}: exactly one envelope line, got: {stdout:?}"
    );
    assert!(
        !stdout.contains('\u{1b}'),
        "{what}: JSON output carries no ANSI escapes"
    );
    let envelope: Value = serde_json::from_str(stdout.trim_end())
        .unwrap_or_else(|error| panic!("{what}: the envelope must parse: {error}: {stdout}"));
    for field in [
        "schema_version",
        "command",
        "ok",
        "data",
        "error",
        "warnings",
        "effects",
    ] {
        assert!(
            envelope.get(field).is_some(),
            "{what}: the envelope must carry {field}"
        );
    }
    for field in [
        "working_tree",
        "index",
        "commit",
        "commit_oid",
        "repository",
        "configuration",
        "integration",
    ] {
        assert!(
            envelope["effects"].get(field).is_some(),
            "{what}: the effects must carry {field}"
        );
    }
    assert!(
        output.stderr.is_empty(),
        "{what}: machine mode writes no stderr, got: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    envelope
}

/// The error code of one failed envelope.
fn code_of(envelope: &Value) -> &str {
    envelope["error"]["code"].as_str().unwrap()
}

#[test]
fn version_json_reports_build_schema_engine_and_git() {
    let outside = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let env = IsolatedEnv::new(home.path());
    let output = margin(&env, outside.path(), &["--version", "--json"]);

    assert_eq!(exit_of(&output), 0, "version succeeds");
    let envelope = envelope_of(&output, "version");
    assert_eq!(envelope["command"], "version");
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["error"], Value::Null);
    let data = &envelope["data"];
    assert_eq!(data["name"], "margin");
    assert_eq!(data["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(data["envelope_schema_version"], 1);
    assert_eq!(data["engine_version"], "1");
    assert_eq!(data["git"]["version"], environment_report().git_version);
    // Outside a repository no object format is claimed.
    assert_eq!(data["git"]["object_format"], Value::Null);
}

#[test]
fn version_json_inside_a_repository_reports_the_object_format() {
    let repo = TestRepo::init();
    let output = margin(repo.env(), repo.path(), &["--version", "--json"]);

    assert_eq!(exit_of(&output), 0);
    let envelope = envelope_of(&output, "version in a repository");
    assert_eq!(envelope["data"]["git"]["object_format"], "sha1");
}

#[test]
fn help_json_lists_the_specified_command_inventory() {
    let outside = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let env = IsolatedEnv::new(home.path());
    let output = margin(&env, outside.path(), &["--help", "--json"]);

    assert_eq!(exit_of(&output), 0);
    let envelope = envelope_of(&output, "structured help");
    assert_eq!(envelope["command"], "help");
    assert_eq!(envelope["ok"], true);
    let data = &envelope["data"];

    let mut names: Vec<&str> = data["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "checkpoint",
            "desk",
            "diff",
            "doctor",
            "edit",
            "hook claude",
            "hook install",
            "hook remove",
            "init",
            "log",
            "note",
            "notes",
            "restore",
            "review",
            "show",
            "status",
            "tui"
        ]
    );

    let globals: Vec<&str> = data["global_options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["option"].as_str().unwrap())
        .collect();
    for expected in [
        "--json",
        "--version",
        "--cwd",
        "--timeout-ms",
        "--color",
        "--help",
    ] {
        assert!(
            globals.contains(&expected),
            "the inventory must list {expected}"
        );
    }

    // The exit classes of SPEC 7.5 reach the machine interface unchanged.
    let classes = data["exit_classes"].as_array().unwrap();
    let exits: Vec<u64> = classes
        .iter()
        .map(|row| row["exit"].as_u64().unwrap())
        .collect();
    assert_eq!(exits, [0, 2, 3, 4, 5, 130]);
    let class_four = classes.iter().find(|row| row["exit"] == 4).unwrap();
    for code in ["NOT_INITIALIZED", "STAGED_CHANGES", "BASELINE_UNAVAILABLE"] {
        assert!(
            class_four["codes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|listed| listed == code),
            "class 4 lists {code}"
        );
    }
}

#[test]
fn help_and_version_stay_available_in_text_mode() {
    let outside = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let env = IsolatedEnv::new(home.path());

    let help = margin(&env, outside.path(), &["--help"]);
    assert_eq!(exit_of(&help), 0);
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(text.contains("Usage:"), "got: {text}");
    assert!(text.contains("Exit classes:"));
    assert!(text.contains("checkpoint"));
    assert!(help.stderr.is_empty());

    let version = margin(&env, outside.path(), &["--version"]);
    assert_eq!(exit_of(&version), 0);
    assert_eq!(String::from_utf8_lossy(&version.stdout), "margin 0.1.0\n");
}

#[test]
fn parser_errors_use_one_envelope_before_and_after_the_command() {
    let repo = TestRepo::init();
    for args in [
        vec!["--json", "status", "--nope"],
        vec!["status", "--nope", "--json"],
    ] {
        let output = margin(repo.env(), repo.path(), &args);
        assert_eq!(exit_of(&output), 2, "{args:?}: invalid invocation");
        let envelope = envelope_of(&output, "a parser error");
        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["command"], "status");
        assert_eq!(code_of(&envelope), "INVALID_ARGUMENT");
        assert_eq!(
            envelope["error"]["recovery_argv"],
            serde_json::json!(["margin", "--help", "--json"])
        );
        assert_eq!(envelope["effects"]["working_tree"], "unchanged");
    }
}

#[test]
fn an_unknown_command_reports_no_command() {
    let repo = TestRepo::init();
    let output = margin(repo.env(), repo.path(), &["--json", "frobnicate"]);
    assert_eq!(exit_of(&output), 2);
    let envelope = envelope_of(&output, "an unknown command");
    assert_eq!(envelope["command"], Value::Null);
    assert_eq!(code_of(&envelope), "INVALID_ARGUMENT");
}

#[test]
fn text_mode_reports_parser_errors_on_stderr() {
    let repo = TestRepo::init();
    let output = margin(repo.env(), repo.path(), &["status", "--nope"]);
    assert_eq!(exit_of(&output), 2);
    assert!(output.stdout.is_empty(), "text failures write no stdout");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("INVALID_ARGUMENT"), "got: {stderr}");
}

#[test]
fn status_without_stored_settings_is_not_initialized() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let before = support::oracle::snapshot(&repo);

    let output = margin(repo.env(), repo.path(), &["status", "--json"]);
    assert_eq!(exit_of(&output), 4, "an unmet precondition");
    let envelope = envelope_of(&output, "status");
    assert_eq!(envelope["command"], "status");
    assert_eq!(code_of(&envelope), "NOT_INITIALIZED");
    assert_eq!(
        envelope["error"]["recovery_argv"],
        serde_json::json!(["margin", "init", "--json"])
    );
    assert_eq!(envelope["effects"]["commit"], "none");

    // A query leaves the repository exactly as it was.
    before.assert_unchanged(&support::oracle::snapshot(&repo), "status");
}

#[test]
fn status_outside_any_repository_is_not_initialized() {
    let outside = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let env = IsolatedEnv::new(home.path());
    let output = margin(&env, outside.path(), &["status", "--json"]);
    assert_eq!(exit_of(&output), 4);
    let envelope = envelope_of(&output, "status outside a repository");
    assert_eq!(code_of(&envelope), "NOT_INITIALIZED");
}

#[test]
fn a_checkpoint_on_another_branch_is_exit_class_three() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let init = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes", "--json"],
    );
    assert_eq!(exit_of(&init), 0, "the workspace adopts branch main");
    repo.git_ok(["switch", "--quiet", "--create", "other"]);
    repo.write_file("notes/a.md", b"# One revised\n");
    let before = support::oracle::snapshot(&repo);

    // The stale-context exit class with unchanged effects: the adoption
    // stays what it was and the working edit stays on disk (SPEC 7.5, 8.2).
    let output = margin(repo.env(), repo.path(), &["checkpoint", "--json"]);
    assert_eq!(exit_of(&output), 3, "a stale context refuses the mutation");
    let envelope = envelope_of(&output, "a foreign-branch checkpoint");
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["command"], "checkpoint");
    assert_eq!(code_of(&envelope), "BRANCH_CHANGED");
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("other"),
        "the message names the current branch"
    );
    assert_eq!(
        envelope["error"]["recovery_argv"],
        serde_json::json!(["margin", "init", "--json"])
    );
    assert_eq!(envelope["effects"]["commit"], "none");
    assert_eq!(envelope["effects"]["index"], "unchanged");
    assert_eq!(envelope["effects"]["working_tree"], "unchanged");
    assert_eq!(envelope["effects"]["configuration"], "unchanged");
    before.assert_unchanged(&support::oracle::snapshot(&repo), "the refusal");
    let branch = repo.git(["config", "--local", "--get", "margin.branch"]);
    assert_eq!(
        String::from_utf8_lossy(&branch.stdout),
        "main\n",
        "the stored adoption survives the refusal"
    );
}

/// The reported defect of task T061 at the executable boundary: an edit
/// behind an index flag used to leave `status` reporting `captured` and
/// `checkpoint` reporting the successful no-op while `HEAD` kept the old
/// bytes. The contract now keeps the query truthful and the mutation
/// refused: exit class 4 with one specific blocker (SPEC 4.2, 7.5, 9.1).
#[test]
fn index_flags_never_hide_unsaved_content() {
    let repo = TestRepo::init();
    repo.write_file("draft.md", b"# Draft\n");
    repo.commit_all("import");
    let init = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "draft.md", "--json"],
    );
    assert_eq!(exit_of(&init), 0);
    repo.write_file("draft.md", b"# Draft revised\n");

    for flag in ["assume-unchanged", "skip-worktree"] {
        repo.git_ok(["update-index", &format!("--{flag}"), "--", "draft.md"]);
        let before = support::oracle::snapshot(&repo);

        // The query stays truthful: the flagged edit is uncaptured and one
        // blocker names the flag (SPEC 4.2, 6.3).
        let status = margin(repo.env(), repo.path(), &["status", "--json"]);
        assert_eq!(exit_of(&status), 0, "finding differences is a success");
        let envelope = envelope_of(&status, "status behind an index flag");
        assert_eq!(envelope["ok"], true);
        assert_eq!(
            envelope["data"]["preservation"], "uncaptured",
            "behind --{flag}"
        );
        assert_eq!(envelope["data"]["counts"]["uncaptured_total"], 1);
        let blockers = envelope["data"]["blockers"].as_array().unwrap();
        assert_eq!(blockers.len(), 1, "one specific blocker: {blockers:?}");
        assert!(
            blockers[0].as_str().unwrap().contains(flag),
            "the blocker names --{flag}: {blockers:?}"
        );

        // The mutation refuses before anything is written: no successful
        // no-op while HEAD keeps the old bytes (SPEC 9.1 step 4).
        let output = margin(repo.env(), repo.path(), &["checkpoint", "--json"]);
        assert_eq!(exit_of(&output), 4, "an unmet operational precondition");
        let envelope = envelope_of(&output, "a flagged checkpoint");
        assert_eq!(envelope["ok"], false);
        assert_eq!(code_of(&envelope), "UNSUPPORTED_REPOSITORY");
        let message = envelope["error"]["message"].as_str().unwrap();
        assert!(
            message.contains(flag) && message.contains("draft.md"),
            "the diagnosis names the flag and the path behind --{flag}: {message}"
        );
        assert_eq!(envelope["effects"]["commit"], "none");
        assert_eq!(envelope["effects"]["index"], "unchanged");
        assert_eq!(envelope["effects"]["working_tree"], "unchanged");
        before.assert_unchanged(&support::oracle::snapshot(&repo), "behind --{flag}");
        assert_eq!(
            repo.git_ok(["show", "HEAD:draft.md"]),
            b"# Draft\n",
            "HEAD keeps the old bytes behind --{flag}"
        );

        repo.git_ok(["update-index", &format!("--no-{flag}"), "--", "draft.md"]);
    }
}

#[test]
fn the_recovery_capture_needs_its_expected_state_on_the_command_line() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let init = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes", "--json"],
    );
    assert_eq!(exit_of(&init), 0);
    let before = support::oracle::snapshot(&repo);

    // The recovery mode without its three fingerprints is a parser error:
    // one bounded envelope of the invalid-invitation class, so no terminal
    // decoration and no second vocabulary (SPEC 6.1, 9.7).
    let output = margin(
        repo.env(),
        repo.path(),
        &["checkpoint", "--include-staged", "--json"],
    );
    assert_eq!(exit_of(&output), 2, "an invalid invocation");
    let envelope = envelope_of(&output, "the recovery mode without its state");
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["command"], "checkpoint");
    assert_eq!(code_of(&envelope), "INVALID_ARGUMENT");
    assert_eq!(envelope["effects"]["commit"], "none");

    // Each expected value belongs to the recovery mode alone.
    for args in [
        vec![
            "checkpoint",
            "--expect-head",
            "0123456789012345678901234567890123456789",
            "--json",
        ],
        vec![
            "checkpoint",
            "--include-staged",
            "--expect-head",
            "0123456789012345678901234567890123456789",
            "--expect-index",
            "0123456789012345678901234567890123456789",
            "--json",
        ],
    ] {
        let output = margin(repo.env(), repo.path(), &args);
        assert_eq!(exit_of(&output), 2, "{args:?}: an invalid invocation");
        let envelope = envelope_of(&output, "an incomplete expected state");
        assert_eq!(code_of(&envelope), "INVALID_ARGUMENT");
    }

    // Nothing was written while the command line was refused.
    before.assert_unchanged(&support::oracle::snapshot(&repo), "the refusals");
}

#[test]
fn commands_without_algorithms_refuse_typed_and_unchanged() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let before = support::oracle::snapshot(&repo);
    // The `log` timeline left this list with task T022; the interactive TUI
    // stays the one query-shaped command without a noninteractive result.
    let cases: Vec<(&[&str], &str)> = vec![(&["tui"], "tui")];
    for (args, command) in cases {
        let output = margin(repo.env(), repo.path(), args);
        assert_eq!(exit_of(&output), 4, "{command} is a pending operation");
        // Text mode: the refusal is one bounded stderr line.
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("OPERATION_UNAVAILABLE"));
    }
    before.assert_unchanged(&support::oracle::snapshot(&repo), "pending commands");
}

#[test]
fn interactive_commands_reject_json_with_a_typed_error() {
    let repo = TestRepo::init();
    for args in [
        vec!["tui", "--json"],
        vec!["edit", "--path", "notes/a.md", "--json"],
        vec!["desk", "--json", "--", "claude"],
    ] {
        let output = margin(repo.env(), repo.path(), &args);
        assert_eq!(exit_of(&output), 2, "{args:?}: interactive commands");
        let envelope = envelope_of(&output, "an interactive rejection");
        assert_eq!(code_of(&envelope), "INVALID_ARGUMENT");
        assert!(
            envelope["error"]["message"]
                .as_str()
                .unwrap()
                .contains("interactive"),
            "the message names the reason"
        );
    }
}

#[test]
fn hook_claude_answers_outside_the_envelope() {
    let repo = TestRepo::init();
    // No payload arrives on standard input, so the event is missing: one
    // non-blocking refusal, never the blocking exit 2.
    let output = margin(repo.env(), repo.path(), &["hook", "claude"]);
    assert_eq!(exit_of(&output), 1);
    // The hook adapter speaks the upstream hook protocol, not the Margin
    // envelope (SPEC 6.2), so stdout stays empty and stderr stays bounded.
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("margin hook: "),
        "the refusal is one bounded line: {stderr}"
    );
    assert!(
        stderr.contains("the hook payload was empty"),
        "the missing event is named: {stderr}"
    );
    assert!(stderr.lines().count() <= 2);
}

#[test]
fn hook_setup_commands_use_the_envelope() {
    let repo = TestRepo::init();
    // Both setup actions answer in the shared envelope (SPEC 12.4): an
    // installation changes the integration configuration, and the removal
    // of nothing is the documented no-op.
    let output = margin(
        repo.env(),
        repo.path(),
        &["hook", "install", "claude", "--json"],
    );
    assert_eq!(exit_of(&output), 0, "the installation succeeds");
    let envelope = envelope_of(&output, "hook install");
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["data"]["action"], "install");
    assert_eq!(envelope["effects"]["integration"], "changed");

    let output = margin(
        repo.env(),
        repo.path(),
        &["hook", "remove", "claude", "--json"],
    );
    assert_eq!(exit_of(&output), 0, "the removal succeeds");
    let envelope = envelope_of(&output, "hook remove");
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["data"]["action"], "remove");
    assert_eq!(envelope["effects"]["integration"], "changed");

    // A repository without the entries reports the no-op, not a change.
    let output = margin(
        repo.env(),
        repo.path(),
        &["hook", "remove", "claude", "--json"],
    );
    let envelope = envelope_of(&output, "the repeated removal");
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["data"]["no_op"], true);
    assert_eq!(envelope["effects"]["integration"], "unchanged");

    // Only the claude integration exists in release 0.1.
    let output = margin(
        repo.env(),
        repo.path(),
        &["hook", "install", "codex", "--json"],
    );
    assert_eq!(exit_of(&output), 2);
    let envelope = envelope_of(&output, "an unknown integration");
    assert_eq!(code_of(&envelope), "INVALID_ARGUMENT");
}

#[test]
fn init_validates_the_editor_argv_setting() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");

    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "init",
            "--scope",
            "notes/",
            "--editor-argv-json",
            "[\"nvim\",\"--\"]",
            "--json",
        ],
    );
    assert_eq!(exit_of(&output), 0, "a valid editor argv initializes");
    let envelope = envelope_of(&output, "a valid editor argv");
    assert_eq!(envelope["ok"], true);
    assert!(envelope["data"]["start_commit"].is_string());
    assert!(
        envelope["data"]["settings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["key"] == "margin.editorArgv")
    );

    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "init",
            "--scope",
            "notes/",
            "--editor-argv-json",
            "\"nvim\"",
            "--json",
        ],
    );
    assert_eq!(exit_of(&output), 2, "a scalar is not an argv");
    let envelope = envelope_of(&output, "an invalid editor argv");
    assert_eq!(code_of(&envelope), "INVALID_ARGUMENT");
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("editor-argv-json"),
        "the message names the option"
    );
}

#[test]
fn invocation_bounds_are_typed_invalid_input() {
    let repo = TestRepo::init();
    for args in [
        vec!["--timeout-ms", "0", "status", "--json"],
        vec!["--cwd", "/no/such/margin-directory", "status", "--json"],
        // An exclusion without an inclusion has nothing to subtract from.
        vec!["init", "--exclude", "notes/skip/", "--json"],
        vec!["review", "--input", "/no/such/plan.json", "--json"],
    ] {
        let output = margin(repo.env(), repo.path(), &args);
        assert_eq!(exit_of(&output), 2, "{args:?}: invalid invocation");
        let envelope = envelope_of(&output, "an invalid invocation");
        assert_eq!(code_of(&envelope), "INVALID_ARGUMENT");
    }
}

#[test]
fn opposing_setup_flags_conflict() {
    let repo = TestRepo::init();
    let output = margin(
        repo.env(),
        repo.path(),
        &[
            "init",
            "--scope",
            "notes/",
            "--auto-checkpoint",
            "--no-auto-checkpoint",
            "--json",
        ],
    );
    assert_eq!(exit_of(&output), 2);
    let envelope = envelope_of(&output, "opposing flags");
    assert_eq!(code_of(&envelope), "INVALID_ARGUMENT");
}

#[test]
fn a_huge_argument_stays_bounded() {
    let repo = TestRepo::init();
    let huge = "A".repeat(100_000);
    let output = margin(repo.env(), repo.path(), &["--json", &huge]);
    assert_eq!(exit_of(&output), 2);
    let envelope = envelope_of(&output, "a huge argument");
    let message = envelope["error"]["message"].as_str().unwrap();
    assert!(
        message.chars().count() <= 2_000,
        "the message stays bounded, got {} characters",
        message.chars().count()
    );
}

#[test]
fn every_json_call_emits_exactly_one_valid_envelope() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let invocations: Vec<Vec<&str>> = vec![
        vec!["--version", "--json"],
        vec!["--help", "--json"],
        vec!["status", "--json"],
        vec!["diff", "--json"],
        vec!["log", "--json"],
        vec!["doctor", "--json"],
        vec!["checkpoint", "--dry-run", "--json"],
        vec!["--json"],
        vec!["--json", "not-a-command"],
        vec!["status", "--nope", "--json"],
        vec!["tui", "--json"],
        vec!["hook", "install", "claude", "--json"],
        vec![
            "show",
            "--path",
            "notes/a.md",
            "--revision",
            "HEAD",
            "--json",
        ],
    ];
    for args in &invocations {
        let output = margin(repo.env(), repo.path(), args);
        // With an intact pipe, every machine-mode call answers with one
        // parseable envelope whatever its exit class (SPEC 16.8).
        envelope_of(&output, &format!("{args:?}"));
    }
}

// ---------------------------------------------------------------------------
// The remaining conformance families of SPEC 16.8 (task T039)
// ---------------------------------------------------------------------------

/// Builds the installed executable with the isolated environment, standard
/// input closed, and both output channels piped, so a case can adjust the
/// command further before it runs.
fn prepared_command(env: &IsolatedEnv, cwd: &Path, args: &[&str]) -> Command {
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
}

/// Runs the installed executable with `input` on standard input (SPEC 6.2
/// `--input -`).
fn margin_stdin(env: &IsolatedEnv, cwd: &Path, args: &[&str], input: &[u8]) -> Output {
    let mut command = prepared_command(env, cwd, args);
    command.stdin(Stdio::piped());
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"));
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(input)
            .unwrap_or_else(|error| panic!("cannot feed the binary standard input: {error}"));
    }
    child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("cannot await the margin binary: {error}"))
}

/// Runs the installed executable with the terminal removed from its
/// environment: no `TERM`, no terminal database, and no assumed screen
/// size. Standard input stays closed and both channels stay pipes.
fn margin_without_terminal(env: &IsolatedEnv, cwd: &Path, args: &[&str]) -> Output {
    let mut command = prepared_command(env, cwd, args);
    for variable in ["TERM", "TERMINFO", "TERMINFO_DIRS", "COLUMNS", "LINES"] {
        command.env_remove(variable);
    }
    command
        .output()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"))
}

/// Runs the installed executable with one directory ahead of every other on
/// `PATH`, so a case controls the `git` the adapter resolves.
fn margin_with_git_directory(
    env: &IsolatedEnv,
    cwd: &Path,
    args: &[&str],
    directory: &Path,
) -> Output {
    let mut command = prepared_command(env, cwd, args);
    command.env("PATH", directory.as_os_str());
    command
        .output()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"))
}

/// Runs the installed executable into a pipe this process closes before the
/// command can deliver its answer, then waits for its termination. The
/// returned output carries no standard output: the consumer went away
/// (SPEC 16.8).
fn margin_into_closed_pipe(env: &IsolatedEnv, cwd: &Path, args: &[&str]) -> Output {
    let mut child = prepared_command(env, cwd, args)
        .spawn()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"));
    drop(child.stdout.take());
    child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("cannot await the margin binary: {error}"))
}

/// Runs the installed executable into a pipe this process abandons after a
/// partial read, so the command's write fails while its payload is still
/// flowing. The returned output carries only the bytes that arrived.
fn margin_into_abandoned_pipe(env: &IsolatedEnv, cwd: &Path, args: &[&str]) -> Output {
    let mut child = prepared_command(env, cwd, args)
        .spawn()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"));
    let mut reader = child.stdout.take().expect("standard output is piped");
    let mut arrived = Vec::new();
    reader
        .by_ref()
        .take(64)
        .read_to_end(&mut arrived)
        .unwrap_or_else(|error| panic!("cannot sample the pipe: {error}"));
    drop(reader);
    let output = child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("cannot await the margin binary: {error}"));
    Output {
        status: output.status,
        stdout: arrived,
        stderr: output.stderr,
    }
}

/// Writes one executable `git` wrapper with `body` and returns the
/// directory that holds it.
fn git_wrapper(body: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("git");
    fs::write(&script, body).unwrap();
    let metadata = fs::metadata(&script).unwrap();
    let mut permissions = metadata.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).unwrap();
    directory
}

/// One workspace over `notes` with one pending scoped change, adopted
/// through the executable itself. Returns the repository.
fn adopted_workspace() -> TestRepo {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# Title\n\nOriginal passage.\n\nClosing.\n");
    repo.commit_all("import");
    let output = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes", "--json"],
    );
    assert_eq!(exit_of(&output), 0, "initialization must succeed");
    repo.write_file("notes/a.md", b"# Title\n\nChanged passage.\n\nClosing.\n");
    repo.commit_all("writing pass");
    repo
}

/// One keep-all review plan for the workspace's pinned comparison, built
/// from the envelope `diff --json` itself reported, and written outside the
/// repository so it stays out of every scope.
fn keep_all_plan(repo: &TestRepo) -> String {
    let diff = margin(repo.env(), repo.path(), &["diff", "--json"]);
    assert_eq!(exit_of(&diff), 0, "the pinned review builds");
    let envelope = envelope_of(&diff, "diff");
    assert_eq!(envelope["ok"], true, "diff: {envelope}");
    let data = &envelope["data"];
    let plan = serde_json::json!({
        "schema_version": 1,
        "request_id": "cli-conformance-01",
        "review_id": data["review_id"],
        "branch_ref": data["branch_ref"],
        "scope_id": data["scope_id"],
        "baseline_oid": data["baseline_oid"],
        "candidate_oid": data["candidate_oid"],
        "restore": [],
        "keep_remaining": true,
    });
    let path = repo.root().join("plan.json");
    fs::write(&path, plan.to_string()).unwrap();
    path.to_str().unwrap().to_string()
}

/// A plan document on standard input feeds the restoration contract the
/// same way a named file does (SPEC 6.2 `--input -`, 16.8).
#[test]
fn restore_reads_one_plan_from_standard_input() {
    let repo = adopted_workspace();
    let status = margin(repo.env(), repo.path(), &["status", "--json"]);
    let envelope = envelope_of(&status, "status");
    let data = &envelope["data"];
    assert_eq!(data["baseline"]["state"], "anchored");
    let plan = serde_json::json!({
        "schema_version": 1,
        "request_id": "cli-restore-stdin-01",
        "branch_ref": data["branch"],
        "scope_id": data["scope"]["digest"],
        "candidate_oid": data["head"],
        "path": "notes/a.md",
        "revision": data["baseline"]["commit"],
    })
    .to_string();

    let output = margin_stdin(
        repo.env(),
        repo.path(),
        &["restore", "--input", "-", "--dry-run", "--json"],
        plan.as_bytes(),
    );
    assert_eq!(exit_of(&output), 0, "a plan on standard input previews");
    let envelope = envelope_of(&output, "a restoration plan on standard input");
    assert_eq!(envelope["command"], "restore");
    assert_eq!(envelope["ok"], true, "{envelope}");
    assert_eq!(envelope["data"]["dry_run"], true);
    assert_eq!(envelope["data"]["path"], "notes/a.md");
    assert_eq!(envelope["effects"]["commit"], "none");

    // Bytes that cannot be a plan refuse typed, in the same one envelope.
    let output = margin_stdin(
        repo.env(),
        repo.path(),
        &["restore", "--input", "-", "--dry-run", "--json"],
        b"{not json",
    );
    assert_eq!(exit_of(&output), 2);
    let envelope = envelope_of(&output, "broken bytes on standard input");
    assert_eq!(code_of(&envelope), "INVALID_PLAN");
}

/// A consumer that closed its pipe loses the envelope and nothing else: the
/// command terminates with its own exit class, the repository holds the
/// effect it produced, and the retry by receipt rediscovers that effect
/// without repeating it (SPEC 7.1, 16.8).
#[test]
fn a_closed_pipe_loses_only_the_envelope() {
    let repo = adopted_workspace();
    let plan = keep_all_plan(&repo);
    let candidate = support::oracle::head_oid(&repo).expect("the candidate stands");

    // A query through a closed pipe still terminates with class zero.
    let query = margin_into_closed_pipe(repo.env(), repo.path(), &["status", "--json"]);
    assert_eq!(exit_of(&query), 0, "a lost reader never changes the class");
    assert!(query.stdout.is_empty(), "nothing can be delivered");
    assert_eq!(
        support::oracle::head_oid(&repo).as_deref(),
        Some(candidate.as_str()),
        "the query wrote nothing"
    );

    // The application commits, then cannot deliver its envelope.
    let output = margin_into_closed_pipe(
        repo.env(),
        repo.path(),
        &["review", "--input", &plan, "--json"],
    );
    assert_eq!(
        exit_of(&output),
        0,
        "the exit class reports the operation, not the write"
    );
    assert!(output.stdout.is_empty(), "the envelope cannot be delivered");

    // Stock Git sees the review the caller never saw: one new commit over
    // the candidate, the candidate's whole tree, and the reserved marker.
    let head = support::oracle::head_oid(&repo).expect("the review commit exists");
    assert_ne!(head, candidate);
    assert_eq!(
        support::text(&repo.git_ok(["log", "--format=%P", "-n1", "HEAD"])),
        candidate,
        "the review commit sits on the candidate"
    );
    assert_eq!(
        support::oracle::commit_tree(&repo, &head),
        support::oracle::commit_tree(&repo, &candidate),
        "a keep-all decision changes no entry"
    );
    let message = support::text(&repo.git_ok(["log", "--format=%B", "-n1", "HEAD"]));
    assert!(
        message.contains("Margin-Kind: review"),
        "the commit carries the review marker: {message}"
    );

    // The retry by receipt returns the standing decision, not a second one.
    let commits = support::text(&repo.git_ok(["rev-list", "--count", "HEAD"]));
    let retry = margin(
        repo.env(),
        repo.path(),
        &["review", "--input", &plan, "--json"],
    );
    let envelope = envelope_of(&retry, "the retry after the lost response");
    assert_eq!(exit_of(&retry), 0);
    assert_eq!(envelope["ok"], true);
    assert_eq!(
        envelope["data"]["existing_receipt"],
        serde_json::json!(head)
    );
    assert_eq!(envelope["effects"]["commit"], "existing_receipt");
    assert_eq!(envelope["effects"]["commit_oid"], serde_json::json!(head));
    assert_eq!(
        support::text(&repo.git_ok(["rev-list", "--count", "HEAD"])),
        commits,
        "the retry commits nothing new"
    );
}

/// One large raw export into an abandoned pipe: the write fails while the
/// bytes are still flowing, and the command still terminates with its own
/// class instead of hanging or dying from the pipe (SPEC 16.8).
#[test]
fn a_large_export_into_an_abandoned_pipe_still_terminates() {
    let repo = TestRepo::init();
    let mut body = String::from("# Large\n\n");
    while body.len() < 256 * 1024 {
        body.push_str("abcdefghijklmnopqrstuvwxyz0123456789\n");
    }
    let bytes = body.into_bytes();
    repo.write_file("notes/big.md", &bytes);
    repo.commit_all("import");
    let before = support::oracle::snapshot(&repo);

    let output = margin_into_abandoned_pipe(
        repo.env(),
        repo.path(),
        &[
            "show",
            "--path",
            "notes/big.md",
            "--revision",
            "HEAD",
            "--format",
            "raw",
        ],
    );
    assert_eq!(
        exit_of(&output),
        0,
        "the failed write terminates without changing the class"
    );
    assert!(!output.stdout.is_empty(), "some bytes arrived first");
    assert_eq!(
        &output.stdout[..],
        &bytes[..output.stdout.len()],
        "the bytes that arrived are the document's own"
    );
    before.assert_unchanged(&support::oracle::snapshot(&repo), "the raw export");
}

/// A Git that fails reaches the caller as the execution-failure class, in
/// the same one envelope as every other answer (SPEC 7.5, 16.8).
#[test]
fn a_failing_git_subprocess_is_exit_class_five() {
    let wrapper = git_wrapper("#!/bin/sh\nprintf 'fatal: injected failure\\n' >&2\nexit 128\n");
    let outside = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let env = IsolatedEnv::new(home.path());

    // The version query is the interface's first Git call, so its failure
    // cannot be mistaken for an unmet precondition.
    let output = margin_with_git_directory(
        &env,
        outside.path(),
        &["--version", "--json"],
        wrapper.path(),
    );
    assert_eq!(exit_of(&output), 5, "an execution failure is class five");
    let envelope = envelope_of(&output, "a failing Git");
    assert_eq!(envelope["command"], "version");
    assert_eq!(envelope["ok"], false);
    assert_eq!(code_of(&envelope), "GIT_FAILED");
    let message = envelope["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("128"),
        "the message carries Git's own exit: {message}"
    );
    assert!(
        message.contains("injected failure"),
        "the bounded diagnostic names the cause: {message}"
    );
}

/// A Git that never answers dies at the configured budget, and the caller
/// learns an unknown outcome instead of a success or a hang (SPEC 6.1, 7.5).
#[test]
fn a_git_beyond_the_budget_reports_an_unknown_outcome() {
    // `exec` keeps the wrapper one process, so the budget kills the sleep
    // itself instead of leaving an orphan holding the pipe.
    let wrapper = git_wrapper("#!/bin/sh\nexec /bin/sleep 30\n");
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");

    let output = margin_with_git_directory(
        repo.env(),
        repo.path(),
        &["--timeout-ms", "500", "status", "--json"],
        wrapper.path(),
    );
    assert_eq!(exit_of(&output), 5, "an uncertain outcome is class five");
    let envelope = envelope_of(&output, "a Git beyond its budget");
    assert_eq!(code_of(&envelope), "OUTCOME_UNKNOWN");
    assert_eq!(envelope["error"]["retryable"], false);
}

/// The binary answers with no terminal at all: no `TERM`, no terminal
/// database, no screen size, and a closed standard input. Text mode stays
/// one bounded line, JSON stays one envelope, and a decoration request
/// cannot introduce escape sequences (SPEC 6.1, 16.8).
#[test]
fn the_binary_answers_without_any_terminal() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let init = margin_without_terminal(
        repo.env(),
        repo.path(),
        &["init", "--scope", "notes", "--json"],
    );
    assert_eq!(exit_of(&init), 0, "initialization needs no terminal");
    envelope_of(&init, "init without a terminal");

    let invocations: Vec<Vec<&str>> = vec![
        vec!["--version"],
        vec!["--version", "--json"],
        vec!["--help"],
        vec!["status"],
        vec!["status", "--json"],
        vec!["diff"],
        vec!["log"],
        vec!["doctor"],
        vec!["--color", "always", "status"],
        vec!["--color", "always", "status", "--json"],
    ];
    for args in &invocations {
        let output = margin_without_terminal(repo.env(), repo.path(), args);
        assert_eq!(exit_of(&output), 0, "{args:?} answers without a terminal");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stdout.is_empty(), "{args:?} prints its answer");
        assert!(
            stdout.lines().count() <= 60,
            "{args:?} stays bounded without a pager, got {} lines",
            stdout.lines().count()
        );
        for stream in [&stdout, &stderr] {
            assert!(
                !stream.contains('\u{1b}'),
                "{args:?} decorates nothing: {stream:?}"
            );
        }
    }

    // No run above touched the repository.
    let after = margin_without_terminal(repo.env(), repo.path(), &["status", "--json"]);
    let envelope = envelope_of(&after, "a final status");
    assert_eq!(envelope["effects"]["working_tree"], "unchanged");
}

/// The example walkthrough of `examples/README.md` still invokes the program
/// correctly: every documented command answers, the plan it builds uses the
/// generated identities of this run, and the outcomes the README promises
/// happen (SPEC 16.8). A command the parser no longer accepts, or an example
/// whose promised result no longer holds, fails here.
#[test]
fn the_example_walkthrough_still_invokes_the_program() {
    let documents = ["article.md", "field-notes.md", "checklist.md"];
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let before: std::collections::BTreeMap<String, Vec<u8>> = documents
        .iter()
        .map(|name| {
            (
                name.to_string(),
                fs::read(examples.join("before").join(name)).unwrap(),
            )
        })
        .collect();
    let after: std::collections::BTreeMap<String, Vec<u8>> = documents
        .iter()
        .map(|name| {
            (
                name.to_string(),
                fs::read(examples.join("after").join(name)).unwrap(),
            )
        })
        .collect();

    // Step 1: one repository that holds the starting documents.
    let repo = TestRepo::init();
    for name in documents {
        let bytes = &before[name];
        repo.write_file(&format!("content/{name}"), bytes);
    }
    repo.commit_all("import the garden documents");

    // Step 2: adopt the writing scope and record its starting point.
    let output = margin(
        repo.env(),
        repo.path(),
        &["init", "--scope", "content/", "--json"],
    );
    assert_eq!(exit_of(&output), 0, "init: the README starts here");
    let envelope = envelope_of(&output, "init");
    assert_eq!(envelope["ok"], true);
    let start = envelope["data"]["start_commit"]
        .as_str()
        .unwrap()
        .to_string();

    // Step 3: apply the example edits and capture one draft.
    for name in documents {
        let bytes = &after[name];
        repo.write_file(&format!("content/{name}"), bytes);
    }
    repo.write_file("private-note.md", b"An unrelated note outside the scope.\n");
    for args in [
        vec!["status"],
        vec!["diff", "--working"],
        vec![
            "checkpoint",
            "--message",
            "Revise the garden documents",
            "--json",
        ],
        vec!["status"],
    ] {
        let output = margin(repo.env(), repo.path(), &args);
        assert_eq!(exit_of(&output), 0, "{args:?}: the walkthrough step runs");
    }

    // Step 4: read the whole comparison and build one plan from the
    // identities this run generated (SPEC 16.8: no placeholder strings).
    let output = margin(repo.env(), repo.path(), &["diff", "--json"]);
    let envelope = envelope_of(&output, "diff");
    assert_eq!(envelope["ok"], true, "{envelope}");
    assert_eq!(envelope["data"]["complete"], true, "read every page first");
    let data = &envelope["data"];
    let restore: Vec<String> = data["changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|change| change["path"] == "content/field-notes.md")
        .map(|change| change["change_id"].as_str().unwrap().to_string())
        .collect();
    assert!(
        !restore.is_empty(),
        "the example expects changes in field-notes.md"
    );
    let plan = serde_json::json!({
        "schema_version": 1,
        "request_id": "garden-review-1",
        "review_id": data["review_id"],
        "branch_ref": data["branch_ref"],
        "scope_id": data["scope_id"],
        "baseline_oid": data["baseline_oid"],
        "candidate_oid": data["candidate_oid"],
        "restore": restore,
        "keep_remaining": true,
        "message": "Keep clearer prose and checklist; restore field notes",
    });
    let plan_path = repo.root().join("review.json");
    fs::write(&plan_path, plan.to_string()).unwrap();
    let plan_path = plan_path.to_str().unwrap().to_string();

    // Step 5: preview, apply, and retry the same request.
    let output = margin(
        repo.env(),
        repo.path(),
        &["review", "--input", &plan_path, "--dry-run", "--json"],
    );
    assert_eq!(exit_of(&output), 0, "the dry run previews");
    envelope_of(&output, "the dry run");

    let output = margin(
        repo.env(),
        repo.path(),
        &["review", "--input", &plan_path, "--json"],
    );
    assert_eq!(exit_of(&output), 0, "the application applies");
    let envelope = envelope_of(&output, "the application");
    let commit = envelope["data"]["commit"].as_str().unwrap().to_string();
    assert_eq!(envelope["effects"]["commit"], "created");

    let commits = support::text(&repo.git_ok(["rev-list", "--count", "HEAD"]));
    let output = margin(
        repo.env(),
        repo.path(),
        &["review", "--input", &plan_path, "--json"],
    );
    let envelope = envelope_of(&output, "the retry of the same request");
    assert_eq!(exit_of(&output), 0);
    assert_eq!(
        envelope["data"]["existing_receipt"],
        serde_json::json!(commit)
    );
    assert_eq!(
        support::text(&repo.git_ok(["rev-list", "--count", "HEAD"])),
        commits,
        "the retry reports the receipt and creates nothing"
    );
    let output = margin(repo.env(), repo.path(), &["status", "--json"]);
    let envelope = envelope_of(&output, "status after the review");
    assert_eq!(envelope["data"]["review"], "reviewed");

    // The documented outcomes of the decision itself.
    assert_eq!(repo.read_file("content/article.md"), after["article.md"]);
    assert_eq!(
        repo.read_file("content/checklist.md"),
        after["checklist.md"]
    );
    assert_eq!(
        repo.read_file("content/field-notes.md"),
        before["field-notes.md"]
    );
    let dirty = support::text(&repo.git_ok(["status", "--short"]));
    assert_eq!(dirty, "?? private-note.md", "only the note stays untracked");

    // Step 6: read the history, then restore the original introduction.
    for args in [vec!["log"], vec!["--json", "status"]] {
        let output = margin(repo.env(), repo.path(), &args);
        assert_eq!(exit_of(&output), 0, "{args:?}: the walkthrough step runs");
    }
    let output = margin(
        repo.env(),
        repo.path(),
        &["show", "--path", "content/article.md", "--revision", &start],
    );
    assert_eq!(exit_of(&output), 0, "show reads the starting revision");

    let output = margin(repo.env(), repo.path(), &["status", "--json"]);
    let envelope = envelope_of(&output, "status before the restoration");
    let data = &envelope["data"];
    let restore_plan = serde_json::json!({
        "schema_version": 1,
        "request_id": "garden-restore-1",
        "branch_ref": data["branch"],
        "scope_id": data["scope"]["digest"],
        "candidate_oid": data["head"],
        "path": "content/article.md",
        "revision": start,
        "message": "Restore the original introduction for another pass",
    });
    let restore_path = repo.root().join("restore.json");
    fs::write(&restore_path, restore_plan.to_string()).unwrap();
    let restore_path = restore_path.to_str().unwrap().to_string();

    let output = margin(
        repo.env(),
        repo.path(),
        &["restore", "--input", &restore_path, "--dry-run", "--json"],
    );
    assert_eq!(exit_of(&output), 0, "the restoration previews");
    envelope_of(&output, "the restoration dry run");
    let output = margin(
        repo.env(),
        repo.path(),
        &["restore", "--input", &restore_path, "--json"],
    );
    assert_eq!(exit_of(&output), 0, "the restoration applies");
    let envelope = envelope_of(&output, "the restoration");
    assert_eq!(envelope["effects"]["commit"], "created");
    assert_eq!(repo.read_file("content/article.md"), before["article.md"]);

    // The restored introduction is pending again, exactly as documented.
    let output = margin(repo.env(), repo.path(), &["status", "--json"]);
    let envelope = envelope_of(&output, "the final status");
    assert_eq!(envelope["data"]["review"], "pending");
}
