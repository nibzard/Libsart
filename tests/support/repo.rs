//! Fresh temporary repositories and the Git runner for integration tests.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

use super::env::{DEFAULT_BRANCH, FIXTURE_USER_EMAIL, FIXTURE_USER_NAME, IsolatedEnv, is_scrubbed};

/// Object format of a fixture repository. SPEC 16.5 requires both formats.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ObjectFormat {
    /// The traditional format with 40-hex-digit object IDs.
    #[default]
    Sha1,
    /// The newer format with 64-hex-digit object IDs.
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
}

/// Options describing one fixture repository.
///
/// The defaults follow the supported mutation envelope in SPEC 3.1 and 8.8:
/// no line-ending conversion, no signing, and no background garbage
/// collection. Tests that need an opt-in custom configuration pass it here,
/// so every deviation lives in its own dedicated variant (SPEC 16.3).
#[derive(Clone, Debug, Default)]
pub struct RepoOptions {
    object_format: Option<ObjectFormat>,
    initial_branch: Option<String>,
    local_config: Vec<(String, String)>,
    global_config: Vec<(String, String)>,
}

impl RepoOptions {
    /// Returns the default options: SHA-1 on the default branch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Selects the repository object format.
    pub fn with_object_format(mut self, format: ObjectFormat) -> Self {
        self.object_format = Some(format);
        self
    }

    /// Names the initial branch instead of `main`.
    pub fn with_initial_branch(mut self, branch: &str) -> Self {
        self.initial_branch = Some(branch.to_string());
        self
    }

    /// Adds one repository-local configuration entry.
    pub fn with_local_config(mut self, key: &str, value: &str) -> Self {
        self.local_config.push((key.to_string(), value.to_string()));
        self
    }

    /// Adds one entry to the test-only global configuration.
    pub fn with_global_config(mut self, key: &str, value: &str) -> Self {
        self.global_config
            .push((key.to_string(), value.to_string()));
        self
    }
}

/// Configuration written to the test-only global file for every repository.
const GLOBAL_CONFIG: &[(&str, &str)] = &[
    ("user.name", FIXTURE_USER_NAME),
    ("user.email", FIXTURE_USER_EMAIL),
    ("init.defaultBranch", DEFAULT_BRANCH),
    ("commit.gpgsign", "false"),
    ("core.autocrlf", "false"),
    ("gc.auto", "0"),
];

/// Builds a Git command with the isolated environment for `cwd`.
fn git_command(env: &IsolatedEnv, cwd: &Path, args: &[OsString]) -> Command {
    let mut command = Command::new("git");
    command.current_dir(cwd).args(args);
    for (key, _) in std::env::vars_os() {
        if is_scrubbed(&key) {
            command.env_remove(&key);
        }
    }
    command.envs(env.vars());
    command.stdin(Stdio::null());
    command
}

/// Renders arguments for an assertion message.
fn describe(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Runs Git and asserts a zero exit, returning stdout.
fn run_checked(env: &IsolatedEnv, cwd: &Path, args: &[OsString], what: &str) -> Vec<u8> {
    let output = git_command(env, cwd, args)
        .output()
        .unwrap_or_else(|error| panic!("cannot run git to {what}: {error}"));
    assert!(
        output.status.success(),
        "cannot {what}: git {} exited with {} in {}\nstderr: {}",
        describe(args),
        output.status,
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// Applies one configuration entry through Git's own config interface.
fn set_config(env: &IsolatedEnv, cwd: &Path, scope: &[OsString], key: &str, value: &str) {
    let mut args: Vec<OsString> = vec!["config".into()];
    args.extend(scope.iter().cloned());
    args.push(key.into());
    args.push(value.into());
    run_checked(env, cwd, &args, "set configuration");
}

/// One fresh temporary repository together with its isolated environment.
///
/// The value owns one private directory tree, so each independent case gets
/// its own repository, home, and configuration (SPEC 16.3):
///
/// ```text
/// <root>/home   test-only HOME with its own global Git configuration
/// <root>/repo   the working repository
/// ```
///
/// Dropping the value removes the whole tree.
pub struct TestRepo {
    base: TempDir,
    repo: PathBuf,
    env: IsolatedEnv,
}

impl TestRepo {
    /// Creates a repository with the default options.
    pub fn init() -> Self {
        Self::init_with(RepoOptions::new())
    }

    /// Creates a repository with `options`.
    pub fn init_with(options: RepoOptions) -> Self {
        let base = TempDir::new()
            .unwrap_or_else(|error| panic!("cannot create a temporary root: {error}"));
        let home = base.path().join("home");
        fs::create_dir_all(&home)
            .unwrap_or_else(|error| panic!("cannot create {}: {error}", home.display()));
        let repo = base.path().join("repo");
        let env = IsolatedEnv::new(&home);

        let global_scope: Vec<OsString> =
            vec!["--file".into(), env.global_config().into_os_string()];
        for (key, value) in GLOBAL_CONFIG {
            set_config(&env, base.path(), &global_scope, key, value);
        }
        for (key, value) in &options.global_config {
            set_config(&env, base.path(), &global_scope, key, value);
        }

        let format = options.object_format.unwrap_or_default().as_str();
        let branch = options.initial_branch.as_deref().unwrap_or(DEFAULT_BRANCH);
        let args: Vec<OsString> = vec![
            "init".into(),
            "--quiet".into(),
            format!("--object-format={format}").into(),
            format!("--initial-branch={branch}").into(),
            repo.clone().into_os_string(),
        ];
        run_checked(&env, base.path(), &args, "create the repository");

        let instance = Self { base, repo, env };
        for (key, value) in &options.local_config {
            instance.set_local_config(key, value);
        }
        instance
    }

    /// The private root that holds the repository and the test home.
    pub fn root(&self) -> &Path {
        self.base.path()
    }

    /// Path of the repository working tree.
    pub fn path(&self) -> &Path {
        &self.repo
    }

    /// Path of the test-only home directory.
    pub fn home(&self) -> &Path {
        self.env.home()
    }

    /// The isolated environment used for every child Git process.
    pub fn env(&self) -> &IsolatedEnv {
        &self.env
    }

    /// Runs Git in the repository and returns the raw process output.
    pub fn git<I, S>(&self, args: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let os_args: Vec<OsString> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_os_string())
            .collect();
        git_command(&self.env, &self.repo, &os_args)
            .output()
            .unwrap_or_else(|error| {
                panic!(
                    "cannot run git {} in {}: {error}",
                    describe(&os_args),
                    self.repo.display()
                )
            })
    }

    /// Runs Git, asserts a zero exit, and returns stdout bytes.
    pub fn git_ok<I, S>(&self, args: I) -> Vec<u8>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let os_args: Vec<OsString> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_os_string())
            .collect();
        let output = git_command(&self.env, &self.repo, &os_args)
            .output()
            .unwrap_or_else(|error| {
                panic!(
                    "cannot run git {} in {}: {error}",
                    describe(&os_args),
                    self.repo.display()
                )
            });
        assert!(
            output.status.success(),
            "git {} failed with exit {} in {}\nstderr: {}",
            describe(&os_args),
            output.status,
            self.repo.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    /// Runs Git with `input` on stdin, asserts a zero exit, and returns
    /// stdout bytes. Use this for NUL-delimited path protocols.
    pub fn git_stdin<I, S>(&self, args: I, input: &[u8]) -> Vec<u8>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let os_args: Vec<OsString> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_os_string())
            .collect();
        let mut command = git_command(&self.env, &self.repo, &os_args);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .unwrap_or_else(|error| panic!("cannot run git {}: {error}", describe(&os_args)));
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(input)
                .unwrap_or_else(|error| panic!("cannot feed git stdin: {error}"));
        }
        let output = child
            .wait_with_output()
            .unwrap_or_else(|error| panic!("cannot wait for git {}: {error}", describe(&os_args)));
        assert!(
            output.status.success(),
            "git {} failed with exit {} in {}\nstderr: {}",
            describe(&os_args),
            output.status,
            self.repo.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    /// Runs Git and asserts a nonzero exit; returns the raw output.
    pub fn git_fail<I, S>(&self, args: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let os_args: Vec<OsString> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_os_string())
            .collect();
        let output = git_command(&self.env, &self.repo, &os_args)
            .output()
            .unwrap_or_else(|error| {
                panic!(
                    "cannot run git {} in {}: {error}",
                    describe(&os_args),
                    self.repo.display()
                )
            });
        assert!(
            !output.status.success(),
            "git {} unexpectedly succeeded in {}",
            describe(&os_args),
            self.repo.display()
        );
        output
    }

    /// Absolute path of a repository-relative path written with `/`.
    pub fn file_path(&self, relative: &str) -> PathBuf {
        let mut path = self.repo.clone();
        for part in relative.split('/') {
            path.push(part);
        }
        path
    }

    /// Writes raw bytes to a repository-relative file and returns its path.
    /// Missing parent directories are created.
    pub fn write_file(&self, relative: &str, bytes: &[u8]) -> PathBuf {
        let path = self.file_path(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("cannot create {}: {error}", parent.display()));
        }
        fs::write(&path, bytes)
            .unwrap_or_else(|error| panic!("cannot write {}: {error}", path.display()));
        path
    }

    /// Reads raw bytes of a repository-relative working file.
    pub fn read_file(&self, relative: &str) -> Vec<u8> {
        let path = self.file_path(relative);
        fs::read(&path).unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
    }

    /// Sets the Unix permission bits of a repository-relative file.
    pub fn set_mode(&self, relative: &str, mode: u32) {
        let path = self.file_path(relative);
        let metadata = fs::metadata(&path)
            .unwrap_or_else(|error| panic!("cannot inspect {}: {error}", path.display()));
        let mut permissions = metadata.permissions();
        permissions.set_mode(mode);
        fs::set_permissions(&path, permissions)
            .unwrap_or_else(|error| panic!("cannot set the mode of {}: {error}", path.display()));
    }

    /// Applies one repository-local configuration entry.
    pub fn set_local_config(&self, key: &str, value: &str) {
        set_config(&self.env, &self.repo, &["--local".into()], key, value);
    }

    /// Stages every change and creates one fixture commit. Returns the new
    /// HEAD object ID.
    pub fn commit_all(&self, message: &str) -> String {
        self.git_ok(["add", "--all"]);
        self.git_ok(["commit", "--quiet", "--message", message]);
        super::text(&self.git_ok(["rev-parse", "HEAD"]))
    }
}
