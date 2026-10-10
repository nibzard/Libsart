//! Deterministic helpers for the shared mutation lock of SPEC 9.1 step 2.
//!
//! [`hold_mutation_lock`] takes the operating-system lock of one repository
//! the way one competing Margin process would, so a `BUSY` case needs no
//! racing processes and no sleeps (SPEC 16.6).
//!
//! [`seam_adapter`] builds the deterministic process seam the suite uses
//! for mid-operation state changes: a wrapper program that delegates every
//! call to the installed Git and runs one injected action when a chosen
//! staging pass arrives, so a writer can move `HEAD`, the branch, the
//! index, or the working files at an exact point inside one operation.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use margin::git::Git;

use super::repo::TestRepo;
use super::text;

/// The application mutation lock of one repository, held by this process
/// like one competing Margin operation (SPEC 9.1 step 2). Dropping the
/// value releases the operating-system lock.
pub struct HeldMutationLock {
    /// The locked open file; the descriptor keeps the lock alive.
    _file: fs::File,
}

/// Takes the repository's application mutation lock through the operating
/// system, resolving the lock file with stock Git so the test vouches for
/// the location Margin uses (SPEC 8.2: one lock file in the Git directory).
pub fn hold_mutation_lock(repo: &TestRepo) -> HeldMutationLock {
    let path = lock_path(repo);
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .unwrap_or_else(|error| panic!("cannot open {}: {error}", path.display()));
    file.try_lock()
        .unwrap_or_else(|error| panic!("the lock at {} must be free: {error}", path.display()));
    HeldMutationLock { _file: file }
}

/// The lock file path of `repo`, from stock Git itself.
pub fn lock_path(repo: &TestRepo) -> PathBuf {
    let resolved = text(&repo.git_ok(["rev-parse", "--git-path", "margin.lock"]));
    let path = PathBuf::from(resolved);
    // Git reports the path relative to the directory it ran in, which is
    // the repository root here.
    if path.is_absolute() {
        path
    } else {
        repo.path().join(path)
    }
}

/// Which staging pass of one operation triggers the injected action.
#[derive(Clone, Copy, Debug)]
pub enum SeamPass {
    /// The first disposable staging pass: the checkpoint's intended-tree
    /// proof staging, after the change enumeration.
    FirstDisposable,
    /// The second disposable staging pass: the initialization's
    /// intended-tree proof staging, after the identity-profile proof.
    SecondDisposable,
    /// The disposable `write-tree` that ends one intended-tree proof,
    /// between the proof and the precondition recheck.
    ProofTail,
    /// The real staging of the working index, after every proof and
    /// recheck.
    Real,
}

impl SeamPass {
    /// The value the wrapper compares: the disposable pass number,
    /// `proof-tail` for the tree write that ends a proof, or `real` for
    /// the staging of the working index.
    fn as_value(self) -> String {
        match self {
            SeamPass::FirstDisposable => "1".to_string(),
            SeamPass::SecondDisposable => "2".to_string(),
            SeamPass::ProofTail => "proof-tail".to_string(),
            SeamPass::Real => "real".to_string(),
        }
    }
}

/// Builds the Git adapter behind the failpoint seam (SPEC 16.6): every call
/// is delegated to the installed Git, and when the staging pass `pass`
/// arrives, `action` runs first, in the repository, without the disposable
/// index override of that call.
///
/// The action is ordinary shell text the test supplies; its effects are the
/// concurrent writer the rechecks of SPEC 9.1 step 6 must detect. The pass
/// counter lives outside the repository, so no scoped content or sentinel
/// observes it.
pub fn seam_adapter(repo: &TestRepo, pass: SeamPass, action: &str) -> Git {
    let script = repo.root().join("seam.sh");
    let counter = repo.root().join("seam-count");
    fs::write(
        &script,
        concat!(
            "#!/bin/sh\n",
            "# Deterministic failpoint seam (SPEC 16.6): delegate to the installed\n",
            "# git, and run the injected action when the configured pass arrives.\n",
            "# The counter lives outside the repository.\n",
            "count=0\n",
            "if [ -f \"$MARGIN_SEAM_COUNT\" ]; then\n",
            "  count=$(cat \"$MARGIN_SEAM_COUNT\")\n",
            "fi\n",
            "if [ -n \"$GIT_INDEX_FILE\" ]; then\n",
            "  if [ \"$1\" = \"update-index\" ]; then\n",
            "    count=$((count + 1))\n",
            "    printf '%s' \"$count\" > \"$MARGIN_SEAM_COUNT\"\n",
            "    if [ \"$MARGIN_SEAM_FIRE\" = \"$count\" ]; then\n",
            "      env -u GIT_INDEX_FILE sh -c \"$MARGIN_SEAM_ACTION\"\n",
            "    fi\n",
            "  elif [ \"$1\" = \"write-tree\" ] && [ \"$MARGIN_SEAM_FIRE\" = \"proof-tail\" ]; then\n",
            "    env -u GIT_INDEX_FILE sh -c \"$MARGIN_SEAM_ACTION\"\n",
            "  fi\n",
            "elif [ \"$1\" = \"update-index\" ] && [ \"$MARGIN_SEAM_FIRE\" = \"real\" ]; then\n",
            "  env -u GIT_INDEX_FILE sh -c \"$MARGIN_SEAM_ACTION\"\n",
            "fi\n",
            "exec git \"$@\"\n",
        ),
    )
    .unwrap_or_else(|error| panic!("cannot write {}: {error}", script.display()));
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|error| panic!("cannot chmod {}: {error}", script.display()));
    let mut vars = repo.env().vars().clone();
    vars.insert(
        OsString::from("MARGIN_SEAM_FIRE"),
        OsString::from(pass.as_value()),
    );
    vars.insert(OsString::from("MARGIN_SEAM_ACTION"), OsString::from(action));
    vars.insert(
        OsString::from("MARGIN_SEAM_COUNT"),
        counter.into_os_string(),
    );
    Git::from_env(vars)
        .at(repo.path())
        .with_program(script.into_os_string())
}
