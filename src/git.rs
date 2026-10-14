//! Adapter for the installed Git executable and its binary protocols.
//!
//! SPEC 14.1 fixes the rules for every Git process Margin starts:
//!
//! - Git runs through `std::process::Command` with separate argv. Paths,
//!   revisions, and messages are data, never shell syntax.
//! - Execution is bounded in time and captured output is bounded in bytes.
//! - Pathspecs stay literal and path lists travel NUL-delimited, so unusual
//!   filenames cannot be reinterpreted.
//! - Failures are typed: process errors, timeouts, output limits, nonzero
//!   exits, malformed output, and unexpected inherited environment.
//! - Read-only calls set `GIT_OPTIONAL_LOCKS=0` and `GIT_NO_LAZY_FETCH=1`,
//!   so a query neither refreshes the real index nor fetches missing
//!   objects from a promisor remote.
//!
//! The ordinary user identity, signing policy, and hooks stay in force:
//! the adapter never sets a blanket empty hooks path, `--no-verify`, or
//! disabled signing (SPEC 14.1).

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// How long to sleep between exit polls of a running child.
const POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Largest slice of captured child stderr kept inside an error. SPEC 17.5
/// bounds an ordinary error at 4 KiB.
const MAX_ERROR_DETAIL_BYTES: usize = 4 * 1024;

/// Inherited variables that redirect Git to another repository or override
/// its configuration. SPEC 14.1 requires refusing an inherited value unless
/// Margin set it itself through [`Git::set_env_override`].
const REDIRECTING_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_GRAFT_FILE",
    "GIT_REPLACE_REF_BASE",
    "GIT_CONFIG_COUNT",
];

/// Object format of a repository (SPEC 7.1). No parser assumes a fixed
/// object-ID width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectFormat {
    /// Object IDs are 40 lowercase hex digits.
    Sha1,
    /// Object IDs are 64 lowercase hex digits.
    Sha256,
}

impl ObjectFormat {
    /// The value reported by `git rev-parse --show-object-format`.
    pub fn as_str(self) -> &'static str {
        match self {
            ObjectFormat::Sha1 => "sha1",
            ObjectFormat::Sha256 => "sha256",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "sha1" => Some(ObjectFormat::Sha1),
            "sha256" => Some(ObjectFormat::Sha256),
            _ => None,
        }
    }
}

/// Why one Git call runs: observation or mutation. Query calls avoid the
/// optional index refresh and every lazy object fetch (SPEC 14.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The call only observes repository state.
    Query,
    /// The call may write refs, the index, objects, or configuration.
    Mutate,
}

/// Bounds applied to every child process.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Wall-clock budget for one process, including output capture.
    pub timeout: Duration,
    /// Largest number of bytes captured from stdout, and separately from
    /// stderr. A stream that writes more fails with
    /// [`GitError::OutputLimit`] instead of growing without bound.
    pub max_output_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        // SPEC 6.1 sets the default command budget to 10,000 ms; SPEC 3.3
        // uses 16 MiB as the per-file ceiling, which also bounds captures.
        Limits {
            timeout: Duration::from_millis(10_000),
            max_output_bytes: 16 * 1024 * 1024,
        }
    }
}

/// Raw result of one finished Git process.
#[derive(Clone, Debug)]
pub struct GitOutput {
    /// Bytes the process wrote to stdout, bounded by [`Limits`].
    pub stdout: Vec<u8>,
    /// Bytes the process wrote to stderr, bounded by [`Limits`].
    pub stderr: Vec<u8>,
    /// The exit code, or `None` when the process died from a signal.
    pub exit_code: Option<i32>,
}

impl GitOutput {
    /// Says whether the process exited with status zero.
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Typed failure of one Git process call (SPEC 14.1 and 7.5).
#[derive(Debug)]
pub enum GitError {
    /// The process could not be spawned or awaited.
    Io {
        /// What Margin was doing, such as `spawn git status`.
        context: String,
        /// The underlying process error.
        source: io::Error,
    },
    /// The process exceeded its time budget and was killed. The outcome of
    /// any intended mutation is unknown until inspected.
    Timeout {
        /// The argv that was run.
        argv: Vec<String>,
        /// The budget that expired.
        budget: Duration,
    },
    /// A captured stream wrote more bytes than the limit allows.
    OutputLimit {
        /// The argv that was run.
        argv: Vec<String>,
        /// The per-stream byte limit that was exceeded.
        limit: usize,
    },
    /// The process exited with a nonzero status.
    Exit {
        /// The argv that was run.
        argv: Vec<String>,
        /// The exit code, or `None` for a signal.
        exit_code: Option<i32>,
        /// The bounded head of stderr, for diagnostics.
        stderr: Vec<u8>,
    },
    /// The inherited environment redirects Git somewhere unexpected and
    /// Margin did not set that variable itself.
    UnexpectedEnvironment {
        /// The offending variable names.
        vars: Vec<OsString>,
    },
    /// Git output did not match the protocol Margin requested.
    MalformedOutput {
        /// The command whose output could not be parsed.
        command: String,
        /// Why the bytes were rejected.
        detail: String,
    },
}

impl GitError {
    /// The SPEC 7.5 error code this failure maps to.
    pub fn public_code(&self) -> &'static str {
        match self {
            GitError::Io { .. } => "IO_FAILED",
            GitError::Timeout { .. } => "OUTCOME_UNKNOWN",
            GitError::OutputLimit { .. } => "LIMIT_EXCEEDED",
            GitError::Exit { .. } => "GIT_FAILED",
            GitError::UnexpectedEnvironment { .. } => "UNSUPPORTED_REPOSITORY",
            GitError::MalformedOutput { .. } => "CONTRACT_VIOLATION",
        }
    }

    /// Says whether repeating the identical process call can be useful and
    /// safe. Only a process-level failure is retryable at this layer; a
    /// timeout or nonzero exit needs outcome inspection first (SPEC 7.5).
    pub fn retryable(&self) -> bool {
        matches!(self, GitError::Io { .. })
    }
}

impl fmt::Display for GitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitError::Io { context, source } => {
                write!(formatter, "cannot {context}: {source}")
            }
            GitError::Timeout { argv, budget } => {
                write!(
                    formatter,
                    "git {} exceeded its {:?} time budget and was killed",
                    argv.join(" "),
                    budget
                )
            }
            GitError::OutputLimit { argv, limit } => {
                write!(
                    formatter,
                    "git {} wrote more than {limit} output bytes",
                    argv.join(" ")
                )
            }
            GitError::Exit {
                argv,
                exit_code,
                stderr,
            } => {
                let detail = String::from_utf8_lossy(stderr);
                let detail = detail.trim_end();
                write!(
                    formatter,
                    "git {} exited with {}",
                    argv.join(" "),
                    exit_code.map_or_else(|| "a signal".to_string(), |code| code.to_string())
                )?;
                if detail.is_empty() {
                    Ok(())
                } else {
                    write!(formatter, ": {detail}")
                }
            }
            GitError::UnexpectedEnvironment { vars } => {
                let names: Vec<String> = vars
                    .iter()
                    .map(|name| name.to_string_lossy().into_owned())
                    .collect();
                write!(
                    formatter,
                    "the inherited environment redirects Git: {}",
                    names.join(", ")
                )
            }
            GitError::MalformedOutput { command, detail } => {
                write!(formatter, "malformed output from git {command}: {detail}")
            }
        }
    }
}

impl std::error::Error for GitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GitError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Builds the child environment for one mode.
fn fixed_environment(mode: Mode) -> Vec<(&'static str, &'static str)> {
    let mut vars = vec![
        // Margin never introduces a glob pathspec language (SPEC 3.2), so
        // every pathspec stays literal even for adversarial names.
        ("GIT_LITERAL_PATHSPECS", "1"),
        // Machine mode has no hidden credential flow (SPEC 6.1).
        ("GIT_TERMINAL_PROMPT", "0"),
        // Object inspection must fail explicitly instead of fetching from
        // a promisor remote (SPEC 14.1).
        ("GIT_NO_LAZY_FETCH", "1"),
    ];
    if matches!(mode, Mode::Query) {
        // A query must not refresh the real index through an optional lock
        // (SPEC 14.1).
        vars.push(("GIT_OPTIONAL_LOCKS", "0"));
    }
    vars
}

/// Decodes raw path bytes into an `OsString`.
pub(crate) fn os_string_from_bytes(bytes: Vec<u8>) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(bytes)
    }
    #[cfg(not(unix))]
    {
        // Margin supports Linux and macOS only; keep other builds working
        // for `cargo check` by lossy-decoding.
        OsString::from(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// Splits NUL-delimited protocol records (SPEC 14.1). The trailing empty
/// record after the final NUL is dropped.
pub fn split_nul(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .split(|&byte| byte == 0)
        .filter(|record| !record.is_empty())
}

/// Removes one trailing newline, if present.
fn strip_trailing_newline(bytes: &[u8]) -> &[u8] {
    bytes.strip_suffix(b"\n").unwrap_or(bytes)
}

/// Validates one full lowercase hex object ID. Both documented widths are
/// accepted; no caller may assume 40 characters (SPEC 7.1).
fn validate_oid(bytes: &[u8]) -> Option<String> {
    let valid_length = bytes.len() == 40 || bytes.len() == 64;
    let lowercase_hex = bytes
        .iter()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
    if valid_length && lowercase_hex {
        // The length and character checks already imply ASCII.
        Some(String::from_utf8(bytes.to_vec()).expect("ASCII bytes are UTF-8"))
    } else {
        None
    }
}

/// Renders argv for an error message.
fn describe_argv(argv: &[OsString]) -> Vec<String> {
    argv.iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

/// One captured output stream of a child process.
///
/// A reader thread drains the pipe into a bounded buffer and keeps reading
/// after the limit so the child never blocks on a full pipe. The thread
/// finishes when the pipe closes, which normally happens at process exit; a
/// grandchild that inherits the pipe can keep it open, so the waiter always
/// applies the remaining time budget.
struct Capture {
    bytes: Arc<Mutex<Vec<u8>>>,
    overflow: Arc<AtomicBool>,
    /// Disconnects when the reader thread reached end of stream.
    finished: mpsc::Receiver<()>,
}

impl Capture {
    /// Drains `pipe` into a bounded buffer on its own thread.
    fn spawn<W: Read + Send + 'static>(pipe: W, limit: usize) -> Capture {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let overflow = Arc::new(AtomicBool::new(false));
        let (done_tx, done_rx) = mpsc::channel();
        let sink = Arc::clone(&bytes);
        let flag = Arc::clone(&overflow);
        thread::spawn(move || {
            let mut pipe = pipe;
            let mut chunk = [0u8; 8192];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => {
                        let mut buffer = sink.lock().expect("the capture lock is live");
                        let room = limit.saturating_sub(buffer.len());
                        if read > room {
                            flag.store(true, Ordering::Relaxed);
                        }
                        buffer.extend_from_slice(&chunk[..read.min(room)]);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            drop(done_tx);
        });
        Capture {
            bytes,
            overflow,
            finished: done_rx,
        }
    }

    /// Waits until end of stream or `deadline`. Returns `false` on timeout.
    fn wait(&self, deadline: Instant) -> bool {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match self.finished.recv_timeout(remaining) {
            // Nothing is ever sent; disconnect marks end of stream.
            Ok(()) | Err(RecvTimeoutError::Disconnected) => true,
            Err(RecvTimeoutError::Timeout) => false,
        }
    }

    /// Returns the bytes captured so far.
    fn snapshot(&self) -> Vec<u8> {
        self.bytes.lock().expect("the capture lock is live").clone()
    }

    /// Says whether the stream wrote past the byte limit.
    fn over_limit(&self) -> bool {
        self.overflow.load(Ordering::Relaxed)
    }
}

/// One effective attribute assignment from `git check-attr -z --stdin`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttributeAssignment {
    /// The literal repository-relative path.
    pub path: OsString,
    /// The attribute name, such as `filter`.
    pub attribute: String,
    /// The effective value: `unspecified`, `unset`, `set`, or a string.
    pub value: String,
}

/// One entry of `git ls-files --stage -z`, with the path kept as raw bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    /// Six octal digits, such as `100644`.
    pub mode: String,
    /// Full lowercase hex blob or commit object ID.
    pub oid: String,
    /// Zero for ordinary entries; 1 to 3 mark conflict stages.
    pub stage: u32,
    /// The literal repository-relative path.
    pub path: OsString,
}

/// One index flag that makes Git skip its own change detection for a
/// path (SPEC 3.1). `git update-index` records the flag on the index
/// entry, and `git diff-files`, `git diff-index`, and `git status` then
/// report no working change for that path, so unsaved content can sit
/// behind the flag invisibly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexFlag {
    /// `git update-index --assume-unchanged`: Git trusts the stat cache
    /// and stops comparing the working file.
    AssumeUnchanged,
    /// `git update-index --skip-worktree`: Git also promises not to touch
    /// the working file, the sparse-checkout mechanism.
    SkipWorktree,
}

/// One entry of `git ls-tree -r -z`, with the path kept as raw bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    /// Six octal digits, such as `100644` or `160000`.
    pub mode: String,
    /// Entry kind: `blob` or `commit`. A gitlink is a `commit` entry.
    pub kind: String,
    /// Full lowercase hex object ID.
    pub oid: String,
    /// The literal repository-relative path.
    pub path: OsString,
}

/// Result of staging an exact path list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageOutcome {
    /// The path list was empty, so no Git process ran at all. An empty
    /// resolved staging list is a no-op, never a Git command that defaults
    /// to the whole repository (SPEC 14.1).
    Noop,
    /// The paths were staged through the NUL-delimited index protocol.
    Staged,
}

/// The Git process adapter: program, environment policy, and bounds.
///
/// The environment map is exactly what every child receives. Production
/// code builds it from the real process environment so the ordinary user
/// identity, signing policy, and hooks stay in force; tests build it from
/// the isolated harness environment.
#[derive(Clone, Debug)]
pub struct Git {
    program: OsString,
    environment: BTreeMap<OsString, OsString>,
    /// Variables Margin set itself, such as `GIT_INDEX_FILE` for a
    /// disposable index. These do not trigger the inheritance check.
    own_overrides: BTreeMap<OsString, OsString>,
    working_dir: Option<PathBuf>,
    limits: Limits,
    /// The object format, once read. The format of one repository cannot
    /// change while this process runs, and clones of the adapter share the
    /// answer, so the identity work of one operation reads it once
    /// (SPEC 17.5).
    object_format: Arc<OnceLock<ObjectFormat>>,
}

impl Default for Git {
    fn default() -> Self {
        Self::new()
    }
}

impl Git {
    /// Builds an adapter that runs `git` with the current environment.
    pub fn new() -> Self {
        Self::from_env(std::env::vars_os())
    }

    /// Builds an adapter whose children receive exactly `environment`.
    pub fn from_env(environment: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        Git {
            program: OsString::from("git"),
            environment: environment.into_iter().collect(),
            own_overrides: BTreeMap::new(),
            working_dir: None,
            limits: Limits::default(),
            object_format: Arc::new(OnceLock::new()),
        }
    }

    /// Runs this program instead of `git`. The process seam tests use it to
    /// inject hangs, floods, and malformed output (SPEC 16.5).
    pub fn with_program(mut self, program: impl Into<OsString>) -> Self {
        self.program = program.into();
        self
    }

    /// Replaces the time and output budget.
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Tightens a query's output bound without changing the caller's timeout.
    pub fn with_output_limit(mut self, bytes: usize) -> Self {
        self.limits.max_output_bytes = bytes.min(self.limits.max_output_bytes);
        self
    }

    /// Runs every child in this directory.
    pub fn at(mut self, directory: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(directory.into());
        self
    }

    /// Sets one environment variable as Margin's own explicit choice, for
    /// example `GIT_INDEX_FILE` pointing at a disposable index. The value
    /// overrides the inherited environment and never triggers the
    /// redirection refusal (SPEC 14.1).
    pub fn set_env_override(&mut self, key: &str, value: impl Into<OsString>) {
        let key = OsString::from(key);
        let value = value.into();
        self.environment.insert(key.clone(), value.clone());
        self.own_overrides.insert(key, value);
    }

    /// Refuses inherited variables that would redirect Git somewhere
    /// Margin did not choose.
    fn environment_problem(&self) -> Result<(), GitError> {
        let vars = REDIRECTING_VARS
            .iter()
            .map(OsStr::new)
            .filter(|key| self.environment.contains_key(*key))
            .filter(|key| !self.own_overrides.contains_key(*key))
            .map(|key| key.to_os_string())
            .collect::<Vec<_>>();
        if vars.is_empty() {
            Ok(())
        } else {
            Err(GitError::UnexpectedEnvironment { vars })
        }
    }

    /// Runs Git and returns the raw process result, including nonzero exits.
    pub fn run<I, S>(&self, mode: Mode, args: I) -> Result<GitOutput, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let argv = collect_argv(args);
        self.execute(mode, &argv, None)
    }

    /// Runs Git with `input` on stdin and returns the raw process result.
    /// Use this for NUL-delimited path protocols.
    pub fn run_stdin<I, S>(&self, mode: Mode, args: I, input: &[u8]) -> Result<GitOutput, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let argv = collect_argv(args);
        self.execute(mode, &argv, Some(input))
    }

    /// Runs Git, requires a zero exit, and returns stdout.
    pub fn run_ok<I, S>(&self, mode: Mode, args: I) -> Result<Vec<u8>, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let argv = collect_argv(args);
        let output = self.execute(mode, &argv, None)?;
        require_success(&argv, output)
    }

    /// Spawns the process, captures both streams within the limits, and
    /// applies the time budget.
    fn execute(
        &self,
        mode: Mode,
        argv: &[OsString],
        input: Option<&[u8]>,
    ) -> Result<GitOutput, GitError> {
        self.environment_problem()?;

        let mut command = Command::new(&self.program);
        command.args(argv);
        if let Some(directory) = &self.working_dir {
            command.current_dir(directory);
        }
        // Margin closes unused stdin (SPEC 6.1); callers that feed a
        // protocol get a pipe that closes after the input is written.
        command.stdin(match input {
            Some(_) => Stdio::piped(),
            None => Stdio::null(),
        });
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        command.env_clear();
        command.envs(&self.environment);
        for (key, value) in fixed_environment(mode) {
            command.env(key, value);
        }

        let described = describe_argv(argv);
        let program = described
            .first()
            .cloned()
            .unwrap_or_else(|| "git".to_string());
        let mut child = command.spawn().map_err(|source| GitError::Io {
            context: format!("spawn {program}"),
            source,
        })?;
        let limit = self.limits.max_output_bytes;
        let stdout = Capture::spawn(child.stdout.take().expect("stdout is piped"), limit);
        let stderr = Capture::spawn(child.stderr.take().expect("stderr is piped"), limit);
        if let Some(mut stdin) = child.stdin.take() {
            let bytes = input.map(<[u8]>::to_vec);
            thread::spawn(move || {
                if let Some(bytes) = bytes {
                    // A failed write only closes the pipe earlier.
                    let _ = stdin.write_all(&bytes);
                }
            });
        }

        let deadline = Instant::now() + self.limits.timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    // The pipes close at exit unless a grandchild holds one.
                    if !stdout.wait(deadline) || !stderr.wait(deadline) {
                        return Err(GitError::Timeout {
                            argv: described,
                            budget: self.limits.timeout,
                        });
                    }
                    if stdout.over_limit() || stderr.over_limit() {
                        return Err(GitError::OutputLimit {
                            argv: described,
                            limit,
                        });
                    }
                    return Ok(GitOutput {
                        stdout: stdout.snapshot(),
                        stderr: stderr.snapshot(),
                        exit_code: status.code(),
                    });
                }
                Ok(None) => {}
                Err(source) => {
                    return Err(GitError::Io {
                        context: "wait for git".to_string(),
                        source,
                    });
                }
            }
            if Instant::now() >= deadline {
                // The outcome of an interrupted mutation stays unknown until
                // inspected; a killed query has no result (SPEC 9.6).
                let _ = child.kill();
                let _ = child.wait();
                return Err(GitError::Timeout {
                    argv: described,
                    budget: self.limits.timeout,
                });
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Returns the version string of the installed Git, without the
    /// `git version ` prefix.
    pub fn git_version(&self) -> Result<String, GitError> {
        let stdout = self.run_ok(Mode::Query, ["--version"])?;
        let raw = String::from_utf8(strip_trailing_newline(&stdout).to_vec()).map_err(|error| {
            GitError::MalformedOutput {
                command: "--version".to_string(),
                detail: error.to_string(),
            }
        })?;
        raw.strip_prefix("git version ")
            .map(|version| version.trim().to_string())
            .ok_or_else(|| GitError::MalformedOutput {
                command: "--version".to_string(),
                detail: format!("unexpected version line {raw:?}"),
            })
    }

    /// Returns the object format of the repository.
    pub fn object_format(&self) -> Result<ObjectFormat, GitError> {
        if let Some(cached) = self.object_format.get() {
            return Ok(*cached);
        }
        let stdout = self.run_ok(Mode::Query, ["rev-parse", "--show-object-format"])?;
        let raw = String::from_utf8(strip_trailing_newline(&stdout).to_vec()).map_err(|error| {
            GitError::MalformedOutput {
                command: "rev-parse --show-object-format".to_string(),
                detail: error.to_string(),
            }
        })?;
        let format = ObjectFormat::parse(&raw).ok_or_else(|| GitError::MalformedOutput {
            command: "rev-parse --show-object-format".to_string(),
            detail: format!("unknown object format {raw:?}"),
        })?;
        let _ = self.object_format.set(format);
        Ok(format)
    }

    /// Resolves `revision` to exactly one commit object ID.
    ///
    /// The `--end-of-options` boundary keeps a revision that starts with a
    /// dash from being parsed as an option, and `^{commit}` rejects every
    /// other object type. Later commands receive the validated canonical
    /// object ID, not the user string (SPEC 14.1).
    pub fn resolve_commit(&self, revision: &OsStr) -> Result<String, GitError> {
        let mut expression = OsString::from(revision);
        expression.push("^{commit}");
        let argv: Vec<OsString> = vec![
            "rev-parse".into(),
            "--verify".into(),
            "--end-of-options".into(),
            expression,
        ];
        let stdout = self.run_ok(Mode::Query, argv)?;
        let oid = validate_oid(strip_trailing_newline(&stdout));
        oid.ok_or_else(|| GitError::MalformedOutput {
            command: "rev-parse --verify".to_string(),
            detail: format!(
                "not a full lowercase hex object ID: {:?}",
                String::from_utf8_lossy(strip_trailing_newline(&stdout))
            ),
        })
    }

    /// Returns the index entries from `git ls-files --stage -z`. Paths stay
    /// raw bytes; unusual names are never reinterpreted.
    pub fn ls_files_stage(&self) -> Result<Vec<IndexEntry>, GitError> {
        let stdout = self.run_ok(Mode::Query, ["ls-files", "--stage", "-z"])?;
        split_nul(&stdout)
            .map(parse_ls_files_record)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| GitError::MalformedOutput {
                command: "ls-files --stage".to_string(),
                detail: "expected `<mode> <oid> <stage>\\t<path>` records".to_string(),
            })
    }

    /// Stages exactly `paths` through the NUL-delimited index protocol
    /// (`git update-index --add --remove -z --stdin`). Paths are literal
    /// data: additions, modifications, and deletions are all captured, and
    /// no pathspec matching happens at all (SPEC 9.2).
    pub fn stage_paths(&self, paths: &[OsString]) -> Result<StageOutcome, GitError> {
        if paths.is_empty() {
            return Ok(StageOutcome::Noop);
        }
        let mut input = Vec::new();
        for path in paths {
            input.extend_from_slice(path.as_encoded_bytes());
            input.push(0);
        }
        // `--stdin` must be the last argument.
        let argv: Vec<OsString> = ["update-index", "--add", "--remove", "-z", "--stdin"]
            .iter()
            .map(|arg| OsString::from(*arg))
            .collect();
        let output = self.run_stdin(Mode::Mutate, argv.clone(), &input)?;
        require_success(&argv, output)?;
        Ok(StageOutcome::Staged)
    }

    /// Returns the repository-relative untracked paths that no exclude rule
    /// ignores, from `git ls-files --others --exclude-standard -z`.
    ///
    /// Plain untracked files are listed individually. A nested repository
    /// is never descended and appears as one directory entry that ends with
    /// `/`; callers must treat such entries as a whole.
    pub fn ls_files_others(&self) -> Result<Vec<OsString>, GitError> {
        let stdout = self.run_ok(
            Mode::Query,
            ["ls-files", "--others", "--exclude-standard", "-z"],
        )?;
        Ok(split_nul(&stdout)
            .map(|record| os_string_from_bytes(record.to_vec()))
            .collect())
    }

    /// Computes the blob object ID of `bytes` without storing an object,
    /// through `git hash-object -t blob --stdin` (SPEC 8.6).
    pub fn hash_object(&self, bytes: &[u8]) -> Result<String, GitError> {
        let argv: Vec<OsString> = ["hash-object", "-t", "blob", "--stdin"]
            .iter()
            .map(|arg| OsString::from(*arg))
            .collect();
        let output = self.run_stdin(Mode::Query, argv.clone(), bytes)?;
        let stdout = require_success(&argv, output)?;
        validate_oid(strip_trailing_newline(&stdout)).ok_or_else(|| GitError::MalformedOutput {
            command: "hash-object".to_string(),
            detail: format!(
                "not a full lowercase hex object ID: {:?}",
                String::from_utf8_lossy(strip_trailing_newline(&stdout))
            ),
        })
    }

    /// Largest number of working files hashed by one process. Batching keeps
    /// a whole-scope digest from scaling one process per document (SPEC
    /// 17.5) while the argv stays far below the operating-system limit.
    const HASH_PATHS_BATCH: usize = 256;

    /// Computes the blob object IDs of working files `paths` through
    /// `git hash-object -t blob --no-filters -- <path>...`, in batched
    /// processes.
    ///
    /// `--no-filters` hashes each file's raw bytes exactly as
    /// [`Git::hash_object`] on the read bytes would, whatever attributes
    /// apply (SPEC 8.6 working content). Paths are literal argv and resolve
    /// against the adapter's working directory; an empty list runs no
    /// process at all (SPEC 14.1).
    pub fn hash_object_paths(&self, paths: &[OsString]) -> Result<Vec<String>, GitError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut oids = Vec::with_capacity(paths.len());
        for chunk in paths.chunks(Self::HASH_PATHS_BATCH) {
            let mut argv: Vec<OsString> = ["hash-object", "-t", "blob", "--no-filters", "--"]
                .iter()
                .map(|arg| OsString::from(*arg))
                .collect();
            argv.extend(chunk.iter().cloned());
            let stdout = self.run_ok(Mode::Query, argv)?;
            let lines: Vec<&[u8]> = stdout
                .split(|&byte| byte == b'\n')
                .filter(|line| !line.is_empty())
                .collect();
            if lines.len() != chunk.len() {
                return Err(GitError::MalformedOutput {
                    command: "hash-object".to_string(),
                    detail: format!("expected {} object ID(s), got {}", chunk.len(), lines.len()),
                });
            }
            for line in lines {
                let Some(oid) = validate_oid(line) else {
                    return Err(GitError::MalformedOutput {
                        command: "hash-object".to_string(),
                        detail: format!(
                            "not a full lowercase hex object ID: {:?}",
                            String::from_utf8_lossy(line)
                        ),
                    });
                };
                oids.push(oid);
            }
        }
        Ok(oids)
    }

    /// Lists every blob and gitlink entry of `revision` recursively, from
    /// `git ls-tree -r -z` (SPEC 8.6). `revision` must be a validated
    /// canonical object ID from this adapter, never a user string
    /// (SPEC 14.1).
    pub fn ls_tree(&self, revision: &str) -> Result<Vec<TreeEntry>, GitError> {
        let stdout = self.run_ok(Mode::Query, ["ls-tree", "-r", "-z", revision])?;
        split_nul(&stdout)
            .map(parse_ls_tree_record)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| GitError::MalformedOutput {
                command: "ls-tree".to_string(),
                detail: "expected `<mode> SP <kind> SP <oid>\\t<path>` records".to_string(),
            })
    }

    /// Returns the absolute path of the working tree root from
    /// `git rev-parse --show-toplevel`.
    ///
    /// `-z` is not honored for `--show-toplevel`, so exactly one trailing
    /// newline is removed and interior bytes, including newlines inside the
    /// path, survive unchanged. A bare repository has no working tree and
    /// fails with a typed error.
    pub fn toplevel(&self) -> Result<PathBuf, GitError> {
        let stdout = self.run_ok(Mode::Query, ["rev-parse", "--show-toplevel"])?;
        let raw = strip_trailing_newline(&stdout);
        if raw.is_empty() {
            return Err(GitError::MalformedOutput {
                command: "rev-parse --show-toplevel".to_string(),
                detail: "no working tree root; a bare repository has no scope".to_string(),
            });
        }
        Ok(PathBuf::from(os_string_from_bytes(raw.to_vec())))
    }

    /// Returns the path of one repository metadata file, from
    /// `git rev-parse --git-path`. The name follows Git's per-worktree
    /// metadata layout, so an in-progress-operation marker of a linked
    /// worktree resolves inside `.git/worktrees/` while the main worktree
    /// resolves inside `.git/`.
    ///
    /// A relative result resolves against the adapter's working directory,
    /// which is where Git itself resolves it.
    pub fn git_path(&self, name: &str) -> Result<PathBuf, GitError> {
        let stdout = self.run_ok(Mode::Query, ["rev-parse", "--git-path", name])?;
        let raw = strip_trailing_newline(&stdout);
        if raw.is_empty() {
            return Err(GitError::MalformedOutput {
                command: "rev-parse --git-path".to_string(),
                detail: format!("no path for the metadata file {name:?}"),
            });
        }
        let path = PathBuf::from(os_string_from_bytes(raw.to_vec()));
        Ok(match (&self.working_dir, path.is_absolute()) {
            (Some(root), false) => root.join(path),
            _ => path,
        })
    }

    /// Returns the paths of several repository metadata files through one
    /// `git rev-parse` process with one repeated `--git-path` flag per
    /// name, in the order the names were given (SPEC 17.5: a repository
    /// inspection does not scale one process per probe).
    ///
    /// The names follow Git's per-worktree metadata layout, exactly as
    /// [`Git::git_path`] resolves them. An empty list runs no process at
    /// all (SPEC 14.1).
    pub fn git_paths(&self, names: &[&str]) -> Result<Vec<PathBuf>, GitError> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let mut argv: Vec<OsString> = vec!["rev-parse".into()];
        for name in names {
            argv.push("--git-path".into());
            argv.push(OsString::from(*name));
        }
        let stdout = self.run_ok(Mode::Query, argv)?;
        let lines = stdout.split(|byte| *byte == b'\n').collect::<Vec<_>>();
        if lines.len() != names.len() + 1 || !lines.last().expect("nonempty").is_empty() {
            return Err(GitError::MalformedOutput {
                command: "rev-parse --git-path".to_string(),
                detail: format!("expected {} path line(s), got another shape", names.len()),
            });
        }
        let mut paths = Vec::with_capacity(names.len());
        for (name, line) in names.iter().zip(lines) {
            if line.is_empty() {
                return Err(GitError::MalformedOutput {
                    command: "rev-parse --git-path".to_string(),
                    detail: format!("no path for the metadata file {name:?}"),
                });
            }
            let path = PathBuf::from(os_string_from_bytes(line.to_vec()));
            paths.push(match (&self.working_dir, path.is_absolute()) {
                (Some(root), false) => root.join(path),
                _ => path,
            });
        }
        Ok(paths)
    }

    /// Says whether the repository is bare and has no working tree.
    pub fn is_bare_repository(&self) -> Result<bool, GitError> {
        self.rev_parse_boolean("--is-bare-repository")
    }

    /// Says whether the working directory lies inside a working tree.
    pub fn is_inside_work_tree(&self) -> Result<bool, GitError> {
        self.rev_parse_boolean("--is-inside-work-tree")
    }

    /// Says whether the working directory lies inside any Git repository,
    /// including a bare repository or a Git metadata directory, from
    /// `git rev-parse --git-dir`.
    ///
    /// Outside any repository Git exits 128, which this probe reports as
    /// `false` instead of a failure, so `margin init --create-repo` can
    /// decide between adopting and creating a repository (SPEC 5.1). Any
    /// other outcome stays one typed failure.
    pub fn inside_repository(&self) -> Result<bool, GitError> {
        let argv: Vec<OsString> = vec!["rev-parse".into(), "--git-dir".into()];
        let output = self.run(Mode::Query, argv.clone())?;
        match output.exit_code {
            Some(0) => Ok(true),
            Some(128) => Ok(false),
            _ => Err(GitError::Exit {
                argv: describe_argv(&argv),
                exit_code: output.exit_code,
                stderr: truncate_detail(output.stderr),
            }),
        }
    }

    /// Creates one repository at the adapter's working directory through
    /// ordinary `git init --quiet` (SPEC 5.1). `initial_branch` names the
    /// initial branch when given; otherwise Git's configured default
    /// applies.
    ///
    /// The call creates no identity and configures nothing beyond the
    /// repository itself, and it never switches the branch of a repository
    /// that already exists: the caller adopts an existing repository instead
    /// of reinitializing it.
    pub fn init_repository(&self, initial_branch: Option<&str>) -> Result<(), GitError> {
        let mut argv: Vec<OsString> = vec!["init".into(), "--quiet".into()];
        if let Some(branch) = initial_branch {
            argv.push(OsString::from(format!("--initial-branch={branch}")));
        }
        argv.push(".".into());
        self.run_ok(Mode::Mutate, argv)?;
        Ok(())
    }

    /// Resolves the effective author identity through
    /// `git var GIT_AUTHOR_IDENT` (SPEC 5.1). The probe reads configuration
    /// and environment only; a missing author and committer configuration is
    /// one nonzero exit, and Margin never writes an identity itself.
    pub fn author_ident(&self) -> Result<String, GitError> {
        let stdout = self.run_ok(Mode::Query, ["var", "GIT_AUTHOR_IDENT"])?;
        String::from_utf8(strip_trailing_newline(&stdout).to_vec()).map_err(|error| {
            GitError::MalformedOutput {
                command: "var GIT_AUTHOR_IDENT".to_string(),
                detail: error.to_string(),
            }
        })
    }

    fn rev_parse_boolean(&self, flag: &str) -> Result<bool, GitError> {
        let stdout = self.run_ok(Mode::Query, ["rev-parse", flag])?;
        match strip_trailing_newline(&stdout) {
            b"true" => Ok(true),
            b"false" => Ok(false),
            raw => Err(GitError::MalformedOutput {
                command: format!("rev-parse {flag}"),
                detail: format!(
                    "expected true or false, got {:?}",
                    String::from_utf8_lossy(raw)
                ),
            }),
        }
    }

    /// Returns the ref `HEAD` points at, or `None` when `HEAD` is detached.
    /// An unborn branch stays attached and resolves to its ref name.
    pub fn head_symbolic_ref(&self) -> Result<Option<String>, GitError> {
        let argv: Vec<OsString> = ["symbolic-ref", "--quiet", "HEAD"]
            .iter()
            .map(|arg| OsString::from(*arg))
            .collect();
        let output = self.run(Mode::Query, argv.clone())?;
        match optional_success(&argv, output)? {
            Some(stdout) => String::from_utf8(strip_trailing_newline(&stdout).to_vec())
                .map(Some)
                .map_err(|error| GitError::MalformedOutput {
                    command: "symbolic-ref".to_string(),
                    detail: error.to_string(),
                }),
            None => Ok(None),
        }
    }

    /// Returns the `HEAD` commit object ID, or `None` while the branch is
    /// unborn.
    pub fn head_commit(&self) -> Result<Option<String>, GitError> {
        let argv: Vec<OsString> = [
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            "HEAD^{commit}",
        ]
        .iter()
        .map(|arg| OsString::from(*arg))
        .collect();
        let output = self.run(Mode::Query, argv.clone())?;
        match optional_success(&argv, output)? {
            Some(stdout) => validate_oid(strip_trailing_newline(&stdout))
                .map(Some)
                .ok_or_else(|| GitError::MalformedOutput {
                    command: "rev-parse --verify HEAD".to_string(),
                    detail: format!(
                        "not a full lowercase hex object ID: {:?}",
                        String::from_utf8_lossy(strip_trailing_newline(&stdout))
                    ),
                }),
            None => Ok(None),
        }
    }

    /// Returns the working tree of the superproject that registered this
    /// repository as a gitlink, or `None` for an ordinary repository
    /// (SPEC 3.1).
    pub fn superproject_working_tree(&self) -> Result<Option<String>, GitError> {
        let stdout = self.run_ok(
            Mode::Query,
            ["rev-parse", "--show-superproject-working-tree"],
        )?;
        let raw = strip_trailing_newline(&stdout);
        if raw.is_empty() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(raw).into_owned()))
    }

    /// Returns every registered working tree, from
    /// `git worktree list --porcelain`. One entry means the repository has
    /// the single conventional working tree of SPEC 3.1.
    pub fn worktree_paths(&self) -> Result<Vec<PathBuf>, GitError> {
        let stdout = self.run_ok(Mode::Query, ["worktree", "list", "--porcelain"])?;
        let mut paths = Vec::new();
        for line in stdout.split(|&byte| byte == b'\n') {
            let Some(rest) = line.strip_prefix(b"worktree ") else {
                continue;
            };
            let rest = rest.strip_suffix(b"\r").unwrap_or(rest);
            if !rest.is_empty() {
                paths.push(PathBuf::from(os_string_from_bytes(rest.to_vec())));
            }
        }
        if paths.is_empty() {
            return Err(GitError::MalformedOutput {
                command: "worktree list".to_string(),
                detail: "no working-tree records".to_string(),
            });
        }
        Ok(paths)
    }

    /// Returns the effective value of one configuration key, or `None` when
    /// the key is unset. The value is the last one set across every
    /// configuration file, which is the value Git itself applies.
    pub fn config_value(&self, key: &str) -> Result<Option<String>, GitError> {
        let argv: Vec<OsString> = ["config", "--get", key]
            .iter()
            .map(|arg| OsString::from(*arg))
            .collect();
        let output = self.run(Mode::Query, argv.clone())?;
        Ok(optional_success(&argv, output)?
            .map(|stdout| String::from_utf8_lossy(strip_trailing_newline(&stdout)).into_owned()))
    }

    /// Returns the effective values of several configuration keys through
    /// one `git config --list -z` process (SPEC 17.5: reading the settings
    /// does not scale one process per key).
    ///
    /// The list holds every entry of every scope in Git's own precedence
    /// order, NUL-delimited as `key\nvalue`, so the last entry of a key is
    /// the value `git config --get` would answer — the same value, through
    /// one process instead of one per key. Keys match case-insensitively,
    /// because the list prints lowercase while `--get` accepts any case. A
    /// key the configuration never sets holds no entry in the result.
    pub fn config_values(&self, keys: &[&str]) -> Result<BTreeMap<String, String>, GitError> {
        if keys.is_empty() {
            return Ok(BTreeMap::new());
        }
        let argv: Vec<OsString> = ["config", "--list", "--null"]
            .iter()
            .map(|argument| OsString::from(*argument))
            .collect();
        let stdout = self.run_ok(Mode::Query, argv)?;
        let mut effective: BTreeMap<String, String> = BTreeMap::new();
        for record in stdout.split(|byte| *byte == 0) {
            if record.is_empty() {
                continue;
            }
            let (key, value) = match record.iter().position(|byte| *byte == b'\n') {
                Some(split) => (&record[..split], &record[split + 1..]),
                None => (record, &record[..0]),
            };
            // The list prints every key in lowercase; Git matches
            // configuration keys case-insensitively, so the lookup
            // lowercases the asked-for key the same way.
            effective.insert(
                String::from_utf8_lossy(key).to_lowercase(),
                String::from_utf8_lossy(value).into_owned(),
            );
        }
        Ok(keys
            .iter()
            .filter_map(|key| {
                effective
                    .get(&key.to_lowercase())
                    .cloned()
                    .map(|value| (key.to_string(), value))
            })
            .collect())
    }

    /// Returns every configuration entry whose key matches `pattern`, from
    /// `git config --get-regexp`.
    pub fn config_matches(&self, pattern: &str) -> Result<Vec<(String, String)>, GitError> {
        let argv: Vec<OsString> = ["config", "--get-regexp", pattern]
            .iter()
            .map(|arg| OsString::from(*arg))
            .collect();
        let output = self.run(Mode::Query, argv.clone())?;
        let Some(stdout) = optional_success(&argv, output)? else {
            return Ok(Vec::new());
        };
        let mut entries = Vec::new();
        for line in stdout.split(|&byte| byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let text = String::from_utf8_lossy(line).into_owned();
            match text.split_once(' ') {
                Some((key, value)) => entries.push((key.to_string(), value.to_string())),
                None => entries.push((text, String::new())),
            }
        }
        Ok(entries)
    }

    /// Writes one configuration entry to the repository-local file through
    /// Git's own configuration interface (SPEC 8.2). The call replaces
    /// every prior value of `key` and leaves every other entry untouched.
    ///
    /// `--end-of-options` keeps a value that starts with a dash, such as an
    /// editor argument, from being parsed as an option (SPEC 14.1).
    pub fn set_local_config(&self, key: &str, value: &str) -> Result<(), GitError> {
        self.run_ok(
            Mode::Mutate,
            ["config", "--local", "--end-of-options", key, value],
        )?;
        Ok(())
    }

    /// Returns the index paths that differ from `treeish`, from
    /// `git diff-index --cached --name-only -z`. An empty list means the
    /// index is clean against that tree (SPEC 3.1).
    pub fn staged_against(&self, treeish: &str) -> Result<Vec<OsString>, GitError> {
        let stdout = self.run_ok(
            Mode::Query,
            ["diff-index", "--cached", "--name-only", "-z", treeish],
        )?;
        Ok(split_nul(&stdout)
            .map(|record| os_string_from_bytes(record.to_vec()))
            .collect())
    }

    /// Returns the working-tree paths whose content or mode differs from
    /// the index, from `git diff-files --name-only -z`. Untracked paths
    /// never appear; an empty list means every tracked working file matches
    /// its index entry.
    ///
    /// Git answers from the index stat cache, so an unchanged file costs no
    /// content read, which keeps one query inexpensive (SPEC 6.3). The call
    /// only observes: query mode sets `GIT_OPTIONAL_LOCKS=0`, so the cached
    /// stat data is read, never rewritten.
    pub fn changed_against_index(&self) -> Result<Vec<OsString>, GitError> {
        let stdout = self.run_ok(Mode::Query, ["diff-files", "--name-only", "-z"])?;
        Ok(split_nul(&stdout)
            .map(|record| os_string_from_bytes(record.to_vec()))
            .collect())
    }

    /// Returns the index entries that carry a flag Git itself obeys by
    /// reporting no change for the path, from `git ls-files -v -z`: one
    /// tag byte, one space, and the NUL-terminated path. `S` marks
    /// skip-worktree and a lowercase tag marks assume-unchanged; every
    /// other entry is reported normally by the diff calls above and stays
    /// out of this list (SPEC 3.1).
    ///
    /// The flags decide what [`Git::changed_against_index`] and `git
    /// status` show, so a caller comparing scoped content must consult
    /// this list beside them. The call only observes: it sets no flag and
    /// clears none, exactly as the user left the index (SPEC 14.1).
    pub fn hidden_index_paths(&self) -> Result<Vec<(OsString, IndexFlag)>, GitError> {
        let stdout = self.run_ok(Mode::Query, ["ls-files", "-v", "-z"])?;
        let records = split_nul(&stdout)
            .map(parse_ls_files_verbose_record)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| GitError::MalformedOutput {
                command: "ls-files -v".to_string(),
                detail: "expected `<tag> <path>` records".to_string(),
            })?;
        Ok(records
            .into_iter()
            .filter_map(|(path, flag)| flag.map(|flag| (path, flag)))
            .collect())
    }

    /// Returns the paths whose entry differs between the trees of two
    /// commits, from `git diff-tree -r --name-only --no-commit-id -z`.
    /// Rename detection stays off: a rename is a deletion plus an addition
    /// (SPEC 3.2 rule 6). An empty list means the trees hold equal entries.
    ///
    /// Both revisions must be validated canonical commit object IDs from
    /// this adapter, never user strings (SPEC 14.1).
    pub fn changed_between_commits(
        &self,
        base: &str,
        target: &str,
    ) -> Result<Vec<OsString>, GitError> {
        let stdout = self.run_ok(
            Mode::Query,
            [
                "diff-tree",
                "-r",
                "--name-only",
                "--no-commit-id",
                "-z",
                base,
                target,
            ],
        )?;
        Ok(split_nul(&stdout)
            .map(|record| os_string_from_bytes(record.to_vec()))
            .collect())
    }

    /// The fixed prefix of every source-patch process (SPEC 10.1). It pins
    /// every setting that repository or display configuration could change,
    /// so identical trees always produce identical patch bytes:
    ///
    /// - `core.quotePath=false` and `diff.suppressBlankEmpty=false` keep
    ///   header names and blank context lines byte-stable
    /// - `--no-color`, `--no-ext-diff`, and `--no-textconv` exclude
    ///   terminal, driver, and conversion decoration
    /// - `--no-renames` keeps a rename a deletion plus an addition
    ///   (SPEC 3.2 rule 6)
    /// - `--no-indent-heuristic`, `--diff-algorithm=myers`,
    ///   `--inter-hunk-context=0`, and `--unified=3` fix the algorithm and
    ///   its context
    /// - `--full-index`, `--src-prefix=a/`, and `--dst-prefix=b/` fix the
    ///   object identifiers and name prefixes
    /// - `--binary` keeps opaque content as a complete binary patch, and
    ///   `--no-relative` ignores `diff.relative`
    ///
    /// These choices are part of the engine contract version that every
    /// review identity covers (SPEC 8.6 `ENGINE_VERSION`).
    const DIFF_TREE_PATCH_ARGV: &[&str] = &[
        "-c",
        "core.quotePath=false",
        "-c",
        "diff.suppressBlankEmpty=false",
        "diff-tree",
        "-r",
        "-p",
        "--binary",
        "--no-commit-id",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--no-indent-heuristic",
        "--diff-algorithm=myers",
        "--inter-hunk-context=0",
        "--full-index",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "--no-relative",
        "--unified=3",
    ];

    /// Largest number of pathspecs one patch process receives. Batching
    /// keeps a whole-scope comparison from scaling one process per document
    /// (SPEC 13.3) while argv stays far below the operating-system limit.
    const PATCH_PATHSPEC_BATCH: usize = 256;

    /// Generates the source patches of `paths` between two commits under
    /// the fixed diff contract of SPEC 10.1 (see [`Self::DIFF_TREE_PATCH_ARGV`]).
    ///
    /// The returned bytes are the concatenation of complete per-file
    /// patches in Git's own order, each holding its original structural
    /// headers, every hunk, and any binary payload. Whitespace and final
    /// newlines stay exact; nothing is renamed, converted, or filtered. An
    /// empty `paths` list runs no process at all and returns no bytes
    /// (SPEC 14.1).
    ///
    /// Every path is one literal pathspec: this adapter sets
    /// `GIT_LITERAL_PATHSPECS`, so glob characters and unusual names match
    /// themselves (SPEC 3.3), and the paths after `--` never read as
    /// options. Paths are repository-relative and resolve against the
    /// adapter's working directory, which callers anchor at the discovery
    /// root. Both revisions must be validated canonical commit object IDs
    /// from this adapter, never user strings (SPEC 14.1).
    pub fn diff_tree_patch(
        &self,
        base: &str,
        target: &str,
        paths: &[OsString],
    ) -> Result<Vec<u8>, GitError> {
        let mut output = Vec::new();
        for chunk in paths.chunks(Self::PATCH_PATHSPEC_BATCH) {
            let mut argv: Vec<OsString> = Self::DIFF_TREE_PATCH_ARGV
                .iter()
                .map(|argument| OsString::from(*argument))
                .collect();
            argv.push(base.into());
            argv.push(target.into());
            argv.push("--".into());
            argv.extend(chunk.iter().cloned());
            output.extend_from_slice(&self.run_ok(Mode::Query, argv)?);
        }
        Ok(output)
    }

    /// Returns the effective attribute assignments of `paths`, through Git's
    /// effective-attribute interface `git check-attr -z --stdin`
    /// (SPEC 8.8). Paths stay literal data; every attribute of
    /// `attributes` is queried for every path.
    pub fn check_attr(
        &self,
        attributes: &[&str],
        paths: &[OsString],
    ) -> Result<Vec<AttributeAssignment>, GitError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut argv: Vec<OsString> = vec!["check-attr".into(), "-z".into(), "--stdin".into()];
        argv.extend(attributes.iter().map(|attribute| (*attribute).into()));
        let mut input = Vec::new();
        for path in paths {
            input.extend_from_slice(path.as_encoded_bytes());
            input.push(0);
        }
        let stdout = require_success(&argv, self.run_stdin(Mode::Query, argv.clone(), &input)?)?;
        let records: Vec<&[u8]> = split_nul(&stdout).collect();
        if !records.len().is_multiple_of(3) {
            return Err(GitError::MalformedOutput {
                command: "check-attr".to_string(),
                detail: format!(
                    "expected path, attribute, and value triples, got {} fields",
                    records.len()
                ),
            });
        }
        Ok(records
            .as_chunks::<3>()
            .0
            .iter()
            .map(|triple| AttributeAssignment {
                path: os_string_from_bytes(triple[0].to_vec()),
                attribute: String::from_utf8_lossy(triple[1]).into_owned(),
                value: String::from_utf8_lossy(triple[2]).into_owned(),
            })
            .collect())
    }

    /// Loads `treeish` into the current index, or the `GIT_INDEX_FILE`
    /// override, without any working-tree update flag: the disposable-index
    /// seeding of SPEC 9.3. A commit names its own tree.
    ///
    /// `treeish` must be a validated canonical object ID from this adapter,
    /// never a user string (SPEC 14.1).
    pub fn read_tree(&self, treeish: &str) -> Result<(), GitError> {
        self.run_ok(Mode::Mutate, ["read-tree", treeish])?;
        Ok(())
    }

    /// Prepares the current index, or the `GIT_INDEX_FILE` override, as an
    /// empty tree: the disposable-index starting point of SPEC 9.3.
    pub fn read_tree_empty(&self) -> Result<(), GitError> {
        self.run_ok(Mode::Mutate, ["read-tree", "--empty"])?;
        Ok(())
    }

    /// The stage-zero entry of one path, from
    /// `git ls-files --stage -z -- <path>`: its mode and blob object ID,
    /// or `None` when the index holds no entry for the path. One probe
    /// read of one path never lists the whole index (SPEC 17.5).
    pub fn staged_entry_of(&self, path: &OsStr) -> Result<Option<(String, String)>, GitError> {
        let argv: Vec<OsString> = ["ls-files", "--stage", "-z", "--"]
            .iter()
            .map(|argument| OsString::from(*argument))
            .chain([path.to_os_string()])
            .collect();
        let stdout = self.run_ok(Mode::Query, argv)?;
        Ok(split_nul(&stdout)
            .filter_map(parse_ls_files_record)
            .find(|entry| entry.stage == 0)
            .map(|entry| (entry.mode, entry.oid)))
    }

    /// Reads many blob objects through chunked `git cat-file --batch`
    /// processes (SPEC 17.5: a whole-scope comparison never scales one
    /// process per document).
    ///
    /// One `--batch-check` pass reads every size first, so the content
    /// passes stay under the capture limit whatever the blobs hold: chunks
    /// of at most [`Self::BLOB_BATCH_BYTES`] are read whole. Larger blobs
    /// use an individual read without batch framing, up to the adapter's
    /// output limit. Replacement objects stay disabled, exactly as
    /// [`Git::read_blob`] disables them. Every object ID must be a
    /// validated canonical object ID from this adapter, never a user
    /// string (SPEC 14.1). An empty list runs no process at all
    /// (SPEC 14.1).
    pub fn read_blobs(&self, oids: &[String]) -> Result<BTreeMap<String, Vec<u8>>, GitError> {
        if oids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let malformed = |detail: String| GitError::MalformedOutput {
            command: "cat-file --batch".to_string(),
            detail,
        };
        let mut input = Vec::new();
        for oid in oids {
            input.extend_from_slice(oid.as_bytes());
            input.push(b'\n');
        }
        let check_argv: Vec<OsString> = [
            "--no-replace-objects",
            "cat-file",
            "--batch-check=%(objectname) %(objecttype) %(objectsize)",
        ]
        .iter()
        .map(|argument| OsString::from(*argument))
        .collect();
        let check = self.run_stdin(Mode::Query, check_argv.clone(), &input)?;
        let check = require_success(&check_argv, check)?;
        let mut sizes: BTreeMap<String, u64> = BTreeMap::new();
        for line in check.split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let text = String::from_utf8_lossy(line).into_owned();
            let mut fields = text.split(' ');
            let oid = fields.next();
            let kind = fields.next();
            let size = fields.next();
            let (Some(oid), Some(kind), Some(size)) = (oid, kind, size) else {
                // A missing object answers two fields: its ID and the
                // word `missing`, with no size.
                if oid.is_some() && kind == Some("missing") && size.is_none() {
                    return Err(missing_object_failure(&check_argv, &text));
                }
                return Err(malformed(format!("not a batch-check line: {text}")));
            };
            if kind != "blob" {
                return Err(malformed(format!("object {oid} is a {kind}, not a blob")));
            }
            let size = size
                .parse::<u64>()
                .map_err(|_| malformed(format!("not a byte size: {size}")))?;
            sizes.insert(oid.to_string(), size);
        }
        if sizes.len() != oids.len() {
            return Err(malformed(format!(
                "expected {} object(s), the batch-check answered {}",
                oids.len(),
                sizes.len()
            )));
        }
        let mut blobs: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        let mut chunk: Vec<String> = Vec::new();
        let mut chunk_bytes: u64 = 0;
        for oid in oids {
            let size = *sizes
                .get(oid)
                .ok_or_else(|| malformed(format!("object {oid} was never checked")))?;
            if size > Self::BLOB_BATCH_BYTES as u64 {
                self.read_blob_chunk(&chunk, &mut blobs)?;
                chunk.clear();
                chunk_bytes = 0;
                // A batch budget does not reduce the per-file limit. Read
                // this blob without batch framing so a file at the output
                // limit still fits, and let the adapter enforce that limit.
                blobs.insert(oid.clone(), self.read_blob(oid)?);
                continue;
            }
            if chunk_bytes + size > Self::BLOB_BATCH_BYTES as u64 && !chunk.is_empty() {
                self.read_blob_chunk(&chunk, &mut blobs)?;
                chunk.clear();
                chunk_bytes = 0;
            }
            chunk.push(oid.clone());
            chunk_bytes += size;
        }
        self.read_blob_chunk(&chunk, &mut blobs)?;
        Ok(blobs)
    }

    /// The largest number of content bytes one `cat-file --batch` chunk of
    /// [`Git::read_blobs`] reads, kept under the captured-output limit of
    /// [`Limits`].
    const BLOB_BATCH_BYTES: usize = 12 * 1024 * 1024;

    /// Reads one chunk of blobs into `blobs`.
    fn read_blob_chunk(
        &self,
        chunk: &[String],
        blobs: &mut BTreeMap<String, Vec<u8>>,
    ) -> Result<(), GitError> {
        if chunk.is_empty() {
            return Ok(());
        }
        let mut input = Vec::new();
        for oid in chunk {
            input.extend_from_slice(oid.as_bytes());
            input.push(b'\n');
        }
        let argv: Vec<OsString> = ["--no-replace-objects", "cat-file", "--batch"]
            .iter()
            .map(|argument| OsString::from(*argument))
            .collect();
        let output = self.run_stdin(Mode::Query, argv.clone(), &input)?;
        let stdout = crate::git::require_success(&argv, output)?;
        let malformed = |detail: String| GitError::MalformedOutput {
            command: "cat-file --batch".to_string(),
            detail,
        };
        let mut rest = stdout.as_slice();
        while !rest.is_empty() {
            let Some(header_end) = rest.iter().position(|byte| *byte == b'\n') else {
                return Err(malformed("a header line never ended".to_string()));
            };
            let header = String::from_utf8_lossy(&rest[..header_end]).into_owned();
            rest = &rest[header_end + 1..];
            let mut fields = header.split(' ');
            let Some(oid) = fields.next().map(str::to_string) else {
                return Err(malformed("an empty header line".to_string()));
            };
            let kind = fields.next().unwrap_or("");
            if kind == "missing" {
                return Err(missing_object_failure(&argv, &header));
            }
            if kind != "blob" {
                return Err(malformed(format!("object {oid} is a {kind}, not a blob")));
            }
            let Some(size) = fields.next().and_then(|raw| raw.parse::<usize>().ok()) else {
                return Err(malformed(format!("no byte size for {oid}")));
            };
            if rest.len() < size + 1 {
                return Err(malformed(format!("fewer than {size} bytes followed {oid}")));
            }
            blobs.insert(oid, rest[..size].to_vec());
            rest = &rest[size + 1..];
        }
        Ok(())
    }

    /// Computes the blob object IDs of many byte strings through one
    /// `git hash-object -t blob --stdin-paths` process over one private
    /// scratch directory (SPEC 17.5: identity work never scales one
    /// process per identity).
    ///
    /// The hashed bytes travel through files of one directory the operating
    /// system gives this process alone, created with owner-only
    /// permissions and removed before the call returns, whatever it
    /// answers. `git hash-object --stdin-paths` hashes each file's exact
    /// bytes without storing an object — the same hash
    /// [`Git::hash_object`] computes for the same bytes, through the same
    /// Git. An empty list runs no process at all (SPEC 14.1).
    pub fn hash_objects(&self, inputs: &[Vec<u8>]) -> Result<Vec<String>, GitError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let directory = std::env::temp_dir().join(format!(
            "margin-hash-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&directory).map_err(|source| GitError::Io {
            context: "create the identity scratch directory".to_string(),
            source,
        })?;
        // Owner-only permissions: identity inputs carry document content.
        let _ = std::fs::set_permissions(
            &directory,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        );
        let result = self.hash_objects_in(&directory, inputs);
        let _ = std::fs::remove_dir_all(&directory);
        result
    }

    /// One `hash-object --stdin-paths` run over `directory`.
    fn hash_objects_in(
        &self,
        directory: &Path,
        inputs: &[Vec<u8>],
    ) -> Result<Vec<String>, GitError> {
        let mut paths = String::new();
        for (index, input) in inputs.iter().enumerate() {
            let path = directory.join(format!("input-{index:08}"));
            std::fs::write(&path, input).map_err(|source| GitError::Io {
                context: "write one identity input".to_string(),
                source,
            })?;
            let _ = std::fs::set_permissions(
                &path,
                std::os::unix::fs::PermissionsExt::from_mode(0o600),
            );
            paths.push_str(&path.to_string_lossy());
            paths.push('\n');
        }
        let hash_argv: Vec<OsString> = ["hash-object", "-t", "blob", "--stdin-paths"]
            .iter()
            .map(|argument| OsString::from(*argument))
            .collect();
        let stdout = self.run_stdin(Mode::Query, hash_argv.clone(), paths.as_bytes())?;
        let stdout = require_success(&hash_argv, stdout)?;
        let oids: Vec<String> = stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                validate_oid(line).ok_or_else(|| GitError::MalformedOutput {
                    command: "hash-object".to_string(),
                    detail: format!(
                        "not a full lowercase hex object ID: {}",
                        String::from_utf8_lossy(line)
                    ),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if oids.len() != inputs.len() {
            return Err(GitError::MalformedOutput {
                command: "hash-object".to_string(),
                detail: format!("expected {} object ID(s), got {}", inputs.len(), oids.len()),
            });
        }
        Ok(oids)
    }

    /// Writes the current index as a tree and returns its object ID from
    /// `git write-tree`. The call stores one tree object and touches no ref
    /// and no working file, but it rewrites the index it addresses to store
    /// the computed tree cache — optional locks off or not — so a caller
    /// that must leave the real index untouched points `GIT_INDEX_FILE` at
    /// one disposable copy first. It fails on unresolved conflict stages.
    pub fn write_tree(&self) -> Result<String, GitError> {
        let stdout = self.run_ok(Mode::Query, ["write-tree"])?;
        validate_oid(strip_trailing_newline(&stdout)).ok_or_else(|| GitError::MalformedOutput {
            command: "write-tree".to_string(),
            detail: format!(
                "not a full lowercase hex object ID: {:?}",
                String::from_utf8_lossy(strip_trailing_newline(&stdout))
            ),
        })
    }

    /// Commits the current index with the exact bytes of `message_file`
    /// through `git commit --file --cleanup=verbatim --allow-empty`, and
    /// returns the new `HEAD` commit object ID (SPEC 9.2).
    ///
    /// Hooks stay in force: the adapter passes no `--no-verify`, no hook-path
    /// override, and no signing override, so a hook can still reject the
    /// commit or alter its message file (SPEC 14.1). Callers must verify the
    /// committed parent, tree, and trailers afterwards.
    pub fn commit_index(&self, message_file: &Path) -> Result<String, GitError> {
        let argv: Vec<OsString> = [
            "commit",
            "--quiet",
            "--allow-empty",
            "--cleanup=verbatim",
            "--file",
        ]
        .iter()
        .map(|arg| OsString::from(*arg))
        .chain([message_file.as_os_str().to_os_string()])
        .collect();
        self.run_ok(Mode::Mutate, argv)?;
        self.head_commit()?
            .ok_or_else(|| GitError::MalformedOutput {
                command: "commit".to_string(),
                detail: "the commit reported success but HEAD does not resolve".to_string(),
            })
    }

    /// Reads one commit object's tree, parents, and exact message bytes from
    /// `git cat-file commit` (SPEC 8.4).
    ///
    /// Replacement objects are disabled for this read, so a `refs/replace/`
    /// entry cannot substitute other bytes for the named object. `commit`
    /// must be a validated canonical object ID from this adapter, never a
    /// user string (SPEC 14.1).
    pub fn read_commit(&self, commit: &str) -> Result<CommitObject, GitError> {
        let stdout = self.run_ok(
            Mode::Query,
            ["--no-replace-objects", "cat-file", "commit", commit],
        )?;
        parse_commit_object(&stdout)
    }

    /// Reads one blob object's exact bytes through `git cat-file blob`
    /// (SPEC 10.2 step 3: the source a comparison groups by).
    ///
    /// Replacement objects are disabled for this read, so a `refs/replace/`
    /// entry cannot substitute other bytes for the named object. `oid` must
    /// be a validated canonical object ID from this adapter, never a user
    /// string (SPEC 14.1). The [`Limits`] capture bound applies: a larger
    /// blob fails with a typed error instead of truncating source bytes.
    pub fn read_blob(&self, oid: &str) -> Result<Vec<u8>, GitError> {
        self.run_ok(
            Mode::Query,
            ["--no-replace-objects", "cat-file", "blob", oid],
        )
    }

    /// Applies one patch to the index only, through
    /// `git -c apply.ignoreWhitespace=false apply --cached
    /// --whitespace=nowarn` (SPEC 9.3-9.4): the same explicit whitespace
    /// policy for applicability checks and disposable-index previews.
    ///
    /// The call writes the current index or the `GIT_INDEX_FILE` override
    /// and never the working tree, so a disposable index turns it into one
    /// exact application check. `reverse` restores the patch's old side.
    /// The patch arrives on standard input, never as command syntax; a
    /// nonapplicable patch is one typed failure, never a partial `.rej`
    /// write.
    pub fn apply_patch_cached(&self, patch: &[u8], reverse: bool) -> Result<(), GitError> {
        self.apply_patch_documents_cached(&[patch], reverse)
    }

    /// Applies several complete patch documents in one process, in the
    /// order given: the documents are concatenated into one patch series,
    /// which Git applies file section by file section exactly as separate
    /// invocations would (SPEC 9.3-9.4, 17.5 — one composition proof, one
    /// process). A nonapplicable section fails the whole call, exactly as
    /// the first of the separate invocations would have.
    pub fn apply_patch_documents_cached(
        &self,
        documents: &[&[u8]],
        reverse: bool,
    ) -> Result<(), GitError> {
        let mut series = Vec::new();
        for document in documents {
            series.reserve(document.len() + 1);
            series.extend_from_slice(document);
            // One document that does not end in a newline cannot be the
            // last line of a valid patch; keep the series parseable anyway.
            if !document.ends_with(b"\n") {
                series.push(b'\n');
            }
        }
        let mut argv: Vec<OsString> = vec![
            "-c".into(),
            "apply.ignoreWhitespace=false".into(),
            "apply".into(),
            "--cached".into(),
            "--whitespace=nowarn".into(),
        ];
        if reverse {
            argv.push("--reverse".into());
        }
        argv.push("-".into());
        let output = self.run_stdin(Mode::Mutate, &argv, &series)?;
        require_success(&argv, output).map(|_| ())
    }

    /// Applies one patch to the real index and the working tree through
    /// `git -c apply.ignoreWhitespace=false apply --index --reverse
    /// --whitespace=nowarn` (SPEC 9.4 step 2), or only checks that the
    /// patch applies to both with `--check` (SPEC 9.4 step 1).
    ///
    /// This is the one real mutation of a restoring review: without
    /// `check` the call stages the patched entries in the repository's
    /// real index and rewrites the patched working files, which is why the
    /// callers prove the intended result in one disposable index first and
    /// verify the real state afterwards. The whitespace policy is the one
    /// the applicability check and the disposable-index preview used, and
    /// `reverse` restores the patch's old side. The patch arrives on
    /// standard input, never as command syntax; a nonapplicable patch is
    /// one typed failure, never a partial `.rej` write.
    pub fn apply_patch_index(
        &self,
        patch: &[u8],
        reverse: bool,
        check: bool,
    ) -> Result<(), GitError> {
        let mut argv: Vec<OsString> = vec![
            "-c".into(),
            "apply.ignoreWhitespace=false".into(),
            "apply".into(),
        ];
        if check {
            argv.push("--check".into());
        }
        argv.push("--index".into());
        if reverse {
            argv.push("--reverse".into());
        }
        argv.push("--whitespace=nowarn".into());
        argv.push("-".into());
        let output = self.run_stdin(Mode::Mutate, &argv, patch)?;
        require_success(&argv, output).map(|_| ())
    }

    /// Returns the unfiltered first-parent ancestry of `commit`, newest
    /// first, including `commit` itself, from
    /// `git rev-list --first-parent`.
    ///
    /// The listing takes no pathspec: an empty review commit stays in the
    /// walk, while a path-limited log could drop it (SPEC 8.5). Replacement
    /// objects stay disabled, so the walk lists the same objects
    /// [`Git::read_commit`] validates. `commit` must be a validated
    /// canonical object ID from this adapter, never a user string (SPEC
    /// 14.1). The [`Limits`] capture bound applies: a longer listing fails
    /// with a typed error instead of truncating history silently.
    pub fn first_parent_history(&self, commit: &str) -> Result<Vec<String>, GitError> {
        let stdout = self.run_ok(
            Mode::Query,
            ["--no-replace-objects", "rev-list", "--first-parent", commit],
        )?;
        parse_rev_list_oids(&stdout)
    }

    /// Reads many commit objects through one `git cat-file --batch`
    /// process, so a history walk never scales one process per commit
    /// (SPEC 17.5).
    ///
    /// Replacement objects stay disabled, exactly as [`Git::read_commit`]
    /// disables them, and every object goes through the same strict parse.
    /// Every object ID must be a validated canonical object ID from this
    /// adapter, never a user string (SPEC 14.1). An object the batch cannot
    /// return is one typed failure: the walk that named it cannot validate
    /// the history it asked for. An empty list runs no process at all
    /// (SPEC 14.1).
    pub fn read_commits_batch(
        &self,
        commits: &[String],
    ) -> Result<BTreeMap<String, CommitObject>, GitError> {
        if commits.is_empty() {
            return Ok(BTreeMap::new());
        }
        let argv: Vec<OsString> = ["--no-replace-objects", "cat-file", "--batch"]
            .iter()
            .map(|argument| OsString::from(*argument))
            .collect();
        let mut input = Vec::new();
        for commit in commits {
            input.extend_from_slice(commit.as_bytes());
            input.push(b'\n');
        }
        let output = self.run_stdin(Mode::Query, argv.clone(), &input)?;
        let stdout = require_success(&argv, output)?;
        let malformed = |detail: String| GitError::MalformedOutput {
            command: "cat-file --batch".to_string(),
            detail,
        };
        let mut objects = BTreeMap::new();
        let mut rest = stdout.as_slice();
        while !rest.is_empty() {
            let Some(header_end) = rest.iter().position(|byte| *byte == b'\n') else {
                return Err(malformed("a header line never ended".to_string()));
            };
            let header = String::from_utf8_lossy(&rest[..header_end]).into_owned();
            rest = &rest[header_end + 1..];
            let mut fields = header.split(' ');
            let Some(oid) = fields.next().map(str::to_string) else {
                return Err(malformed("an empty header line".to_string()));
            };
            if validate_oid(oid.as_bytes()).is_none() {
                return Err(malformed(format!("not an object ID: {oid}")));
            }
            let kind = fields.next().unwrap_or("");
            if kind == "missing" {
                return Err(missing_object_failure(&argv, &header));
            }
            if kind != "commit" {
                return Err(malformed(format!("object {oid} is a {kind}, not a commit")));
            }
            let Some(size) = fields.next().and_then(|raw| raw.parse::<usize>().ok()) else {
                return Err(malformed(format!("no byte size for {oid}")));
            };
            if rest.len() < size + 1 {
                return Err(malformed(format!("fewer than {size} bytes followed {oid}")));
            }
            let object = parse_commit_object(&rest[..size])
                .map_err(|_| malformed(format!("commit {oid} is not a commit object")))?;
            objects.insert(oid, object);
            rest = &rest[size + 1..];
        }
        Ok(objects)
    }

    /// The paths each commit changed against its first parent, through one
    /// `git diff-tree -r -z --stdin` process fed the commit list, so a
    /// history walk never scales one process per commit (SPEC 17.5).
    ///
    /// Every commit of the input holds an entry: Git prints no section for
    /// a commit whose diff is empty — a root commit without `--root` and a
    /// merge commit without `-m` both list nothing — and that silence is
    /// the one-commit form's ordinary answer of "changed nothing". For a
    /// commit with exactly one parent the listed paths are the paths whose
    /// tree entry (mode or object ID) differs from that parent's, which is
    /// the same set a comparison of two full tree listings derives. Rename
    /// detection stays off: a rename is a deletion plus an addition
    /// (SPEC 3.2 rule 6). Every object ID must be a validated canonical
    /// object ID from this adapter, never a user string (SPEC 14.1). An
    /// empty list runs no process at all (SPEC 14.1).
    pub fn first_parent_changed_paths_batch(
        &self,
        commits: &[String],
    ) -> Result<BTreeMap<String, Vec<OsString>>, GitError> {
        if commits.is_empty() {
            return Ok(BTreeMap::new());
        }
        let argv: Vec<OsString> = ["--no-replace-objects", "diff-tree", "-r", "-z", "--stdin"]
            .iter()
            .map(|argument| OsString::from(*argument))
            .collect();
        let mut input = Vec::new();
        for commit in commits {
            input.extend_from_slice(commit.as_bytes());
            input.push(b'\n');
        }
        let output = self.run_stdin(Mode::Query, argv.clone(), &input)?;
        let stdout = require_success(&argv, output)?;
        let malformed = |detail: String| GitError::MalformedOutput {
            command: "diff-tree --stdin".to_string(),
            detail,
        };
        let mut changed: BTreeMap<String, Vec<OsString>> = BTreeMap::new();
        // Every commit of the input starts at no changes: Git prints no
        // section at all for a commit whose diff is empty, so silence means
        // "changed nothing", not "was never read".
        for commit in commits {
            changed.entry(commit.clone()).or_default();
        }
        // The commit line whose changes are being read; the input order,
        // never the map's key order, decides it.
        let mut open: Option<String> = None;
        let mut records = split_nul(&stdout);
        while let Some(record) = records.next() {
            if record.first() == Some(&b':') {
                // A change header: `:<mode> <mode> <oid> <oid> <status>`.
                // The path is the next record, unconditionally, so a path
                // that itself starts with a colon cannot be misread.
                let Some(path) = records.next() else {
                    return Err(malformed("a change header without its path".to_string()));
                };
                let commit = open
                    .clone()
                    .ok_or_else(|| malformed("a change before any commit line".to_string()))?;
                changed
                    .entry(commit)
                    .or_default()
                    .push(os_string_from_bytes(path.to_vec()));
                continue;
            }
            let Some(oid) = validate_oid(record) else {
                return Err(malformed(format!(
                    "not a commit line: {}",
                    String::from_utf8_lossy(record)
                )));
            };
            open = Some(oid.to_string());
            changed.entry(oid.to_string()).or_default();
        }
        Ok(changed)
    }

    /// Returns every replacement ref name from
    /// `git for-each-ref refs/replace/`. A nonempty list means the
    /// repository's ordinary history view depends on replacement objects
    /// (SPEC 8.4 rule 7).
    pub fn replace_ref_names(&self) -> Result<Vec<String>, GitError> {
        let stdout = self.run_ok(
            Mode::Query,
            ["for-each-ref", "--format=%(refname)", "refs/replace/"],
        )?;
        Ok(String::from_utf8_lossy(&stdout)
            .lines()
            .map(str::to_string)
            .collect())
    }
}

/// One raw commit object read back from the object database (SPEC 8.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitObject {
    /// The commit's full tree object ID.
    pub tree: String,
    /// The parent commit object IDs, in recorded order.
    pub parents: Vec<String>,
    /// The committer timestamp of the `committer` header, in seconds since
    /// the Unix epoch, when the header parses. The timeline of SPEC 5.5
    /// displays it while ordering by ancestry, never by clock.
    pub committer_time: Option<i64>,
    /// The exact message bytes stored after the header block.
    pub message: Vec<u8>,
}

/// Parses the newline-delimited commit object IDs of `git rev-list`. Every
/// line must be one full lowercase hex object ID of either documented
/// width, and the listing must name at least the requested commit itself.
fn parse_rev_list_oids(bytes: &[u8]) -> Result<Vec<String>, GitError> {
    let malformed = |detail: String| GitError::MalformedOutput {
        command: "rev-list".to_string(),
        detail,
    };
    let oids = String::from_utf8_lossy(bytes)
        .lines()
        .map(|line| {
            validate_oid(line.as_bytes())
                .ok_or_else(|| malformed(format!("not a full lowercase hex object ID: {line:?}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if oids.is_empty() {
        return Err(malformed("the listing names no commit".to_string()));
    }
    Ok(oids)
}

/// Parses one raw `commit` object: header lines, one blank line, then the
/// exact message bytes (SPEC 8.4). Unknown headers are skipped; a
/// continuation line, such as a `gpgsig` block, extends the header above it.
fn parse_commit_object(bytes: &[u8]) -> Result<CommitObject, GitError> {
    let malformed = |detail: &str| GitError::MalformedOutput {
        command: "cat-file commit".to_string(),
        detail: detail.to_string(),
    };
    let mut rest = bytes;
    let mut tree: Option<String> = None;
    let mut parents = Vec::new();
    let mut committer_time = None;
    let mut saw_header = false;
    loop {
        let line_end = rest
            .iter()
            .position(|&byte| byte == b'\n')
            .ok_or_else(|| malformed("the header block has no blank line"))?;
        let line = &rest[..line_end];
        rest = &rest[line_end + 1..];
        if line.is_empty() {
            break;
        }
        // A leading space continues the preceding header, whatever it names.
        if line[0] == b' ' {
            if !saw_header {
                return Err(malformed("a continuation line has no header"));
            }
            continue;
        }
        saw_header = true;
        let separator = line
            .iter()
            .position(|&byte| byte == b' ')
            .ok_or_else(|| malformed("a header line has no value"))?;
        let (key, value) = (&line[..separator], &line[separator + 1..]);
        match key {
            b"tree" => {
                if tree.is_some() {
                    return Err(malformed("the tree header appears twice"));
                }
                tree = Some(
                    validate_oid(value)
                        .ok_or_else(|| malformed("the tree header has no full object ID"))?,
                );
            }
            b"parent" => {
                parents.push(
                    validate_oid(value)
                        .ok_or_else(|| malformed("a parent header has no full object ID"))?,
                );
            }
            b"committer" => {
                committer_time = committer_timestamp(value);
            }
            _ => {}
        }
    }
    Ok(CommitObject {
        tree: tree.ok_or_else(|| malformed("the tree header is missing"))?,
        parents,
        committer_time,
        // Everything after the blank line is the exact message.
        message: rest.to_vec(),
    })
}

/// Reads the timestamp of one `committer` header value
/// (`Name <email> <seconds> <offset>`): the second token from the end. The
/// timestamp stays optional, because this parser holds only the fields its
/// callers judge; a header that does not parse names no time, never a
/// fabricated one.
fn committer_timestamp(value: &[u8]) -> Option<i64> {
    let seconds = value.split(|&byte| byte == b' ').nth_back(1)?;
    std::str::from_utf8(seconds).ok()?.parse().ok()
}

fn collect_argv<I, S>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    args.into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect()
}

fn require_success(argv: &[OsString], output: GitOutput) -> Result<Vec<u8>, GitError> {
    if output.success() {
        Ok(output.stdout)
    } else {
        Err(GitError::Exit {
            argv: describe_argv(argv),
            exit_code: output.exit_code,
            stderr: truncate_detail(output.stderr),
        })
    }
}

/// The typed failure for one object a batch protocol reports missing.
///
/// `cat-file --batch` prints `<oid> missing` and exits zero, while the
/// equivalent one-object call exits nonzero. A missing object is the
/// repository's own answer, not a protocol surprise, so it maps to the
/// same execution failure that one-object call returns — `GIT_FAILED`
/// under SPEC 7.5 — carrying the protocol's own line as the detail.
fn missing_object_failure(argv: &[OsString], line: &str) -> GitError {
    GitError::Exit {
        argv: describe_argv(argv),
        exit_code: Some(128),
        stderr: truncate_detail(line.as_bytes().to_vec()),
    }
}

/// Interprets the raw result of one probe command: success returns stdout,
/// exit status 1 returns `None`, and every other outcome is an error.
fn optional_success(argv: &[OsString], output: GitOutput) -> Result<Option<Vec<u8>>, GitError> {
    match output.exit_code {
        Some(0) => Ok(Some(output.stdout)),
        Some(1) => Ok(None),
        _ => Err(GitError::Exit {
            argv: describe_argv(argv),
            exit_code: output.exit_code,
            stderr: truncate_detail(output.stderr),
        }),
    }
}

fn truncate_detail(mut bytes: Vec<u8>) -> Vec<u8> {
    bytes.truncate(MAX_ERROR_DETAIL_BYTES);
    bytes
}

/// Parses one `<mode> SP <oid> SP <stage> TAB <path>` record.
fn parse_ls_files_record(record: &[u8]) -> Option<IndexEntry> {
    let separator = record.iter().position(|&byte| byte == b'\t')?;
    let (meta, path) = (&record[..separator], &record[separator + 1..]);
    let mut fields = meta.split(|&byte| byte == b' ');
    let mode = String::from_utf8(fields.next()?.to_vec()).ok()?;
    let oid = validate_oid(fields.next()?)?;
    let stage = String::from_utf8(fields.next()?.to_vec()).ok()?;
    if fields.next().is_some() {
        return None;
    }
    if mode.len() != 6 || !mode.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let stage = stage.parse::<u32>().ok()?;
    if stage > 3 {
        return None;
    }
    Some(IndexEntry {
        mode,
        oid,
        stage,
        path: os_string_from_bytes(path.to_vec()),
    })
}

/// Parses one `<tag> SP <path>` record of `git ls-files -v -z` into the
/// path and its hiding flag. `None` rejects a malformed record; a parsed
/// record without a hiding flag carries `None` as its flag, because the
/// diff calls report that path normally.
fn parse_ls_files_verbose_record(record: &[u8]) -> Option<(OsString, Option<IndexFlag>)> {
    let (&tag, rest) = record.split_first()?;
    if rest.first() != Some(&b' ') || rest.len() < 2 {
        return None;
    }
    let flag = match tag {
        b'S' => Some(IndexFlag::SkipWorktree),
        tag if tag.is_ascii_lowercase() => Some(IndexFlag::AssumeUnchanged),
        _ => None,
    };
    Some((os_string_from_bytes(rest[1..].to_vec()), flag))
}

/// Parses one `<mode> SP <kind> SP <oid> TAB <path>` record.
fn parse_ls_tree_record(record: &[u8]) -> Option<TreeEntry> {
    let separator = record.iter().position(|&byte| byte == b'\t')?;
    let (meta, path) = (&record[..separator], &record[separator + 1..]);
    let mut fields = meta.split(|&byte| byte == b' ');
    let mode = String::from_utf8(fields.next()?.to_vec()).ok()?;
    let kind = String::from_utf8(fields.next()?.to_vec()).ok()?;
    let oid = validate_oid(fields.next()?)?;
    if fields.next().is_some() {
        return None;
    }
    if mode.len() != 6 || !mode.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    // Recursive listing never reports a tree entry.
    if !matches!(kind.as_str(), "blob" | "commit") {
        return None;
    }
    Some(TreeEntry {
        mode,
        kind,
        oid,
        path: os_string_from_bytes(path.to_vec()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter_with(vars: &[(&str, &str)]) -> Git {
        Git::from_env(
            vars.iter()
                .map(|(key, value)| (OsString::from(*key), OsString::from(*value))),
        )
    }

    #[test]
    fn inherited_redirection_is_refused() {
        let adapter = adapter_with(&[("HOME", "/home"), ("GIT_DIR", "/elsewhere/.git")]);
        let error = adapter.environment_problem().unwrap_err();
        match error {
            GitError::UnexpectedEnvironment { vars } => {
                assert_eq!(vars, vec![OsString::from("GIT_DIR")]);
            }
            other => panic!("unexpected error {other:?}"),
        }
    }

    #[test]
    fn own_overrides_are_allowed() {
        let mut adapter = adapter_with(&[("GIT_INDEX_FILE", "/stolen-index")]);
        assert!(adapter.environment_problem().is_err());
        adapter.set_env_override("GIT_INDEX_FILE", "/tmp/disposable-index");
        assert!(adapter.environment_problem().is_ok());
        assert_eq!(
            adapter.environment.get(OsStr::new("GIT_INDEX_FILE")),
            Some(&OsString::from("/tmp/disposable-index"))
        );
    }

    #[test]
    fn oid_validation_accepts_both_formats_lowercase_only() {
        let sha1 = "0123456789abcdef0123456789abcdef01234567";
        let sha256 = format!("{sha1}0123456789abcdef01234567");
        assert_eq!(sha256.len(), 64);
        assert_eq!(validate_oid(sha1.as_bytes()), Some(sha1.to_string()));
        assert_eq!(validate_oid(sha256.as_bytes()), Some(sha256));
        assert_eq!(validate_oid(sha1.to_uppercase().as_bytes()), None);
        assert_eq!(validate_oid(&sha1.as_bytes()[..39]), None);
        assert_eq!(validate_oid(b"zzz"), None);
        assert_eq!(validate_oid(b""), None);
    }

    #[test]
    fn nul_records_drop_only_the_trailing_empty_record() {
        let records: Vec<&[u8]> = split_nul(b"one\0two with \0\0three\0").collect();
        assert_eq!(
            records,
            vec![&b"one"[..], b"two with ".as_slice(), b"three".as_slice()]
        );
        assert_eq!(split_nul(b"").count(), 0);
    }

    #[test]
    fn ls_files_records_parse_strictly() {
        let oid = "0123456789abcdef0123456789abcdef01234567";
        let record = format!("100644 {oid} 0\tspa ce.md\n").into_bytes();
        let entry = parse_ls_files_record(&record).expect("a valid record parses");
        assert_eq!(entry.mode, "100644");
        assert_eq!(entry.oid, oid);
        assert_eq!(entry.stage, 0);
        assert_eq!(entry.path, OsString::from("spa ce.md\n"));

        assert!(parse_ls_files_record(b"no tab here").is_none());
        assert!(parse_ls_files_record("100644 not-an-oid 0\tp\n".to_string().as_bytes()).is_none());
        assert!(parse_ls_files_record(format!("10064 {oid} 0\tp\n").as_bytes()).is_none());
        assert!(parse_ls_files_record(format!("100644 {oid} 9\tp\n").as_bytes()).is_none());
        assert!(parse_ls_files_record(format!("100644 {oid} 0 extra\tp\n").as_bytes()).is_none());
    }

    #[test]
    fn ls_files_verbose_records_parse_strictly() {
        let (path, flag) =
            parse_ls_files_verbose_record(b"S spa ce.md\n").expect("skip-worktree parses");
        assert_eq!(path, OsString::from("spa ce.md\n"));
        assert_eq!(flag, Some(IndexFlag::SkipWorktree));
        let (path, flag) = parse_ls_files_verbose_record(b"h a.md").expect("assume-unchanged");
        assert_eq!(path, OsString::from("a.md"));
        assert_eq!(flag, Some(IndexFlag::AssumeUnchanged));

        // Ordinary entries report their changes normally, so they parse
        // with no hiding flag.
        for ordinary in ["H a.md", "R a.md", "? a.md", "M a.md"] {
            let (path, flag) = parse_ls_files_verbose_record(ordinary.as_bytes())
                .unwrap_or_else(|| panic!("{ordinary} parses"));
            assert_eq!(path, OsString::from("a.md"));
            assert_eq!(flag, None, "{ordinary} hides nothing");
        }
        // Malformed records never reach a caller.
        assert!(parse_ls_files_verbose_record(b"S").is_none());
        assert!(parse_ls_files_verbose_record(b"S ").is_none());
        assert!(parse_ls_files_verbose_record(b"Sno-space a.md").is_none());
        assert!(parse_ls_files_verbose_record(b"").is_none());
    }

    #[test]
    fn ls_tree_records_parse_strictly() {
        let oid = "0123456789abcdef0123456789abcdef01234567";
        let record = format!("100755 blob {oid}\tspa ce.md\n").into_bytes();
        let entry = parse_ls_tree_record(&record).expect("a valid record parses");
        assert_eq!(entry.mode, "100755");
        assert_eq!(entry.kind, "blob");
        assert_eq!(entry.oid, oid);
        assert_eq!(entry.path, OsString::from("spa ce.md\n"));

        let gitlink = format!("160000 commit {oid}\tsub\n").into_bytes();
        let entry = parse_ls_tree_record(&gitlink).expect("a gitlink parses");
        assert_eq!(entry.kind, "commit");

        assert!(parse_ls_tree_record(b"no tab here").is_none());
        assert!(
            parse_ls_tree_record(format!("100644 tree {oid}\tp\n").to_string().as_bytes())
                .is_none()
        );
        assert!(
            parse_ls_tree_record("100644 blob not-an-oid\tp\n".to_string().as_bytes()).is_none()
        );
        assert!(
            parse_ls_tree_record(format!("10064 blob {oid}\tp\n").to_string().as_bytes()).is_none()
        );
        assert!(
            parse_ls_tree_record(
                format!("100644 blob {oid} extra\tp\n")
                    .to_string()
                    .as_bytes()
            )
            .is_none()
        );
    }

    #[test]
    fn rev_list_lines_parse_strictly() {
        let sha1 = "0123456789abcdef0123456789abcdef01234567";
        let sha256 = format!("{sha1}0123456789abcdef01234567");
        assert_eq!(sha256.len(), 64);
        // The newest commit comes first, both widths parse, and a missing
        // trailing newline on the last line is not an error.
        let listed = format!("{sha256}\n{sha1}\n{sha1}");
        assert_eq!(
            parse_rev_list_oids(listed.as_bytes()).unwrap(),
            vec![sha256.clone(), sha1.to_string(), sha1.to_string()]
        );
        assert!(parse_rev_list_oids(b"").is_err());
        assert!(parse_rev_list_oids(b"HEAD\n").is_err());
        assert!(parse_rev_list_oids(format!("{sha1}\n{sha256}X").as_bytes()).is_err());
    }

    #[test]
    fn empty_staging_list_never_spawns() {
        // A nonexistent program proves that no process was started: the
        // empty list must return before any spawn could fail.
        let adapter = adapter_with(&[]).with_program("/nonexistent-margin-git");
        assert_eq!(adapter.stage_paths(&[]).ok(), Some(StageOutcome::Noop));
        assert!(adapter.run_ok(Mode::Query, ["--version"]).is_err());
    }

    #[test]
    fn version_line_rejects_empty_output() {
        let adapter = Git::from_env(vec![(OsString::from("PATH"), OsString::from("/usr/bin"))])
            .with_program("true");
        // The `true` program prints nothing, so parsing must fail cleanly.
        assert!(matches!(
            adapter.git_version(),
            Err(GitError::MalformedOutput { .. })
        ));
    }

    #[test]
    fn commit_objects_parse_headers_and_exact_message() {
        let oid = "0123456789abcdef0123456789abcdef01234567";
        let other = "89abcdef0123456789abcdef0123456789abcdef";
        let raw = format!(
            "tree {oid}\n\
             parent {other}\n\
             author A <a@example.invalid> 1767225600 +0000\n\
             committer A <a@example.invalid> 1767225600 +0000\n\
             \n\
             subject\n\
             \n\
             Margin-Version: 1\n"
        );
        let object = parse_commit_object(raw.as_bytes()).expect("the commit object parses");
        assert_eq!(object.tree, oid);
        assert_eq!(object.parents, vec![other]);
        assert_eq!(object.committer_time, Some(1767225600));
        assert_eq!(object.message, b"subject\n\nMargin-Version: 1\n");

        // A signing block and several parents survive parsing.
        let signed = format!(
            "tree {oid}\nparent {other}\nparent {oid}\n\
             gpgsig -----BEGIN PGP SIGNATURE-----\n \
             iQEcBAABCgAGBQJ\n \
             =abcd\n \
             -----END PGP SIGNATURE-----\n\
             \n\
             m\n"
        );
        let object = parse_commit_object(signed.as_bytes()).expect("the signed object parses");
        assert_eq!(object.parents, vec![other, oid]);
        assert_eq!(object.message, b"m\n");
        // No committer header names no time, and a malformed one does not
        // fail the whole read.
        assert_eq!(object.committer_time, None);
        let offset =
            parse_commit_object(format!("tree {oid}\ncommitter No Time\n\nm\n").as_bytes())
                .expect("a header without a timestamp still parses");
        assert_eq!(offset.committer_time, None);

        // An empty message, a missing blank line, a second tree header, and a
        // bad object ID all refuse.
        assert_eq!(
            parse_commit_object(format!("tree {oid}\n\n").as_bytes())
                .expect("an empty message is exact bytes")
                .message,
            Vec::<u8>::new()
        );
        for bad in [
            format!("tree {oid}\n"),
            format!("tree {oid}\ntree {other}\n\nm\n"),
            "tree not-an-oid\n\nm\n".to_string(),
            format!("tree {oid}\nparent short\n\nm\n"),
            " continued-without-header\n\nm\n".to_string(),
        ] {
            assert!(
                parse_commit_object(bad.as_bytes()).is_err(),
                "the object {:?} must not parse",
                bad
            );
        }
    }
}
