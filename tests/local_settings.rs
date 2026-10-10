//! Real-Git tests for the local Margin settings (SPEC 6.2, 8.2, task T016).
//!
//! Every case proves one contract of the settings engine against the
//! installed Git:
//!
//! - the documented `margin.*` keys store branch, scope, capture policy,
//!   and editor argv, each in its documented form, through Git's own
//!   configuration interface
//! - one explicit update writes only the settings it names: unrelated
//!   configuration entries and omitted settings keep their values
//! - first use reads both capture policies as off and writes nothing
//! - a value outside its documented form, supplied or stored, fails typed
//!   without writing
//! - the stored scope re-enters through the one resolver, and the shared
//!   queries and mutations adopt it
//!
//! Raw configuration values are checked through stock Git, never through
//! Margin's own reader (SPEC 16.3).

mod support;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::process::{Command, Output, Stdio};

use margin::app::{
    Effects, InspectRequest, ObservedEffect, OperationOutcome, Operations, ReviewRequest,
    resolve_scope,
};
use margin::domain::{LocalSettings, SETTINGS_KEY_SCOPE, ScopeInput, SettingsUpdate};
use margin::git::Git;
use serde_json::Value;
use support::env::is_scrubbed;
use support::repo::TestRepo;
use support::{oracle, text};

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

/// One update that names every capture-related setting.
fn full_update(descriptor: &margin::domain::ScopeDescriptor) -> SettingsUpdate {
    SettingsUpdate {
        branch: Some("writing".to_string()),
        scope: Some(descriptor.clone()),
        auto_checkpoint: Some(true),
        capture_after_edit: Some(false),
        editor_argv: Some(vec!["nvim".to_string(), "--".to_string()]),
    }
}

/// The raw stored value of one key, read through stock Git. `None` means
/// the key is unset.
fn raw_value(repo: &TestRepo, key: &str) -> Option<String> {
    let output = repo.git(["config", "--get", key]);
    if output.status.success() {
        Some(text(&output.stdout))
    } else {
        None
    }
}

/// Every repository-local configuration entry, read through stock Git.
/// This is the independent oracle for what an update preserved.
fn local_config(repo: &TestRepo) -> BTreeMap<String, String> {
    let stdout = repo.git_ok(["config", "--list", "--local"]);
    let mut entries = BTreeMap::new();
    for line in String::from_utf8_lossy(&stdout).lines() {
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once('=').unwrap_or((line, ""));
        entries.insert(key.to_string(), value.to_string());
    }
    entries
}

/// Asserts one typed refusal: not successful, one primary code, and the
/// unchanged effects of an operation that wrote nothing.
fn assert_refusal<T>(result: &OperationOutcome<T>, code: &str, what: &str) {
    assert!(!result.ok(), "{what} must not succeed");
    assert_eq!(
        result.error.as_ref().unwrap().code,
        code,
        "{what} reports the wrong code"
    );
    assert_eq!(result.effects, Effects::unchanged(), "{what} wrote nothing");
}

/// Runs the installed executable once with the isolated environment.
fn margin<I, S>(repo: &TestRepo, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new(env!("CARGO_BIN_EXE_margin"));
    command
        .args(args)
        .current_dir(repo.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, _) in std::env::vars_os() {
        if is_scrubbed(&key) {
            command.env_remove(&key);
        }
    }
    command.envs(repo.env().vars());
    command
        .output()
        .unwrap_or_else(|error| panic!("cannot run the margin binary: {error}"))
}

/// The canonical scope descriptor of the scope rooted at `notes`.
fn notes_descriptor(repo: &TestRepo) -> margin::domain::ScopeDescriptor {
    let git = adapter_for(repo);
    resolve_scope(&git, &scope_input(&["notes"], &[], &[".md"]))
        .unwrap_or_else(|error| panic!("the scope must resolve: {error}"))
        .descriptor
}

#[test]
fn first_use_reads_off_defaults_and_writes_nothing() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);

    let before = oracle::snapshot(&repo);
    let result = operations.read_settings();

    assert!(result.ok(), "reading an untouched repository succeeds");
    let data = result.data.as_ref().unwrap();
    // First use: nothing is stored and both capture policies are off
    // (SPEC 6.2).
    assert_eq!(data.settings, LocalSettings::first_use());
    assert!(data.applied.is_empty(), "a read applies nothing");
    assert_eq!(result.effects, Effects::unchanged());
    assert!(
        !local_config(&repo)
            .keys()
            .any(|key| key.starts_with("margin.")),
        "no margin.* key exists"
    );
    before.assert_unchanged(&oracle::snapshot(&repo), "read settings");
}

#[test]
fn updates_store_the_documented_keys_in_their_documented_forms() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    let descriptor = notes_descriptor(&repo);
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);

    let result = operations.update_settings(&full_update(&descriptor));

    assert!(result.ok(), "the update must succeed");
    let data = result.data.as_ref().unwrap();
    let keys: Vec<&str> = data.applied.iter().map(|a| a.key.as_str()).collect();
    assert_eq!(
        keys,
        vec![
            "margin.version",
            "margin.branch",
            "margin.scope",
            "margin.autoCheckpoint",
            "margin.captureAfterEdit",
            "margin.editorArgv",
        ],
        "the first write stamps the settings contract version"
    );

    // Stock Git sees every documented key in its documented form.
    let scope_json = String::from_utf8(descriptor.canonical_bytes()).unwrap();
    assert_eq!(raw_value(&repo, "margin.version").as_deref(), Some("1"));
    assert_eq!(
        raw_value(&repo, "margin.branch").as_deref(),
        Some("writing")
    );
    assert_eq!(
        raw_value(&repo, "margin.scope").as_deref(),
        Some(scope_json.as_str())
    );
    assert_eq!(
        raw_value(&repo, "margin.autoCheckpoint").as_deref(),
        Some("true")
    );
    assert_eq!(
        raw_value(&repo, "margin.captureAfterEdit").as_deref(),
        Some("false")
    );
    assert_eq!(
        raw_value(&repo, "margin.editorArgv").as_deref(),
        Some("[\"nvim\",\"--\"]")
    );

    // The read path returns the typed settings those bytes encode.
    let read = operations.read_settings();
    assert!(read.ok());
    let settings = &read.data.as_ref().unwrap().settings;
    assert_eq!(settings.version, Some(1));
    assert_eq!(settings.branch.as_deref(), Some("writing"));
    assert_eq!(settings.scope.as_ref(), Some(&descriptor));
    assert!(settings.auto_checkpoint);
    assert!(!settings.capture_after_edit);
    assert_eq!(
        settings.editor_argv,
        Some(vec!["nvim".to_string(), "--".to_string()])
    );

    // Only the local configuration changed.
    assert_eq!(result.effects.configuration, ObservedEffect::Changed);
    assert_eq!(result.effects.working_tree, ObservedEffect::Unchanged);
    assert_eq!(result.effects.index, ObservedEffect::Unchanged);
    assert_eq!(
        result.effects.commit,
        margin::app::CommitEffect::None,
        "a settings update commits nothing"
    );
}

#[test]
fn explicit_updates_preserve_unrelated_entries_and_omitted_settings() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    let descriptor = notes_descriptor(&repo);
    // Unrelated local configuration a write must retain (SPEC 8.2).
    repo.set_local_config("user.signingkey", "kept-key");
    repo.set_local_config("mytool.setting", "kept as well");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    assert!(operations.update_settings(&full_update(&descriptor)).ok());

    // The second update names only the branch.
    let second = SettingsUpdate {
        branch: Some("drafts".to_string()),
        ..SettingsUpdate::default()
    };
    let result = operations.update_settings(&second);

    assert!(result.ok(), "the narrowed update must succeed");
    let data = result.data.as_ref().unwrap();
    let keys: Vec<&str> = data.applied.iter().map(|a| a.key.as_str()).collect();
    assert_eq!(
        keys,
        vec!["margin.branch"],
        "only the named setting is written"
    );

    // Every omitted setting kept its value.
    assert_eq!(raw_value(&repo, "margin.branch").as_deref(), Some("drafts"));
    let scope_json = String::from_utf8(descriptor.canonical_bytes()).unwrap();
    assert_eq!(
        raw_value(&repo, "margin.scope").as_deref(),
        Some(scope_json.as_str())
    );
    assert_eq!(
        raw_value(&repo, "margin.autoCheckpoint").as_deref(),
        Some("true")
    );
    assert_eq!(
        raw_value(&repo, "margin.captureAfterEdit").as_deref(),
        Some("false")
    );
    assert_eq!(
        raw_value(&repo, "margin.editorArgv").as_deref(),
        Some("[\"nvim\",\"--\"]")
    );
    // The already-stamped version is not rewritten.
    assert_eq!(raw_value(&repo, "margin.version").as_deref(), Some("1"));

    // Unrelated entries survived both writes.
    let config = local_config(&repo);
    assert_eq!(
        config.get("user.signingkey").map(String::as_str),
        Some("kept-key")
    );
    assert_eq!(
        config.get("mytool.setting").map(String::as_str),
        Some("kept as well")
    );
    assert_eq!(result.effects.configuration, ObservedEffect::Changed);
}

#[test]
fn first_use_capture_policies_stay_off_until_explicitly_set() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    let descriptor = notes_descriptor(&repo);
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);

    // An update that names no capture policy leaves both off (SPEC 6.2).
    let scoped = SettingsUpdate {
        scope: Some(descriptor),
        ..SettingsUpdate::default()
    };
    let result = operations.update_settings(&scoped);
    assert!(result.ok());
    let settings = &result.data.as_ref().unwrap().settings;
    assert!(!settings.auto_checkpoint, "first use captures nothing");
    assert!(!settings.capture_after_edit, "first use captures nothing");
    assert_eq!(raw_value(&repo, "margin.autoCheckpoint"), None);
    assert_eq!(raw_value(&repo, "margin.captureAfterEdit"), None);

    // Capturing starts only through an explicit update.
    let opt_in = SettingsUpdate {
        auto_checkpoint: Some(true),
        ..SettingsUpdate::default()
    };
    let result = operations.update_settings(&opt_in);
    assert!(result.ok());
    assert_eq!(
        raw_value(&repo, "margin.autoCheckpoint").as_deref(),
        Some("true")
    );
    assert!(
        result.data.as_ref().unwrap().settings.auto_checkpoint,
        "the effective policy follows the stored value"
    );
    // The other policy stays off and unset.
    assert_eq!(raw_value(&repo, "margin.captureAfterEdit"), None);
}

#[test]
fn invalid_updates_change_nothing() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    let descriptor = notes_descriptor(&repo);
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);

    let mut unsorted = descriptor.clone();
    unsorted.include = vec!["zzz/".to_string(), "aaa/".to_string()];
    let invalid = [
        (
            "a branch with a line break",
            SettingsUpdate {
                branch: Some("two\nlines".to_string()),
                ..SettingsUpdate::default()
            },
        ),
        (
            "an empty editor argv",
            SettingsUpdate {
                editor_argv: Some(Vec::new()),
                ..SettingsUpdate::default()
            },
        ),
        (
            "a non-canonical descriptor",
            SettingsUpdate {
                scope: Some(unsorted),
                ..SettingsUpdate::default()
            },
        ),
    ];
    for (what, update) in invalid {
        let result = operations.update_settings(&update);
        assert_refusal(&result, "INVALID_ARGUMENT", what);
        assert!(
            result.error.as_ref().unwrap().message.contains("margin."),
            "{what}: the message names the setting"
        );
        assert!(
            !local_config(&repo)
                .keys()
                .any(|key| key.starts_with("margin.")),
            "{what}: nothing was written"
        );
    }
}

#[test]
fn stored_values_outside_the_documented_form_fail_typed() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    repo.set_local_config("margin.autoCheckpoint", "yes");

    let read = operations.read_settings();
    assert_refusal(
        &read,
        "INVALID_SETTINGS",
        "a stored flag with a wrong spelling",
    );
    assert!(
        read.error
            .as_ref()
            .unwrap()
            .message
            .contains("margin.autoCheckpoint"),
        "the message names the key"
    );

    // The shared queries refuse on the same stored value rather than
    // guessing a meaning for it.
    let status = operations.inspect_workspace(&InspectRequest {
        scope: None,
        path_filter: Vec::new(),
    });
    assert_refusal(&status, "INVALID_SETTINGS", "status with unusable settings");

    // Supplying the broken setting repairs it; an update that keeps the
    // broken value cannot report the effective settings and refuses.
    let keeps = SettingsUpdate {
        branch: Some("writing".to_string()),
        ..SettingsUpdate::default()
    };
    assert_refusal(
        &operations.update_settings(&keeps),
        "INVALID_SETTINGS",
        "an update that keeps a broken flag",
    );
    let repair = SettingsUpdate {
        auto_checkpoint: Some(false),
        ..SettingsUpdate::default()
    };
    let result = operations.update_settings(&repair);
    assert!(result.ok(), "supplying the setting repairs it");
    assert_eq!(
        raw_value(&repo, "margin.autoCheckpoint").as_deref(),
        Some("false")
    );

    // A broken stored scope behaves the same way.
    repo.set_local_config(SETTINGS_KEY_SCOPE, "{ not a descriptor");
    let keeps = SettingsUpdate {
        branch: Some("writing".to_string()),
        ..SettingsUpdate::default()
    };
    assert_refusal(
        &operations.update_settings(&keeps),
        "INVALID_SETTINGS",
        "an update that keeps a broken scope",
    );
    let repair = SettingsUpdate {
        scope: Some(notes_descriptor(&repo)),
        ..SettingsUpdate::default()
    };
    assert!(
        operations.update_settings(&repair).ok(),
        "a new scope repairs it"
    );
}

#[test]
fn an_unsupported_settings_version_blocks_reads_and_updates() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    repo.set_local_config("margin.version", "2");

    let read = operations.read_settings();
    assert_refusal(&read, "INVALID_SETTINGS", "a newer settings contract");
    assert!(
        read.error
            .as_ref()
            .unwrap()
            .message
            .contains("margin.version"),
        "the message names the version key"
    );

    let update = SettingsUpdate {
        branch: Some("writing".to_string()),
        ..SettingsUpdate::default()
    };
    let result = operations.update_settings(&update);
    assert_refusal(
        &result,
        "INVALID_SETTINGS",
        "an update over a newer contract",
    );
    assert_eq!(
        raw_value(&repo, "margin.branch"),
        None,
        "nothing was written and nothing was downgraded"
    );
    assert_eq!(raw_value(&repo, "margin.version").as_deref(), Some("2"));
}

#[test]
fn the_stored_scope_reenters_the_shared_operations() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.write_file("other.txt", b"out of scope\n");
    repo.commit_all("import");
    let descriptor = notes_descriptor(&repo);
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    let stored = SettingsUpdate {
        scope: Some(descriptor.clone()),
        ..SettingsUpdate::default()
    };
    assert!(operations.update_settings(&stored).ok());

    // A query with no explicit scope adopts the stored one and resolves it
    // through the one resolver: the digest equals the explicit resolution.
    let explicit = resolve_scope(&git, &scope_input(&["notes"], &[], &[".md"])).unwrap();
    let status = operations.inspect_workspace(&InspectRequest {
        scope: None,
        path_filter: Vec::new(),
    });
    assert!(status.ok(), "the stored scope anchors the query");
    let data = status.data.as_ref().unwrap();
    assert_eq!(data.scope.digest, explicit.digest.id());
    assert_eq!(data.scope.tracked, 1, "notes/a.md is the one tracked file");
    assert_eq!(data.settings.scope.as_ref(), Some(&descriptor));

    // A mutation with no explicit scope adopts the stored one and runs the
    // shared sequence instead of refusing the uninitialized state: the
    // plan parses, and the first-parent walk then finds no valid anchor in
    // this repository, so the typed refusal is the unavailable baseline —
    // never `NOT_INITIALIZED` and never a silent second scope. A
    // checkpoint through the stored scope is covered by the checkpoint
    // suite.
    let review = operations.apply_review(&ReviewRequest {
        scope: None,
        plan: br#"{"schema_version":1,"request_id":"pending-01","review_id":"sha1:0000000000000000000000000000000000000000","branch_ref":"refs/heads/main","scope_id":"sha1:0000000000000000000000000000000000000000","baseline_oid":"0000000000000000000000000000000000000000","candidate_oid":"0000000000000000000000000000000000000000","restore":[],"keep_remaining":true}"#.to_vec(),
        dry_run: false,
    });
    assert_refusal(
        &review,
        "BASELINE_UNAVAILABLE",
        "review application through the stored scope",
    );

    // Without a stored scope the typed uninitialized state remains.
    let fresh = TestRepo::init();
    fresh.write_file("notes/a.md", b"# One\n");
    let fresh_git = adapter_for(&fresh);
    let fresh_operations = Operations::new(&fresh_git);
    let status = fresh_operations.inspect_workspace(&InspectRequest {
        scope: None,
        path_filter: Vec::new(),
    });
    assert_refusal(
        &status,
        "NOT_INITIALIZED",
        "a repository without a stored scope",
    );
    let error = status.error.as_ref().unwrap();
    assert_eq!(
        error.recovery_argv,
        vec![
            "margin".to_string(),
            "init".to_string(),
            "--json".to_string()
        ]
    );
}

#[test]
fn a_failed_configuration_write_reports_honest_effects() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    // A stale configuration lock, as another concurrent writer leaves
    // behind, makes Git refuse the write while reads keep working.
    let lock = repo.path().join(".git/config.lock");
    fs::write(&lock, b"").unwrap();

    let update = SettingsUpdate {
        branch: Some("writing".to_string()),
        ..SettingsUpdate::default()
    };
    let result = operations.update_settings(&update);

    assert!(!result.ok(), "a locked configuration file blocks the write");
    assert_eq!(result.error.as_ref().unwrap().code, "GIT_FAILED");
    assert!(
        result
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("could not lock config file"),
        "the diagnosis carries Git's own refusal"
    );
    // The failed process wrote nothing, so the observation stays unchanged.
    assert_eq!(result.effects, Effects::unchanged());
    assert_eq!(raw_value(&repo, "margin.branch"), None);

    // Remove the stale lock so the temporary tree cleans up normally.
    fs::remove_file(&lock).unwrap();
}

#[test]
fn status_reports_the_effective_settings_without_secrets() {
    let repo = TestRepo::init();
    repo.write_file("notes/a.md", b"# One\n");
    repo.commit_all("import");
    let descriptor = notes_descriptor(&repo);
    let git = adapter_for(&repo);
    let operations = Operations::new(&git);
    assert!(operations.update_settings(&full_update(&descriptor)).ok());

    let output = margin(&repo, ["status", "--json"]);
    assert_eq!(output.status.code(), Some(0), "status succeeds");
    assert!(output.stderr.is_empty(), "machine mode writes no stderr");
    let envelope: Value = serde_json::from_str(&text(&output.stdout)).expect("one JSON envelope");
    assert_eq!(envelope["ok"], true);
    let settings = &envelope["data"]["settings"];
    assert_eq!(settings["version"], 1);
    assert_eq!(settings["branch"], "writing");
    assert_eq!(settings["scope"]["include"], serde_json::json!(["notes/"]));
    assert_eq!(settings["auto_checkpoint"], true);
    assert_eq!(settings["capture_after_edit"], false);
    assert_eq!(settings["editor_argv"], serde_json::json!(["nvim", "--"]));
    assert_eq!(envelope["effects"]["configuration"], "unchanged");
}
