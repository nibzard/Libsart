//! Test-only home directory, child environment, and tool version records.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Author and committer name for every fixture commit.
pub const FIXTURE_USER_NAME: &str = "Margin Test Fixture";

/// Author and committer mailbox, under a domain that can never be delivered.
pub const FIXTURE_USER_EMAIL: &str = "margin-fixture@example.invalid";

/// Fixed date for every fixture commit: 2026-01-01T00:00:00+00:00. Equal
/// trees, parents, messages, identity, and dates produce equal commit object
/// IDs in every run.
pub const FIXTURE_DATE: &str = "@1767225600 +0000";

/// Initial branch of every fixture repository.
pub const DEFAULT_BRANCH: &str = "main";

/// Inherited variables that override the fixture identity but do not start
/// with the `GIT_` prefix covered by [`isolated_vars`].
const SCRUBBED_VARS: &[&str] = &["EMAIL"];

/// Says whether an inherited variable is removed from child processes.
pub(crate) fn is_scrubbed(key: &OsStr) -> bool {
    key.as_encoded_bytes().starts_with(b"GIT_")
        || SCRUBBED_VARS.iter().any(|name| key == OsStr::new(name))
}

/// Copies `parent`, removes every variable that could redirect Git to another
/// repository or override the fixture identity, and adds the test-only
/// settings for `home`.
///
/// SPEC 14.1 requires rejecting inherited repository-redirection variables
/// such as `GIT_DIR`, `GIT_WORK_TREE`, and `GIT_INDEX_FILE`. The harness
/// removes them so tests stay hermetic.
pub fn isolated_vars(
    parent: impl IntoIterator<Item = (OsString, OsString)>,
    home: &Path,
) -> BTreeMap<OsString, OsString> {
    let mut vars = BTreeMap::new();
    for (key, value) in parent {
        if !is_scrubbed(&key) {
            vars.insert(key, value);
        }
    }
    let mut set = |key: &str, value: OsString| {
        vars.insert(OsString::from(key), value);
    };
    set("HOME", home.as_os_str().to_owned());
    set("XDG_CONFIG_HOME", home.join(".config").into_os_string());
    set(
        "GIT_CONFIG_GLOBAL",
        home.join(".gitconfig").into_os_string(),
    );
    set("GIT_CONFIG_NOSYSTEM", OsString::from("1"));
    set("GIT_TERMINAL_PROMPT", OsString::from("0"));
    set("GIT_OPTIONAL_LOCKS", OsString::from("0"));
    set("GIT_NO_LAZY_FETCH", OsString::from("1"));
    set("GIT_AUTHOR_DATE", OsString::from(FIXTURE_DATE));
    set("GIT_COMMITTER_DATE", OsString::from(FIXTURE_DATE));
    set("TZ", OsString::from("UTC"));
    set("LC_ALL", OsString::from("C"));
    vars
}

/// A test-only home directory plus the child environment derived from it.
pub struct IsolatedEnv {
    home: PathBuf,
    vars: BTreeMap<OsString, OsString>,
}

impl IsolatedEnv {
    /// Builds the environment for `home` from the current environment.
    pub fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            vars: isolated_vars(std::env::vars_os(), home),
        }
    }

    /// The test-only home directory.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The global configuration file that every child Git reads.
    pub fn global_config(&self) -> PathBuf {
        self.home.join(".gitconfig")
    }

    /// The environment map applied to every child Git process.
    pub fn vars(&self) -> &BTreeMap<OsString, OsString> {
        &self.vars
    }
}

/// Tool versions of the machine that runs the tests (SPEC 16.3).
pub struct EnvironmentReport {
    /// Output of `git --version` without its `git version ` prefix.
    pub git_version: String,
    /// Operating system name, such as `linux` or `macos`.
    pub os: &'static str,
    /// CPU architecture name, such as `x86_64`.
    pub arch: &'static str,
    /// Kernel release, or `unknown` when it cannot be read.
    pub kernel: String,
}

/// Returns the environment record, querying Git once per test process.
pub fn environment_report() -> &'static EnvironmentReport {
    static REPORT: OnceLock<EnvironmentReport> = OnceLock::new();
    REPORT.get_or_init(|| EnvironmentReport {
        git_version: git_version(),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        kernel: kernel_release(),
    })
}

fn git_version() -> String {
    let output = Command::new("git")
        .arg("--version")
        .output()
        .unwrap_or_else(|error| panic!("cannot run git --version: {error}"));
    assert!(
        output.status.success(),
        "git --version failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    raw.strip_prefix("git version ")
        .unwrap_or(&raw)
        .trim()
        .to_string()
}

/// Reads the version triple out of a raw `git --version` string, so tests can
/// require a minimum version.
pub fn parse_git_version(raw: &str) -> Option<(u32, u32, u32)> {
    let digits: String = raw
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = digits.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

fn kernel_release() -> String {
    if let Ok(release) = fs::read_to_string("/proc/sys/kernel/osrelease") {
        return release.trim().to_string();
    }
    Command::new("uname")
        .arg("-r")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Writes the environment record as JSON, so a test run can be attributed to
/// the exact Git and OS versions that produced it.
pub fn write_environment_record(report: &EnvironmentReport, path: &Path) {
    let record = serde_json::json!({
        "schema_version": 1,
        "git_version": report.git_version,
        "os": report.os,
        "arch": report.arch,
        "kernel": report.kernel,
    });
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .unwrap_or_else(|error| panic!("cannot create {}: {error}", parent.display()));
    }
    let body = serde_json::to_string_pretty(&record)
        .unwrap_or_else(|error| panic!("cannot encode the environment record: {error}"));
    fs::write(path, body)
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", path.display()));
}
