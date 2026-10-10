//! Installed-binary tests for the CLI envelope and command discovery
//! (SPEC 6, 7, and 16.8, task T015).
//!
//! Every case runs the real executable through an intact pipe with the
//! isolated harness environment, then checks the one JSON envelope, the
//! exit class, and the absence of terminal decoration. These tests cover
//! the interface contract; the operation semantics behind each command
//! arrive with their own tasks.

mod support;

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
    let output = margin(repo.env(), repo.path(), &["hook", "claude"]);
    assert_eq!(exit_of(&output), 4);
    // The hook adapter speaks the upstream hook protocol, not the Margin
    // envelope (SPEC 6.2), so stdout stays empty and stderr stays bounded.
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("OPERATION_UNAVAILABLE"), "got: {stderr}");
    assert!(stderr.lines().count() <= 2);
}

#[test]
fn hook_setup_commands_use_the_envelope() {
    let repo = TestRepo::init();
    for args in [
        vec!["hook", "install", "claude", "--json"],
        vec!["hook", "remove", "claude", "--json"],
    ] {
        let output = margin(repo.env(), repo.path(), &args);
        assert_eq!(exit_of(&output), 4, "{args:?}: pending setup");
        let envelope = envelope_of(&output, "hook setup");
        assert_eq!(code_of(&envelope), "OPERATION_UNAVAILABLE");
    }
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
