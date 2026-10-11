//! Independent oracles built on stock Git output and raw disk bytes.
//!
//! Every function here observes the repository through Git itself or
//! through the filesystem, never through Margin code. SPEC 16.3 requires
//! refs, index and tree identity, raw working bytes, modes, and out-of-scope
//! sentinels to be verified this way.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use super::repo::TestRepo;
use super::text;

/// One entry of `git ls-tree`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: String,
    pub kind: String,
    pub oid: String,
    pub path: String,
}

/// One entry of `git ls-files --stage`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    pub mode: String,
    pub oid: String,
    pub stage: u32,
    pub path: String,
}

/// One captured working file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkingFile {
    /// File bytes, or the link target for a symbolic link.
    pub bytes: Vec<u8>,
    /// Permission bits, including setuid, setgid, and sticky.
    pub mode: u32,
    /// Whether this entry is a symbolic link.
    pub symlink: bool,
}

/// Observed state of a repository at one moment.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    /// Full ref name to object ID.
    pub refs: BTreeMap<String, String>,
    /// HEAD commit ID, or `None` while the branch is unborn.
    pub head: Option<String>,
    /// Index tree ID, or `None` while unresolved conflicts stop `write-tree`.
    pub index_tree: Option<String>,
    /// Repository-relative path to captured working file.
    pub working: BTreeMap<String, WorkingFile>,
}

/// Returns the HEAD commit ID, or `None` while the branch is unborn.
pub fn head_oid(repo: &TestRepo) -> Option<String> {
    let output = repo.git(["rev-parse", "--verify", "--quiet", "HEAD"]);
    if output.status.success() {
        Some(text(&output.stdout))
    } else {
        None
    }
}

/// Returns every ref as full ref name to object ID. Refnames cannot contain
/// spaces, so one line holds one pair.
pub fn refs(repo: &TestRepo) -> BTreeMap<String, String> {
    let stdout = repo.git_ok(["for-each-ref", "--format=%(refname) %(objectname)"]);
    let mut refs = BTreeMap::new();
    for line in String::from_utf8_lossy(&stdout).lines() {
        let (name, oid) = line
            .split_once(' ')
            .unwrap_or_else(|| panic!("cannot parse ref line {line:?}"));
        refs.insert(name.to_string(), oid.to_string());
    }
    refs
}

/// Resolves one revision and asserts that it names one object.
pub fn rev_parse(repo: &TestRepo, revision: &str) -> String {
    text(&repo.git_ok(["rev-parse", "--verify", revision]))
}

/// Returns the tree of a commit.
pub fn commit_tree(repo: &TestRepo, commit: &str) -> String {
    rev_parse(repo, &format!("{commit}^{{tree}}"))
}

/// Returns the path of the repository's real index file, as Git itself
/// resolves it.
fn index_file(repo: &TestRepo) -> PathBuf {
    let path = PathBuf::from(text(&repo.git_ok(["rev-parse", "--git-path", "index"])));
    if path.is_absolute() {
        path
    } else {
        repo.path().join(path)
    }
}

/// Returns the raw bytes of the repository's real index file, or an empty
/// vector while no index file exists. Compared before and after one
/// command, these bytes prove the command changed no index byte — the
/// cached-tree extension included — which the tree ID alone cannot show
/// (SPEC 16.3, task T062).
pub fn index_bytes(repo: &TestRepo) -> Vec<u8> {
    match fs::read(index_file(repo)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!(
            "cannot read the index of {}: {error}",
            repo.path().display()
        ),
    }
}

/// Returns the index tree ID from `git write-tree` run against one
/// disposable copy of the real index, or `None` when the index cannot be
/// written as a tree. The real index keeps its exact bytes, cache included,
/// so no oracle prepares the state a later observation compares against
/// (task T062).
pub fn index_tree(repo: &TestRepo) -> Option<String> {
    let real = index_file(repo);
    let disposable = tempfile::NamedTempFile::new().expect("a disposable index can be reserved");
    match fs::metadata(&real) {
        Ok(_) => {
            fs::copy(&real, disposable.path()).expect("the real index can be copied");
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // One absent index is the empty index Git itself reads.
            let seeded = run_with_index(repo, disposable.path(), &["read-tree", "--empty"]);
            assert!(
                seeded.status.success(),
                "stock git cannot seed an empty disposable index: {}",
                String::from_utf8_lossy(&seeded.stderr)
            );
        }
        Err(error) => panic!(
            "cannot inspect the index of {}: {error}",
            repo.path().display()
        ),
    }
    let output = run_with_index(repo, disposable.path(), &["write-tree"]);
    if output.status.success() {
        Some(text(&output.stdout))
    } else {
        None
    }
}

/// Runs stock Git in `repo` with `GIT_INDEX_FILE` pointed at `disposable`
/// and returns the raw process output. Only the disposable index changes.
fn run_with_index(repo: &TestRepo, disposable: &Path, args: &[&str]) -> Output {
    let mut command = Command::new("git");
    command
        .current_dir(repo.path())
        .args(args)
        .stdin(Stdio::null());
    for (key, value) in repo.env().vars() {
        command.env(key, value);
    }
    command.env("GIT_INDEX_FILE", disposable);
    command.output().expect("stock git can run")
}

/// Returns the recursive entries of a tree-ish from `git ls-tree -r`.
///
/// Paths are decoded as UTF-8 with lossy conversion. Tests that compare
/// non-UTF-8 pathnames must parse the raw records themselves.
pub fn ls_tree(repo: &TestRepo, treeish: &str) -> Vec<TreeEntry> {
    let stdout = repo.git_ok(["ls-tree", "-r", "-z", treeish]);
    split_nul(&stdout)
        .filter(|record| !record.is_empty())
        .map(|record| {
            // <mode> SP <type> SP <oid> TAB <path>
            let (meta, path) = split_once(record, b'\t').unwrap_or_else(|| {
                panic!(
                    "cannot parse ls-tree record {:?}",
                    String::from_utf8_lossy(record)
                )
            });
            let mut fields = meta.split(|&byte| byte == b' ');
            let mode = next_field(&mut fields, "mode", treeish);
            let kind = next_field(&mut fields, "type", treeish);
            let oid = next_field(&mut fields, "object ID", treeish);
            TreeEntry {
                mode,
                kind,
                oid,
                path: String::from_utf8_lossy(path).into_owned(),
            }
        })
        .collect()
}

/// Returns the index entries from `git ls-files --stage`.
pub fn ls_files(repo: &TestRepo) -> Vec<IndexEntry> {
    let stdout = repo.git_ok(["ls-files", "--stage", "-z"]);
    split_nul(&stdout)
        .filter(|record| !record.is_empty())
        .map(|record| {
            // <mode> SP <oid> SP <stage> TAB <path>
            let (meta, path) = split_once(record, b'\t').unwrap_or_else(|| {
                panic!(
                    "cannot parse ls-files record {:?}",
                    String::from_utf8_lossy(record)
                )
            });
            let mut fields = meta.split(|&byte| byte == b' ');
            let mode = next_field(&mut fields, "mode", "index");
            let oid = next_field(&mut fields, "object ID", "index");
            let stage_field = next_field(&mut fields, "stage", "index");
            let stage = stage_field.parse().unwrap_or_else(|error| {
                panic!("cannot parse ls-files stage {stage_field:?}: {error}")
            });
            IndexEntry {
                mode,
                oid,
                stage,
                path: String::from_utf8_lossy(path).into_owned(),
            }
        })
        .collect()
}

/// Returns the raw content of one blob.
pub fn blob(repo: &TestRepo, oid: &str) -> Vec<u8> {
    repo.git_ok(["cat-file", "blob", oid])
}

/// Returns the permission bits of a working file, read from the disk.
pub fn disk_mode(repo: &TestRepo, relative: &str) -> u32 {
    let path = repo.file_path(relative);
    let metadata = fs::symlink_metadata(&path)
        .unwrap_or_else(|error| panic!("cannot inspect {}: {error}", path.display()));
    metadata.permissions().mode() & 0o7777
}

/// Says whether a repository-relative path lies under one of the scope roots.
/// A root is a literal directory or file; `.` covers the whole repository.
pub fn in_scope(path: &str, roots: &[&str]) -> bool {
    roots.iter().any(|root| {
        let root = root.trim_end_matches('/');
        root == "." || root.is_empty() || path == root || path.starts_with(&format!("{root}/"))
    })
}

/// Captures refs, HEAD, the index tree, and every working file. `.git`
/// directories are skipped; other unusual file kinds are not captured.
pub fn snapshot(repo: &TestRepo) -> Snapshot {
    let mut working = BTreeMap::new();
    walk(repo.path(), "", &mut working);
    Snapshot {
        refs: refs(repo),
        head: head_oid(repo),
        index_tree: index_tree(repo),
        working,
    }
}

fn split_nul(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.split(|&byte| byte == 0)
}

fn split_once(bytes: &[u8], separator: u8) -> Option<(&[u8], &[u8])> {
    bytes
        .iter()
        .position(|&byte| byte == separator)
        .map(|index| (&bytes[..index], &bytes[index + 1..]))
}

fn next_field<'a, I: Iterator<Item = &'a [u8]>>(
    fields: &mut I,
    name: &str,
    source: &str,
) -> String {
    let field = fields
        .next()
        .unwrap_or_else(|| panic!("missing {name} in a {source} record"));
    String::from_utf8_lossy(field).into_owned()
}

/// Visits every working file below `dir`, except `.git` directories.
fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, WorkingFile>) {
    let entries =
        fs::read_dir(dir).unwrap_or_else(|error| panic!("cannot read {}: {error}", dir.display()));
    for entry in entries {
        let entry = entry
            .unwrap_or_else(|error| panic!("cannot read an entry of {}: {error}", dir.display()));
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let displayed = name.to_string_lossy().into_owned();
        let relative = if prefix.is_empty() {
            displayed
        } else {
            format!("{prefix}/{displayed}")
        };
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .unwrap_or_else(|error| panic!("cannot inspect {}: {error}", path.display()));
        let file_type = metadata.file_type();
        let mode = metadata.permissions().mode() & 0o7777;
        if file_type.is_dir() {
            walk(&path, &relative, out);
        } else if file_type.is_file() {
            let bytes = fs::read(&path)
                .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
            out.insert(
                relative,
                WorkingFile {
                    bytes,
                    mode,
                    symlink: false,
                },
            );
        } else if file_type.is_symlink() {
            let target = fs::read_link(&path)
                .unwrap_or_else(|error| panic!("cannot read the link {}: {error}", path.display()));
            out.insert(
                relative,
                WorkingFile {
                    bytes: target.as_os_str().as_encoded_bytes().to_vec(),
                    mode,
                    symlink: true,
                },
            );
        }
    }
}

impl Snapshot {
    /// Asserts that refs, HEAD, the index tree, and all working files are
    /// equal. Use this to prove that a query left the repository alone.
    pub fn assert_unchanged(&self, after: &Snapshot, context: &str) {
        let differences = self.differences(after);
        assert!(
            differences.is_empty(),
            "{context}: repository state changed\n  {}",
            differences.join("\n  ")
        );
    }

    /// Returns the repository-relative paths whose bytes, mode, kind, or
    /// presence changed between the two snapshots, sorted by path.
    pub fn working_diff(&self, after: &Snapshot) -> Vec<String> {
        let mut paths: Vec<String> = self
            .working
            .keys()
            .chain(after.working.keys())
            .cloned()
            .collect();
        paths.sort();
        paths.dedup();
        paths
            .into_iter()
            .filter(|path| self.working.get(path) != after.working.get(path))
            .collect()
    }

    /// Asserts that working files outside the scope roots kept their exact
    /// bytes, modes, and presence. This is the out-of-scope sentinel check
    /// that SPEC 16.3 requires before and after an operation.
    pub fn assert_out_of_scope_unchanged(&self, after: &Snapshot, scoped: &[&str], context: &str) {
        let changed: Vec<String> = self
            .working_diff(after)
            .into_iter()
            .filter(|path| !in_scope(path, scoped))
            .collect();
        assert!(
            changed.is_empty(),
            "{context}: out-of-scope files changed outside {}: {}",
            scoped.join(", "),
            changed.join(", ")
        );
    }

    /// Lists every difference between two snapshots.
    fn differences(&self, after: &Snapshot) -> Vec<String> {
        let mut differences = Vec::new();
        let mut names: Vec<String> = self.refs.keys().chain(after.refs.keys()).cloned().collect();
        names.sort();
        names.dedup();
        for name in names {
            match (self.refs.get(&name), after.refs.get(&name)) {
                (Some(before), Some(next)) if before == next => {}
                (Some(before), Some(next)) => {
                    differences.push(format!("ref {name}: {before} -> {next}"));
                }
                (Some(before), None) => differences.push(format!("ref {name}: {before} -> absent")),
                (None, Some(next)) => differences.push(format!("ref {name}: absent -> {next}")),
                (None, None) => unreachable!("the key set supplies each name"),
            }
        }
        if self.head != after.head {
            differences.push(format!("HEAD: {:?} -> {:?}", self.head, after.head));
        }
        if self.index_tree != after.index_tree {
            differences.push(format!(
                "index tree: {:?} -> {:?}",
                self.index_tree, after.index_tree
            ));
        }
        for path in self.working_diff(after) {
            differences.push(format!("working file {path} changed"));
        }
        differences
    }
}

/// Returns the out-of-scope entries that differ between two tree-ish objects,
/// compared by path, mode, and object ID. This is the tree oracle for INV-07.
pub fn out_of_scope_tree_diff(
    repo: &TestRepo,
    before: &str,
    after: &str,
    scoped: &[&str],
) -> Vec<String> {
    let old = tree_map(repo, before, scoped);
    let new = tree_map(repo, after, scoped);
    let mut paths: Vec<String> = old.keys().chain(new.keys()).cloned().collect();
    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .filter_map(|path| match (old.get(&path), new.get(&path)) {
            (Some(entry), Some(next)) if entry == next => None,
            (Some(entry), Some(next)) => Some(format!("{path}: {} -> {}", entry, next)),
            (Some(entry), None) => Some(format!("{path}: {entry} -> absent")),
            (None, Some(entry)) => Some(format!("{path}: absent -> {entry}")),
            (None, None) => unreachable!("the key set supplies each path"),
        })
        .collect()
}

/// Asserts that two tree-ish objects agree on every out-of-scope entry.
pub fn assert_out_of_scope_tree_unchanged(
    repo: &TestRepo,
    before: &str,
    after: &str,
    scoped: &[&str],
    context: &str,
) {
    let differences = out_of_scope_tree_diff(repo, before, after, scoped);
    assert!(
        differences.is_empty(),
        "{context}: out-of-scope tree entries changed outside {}\n  {}",
        scoped.join(", "),
        differences.join("\n  ")
    );
}

fn tree_map(repo: &TestRepo, treeish: &str, scoped: &[&str]) -> BTreeMap<String, String> {
    ls_tree(repo, treeish)
        .into_iter()
        .filter(|entry| !in_scope(&entry.path, scoped))
        .map(|entry| (entry.path, format!("{} {}", entry.mode, entry.oid)))
        .collect()
}

/// Asserts that the index holds exactly the entries of `treeish`, compared by
/// path, mode, and object ID. This is the index oracle that SPEC 16.5
/// requires beyond a displayed clean status.
pub fn assert_index_matches_tree(repo: &TestRepo, treeish: &str) {
    let index: BTreeSet<(String, String, String)> = ls_files(repo)
        .into_iter()
        .map(|entry| (entry.path, entry.mode, entry.oid))
        .collect();
    let tree: BTreeSet<(String, String, String)> = ls_tree(repo, treeish)
        .into_iter()
        .map(|entry| (entry.path, entry.mode, entry.oid))
        .collect();
    let differences: Vec<String> = index
        .symmetric_difference(&tree)
        .map(|(path, mode, oid)| format!("{path} {mode} {oid}"))
        .collect();
    assert!(
        differences.is_empty(),
        "the index does not match {treeish}:\n  {}",
        differences.join("\n  ")
    );
}
