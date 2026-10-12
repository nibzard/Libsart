//! Hook installation and removal tests (SPEC 12.4, task T049).
//!
//! Every case runs the built binary against a real Git repository and
//! checks the target configuration with ordinary file reads and stock
//! JSON, so the claims are about the file the agent reads: a dry run
//! reports the exact planned patch, the supported event contract, and the
//! adapter capabilities without writing; an installation checks the
//! fingerprint of the bytes it planned against, preserves every unrelated
//! key and user hook, and creates the file only inside the explicit
//! action; a removal deletes only the entries Margin can identify; and a
//! repeated action in either direction is one clearly reported no-op.

#![cfg(unix)]

mod support;

use std::path::Path;
use std::process::Output;

use serde_json::{Value, json};

use support::repo::TestRepo;

/// The repository-relative file every case inspects.
const TARGET: &str = ".claude/settings.local.json";

/// The command of every entry Margin installs.
const HOOK_COMMAND: &str = "margin hook claude";

/// The supported event contract of the adapter (SPEC 12.2).
const SUPPORTED_EVENTS: &[&str] = &[
    "UserPromptSubmit",
    "Stop",
    "StopFailure",
    "SessionStart",
    "SessionEnd",
];

/// Runs the built binary with the scrubbed environment and no standard
/// input.
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

/// Decodes one JSON envelope the binary printed.
fn envelope_of(output: &Output, what: &str) -> Value {
    assert_eq!(
        output.status.code(),
        Some(0),
        "{what}: the command succeeds: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        output.stderr.is_empty(),
        "{what}: machine mode writes no stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{what}: one envelope: {error}"))
}

/// One user-owned configuration the preservation cases start from: keys
/// Margin must never touch, one user hook on an event Margin also uses,
/// and one user hook on an event outside the supported contract.
fn user_configuration() -> Value {
    json!({
        "permissions": { "allow": ["Bash(git log:*)"] },
        "outputStyle": "plain",
        "hooks": {
            "Stop": [
                { "hooks": [{ "type": "command", "command": "notify.sh" }] }
            ],
            "PreToolUse": [
                { "matcher": "Bash", "hooks": [{ "type": "command", "command": "guard.sh" }] }
            ]
        }
    })
}

/// Writes one starting configuration and returns its exact bytes.
fn seeded(repo: &TestRepo, config: &Value) -> Vec<u8> {
    let bytes = format!("{}\n", serde_json::to_string_pretty(config).unwrap());
    repo.write_file(TARGET, bytes.as_bytes());
    bytes.into()
}

/// Reads the target configuration as one parsed object, or `None` when no
/// file exists.
fn target_config(repo: &TestRepo) -> Option<Value> {
    let path = repo.file_path(TARGET);
    let bytes = std::fs::read(path).ok()?;
    Some(
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|error| panic!("the target is one JSON document: {error}")),
    )
}

/// The commands of one event's hooks, in file order.
fn commands_of(config: &Value, event: &str) -> Vec<String> {
    config["hooks"][event]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|group| {
            group["hooks"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|hook| hook["command"].as_str())
                .map(str::to_string)
                .collect::<Vec<String>>()
        })
        .collect()
}

#[test]
fn dry_run_reports_the_patch_contract_and_capabilities_without_writing() {
    let repo = TestRepo::init();
    let started = seeded(&repo, &user_configuration());

    let output = margin(
        repo.env(),
        repo.path(),
        &["hook", "install", "claude", "--dry-run", "--json"],
    );
    let envelope = envelope_of(&output, "the dry run");
    assert_eq!(envelope["command"], "hook install");
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["effects"]["integration"], "unchanged");
    let data = &envelope["data"];
    assert_eq!(data["action"], "install");
    assert_eq!(data["dry_run"], true);
    assert_eq!(data["target_existed"], true);
    assert!(
        data["fingerprint"]
            .as_str()
            .is_some_and(|value| value.len() == 16),
        "the planned fingerprint is reported: {}",
        data["fingerprint"],
    );

    // The exact planned patch: every supported event gains the one
    // adapter entry, and nothing already present moves.
    let changes = data["changes"].as_array().unwrap();
    let mut events: Vec<&str> = changes
        .iter()
        .map(|change| change["event"].as_str().unwrap())
        .collect();
    let mut expected: Vec<&str> = SUPPORTED_EVENTS.to_vec();
    events.sort_unstable();
    expected.sort_unstable();
    assert_eq!(events, expected, "one row per supported event");
    assert!(changes.iter().all(|change| change["change"] == "add"));
    let planned = &data["planned_config"];
    assert_eq!(planned["permissions"], user_configuration()["permissions"]);
    assert_eq!(planned["outputStyle"], "plain");
    assert_eq!(
        commands_of(planned, "Stop"),
        vec!["notify.sh".to_string(), HOOK_COMMAND.to_string()],
        "the user hook keeps its place beside the new entry",
    );
    assert_eq!(
        commands_of(planned, "PreToolUse"),
        vec!["guard.sh".to_string()],
        "one unsupported event is untouched",
    );

    // The capabilities of the entries the patch installs (SPEC 12.4).
    let capabilities = &data["capabilities"];
    assert_eq!(capabilities["command"], HOOK_COMMAND);
    let mut supported: Vec<&str> = capabilities["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event.as_str().unwrap())
        .collect();
    supported.sort_unstable();
    assert_eq!(supported, expected, "the contract names every event");
    assert_eq!(capabilities["blocking_events"], json!(["UserPromptSubmit"]));
    assert_eq!(
        capabilities["approval"],
        "none: every capture is one unreviewed draft",
    );

    assert_eq!(repo.read_file(TARGET), started, "the dry run wrote nothing",);
}

#[test]
fn install_creates_the_missing_file_with_one_entry_per_event() {
    let repo = TestRepo::init();
    assert!(
        !repo.file_path(".claude").exists(),
        "no directory exists before the explicit action",
    );

    let output = margin(
        repo.env(),
        repo.path(),
        &["hook", "install", "claude", "--json"],
    );
    let envelope = envelope_of(&output, "the installation");
    assert_eq!(envelope["effects"]["integration"], "changed");
    let data = &envelope["data"];
    assert_eq!(data["target_existed"], false);
    assert_eq!(data["fingerprint"], Value::Null);
    assert_eq!(data["no_op"], false);

    let config = target_config(&repo).expect("the file was created");
    for event in SUPPORTED_EVENTS {
        assert_eq!(
            commands_of(&config, event),
            vec![HOOK_COMMAND.to_string()],
            "{event} holds exactly one adapter entry",
        );
    }
    // The written file is the exact planned configuration the dry run of
    // the same repository reports.
    let dry = margin(
        repo.env(),
        repo.path(),
        &["hook", "install", "claude", "--dry-run", "--json"],
    );
    let planned = &envelope_of(&dry, "the later dry run")["data"]["planned_config"];
    assert_eq!(&config, planned, "the dry run names the bytes on disk");

    // Text mode answers in one line.
    let text = margin(repo.env(), repo.path(), &["hook", "remove", "claude"]);
    assert_eq!(text.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&text.stdout),
        format!("margin: removed 5 entries from {TARGET}\n"),
    );
}

#[test]
fn install_preserves_unrelated_keys_and_user_hooks() {
    let repo = TestRepo::init();
    seeded(&repo, &user_configuration());

    let output = margin(
        repo.env(),
        repo.path(),
        &["hook", "install", "claude", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));
    let config = target_config(&repo).expect("the file survives the install");

    assert_eq!(config["permissions"], user_configuration()["permissions"]);
    assert_eq!(config["outputStyle"], "plain");
    assert_eq!(
        commands_of(&config, "Stop"),
        vec!["notify.sh".to_string(), HOOK_COMMAND.to_string()],
        "the user hook survives before the adapter entry",
    );
    assert_eq!(
        commands_of(&config, "PreToolUse"),
        vec!["guard.sh".to_string()],
        "one unsupported event keeps its matcher and hook",
    );
    for event in SUPPORTED_EVENTS {
        assert!(
            commands_of(&config, event)
                .iter()
                .any(|command| command == HOOK_COMMAND),
            "{event} holds the adapter entry",
        );
    }
}

#[test]
fn a_repeated_install_is_one_clear_no_op() {
    let repo = TestRepo::init();
    margin(repo.env(), repo.path(), &["hook", "install", "claude"]);
    let installed = repo.read_file(TARGET);

    let output = margin(
        repo.env(),
        repo.path(),
        &["hook", "install", "claude", "--dry-run", "--json"],
    );
    let envelope = envelope_of(&output, "the repeated dry run");
    assert_eq!(envelope["effects"]["integration"], "unchanged");
    assert_eq!(envelope["data"]["no_op"], true);
    assert_eq!(
        envelope["data"]["changes"].as_array().unwrap().len(),
        0,
        "no entry is planned twice",
    );

    let output = margin(
        repo.env(),
        repo.path(),
        &["hook", "install", "claude", "--json"],
    );
    let envelope = envelope_of(&output, "the repeated install");
    assert_eq!(envelope["effects"]["integration"], "unchanged");
    assert_eq!(envelope["data"]["no_op"], true);
    assert_eq!(
        repo.read_file(TARGET),
        installed,
        "the file keeps its bytes"
    );

    let text = margin(repo.env(), repo.path(), &["hook", "install", "claude"]);
    assert_eq!(text.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&text.stdout),
        "margin: install no-op; every entry is already installed\n",
    );
}

#[test]
fn remove_deletes_only_the_identifiable_margin_entries() {
    let repo = TestRepo::init();
    let mut seeded_config = user_configuration();
    seeded_config["hooks"]["SessionEnd"] =
        json!([{ "hooks": [{ "type": "command", "command": HOOK_COMMAND }] }]);
    seeded(&repo, &seeded_config);

    let output = margin(
        repo.env(),
        repo.path(),
        &["hook", "remove", "claude", "--json"],
    );
    let envelope = envelope_of(&output, "the removal");
    assert_eq!(envelope["effects"]["integration"], "changed");
    assert_eq!(envelope["data"]["no_op"], false);
    assert_eq!(envelope["data"]["changes"].as_array().unwrap().len(), 1);
    assert_eq!(envelope["data"]["changes"][0]["event"], "SessionEnd");
    assert_eq!(envelope["data"]["changes"][0]["change"], "remove");

    let config = target_config(&repo).expect("the file survives the removal");
    assert_eq!(config["permissions"], user_configuration()["permissions"]);
    assert_eq!(config["outputStyle"], "plain");
    assert_eq!(
        commands_of(&config, "Stop"),
        vec!["notify.sh".to_string()],
        "the user hook of the event Margin also used survives",
    );
    assert_eq!(
        commands_of(&config, "PreToolUse"),
        vec!["guard.sh".to_string()],
    );
    for event in SUPPORTED_EVENTS {
        assert!(
            !commands_of(&config, event)
                .iter()
                .any(|command| command == HOOK_COMMAND),
            "{event} holds no adapter entry anymore",
        );
    }
}

#[test]
fn a_repeated_remove_is_one_clear_no_op() {
    let repo = TestRepo::init();
    let started = seeded(&repo, &user_configuration());

    // No Margin entries exist yet, so the first removal is already the
    // documented no-op.
    let output = margin(
        repo.env(),
        repo.path(),
        &["hook", "remove", "claude", "--json"],
    );
    let envelope = envelope_of(&output, "the removal without entries");
    assert_eq!(envelope["effects"]["integration"], "unchanged");
    assert_eq!(envelope["data"]["no_op"], true);
    assert_eq!(repo.read_file(TARGET), started, "nothing moved");

    // A removal of entries followed by its own repeat.
    margin(repo.env(), repo.path(), &["hook", "install", "claude"]);
    margin(repo.env(), repo.path(), &["hook", "remove", "claude"]);
    let removed = repo.read_file(TARGET);
    let output = margin(
        repo.env(),
        repo.path(),
        &["hook", "remove", "claude", "--json"],
    );
    let envelope = envelope_of(&output, "the repeated removal");
    assert_eq!(envelope["effects"]["integration"], "unchanged");
    assert_eq!(envelope["data"]["no_op"], true);
    assert_eq!(repo.read_file(TARGET), removed);

    // A missing file is the same no-op, and nothing is created for it.
    std::fs::remove_file(repo.file_path(TARGET)).expect("the target is removed");
    let output = margin(
        repo.env(),
        repo.path(),
        &["hook", "remove", "claude", "--json"],
    );
    let envelope = envelope_of(&output, "the removal of no file");
    assert_eq!(envelope["data"]["no_op"], true);
    assert_eq!(envelope["data"]["target_existed"], false);
    assert!(
        !repo
            .file_path(".claude")
            .join("settings.local.json")
            .exists(),
        "removal creates no file",
    );
    let text = margin(repo.env(), repo.path(), &["hook", "remove", "claude"]);
    assert_eq!(text.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&text.stdout),
        "margin: remove no-op; no margin entries are installed\n",
    );
}

#[test]
fn install_then_remove_round_trips_the_user_configuration() {
    let repo = TestRepo::init();
    let started = seeded(&repo, &user_configuration());

    margin(repo.env(), repo.path(), &["hook", "install", "claude"]);
    assert!(
        target_config(&repo).is_some_and(|config| config != user_configuration()),
        "the installation changed the file",
    );
    margin(repo.env(), repo.path(), &["hook", "remove", "claude"]);

    // The round trip ends at exactly the user content it began from:
    // every key, every user hook, every matcher (SPEC 12.4).
    let ended = target_config(&repo).expect("the file survives the round trip");
    assert_eq!(ended, user_configuration());
    assert_eq!(
        repo.read_file(TARGET),
        started,
        "the round trip rewrote no byte it did not have to",
    );
}

#[test]
fn an_unusable_configuration_refuses_without_writing() {
    let repo = TestRepo::init();
    for (label, bytes) in [
        ("not one document", &b"{\"hooks\""[..]),
        ("not one object", b"[1, 2, 3]"),
        ("hooks of the wrong type", b"{\"hooks\": 7}"),
        ("one event of the wrong type", b"{\"hooks\": {\"Stop\": 7}}"),
    ] {
        repo.write_file(TARGET, bytes);
        let output = margin(
            repo.env(),
            repo.path(),
            &["hook", "install", "claude", "--json"],
        );
        assert_eq!(
            output.status.code(),
            Some(4),
            "{label}: one unmet precondition",
        );
        let envelope: Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("{label}: one envelope: {error}"));
        assert_eq!(envelope["error"]["code"], "INVALID_SETTINGS", "{label}");
        assert_eq!(
            repo.read_file(TARGET),
            bytes.to_vec(),
            "{label}: the unusable file keeps its bytes",
        );
    }
}

#[test]
fn outside_any_repository_there_is_no_project_to_configure() {
    let repo = TestRepo::init();
    // The private root of the test harness holds no repository of its own.
    let outside = repo.root().to_path_buf();

    for args in [
        vec!["hook", "install", "claude"],
        vec!["hook", "remove", "claude"],
    ] {
        let output = margin(repo.env(), &outside, &args);
        assert_eq!(output.status.code(), Some(4), "{args:?}: no repository");
        assert!(
            output.stdout.is_empty(),
            "{args:?}: text mode writes no stdout",
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("NOT_INITIALIZED"), "{args:?}: {stderr}");
        assert!(
            !outside.join(".claude").exists(),
            "{args:?}: nothing was created outside a repository",
        );
    }
}
