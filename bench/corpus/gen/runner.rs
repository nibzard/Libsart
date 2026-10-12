//! Isolated Git execution for corpus generation (SPEC 16.3, 17.2).
//!
//! The generator writes benchmark fixtures, not product state, so it keeps
//! its own small runner instead of sharing the integration-test harness:
//! corpora must stay buildable by `cargo run --example corpusgen` alone.
//! The isolation rules match the test harness exactly — a private home and
//! global configuration, a scrubbed child environment, deterministic
//! identity, and advancing fixed commit dates — so the same seed always
//! produces the same object identifiers.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Object format of a corpus repository (SPEC 16.5: both are supported).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ObjectFormat {
    /// The traditional format with 40-hex object IDs.
    Sha1,
    /// The newer format with 64-hex object IDs.
    Sha256,
}

impl ObjectFormat {
    /// Value reported by `git rev-parse --show-object-format`.
    pub fn as_str(self) -> &'static str {
        match self {
            ObjectFormat::Sha1 => "sha1",
            ObjectFormat::Sha256 => "sha256",
        }
    }

    /// Parses the `--object-format` spelling.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "sha1" => Some(ObjectFormat::Sha1),
            "sha256" => Some(ObjectFormat::Sha256),
            _ => None,
        }
    }
}

/// Author and committer identity of every corpus commit. The domain is
/// reserved for invalid delivery, matching the integration harness.
const USER_NAME: &str = "Margin Bench Fixture";
const USER_EMAIL: &str = "margin-bench@example.invalid";

/// First corpus commit date: 2026-01-05T00:00:00+00:00. Each later commit
/// advances by one minute, so histories stay ordered while every date stays
/// a pure function of the commit index.
const FIRST_DATE: u64 = 1767667200;

/// The branch every corpus repository adopts.
pub const BRANCH: &str = "main";

/// Inherited variables removed from every child process: the whole `GIT_`
/// namespace plus variables that override the fixture identity.
fn is_scrubbed(key: &OsStr) -> bool {
    key.as_encoded_bytes().starts_with(b"GIT_") || key == OsStr::new("EMAIL")
}

/// One generated repository under `root`:
///
/// ```text
/// <root>/repo       the working repository
/// <root>/gen-home   private HOME with the fixture global configuration
/// ```
pub struct Runner {
    repo: PathBuf,
    env: BTreeMap<OsString, OsString>,
    commit_index: u64,
}

impl Runner {
    /// Creates `repo` with the requested object format, the adopted branch,
    /// and the fixture identity, then returns the runner for it.
    pub fn init(root: &Path, format: ObjectFormat) -> Self {
        let home = root.join("gen-home");
        fs::create_dir_all(&home)
            .unwrap_or_else(|error| panic!("cannot create {}: {error}", home.display()));
        let repo = root.join("repo");
        fs::create_dir_all(&repo)
            .unwrap_or_else(|error| panic!("cannot create {}: {error}", repo.display()));

        let mut env = BTreeMap::new();
        for (key, value) in std::env::vars_os() {
            if !is_scrubbed(&key) {
                env.insert(key, value);
            }
        }
        let mut set = |key: &str, value: OsString| {
            env.insert(OsString::from(key), value);
        };
        set("HOME", home.as_os_str().to_owned());
        set("XDG_CONFIG_HOME", home.join(".config").into_os_string());
        set(
            "GIT_CONFIG_GLOBAL",
            home.join(".gitconfig").into_os_string(),
        );
        set("GIT_CONFIG_NOSYSTEM", OsString::from("1"));
        set("GIT_TERMINAL_PROMPT", OsString::from("0"));
        set("TZ", OsString::from("UTC"));
        set("LC_ALL", OsString::from("C"));

        let runner = Runner {
            repo,
            env,
            commit_index: 0,
        };
        let global = format!(
            "[user]\n\tname = {USER_NAME}\n\temail = {USER_EMAIL}\n\
             [commit]\n\tgpgsign = false\n[core]\n\tautocrlf = false\n\tfileMode = true\n\
             [init]\n\tdefaultBranch = {BRANCH}\n[gc]\n\tauto = 0\n"
        );
        fs::write(home.join(".gitconfig"), global)
            .unwrap_or_else(|error| panic!("cannot write the fixture gitconfig: {error}"));
        runner.ok([
            "init",
            "--quiet",
            &format!("--object-format={}", format.as_str()),
            &format!("--initial-branch={BRANCH}"),
        ]);
        runner
    }

    /// The repository working tree.
    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// Builds one Git command with the isolated environment.
    fn command<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new("git");
        command.current_dir(&self.repo).args(args);
        for (key, _) in std::env::vars_os() {
            if is_scrubbed(&key) {
                command.env_remove(&key);
            }
        }
        command.envs(&self.env);
        command.stdin(Stdio::null());
        command
    }

    /// Runs Git and returns the raw process output without asserting the
    /// exit status, for the deliberately failing fixture steps.
    pub fn run<I, S>(&self, args: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.command(args)
            .output()
            .unwrap_or_else(|error| panic!("cannot run git in {}: {error}", self.repo.display()))
    }

    /// Runs Git, asserts a zero exit, and returns stdout.
    pub fn ok<I, S>(&self, args: I) -> Vec<u8>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "git failed with exit {} in {}\nstderr: {}",
            output.status,
            self.repo.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    /// Runs Git with `input` on stdin, asserts a zero exit, and returns
    /// stdout. NUL-delimited path protocols use this path.
    pub fn feed<I, S>(&self, args: I, input: &[u8]) -> Vec<u8>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = self.command(args);
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut child = command.spawn().expect("cannot spawn git");
        use std::io::Write as _;
        child
            .stdin
            .take()
            .expect("git stdin")
            .write_all(input)
            .expect("cannot feed git stdin");
        let output = child.wait_with_output().expect("cannot wait for git");
        assert!(
            output.status.success(),
            "git failed with exit {} in {}\nstderr: {}",
            output.status,
            self.repo.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    /// Decoded stdout without the trailing newline.
    pub fn text<I, S>(&self, args: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        String::from_utf8_lossy(&self.ok(args)).trim().to_string()
    }

    /// Writes raw bytes at a repository-relative path.
    pub fn write(&self, relative: &str, bytes: &[u8]) {
        let path = self.repo.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("cannot create {}: {error}", parent.display()));
        }
        fs::write(&path, bytes)
            .unwrap_or_else(|error| panic!("cannot write {}: {error}", path.display()));
    }

    /// Removes a repository-relative file.
    pub fn remove(&self, relative: &str) {
        let path = self.repo.join(relative);
        fs::remove_file(&path)
            .unwrap_or_else(|error| panic!("cannot remove {}: {error}", path.display()));
    }

    /// Sets one repository-local configuration entry.
    pub fn config(&self, key: &str, value: &str) {
        self.ok(["config", "--local", key, value]);
    }

    /// Commits the current index with an exact message, advancing the fixed
    /// fixture clock by one minute per commit. `allow_empty` permits the
    /// empty review commits of a keep-all review (SPEC 9.4). Returns the
    /// new HEAD object ID.
    pub fn commit_message(&mut self, message: &[u8], allow_empty: bool) -> String {
        let stamp = FIRST_DATE + self.commit_index * 60;
        self.commit_index += 1;
        let date = format!("@{stamp} +0000");
        let message_file = self.repo.join(format!(".gen-message-{stamp}"));
        fs::write(&message_file, message)
            .unwrap_or_else(|error| panic!("cannot write {}: {error}", message_file.display()));
        let mut argv: Vec<String> = vec![
            "commit".into(),
            "--quiet".into(),
            format!("--date={date}"),
            "--file".into(),
            message_file
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned(),
        ];
        if allow_empty {
            argv.push("--allow-empty".into());
        }
        // The committer date comes from the environment, the author date
        // from --date, so both clocks stay under the generator's control.
        let output = {
            let mut command = self.command(&argv);
            command.env_remove("GIT_COMMITTER_DATE");
            command.env_remove("GIT_AUTHOR_DATE");
            command.env("GIT_COMMITTER_DATE", &date);
            command.env("GIT_AUTHOR_DATE", &date);
            command.output().expect("cannot spawn git commit")
        };
        let _ = fs::remove_file(&message_file);
        assert!(
            output.status.success(),
            "git commit failed with exit {} in {}\nstderr: {}",
            output.status,
            self.repo.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        self.text(["rev-parse", "HEAD"])
    }

    /// The current HEAD object ID.
    pub fn head(&self) -> String {
        self.text(["rev-parse", "HEAD"])
    }

    /// The tree of `revision`.
    pub fn tree_of(&self, revision: &str) -> String {
        self.text(["rev-parse", &format!("{revision}^{{tree}}")])
    }

    /// Hashes `bytes` through Git's own object-hash interface without
    /// storing them (SPEC 8.6), under this repository's object format.
    pub fn hash_object(&self, bytes: &[u8]) -> String {
        String::from_utf8_lossy(&self.feed(["hash-object", "--stdin"], bytes))
            .trim()
            .to_string()
    }
}
