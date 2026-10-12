//! Application operations and mutation coordination.
//!
//! This file holds the one scope resolver required by SPEC 3.2 and 13.6,
//! the canonical identities of SPEC 8.6, and the repository and storage
//! policy guards of SPEC 3.1 and 8.8.
//!
//! Every interface adopts a writing scope through [`resolve_scope`]; no
//! interface may assemble its own descriptor or path set. The resolver
//! combines the pure rules of [`crate::domain`] with the Git adapter and
//! the working tree:
//!
//! 1. Parse and normalize user-supplied roots and suffixes (pure rules).
//! 2. Annotate roots with the filesystem: reject symlink components, reject
//!    roots below an existing file, and record directory roots.
//! 3. Assemble the canonical descriptor and its Git-hashed digest.
//! 4. Enumerate scoped paths: tracked files (even when an ignore rule now
//!    matches them), eligible untracked files, and deletions.
//! 5. Report actionable blockers: symlinks, submodules, nested repository
//!    roots, and non-UTF-8 paths are never silently treated as reviewed.
//!
//! Every identity of SPEC 8.6 flows through [`Identities`]: canonical bytes
//! assembled once in [`crate::domain`], hashed by Git's own object hashing,
//! and qualified by the repository object format. Working and stored content
//! digests reuse Git hashing as well; an entry that cannot honestly join a
//! digest refuses with a typed error instead of narrowing it.
//!
//! Every normal mutation passes through [`enforce_mutation_policy`] before
//! it writes (SPEC 9.1 step 4). The guard refuses a bare repository, a
//! missing working tree, linked worktrees, sparse checkout, shallow and
//! partial clones, a nested superproject, in-progress Git operations, a
//! detached `HEAD`, a dirty index, and every content transformation the
//! `identity-v1` storage profile excludes. [`verify_identity_profile`]
//! then proves, through a disposable Git index, that staging the scoped
//! working files stores their raw bytes unchanged. The shared mutation
//! context also enforces the adopted branch of the local settings
//! (SPEC 8.2): a `HEAD` on another branch — even at the same commit —
//! refuses `BRANCH_CHANGED` before any index write, because Margin never
//! switches or re-adopts a branch on its own and one explicit `init`
//! remains the path that adopts another branch.
//!
//! Every write also passes through one shared mutation lock (SPEC 9.1
//! step 2): a nonblocking operating-system lock on one named file in the
//! Git directory, held from the mutation inspection through the
//! postcondition inspection and the settings persistence. A competing
//! Margin operation returns `BUSY` without intended effects. The lock
//! coordinates Margin processes only — it does not lock Git, an editor, or
//! an agent (SPEC 14.3) — and it is never approximated by a stale PID
//! file. Immediately before the first real mutation, each capture
//! rechecks the branch, the scope with its path-safety rules, `HEAD`, the
//! index, and the scoped working content it prepared against, and refuses
//! before staging when any of them moved (SPEC 9.1 step 6, 16.5 stale
//! state).
//!
//! Every Margin commit is one marker written and validated here (SPEC
//! 8.3-8.4): [`commit_marker`] prepares the reserved trailer map, commits the
//! verified index through normal `git commit`, and reads the actual commit
//! back to compare every reserved value against that prepared map, because a
//! hook can rewrite a message into agreement with its own commit.
//! [`validate_marker`] runs the same structural checks over history: exact
//! trailer counts, candidate and parent agreement, the full result tree, a
//! scope digest the canonical descriptor reproduces, and out-of-scope
//! equality. Forged, copied, or hook-altered markers stay ordinary history
//! and confer no approval.
//!
//! Baseline selection walks that history (SPEC 8.5): [`select_baseline`]
//! follows the unfiltered first-parent chain from the candidate, newest
//! first, and stops at the nearest valid matching start or review marker,
//! including empty review commits that a path-limited log would hide. A
//! start of any scope resets the anchor and the walk never crosses one.
//! Each review must record the baseline this same walk selects at its own
//! candidate (SPEC 8.4 rule 6). When no anchor can be established, the
//! result is `BASELINE_UNAVAILABLE`, never an arbitrary initial commit.
//!
//! The canonical change manifest lives here too (SPEC 10.1-10.2):
//! [`build_change_manifest`] compares the immutable scoped tree entries of
//! baseline and candidate, generates every source patch under the fixed
//! diff contract, parses each complete patch into the one authoritative
//! change model, and derives the manifest, review, and change identities
//! from the full patch bytes. [`build_grouped_manifest`] then runs the
//! grouping stage over the same model: the presentation layer
//! ([`crate::markdown`]) parses both versions by source offset and forms
//! conservative passages of complete hunks, uncertain cases enlarge to one
//! whole-file decision with their reason, one disposable Git index proves
//! every file's groups apply exactly, and only then do the manifest,
//! review, and change identities issue over the grouped patch units — so
//! no group changes after actionable IDs exist. No interface may compute a
//! second authoritative diff (SPEC 13.6).
//!
//! Local settings live in the documented `margin.*` Git configuration keys
//! (SPEC 8.2): [`read_local_settings`] reads their effective values through
//! Git's configuration interface and [`update_local_settings`] writes only
//! the settings an update names, so unrelated entries survive every write.
//! An omitted setting keeps its value, first use captures nothing, and a
//! value outside its documented form fails typed instead of being guessed.
//! Every interface adopts the stored scope through the same resolver as an
//! explicit one; there is no second resolution path.
//!
//! First use composes those pieces in [`Operations::initialize_scope`]
//! (SPEC 5.1): adopt the repository of the adapter or create one only when
//! the interface explicitly asks, refuse on staged changes, scope blockers,
//! storage transformations, and a missing Git identity, capture the eligible
//! scoped working content — deletions included — in exactly one verified
//! `start` marker (a zero-parent `unborn` marker on an unborn branch, empty
//! when the tree is already saved), and persist the local settings only
//! after that commit verifies.
//!
//! Repeat initialization reuses the same walk (SPEC 8.2): a valid matching
//! anchor at the candidate without `--new-start` changes only the settings
//! the request supplies and adds no start commit, while a new or changed
//! scope, an unavailable anchor, or an explicit `--new-start` creates a
//! fresh starting marker. The walk never crosses a start marker, so scope
//! A → B → A anchors at its newly established A start instead of reviving
//! the older A approval.
//!
//! One writing pass then checkpoints through
//! [`Operations::create_checkpoint`] (SPEC 9.2): the shared guards refuse a
//! dirty index and every incompatible condition first, the capture
//! enumerates exactly the eligible scoped changes — the same set an
//! inspection reports uncaptured — and proves the intended tree in one
//! disposable index, where the staged delta names only those paths and
//! every staged entry equals the observed mode and raw bytes. Unchanged
//! content is the documented no-op of `created:false`. The real capture
//! stages only that explicit literal path set and commits it through the
//! shared marker writer, so the draft changes no out-of-scope entry and
//! confers no approval. A file the writer changed after it was staged is
//! reported as newer uncaptured content, never as saved. A staging or
//! commit that fails is observed, not assumed: the head, the index, and the
//! scoped working files are inspected again, a commit that exists despite
//! the failure is reported with its object ID, and an outcome that cannot
//! be inspected stays unknown (SPEC 9.1 step 7, 9.6, 14.1).
//!
//! A review plan then enters through one strict parser in the domain
//! (SPEC 7.3): [`Operations::validate_review`] reads the document, refuses
//! every unknown field, duplicate key, invalid identity, missing decision,
//! overlap, and unknown or incomplete selection, and compares every
//! context field with the pinned comparison the same build of
//! [`Operations::build_review`] produced, so a plan written against another
//! review is stale instead of silently reused. The shipped JSON Schema
//! documents the same field list the parser reads; it is never a second
//! validator (SPEC 13.6).
//!
//! Before any of those stale-state checks, the request's receipt is looked
//! up in history (SPEC 8.7): [`find_receipt`] scans the complete unfiltered
//! first-parent ancestry for the valid receipt of the same operation kind,
//! request ID, and plan fingerprint. An exact retry returns the original
//! verified commit as `existing_receipt`, a request ID reused under another
//! fingerprint conflicts, and a scan that cannot complete — or a history
//! context no marker may depend on — blocks the mutation instead of
//! guessing the request absent. Present-day dirty files, staged entries,
//! storage blockers, and a scope no new mutation could resolve never hide a
//! receipt the history still holds, because the receipt response is built
//! from the recorded canonical scope and immutable Git objects alone, and
//! rewritten history ends the deduplication promise while the
//! expected-candidate guards stay in force.
//!
//! The validated plan then receives its intended result (SPEC 9.3): one
//! disposable Git index seeded from C through `read-tree` with no
//! working-tree update flag reverse-applies each restored group's complete
//! patch — one file's groups latest source position first — and
//! `write-tree` yields the full intended tree T. The staged delta against
//! C names exactly the restored paths, so no out-of-scope entry moves and
//! no restored group silently changes nothing. The dry run reports that
//! expected effect and the exact scoped result of T, moves no ref, touches
//! neither the real index nor one working file, and advances no approval
//! state (SPEC 6.4).
//!
//! The shared operation layer of SPEC 13.2 composes those pieces into the
//! typed calls every interface uses. [`Operations::inspect_workspace`] and
//! its sibling operations are the only entry points; none of the CLI, the
//! TUI, or a hook adapter may resolve a scope, order the guards, or judge
//! an outcome on its own (SPEC 13.6). One inspection reports the three
//! separate state dimensions of SPEC 4.2 — preservation against the
//! candidate, review against the baseline, and the operation state the
//! repository itself asks for — with counts that are known or honestly
//! null with one reason, never fabricated zeros (SPEC 6.3). One document
//! view ([`Operations::show_document`]) resolves its revision to one
//! immutable commit before any content read and reports absence, emptiness,
//! readable UTF-8, or opaque bytes explicitly, with bounded views, a
//! bounded hexadecimal form for opaque content, and one exact raw export
//! (SPEC 6.3). One timeline ([`Operations::list_history`]) pages the
//! ancestry-based editorial history of SPEC 5.5: labels come from validated
//! markers — including rule 6 for a review — consecutive drafts collapse
//! into one writing pass that stays listable again, order is Git ancestry
//! under the exact committer clock, and the unfiltered walk of the labels
//! never narrows for presentation, so a short log hides no baseline and no
//! empty review commit. One recovery inspection
//! ([`Operations::inspect_recovery`]) reports the state a writer needs
//! after an uncertain outcome (SPEC 9.7): the head, the valid baseline of
//! the one walk, the index tree, the working-scope fingerprint, the staged
//! paths, and whether history holds a valid receipt, next to the bounded
//! recovery argv assembled from those observations. The query never
//! mutates: it moves no ref, no index entry, and no working byte. Every
//! result, successful or failed, carries the
//! observed effects of SPEC 7.1: what happened to the working tree, the
//! index, commits, the repository, the local configuration, and the hook
//! integration.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::domain::{
    self, ChangeGroup, ChangeIdentity, CommitKind, CommitMetadata, ContentEntry, EntryMode,
    LocalSettings, MAX_DIFF_PAGE_BYTES, MAX_DOCUMENT_BYTES, MAX_GROUP_PRESENTATION_BYTES,
    MAX_PRESENTATION_EXCERPT_BYTES, MAX_PRESENTATION_WORD_HINTS, MAX_RENDERED_COMPARISON_BYTES,
    OperationState, PlanIdentity, PlanProblem, PreservationState, RestoreIdentity, RestorePlan,
    RestoreSource, ReviewContext, ReviewPlan, ReviewState, RootEntry, ScopeDescriptor, ScopeError,
    ScopeInput, SettingsUpdate, SettingsValueError, TrailerParseError, TrailerProblem,
};
use crate::git::{
    AttributeAssignment, CommitObject, Git, GitError, IndexEntry, IndexFlag, ObjectFormat,
    os_string_from_bytes,
};
use crate::markdown;

/// Mode `120000` in `git ls-files --stage`: a symbolic link.
const SYMLINK_MODE: &str = "120000";
/// Mode `160000` in `git ls-files --stage`: a gitlink, such as a submodule.
const GITLINK_MODE: &str = "160000";
/// The name of the application lock file inside the Git directory (SPEC
/// 8.2): one named piece of allowed disposable state, resolved through
/// Git's own metadata layout (`rev-parse --git-path`), so every Margin
/// process of one repository computes the same path.
const MUTATION_LOCK_FILE: &str = "margin.lock";

/// One Git-hashed canonical identity (SPEC 8.6): the object format of the
/// repository that computed it and the blob object ID of the
/// domain-separated canonical bytes.
///
/// The object ID comes from Git's own hashing interface, so no Margin code
/// holds a second hash implementation, and the identifier names the storage
/// object format it was computed with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    /// Object format of the repository that computed the identity.
    pub object_format: ObjectFormat,
    /// The blob object ID of the canonical bytes.
    pub oid: String,
}

impl Fingerprint {
    /// The full identifier: object format and object ID (SPEC 8.6).
    pub fn id(&self) -> String {
        format!("{}:{}", self.object_format.as_str(), self.oid)
    }
}

/// The digest of one canonical scope descriptor (SPEC 3.2, 8.6).
pub type ScopeDigest = Fingerprint;

/// How one scoped path exists relative to Git and the working tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopedPathState {
    /// Tracked in the index with a non-symlink working entry present. The
    /// path stays in scope even when an exclude rule now matches it, because
    /// tracking comes from `git ls-files`, which ignores exclude rules
    /// (SPEC 3.2 rule 3).
    Tracked,
    /// Tracked in the index, but the working file is gone. A deletion stays
    /// part of review (SPEC 3.2 rule 4).
    TrackedDeleted,
    /// Untracked and not ignored, therefore eligible for capture. Untracked
    /// paths that an exclude rule matches never reach this state, because
    /// the listing asks for `--exclude-standard` files only.
    Untracked,
}

/// An actionable scope blocker (SPEC 3.2 rule 5). A blocker is never a
/// silent exclusion: the path stays outside the scoped set and the blocker
/// names it, so adoption can refuse and `status` can explain why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeBlocker {
    /// A symbolic link sits at a scoped path or in one of its ancestor
    /// directories.
    SymlinkPath {
        /// The repository-relative path.
        path: OsString,
    },
    /// A gitlink, such as a submodule, is tracked at a scoped path.
    Submodule {
        /// The repository-relative path.
        path: OsString,
    },
    /// An untracked directory inside the scope is itself a repository.
    NestedRepository {
        /// The repository-relative directory, without a trailing slash.
        path: OsString,
    },
    /// A scoped path is not valid UTF-8 and release 0.1 defines no encoding
    /// for it (SPEC 3.3).
    NonUtf8Path {
        /// The raw path bytes.
        path: Vec<u8>,
    },
}

impl ScopeBlocker {
    /// Sort key: path bytes first, blocker kind second.
    fn sort_key(&self) -> (&[u8], u8) {
        fn key(path: &OsStr, kind: u8) -> (&[u8], u8) {
            (path.as_encoded_bytes(), kind)
        }
        match self {
            ScopeBlocker::SymlinkPath { path } => key(path, 0),
            ScopeBlocker::Submodule { path } => key(path, 1),
            ScopeBlocker::NestedRepository { path } => key(path, 2),
            ScopeBlocker::NonUtf8Path { path } => (path.as_slice(), 3),
        }
    }
}

impl fmt::Display for ScopeBlocker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn quoted(path: &OsStr) -> String {
            format!("{:?}", path.to_string_lossy())
        }
        match self {
            ScopeBlocker::SymlinkPath { path } => {
                write!(
                    formatter,
                    "symbolic link at the scoped path {}",
                    quoted(path)
                )
            }
            ScopeBlocker::Submodule { path } => {
                write!(formatter, "submodule at the scoped path {}", quoted(path))
            }
            ScopeBlocker::NestedRepository { path } => {
                write!(
                    formatter,
                    "nested repository at the scoped path {}",
                    quoted(path)
                )
            }
            ScopeBlocker::NonUtf8Path { path } => write!(
                formatter,
                "scoped path is not valid UTF-8: {:?}",
                String::from_utf8_lossy(path)
            ),
        }
    }
}

/// Failure of scope resolution.
#[derive(Debug)]
pub enum ScopeResolutionError {
    /// User-supplied scope input is invalid.
    Invalid(ScopeError),
    /// A Git process failed.
    Git(GitError),
    /// The working tree could not be inspected.
    Io(io::Error),
}

impl ScopeResolutionError {
    /// The SPEC 7.5 error code of this failure.
    pub fn public_code(&self) -> &'static str {
        match self {
            ScopeResolutionError::Invalid(error) => error.public_code(),
            ScopeResolutionError::Git(error) => error.public_code(),
            ScopeResolutionError::Io(_) => "IO_FAILED",
        }
    }
}

impl fmt::Display for ScopeResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScopeResolutionError::Invalid(error) => write!(formatter, "invalid scope: {error}"),
            ScopeResolutionError::Git(error) => write!(formatter, "{error}"),
            ScopeResolutionError::Io(error) => {
                write!(formatter, "cannot inspect the working tree: {error}")
            }
        }
    }
}

impl std::error::Error for ScopeResolutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ScopeResolutionError::Invalid(error) => Some(error),
            ScopeResolutionError::Git(error) => Some(error),
            ScopeResolutionError::Io(error) => Some(error),
        }
    }
}

impl From<ScopeError> for ScopeResolutionError {
    fn from(error: ScopeError) -> Self {
        ScopeResolutionError::Invalid(error)
    }
}

impl From<GitError> for ScopeResolutionError {
    fn from(error: GitError) -> Self {
        ScopeResolutionError::Git(error)
    }
}

impl From<io::Error> for ScopeResolutionError {
    fn from(error: io::Error) -> Self {
        ScopeResolutionError::Io(error)
    }
}

/// The result of resolving one writing scope: canonical descriptor, digest,
/// scoped path set, and blockers (SPEC 3.2).
#[derive(Clone, Debug)]
pub struct ResolvedScope {
    /// The canonical scope descriptor.
    pub descriptor: ScopeDescriptor,
    /// The Git-hashed digest of the canonical descriptor.
    pub digest: ScopeDigest,
    /// Every actionable scoped path, sorted by path bytes, with its state.
    pub paths: BTreeMap<OsString, ScopedPathState>,
    /// Blockers sorted by path bytes, then kind, without duplicates.
    pub blockers: Vec<ScopeBlocker>,
}

/// Resolves one writing scope against the repository of `git`.
///
/// This is the single resolver of the product (SPEC 13.6). It accepts
/// tracked, eligible untracked, ignored, and deleted paths, keeps unusual
/// names literal, and rejects escaping input while reporting symlinks,
/// submodules, nested repositories, and non-UTF-8 paths as blockers.
pub fn resolve_scope(git: &Git, input: &ScopeInput) -> Result<ResolvedScope, ScopeResolutionError> {
    let parsed = domain::parse_scope(input).map_err(ScopeResolutionError::Invalid)?;
    let repo_root = git.toplevel()?;
    let index = git.ls_files_stage()?;
    let tracked = tracked_kinds(&index);

    let mut blockers = Vec::new();
    let include = annotate_roots(&parsed.include, &repo_root, &tracked, true, &mut blockers)?;
    let exclude = annotate_roots(&parsed.exclude, &repo_root, &tracked, false, &mut blockers)?;
    let descriptor = ScopeDescriptor::assemble(&include, &exclude, &parsed.suffixes)?;

    let mut paths = BTreeMap::new();
    let mut memo = HashMap::new();

    for (path, kind) in &tracked {
        // A gitlink is a directory-like entry: it holds a whole subtree
        // inside the scope boundary even though its own name has no allowed
        // suffix, so scope boundaries apply to it like they do to untracked
        // directory entries.
        let scoped = match kind {
            TrackedKind::Gitlink => {
                let mut directory_entry = path.as_encoded_bytes().to_vec();
                directory_entry.push(b'/');
                descriptor.contains_path(path.as_encoded_bytes())
                    || descriptor.contains_directory(&directory_entry)
            }
            _ => descriptor.contains_path(path.as_encoded_bytes()),
        };
        if !scoped {
            continue;
        }
        if path.to_str().is_none() {
            blockers.push(ScopeBlocker::NonUtf8Path {
                path: path.as_encoded_bytes().to_vec(),
            });
            continue;
        }
        match kind {
            TrackedKind::Gitlink => {
                blockers.push(ScopeBlocker::Submodule { path: path.clone() });
                continue;
            }
            // A tracked symlink has mode 120000 in the index.
            TrackedKind::Symlink => {
                blockers.push(ScopeBlocker::SymlinkPath { path: path.clone() });
                continue;
            }
            TrackedKind::Regular => {}
        }
        match classify_worktree(&repo_root, path, &mut memo)? {
            Worktree::Absent => {
                paths.insert(path.clone(), ScopedPathState::TrackedDeleted);
            }
            Worktree::Present => {
                paths.insert(path.clone(), ScopedPathState::Tracked);
            }
            Worktree::Symlink => blockers.push(ScopeBlocker::SymlinkPath { path: path.clone() }),
        }
    }

    for entry in git.ls_files_others()? {
        let bytes = entry.as_encoded_bytes();
        // A trailing slash marks a directory Git did not descend into: a
        // nested repository, or possibly a linked directory.
        if bytes.last() == Some(&b'/') {
            if let Some(blocker) = directory_blocker(&repo_root, bytes, &descriptor) {
                blockers.push(blocker);
            }
            continue;
        }
        if !descriptor.contains_path(bytes) {
            continue;
        }
        match std::str::from_utf8(bytes) {
            Err(_) => blockers.push(ScopeBlocker::NonUtf8Path {
                path: bytes.to_vec(),
            }),
            Ok(_) => match classify_worktree(&repo_root, &entry, &mut memo)? {
                Worktree::Present => {
                    paths.insert(entry, ScopedPathState::Untracked);
                }
                Worktree::Symlink => blockers.push(ScopeBlocker::SymlinkPath { path: entry }),
                // The listing named a path that has vanished; nothing to do.
                Worktree::Absent => {}
            },
        }
    }

    blockers.sort_by(|left, right| left.sort_key().cmp(&right.sort_key()));
    blockers.dedup_by(|left, right| left.sort_key() == right.sort_key());

    // The scope digest follows the one identity path of SPEC 8.6 and 13.6.
    let digest = Identities::new(git)?.scope(&descriptor)?;
    Ok(ResolvedScope {
        descriptor,
        digest,
        paths,
        blockers,
    })
}

/// How a path is tracked in the index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrackedKind {
    /// An ordinary blob entry.
    Regular,
    /// Mode `120000`: a symbolic link.
    Symlink,
    /// Mode `160000`: a gitlink, such as a submodule.
    Gitlink,
}

/// Index paths mapped to their tracked kind. A path with several conflict
/// stages appears once; any gitlink stage marks the path as a gitlink.
fn tracked_kinds(index: &[IndexEntry]) -> BTreeMap<OsString, TrackedKind> {
    let mut tracked = BTreeMap::new();
    for entry in index {
        let kind = match entry.mode.as_str() {
            SYMLINK_MODE => TrackedKind::Symlink,
            GITLINK_MODE => TrackedKind::Gitlink,
            _ => TrackedKind::Regular,
        };
        tracked
            .entry(entry.path.clone())
            .and_modify(|existing| {
                if kind == TrackedKind::Gitlink {
                    *existing = TrackedKind::Gitlink;
                }
            })
            .or_insert(kind);
    }
    tracked
}

/// Annotates every root with filesystem facts. Include roots also collect
/// root-level blockers: a root that is itself a submodule or a nested
/// repository blocks adoption instead of being silently ignored.
fn annotate_roots(
    roots: &[domain::ParsedRoot],
    repo_root: &Path,
    tracked: &BTreeMap<OsString, TrackedKind>,
    is_include: bool,
    blockers: &mut Vec<ScopeBlocker>,
) -> Result<Vec<RootEntry>, ScopeResolutionError> {
    roots
        .iter()
        .map(|root| annotate_root(root, repo_root, tracked, is_include, blockers))
        .collect()
}

/// Annotates one root: directory-ness, symlink rejection, and root-level
/// blockers (SPEC 3.2 rules 1 and 5).
fn annotate_root(
    root: &domain::ParsedRoot,
    repo_root: &Path,
    tracked: &BTreeMap<OsString, TrackedKind>,
    is_include: bool,
    blockers: &mut Vec<ScopeBlocker>,
) -> Result<RootEntry, ScopeResolutionError> {
    let rendered = if root.components.is_empty() {
        "./".to_string()
    } else {
        root.components.join("/")
    };
    // The repository root is always a directory and needs no check.
    let mut is_dir_root = root.components.is_empty();
    let mut exists = root.components.is_empty();

    let mut path = repo_root.to_path_buf();
    for (index, component) in root.components.iter().enumerate() {
        path.push(component);
        let is_last = index + 1 == root.components.len();
        match fs::symlink_metadata(&path) {
            // The remaining components do not exist yet; the user's own
            // spelling decides whether the root names a directory.
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(ScopeResolutionError::Io(error)),
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(ScopeResolutionError::Invalid(ScopeError::SymlinkRoot {
                        root: rendered,
                        at: component.clone(),
                    }));
                }
                if metadata.is_dir() {
                    if is_last {
                        exists = true;
                        is_dir_root = true;
                    }
                } else if is_last {
                    exists = true;
                } else {
                    return Err(ScopeResolutionError::Invalid(
                        ScopeError::RootBelowNonDirectory {
                            root: rendered,
                            at: component.clone(),
                        },
                    ));
                }
            }
        }
    }
    if !exists {
        is_dir_root = root.spelled_as_dir;
    }

    if is_include && !root.components.is_empty() {
        let relative = OsString::from(root.components.join("/"));
        if tracked.get(&relative) == Some(&TrackedKind::Gitlink) {
            blockers.push(ScopeBlocker::Submodule { path: relative });
        } else if exists && is_dir_root && fs::symlink_metadata(path.join(".git")).is_ok() {
            blockers.push(ScopeBlocker::NestedRepository { path: relative });
        }
    }

    Ok(RootEntry {
        components: root.components.clone(),
        is_dir_root,
    })
}

/// The state of a working-tree path relative to its ancestors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Worktree {
    /// A non-symlink entry exists at the path.
    Present,
    /// No entry exists at the path.
    Absent,
    /// The path or one of its ancestors is a symbolic link.
    Symlink,
}

/// Cached state of one ancestor directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirState {
    /// The directory does not exist.
    Missing,
    /// The directory is a symbolic link.
    Symlink,
    /// An ordinary directory.
    Directory,
}

/// Classifies the working-tree state of one repository-relative path.
///
/// Ancestor directories are checked once each through `memo`, so a scope
/// with many documents under one directory does not repeat work.
fn classify_worktree(
    repo_root: &Path,
    relative: &OsStr,
    memo: &mut HashMap<PathBuf, DirState>,
) -> io::Result<Worktree> {
    let components: Vec<&OsStr> = Path::new(relative)
        .components()
        .map(|component| component.as_os_str())
        .collect();
    let mut path = repo_root.to_path_buf();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        path.push(component);
        let state = match memo.get(&path) {
            Some(state) => *state,
            None => {
                let state = match fs::symlink_metadata(&path) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => DirState::Missing,
                    Err(error) => return Err(error),
                    Ok(metadata) if metadata.file_type().is_symlink() => DirState::Symlink,
                    Ok(metadata) if metadata.is_dir() => DirState::Directory,
                    // Nothing can exist below a tracked ancestor file.
                    Ok(_) => DirState::Missing,
                };
                memo.insert(path.clone(), state);
                state
            }
        };
        match state {
            DirState::Missing => return Ok(Worktree::Absent),
            DirState::Symlink => return Ok(Worktree::Symlink),
            DirState::Directory => {}
        }
    }
    path.push(components.last().copied().unwrap_or_default());
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Worktree::Absent),
        Err(error) => Err(error),
        Ok(metadata) if metadata.file_type().is_symlink() => Ok(Worktree::Symlink),
        Ok(_) => Ok(Worktree::Present),
    }
}

/// Builds the blocker for one untracked directory entry, spelled with a
/// trailing slash by `git ls-files --others`. The entry must lie inside the
/// scope boundaries; entries outside them are not actionable and produce no
/// blocker.
fn directory_blocker(
    repo_root: &Path,
    entry: &[u8],
    descriptor: &ScopeDescriptor,
) -> Option<ScopeBlocker> {
    if !descriptor.contains_directory(entry) {
        return None;
    }
    let trimmed = entry.strip_suffix(b"/").unwrap_or(entry);
    let path = os_string_from_bytes(trimmed.to_vec());
    let absolute = repo_root.join(&path);
    match fs::symlink_metadata(&absolute) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Some(ScopeBlocker::SymlinkPath { path })
        }
        Ok(metadata) if metadata.is_dir() => {
            if fs::symlink_metadata(absolute.join(".git")).is_ok() {
                Some(ScopeBlocker::NestedRepository { path })
            } else {
                None
            }
        }
        // The directory vanished between listing and inspection.
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Canonical identities (SPEC 8.6)
// ---------------------------------------------------------------------------

/// Failure of one identity computation.
#[derive(Debug)]
pub enum IdentityError {
    /// A Git process failed.
    Git(GitError),
    /// The working tree could not be inspected.
    Io(io::Error),
    /// One scoped entry cannot join a content digest: a symbolic link, a
    /// gitlink, a directory, or a path that is not valid UTF-8. Such entries
    /// are scope blockers (SPEC 3.2 rule 5), so the identity refuses instead
    /// of silently narrowing the content it attests.
    UnsupportedEntry {
        /// The repository-relative path.
        path: OsString,
        /// What the entry is.
        what: &'static str,
    },
}

impl IdentityError {
    /// The SPEC 7.5 error code of this failure.
    pub fn public_code(&self) -> &'static str {
        match self {
            IdentityError::Git(error) => error.public_code(),
            IdentityError::Io(_) => "IO_FAILED",
            IdentityError::UnsupportedEntry { .. } => "UNSUPPORTED_REPOSITORY",
        }
    }
}

impl fmt::Display for IdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdentityError::Git(error) => write!(formatter, "{error}"),
            IdentityError::Io(error) => {
                write!(formatter, "cannot inspect the working tree: {error}")
            }
            IdentityError::UnsupportedEntry { path, what } => {
                write!(formatter, "{what} at {:?}", path.to_string_lossy())
            }
        }
    }
}

impl std::error::Error for IdentityError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            IdentityError::Git(error) => Some(error),
            IdentityError::Io(error) => Some(error),
            IdentityError::UnsupportedEntry { .. } => None,
        }
    }
}

impl From<GitError> for IdentityError {
    fn from(error: GitError) -> Self {
        IdentityError::Git(error)
    }
}

impl From<io::Error> for IdentityError {
    fn from(error: io::Error) -> Self {
        IdentityError::Io(error)
    }
}

/// Computes every canonical identity of SPEC 8.6 through Git's object
/// hashing interface: `git hash-object --stdin` without `-w`, which
/// computes an object ID without requesting object storage.
///
/// The repository object format is read once and every identity includes it
/// through [`Fingerprint::id`], so the same canonical bytes yield different
/// identifiers in a SHA-1 and a SHA-256 repository.
#[derive(Debug)]
pub struct Identities<'a> {
    git: &'a Git,
    object_format: ObjectFormat,
}

impl<'a> Identities<'a> {
    /// Reads the repository object format once.
    pub fn new(git: &'a Git) -> Result<Self, GitError> {
        Ok(Identities {
            git,
            object_format: git.object_format()?,
        })
    }

    /// Hashes canonical bytes into one fingerprint.
    fn hash(&self, canonical: &[u8]) -> Result<Fingerprint, GitError> {
        Ok(Fingerprint {
            object_format: self.object_format,
            oid: self.git.hash_object(canonical)?,
        })
    }

    /// The scope digest of one canonical descriptor (SPEC 3.2, 8.6).
    pub fn scope(&self, descriptor: &ScopeDescriptor) -> Result<Fingerprint, GitError> {
        self.hash(&descriptor.digest_input_bytes())
    }

    /// The scoped content digest of sorted `(path, mode, blob_oid)` entries
    /// (SPEC 8.6). The entry set itself distinguishes presence from absence.
    pub fn content(&self, entries: &[ContentEntry]) -> Result<Fingerprint, GitError> {
        self.hash(&domain::content_digest_input_bytes(entries))
    }

    /// The manifest digest of every group, path, patch byte, and ordering
    /// (SPEC 7.2, 8.6).
    pub fn manifest(&self, groups: &[ChangeGroup]) -> Result<Fingerprint, GitError> {
        self.hash(&domain::manifest_digest_input_bytes(groups))
    }

    /// The review identity of one pinned comparison (SPEC 8.6).
    pub fn review(&self, context: &ReviewContext) -> Result<Fingerprint, GitError> {
        self.hash(&domain::review_id_input_bytes(context))
    }

    /// The cursor digest of one detail-page position (SPEC 6.3): the review
    /// identity, the page contract, and the position in one canonical
    /// input. The digest lets a stateless caller be told that its cursor
    /// belongs to another comparison or contract; it authorizes nothing.
    pub fn cursor(&self, context: &domain::CursorContext) -> Result<Fingerprint, GitError> {
        self.hash(&domain::cursor_digest_input_bytes(context))
    }

    /// The cursor digest of one timeline page position (SPEC 6.3): the
    /// commit the walk starts from, the adopted scope, the checkpoint
    /// display policy, the presentation filter, and the position in one
    /// canonical input under its own domain, so a timeline cursor never
    /// matches a detail-page cursor either. The digest authorizes nothing.
    pub fn log_cursor(&self, context: &domain::LogCursorContext) -> Result<Fingerprint, GitError> {
        self.hash(&domain::log_cursor_digest_input_bytes(context))
    }

    /// The change identity of the group at `group_index` of a manifest
    /// (SPEC 8.6): the review it belongs to, the literal path, the original
    /// complete patch units, and the group ordering.
    pub fn change(
        &self,
        review: &Fingerprint,
        group: &ChangeGroup,
        group_index: u64,
    ) -> Result<Fingerprint, GitError> {
        self.hash(&domain::change_id_input_bytes(&ChangeIdentity {
            review_oid: review.oid.clone(),
            path: group.path.clone(),
            patch_units: group.patch_units.clone(),
            group_index,
        }))
    }

    /// The change identities of a whole manifest, in manifest order
    /// (SPEC 8.6).
    pub fn changes(
        &self,
        review: &Fingerprint,
        groups: &[ChangeGroup],
    ) -> Result<Vec<Fingerprint>, GitError> {
        // Every change identity of one manifest hashes through one batched
        // process, in manifest order (SPEC 17.5).
        let inputs: Vec<Vec<u8>> = groups
            .iter()
            .enumerate()
            .map(|(index, group)| {
                domain::change_id_input_bytes(&ChangeIdentity {
                    review_oid: review.oid.clone(),
                    path: group.path.clone(),
                    patch_units: group.patch_units.clone(),
                    group_index: index as u64,
                })
            })
            .collect();
        Ok(self
            .git
            .hash_objects(&inputs)?
            .into_iter()
            .map(|oid| Fingerprint {
                object_format: self.object_format,
                oid,
            })
            .collect())
    }

    /// The plan fingerprint of one submitted plan (SPEC 7.3, 8.6).
    pub fn plan(&self, plan: &PlanIdentity) -> Result<Fingerprint, GitError> {
        self.hash(&domain::plan_fingerprint_input_bytes(plan))
    }

    /// The plan fingerprint of one submitted restoration plan (SPEC 7.4,
    /// 8.6). The shared plan-fingerprint domain keeps the two operation
    /// kinds separable by their leading operation field.
    pub fn restore_plan(&self, plan: &RestoreIdentity) -> Result<Fingerprint, GitError> {
        self.hash(&domain::restore_plan_fingerprint_input_bytes(plan))
    }
}

/// The scoped working-content digest of SPEC 8.6: raw working bytes hashed
/// under the identity profile, with mode and presence per entry.
///
/// Presence is membership in the entry set, so a deleted tracked path is
/// absent from the digest. Every present scoped path is hashed by Git in
/// batched processes, so the digest does not scale one process per document
/// (SPEC 17.5). The scope must come from [`resolve_scope`], which already
/// reports symbolic links, nested repositories, and non-UTF-8 paths as
/// blockers.
pub fn working_content_digest(
    git: &Git,
    scope: &ResolvedScope,
) -> Result<Fingerprint, IdentityError> {
    let repo_root = git.toplevel()?;
    let present: Vec<OsString> = scope
        .paths
        .iter()
        .filter(|(_, state)| **state != ScopedPathState::TrackedDeleted)
        .map(|(path, _)| path.clone())
        .collect();
    let blob_oids = git.hash_object_paths(&present)?;
    let mut entries = Vec::with_capacity(present.len());
    for (path, blob_oid) in present.iter().zip(&blob_oids) {
        let text = path
            .to_str()
            .ok_or_else(|| IdentityError::UnsupportedEntry {
                path: path.clone(),
                what: "the scoped path is not valid UTF-8",
            })?;
        let metadata = fs::symlink_metadata(repo_root.join(path))?;
        if metadata.file_type().is_symlink() {
            return Err(IdentityError::UnsupportedEntry {
                path: path.clone(),
                what: "a symbolic link sits at the scoped path",
            });
        }
        if !metadata.is_file() {
            return Err(IdentityError::UnsupportedEntry {
                path: path.clone(),
                what: "the scoped working entry is not a regular file",
            });
        }
        entries.push(ContentEntry {
            path: text.to_string(),
            mode: working_mode(&metadata),
            blob_oid: blob_oid.clone(),
        });
    }
    Ok(Identities::new(git)?.content(&entries)?)
}

/// The storage mode of one working file: Git records only the owner
/// executable bit of a regular file, so that one bit decides between
/// `100755` and `100644`.
fn working_mode(metadata: &fs::Metadata) -> EntryMode {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o100 != 0 {
            return EntryMode::Executable;
        }
    }
    EntryMode::Regular
}

/// The scoped content digest of one committed revision (SPEC 7.2
/// `candidate_scope_digest`, 8.6): the sorted `(path, mode, blob_oid)`
/// entries of every scoped path in the revision's tree.
///
/// `revision` must be a validated canonical commit object ID from the
/// adapter, never a user string (SPEC 14.1).
pub fn scoped_content_digest(
    git: &Git,
    revision: &str,
    scope: &ResolvedScope,
) -> Result<Fingerprint, IdentityError> {
    recorded_content_digest(git, revision, &scope.descriptor)
}

/// The scoped content digest of one committed revision through one recorded
/// canonical descriptor (SPEC 8.6, 8.7): the same digest as
/// [`scoped_content_digest`], answered from immutable tree objects alone.
///
/// A receipt response uses this path, because the descriptor a verified
/// marker records already names the scope the request ran under: no working
/// path is traversed, so a scope root that became a symlink, vanished, or
/// became unreadable cannot hide a receipt that history still holds.
fn recorded_content_digest(
    git: &Git,
    revision: &str,
    descriptor: &ScopeDescriptor,
) -> Result<Fingerprint, IdentityError> {
    let entries = tree_content_entries(git, revision, descriptor)?;
    Ok(Identities::new(git)?.content(&entries)?)
}

/// The scoped entries that differ between two immutable revisions, sorted
/// by path bytes (SPEC 8.7): every path of the recorded scope whose
/// `(mode, blob_oid)` entry changed from `before` to `after`.
///
/// Like [`recorded_content_digest`], this reads only committed tree objects
/// through one recorded descriptor, so a receipt response derives its
/// rewritten paths from history alone — no grouping engine reruns and no
/// working path is traversed.
fn recorded_tree_difference(
    git: &Git,
    before: &str,
    after: &str,
    descriptor: &ScopeDescriptor,
) -> Result<Vec<String>, IdentityError> {
    let entries_of = |revision| tree_content_entries(git, revision, descriptor);
    let entry_map = |entries: Vec<ContentEntry>| {
        entries
            .into_iter()
            .map(|entry| (entry.path, (entry.mode, entry.blob_oid)))
            .collect::<BTreeMap<String, (EntryMode, String)>>()
    };
    let before = entry_map(entries_of(before)?);
    let after = entry_map(entries_of(after)?);
    Ok(before
        .keys()
        .chain(after.keys())
        .collect::<BTreeSet<&String>>()
        .into_iter()
        .filter(|path| before.get(*path) != after.get(*path))
        .cloned()
        .collect())
}

/// Collects the content entries of every scoped path in `revision`'s tree.
fn tree_content_entries(
    git: &Git,
    revision: &str,
    descriptor: &ScopeDescriptor,
) -> Result<Vec<ContentEntry>, IdentityError> {
    let mut entries = Vec::new();
    for entry in git.ls_tree(revision)? {
        if !descriptor.contains_path(entry.path.as_encoded_bytes()) {
            continue;
        }
        let path = entry
            .path
            .to_str()
            .ok_or_else(|| IdentityError::UnsupportedEntry {
                path: entry.path.clone(),
                what: "the scoped path is not valid UTF-8",
            })?
            .to_string();
        let mode =
            EntryMode::parse(&entry.mode).ok_or_else(|| IdentityError::UnsupportedEntry {
                path: entry.path.clone(),
                what: stored_entry_reason(&entry.mode),
            })?;
        entries.push(ContentEntry {
            path,
            mode,
            blob_oid: entry.oid,
        });
    }
    Ok(entries)
}

/// Names why one stored entry mode cannot join a content digest.
fn stored_entry_reason(mode: &str) -> &'static str {
    match mode {
        "120000" => "a symbolic link is stored at the scoped path",
        "160000" => "a gitlink is stored at the scoped path",
        _ => "the stored mode is not a regular file mode",
    }
}

/// One unsupported repository, branch, index, or storage condition found
/// before a mutation (SPEC 3.1 and 8.8). Every variant names the exact
/// condition, so an interface can explain the specific blocker instead of
/// silently lowering its guarantee.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PolicyBlocker {
    /// The repository is bare and has no working tree (SPEC 3.1).
    BareRepository,
    /// The working directory lies outside any working tree, for example
    /// inside the Git directory itself.
    NotInWorkTree,
    /// The repository has more than one registered working tree.
    LinkedWorktree,
    /// Sparse checkout is enabled, so index entries do not describe the
    /// full working tree.
    SparseCheckout,
    /// The repository is a shallow clone with cut-off history.
    ShallowClone,
    /// The repository has the promisor remote `remote` of a partial clone,
    /// so objects may be missing until a lazy fetch.
    PartialClone {
        /// The promisor remote name, such as `origin`.
        remote: String,
    },
    /// The repository is nested inside the working tree of `superproject`
    /// as a gitlink, so it is itself a submodule.
    NestedRepository {
        /// The working tree of the enclosing superproject.
        superproject: String,
    },
    /// A merge is in progress.
    ActiveMerge,
    /// A rebase is in progress.
    ActiveRebase,
    /// A cherry-pick is in progress.
    ActiveCherryPick,
    /// A revert is in progress.
    ActiveRevert,
    /// A bisect session is in progress.
    ActiveBisect,
    /// A cherry-pick or revert sequence is in progress.
    ActiveSequencer,
    /// The index holds unresolved conflict stages (SPEC 3.1).
    UnresolvedConflict {
        /// How many paths hold conflict stages.
        count: usize,
        /// At most [`MAX_LISTED_PATHS`] of those paths.
        paths: Vec<OsString>,
    },
    /// `HEAD` is detached or points outside `refs/heads/`, so no attached
    /// branch can be adopted (SPEC 3.1).
    DetachedHead,
    /// `HEAD` sits on `current` while the workspace adopted `adopted`
    /// (SPEC 8.2). Margin never switches or re-adopts a branch on its own,
    /// so only one explicit `init` adopts the new branch. Two branches at
    /// the same commit still mismatch: the names, not the commits, decide.
    BranchMismatch {
        /// The branch the local settings adopted.
        adopted: String,
        /// The branch `HEAD` is attached to.
        current: String,
    },
    /// The index differs from `HEAD`; normal mutations require a clean
    /// index (SPEC 3.1). On an unborn branch any staged entry counts.
    StagedChanges {
        /// How many staged paths differ.
        count: usize,
        /// At most [`MAX_LISTED_PATHS`] of those paths.
        paths: Vec<OsString>,
    },
    /// Scoped index entries are marked assume-unchanged, so Git reports no
    /// working change for them (SPEC 3.1). Working content can differ from
    /// the candidate while every Git change report stays clean, and Git's
    /// own staging honors the flag too, so no normal mutation proceeds
    /// behind it.
    AssumeUnchangedEntries {
        /// How many scoped paths carry the flag.
        count: usize,
        /// At most [`MAX_LISTED_PATHS`] of those paths.
        paths: Vec<OsString>,
    },
    /// Scoped index entries are marked skip-worktree — the sparse-checkout
    /// mechanism — so Git reports no working change for them and promises
    /// not to touch their working files (SPEC 3.1). Unsaved content can
    /// hide behind the flag, and a Margin write would fight that promise,
    /// so no normal mutation proceeds behind it.
    SkipWorktreeEntries {
        /// How many scoped paths carry the flag.
        count: usize,
        /// At most [`MAX_LISTED_PATHS`] of those paths.
        paths: Vec<OsString>,
    },
    /// `core.autocrlf` converts line endings on check-in; the
    /// `identity-v1` profile requires `false` (SPEC 8.8).
    AutoCrlfConversion {
        /// The effective configuration value.
        value: String,
    },
    /// The `text` attribute normalizes line endings for the path.
    TextNormalization {
        /// The scoped path.
        path: OsString,
        /// The effective attribute value, such as `auto`.
        value: String,
    },
    /// The `eol` attribute fixes line endings for the path.
    EolNormalization {
        /// The scoped path.
        path: OsString,
        /// The effective attribute value, such as `crlf`.
        value: String,
    },
    /// A content filter driver cleans or smudges the path.
    ContentFilter {
        /// The scoped path.
        path: OsString,
        /// The filter driver name.
        filter: String,
    },
    /// The path converts between its working-tree and repository encoding.
    WorkingTreeEncoding {
        /// The scoped path.
        path: OsString,
        /// The effective encoding name.
        encoding: String,
    },
    /// The path expands `$Id$` markers on check-in.
    IdentExpansion {
        /// The scoped path.
        path: OsString,
    },
    /// A scoped working file disappeared while its staging was verified.
    AbsentWorkingFile {
        /// The scoped path.
        path: OsString,
    },
    /// Staging the file through Git would store bytes that differ from the
    /// raw working bytes (SPEC 8.8).
    ByteTransformation {
        /// The scoped path.
        path: OsString,
    },
}

impl PolicyBlocker {
    /// The SPEC 7.5 error code of this blocker.
    pub fn public_code(&self) -> &'static str {
        match self {
            PolicyBlocker::StagedChanges { .. } => "STAGED_CHANGES",
            PolicyBlocker::BranchMismatch { .. } => "BRANCH_CHANGED",
            _ => "UNSUPPORTED_REPOSITORY",
        }
    }
}

impl fmt::Display for PolicyBlocker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn quoted(path: &OsStr) -> String {
            format!("{:?}", path.to_string_lossy())
        }
        fn listed(count: usize, paths: &[OsString]) -> String {
            let mut names: Vec<String> = paths.iter().map(|path| quoted(path)).collect();
            if count > paths.len() {
                names.push(format!("and {} more", count - paths.len()));
            }
            names.join(", ")
        }
        match self {
            PolicyBlocker::BareRepository => {
                write!(formatter, "the repository is bare and has no working tree")
            }
            PolicyBlocker::NotInWorkTree => {
                write!(formatter, "the directory is not inside a Git working tree")
            }
            PolicyBlocker::LinkedWorktree => {
                write!(
                    formatter,
                    "the repository uses linked worktrees instead of one working tree"
                )
            }
            PolicyBlocker::SparseCheckout => {
                write!(formatter, "sparse checkout is enabled for the repository")
            }
            PolicyBlocker::ShallowClone => {
                write!(
                    formatter,
                    "the repository is a shallow clone with cut-off history"
                )
            }
            PolicyBlocker::PartialClone { remote } => write!(
                formatter,
                "the repository has the promisor remote {remote:?} of a partial clone"
            ),
            PolicyBlocker::NestedRepository { superproject } => write!(
                formatter,
                "the repository is nested inside the superproject at {superproject}"
            ),
            PolicyBlocker::ActiveMerge => write!(formatter, "a merge is in progress"),
            PolicyBlocker::ActiveRebase => write!(formatter, "a rebase is in progress"),
            PolicyBlocker::ActiveCherryPick => write!(formatter, "a cherry-pick is in progress"),
            PolicyBlocker::ActiveRevert => write!(formatter, "a revert is in progress"),
            PolicyBlocker::ActiveBisect => write!(formatter, "a bisect session is in progress"),
            PolicyBlocker::ActiveSequencer => {
                write!(formatter, "a cherry-pick or revert sequence is in progress")
            }
            PolicyBlocker::UnresolvedConflict { count, paths } => write!(
                formatter,
                "the index holds unresolved conflicts at {count} path(s): {}",
                listed(*count, paths)
            ),
            PolicyBlocker::DetachedHead => write!(
                formatter,
                "HEAD is detached instead of attached to a branch under refs/heads/"
            ),
            PolicyBlocker::BranchMismatch { adopted, current } => write!(
                formatter,
                "HEAD is on branch {current}, but the workspace adopted {adopted}; \
                 only an explicit init adopts another branch"
            ),
            PolicyBlocker::StagedChanges { count, paths } => write!(
                formatter,
                "the index has {count} staged change(s) against HEAD: {}",
                listed(*count, paths)
            ),
            PolicyBlocker::AssumeUnchangedEntries { count, paths } => write!(
                formatter,
                "assume-unchanged is set for {count} scoped path(s): {}; Git then reports no \
                 working change there, so run git update-index --no-assume-unchanged on each \
                 path before a Margin write",
                listed(*count, paths)
            ),
            PolicyBlocker::SkipWorktreeEntries { count, paths } => write!(
                formatter,
                "skip-worktree is set for {count} scoped path(s): {}; Git then reports no \
                 working change there, so run git update-index --no-skip-worktree on each path \
                 before a Margin write",
                listed(*count, paths)
            ),
            PolicyBlocker::AutoCrlfConversion { value } => write!(
                formatter,
                "core.autocrlf is {value:?}; the identity storage profile requires false"
            ),
            PolicyBlocker::TextNormalization { path, value } => write!(
                formatter,
                "the attribute text={value:?} normalizes the scoped path {}",
                quoted(path)
            ),
            PolicyBlocker::EolNormalization { path, value } => write!(
                formatter,
                "the attribute eol={value:?} sets line endings for the scoped path {}",
                quoted(path)
            ),
            PolicyBlocker::ContentFilter { path, filter } => write!(
                formatter,
                "the content filter {filter:?} applies to the scoped path {}",
                quoted(path)
            ),
            PolicyBlocker::WorkingTreeEncoding { path, encoding } => write!(
                formatter,
                "the working-tree encoding {encoding:?} applies to the scoped path {}",
                quoted(path)
            ),
            PolicyBlocker::IdentExpansion { path } => write!(
                formatter,
                "ident expansion applies to the scoped path {}",
                quoted(path)
            ),
            PolicyBlocker::AbsentWorkingFile { path } => write!(
                formatter,
                "the scoped working file {} disappeared during inspection",
                quoted(path)
            ),
            PolicyBlocker::ByteTransformation { path } => write!(
                formatter,
                "staging {} would transform its raw bytes",
                quoted(path)
            ),
        }
    }
}

/// Failure of a policy inspection: a Git process or the filesystem failed
/// before the guards could decide.
#[derive(Debug)]
pub enum PolicyError {
    /// A Git process failed.
    Git(GitError),
    /// The working tree could not be inspected.
    Io(io::Error),
}

impl PolicyError {
    /// The SPEC 7.5 error code of this failure.
    pub fn public_code(&self) -> &'static str {
        match self {
            PolicyError::Git(error) => error.public_code(),
            PolicyError::Io(_) => "IO_FAILED",
        }
    }
}

impl fmt::Display for PolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PolicyError::Git(error) => write!(formatter, "{error}"),
            PolicyError::Io(error) => {
                write!(formatter, "cannot inspect the repository: {error}")
            }
        }
    }
}

impl std::error::Error for PolicyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PolicyError::Git(error) => Some(error),
            PolicyError::Io(error) => Some(error),
        }
    }
}

impl From<GitError> for PolicyError {
    fn from(error: GitError) -> Self {
        PolicyError::Git(error)
    }
}

impl From<io::Error> for PolicyError {
    fn from(error: io::Error) -> Self {
        PolicyError::Io(error)
    }
}

/// The combined result of the mutation policy guards: every unsupported
/// condition found, in guard order (layout, operations, branch, index,
/// storage).
#[derive(Clone, Debug)]
pub struct MutationPolicyReport {
    /// Every blocker; empty means a normal mutation may proceed.
    pub blockers: Vec<PolicyBlocker>,
}

impl MutationPolicyReport {
    /// Says whether a normal mutation may proceed.
    pub fn ok(&self) -> bool {
        self.blockers.is_empty()
    }
}

/// Largest number of paths one blocker lists before it reports only the
/// remaining count (SPEC 7.6 bounds diagnostics).
const MAX_LISTED_PATHS: usize = 20;

/// Repository metadata markers of in-progress Git operations (SPEC 3.1).
/// `rebase-merge`, `rebase-apply`, and `sequencer` are directories; the
/// rest are files.
const OPERATION_MARKERS: &[(&str, PolicyBlocker)] = &[
    ("MERGE_HEAD", PolicyBlocker::ActiveMerge),
    ("CHERRY_PICK_HEAD", PolicyBlocker::ActiveCherryPick),
    ("REVERT_HEAD", PolicyBlocker::ActiveRevert),
    ("BISECT_LOG", PolicyBlocker::ActiveBisect),
    ("rebase-merge", PolicyBlocker::ActiveRebase),
    ("rebase-apply", PolicyBlocker::ActiveRebase),
    ("sequencer", PolicyBlocker::ActiveSequencer),
];

/// The attributes the `identity-v1` storage profile cannot act on (SPEC
/// 8.8). The order is the order `git check-attr` reports them in.
pub const TRANSFORMATION_ATTRIBUTES: &[&str] =
    &["text", "eol", "filter", "working-tree-encoding", "ident"];

/// Inspects the repository, branch, index, and in-progress-operation policy
/// of SPEC 3.1. Every check only observes.
///
/// The scope is not needed here: index and branch policy are repository
/// properties, not scoped ones.
pub fn repository_policy_blockers(git: &Git) -> Result<Vec<PolicyBlocker>, PolicyError> {
    let mut blockers = Vec::new();

    // Layout: one local, non-bare repository with one conventional working
    // tree. Without a working tree no further check can run.
    if git.is_bare_repository()? {
        blockers.push(PolicyBlocker::BareRepository);
        return Ok(blockers);
    }
    if !git.is_inside_work_tree()? {
        blockers.push(PolicyBlocker::NotInWorkTree);
        return Ok(blockers);
    }
    if git.worktree_paths()?.len() > 1 {
        blockers.push(PolicyBlocker::LinkedWorktree);
    }
    if let Some(value) = git.config_value("core.sparseCheckout")?
        && config_bool(&value).unwrap_or(true)
    {
        blockers.push(PolicyBlocker::SparseCheckout);
    }
    if git.git_path("shallow")?.exists() {
        blockers.push(PolicyBlocker::ShallowClone);
    }
    let mut promisor_remotes: Vec<String> = git
        .config_matches(r"remote\..*\.promisor")?
        .into_iter()
        .filter(|(_, value)| config_bool(value).unwrap_or(true))
        .map(|(key, _)| {
            key.trim_start_matches("remote.")
                .trim_end_matches(".promisor")
                .to_string()
        })
        .collect();
    for (_, remote) in git.config_matches("extensions.partialclone")? {
        if !promisor_remotes.contains(&remote) {
            promisor_remotes.push(remote);
        }
    }
    for remote in promisor_remotes {
        blockers.push(PolicyBlocker::PartialClone { remote });
    }
    if let Some(superproject) = git.superproject_working_tree()? {
        blockers.push(PolicyBlocker::NestedRepository { superproject });
    }

    // In-progress Git operations (SPEC 3.1), resolved by one process.
    let names: Vec<&str> = OPERATION_MARKERS.iter().map(|(marker, _)| *marker).collect();
    for ((_marker, blocker), path) in OPERATION_MARKERS.iter().zip(git.git_paths(&names)?) {
        if path.exists() {
            blockers.push(blocker.clone());
        }
    }

    // Branch: one attached branch, unborn or born.
    match git.head_symbolic_ref()? {
        Some(branch_ref) if branch_ref.starts_with("refs/heads/") => {}
        _ => blockers.push(PolicyBlocker::DetachedHead),
    }

    // Index: globally clean against HEAD, with no conflict stages.
    let index = git.ls_files_stage()?;
    // One path with several conflict stages is one conflict.
    let mut conflicted: Vec<OsString> = index
        .iter()
        .filter(|entry| entry.stage > 0)
        .map(|entry| entry.path.clone())
        .collect();
    conflicted.sort_by(|left, right| left.as_encoded_bytes().cmp(right.as_encoded_bytes()));
    conflicted.dedup();
    if !conflicted.is_empty() {
        blockers.push(PolicyBlocker::UnresolvedConflict {
            count: conflicted.len(),
            paths: list_paths(&conflicted),
        });
    }
    let staged = staged_paths_against_head(git, &index)?;
    if !staged.is_empty() {
        blockers.push(PolicyBlocker::StagedChanges {
            count: staged.len(),
            paths: list_paths(&staged),
        });
    }
    Ok(blockers)
}

/// The paths staged against `HEAD` (SPEC 9.1 step 4): `git diff-index
/// --cached` against the candidate, or every stage-zero entry while the
/// branch is unborn and no `HEAD` exists to compare with. The mutation
/// policy and the recovery inspection share this one observation, so both
/// spell the same state the same way.
fn staged_paths_against_head(git: &Git, index: &[IndexEntry]) -> Result<Vec<OsString>, GitError> {
    match git.head_commit()? {
        Some(_) => git.staged_against("HEAD"),
        None => Ok(index
            .iter()
            .filter(|entry| entry.stage == 0)
            .map(|entry| entry.path.clone())
            .collect()),
    }
}

/// Inspects the storage policy of the `identity-v1` profile for one
/// resolved scope (SPEC 8.8): effective `core.autocrlf`, and the effective
/// `text`, `eol`, `filter`, `working-tree-encoding`, and `ident`
/// attributes of every scoped working file. Explicit non-text markings
/// such as `-text` or `binary` stay allowed; every configured
/// transformation is a blocker.
pub fn storage_policy_blockers(
    git: &Git,
    scope: &ResolvedScope,
) -> Result<Vec<PolicyBlocker>, PolicyError> {
    let mut blockers = Vec::new();
    // SPEC 8.8 names core.autocrlf only; without a text attribute
    // core.eol converts nothing, so the attribute check below covers it.
    if let Some(value) = git.config_value("core.autocrlf")?
        && config_bool(&value).unwrap_or(true)
    {
        blockers.push(PolicyBlocker::AutoCrlfConversion { value });
    }
    let present = present_scope_paths(scope);
    for assignment in git.check_attr(TRANSFORMATION_ATTRIBUTES, &present)? {
        if let Some(blocker) = attribute_violation(&assignment) {
            blockers.push(blocker);
        }
    }
    Ok(blockers)
}

/// Inspects the scoped index entries Git hides from its own change
/// detection (SPEC 3.1, 9.1 step 4): `assume-unchanged` and
/// `skip-worktree`. A flagged path shows no working change in `git status`
/// or the diff calls, so unsaved content can differ from the candidate
/// behind an apparently clean repository, and Git's staging honors the
/// flag as well, so an attempted capture could silently store the old
/// bytes. Every scoped flag is one specific blocker before any mutation
/// (INV-08); the inspection only reads the flags and preserves them
/// exactly as the user set them.
pub fn hidden_index_blockers(
    git: &Git,
    scope: &ResolvedScope,
) -> Result<Vec<PolicyBlocker>, PolicyError> {
    let mut assume_unchanged: Vec<OsString> = Vec::new();
    let mut skip_worktree: Vec<OsString> = Vec::new();
    for (path, flag) in git.hidden_index_paths()? {
        if !scope.paths.contains_key(&path) {
            continue;
        }
        match flag {
            IndexFlag::AssumeUnchanged => assume_unchanged.push(path),
            IndexFlag::SkipWorktree => skip_worktree.push(path),
        }
    }
    let mut blockers = Vec::new();
    if !assume_unchanged.is_empty() {
        blockers.push(PolicyBlocker::AssumeUnchangedEntries {
            count: assume_unchanged.len(),
            paths: list_paths(&assume_unchanged),
        });
    }
    if !skip_worktree.is_empty() {
        blockers.push(PolicyBlocker::SkipWorktreeEntries {
            count: skip_worktree.len(),
            paths: list_paths(&skip_worktree),
        });
    }
    Ok(blockers)
}

/// The one shared mutation guard (SPEC 9.1 step 4 and 13.6). Every
/// interface calls this before a normal mutation writes.
///
/// `scope` supplies the scoped paths of the storage policy; pass `None`
/// when no scope is resolved yet, for example while reporting why a
/// repository cannot be adopted at all. Without a usable working tree the
/// storage policy is skipped, because Git cannot answer it.
pub fn enforce_mutation_policy(
    git: &Git,
    scope: Option<&ResolvedScope>,
) -> Result<MutationPolicyReport, PolicyError> {
    let mut blockers = repository_policy_blockers(git)?;
    let worktree_usable = !blockers.iter().any(|blocker| {
        matches!(
            blocker,
            PolicyBlocker::BareRepository | PolicyBlocker::NotInWorkTree
        )
    });
    if let Some(scope) = scope
        && worktree_usable
    {
        // Index flags before storage transformations: a flag that hides a
        // path from Git's own reporting is an index-state refusal, in the
        // guard order of the report (SPEC 3.1, 9.1 step 4).
        blockers.extend(hidden_index_blockers(git, scope)?);
        blockers.extend(storage_policy_blockers(git, scope)?);
    }
    Ok(MutationPolicyReport { blockers })
}

/// Proves the raw-byte half of the `identity-v1` profile with Git itself
/// (SPEC 8.8): each scoped working file is staged into one disposable
/// index, and the staged blob must equal the hash of the raw working
/// bytes.
///
/// The check never touches the real index, refs, or working files, but it
/// does create disposable Git objects and a temporary index file, which
/// SPEC 6.4 and 8.2 permit for computation. Any mismatch means Git's own
/// staging pipeline would transform the bytes, whatever configured it.
pub fn verify_identity_profile(
    git: &Git,
    scope: &ResolvedScope,
) -> Result<Vec<PolicyBlocker>, PolicyError> {
    let present = present_scope_paths(scope);
    if present.is_empty() {
        return Ok(Vec::new());
    }
    let repo_root = git.toplevel()?;
    let temp_index = reserve_temp_index()?;
    let result = staged_byte_blockers(git, &repo_root, &temp_index, &present);
    // The disposable index never outlives the check, on success or failure.
    let _ = fs::remove_file(&temp_index);
    result
}

/// Compares Git's staging of `paths` against their raw working bytes
/// through the disposable index at `temp_index`.
fn staged_byte_blockers(
    git: &Git,
    repo_root: &Path,
    temp_index: &Path,
    paths: &[OsString],
) -> Result<Vec<PolicyBlocker>, PolicyError> {
    let mut probe = git.clone();
    probe.set_env_override("GIT_INDEX_FILE", temp_index);
    probe.read_tree_empty()?;
    // `--add --remove` stages the current working content of every path,
    // with the effective conversion pipeline applied, exactly as a later
    // real staging would.
    probe.stage_paths(paths)?;
    let staged: BTreeMap<OsString, String> = probe
        .ls_files_stage()?
        .into_iter()
        .filter(|entry| entry.stage == 0)
        .map(|entry| (entry.path, entry.oid))
        .collect();

    let mut blockers = Vec::new();
    // The raw bytes of every present path are hashed by one batched call,
    // never one process per document (SPEC 17.5). A path whose file cannot
    // be inspected is the same hard failure it was per path.
    let mut present: Vec<OsString> = Vec::new();
    for path in paths {
        match fs::symlink_metadata(repo_root.join(path)) {
            Ok(_) => present.push(path.clone()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                blockers.push(PolicyBlocker::AbsentWorkingFile { path: path.clone() });
            }
            Err(error) => return Err(PolicyError::Io(error)),
        }
    }
    let raw_oids = git.hash_object_paths(&present)?;
    for (path, raw_oid) in present.iter().zip(raw_oids) {
        let Some(staged_oid) = staged.get(path) else {
            // The staging probe listed nothing for a path that exists now:
            // it vanished between the two reads, which is the same absent
            // working file.
            blockers.push(PolicyBlocker::AbsentWorkingFile { path: path.clone() });
            continue;
        };
        if raw_oid != *staged_oid {
            blockers.push(PolicyBlocker::ByteTransformation { path: path.clone() });
        }
    }
    Ok(blockers)
}

/// Computes the intended starting tree of one initialization through one
/// disposable Git index (SPEC 6.4, 9.1 step 5): the index starts empty for
/// an unborn branch and from the candidate's tree otherwise, then stages
/// exactly `paths` with the working content they hold.
///
/// The real index, the refs, and the working files stay untouched; only
/// disposable Git objects and one temporary index file exist, and the
/// temporary index never outlives the call, whatever the result.
fn intended_start_tree(
    git: &Git,
    head: Option<&str>,
    paths: &[OsString],
) -> Result<String, PolicyError> {
    let temp_index = reserve_temp_index()?;
    let result = (|| {
        let mut probe = git.clone();
        probe.set_env_override("GIT_INDEX_FILE", &temp_index);
        match head {
            Some(candidate) => probe.read_tree(candidate)?,
            None => probe.read_tree_empty()?,
        }
        probe.stage_paths(paths)?;
        probe.write_tree()
    })();
    let _ = fs::remove_file(&temp_index);
    result.map_err(PolicyError::from)
}

/// Scoped paths that hold working bytes: every state except a deletion.
fn present_scope_paths(scope: &ResolvedScope) -> Vec<OsString> {
    scope
        .paths
        .iter()
        .filter(|(_, state)| **state != ScopedPathState::TrackedDeleted)
        .map(|(path, _)| path.clone())
        .collect()
}

/// Reserves one exclusive temporary file for disposable Git state, named
/// after `purpose` (SPEC 8.2 allows temporary indexes and message files).
fn reserve_temp_file(purpose: &str) -> io::Result<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or_default();
    let path = std::env::temp_dir().join(format!(
        "margin-{purpose}-{}-{sequence}-{nanos}.tmp",
        std::process::id()
    ));
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    Ok(path)
}

/// Reserves one exclusive temporary file for a disposable Git index.
fn reserve_temp_index() -> io::Result<PathBuf> {
    reserve_temp_file("index")
}

/// Caps one path list for a blocker message.
fn list_paths(paths: &[OsString]) -> Vec<OsString> {
    paths.iter().take(MAX_LISTED_PATHS).cloned().collect()
}

/// Parses one Git boolean configuration value. Returns `None` when Git
/// itself would reject the value.
fn config_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" | "" => Some(false),
        _ => None,
    }
}

/// Classifies one effective attribute assignment against the
/// `identity-v1` storage profile. `unspecified` and an explicit `unset`
/// stay allowed; every specified transformation attribute is a blocker
/// (SPEC 8.8).
fn attribute_violation(assignment: &AttributeAssignment) -> Option<PolicyBlocker> {
    let specified = assignment.value != "unspecified" && assignment.value != "unset";
    if !specified {
        return None;
    }
    let path = assignment.path.clone();
    match assignment.attribute.as_str() {
        "text" => Some(PolicyBlocker::TextNormalization {
            path,
            value: assignment.value.clone(),
        }),
        "eol" => Some(PolicyBlocker::EolNormalization {
            path,
            value: assignment.value.clone(),
        }),
        "filter" => Some(PolicyBlocker::ContentFilter {
            path,
            filter: assignment.value.clone(),
        }),
        "working-tree-encoding" => Some(PolicyBlocker::WorkingTreeEncoding {
            path,
            encoding: assignment.value.clone(),
        }),
        "ident" => Some(PolicyBlocker::IdentExpansion { path }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Canonical change manifest (SPEC 10.1-10.2)
// ---------------------------------------------------------------------------

/// Failure to build one canonical change manifest.
#[derive(Debug)]
pub enum ManifestError {
    /// A Git process failed.
    Git(GitError),
    /// One scoped tree entry cannot join the comparison: a symbolic link, a
    /// gitlink, or a path that is not valid UTF-8 (SPEC 3.2 rule 5).
    Entry(IdentityError),
    /// A revision is not a validated canonical commit object ID.
    InvalidRevision {
        /// Which side of the comparison.
        which: &'static str,
        /// The rejected value.
        value: String,
    },
    /// The patch stream disagrees with the scoped tree entries: a section
    /// for an unexpected or duplicated path, or a presence transition or
    /// mode the immutable entries contradict. The fixed diff contract makes
    /// every such disagreement a contract violation, never a guess
    /// (SPEC 10.1).
    PatchContract {
        /// What disagreed.
        detail: String,
    },
    /// One patch section did not parse under the fixed contract.
    Parse(domain::SourcePatchError),
    /// The disposable validation index could not be created.
    Io(io::Error),
}

impl ManifestError {
    /// The SPEC 7.5 error code of this failure.
    pub fn public_code(&self) -> &'static str {
        match self {
            ManifestError::Git(error) => error.public_code(),
            ManifestError::Entry(error) => error.public_code(),
            ManifestError::InvalidRevision { .. } => "INVALID_ARGUMENT",
            ManifestError::PatchContract { .. } => "CONTRACT_VIOLATION",
            ManifestError::Parse(error) => error.public_code(),
            ManifestError::Io(_) => "IO_FAILED",
        }
    }
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::Git(error) => write!(formatter, "{error}"),
            ManifestError::Entry(error) => write!(formatter, "{error}"),
            ManifestError::InvalidRevision { which, value } => write!(
                formatter,
                "the {which} revision is not a validated canonical commit ID: {value}"
            ),
            ManifestError::PatchContract { detail } => {
                write!(
                    formatter,
                    "the patch stream broke the diff contract: {detail}"
                )
            }
            ManifestError::Parse(error) => write!(formatter, "{error}"),
            ManifestError::Io(error) => {
                write!(formatter, "cannot create the disposable index: {error}")
            }
        }
    }
}

impl std::error::Error for ManifestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ManifestError::Git(error) => Some(error),
            ManifestError::Entry(error) => Some(error),
            ManifestError::InvalidRevision { .. } => None,
            ManifestError::PatchContract { .. } => None,
            ManifestError::Parse(error) => Some(error),
            ManifestError::Io(error) => Some(error),
        }
    }
}

impl From<GitError> for ManifestError {
    fn from(error: GitError) -> Self {
        ManifestError::Git(error)
    }
}

impl From<IdentityError> for ManifestError {
    fn from(error: IdentityError) -> Self {
        ManifestError::Entry(error)
    }
}

impl From<domain::SourcePatchError> for ManifestError {
    fn from(error: domain::SourcePatchError) -> Self {
        ManifestError::Parse(error)
    }
}

/// One scoped B-to-C change of the canonical manifest (SPEC 10.1): the
/// immutable tree entries of both sides, the parsed patch structure, and
/// the change identity of the group.
#[derive(Clone, Debug)]
pub struct ScopedChange {
    /// Literal repository-relative path, valid UTF-8.
    pub path: String,
    /// The presence transition, cross-checked between the tree entries and
    /// the patch header.
    pub kind: domain::ChangeKind,
    /// Storage mode in B; `None` for an addition.
    pub mode_before: Option<EntryMode>,
    /// Storage mode in C; `None` for a deletion.
    pub mode_after: Option<EntryMode>,
    /// Full lowercase hex blob object ID in B; `None` for an addition.
    pub oid_before: Option<String>,
    /// Full lowercase hex blob object ID in C; `None` for a deletion.
    pub oid_after: Option<String>,
    /// Whether the content change is opaque: a binary patch without
    /// readable hunks (SPEC 3.3, 10.4).
    pub opaque: bool,
    /// The parsed hunks with their source ranges; empty for opaque and
    /// mode-only changes.
    pub hunks: Vec<domain::SourceHunk>,
    /// The canonical change-group input of this file: its literal path and
    /// its complete original patch as the one patch unit (SPEC 8.6).
    pub group: ChangeGroup,
    /// The change identity of this group within this review (SPEC 8.6).
    pub change_id: Fingerprint,
}

/// The canonical manifest of every scoped B-to-C change (SPEC 10.1-10.2,
/// 7.2): one authoritative model that counts, presentation, plan
/// validation, and mutation all derive from.
///
/// The grouping of this manifest is deliberately whole-file: one group per
/// changed path, holding that file's complete original patch. Finer
/// passages and their reasons arrive with the grouping stage (SPEC 10.3);
/// regrouping reuses the same patch bytes and ranges instead of computing a
/// second diff.
#[derive(Clone, Debug)]
pub struct ChangeManifest {
    /// The full adopted branch ref the comparison ran on.
    pub branch_ref: String,
    /// The baseline commit B.
    pub baseline_oid: String,
    /// The candidate commit C.
    pub candidate_oid: String,
    /// The canonical scope descriptor.
    pub scope: ScopeDescriptor,
    /// The engine contract version; see [`domain::ENGINE_VERSION`].
    pub engine_version: String,
    /// The effective Git version that contributed to the comparison.
    pub git_version: String,
    /// Every scoped change, sorted by path bytes. This order is the
    /// manifest ordering of the digest and the change identities.
    pub changes: Vec<ScopedChange>,
    /// The digest of every group, path, patch byte, and the ordering
    /// (SPEC 7.2 `manifest_digest`).
    pub manifest_digest: Fingerprint,
    /// The review identity of this pinned comparison (SPEC 7.2, 8.6).
    pub review_id: Fingerprint,
}

/// Builds the canonical change manifest of one scoped B-to-C comparison
/// (SPEC 10.2 steps 1-2 and 7).
///
/// The pipeline is fixed. It resolves the immutable tree entries of both
/// revisions for the complete scope, including additions, deletions, and
/// modes; generates the source patches of exactly the changed paths under
/// the fixed diff contract; parses each complete patch into the canonical
/// change model, keeping every byte; and requires the two views to agree:
/// every changed scoped path appears exactly once, with the presence
/// transition and modes the entries state. Full patch bytes then produce
/// the manifest digest, the review identity, and every change identity
/// through Git's own hashing (SPEC 8.6).
///
/// Both revision arguments must be validated canonical commit object IDs
/// from the adapter, never user strings (SPEC 14.1). The query observes
/// only: it reads trees, runs the diff process, and hashes bytes without
/// storing objects, so refs, the index, and working files stay unchanged.
pub fn build_change_manifest(
    git: &Git,
    branch_ref: &str,
    baseline_oid: &str,
    candidate_oid: &str,
    scope: &ScopeDescriptor,
) -> Result<ChangeManifest, ManifestError> {
    for (which, value) in [("baseline", baseline_oid), ("candidate", candidate_oid)] {
        if !domain::is_full_oid(value) {
            return Err(ManifestError::InvalidRevision {
                which,
                value: value.to_string(),
            });
        }
    }
    let collected = collect_source_changes(git, baseline_oid, candidate_oid, scope)?;

    let identities = Identities::new(git)?;
    let git_version = git.git_version()?;
    let mut groups = Vec::with_capacity(collected.len());
    for change in &collected {
        groups.push(ChangeGroup {
            path: change.entry.path.clone(),
            patch_units: vec![change.patch.bytes.clone()],
        });
    }

    let manifest_digest = identities.manifest(&groups)?;
    let review_id = identities.review(&ReviewContext {
        branch_ref: branch_ref.to_string(),
        baseline_oid: baseline_oid.to_string(),
        candidate_oid: candidate_oid.to_string(),
        scope: scope.clone(),
        engine_version: domain::ENGINE_VERSION.to_string(),
        git_version: git_version.clone(),
        manifest_digest: manifest_digest.oid.clone(),
    })?;
    let change_ids = identities.changes(&review_id, &groups)?;
    let changes = collected
        .into_iter()
        .zip(groups.into_iter().zip(change_ids))
        .map(|(change, (group, change_id))| ScopedChange {
            path: change.entry.path,
            kind: change.entry.kind,
            mode_before: change.entry.entry_before.as_ref().map(|entry| entry.mode),
            mode_after: change.entry.entry_after.as_ref().map(|entry| entry.mode),
            oid_before: change
                .entry
                .entry_before
                .as_ref()
                .map(|entry| entry.blob_oid.clone()),
            oid_after: change
                .entry
                .entry_after
                .as_ref()
                .map(|entry| entry.blob_oid.clone()),
            opaque: change.patch.opaque,
            hunks: change.patch.hunks,
            group,
            change_id,
        })
        .collect();

    Ok(ChangeManifest {
        branch_ref: branch_ref.to_string(),
        baseline_oid: baseline_oid.to_string(),
        candidate_oid: candidate_oid.to_string(),
        scope: scope.clone(),
        engine_version: domain::ENGINE_VERSION.to_string(),
        git_version,
        changes,
        manifest_digest,
        review_id,
    })
}

/// One changed scoped file with its parsed canonical patch: the shared
/// input of the manifest stage and the grouping stage.
struct CollectedChange {
    /// The changed path with the immutable tree entries of both sides.
    entry: ChangedEntry,
    /// The complete parsed patch of the file under the fixed contract.
    patch: domain::SourcePatch,
}

/// Runs steps 1 and 2 of the comparison pipeline (SPEC 10.2) with every
/// cross-check: the immutable scoped tree entries of both revisions, the
/// source patches of exactly the changed paths under the fixed diff
/// contract, and the requirement that the two views agree — one section
/// per changed path, with the presence transition the entries state.
fn collect_source_changes(
    git: &Git,
    baseline_oid: &str,
    candidate_oid: &str,
    scope: &ScopeDescriptor,
) -> Result<Vec<CollectedChange>, ManifestError> {
    // Step 1: the immutable scoped entries of both revisions.
    let before = tree_content_entries(git, baseline_oid, scope)?;
    let after = tree_content_entries(git, candidate_oid, scope)?;
    let changed = merge_changed_entries(&before, &after);

    // Step 2: source patches for exactly the changed paths.
    let paths: Vec<OsString> = changed
        .iter()
        .map(|entry| OsString::from(&entry.path))
        .collect();
    let stream = git.diff_tree_patch(baseline_oid, candidate_oid, &paths)?;
    let sections = domain::split_patch_stream(&stream)?;

    // Association: each section's `diff --git` line must be the expected
    // header of exactly one changed path (SPEC 14.1 literal paths).
    let mut expected: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
    for (index, entry) in changed.iter().enumerate() {
        let header = domain::expected_diff_header(&entry.path);
        if expected.insert(header, index).is_some() {
            return Err(ManifestError::PatchContract {
                detail: "two changed paths share one diff --git header".to_string(),
            });
        }
    }
    let mut parsed: Vec<Option<domain::SourcePatch>> = vec![None; changed.len()];
    for section in sections {
        let first_line = match section.split(|&byte| byte == b'\n').next() {
            Some(line) => line,
            None => return Err(domain::SourcePatchError::MissingHeader.into()),
        };
        let index = expected
            .get(first_line)
            .ok_or_else(|| ManifestError::PatchContract {
                detail: format!(
                    "a patch section names an unexpected path: {:?}",
                    String::from_utf8_lossy(first_line)
                ),
            })?;
        if parsed[*index].is_some() {
            return Err(ManifestError::PatchContract {
                detail: format!(
                    "the patch stream holds more than one section for {:?}",
                    changed[*index].path
                ),
            });
        }
        parsed[*index] = Some(domain::parse_source_patch(section)?);
    }

    let mut collected = Vec::with_capacity(changed.len());
    for (entry, patch) in changed.into_iter().zip(parsed) {
        let patch = patch.ok_or_else(|| ManifestError::PatchContract {
            detail: format!("no patch section covers {:?}", entry.path),
        })?;
        if patch.kind != entry.kind {
            return Err(ManifestError::PatchContract {
                detail: format!(
                    "the patch of {:?} states {} but its tree entries state {}",
                    entry.path,
                    patch.kind.as_str(),
                    entry.kind.as_str()
                ),
            });
        }
        collected.push(CollectedChange { entry, patch });
    }
    Ok(collected)
}

/// One changed scoped path with the immutable tree entries of both sides.
struct ChangedEntry {
    /// Literal repository-relative path.
    path: String,
    /// The presence transition the two tree entries state.
    kind: domain::ChangeKind,
    /// The blob entry of B, when the path exists there.
    entry_before: Option<ContentEntry>,
    /// The blob entry of C, when the path exists there.
    entry_after: Option<ContentEntry>,
}

/// Merges the scoped entries of both revisions into the changed entries,
/// sorted by path bytes. A path joins only when its mode or blob differs
/// between B and C; the union of both sides decides membership, so a
/// deletion stays comparable even though C no longer holds the path
/// (SPEC 3.2 rule 4).
fn merge_changed_entries(before: &[ContentEntry], after: &[ContentEntry]) -> Vec<ChangedEntry> {
    let before_map: BTreeMap<&str, &ContentEntry> = before
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let after_map: BTreeMap<&str, &ContentEntry> = after
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let mut paths: BTreeSet<&str> = before_map.keys().copied().collect();
    paths.extend(after_map.keys().copied());

    paths
        .into_iter()
        .filter_map(|path| {
            let old = before_map.get(path).copied();
            let new = after_map.get(path).copied();
            let equal = match (old, new) {
                (Some(old), Some(new)) => old.mode == new.mode && old.blob_oid == new.blob_oid,
                _ => false,
            };
            (!equal).then(|| ChangedEntry {
                path: path.to_string(),
                kind: match (old, new) {
                    (None, Some(_)) => domain::ChangeKind::Added,
                    (Some(_), None) => domain::ChangeKind::Deleted,
                    _ => domain::ChangeKind::Modified,
                },
                entry_before: old.cloned(),
                entry_after: new.cloned(),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Safe change groups (SPEC 10.2-10.5)
// ---------------------------------------------------------------------------

/// One final safe change group of a review: a passage of complete hunks,
/// the reason it is one decision, its presentation, its canonical patch
/// unit, and the change identity issued only after the grouping proved
/// exact (SPEC 8.6 and 10.2 step 7).
#[derive(Clone, Debug)]
pub struct PassageChange {
    /// Literal repository-relative path, valid UTF-8.
    pub path: String,
    /// Why these hunks form one decision (SPEC 10.3).
    pub reason: String,
    /// The complete canonical hunks of this group, zero-based indexes into
    /// the file's hunks in patch order.
    pub hunk_indexes: Vec<usize>,
    /// The bounded presentation of the passage (SPEC 7.2, 10.4).
    pub presentation: markdown::GroupPresentation,
    /// The canonical change-group input: the literal path and the group's
    /// complete original patch as the one patch unit (SPEC 8.6).
    pub group: ChangeGroup,
    /// The change identity of this group within this review.
    pub change_id: Fingerprint,
}

/// One changed scoped file with its safe groups: either independent
/// passages, or the whole file as one decision with the fallback reason
/// (SPEC 10.3).
#[derive(Clone, Debug)]
pub struct GroupedFile {
    /// Literal repository-relative path, valid UTF-8.
    pub path: String,
    /// The presence transition the immutable tree entries state.
    pub kind: domain::ChangeKind,
    /// Storage mode in B; `None` for an addition.
    pub mode_before: Option<EntryMode>,
    /// Storage mode in C; `None` for a deletion.
    pub mode_after: Option<EntryMode>,
    /// Full lowercase hex blob object ID in B; `None` for an addition.
    pub oid_before: Option<String>,
    /// Full lowercase hex blob object ID in C; `None` for a deletion.
    pub oid_after: Option<String>,
    /// Whether the content change is opaque (SPEC 10.4).
    pub opaque: bool,
    /// The file's complete canonical hunks with their source ranges.
    pub hunks: Vec<domain::SourceHunk>,
    /// Why the whole file is one decision; `None` when the file's groups
    /// are independent passages.
    pub whole_file_reason: Option<String>,
    /// The safe groups of the file, in source order. A whole-file decision
    /// holds exactly one group.
    pub groups: Vec<PassageChange>,
}

/// The complete grouped manifest of one pinned B-to-C comparison (SPEC
/// 10.2 step 7): every safe group of every scoped change, and the
/// identities issued only after grouping and exact-application validation.
#[derive(Clone, Debug)]
pub struct GroupedManifest {
    /// The full adopted branch ref the comparison ran on.
    pub branch_ref: String,
    /// The baseline commit B.
    pub baseline_oid: String,
    /// The candidate commit C.
    pub candidate_oid: String,
    /// The canonical scope descriptor.
    pub scope: ScopeDescriptor,
    /// The engine contract version; see [`domain::ENGINE_VERSION`].
    pub engine_version: String,
    /// The effective Git version that contributed to the comparison.
    pub git_version: String,
    /// Every changed scoped file, sorted by path bytes.
    pub files: Vec<GroupedFile>,
    /// The digest of every group, path, patch byte, and the ordering.
    pub manifest_digest: Fingerprint,
    /// The review identity of this pinned comparison (SPEC 7.2, 8.6).
    pub review_id: Fingerprint,
}

impl GroupedManifest {
    /// Every safe group in manifest order: files by path bytes, then each
    /// file's groups in source order. This is the order the digest and the
    /// change identities cover.
    pub fn groups(&self) -> impl Iterator<Item = &PassageChange> {
        self.files.iter().flat_map(|file| file.groups.iter())
    }
}

/// One group while a manifest is being assembled: everything a passage
/// carries except the identity, which no group holds before the whole
/// grouping proved exact (SPEC 10.2 steps 6-7).
struct GroupDraft {
    /// Why these hunks form one decision.
    reason: String,
    /// The complete hunks of the group, zero-based, ascending.
    hunk_indexes: Vec<usize>,
    /// The bounded presentation.
    presentation: markdown::GroupPresentation,
    /// The canonical patch unit of the group.
    unit: Vec<u8>,
}

/// Builds the complete grouped manifest of one scoped B-to-C comparison
/// (SPEC 10.2): the pipeline of [`build_change_manifest`] followed by the
/// grouping stage.
///
/// For every changed file the readable Markdown of both versions is parsed
/// in the presentation layer, by source offset and without rewriting
/// source; complete canonical hunks form conservative passages; coupled
/// structures and moved passages are reviewed together; and every case
/// whose independence cannot be established from both versions — presence
/// transitions, opaque bytes, mode transitions, reference-link and
/// footnote relations, unparseable raw HTML — enlarges to a whole-file
/// decision.
///
/// Each file's groups are then proven exact through one disposable Git
/// index seeded from C: every group must reverse-apply alone and
/// forward-apply back to C, and all groups together must recompose B's
/// entry exactly. A grouping that cannot be shown safe enlarges to the
/// whole file before any identity is issued, so groups never change after
/// actionable IDs exist (SPEC 10.2 steps 6-7).
///
/// The query observes only: it reads trees, blobs, and the diff, and its
/// disposable index never aliases the real index, so refs, the index, and
/// working files stay unchanged.
pub fn build_grouped_manifest(
    git: &Git,
    branch_ref: &str,
    baseline_oid: &str,
    candidate_oid: &str,
    scope: &ScopeDescriptor,
) -> Result<GroupedManifest, ManifestError> {
    for (which, value) in [("baseline", baseline_oid), ("candidate", candidate_oid)] {
        if !domain::is_full_oid(value) {
            return Err(ManifestError::InvalidRevision {
                which,
                value: value.to_string(),
            });
        }
    }
    let collected = collect_source_changes(git, baseline_oid, candidate_oid, scope)?;
    let identities = Identities::new(git)?;
    let git_version = git.git_version()?;

    let drafts = group_all_files(git, collected)?;

    // Identities arrive only after grouping and validation, from the
    // grouped patch units in manifest order (SPEC 10.2 steps 6-7).
    let ordered: Vec<ChangeGroup> = drafts
        .iter()
        .flat_map(|draft| {
            draft.groups.iter().map(|group| ChangeGroup {
                path: draft.path.clone(),
                patch_units: vec![group.unit.clone()],
            })
        })
        .collect();
    let manifest_digest = identities.manifest(&ordered)?;
    let review_id = identities.review(&ReviewContext {
        branch_ref: branch_ref.to_string(),
        baseline_oid: baseline_oid.to_string(),
        candidate_oid: candidate_oid.to_string(),
        scope: scope.clone(),
        engine_version: domain::ENGINE_VERSION.to_string(),
        git_version: git_version.clone(),
        manifest_digest: manifest_digest.oid.clone(),
    })?;
    // One batched pass derives every change identity in manifest order
    // (SPEC 8.6, 17.5).
    let change_ids = identities.changes(&review_id, &ordered)?;
    let mut change_identity = change_ids.into_iter();
    let mut files = Vec::with_capacity(drafts.len());
    for draft in drafts {
        let mut groups = Vec::with_capacity(draft.groups.len());
        for group in draft.groups {
            let group_input = ChangeGroup {
                path: draft.path.clone(),
                patch_units: vec![group.unit],
            };
            groups.push(PassageChange {
                path: draft.path.clone(),
                reason: group.reason,
                hunk_indexes: group.hunk_indexes,
                presentation: group.presentation,
                change_id: change_identity
                    .next()
                    .expect("one identity per grouped passage"),
                group: group_input,
            });
        }
        files.push(GroupedFile {
            path: draft.path,
            kind: draft.kind,
            mode_before: draft.mode_before,
            mode_after: draft.mode_after,
            oid_before: draft.oid_before,
            oid_after: draft.oid_after,
            opaque: draft.opaque,
            hunks: draft.hunks,
            whole_file_reason: draft.whole_file_reason,
            groups,
        });
    }

    Ok(GroupedManifest {
        branch_ref: branch_ref.to_string(),
        baseline_oid: baseline_oid.to_string(),
        candidate_oid: candidate_oid.to_string(),
        scope: scope.clone(),
        engine_version: domain::ENGINE_VERSION.to_string(),
        git_version,
        files,
        manifest_digest,
        review_id,
    })
}

/// One changed file while its groups are being assembled.
struct FileDraft {
    /// Literal repository-relative path.
    path: String,
    /// The presence transition the immutable tree entries state.
    kind: domain::ChangeKind,
    /// Storage mode in B; `None` for an addition.
    mode_before: Option<EntryMode>,
    /// Storage mode in C; `None` for a deletion.
    mode_after: Option<EntryMode>,
    /// Full lowercase hex blob object ID in B; `None` for an addition.
    oid_before: Option<String>,
    /// Full lowercase hex blob object ID in C; `None` for a deletion.
    oid_after: Option<String>,
    /// Whether the content change is opaque (SPEC 10.4).
    opaque: bool,
    /// The file's complete canonical hunks with their source ranges.
    hunks: Vec<domain::SourceHunk>,
    /// Why the whole file is one decision; `None` for independent
    /// passages.
    whole_file_reason: Option<String>,
    /// The assembled groups.
    groups: Vec<GroupDraft>,
}

/// Groups every collected change, one file at a time.
fn group_all_files(
    git: &Git,
    collected: Vec<CollectedChange>,
) -> Result<Vec<FileDraft>, ManifestError> {
    // Every changed document's both sides are read through one batched
    // pass before the grouping starts, so the source read of a
    // whole-scope comparison never scales one process per document
    // (SPEC 17.5).
    let mut wanted: Vec<String> = Vec::new();
    for change in &collected {
        for side in [&change.entry.entry_before, &change.entry.entry_after] {
            if let Some(side) = side
                && !wanted.contains(&side.blob_oid)
            {
                wanted.push(side.blob_oid.clone());
            }
        }
    }

    let blobs = git.read_blobs(&wanted)?;

    // The files are independent: one file's grouping reads its own blobs
    // and proves its own passages on its own disposable indexes. Workers
    // claim whole files by order and record the outcome at the file's own
    // slot, so the assembled result and the first failure are exactly the
    // sequential ones, sooner (SPEC 17.5: bounded work, not bounded
    // correctness).
    let total = collected.len();
    let queue: std::sync::Mutex<Vec<Option<CollectedChange>>> =
        std::sync::Mutex::new(collected.into_iter().map(Some).collect());
    let slots: Vec<std::sync::Mutex<Option<Result<FileDraft, ManifestError>>>> =
        (0..total).map(|_| std::sync::Mutex::new(None)).collect();
    let cursor = AtomicU64::new(0);
    let workers = std::thread::available_parallelism()
        // Three workers per core: one proof process spends most of its
        // wall time inflating blobs and writing indexes beside the CPU
        // work, so the pool overlaps that waiting (SPEC 17.5).
        .map(|cores| cores.get().saturating_mul(3))
        .unwrap_or(2)
        .min(total)
        .max(1);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                // One worker's whole claim, in three passes: group every
                // claimed file, prove the light files' passages through one
                // shared chunk index while the group-heavy files keep their
                // own nested rounds, then assemble and record in claim
                // order (SPEC 17.5: the process count of a whole-scope
                // comparison scales with the rounds, not the groups).
                let mut claimed: Vec<(usize, CollectedChange)> = Vec::new();
                while let Some(pair) = claim_next_file(&queue, &cursor) {
                    claimed.push(pair);
                }
                let mut prepared: Vec<(usize, FileDraft, FilePlan<'_>)> = Vec::new();
                let mut failed = false;
                for (index, change) in &claimed {
                    if failed {
                        continue;
                    }
                    match group_one_file_prepare(&blobs, change) {
                        Ok((draft, plan)) => prepared.push((*index, draft, plan)),
                        Err(error) => {
                            *slots[*index].lock().expect("the outcome slot is free") =
                                Some(Err(error));
                            failed = true;
                        }
                    }
                }

                // The light files of this claim share one chunk proof; a
                // file with more groups than the nested-round threshold
                // keeps its own indexes, because its rounds parallelize
                // better than they batch.
                let mut chunk: Vec<ChunkFile<'_>> = Vec::new();
                let mut chunk_slots: Vec<usize> = Vec::new();
                for (position, (_index, _draft, plan)) in prepared.iter().enumerate() {
                    if let FilePlan::Groups {
                        candidates,
                        after_entry,
                        before_entry,
                        ..
                    } = plan
                        && candidates.len() <= PARALLEL_GROUP_ROUNDS
                        && let Some(after_entry) = after_entry
                    {
                        chunk.push(ChunkFile {
                            path: _draft.path.clone(),
                            after_entry: after_entry.clone(),
                            before_shape: staged_shape(before_entry),
                            groups: candidates,
                        });
                        chunk_slots.push(position);
                    }
                }
                let mut group_outcomes: Vec<Option<bool>> = vec![None; prepared.len()];
                if !chunk.is_empty() {
                    match prove_chunk(git, &chunk) {
                        Ok(outcomes) => {
                            for (at, outcome) in chunk_slots.into_iter().zip(outcomes) {
                                group_outcomes[at] = Some(outcome);
                            }
                        }
                        Err(error) => {
                            // The chunk's plumbing failed before any
                            // verdict: the first light file of the claim
                            // carries the failure — the file the
                            // sequential proof would have stopped at —
                            // and everything this worker still held
                            // records nothing.
                            let failed_slot = chunk_slots
                                .first()
                                .map(|&at| prepared[at].0)
                                .or_else(|| prepared.first().map(|(index, _, _)| *index));
                            if let Some(index) = failed_slot {
                                *slots[index].lock().expect("the outcome slot is free") =
                                    Some(Err(error));
                            }
                            return;
                        }
                    }
                }

                for (position, (index, draft, plan)) in prepared.into_iter().enumerate() {
                    let path = draft.path.clone();
                    let proved = match &plan {
                        FilePlan::Groups {
                            candidates,
                            after_entry,
                            ..
                        } if group_outcomes[position].is_none() => {
                            // The group-heavy file proves itself with its
                            // own nested rounds; an infrastructure failure
                            // stops this worker exactly as the sequential
                            // proof would.
                            match passages_apply_exactly(
                                git,
                                &path,
                                after_entry,
                                &plan_before_entry(&plan),
                                candidates,
                            ) {
                                Ok(outcome) => Some(outcome),
                                Err(error) => {
                                    *slots[index].lock().expect("the outcome slot is free") =
                                        Some(Err(error));
                                    return;
                                }
                            }
                        }
                        _ => group_outcomes[position],
                    };
                    let outcome = group_one_file_assemble(git, &path, draft, plan, proved);
                    let outcome = match outcome {
                        Ok(draft) => Ok(draft),
                        Err(error) => {
                            *slots[index].lock().expect("the outcome slot is free") =
                                Some(Err(error));
                            return;
                        }
                    };
                    *slots[index].lock().expect("the outcome slot is free") = Some(outcome);
                }
            });
        }
    });
    let mut drafts = Vec::with_capacity(total);
    for slot in slots {
        match slot.into_inner().expect("the workers finished") {
            Some(Ok(draft)) => drafts.push(draft),
            Some(Err(error)) => return Err(error),
            None => {
                // No worker can leave a slot empty: each claimed file is
                // answered before the scope ends.
                unreachable!("every claimed file records an outcome")
            }
        }
    }
    Ok(drafts)
}

/// The baseline entry a plan carries, for the whole-file proofs.
fn plan_before_entry(plan: &FilePlan<'_>) -> Option<(EntryMode, String)> {
    match plan {
        FilePlan::WholeFile { before_entry, .. } | FilePlan::Groups { before_entry, .. } => {
            before_entry.clone()
        }
    }
}

/// Claims the next unclaimed file with its slot index, in collected order.
fn claim_next_file(
    queue: &std::sync::Mutex<Vec<Option<CollectedChange>>>,
    cursor: &AtomicU64,
) -> Option<(usize, CollectedChange)> {
    let index = cursor.fetch_add(1, Ordering::Relaxed) as usize;
    let mut pending = queue.lock().expect("the queue is short-lived");
    pending
        .get_mut(index)
        .and_then(|slot| slot.take())
        .map(|change| (index, change))
}

/// What one changed file still owes after its grouping ran: the proof of
/// its whole-file unit, or the proof of its passage candidates with the
/// material a fallback whole-file decision needs when they fail.
enum FilePlan<'a> {
    /// The file is one whole-file decision whatever grouped it; the unit
    /// must still be proven to restore B exactly.
    WholeFile {
        /// The candidate entry of the path.
        after_entry: Option<(EntryMode, String)>,
        /// The baseline entry of the path.
        before_entry: Option<(EntryMode, String)>,
        /// The complete patch bytes of the file.
        unit: &'a [u8],
    },
    /// Passage candidates to prove exact; on failure the file falls back
    /// to one whole-file decision built from the same patch.
    Groups {
        /// The candidate entry of the path.
        after_entry: Option<(EntryMode, String)>,
        /// The baseline entry of the path.
        before_entry: Option<(EntryMode, String)>,
        /// The grouped passage candidates (SPEC 10.3).
        candidates: Vec<GroupDraft>,
        /// The material of the fallback whole-file decision.
        fallback: WholeFileFallback<'a>,
    },
}

/// The material a failed passage grouping needs to enlarge into one
/// whole-file decision (SPEC 10.3).
struct WholeFileFallback<'a> {
    /// The complete parsed patch of the file.
    patch: &'a domain::SourcePatch,
    /// The presentation hunks of the patch.
    hunks: Vec<markdown::HunkLines>,
    /// The baseline side's text.
    before: &'a str,
    /// The candidate side's text.
    after: &'a str,
    /// The baseline side's structure.
    before_structure: markdown::DocumentStructure,
    /// The candidate side's structure.
    after_structure: markdown::DocumentStructure,
}

/// Groups one changed file (SPEC 10.2-10.3) without touching Git: the
/// grouping reads only the batch-read blobs and the parsed patch, so a
/// worker can group every file it claimed before any proof process runs.
fn group_one_file_prepare<'a>(
    blobs: &'a BTreeMap<String, Vec<u8>>,
    change: &'a CollectedChange,
) -> Result<(FileDraft, FilePlan<'a>), ManifestError> {
    let CollectedChange { entry, patch } = change;
    let path = entry.path.clone();
    let before_entry = entry
        .entry_before
        .as_ref()
        .map(|before| (before.mode, before.blob_oid.clone()));
    let after_entry = entry
        .entry_after
        .as_ref()
        .map(|after| (after.mode, after.blob_oid.clone()));

    let mut draft = FileDraft {
        path: path.clone(),
        kind: entry.kind,
        mode_before: entry.entry_before.as_ref().map(|entry| entry.mode),
        mode_after: entry.entry_after.as_ref().map(|entry| entry.mode),
        oid_before: entry
            .entry_before
            .as_ref()
            .map(|entry| entry.blob_oid.clone()),
        oid_after: entry
            .entry_after
            .as_ref()
            .map(|entry| entry.blob_oid.clone()),
        opaque: patch.opaque,
        hunks: patch.hunks.clone(),
        whole_file_reason: None,
        groups: Vec::new(),
    };

    // The whole-file decisions whose reason the file's own transition
    // states (SPEC 10.3).
    let forced = if entry.kind == domain::ChangeKind::Added {
        Some("file creation: an added file is one whole-file decision")
    } else if entry.kind == domain::ChangeKind::Deleted {
        Some("file deletion: a removed file is one whole-file decision")
    } else if patch.opaque {
        Some("opaque binary change: Git emits no readable hunks for this file")
    } else if draft.mode_before != draft.mode_after {
        Some(concat!(
            "file-mode change: the mode transition is part of the ",
            "whole-file decision while the content stays with it"
        ))
    } else {
        None
    };

    // Both sides' exact bytes: the source the grouping reads (SPEC 10.2
    // step 3), from the one batch that read every changed document.
    let before_bytes = match &entry.entry_before {
        Some(before) => Some(blobs.get(&before.blob_oid).ok_or_else(|| {
            ManifestError::PatchContract {
                detail: format!("the before blob of {:?} was not batch-read", path),
            }
        })?),
        None => None,
    };
    let after_bytes = match &entry.entry_after {
        Some(after) => Some(blobs.get(&after.blob_oid).ok_or_else(|| {
            ManifestError::PatchContract {
                detail: format!("the after blob of {:?} was not batch-read", path),
            }
        })?),
        None => None,
    };
    let before_text: Option<&str> = match before_bytes {
        Some(bytes) => std::str::from_utf8(bytes).ok(),
        None => None,
    };
    let after_text: Option<&str> = match after_bytes {
        Some(bytes) => std::str::from_utf8(bytes).ok(),
        None => None,
    };

    // A present side that is not readable UTF-8 makes the content opaque:
    // exact bytes, no alignment (SPEC 10.4).
    let unreadable = (entry.entry_before.is_some() && before_text.is_none())
        || (entry.entry_after.is_some() && after_text.is_none());

    if forced.is_some() || unreadable {
        let reason = match forced {
            Some(reason) => reason,
            None => concat!(
                "opaque content: one side holds bytes that are not valid UTF-8, ",
                "so the whole file is one decision"
            ),
        };
        let scope = match entry.kind {
            domain::ChangeKind::Added => markdown::PresentationScope::AddedFile,
            domain::ChangeKind::Deleted => markdown::PresentationScope::DeletedFile,
            _ => markdown::PresentationScope::WholeFile,
        };
        // An added or deleted file previews its present side; unreadable
        // content falls back to the exact opaque bytes.
        let presentation = if unreadable {
            markdown::opaque_presentation(
                scope,
                before_bytes.as_deref().map(Vec::as_slice),
                after_bytes.as_deref().map(Vec::as_slice),
            )
        } else {
            let before = before_text.unwrap_or_default();
            let after = after_text.unwrap_or_default();
            let hunks = markdown::hunk_lines(&patch.bytes, &patch.hunks);
            let indexes: Vec<usize> = (0..hunks.len()).collect();
            markdown::group_presentation(
                scope,
                before,
                after,
                &markdown::analyze(before),
                &markdown::analyze(after),
                &hunks,
                &indexes,
            )
        };
        draft.whole_file_reason = Some(reason.to_string());
        draft.groups = vec![GroupDraft {
            reason: reason.to_string(),
            hunk_indexes: (0..patch.hunks.len()).collect(),
            presentation,
            unit: patch.bytes.clone(),
        }];
        return Ok((
            draft,
            FilePlan::WholeFile {
                after_entry,
                before_entry,
                unit: patch.bytes.as_slice(),
            },
        ));
    }

    // Readable text on both sides: group the complete hunks (SPEC 10.3).
    let before = before_text.unwrap_or_default();
    let after = after_text.unwrap_or_default();
    let hunks = markdown::hunk_lines(&patch.bytes, &patch.hunks);
    let before_structure = markdown::analyze(before);
    let after_structure = markdown::analyze(after);
    let proposal = markdown::group_changes(before, after, &hunks);

    match proposal {
        markdown::FileGrouping::WholeFile { reason } => {
            draft.whole_file_reason = Some(reason.clone());
            draft.groups = vec![whole_file_passage(
                patch,
                &hunks,
                &reason,
                before,
                after,
                &before_structure,
                &after_structure,
            )];
            Ok((
                draft,
                FilePlan::WholeFile {
                    after_entry,
                    before_entry,
                    unit: patch.bytes.as_slice(),
                },
            ))
        }
        markdown::FileGrouping::Groups { groups } => {
            let header_end = patch
                .hunks
                .first()
                .map(|hunk| hunk.offset)
                .unwrap_or(patch.bytes.len());
            let candidates: Vec<GroupDraft> = groups
                .iter()
                .map(|group| {
                    let mut unit = patch.bytes[..header_end].to_vec();
                    for &index in &group.hunks {
                        let hunk = &patch.hunks[index];
                        unit.extend_from_slice(
                            &patch.bytes[hunk.offset..hunk.offset + hunk.length],
                        );
                    }
                    GroupDraft {
                        reason: group.reason.clone(),
                        hunk_indexes: group.hunks.clone(),
                        presentation: markdown::group_presentation(
                            markdown::PresentationScope::Passage,
                            before,
                            after,
                            &before_structure,
                            &after_structure,
                            &hunks,
                            &group.hunks,
                        ),
                        unit,
                    }
                })
                .collect();
            Ok((
                draft,
                FilePlan::Groups {
                    after_entry,
                    before_entry,
                    candidates,
                    fallback: WholeFileFallback {
                        patch,
                        hunks,
                        before,
                        after,
                        before_structure,
                        after_structure,
                    },
                },
            ))
        }
    }
}

/// Finishes one grouped file once its proof outcome is known: the
/// whole-file plan proves its unit, and the groups plan issues the
/// candidates that proved exact or enlarges to the whole file (SPEC 10.2
/// step 6, 10.3).
fn group_one_file_assemble(
    git: &Git,
    path: &str,
    draft: FileDraft,
    plan: FilePlan<'_>,
    proved: Option<bool>,
) -> Result<FileDraft, ManifestError> {
    match plan {
        FilePlan::WholeFile {
            after_entry,
            before_entry,
            unit,
        } => {
            whole_file_applies_exactly(git, path, &after_entry, &before_entry, unit)?;
            Ok(draft)
        }
        FilePlan::Groups {
            after_entry,
            before_entry,
            candidates,
            fallback,
        } => {
            if proved == Some(true) {
                let mut draft = draft;
                draft.groups = candidates;
                return Ok(draft);
            }
            let WholeFileFallback {
                patch,
                hunks,
                before,
                after,
                before_structure,
                after_structure,
            } = fallback;
            let reason = concat!(
                "exact-application fallback: the passages could not be ",
                "shown safe, so the whole file is one decision"
            );
            whole_file_applies_exactly(
                git,
                path,
                &after_entry,
                &before_entry,
                patch.bytes.as_slice(),
            )?;
            let mut draft = draft;
            draft.whole_file_reason = Some(reason.to_string());
            draft.groups = vec![whole_file_passage(
                patch,
                &hunks,
                reason,
                before,
                after,
                &before_structure,
                &after_structure,
            )];
            Ok(draft)
        }
    }
}

/// One whole-file group draft from the complete patch.
fn whole_file_passage(
    patch: &domain::SourcePatch,
    hunks: &[markdown::HunkLines],
    reason: &str,
    before: &str,
    after: &str,
    before_structure: &markdown::DocumentStructure,
    after_structure: &markdown::DocumentStructure,
) -> GroupDraft {
    let indexes: Vec<usize> = (0..hunks.len()).collect();
    GroupDraft {
        reason: reason.to_string(),
        hunk_indexes: indexes.clone(),
        presentation: markdown::group_presentation(
            markdown::PresentationScope::WholeFile,
            before,
            after,
            before_structure,
            after_structure,
            hunks,
            &indexes,
        ),
        unit: patch.bytes.clone(),
    }
}

/// Says whether one file's passage patches apply exactly through the
/// disposable index: each group alone must reverse-apply onto C and
/// forward-apply back to C, and all groups together, applied latest
/// position first, must recompose B's entry byte for byte (SPEC 10.2
/// step 6). A `false` result means the grouping cannot be shown safe, not
/// that Git failed.
fn passages_apply_exactly(
    git: &Git,
    path: &str,
    after_entry: &Option<(EntryMode, String)>,
    before_entry: &Option<(EntryMode, String)>,
    groups: &[GroupDraft],
) -> Result<bool, ManifestError> {
    let after = staged_shape(after_entry);
    let before = staged_shape(before_entry);
    // Each group's round trip runs on its own disposable index seeded with
    // the candidate's own entry of this path, because every patch it
    // applies touches this one path (SPEC 10.2 step 6). The tasks share
    // nothing, so the rounds of one large file run beside every other
    // file's work instead of after it (SPEC 17.5).
    let rounds = prove_group_rounds(git, path, after_entry, groups, &after)?;
    if !rounds.iter().all(|round| *round) {
        return Ok(false);
    }
    // The composition: latest position first, so each patch's recorded
    // line numbers stay valid. One process applies the whole series.
    let temp_index = reserve_temp_index().map_err(ManifestError::Io)?;
    let mut probe = git.clone();
    probe.set_env_override("GIT_INDEX_FILE", &temp_index);
    let outcome = (|| {
        seed_probe_entry(&probe, &temp_index, path, after_entry)?;
        let units: Vec<&[u8]> = groups
            .iter()
            .rev()
            .map(|group| group.unit.as_slice())
            .collect();
        if probe.apply_patch_documents_cached(&units, true).is_err() {
            return Ok(false);
        }
        Ok(staged_entry(&probe, &temp_index, path)? == before)
    })();
    // The disposable index never outlives the proof.
    let _ = fs::remove_file(&temp_index);
    outcome
}

/// Below this many groups, one thread proves the file's groups in order;
/// above it, the groups share threads of their own, because one file can
/// hold most of a comparison's changes (SPEC 17.5).
const PARALLEL_GROUP_ROUNDS: usize = 6;

/// Probes the disposable-index plumbing once per proof context
/// (SPEC 16.6 item 2): the seed itself is written in process, so the
/// first Git call through the disposable index would otherwise be an
/// apply, whose failure can also mean plain nonapplicability. Reading
/// the just-written entry back through Git fails typed when the layer is
/// broken, and a readback that disagrees with the written entry is a
/// writer bug, never a verdict about the patches.
fn validate_probe_plumbing(
    git: &Git,
    path: &str,
    after_entry: &Option<(EntryMode, String)>,
) -> Result<(), ManifestError> {
    let temp_index = reserve_temp_index().map_err(ManifestError::Io)?;
    let mut probe = git.clone();
    probe.set_env_override("GIT_INDEX_FILE", &temp_index);
    let outcome = (|| {
        seed_probe_entry(&probe, &temp_index, path, after_entry)?;
        let read = probe.staged_entry_of(&OsString::from(path))?;
        let expected = after_entry
            .as_ref()
            .map(|(mode, oid)| (mode.as_str().to_string(), oid.clone()));
        if read != expected {
            return Err(ManifestError::Git(GitError::MalformedOutput {
                command: "ls-files --stage".to_string(),
                detail: format!(
                    "the written probe index of {path:?} read back as {read:?}, expected {expected:?}"
                ),
            }));
        }
        Ok(())
    })();
    let _ = fs::remove_file(&temp_index);
    outcome
}

/// Proves every group's round trip, each on its own disposable index.
fn prove_group_rounds(
    git: &Git,
    path: &str,
    after_entry: &Option<(EntryMode, String)>,
    groups: &[GroupDraft],
    after: &Option<(String, String)>,
) -> Result<Vec<bool>, ManifestError> {
    validate_probe_plumbing(git, path, after_entry)?;
    let one_round = |group: &GroupDraft| -> Result<bool, ManifestError> {
        let temp_index = reserve_temp_index().map_err(ManifestError::Io)?;
        let mut probe = git.clone();
        probe.set_env_override("GIT_INDEX_FILE", &temp_index);
        let outcome = (|| {
            seed_probe_entry(&probe, &temp_index, path, after_entry)?;
            if probe.apply_patch_cached(&group.unit, true).is_err() {
                return Ok(false);
            }
            if staged_entry(&probe, &temp_index, path)? == *after {
                // A group that changes nothing cannot be a safe decision
                // unit.
                return Ok(false);
            }
            if probe.apply_patch_cached(&group.unit, false).is_err() {
                return Ok(false);
            }
            Ok(staged_entry(&probe, &temp_index, path)? == *after)
        })();
        let _ = fs::remove_file(&temp_index);
        outcome
    };
    if groups.len() <= PARALLEL_GROUP_ROUNDS {
        return groups.iter().map(one_round).collect();
    }
    // Enough groups share one file that proving them in order would decide
    // the whole comparison's wait: the rounds run beside each other, and
    // every answer comes back in group order whatever it is.
    let threads = groups.len().min(8);
    let chunk = groups.len().div_ceil(threads);
    let answers: Vec<std::sync::Mutex<Option<Result<bool, ManifestError>>>> = groups
        .iter()
        .map(|_| std::sync::Mutex::new(None))
        .collect();
    std::thread::scope(|scope| {
        for (worker, chunk_of_groups) in groups.chunks(chunk).enumerate() {
            let answers = &answers;
            scope.spawn(move || {
                let base = worker * chunk;
                for (offset, group) in chunk_of_groups.iter().enumerate() {
                    *answers[base + offset].lock().expect("the answer slot is free") =
                        Some(one_round(group));
                }
            });
        }
    });
    answers
        .into_iter()
        .map(|answer| answer.into_inner().expect("the workers finished").expect("every group answered"))
        .collect()
}

/// One file of a batched proof chunk: the parts of a light file's plan
/// the chunk runner drives.
struct ChunkFile<'a> {
    /// The repository-relative path.
    path: String,
    /// The candidate entry of the path; a light file always holds one,
    /// because an added or deleted file is a whole-file decision.
    after_entry: (EntryMode, String),
    /// The baseline entry as the index reads it back.
    before_shape: Option<(String, String)>,
    /// The passage candidates to prove.
    groups: &'a [GroupDraft],
}

/// Proves the passage candidates of several light files through one
/// shared disposable index (SPEC 10.2 step 6, 17.5).
///
/// Patches of different files touch disjoint index entries, so one Git
/// process carries one round step — every file's reverse application,
/// then every file's forward application — and the per-file verdicts
/// read back in process. The groups of one file still run in order,
/// because group g+1's reverse must meet the candidate entry, not the
/// entry group g reversed. A batch process that fails retries its
/// members alone on the shared index, so one nonapplicable group never
/// condemns the other files of its step, and a file that leaves the
/// verified candidate state is excluded from later steps with its
/// verdict already false. The compositions of the fully proven files
/// batch the same way, each file's units latest position first.
fn prove_chunk(git: &Git, files: &[ChunkFile<'_>]) -> Result<Vec<bool>, ManifestError> {
    let mut verdicts = vec![true; files.len()];
    if files.is_empty() {
        return Ok(verdicts);
    }
    let temp_index = reserve_temp_index().map_err(ManifestError::Io)?;
    let mut probe = git.clone();
    probe.set_env_override("GIT_INDEX_FILE", &temp_index);
    let width = match probe.object_format().map_err(ManifestError::Git)? {
        ObjectFormat::Sha1 => 20,
        ObjectFormat::Sha256 => 32,
    };
    let outcome = (|| {
        // One index seeded with every file's candidate entry, in the
        // path order Git requires.
        let entries: Vec<(&str, &str, &str)> = files
            .iter()
            .map(|file| {
                (
                    file.after_entry.0.as_str(),
                    file.after_entry.1.as_str(),
                    file.path.as_str(),
                )
            })
            .collect();
        write_probe_index_entries(&temp_index, width, &entries).map_err(ManifestError::Io)?;
        // The plumbing probe: the whole seeded index reads back through
        // one Git listing, so a broken disposable layer fails the chunk
        // typed and a disagreement names the writer (SPEC 16.6 item 2).
        let listed = probe.ls_files_stage()?;
        for file in files {
            let expected = (file.after_entry.0.as_str().to_string(), file.after_entry.1.clone());
            let read = listed
                .iter()
                .find(|entry| {
                    entry.stage == 0 && entry.path.as_encoded_bytes() == file.path.as_bytes()
                })
                .map(|entry| (entry.mode.clone(), entry.oid.clone()));
            if read.as_ref() != Some(&expected) {
                return Err(ManifestError::Git(GitError::MalformedOutput {
                    command: "ls-files --stage".to_string(),
                    detail: format!(
                        "the written probe index of {:?} read back as {read:?}, expected {expected:?}",
                        file.path
                    ),
                }));
            }
        }

        // The rounds: step r proves the r-th group of every file that
        // still holds one and still runs.
        let after_shapes: Vec<Option<(String, String)>> = files
            .iter()
            .map(|file| staged_shape(&Some(file.after_entry.clone())))
            .collect();
        let mut running = vec![true; files.len()];
        let deepest = files.iter().map(|file| file.groups.len()).max().unwrap_or(0);
        for round in 0..deepest {
            let members: Vec<usize> = (0..files.len())
                .filter(|&at| running[at] && round < files[at].groups.len())
                .collect();
            if members.is_empty() {
                continue;
            }
            let reverses: Vec<&[u8]> = members
                .iter()
                .map(|&at| files[at].groups[round].unit.as_slice())
                .collect();
            if probe.apply_patch_documents_cached(&reverses, true).is_err() {
                // Git applied nothing: each member retries its whole
                // round alone, the per-group semantics.
                for &at in &members {
                    let group = &files[at].groups[round];
                    let ok = probe.apply_patch_cached(&group.unit, true).is_ok()
                        && staged_entry(&probe, &temp_index, &files[at].path)? != after_shapes[at]
                        && probe.apply_patch_cached(&group.unit, false).is_ok()
                        && staged_entry(&probe, &temp_index, &files[at].path)? == after_shapes[at];
                    if !ok {
                        verdicts[at] = false;
                        running[at] = false;
                    }
                }
                continue;
            }
            for &at in &members {
                if staged_entry(&probe, &temp_index, &files[at].path)? == after_shapes[at] {
                    // A group that changes nothing cannot be a safe
                    // decision unit; the entry it left is still the
                    // candidate's, so the file simply stops here.
                    verdicts[at] = false;
                    running[at] = false;
                }
            }
            let forwards: Vec<usize> = members
                .into_iter()
                .filter(|&at| running[at])
                .collect();
            if forwards.is_empty() {
                continue;
            }
            let units: Vec<&[u8]> = forwards
                .iter()
                .map(|&at| files[at].groups[round].unit.as_slice())
                .collect();
            if probe.apply_patch_documents_cached(&units, false).is_err() {
                for &at in &forwards {
                    let group = &files[at].groups[round];
                    let ok = probe.apply_patch_cached(&group.unit, false).is_ok()
                        && staged_entry(&probe, &temp_index, &files[at].path)? == after_shapes[at];
                    if !ok {
                        verdicts[at] = false;
                        running[at] = false;
                    }
                }
                continue;
            }
            for &at in &forwards {
                if staged_entry(&probe, &temp_index, &files[at].path)? != after_shapes[at] {
                    verdicts[at] = false;
                    running[at] = false;
                }
            }
        }

        // The compositions of every fully proven file, each file's units
        // latest position first, in one process over disjoint paths.
        let composed: Vec<usize> = (0..files.len())
            .filter(|&at| running[at] && verdicts[at])
            .collect();
        if !composed.is_empty() {
            let mut units: Vec<&[u8]> = Vec::new();
            for &at in &composed {
                for group in files[at].groups.iter().rev() {
                    units.push(group.unit.as_slice());
                }
            }
            if probe.apply_patch_documents_cached(&units, true).is_ok() {
                for &at in &composed {
                    if staged_entry(&probe, &temp_index, &files[at].path)? != files[at].before_shape
                    {
                        verdicts[at] = false;
                    }
                }
            } else {
                // Nothing applied: each member's composition retries
                // alone, still over its own path alone.
                for &at in &composed {
                    let alone: Vec<&[u8]> = files[at]
                        .groups
                        .iter()
                        .rev()
                        .map(|group| group.unit.as_slice())
                        .collect();
                    if probe.apply_patch_documents_cached(&alone, true).is_err()
                        || staged_entry(&probe, &temp_index, &files[at].path)?
                            != files[at].before_shape
                    {
                        verdicts[at] = false;
                    }
                }
            }
        }
        Ok(verdicts)
    })();
    let _ = fs::remove_file(&temp_index);
    outcome
}

/// Seeds the probe index with exactly one entry: the candidate's own entry
/// of `path`, or an empty index when the candidate holds no entry there
/// (SPEC 10.2 step 6 — the proofs read only this path).
///
/// The index is written in process as a version-2 `DIRC`, because one
/// proof round must cost its applies, not its seed (SPEC 17.5). Git's own
/// `apply --cached` still reads and interprets the file; the stat area
/// stays zero except the mode, and the checksum trailer stays zero, which
/// this Git accepts on the read path the proofs use — verified against a
/// real `apply --cached --reverse` round trip on both object formats —
/// and rewrites with its own valid trailer when it saves.
fn seed_probe_entry(
    probe: &Git,
    temp_index: &Path,
    path: &str,
    after_entry: &Option<(EntryMode, String)>,
) -> Result<(), ManifestError> {
    let width = match probe.object_format().map_err(ManifestError::Git)? {
        ObjectFormat::Sha1 => 20,
        ObjectFormat::Sha256 => 32,
    };
    let entry = after_entry
        .as_ref()
        .map(|(mode, oid)| (mode, oid.as_str()));
    write_probe_index(temp_index, width, entry, path).map_err(ManifestError::Io)
}

/// Writes one disposable probe index: a version-2 `DIRC` holding exactly
/// one stage-zero entry of `path`, or no entries at all.
fn write_probe_index(
    index: &Path,
    oid_width: usize,
    entry: Option<(&EntryMode, &str)>,
    path: &str,
) -> Result<(), io::Error> {
    let entries = entry
        .map(|(mode, oid)| vec![(mode.as_str(), oid, path)])
        .unwrap_or_default();
    write_probe_index_entries(index, oid_width, &entries)
}

/// Writes one disposable probe index holding several entries, in the
/// path order Git's index requires. Every entry is stage zero; see
/// [`write_probe_index`] for the format decisions.
fn write_probe_index_entries(
    index: &Path,
    oid_width: usize,
    entries: &[(&str, &str, &str)],
) -> Result<(), io::Error> {
    let mut sorted: Vec<&(&str, &str, &str)> = entries.iter().collect();
    sorted.sort_by(|one, other| one.2.as_bytes().cmp(other.2.as_bytes()));
    let mut bytes = Vec::with_capacity(12 + entries.len() * (42 + oid_width + 32) + oid_width);
    bytes.extend_from_slice(b"DIRC");
    bytes.extend_from_slice(&2u32.to_be_bytes());
    bytes.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for (mode, oid, path) in sorted {
        // Ten big-endian stat words: everything zero except the mode,
        // which `apply --cached` reads and change detection ignores.
        let mut stat = [0u8; 40];
        let mode_bits = u32::from_str_radix(mode, 8).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "the entry mode is not octal")
        })?;
        stat[24..28].copy_from_slice(&mode_bits.to_be_bytes());
        bytes.extend_from_slice(&stat);
        let raw = decode_oid_bytes(oid).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "the entry object ID is not hex")
        })?;
        bytes.extend_from_slice(&raw);
        let name = path.as_bytes();
        // The name length caps at 0x0fff; a longer name reads to its
        // terminating NUL, exactly as Git's own writer marks it.
        bytes.extend_from_slice(&(name.len().min(0x0fff) as u16).to_be_bytes());
        bytes.extend_from_slice(name);
        // One to eight NUL bytes, so the entry occupies a multiple of
        // eight bytes of its own.
        let unpadded = 42 + oid_width + name.len();
        let padding = (unpadded + 8) / 8 * 8 - unpadded;
        bytes.extend(std::iter::repeat_n(0u8, padding));
    }
    // The checksum trailer stays zero.
    bytes.extend(std::iter::repeat_n(0u8, oid_width));
    fs::write(index, bytes)
}

/// Decodes one full lowercase hex object ID to its raw bytes.
fn decode_oid_bytes(oid: &str) -> Option<Vec<u8>> {
    if oid.len() % 2 != 0 {
        return None;
    }
    let mut raw = Vec::with_capacity(oid.len() / 2);
    let digits = oid.as_bytes();
    for pair in digits.chunks(2) {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        raw.push((high * 16 + low) as u8);
    }
    Some(raw)
}

/// Proves one whole-file patch restores B's entry exactly through the
/// disposable index. A patch that cannot do so disagrees with the tree
/// entries and breaks the diff contract.
fn whole_file_applies_exactly(
    git: &Git,
    path: &str,
    after_entry: &Option<(EntryMode, String)>,
    before_entry: &Option<(EntryMode, String)>,
    patch: &[u8],
) -> Result<(), ManifestError> {
    let temp_index = reserve_temp_index().map_err(ManifestError::Io)?;
    let mut probe = git.clone();
    probe.set_env_override("GIT_INDEX_FILE", &temp_index);
    let outcome = (|| {
        validate_probe_plumbing(git, path, after_entry)?;
        seed_probe_entry(&probe, &temp_index, path, after_entry)?;
        if let Err(error) = probe.apply_patch_cached(patch, true) {
            return Err(ManifestError::PatchContract {
                detail: format!(
                    "the complete patch of {path:?} does not reverse-apply onto C: {error}"
                ),
            });
        }
        if staged_entry(&probe, &temp_index, path)? != staged_shape(before_entry) {
            return Err(ManifestError::PatchContract {
                detail: format!(
                    "reversing the complete patch of {path:?} does not recompose the B entry"
                ),
            });
        }
        Ok(())
    })();
    // The disposable index never outlives the proof.
    let _ = fs::remove_file(&temp_index);
    outcome
}

/// The staged shape of one immutable entry: mode and blob object ID.
fn staged_shape(entry: &Option<(EntryMode, String)>) -> Option<(String, String)> {
    entry
        .as_ref()
        .map(|(mode, oid)| (mode.as_str().to_string(), oid.clone()))
}

/// The staged entry of `path` in the probe index: its mode and blob object
/// ID, or `None` when the path holds no stage-zero entry.
///
/// The probe index is Margin's own disposable artifact at a known path, so
/// the proof reads the file itself instead of paying one Git process per
/// check: the entry bytes are the documented `DIRC` format every Git writes
/// the same way, and a file this reader cannot understand — an index
/// version or extension it does not parse — falls back to the adapter's
/// `ls-files` read, never to a guess (SPEC 10.2 step 6, 17.5).
fn staged_entry(probe: &Git, index_path: &Path, path: &str) -> Result<Option<(String, String)>, ManifestError> {
    // The object width decides the entry layout: a SHA-256 repository
    // stores 32-byte identifiers where a SHA-1 repository stores 20. The
    // adapter memoizes the format, so this costs one process per
    // operation, not per check.
    let width = match probe.object_format().map_err(ManifestError::from)? {
        ObjectFormat::Sha1 => 20,
        ObjectFormat::Sha256 => 32,
    };
    match staged_entry_from_index_file(index_path, path, width) {
        Ok(entry) => Ok(entry),
        Err(IndexRead::Absent) => Ok(None),
        Err(IndexRead::Unparsable) => {
            probe.staged_entry_of(&OsString::from(path)).map_err(ManifestError::from)
        }
    }
}

/// Why one probe index file could not be read in-process.
enum IndexRead {
    /// The file does not exist: no entry can be staged.
    Absent,
    /// The bytes are not an index this reader understands.
    Unparsable,
}

/// Reads one path's stage-zero entry from a `DIRC` index file of version 2
/// or 3: the entry's mode and blob object ID, or `None` when the path holds
/// no entry. The layout is the documented one: a 12-byte header, then per
/// entry 40 bytes of stat data, the 20-byte object ID, a 2-byte flag word,
/// two more bytes under version 3's extended flag, the path bytes, and NUL
/// padding to a multiple of eight.
fn staged_entry_from_index_file(
    index_path: &Path,
    wanted: &str,
    oid_width: usize,
) -> Result<Option<(String, String)>, IndexRead> {
    let bytes = match fs::read(index_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(IndexRead::Absent),
        Err(_) => return Err(IndexRead::Unparsable),
    };
    let read_u32 = |at: usize| -> Option<u32> {
        let raw = bytes.get(at..at + 4)?;
        Some(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]))
    };
    if &bytes.get(..4).ok_or(IndexRead::Unparsable)? != b"DIRC" {
        return Err(IndexRead::Unparsable);
    }
    let version = read_u32(4).ok_or(IndexRead::Unparsable)?;
    if version != 2 && version != 3 {
        // Version 4 compresses paths; this reader does not guess.
        return Err(IndexRead::Unparsable);
    }
    let count = read_u32(8).ok_or(IndexRead::Unparsable)? as usize;
    let header_base = 40 + oid_width;
    let mut at = 12usize;
    for _ in 0..count {
        let flags = bytes
            .get(at + header_base..at + header_base + 2)
            .map(|raw| u16::from_be_bytes([raw[0], raw[1]]))
            .ok_or(IndexRead::Unparsable)?;
        let header = if version == 3 && flags & 0x4000 != 0 {
            at + header_base + 4
        } else {
            at + header_base + 2
        };
        let Some(rest) = bytes.get(header..) else {
            return Err(IndexRead::Unparsable);
        };
        let path_end = rest
            .iter()
            .position(|byte| *byte == 0)
            .ok_or(IndexRead::Unparsable)?;
        let stored = &rest[..path_end];
        // The entry occupies a multiple of eight bytes counted from its
        // own start, with at least one NUL after the path — Git pads per
        // entry, never from the file offset.
        let fixed = header - at + path_end;
        let entry_size = (fixed + 8) & !7;
        if stored != wanted.as_bytes() {
            at += entry_size;
            continue;
        }
        let mode = read_u32(at + 24).ok_or(IndexRead::Unparsable)?;
        let sha_raw = bytes
            .get(at + 40..at + 40 + oid_width)
            .ok_or(IndexRead::Unparsable)?;
        let mut hex = String::with_capacity(sha_raw.len() * 2);
        for byte in sha_raw {
            hex.push_str(&format!("{byte:02x}"));
        }
        return Ok(Some((format!("{mode:06o}"), hex)));
    }
    Ok(None)
}


// ---------------------------------------------------------------------------
// Bounded detail pages (SPEC 6.3, 7.2)
// ---------------------------------------------------------------------------

/// One selected detail page of a `diff` result.
struct DetailPage {
    /// The change details this page carries, in manifest order.
    changes: Vec<ChangeDetail>,
    /// Whether all requested detail was returned.
    complete: bool,
    /// The cursor of the next page; `None` when this page is complete.
    next_cursor: Option<String>,
}

/// Selects the detail page of one request from the whole-scope details.
///
/// A path filter first hides every group outside its roots; the page then
/// takes groups in manifest order from the cursor position until the group
/// limit or the byte budget of [`MAX_DIFF_PAGE_BYTES`] is reached, whichever
/// comes first. One page always holds at least one group, so paging always
/// makes progress even when a single presentation fills the whole budget.
/// A `--change` selection replaces the page with that one group.
fn select_detail_page(
    git: &Git,
    details: &[ChangeDetail],
    contract: &domain::CursorContext,
    request: &DiffRequest,
) -> Result<DetailPage, OperationError> {
    if let Some(wanted) = &request.change {
        // One named group is the whole page, whatever the filter would have
        // hidden: the selection names its target by identity.
        let Some(found) = details.iter().find(|detail| &detail.change_id == wanted) else {
            return Err(OperationError::new(
                "UNKNOWN_CHANGE",
                format!("no change group {wanted} exists in this review"),
            )
            .recovery(&["margin", "diff", "--json"]));
        };
        return Ok(DetailPage {
            changes: vec![found.clone()],
            complete: true,
            next_cursor: None,
        });
    }

    let filtered: Vec<usize> = details
        .iter()
        .enumerate()
        .filter(|(_, detail)| passes_filter(OsStr::new(&detail.path), &request.path_filter))
        .map(|(index, _)| index)
        .collect();
    let start = match &request.cursor {
        Some(raw) => verify_cursor(git, raw, contract, filtered.len())?,
        None => 0,
    };

    let mut changes: Vec<ChangeDetail> = Vec::new();
    let mut used = 0usize;
    for &index in filtered.iter().skip(start) {
        let weight = detail_weight(&details[index]);
        if !changes.is_empty()
            && (changes.len() >= contract.limit as usize || used + weight > MAX_DIFF_PAGE_BYTES)
        {
            break;
        }
        used += weight;
        changes.push(details[index].clone());
    }

    // The next position still has a group, or this page is complete.
    let complete = start + changes.len() >= filtered.len();
    let next_cursor = if complete {
        None
    } else {
        let mut next = contract.clone();
        next.offset = (start + changes.len()) as u64;
        let digest = Identities::new(git)
            .and_then(|identities| identities.cursor(&next))
            .map_err(OperationError::from)?;
        Some(format!("{}-{}", next.offset, digest.id()))
    };
    Ok(DetailPage {
        changes,
        complete,
        next_cursor,
    })
}

/// Verifies one cursor against the digest of the current review identity
/// and page contract (SPEC 6.3). Anything that does not match — another
/// review, another contract, a hand-edited offset or digest, or a position
/// no longer covered — is one `STALE_CURSOR` refusal.
fn verify_cursor(
    git: &Git,
    cursor: &str,
    contract: &domain::CursorContext,
    available: usize,
) -> Result<usize, OperationError> {
    let stale = |what: &str| {
        OperationError::new(
            "STALE_CURSOR",
            format!("the cursor {what}; rerun the query without a cursor for a fresh page"),
        )
        .recovery(&["margin", "diff", "--json"])
    };
    let Some((offset, id)) = cursor.rsplit_once('-') else {
        return Err(stale("is malformed"));
    };
    let Ok(offset) = offset.parse::<u64>() else {
        return Err(stale("is malformed"));
    };
    // The digest covers the claimed position, so an edited offset demands a
    // digest its editor cannot compute (SPEC 6.3).
    let mut claimed = contract.clone();
    claimed.offset = offset;
    let digest = Identities::new(git)
        .and_then(|identities| identities.cursor(&claimed))
        .map_err(OperationError::from)?;
    if id != digest.id() {
        return Err(stale(
            "belongs to another review or page contract than this comparison",
        ));
    }
    if offset as usize >= available {
        return Err(stale("names a position this comparison no longer holds"));
    }
    Ok(offset as usize)
}

/// The worst-case JSON size of one change detail, in bytes (SPEC 6.3).
///
/// The page bound covers response body content, so the page builder must
/// know each detail's serialized size before it serializes. This measure is
/// the exact worst case of the CLI projection: every string counts its JSON
/// escaping, and every fixed field counts a constant that never shrinks. A
/// test holds it against the real serializer, so the page built under this
/// bound always fits the bound after projection.
pub fn detail_weight(detail: &ChangeDetail) -> usize {
    // Keys, brackets, booleans, nulls, and the fixed scalar fields. Every
    // per-item constant covers the largest JSON encoding of that item, so
    // the worst-case line numbers of a 16-MiB document still fit.
    let mut weight = 768 + detail.ranges.len() * 128;
    weight += detail.change_id.len() + escaped_json_len(&detail.path);
    weight += detail.reason.len() + escaped_json_len(&detail.presentation.heading);
    for mode in [detail.mode_before, detail.mode_after] {
        weight += mode.map_or(4, |mode| mode.len() + 2);
    }
    for oid in [&detail.oid_before, &detail.oid_after] {
        weight += oid.as_deref().map_or(4, |oid| oid.len() + 2);
    }
    for excerpt in [&detail.presentation.before, &detail.presentation.after] {
        weight += 128;
        if let Some(text) = &excerpt.text {
            weight += escaped_json_len(text) + 2;
        }
        if let Some(hex) = &excerpt.hex {
            weight += hex.len() + 2;
        }
    }
    for hint in &detail.presentation.word_hints {
        weight += 128;
        for part in [&hint.prefix, &hint.removed, &hint.added, &hint.suffix] {
            weight += escaped_json_len(part);
        }
    }
    weight
}

/// The JSON string size of `text` in the worst case: quotes and backslashes
/// escape to two bytes, control characters to six, and every other
/// character counts its UTF-8 encoding.
fn escaped_json_len(text: &str) -> usize {
    text.chars()
        .map(|character| match character {
            '"' | '\\' => 2,
            control if (control as u32) < 0x20 => 6,
            other => other.len_utf8(),
        })
        .sum()
}

/// Builds the bounded detail of one safe group (SPEC 7.2): the stable
/// identity, the path and its presence transition, the source ranges of the
/// group's complete hunks, and the bounded presentation.
fn change_detail(file: &GroupedFile, group: &PassageChange) -> ChangeDetail {
    let mut detail = ChangeDetail {
        change_id: group.change_id.id(),
        path: group.path.clone(),
        kind: file.kind.as_str(),
        mode_before: file.mode_before.map(|mode| mode.as_str()),
        mode_after: file.mode_after.map(|mode| mode.as_str()),
        oid_before: file.oid_before.clone(),
        oid_after: file.oid_after.clone(),
        opaque: file.opaque,
        whole_file: file.whole_file_reason.is_some(),
        reason: group.reason.clone(),
        ranges: group
            .hunk_indexes
            .iter()
            .map(|&index| {
                let hunk = &file.hunks[index];
                ChangeRange {
                    old_start: hunk.old_start,
                    old_count: hunk.old_count,
                    new_start: hunk.new_start,
                    new_count: hunk.new_count,
                }
            })
            .collect(),
        ranges_omitted: 0,
        presentation: bounded_presentation(group.presentation.clone()),
    };
    bound_group_detail(&mut detail);
    detail
}

/// Bounds one group's carried detail to [`MAX_GROUP_PRESENTATION_BYTES`]
/// (SPEC 7.2): the excerpts are already capped per side, so an overflowing
/// group drops its word hints and then its source ranges from the end, each
/// with its omitted count. A group that still overflows — an enormous path
/// or reason — stays whole and fills one page alone; the omitted counts
/// never hide that.
fn bound_group_detail(detail: &mut ChangeDetail) {
    while detail_weight(detail) > MAX_GROUP_PRESENTATION_BYTES {
        if detail.presentation.word_hints.pop().is_some() {
            detail.presentation.word_hints_omitted += 1;
            continue;
        }
        if detail.ranges.pop().is_some() {
            detail.ranges_omitted += 1;
            continue;
        }
        break;
    }
}

/// Bounds one group presentation (SPEC 7.2): both excerpts obey
/// [`MAX_PRESENTATION_EXCERPT_BYTES`] with explicit truncation data, and
/// the word hints obey [`MAX_PRESENTATION_WORD_HINTS`] with their omitted
/// count. The grouping's exact bytes stay untouched; only the carried
/// detail shrinks.
fn bounded_presentation(presentation: markdown::GroupPresentation) -> BoundedPresentation {
    let hints = presentation.word_hints.len();
    BoundedPresentation {
        heading: presentation.heading,
        before: bounded_excerpt(presentation.before),
        after: bounded_excerpt(presentation.after),
        word_hints: presentation
            .word_hints
            .into_iter()
            .take(MAX_PRESENTATION_WORD_HINTS)
            .collect(),
        word_hints_omitted: hints.saturating_sub(MAX_PRESENTATION_WORD_HINTS),
        whitespace_only: presentation.whitespace_only,
        link_change: presentation.link_change,
        invisible_change: presentation.invisible_change,
    }
}

/// Bounds one excerpt. Readable UTF-8 carries its exact leading bytes,
/// cut at the last line boundary inside the bound; a side that is not valid
/// UTF-8 carries one bounded hexadecimal view instead. Either way the true
/// byte length and the truncation flag report what is missing (SPEC 6.3,
/// 7.2).
fn bounded_excerpt(excerpt: markdown::SourceExcerpt) -> BoundedExcerpt {
    let total_bytes = excerpt.bytes.len();
    let cap = MAX_PRESENTATION_EXCERPT_BYTES;
    if let Ok(text) = std::str::from_utf8(&excerpt.bytes) {
        let (text, truncated) = bound_text(text, cap);
        return BoundedExcerpt {
            first_line: excerpt.first_line,
            last_line: excerpt.last_line,
            total_bytes,
            truncated,
            text: Some(text),
            hex: None,
        };
    }
    let hex = excerpt
        .bytes
        .iter()
        .take(cap)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    BoundedExcerpt {
        first_line: excerpt.first_line,
        last_line: excerpt.last_line,
        total_bytes,
        truncated: true,
        text: None,
        hex: Some(hex),
    }
}

/// Cuts `text` to at most `cap` bytes at its last line boundary inside the
/// bound, or at a character boundary when no whole line fits.
fn bound_text(text: &str, cap: usize) -> (String, bool) {
    if text.len() <= cap {
        return (text.to_string(), false);
    }
    let mut cut = text
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|&index| index <= cap)
        .last()
        .unwrap_or(0);
    if let Some(newline) = text[..cut].rfind('\n') {
        cut = newline + 1;
    }
    (text[..cut].to_string(), true)
}

/// Resolves one user-supplied revision to exactly one immutable commit
/// object ID for a document view (SPEC 4.1, 14.1).
///
/// The adapter's `--verify --end-of-options` boundary keeps a revision
/// that starts with a dash from being parsed as an option, and `^{commit}`
/// rejects every other object type. A revision Git cannot resolve to one
/// commit is one invalid input — exit class 2 — while every other process
/// failure keeps its own typed error.
fn resolve_immutable_revision(git: &Git, revision: &str) -> Result<String, OperationError> {
    git.resolve_commit(OsStr::new(revision))
        .map_err(|error| match error {
            GitError::Exit { .. } => OperationError::new(
                "INVALID_ARGUMENT",
                format!("the revision {revision:?} resolves to no one commit: {error}"),
            )
            .recovery(&["margin", "log", "--json"]),
            other => OperationError::from(other),
        })
}

/// Bounds one readable document view (SPEC 6.3): the byte bound cuts at a
/// character boundary, the line bound keeps whole leading lines inside what
/// the byte bound left, and the truncation flag reports that anything was
/// dropped. The totals always describe the complete document.
fn bounded_document_text(
    blob_oid: &str,
    mode: EntryMode,
    text: &str,
    max_bytes: Option<u64>,
    max_lines: Option<u64>,
) -> BoundedDocumentText {
    let total_bytes = text.len();
    let total_lines = count_lines(text.as_bytes());
    let byte_cap = document_byte_cap(max_bytes);
    let mut cut = total_bytes;
    let mut truncated = false;
    if total_bytes > byte_cap {
        cut = text
            .char_indices()
            .map(|(index, _)| index)
            .take_while(|&index| index <= byte_cap)
            .last()
            .unwrap_or(0);
        truncated = true;
    }
    if let Some(lines) = max_lines {
        let kept = &text.as_bytes()[..cut];
        if count_lines(kept) > lines {
            cut = line_cut(&text[..cut], lines);
            truncated = true;
        }
    }
    let returned_bytes = cut;
    let returned_lines = count_lines(&text.as_bytes()[..cut]);
    BoundedDocumentText {
        blob_oid: blob_oid.to_string(),
        mode,
        text: text[..cut].to_string(),
        total_bytes,
        returned_bytes,
        total_lines,
        returned_lines,
        truncated,
    }
}

/// Bounds one opaque document view to its hexadecimal representation
/// (SPEC 6.3): the leading bytes the smaller of the caller's byte bound and
/// [`MAX_SHOW_HEX_BYTES`] allows, with the true byte length and the
/// truncation flag.
fn bounded_document_hex(
    blob_oid: &str,
    mode: EntryMode,
    bytes: &[u8],
    max_bytes: Option<u64>,
) -> BoundedDocumentHex {
    let total_bytes = bytes.len();
    let kept = total_bytes.min(document_byte_cap(max_bytes).min(domain::MAX_SHOW_HEX_BYTES));
    let hex = bytes[..kept]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    BoundedDocumentHex {
        blob_oid: blob_oid.to_string(),
        mode,
        hex,
        returned_bytes: kept,
        total_bytes,
        truncated: kept < total_bytes,
    }
}

/// The byte cap one document view applies: the caller's bound, or the
/// per-file ceiling of SPEC 3.3 when the caller set none.
fn document_byte_cap(max_bytes: Option<u64>) -> usize {
    usize::try_from(max_bytes.unwrap_or(MAX_DOCUMENT_BYTES)).unwrap_or(usize::MAX)
}

/// Counts the lines of one byte string the way [`str::lines`] does: one
/// line per newline, plus one more for a trailing partial line.
fn count_lines(bytes: &[u8]) -> u64 {
    let newlines = bytes.iter().filter(|byte| **byte == b'\n').count();
    let partial = !bytes.is_empty() && bytes[bytes.len() - 1] != b'\n';
    u64::try_from(newlines + usize::from(partial)).unwrap_or(u64::MAX)
}

/// Returns the byte offset just after the `limit`-th line of `text`.
fn line_cut(text: &str, limit: u64) -> usize {
    let mut seen = 0u64;
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            seen += 1;
            if seen == limit {
                return index + 1;
            }
        }
    }
    text.len()
}

/// The tree one checkpoint of the current working content would create
/// (SPEC 6.2 `--working`): the candidate's tree with exactly the eligible
/// scoped working changes staged, proven in one disposable index.
///
/// The preview is deterministic — the same working content yields the same
/// tree — and it creates no commit, moves no ref, and touches no working
/// file. Only disposable Git objects and one temporary index file exist
/// (SPEC 6.4).
fn working_preview_tree(
    git: &Git,
    scope: &ResolvedScope,
    candidate: &str,
) -> Result<String, DraftProofError> {
    let changes = scoped_working_changes(git, scope, candidate)?;
    intended_draft_tree(git, candidate, &changes)
}

/// One pinned comparison before any presentation or plan decision (SPEC
/// 7.2): the resolved scope, the full branch ref the identity names, the
/// one grouped manifest with its baseline and candidate, and the digest of
/// the scoped candidate content.
struct PinnedReview {
    /// The resolved writing scope.
    resolved: ResolvedScope,
    /// The full branch ref of the review identity.
    branch_ref: String,
    /// The complete grouped manifest of the comparison.
    manifest: GroupedManifest,
    /// Digest of the exact scoped entries of the candidate.
    candidate_scope_digest: Fingerprint,
}

/// One pinned restoration context (SPEC 7.4, 9.5): everything the two
/// restoration phases share between validation and application.
struct PinnedRestore {
    /// The resolved writing scope.
    resolved: ResolvedScope,
    /// The full branch ref the plan pins.
    branch_ref: String,
    /// The branch the shared guards observed, for the step-6 recheck.
    guard_branch: Option<String>,
    /// The committed candidate C the plan pins.
    candidate_oid: String,
    /// Digest of the exact scoped entries of C.
    candidate_scope_digest: Fingerprint,
    /// The resolved source revision.
    revision_oid: String,
    /// The proved intended result.
    intended: IntendedRestore,
    /// The baseline the restoration leaves unchanged; `None` when history
    /// anchors none, which no restoration requires.
    baseline_oid: Option<String>,
}

/// The typed refusal when one restoration plan names another workspace than
/// the one it must pin (SPEC 7.4, 7.5): every context field must match the
/// current state, so a plan written against an older or different context is
/// stale, never silently reused.
fn stale_restore_plan(field: &str) -> OperationError {
    OperationError::new(
        "STALE_REVIEW",
        format!(
            "the {field} of the restoration plan names another context than the workspace holds"
        ),
    )
    .recovery(&["margin", "status", "--json"])
}

/// The typed refusal when one plan names another comparison than the
/// workspace pins (SPEC 7.3, 7.5): every context field must match the
/// current pinned review, so a plan written against an older or different
/// review is stale, never silently reused.
fn stale_plan_error(plan: &ReviewPlan, pinned: &PinnedReview) -> Option<OperationError> {
    let stale = |field: &str| {
        OperationError::new(
            "STALE_REVIEW",
            format!("the {field} of the plan names another comparison than the pinned review"),
        )
        .recovery(&["margin", "diff", "--json"])
    };
    if plan.review_id != pinned.manifest.review_id.id() {
        return Some(stale("review_id"));
    }
    if plan.branch_ref != pinned.branch_ref {
        return Some(stale("branch_ref"));
    }
    if plan.scope_id != pinned.resolved.digest.id() {
        return Some(stale("scope_id"));
    }
    if plan.baseline_oid != pinned.manifest.baseline_oid {
        return Some(stale("baseline_oid"));
    }
    if plan.candidate_oid != pinned.manifest.candidate_oid {
        return Some(stale("candidate_oid"));
    }
    None
}

// ---------------------------------------------------------------------------
// The intended review result (SPEC 9.3, 9.1 step 5)
// ---------------------------------------------------------------------------

/// The proved intended result of one decided review plan (SPEC 9.3).
struct IntendedReview {
    /// The full intended result tree T.
    tree: String,
    /// The literal paths the decisions rewrite, sorted by path bytes.
    paths: Vec<String>,
}

/// Computes and proves the intended result tree of one decided review plan
/// (SPEC 9.3): one disposable Git index seeded from C through `read-tree`
/// with no working-tree update flag, each restored group's complete patch
/// reverse-applied, and `write-tree` at the end.
///
/// One file's selected groups apply latest source position first, so every
/// patch's recorded line numbers stay valid while later positions change
/// above them. The proof validates the result before any real application:
/// the staged delta against C names exactly the paths of the restored
/// groups, so no out-of-scope entry moves and no restored group silently
/// changes nothing (SPEC 9.1 step 5).
///
/// The disposable index never aliases the real index and never outlives
/// the call, whatever the result; refs, the real index, and working bytes
/// stay untouched, and only disposable Git objects and one temporary index
/// file exist (SPEC 6.4).
fn intended_review_tree(
    git: &Git,
    manifest: &GroupedManifest,
    restore: &BTreeSet<&str>,
) -> Result<IntendedReview, ReviewProofError> {
    let temp_index = reserve_temp_index().map_err(ReviewProofError::Io)?;
    let proof = prove_intended_review_tree(git, &temp_index, manifest, restore);
    // The disposable index never outlives the proof, on success or failure.
    let _ = fs::remove_file(&temp_index);
    proof
}

/// The selected complete patch units of one decided plan, in the one order
/// every application of this manifest uses (SPEC 9.3-9.4): files in
/// manifest order, each file's selected groups latest source position
/// first, so a unit's recorded line numbers stay valid while the units
/// above it change. The real application replays exactly this sequence, so
/// it never regenerates a different patch, restages working files, or
/// broadens a selected group (task T032).
///
/// Each entry names the file the group rewrites, the group's change
/// identity for the refusals, and the group's one complete original patch
/// unit (SPEC 8.6, 10.3).
fn selected_reverse_units<'a>(
    manifest: &'a GroupedManifest,
    restore: &BTreeSet<&str>,
) -> Vec<(&'a str, &'a Fingerprint, &'a [u8])> {
    let mut units = Vec::new();
    for file in &manifest.files {
        for group in file.groups.iter().rev() {
            if !restore.contains(group.change_id.id().as_str()) {
                continue;
            }
            units.push((
                file.path.as_str(),
                &group.change_id,
                group.group.patch_units[0].as_slice(),
            ));
        }
    }
    units
}

/// One run of the intended-review proof inside the disposable index at
/// `temp_index` (SPEC 9.3).
fn prove_intended_review_tree(
    git: &Git,
    temp_index: &Path,
    manifest: &GroupedManifest,
    restore: &BTreeSet<&str>,
) -> Result<IntendedReview, ReviewProofError> {
    let mut probe = git.clone();
    probe.set_env_override("GIT_INDEX_FILE", temp_index);
    probe.read_tree(&manifest.candidate_oid)?;

    // The reverse application: coverage already proved every selected
    // identity names one complete group of this manifest, and the grouping
    // proved each unit reverse-applies onto C alone (SPEC 10.2 step 6).
    let mut restored_paths: BTreeSet<&str> = BTreeSet::new();
    for (path, change_id, unit) in selected_reverse_units(manifest, restore) {
        if let Err(error) = probe.apply_patch_cached(unit, true) {
            return Err(ReviewProofError::Contract {
                detail: format!(
                    "the complete patch of the restored group {} of {path:?} does not \
                     reverse-apply onto the candidate: {error}",
                    change_id.id()
                ),
            });
        }
        restored_paths.insert(path);
    }

    // The staged delta names exactly the rewritten paths (SPEC 9.1 step 5):
    // no out-of-scope entry moves, and every restored group's file really
    // changed.
    let delta: BTreeSet<OsString> = probe
        .staged_against(&manifest.candidate_oid)?
        .into_iter()
        .collect();
    let expected: BTreeSet<OsString> = restored_paths
        .iter()
        .map(|path| OsString::from(*path))
        .collect();
    if delta != expected {
        let unexpected: Vec<OsString> = delta.difference(&expected).cloned().collect();
        let missing: Vec<OsString> = expected.difference(&delta).cloned().collect();
        return Err(ReviewProofError::Contract {
            detail: format!(
                "the intended review result disagrees with its own manifest: {} path(s) changed \
                 outside the restored groups ({}), {} restored path(s) changed nothing ({})",
                unexpected.len(),
                listed_paths(&unexpected),
                missing.len(),
                listed_paths(&missing),
            ),
        });
    }
    Ok(IntendedReview {
        tree: probe.write_tree()?,
        paths: restored_paths.into_iter().map(str::to_string).collect(),
    })
}

/// Failure of one decided review's intended-result proof (SPEC 9.3).
///
/// Every case is found before any real write — a dry run writes nothing,
/// and the application repeats this proof under the mutation lock — so
/// each refusal carries unchanged effects.
#[derive(Debug)]
enum ReviewProofError {
    /// A Git process or the filesystem failed before the proof could
    /// decide.
    Inspection(PolicyError),
    /// The disposable validation index could not be created.
    Io(io::Error),
    /// The patches or the staged delta disagree with the manifest the same
    /// operation built, so the grouping contract broke; Margin guesses
    /// nothing and writes nothing.
    Contract {
        /// What disagreed.
        detail: String,
    },
}

impl ReviewProofError {
    /// The SPEC 7.5 error code of this refusal.
    fn public_code(&self) -> &'static str {
        match self {
            ReviewProofError::Inspection(error) => error.public_code(),
            ReviewProofError::Io(_) => "IO_FAILED",
            ReviewProofError::Contract { .. } => "CONTRACT_VIOLATION",
        }
    }
}

impl fmt::Display for ReviewProofError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReviewProofError::Inspection(error) => write!(formatter, "{error}"),
            ReviewProofError::Io(error) => {
                write!(formatter, "cannot create the disposable index: {error}")
            }
            ReviewProofError::Contract { detail } => write!(formatter, "{detail}"),
        }
    }
}

impl std::error::Error for ReviewProofError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ReviewProofError::Inspection(error) => Some(error),
            ReviewProofError::Io(error) => Some(error),
            ReviewProofError::Contract { .. } => None,
        }
    }
}

impl From<GitError> for ReviewProofError {
    fn from(error: GitError) -> Self {
        ReviewProofError::Inspection(PolicyError::Git(error))
    }
}

impl From<ReviewProofError> for OperationError {
    fn from(error: ReviewProofError) -> Self {
        OperationError::new(error.public_code(), error.to_string())
    }
}

// ---------------------------------------------------------------------------
// The intended restoration result (SPEC 9.5, 9.1 step 5)
// ---------------------------------------------------------------------------

/// The proved intended result of one historical restoration (SPEC 9.5): the
/// full result tree T, the exact patch that reaches it from C, and the
/// literal paths the restoration rewrites — at most the one document.
struct IntendedRestore {
    /// The full intended result tree T.
    tree: String,
    /// The complete patch from C to T under the fixed diff contract of
    /// SPEC 10.1, restricted to the one document. The real application
    /// replays exactly these bytes, never a regenerated patch.
    patch: Vec<u8>,
    /// The literal paths the restoration rewrites, sorted by path bytes:
    /// the one document when its content changes, nothing when it does not.
    paths: Vec<String>,
}

/// Computes and proves the intended result tree of one historical
/// restoration (SPEC 9.5): one Git patch from C to a temporary tree where
/// exactly the selected document equals the source revision, including
/// absence and mode.
///
/// The patch comes from the same fixed diff contract every source patch
/// uses, restricted to the one document (SPEC 10.1). One disposable Git
/// index seeded from C forward-applies it, and the proof validates the
/// result before any real application: the staged delta against C names
/// exactly the document, so no out-of-scope entry moves and a changed
/// document really changed (SPEC 9.1 step 5). A source revision and a
/// candidate that hold the same entry produce no patch and T equals C's own
/// tree — a no-content-change restoration that still commits one receipt
/// per request (SPEC 7.4).
///
/// The disposable index never aliases the real index and never outlives the
/// call, whatever the result; refs, the real index, and working bytes stay
/// untouched (SPEC 6.4).
fn intended_restore_tree(
    git: &Git,
    candidate: &str,
    revision: &str,
    path: &str,
) -> Result<IntendedRestore, OperationError> {
    // Only a regular or executable blob entry, or absence, is a restorable
    // state. A path below one directory of either tree names several
    // working files, and a symbolic link or a gitlink names no document at
    // all (SPEC 6.3, 3.2 rule 5).
    restorable_document_problem(git, candidate, path)?;
    restorable_document_problem(git, revision, path)?;

    let patch = git
        .diff_tree_patch(candidate, revision, &[OsString::from(path)])
        .map_err(OperationError::from)?;
    let temp_index = reserve_temp_index().map_err(|error| {
        OperationError::new(
            "IO_FAILED",
            format!("cannot create the disposable index: {error}"),
        )
    })?;
    let proof = prove_intended_restore_tree(git, &temp_index, candidate, &patch, path);
    // The disposable index never outlives the proof, on success or failure.
    let _ = fs::remove_file(&temp_index);
    proof
}

/// One run of the intended-restoration proof inside the disposable index at
/// `temp_index` (SPEC 9.5).
fn prove_intended_restore_tree(
    git: &Git,
    temp_index: &Path,
    candidate: &str,
    patch: &[u8],
    path: &str,
) -> Result<IntendedRestore, OperationError> {
    let mut probe = git.clone();
    probe.set_env_override("GIT_INDEX_FILE", temp_index);
    probe.read_tree(candidate)?;
    let mut paths = Vec::new();
    if !patch.is_empty() {
        probe.apply_patch_cached(patch, false).map_err(|error| {
            OperationError::new(
                "GIT_FAILED",
                format!(
                    "the restoration patch of {path:?} does not apply to the candidate: {error}",
                ),
            )
        })?;
        paths.push(path.to_string());
    }

    // The staged delta names exactly the restored document (SPEC 9.1 step
    // 5): no out-of-scope entry moves, and a document the patch touched
    // really changed.
    let delta: BTreeSet<OsString> = probe
        .staged_against(candidate)
        .map_err(OperationError::from)?
        .into_iter()
        .collect();
    let expected: BTreeSet<OsString> = paths.iter().map(OsString::from).collect();
    if delta != expected {
        let unexpected: Vec<OsString> = delta.difference(&expected).cloned().collect();
        return Err(OperationError::new(
            "CONTRACT_VIOLATION",
            format!(
                "the intended restoration disagrees with its own patch: {} path(s) changed \
                 outside the restored document ({}), while the document itself named {} \
                 path(s)",
                unexpected.len(),
                listed_paths(&unexpected),
                paths.len(),
            ),
        ));
    }
    Ok(IntendedRestore {
        tree: probe.write_tree().map_err(OperationError::from)?,
        patch: patch.to_vec(),
        paths,
    })
}

/// Refuses one restoration whose document path names no one restorable
/// document in the tree of `revision` (SPEC 6.3, 3.2 rule 5): a path below
/// one directory names several files, and a symbolic link or a gitlink
/// entry names no document. Absence passes: it is one restorable state.
fn restorable_document_problem(
    git: &Git,
    revision: &str,
    path: &str,
) -> Result<(), OperationError> {
    let entries = git.ls_tree(revision).map_err(OperationError::from)?;
    let wanted = os_string_from_bytes(path.as_bytes().to_vec());
    let directory = format!("{path}/");
    for entry in &entries {
        if entry.path == wanted {
            if entry.kind != "blob" {
                let what = match entry.kind.as_str() {
                    "tree" => "a directory",
                    "commit" => "a gitlink",
                    other => other,
                };
                return Err(OperationError::new(
                    "INVALID_ARGUMENT",
                    format!("the path {path:?} stores {what} in {revision}, not one document",),
                ));
            }
            return Ok(());
        }
    }
    if entries.iter().any(|entry| {
        entry
            .path
            .as_encoded_bytes()
            .starts_with(directory.as_bytes())
    }) {
        return Err(OperationError::new(
            "INVALID_ARGUMENT",
            format!("the path {path:?} names a directory in {revision}, not one document"),
        ));
    }
    Ok(())
}

/// One structurally valid Margin marker: a commit whose reserved trailers,
/// parent, tree, scope, and out-of-scope behavior all survived validation
/// (SPEC 8.4).
///
/// A valid marker is evidence about one commit only. It is not approval of
/// newer work, and only baseline selection (SPEC 8.5) decides whether it
/// anchors the current scope.
#[derive(Clone, Debug)]
pub struct ValidMarker {
    /// The validated commit object ID.
    pub commit: String,
    /// The `Margin-Kind` value.
    pub kind: CommitKind,
    /// The scope descriptor parsed from `Margin-Scope-Spec`.
    pub scope: ScopeDescriptor,
    /// The `Margin-Scope` value.
    pub scope_digest: String,
    /// The `Margin-Scope-Spec` value, in canonical form.
    pub scope_spec: String,
    /// The `Margin-Candidate` value.
    pub candidate: String,
    /// The `Margin-Result-Tree` value, equal to the actual tree.
    pub result_tree: String,
    /// The `Margin-Baseline` value. Review only.
    pub baseline: Option<String>,
    /// The `Margin-Request` value. Review and restore.
    pub request: Option<String>,
    /// The `Margin-Plan` value. Review and restore.
    pub plan: Option<String>,
    /// The `Margin-Source` value. Restore only.
    pub source: Option<RestoreSource>,
}

/// Why one commit is not a valid Margin marker (SPEC 8.4 rules 1 to 5 and 7).
/// An invalid or copied marker remains ordinary Git history and confers no
/// approval; the interface layer surfaces why it was ignored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkerInvalid {
    /// The message holds no strictly parsable trailer block.
    TrailerBlock(TrailerParseError),
    /// A reserved trailer violates the SPEC 8.3 count, value, or kind rules.
    Trailers(TrailerProblem),
    /// Canonicalizing `Margin-Scope-Spec` does not yield `Margin-Scope`.
    ScopeDigestMismatch {
        /// The digest the trailer claims.
        trailer: String,
        /// The digest the canonical descriptor actually produces.
        computed: String,
    },
    /// The commit has more than one parent.
    ParentCount {
        /// How many parents were found.
        found: usize,
    },
    /// The recorded candidate is not the actual parent.
    CandidateMismatch {
        /// The candidate the trailer claims.
        trailer: String,
        /// The parent the commit actually has.
        parent: Option<String>,
    },
    /// A commit without parents must be the initial start marker.
    RootNotStart,
    /// The actual tree differs from `Margin-Result-Tree`.
    ResultTreeMismatch {
        /// The tree the trailer claims.
        trailer: String,
        /// The tree the commit actually has.
        actual: String,
    },
    /// The commit changed entries outside its own scope (SPEC 8.4 rule 5).
    OutOfScopeChange {
        /// The changed out-of-scope paths.
        paths: Vec<String>,
    },
    /// An initial root start captured an entry that is not an eligible
    /// scoped file.
    IneligibleStartEntry {
        /// The repository-relative path.
        path: String,
        /// Why the entry is not eligible.
        why: &'static str,
    },
    /// The commit's context depends on shallow history, grafts, or
    /// replacement objects (SPEC 8.4 rule 7).
    UnavailableContext {
        /// What mechanism the context depends on.
        what: &'static str,
    },
}

impl MarkerInvalid {
    /// The SPEC 7.5 error code of this finding.
    pub fn public_code(&self) -> &'static str {
        match self {
            MarkerInvalid::UnavailableContext { .. } => "BASELINE_UNAVAILABLE",
            _ => "CONTRACT_VIOLATION",
        }
    }
}

impl fmt::Display for MarkerInvalid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MarkerInvalid::TrailerBlock(error) => write!(formatter, "{error}"),
            MarkerInvalid::Trailers(error) => write!(formatter, "{error}"),
            MarkerInvalid::ScopeDigestMismatch { trailer, computed } => write!(
                formatter,
                "the scope digest is {trailer} but the descriptor hashes to {computed}"
            ),
            MarkerInvalid::ParentCount { found } => {
                write!(formatter, "the marker has {found} parents")
            }
            MarkerInvalid::CandidateMismatch { trailer, parent } => write!(
                formatter,
                "the candidate is {trailer} but the parent is {}",
                parent.as_deref().unwrap_or("absent")
            ),
            MarkerInvalid::RootNotStart => write!(
                formatter,
                "only an initial start marker may have no parents"
            ),
            MarkerInvalid::ResultTreeMismatch { trailer, actual } => write!(
                formatter,
                "the result tree is {trailer} but the commit tree is {actual}"
            ),
            MarkerInvalid::OutOfScopeChange { paths } => write!(
                formatter,
                "the marker changed {} out-of-scope entr{}: {}",
                paths.len(),
                if paths.len() == 1 { "y" } else { "ies" },
                paths.join(", ")
            ),
            MarkerInvalid::IneligibleStartEntry { path, why } => write!(
                formatter,
                "the starting tree holds {path}, which is not eligible: {why}"
            ),
            MarkerInvalid::UnavailableContext { what } => {
                write!(formatter, "the marker context depends on {what}")
            }
        }
    }
}

impl From<TrailerParseError> for MarkerInvalid {
    fn from(error: TrailerParseError) -> Self {
        MarkerInvalid::TrailerBlock(error)
    }
}

impl From<TrailerProblem> for MarkerInvalid {
    fn from(error: TrailerProblem) -> Self {
        MarkerInvalid::Trailers(error)
    }
}

impl std::error::Error for MarkerInvalid {}

/// Failure to inspect the repository while validating one marker.
#[derive(Debug)]
pub enum MarkerError {
    /// The commit violates the marker contract; it confers no approval.
    Invalid(MarkerInvalid),
    /// A Git process failed.
    Git(GitError),
    /// The repository could not be inspected.
    Io(io::Error),
}

impl MarkerError {
    /// The SPEC 7.5 error code of this failure.
    pub fn public_code(&self) -> &'static str {
        match self {
            MarkerError::Invalid(problem) => problem.public_code(),
            MarkerError::Git(error) => error.public_code(),
            MarkerError::Io(_) => "IO_FAILED",
        }
    }
}

impl fmt::Display for MarkerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MarkerError::Invalid(problem) => write!(formatter, "{problem}"),
            MarkerError::Git(error) => write!(formatter, "{error}"),
            MarkerError::Io(error) => write!(formatter, "cannot inspect history: {error}"),
        }
    }
}

impl std::error::Error for MarkerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MarkerError::Invalid(problem) => Some(problem),
            MarkerError::Git(error) => Some(error),
            MarkerError::Io(error) => Some(error),
        }
    }
}

impl From<GitError> for MarkerError {
    fn from(error: GitError) -> Self {
        MarkerError::Git(error)
    }
}

impl From<io::Error> for MarkerError {
    fn from(error: io::Error) -> Self {
        MarkerError::Io(error)
    }
}

/// The prefetched, memoized view of one first-parent history walk (SPEC
/// 8.4-8.7, 17.5).
///
/// The walks of SPEC 8.5 (baseline selection), 8.7 (receipt scans), and
/// 5.5 (timeline) all read the same first-parent ancestry and validate the
/// markers they pass under the same rules. Reading each fact through one
/// fresh Git process per commit made a whole-history walk scale one process
/// per commit, and the rule 6 recursion re-walked the same ancestry per
/// review, so a page of the timeline cost thousands of processes (SPEC 17.5
/// forbids process-per-document scaling for the same reason). One
/// `HistoryWalk` reads the ancestry once — one `rev-list`, one
/// `cat-file --batch`, one `diff-tree --stdin` — and then answers every
/// later question from memory.
///
/// The caches are per walk and never outlive the operation that created
/// them, so a mutation cannot read a marker fact the repository no longer
/// holds: the walk is a batched view of one read of one repository state,
/// not a cache the product keeps.
pub struct HistoryWalk {
    /// The adapter every uncached read falls back to. One clone, shared
    /// with the creating adapter through the memoized object format, so a
    /// walk kept alive by a long process adds no repeated reads.
    git: Git,
    /// The rule 7 context problem, computed once per walk: shallow
    /// repositories, grafts, and replacement refs cannot change while one
    /// operation holds this view. A process failure is not cached, so the
    /// next question sees it again.
    context: Option<Option<MarkerInvalid>>,
    /// The loaded first-parent history and the commit it starts from.
    history: Option<(String, Vec<String>)>,
    /// Commit objects, prefetched in one batch and extended per read.
    commits: HashMap<String, CommitObject>,
    /// The paths each commit changed against its first parent, prefetched
    /// in one batch (rule 5).
    changed: HashMap<String, Vec<OsString>>,
    /// Scope digests by canonical descriptor input, so one walk hashes each
    /// distinct descriptor once (rule 2).
    scope_ids: HashMap<Vec<u8>, String>,
    /// Marker validation outcomes by commit. Only a decided validation is
    /// memoized: a process failure stays a failure the caller sees again.
    markers: HashMap<String, Result<ValidMarker, MarkerInvalid>>,
}

impl HistoryWalk {
    /// Starts one empty walk over `git`.
    pub fn new(git: &Git) -> HistoryWalk {
        HistoryWalk {
            git: git.clone(),
            context: None,
            history: None,
            commits: HashMap::new(),
            changed: HashMap::new(),
            scope_ids: HashMap::new(),
            markers: HashMap::new(),
        }
    }

    /// The adapter this walk falls back to.
    pub fn git(&self) -> &Git {
        &self.git
    }

    /// Forgets the rule 7 context answer, so the next validation of any
    /// commit re-reads shallow, graft, and replacement state. A caller
    /// that keeps one walk across repository inspections refreshes the
    /// context each time it observes; the content-addressed facts —
    /// commits, trees, diffs, digests — stay valid because objects never
    /// change under their identifiers.
    pub fn forget_context(&mut self) {
        self.context = None;
    }

    /// The rule 7 context problem, computed once per walk.
    fn context_problem(&mut self) -> Result<Option<MarkerInvalid>, MarkerError> {
        if let Some(cached) = &self.context {
            return Ok(cached.clone());
        }
        let fresh = context_problem(&self.git);
        if let Ok(decided) = &fresh {
            self.context = Some(decided.clone());
        }
        fresh
    }

    /// Ensures the first-parent history of `start` is loaded and returns
    /// the offset `start` sits at. A start already inside the loaded list
    /// reuses the list — the parent of any commit in a first-parent list is
    /// the next entry — so the rule 6 recursion never walks a process
    /// again.
    fn history_offset(&mut self, start: &str) -> Result<usize, MarkerError> {
        if let Some((root, list)) = &self.history {
            if root == start {
                return Ok(0);
            }
            if let Some(position) = list.iter().position(|commit| commit == start) {
                return Ok(position);
            }
        }
        let list = self
            .git
            .first_parent_history(start)
            .map_err(MarkerError::Git)?;
        // The whole list is read through two processes before any commit is
        // examined, so a walk of any length costs a fixed process count.
        let commits = self
            .git
            .read_commits_batch(&list)
            .map_err(MarkerError::Git)?;
        let changed = self
            .git
            .first_parent_changed_paths_batch(&list)
            .map_err(MarkerError::Git)?;
        self.commits.extend(commits);
        self.changed.extend(changed);
        let offset = 0;
        self.history = Some((start.to_string(), list));
        Ok(offset)
    }

    /// How many commits the loaded history holds.
    fn history_len(&self) -> usize {
        self.history
            .as_ref()
            .map(|(_, list)| list.len())
            .unwrap_or(0)
    }

    /// The commit at one offset of the loaded history.
    fn commit_at(&self, index: usize) -> String {
        self.history
            .as_ref()
            .and_then(|(_, list)| list.get(index).cloned())
            .expect("the walk only indexes inside the loaded history")
    }

    /// The loaded history as an owned list, for callers that walk it
    /// themselves (the timeline).
    fn own_history(&mut self, start: &str) -> Result<Vec<String>, MarkerError> {
        self.history_offset(start)?;
        Ok(self
            .history
            .as_ref()
            .expect("the history was just loaded")
            .1
            .clone())
    }

    /// One commit object, from the prefetch or one fresh read.
    fn commit_object(&mut self, commit: &str) -> Result<CommitObject, MarkerError> {
        if let Some(object) = self.commits.get(commit) {
            return Ok(object.clone());
        }
        let object = self.git.read_commit(commit)?;
        self.commits.insert(commit.to_string(), object.clone());
        Ok(object)
    }

    /// The scope digest of one canonical descriptor, hashed once per
    /// distinct descriptor per walk (rule 2).
    fn scope_id(&mut self, descriptor: &ScopeDescriptor) -> Result<String, MarkerError> {
        let input = descriptor.digest_input_bytes();
        if let Some(cached) = self.scope_ids.get(&input) {
            return Ok(cached.clone());
        }
        let computed = Identities::new(&self.git)
            .and_then(|identities| identities.scope(descriptor))
            .map_err(MarkerError::Git)?
            .id();
        self.scope_ids.insert(input, computed.clone());
        Ok(computed)
    }

    /// The paths `commit` changed against its first parent. A prefetched
    /// diff answers from memory; a commit outside the loaded walk falls
    /// back to one two-tree diff, which lists the same paths because a
    /// marker has exactly one parent (rule 3).
    fn changed_against_first_parent(
        &mut self,
        commit: &str,
        object: &CommitObject,
    ) -> Result<Vec<OsString>, MarkerError> {
        if let Some(paths) = self.changed.get(commit) {
            return Ok(paths.clone());
        }
        let parent = object
            .parents
            .first()
            .expect("the caller checked the commit has a parent");
        let paths = self
            .git
            .changed_between_commits(parent, commit)
            .map_err(MarkerError::Git)?;
        self.changed.insert(commit.to_string(), paths.clone());
        Ok(paths)
    }

    /// Validates one commit as a Margin marker, memoizing the decision.
    pub fn validate_marker(&mut self, commit: &str) -> Result<ValidMarker, MarkerError> {
        if let Some(cached) = self.markers.get(commit) {
            return cached.clone().map_err(MarkerError::Invalid);
        }
        let outcome = self.validate_marker_uncached(commit);
        match &outcome {
            Ok(marker) => {
                self.markers
                    .insert(commit.to_string(), Ok(marker.clone()));
            }
            Err(MarkerError::Invalid(problem)) => {
                self.markers
                    .insert(commit.to_string(), Err(problem.clone()));
            }
            // A process failure is not a decision about the commit: the
            // next question must see it again.
            Err(_) => {}
        }
        outcome
    }

    /// The uncached validation of one commit (SPEC 8.4 rules 1 to 5, 7).
    fn validate_marker_uncached(&mut self, commit: &str) -> Result<ValidMarker, MarkerError> {
        if let Some(problem) = self.context_problem()? {
            return Err(MarkerError::Invalid(problem));
        }
        let object = self.commit_object(commit)?;
        let metadata = marker_metadata(&object)?;

        // Rule 2: canonicalization of the recorded descriptor yields the
        // recorded digest, computed through Git's own hashing (SPEC 8.6).
        let descriptor: ScopeDescriptor =
            serde_json::from_str(&metadata.scope_spec).map_err(|_| {
                MarkerError::Invalid(MarkerInvalid::Trailers(TrailerProblem::InvalidValue {
                    name: domain::TRAILER_SCOPE_SPEC,
                    why: "the value is not a scope descriptor",
                }))
            })?;
        let computed = self.scope_id(&descriptor)?;
        if computed != metadata.scope_digest {
            return Err(MarkerError::Invalid(MarkerInvalid::ScopeDigestMismatch {
                trailer: metadata.scope_digest.clone(),
                computed,
            }));
        }

        // Rule 3: exactly one parent equal to the recorded candidate,
        // except a valid initial root start with `unborn`.
        let is_root = object.parents.is_empty();
        match (is_root, metadata.candidate.as_str(), metadata.kind) {
            (true, domain::CANDIDATE_UNBORN, CommitKind::Start) => {}
            (true, domain::CANDIDATE_UNBORN, _) => {
                return Err(MarkerError::Invalid(MarkerInvalid::RootNotStart));
            }
            (true, claimed, _) => {
                return Err(MarkerError::Invalid(MarkerInvalid::CandidateMismatch {
                    trailer: claimed.to_string(),
                    parent: None,
                }));
            }
            (false, _, _) => {
                if object.parents.len() > 1 {
                    return Err(MarkerError::Invalid(MarkerInvalid::ParentCount {
                        found: object.parents.len(),
                    }));
                }
                if object.parents[0] != metadata.candidate {
                    return Err(MarkerError::Invalid(MarkerInvalid::CandidateMismatch {
                        trailer: metadata.candidate.clone(),
                        parent: object.parents.first().cloned(),
                    }));
                }
            }
        }

        // Rule 4: the actual full tree equals the recorded result tree.
        if object.tree != metadata.result_tree {
            return Err(MarkerError::Invalid(MarkerInvalid::ResultTreeMismatch {
                trailer: metadata.result_tree.clone(),
                actual: object.tree.clone(),
            }));
        }

        // Rule 5: no out-of-scope change against the parent; an initial
        // root start holds only eligible scoped entries.
        if is_root {
            start_entry_problem(&self.git, &object.tree, &descriptor)?;
        } else {
            self.out_of_scope_problem(commit, &object, &descriptor)?;
        }

        Ok(ValidMarker {
            commit: commit.to_string(),
            kind: metadata.kind,
            scope: descriptor,
            scope_digest: metadata.scope_digest,
            scope_spec: metadata.scope_spec,
            candidate: metadata.candidate,
            result_tree: metadata.result_tree,
            baseline: metadata.baseline,
            request: metadata.request,
            plan: metadata.plan,
            source: metadata.source,
        })
    }

    /// Rule 5 for one non-root marker: no path the commit changed against
    /// its first parent may lie outside the marker's own scope. The
    /// prefetched per-commit diff lists exactly the paths whose tree entry
    /// (mode or object ID) differs from the parent's, which is the same set
    /// two full tree listings derive.
    fn out_of_scope_problem(
        &mut self,
        commit: &str,
        object: &CommitObject,
        descriptor: &ScopeDescriptor,
    ) -> Result<(), MarkerError> {
        let changed = self.changed_against_first_parent(commit, object)?;
        let mut outside: Vec<String> = changed
            .iter()
            .filter(|path| !descriptor.contains_path(path.as_encoded_bytes()))
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        if !outside.is_empty() {
            outside.sort();
            return Err(MarkerError::Invalid(MarkerInvalid::OutOfScopeChange {
                paths: outside,
            }));
        }
        Ok(())
    }
}

/// Validates one commit as a Margin marker (SPEC 8.4 rules 1 to 5 and 7).
///
/// The check needs no adopted scope: the marker carries its own scope, and
/// the descriptor's canonical form must yield the recorded digest. Whether
/// that scope matches the currently adopted one is a separate question the
/// baseline selection of SPEC 8.5 answers. Rule 6, that a review's B is the
/// baseline selected at C, is likewise composed with SPEC 8.5 rather than
/// decided here.
pub fn validate_marker(git: &Git, commit: &str) -> Result<ValidMarker, MarkerError> {
    HistoryWalk::new(git).validate_marker(commit)
}

/// Extracts the reserved metadata of one raw commit object (rule 1).
fn marker_metadata(object: &CommitObject) -> Result<CommitMetadata, MarkerError> {
    let trailers = domain::parse_trailer_block(&object.message)
        .map_err(|problem| MarkerError::Invalid(problem.into()))?;
    domain::reserved_metadata(&trailers).map_err(|problem| MarkerError::Invalid(problem.into()))
}

/// Returns the history-context problem of SPEC 8.4 rule 7, if any.
fn context_problem(git: &Git) -> Result<Option<MarkerInvalid>, MarkerError> {
    if git.git_path("shallow")?.exists() {
        return Ok(Some(MarkerInvalid::UnavailableContext {
            what: "shallow clone history",
        }));
    }
    let grafts = git.git_path("info/grafts")?;
    if grafts.exists()
        && fs::metadata(&grafts)
            .map(|meta| meta.len() > 0)
            .unwrap_or(false)
    {
        return Ok(Some(MarkerInvalid::UnavailableContext {
            what: "grafted history",
        }));
    }
    if !git.replace_ref_names()?.is_empty() {
        return Ok(Some(MarkerInvalid::UnavailableContext {
            what: "replacement objects",
        }));
    }
    Ok(None)
}

/// Checks rule 5 for an initial root start: its tree holds only eligible
/// scoped entries.
fn start_entry_problem(
    git: &Git,
    tree: &str,
    descriptor: &ScopeDescriptor,
) -> Result<(), MarkerError> {
    for entry in git.ls_tree(tree)? {
        let path = entry.path.to_string_lossy().into_owned();
        if !descriptor.contains_path(entry.path.as_encoded_bytes()) {
            return Err(MarkerError::Invalid(MarkerInvalid::IneligibleStartEntry {
                path,
                why: "the entry lies outside the starting scope",
            }));
        }
        if EntryMode::parse(&entry.mode).is_none() {
            return Err(MarkerError::Invalid(MarkerInvalid::IneligibleStartEntry {
                path,
                why: "the entry is not a regular file",
            }));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Baseline selection (SPEC 8.5)
// ---------------------------------------------------------------------------

/// Why the first-parent walk cannot anchor the adopted scope (SPEC 8.5).
///
/// No case may fall back to an arbitrary initial commit and label it
/// reviewed. The caller reinitializes explicitly instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BaselineUnavailable {
    /// Marker validation depends on shallow history, grafts, or replacement
    /// objects, so this repository cannot anchor anything (SPEC 8.4
    /// rule 7).
    Context {
        /// What mechanism the context depends on.
        what: &'static str,
    },
    /// A structurally valid starting marker for a different scope reset the
    /// anchor. The walk never crosses it to revive an older review, so the
    /// adopted scope needs its own new start (SPEC 8.2, 8.5).
    ForeignStart {
        /// The start commit that reset the anchor.
        commit: String,
        /// The scope digest that start records.
        scope_digest: String,
    },
    /// First-parent history holds no valid matching-scope start or review
    /// marker at all.
    Exhausted {
        /// How many commits the walk examined.
        walked: usize,
    },
}

impl fmt::Display for BaselineUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BaselineUnavailable::Context { what } => {
                write!(formatter, "the history context depends on {what}")
            }
            BaselineUnavailable::ForeignStart {
                commit,
                scope_digest,
            } => write!(
                formatter,
                "the start {commit} anchors scope {scope_digest}, and the walk does not cross it"
            ),
            BaselineUnavailable::Exhausted { walked } => write!(
                formatter,
                "{walked} first-parent commits hold no valid matching start or review marker"
            ),
        }
    }
}

impl std::error::Error for BaselineUnavailable {}

/// Failure of baseline selection (SPEC 8.5).
#[derive(Debug)]
pub enum BaselineError {
    /// The walk cannot anchor the adopted scope.
    Unavailable(BaselineUnavailable),
    /// A Git process failed.
    Git(GitError),
    /// The repository could not be inspected.
    Io(io::Error),
}

impl BaselineError {
    /// The SPEC 7.5 error code of this failure.
    pub fn public_code(&self) -> &'static str {
        match self {
            BaselineError::Unavailable(_) => "BASELINE_UNAVAILABLE",
            BaselineError::Git(error) => error.public_code(),
            BaselineError::Io(_) => "IO_FAILED",
        }
    }
}

impl fmt::Display for BaselineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BaselineError::Unavailable(reason) => write!(formatter, "{reason}"),
            BaselineError::Git(error) => write!(formatter, "{error}"),
            BaselineError::Io(error) => write!(formatter, "cannot walk history: {error}"),
        }
    }
}

impl std::error::Error for BaselineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BaselineError::Unavailable(reason) => Some(reason),
            BaselineError::Git(error) => Some(error),
            BaselineError::Io(error) => Some(error),
        }
    }
}

impl From<GitError> for BaselineError {
    fn from(error: GitError) -> Self {
        BaselineError::Git(error)
    }
}

impl From<io::Error> for BaselineError {
    fn from(error: io::Error) -> Self {
        BaselineError::Io(error)
    }
}

/// Selects the valid review baseline of `scope` at candidate `commit`
/// (SPEC 8.5).
///
/// The walk follows unfiltered first-parent history from `commit`, newest
/// first, and includes `commit` itself, so the baseline equals the
/// candidate immediately after a start or review commit. It stops at the
/// nearest structurally valid marker of the adopted scope whose kind is
/// `start` or `review`. Drafts, restores, ordinary commits, and invalid or
/// copied markers stay ordinary history and confer no approval, so the
/// walk passes them. A start of any scope resets the anchor, and the walk
/// never crosses one to revive an older review (SPEC 8.2). A review
/// anchors the scope only when the baseline it records is the one this
/// same walk selects at its own candidate, which may be that candidate
/// itself (SPEC 8.4 rule 6).
///
/// When the required history is unavailable, or no valid matching marker
/// exists, the result is [`BaselineError::Unavailable`]; the caller never
/// falls back to an arbitrary initial commit and labels it reviewed.
/// The walk of [`select_baseline`] against one full scope digest string,
/// such as the digest a recorded marker carries (SPEC 8.4 rule 2). The
/// string form is what every comparison inside the walk uses, so receipt
/// validation reuses the same walk without recomputing an identity.
impl<'g> HistoryWalk {
    /// The baseline selection walk of SPEC 8.5 from `start`, over the
    /// prefetched ancestry.
    fn select_baseline_matching(
        &mut self,
        start: &str,
        scope_id: &str,
    ) -> Result<ValidMarker, BaselineError> {
        let offset = self.history_offset(start).map_err(walk_failed)?;
        let total = self.history_len();
        let mut index = offset;
        while index < total {
            let commit = self.commit_at(index);
            let marker = match self.validate_marker(&commit) {
                Ok(marker) => marker,
                // The context this marker depends on does not exist here, so
                // no baseline exists either (SPEC 8.4 rule 7, 8.5).
                Err(MarkerError::Invalid(MarkerInvalid::UnavailableContext { what })) => {
                    return Err(BaselineError::Unavailable(BaselineUnavailable::Context {
                        what,
                    }));
                }
                // An invalid or copied marker confers no approval; the walk
                // passes it (SPEC 8.4).
                Err(MarkerError::Invalid(_)) => {
                    index += 1;
                    continue;
                }
                Err(MarkerError::Git(error)) => return Err(BaselineError::Git(error)),
                Err(MarkerError::Io(error)) => return Err(BaselineError::Io(error)),
            };
            if marker.scope_digest != scope_id {
                if marker.kind == CommitKind::Start {
                    // A start resets the anchor for its own scope. The walk
                    // stops here, so the adopted scope needs a new start
                    // rather than a review below this one (SPEC 8.2, 8.5).
                    return Err(BaselineError::Unavailable(
                        BaselineUnavailable::ForeignStart {
                            commit: marker.commit,
                            scope_digest: marker.scope_digest,
                        },
                    ));
                }
                // A non-start marker of another scope is ordinary history
                // here.
                index += 1;
                continue;
            }
            match marker.kind {
                CommitKind::Start => return Ok(marker),
                CommitKind::Review => {
                    if self.review_records_its_own_baseline(&marker, &marker.scope_digest)? {
                        return Ok(marker);
                    }
                    // The review claims a baseline that selection at its own
                    // candidate does not reproduce, so rule 6 fails for it and
                    // the walk passes it (SPEC 8.4 rule 6).
                    index += 1;
                    continue;
                }
                // A draft or a restore never anchors the scope (SPEC 9.5).
                CommitKind::Draft | CommitKind::Restore => {
                    index += 1;
                    continue;
                }
            }
        }
        Err(BaselineError::Unavailable(BaselineUnavailable::Exhausted {
            walked: total - offset,
        }))
    }

    /// Selects the baseline of the adopted scope from `candidate` (SPEC
    /// 8.5).
    pub fn select_baseline(
        &mut self,
        candidate: &str,
        scope: &ScopeDigest,
    ) -> Result<ValidMarker, BaselineError> {
        self.select_baseline_matching(candidate, &scope.id())
    }

    /// Checks SPEC 8.4 rule 6 for one review marker: the baseline it
    /// records is the one Section 8.5 selects at the review's own
    /// candidate, under the marker's own recorded scope digest.
    fn review_records_its_own_baseline(
        &mut self,
        marker: &ValidMarker,
        scope_id: &str,
    ) -> Result<bool, BaselineError> {
        // Validation already proved the recorded candidate is the single
        // parent, which is the candidate the review was selected against.
        let selected = match self.select_baseline_matching(&marker.candidate, scope_id) {
            Ok(selected) => selected,
            // The review's own ancestry supplies no anchor to compare with,
            // so its recorded baseline cannot be reproduced.
            Err(BaselineError::Unavailable(_)) => return Ok(false),
            Err(other) => return Err(other),
        };
        Ok(marker.baseline.as_deref() == Some(selected.commit.as_str()))
    }

    /// Scans the complete unfiltered first-parent ancestry of `start` for
    /// the receipt of one request (SPEC 8.7).
    pub fn find_receipt(
        &mut self,
        start: &str,
        kind: CommitKind,
        request_id: &str,
        fingerprint: &str,
    ) -> Result<ReceiptLookup, ReceiptScanError> {
        let offset = self.history_offset(start).map_err(receipt_scan_failed)?;
        let total = self.history_len();
        let mut index = offset;
        while index < total {
            let commit = self.commit_at(index);
            let object = self.commit_object(&commit).map_err(receipt_scan_failed)?;
            let carries_request = domain::parse_trailer_block(&object.message)
                .map(|trailers| {
                    trailers
                        .iter()
                        .any(|trailer| trailer.token == domain::TRAILER_REQUEST)
                })
                .unwrap_or(false);
            if carries_request {
                let marker = match self.validate_marker(&commit) {
                    Ok(marker) => marker,
                    // The context every marker of this repository depends on
                    // is unavailable (SPEC 8.4 rule 7), so no receipt can be
                    // validated and the absence of the request ID cannot be
                    // established. This is a typed unavailable outcome,
                    // never silence (SPEC 8.7).
                    Err(MarkerError::Invalid(MarkerInvalid::UnavailableContext { what })) => {
                        return Err(ReceiptScanError::UnavailableContext { what });
                    }
                    // An invalid or copied marker confers nothing, so it is
                    // not a receipt; the scan passes it (SPEC 8.4, 8.7).
                    Err(MarkerError::Invalid(_)) => {
                        index += 1;
                        continue;
                    }
                    Err(MarkerError::Git(error)) => return Err(ReceiptScanError::Git(error)),
                    Err(MarkerError::Io(error)) => return Err(ReceiptScanError::Io(error)),
                };
                if marker.kind == kind && marker.request.as_deref() == Some(request_id) {
                    // Rule 6 of SPEC 8.4 for a review receipt: the baseline
                    // it records is the one Section 8.5 selects at its own
                    // candidate. The review's ancestry supplying no anchor
                    // at all fails the rule the same way, so the marker
                    // stays ordinary history and neither answers nor
                    // conflicts, whatever fingerprint it records.
                    if marker.kind == CommitKind::Review {
                        match self
                            .review_records_its_own_baseline(&marker, &marker.scope_digest)
                        {
                            Ok(true) => {}
                            Ok(false) => {
                                index += 1;
                                continue;
                            }
                            Err(BaselineError::Git(error)) => {
                                return Err(ReceiptScanError::Git(error))
                            }
                            Err(BaselineError::Io(error)) => {
                                return Err(ReceiptScanError::Io(error))
                            }
                            Err(BaselineError::Unavailable(_)) => {
                                index += 1;
                                continue;
                            }
                        }
                    }
                    return Ok(if marker.plan.as_deref() == Some(fingerprint) {
                        ReceiptLookup::Found(Box::new(marker))
                    } else {
                        ReceiptLookup::Conflict {
                            commit: marker.commit.clone(),
                            recorded: marker.plan.clone(),
                            requested: fingerprint.to_string(),
                        }
                    });
                }
            }
            index += 1;
        }
        Ok(ReceiptLookup::Absent)
    }

    /// Finds the newest valid receipt the first-parent history of `start`
    /// holds for the adopted scope (SPEC 8.7, 9.7): the answer `doctor`
    /// gives to whether a request already completed.
    ///
    /// The scan is the same walk [`HistoryWalk::find_receipt`] runs — the
    /// complete unfiltered first-parent ancestry, newest first, with no
    /// pathspec, no presentation limit, and no early stop — but it matches
    /// any request, because the recovery inspection holds no request
    /// identifier of its own. A commit that carries no
    /// [`domain::TRAILER_REQUEST`] cannot be a receipt; one that does is
    /// validated whole under every rule of SPEC 8.4, including rule 6 for a
    /// review, so an invalid or copied marker stays ordinary history and
    /// the scan passes it. A receipt of another scope stays history too.
    pub fn find_latest_receipt(
        &mut self,
        start: &str,
        scope_digest: &str,
    ) -> Result<ReceiptPresence, ReceiptScanError> {
        let offset = self.history_offset(start).map_err(receipt_scan_failed)?;
        let total = self.history_len();
        let mut index = offset;
        while index < total {
            let commit = self.commit_at(index);
            let object = self.commit_object(&commit).map_err(receipt_scan_failed)?;
            let carries_request = domain::parse_trailer_block(&object.message)
                .map(|trailers| {
                    trailers
                        .iter()
                        .any(|trailer| trailer.token == domain::TRAILER_REQUEST)
                })
                .unwrap_or(false);
            if carries_request {
                let marker = match self.validate_marker(&commit) {
                    Ok(marker) => marker,
                    // The context every marker of this repository depends on
                    // is unavailable (SPEC 8.4 rule 7), so no receipt can be
                    // validated and the absence of a receipt is not
                    // established. This is one unavailable finding, never a
                    // quiet `Absent` (SPEC 8.7).
                    Err(MarkerError::Invalid(MarkerInvalid::UnavailableContext { what })) => {
                        return Ok(ReceiptPresence::Unavailable { what });
                    }
                    // An invalid or copied marker confers nothing, so it is
                    // not a receipt; the scan passes it (SPEC 8.4, 8.7).
                    Err(MarkerError::Invalid(_)) => {
                        index += 1;
                        continue;
                    }
                    Err(MarkerError::Git(error)) => return Err(ReceiptScanError::Git(error)),
                    Err(MarkerError::Io(error)) => return Err(ReceiptScanError::Io(error)),
                };
                if marker.scope_digest != scope_digest {
                    index += 1;
                    continue;
                }
                // Rule 6 of SPEC 8.4 for a review receipt: the baseline it
                // records is the one Section 8.5 selects at its own
                // candidate. A review that fails the rule stays ordinary
                // history and is no receipt, whatever request it carries.
                if marker.kind == CommitKind::Review {
                    match self.review_records_its_own_baseline(&marker, &marker.scope_digest) {
                        Ok(true) => {}
                        Ok(false) => {
                            index += 1;
                            continue;
                        }
                        Err(BaselineError::Git(error)) => {
                            return Err(ReceiptScanError::Git(error))
                        }
                        Err(BaselineError::Io(error)) => {
                            return Err(ReceiptScanError::Io(error))
                        }
                        Err(BaselineError::Unavailable(_)) => {
                            index += 1;
                            continue;
                        }
                    }
                }
                return Ok(ReceiptPresence::Present(Box::new(ReceiptFinding {
                    commit: marker.commit.clone(),
                    kind: marker.kind,
                    scope_digest: marker.scope_digest.clone(),
                    request: marker.request.clone().unwrap_or_default(),
                    plan: marker.plan.clone(),
                })));
            }
            index += 1;
        }
        Ok(ReceiptPresence::Absent)
    }
}

/// The baseline error of a history the walk could not read.
fn walk_failed(error: MarkerError) -> BaselineError {
    match error {
        MarkerError::Git(inner) => BaselineError::Git(inner),
        MarkerError::Io(inner) => BaselineError::Io(inner),
        MarkerError::Invalid(problem) => {
            unreachable!("a history read cannot decide a marker: {problem}")
        }
    }
}

/// The receipt-scan error of a history the walk could not read.
fn receipt_scan_failed(error: MarkerError) -> ReceiptScanError {
    match error {
        MarkerError::Git(inner) => ReceiptScanError::Git(inner),
        MarkerError::Io(inner) => ReceiptScanError::Io(inner),
        MarkerError::Invalid(problem) => {
            unreachable!("a history read cannot decide a marker: {problem}")
        }
    }
}

/// Selects the baseline of the adopted scope (SPEC 8.5) through one
/// prefetched history walk.
pub fn select_baseline(
    git: &Git,
    candidate: &str,
    scope: &ScopeDigest,
) -> Result<ValidMarker, BaselineError> {
    HistoryWalk::new(git).select_baseline(candidate, scope)
}

// ---------------------------------------------------------------------------
// Request receipts (SPEC 8.7)
// ---------------------------------------------------------------------------

/// The outcome of one receipt scan (SPEC 8.7).
///
/// A request is one `(kind, request ID, plan fingerprint)` triple, because a
/// request identifier is scoped to the reachable adopted history and the
/// operation kind (SPEC 7.3). Only a valid reachable receipt of the same
/// kind that carries the same request ID answers a scan. An invalid or
/// copied marker stays ordinary history and confers nothing, exactly as in
/// baseline selection (SPEC 8.4).
#[derive(Clone, Debug)]
pub enum ReceiptLookup {
    /// The request already holds one valid reachable receipt. The marker is
    /// the verified commit an exact retry returns as its `existing_receipt`.
    /// It stays boxed, because the common `Absent` return stays small.
    Found(Box<ValidMarker>),
    /// A valid reachable receipt of this kind carries the same request ID
    /// with a different fingerprint, so the identifier names two different
    /// plans (SPEC 7.3, 8.7).
    Conflict {
        /// The receipt commit that holds the request ID.
        commit: String,
        /// The `Margin-Plan` fingerprint that receipt records.
        recorded: Option<String>,
        /// The fingerprint the retried request carries.
        requested: String,
    },
    /// The scan completed over the whole unfiltered first-parent history and
    /// no valid reachable receipt holds this request ID.
    Absent,
}

/// Failure of one receipt scan (SPEC 8.7): the scan could not complete, so
/// the absence of the request ID is not established, and a new
/// receipt-producing mutation stays blocked instead of guessing.
#[derive(Debug)]
pub enum ReceiptScanError {
    /// A Git process of the scan failed, for example a timeout or an output
    /// limit reached before the walk listed the whole history.
    Git(GitError),
    /// The repository could not be inspected.
    Io(io::Error),
    /// The history context every marker depends on is unavailable — shallow
    /// history, grafts, or replacement objects (SPEC 8.4 rule 7) — so no
    /// receipt of this repository can be validated at all. This outcome is
    /// not the absence of the request ID; the mutation stays blocked.
    UnavailableContext {
        /// What mechanism the context depends on.
        what: &'static str,
    },
}

impl ReceiptScanError {
    /// The SPEC 7.5 error code of this failure.
    pub fn public_code(&self) -> &'static str {
        match self {
            ReceiptScanError::Git(error) => error.public_code(),
            ReceiptScanError::Io(_) => "IO_FAILED",
            ReceiptScanError::UnavailableContext { .. } => "BASELINE_UNAVAILABLE",
        }
    }

    /// Says whether repeating the identical scan can be useful and safe.
    pub fn retryable(&self) -> bool {
        matches!(self, ReceiptScanError::Git(error) if error.retryable())
    }
}

impl fmt::Display for ReceiptScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReceiptScanError::Git(error) => write!(formatter, "{error}"),
            ReceiptScanError::Io(error) => write!(formatter, "cannot scan history: {error}"),
            ReceiptScanError::UnavailableContext { what } => {
                write!(formatter, "the history context depends on {what}")
            }
        }
    }
}

impl std::error::Error for ReceiptScanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ReceiptScanError::Git(error) => Some(error),
            ReceiptScanError::Io(error) => Some(error),
            ReceiptScanError::UnavailableContext { .. } => None,
        }
    }
}

impl From<GitError> for ReceiptScanError {
    fn from(error: GitError) -> Self {
        ReceiptScanError::Git(error)
    }
}

impl From<io::Error> for ReceiptScanError {
    fn from(error: io::Error) -> Self {
        ReceiptScanError::Io(error)
    }
}

/// Finds the receipt of one request in the reachable history of `start`
/// (SPEC 8.7).
///
/// The search domain is the complete unfiltered first-parent ancestry of
/// `start`, newest first, with no pathspec, no presentation limit, and no
/// early stop: an empty review receipt stays in the walk a path-limited log
/// would hide, and a start marker of another scope does not end it, because
/// a receipt is a durable record of one completed request, never approval
/// the baseline rules of SPEC 8.5 would revive. The capture bound still
/// applies: a truncated listing is one typed failure that blocks the
/// mutation, never a silent claim that the request ID is absent.
///
/// Every commit of the walk is read once before any full validation: a
/// message without a parsable trailer block that carries
/// [`domain::TRAILER_REQUEST`] cannot be a receipt, whatever else it is. A
/// commit that does carry the trailer is validated whole, because only a
/// marker that satisfies every rule of SPEC 8.4 counts as a receipt,
/// including rule 6 for a review: the baseline it records must be the one
/// Section 8.5 selects at the review's own candidate, so a structurally
/// perfect review that names another B stays ordinary history and confers
/// no receipt. A repository whose history context rule 7 cannot support is
/// one typed unavailable outcome, distinct from an absent receipt in the
/// same way an incomplete scan is (SPEC 8.7). `start` must be a validated
/// canonical object ID from the adapter, never a user string (SPEC 14.1).
pub fn find_receipt(
    git: &Git,
    start: &str,
    kind: CommitKind,
    request_id: &str,
    fingerprint: &str,
) -> Result<ReceiptLookup, ReceiptScanError> {
    HistoryWalk::new(git).find_receipt(start, kind, request_id, fingerprint)
}

/// Finds the newest valid receipt the first-parent history of `start` holds
/// for the adopted scope (SPEC 8.7, 9.7): the answer `doctor` gives to
/// whether a request already completed.
///
/// The scan is the same walk [`find_receipt`] runs — the complete unfiltered
/// first-parent ancestry, newest first, with no pathspec, no presentation
/// limit, and no early stop — but it matches any request, because the
/// recovery inspection holds no request identifier of its own. A commit that
/// carries no [`domain::TRAILER_REQUEST`] cannot be a receipt; one that does
/// is validated whole under every rule of SPEC 8.4, including rule 6 for a
/// review, so an invalid or copied marker stays ordinary history and the
/// scan passes it. A receipt of another scope stays history too: the report
/// explains one workspace, and the walk of that workspace never crosses into
/// the receipts of a different comparison. The context rule 7 excludes ends
/// the scan as one unavailable finding, because the empty remainder cannot
/// prove that no receipt exists (SPEC 8.7). `start` must be a validated
/// canonical object ID from the adapter, never a user string (SPEC 14.1).
pub fn find_latest_receipt(
    git: &Git,
    start: &str,
    scope_digest: &str,
) -> Result<ReceiptPresence, ReceiptScanError> {
    HistoryWalk::new(git).find_latest_receipt(start, scope_digest)
}

/// The typed refusal when one request ID already names another plan
/// (SPEC 7.3, 8.7): the identifier is scoped to the reachable history and
/// the operation kind, so reusing it under a different fingerprint is a
/// conflict the writer must resolve with a new request, never a silent
/// second meaning. Stale plans require new inspection.
fn request_id_conflict(
    request_id: &str,
    commit: &str,
    recorded: &Option<String>,
) -> OperationError {
    OperationError::new(
        "REQUEST_ID_CONFLICT",
        format!(
            "the request ID {request_id} already names another plan: the receipt {commit} \
             records the fingerprint {}",
            recorded.as_deref().unwrap_or("none"),
        ),
    )
    .recovery(&["margin", "diff", "--json"])
}

// ---------------------------------------------------------------------------
// The editorial timeline (SPEC 5.5, 6.3)
// ---------------------------------------------------------------------------

/// Largest number of characters one timeline subject carries. Subjects are
/// display data, never a stream; the bound keeps one page bounded whatever
/// a commit message holds (SPEC 7.6).
const MAX_TIMELINE_SUBJECT_CHARS: usize = 200;

/// Largest number of characters one ignored-marker reason carries. The
/// reason names why a claimed marker confers nothing; beyond the bound it
/// truncates, because it is a bounded diagnostic, not evidence (SPEC 8.4,
/// 7.6).
const MAX_TIMELINE_REASON_CHARS: usize = 500;

/// One commit of the walk, classified before any presentation decision.
struct ClassifiedCommit {
    /// The commit object ID.
    commit: String,
    /// The validated kind, the claimed kind of an ignored marker, or
    /// ordinary history.
    kind: TimelineKind,
    /// The validated marker, when validation succeeded.
    marker: Option<ValidMarker>,
    /// Why a claimed marker was ignored.
    ignored_reason: Option<String>,
    /// The bounded subject line.
    subject: String,
    /// Whether the subject bound dropped characters.
    subject_truncated: bool,
    /// The committer timestamp in seconds, when the header parses.
    committer_time: Option<i64>,
    /// Whether the commit changed one of the filter roots; `None` means no
    /// filter was supplied, so every commit matches.
    matches_filter: Option<bool>,
}

impl ClassifiedCommit {
    /// Whether the presentation filter keeps this commit.
    fn matches(&self) -> bool {
        self.matches_filter.unwrap_or(true)
    }
}

/// Classifies one commit of the timeline (SPEC 5.5, 8.4).
///
/// The commit is read once before any validation, and a message whose
/// trailer block carries no [`domain::TRAILER_KIND`] claims nothing, so an
/// ordinary external commit costs no validation at all — the same prefilter
/// the receipt scan uses (SPEC 8.7). A commit that does claim a kind is
/// validated whole through [`validate_marker`], because the label of the
/// timeline is the meaning of a marker and only the marker rules of SPEC
/// 8.4 decide that; a review additionally needs rule 6, the baseline its
/// own candidate selects, exactly as the receipt scan requires. A claim
/// that fails keeps its kind and carries the reason it was ignored: invalid
/// or copied markers remain ordinary Git history and confer no approval,
/// and the timeline surfaces why they were ignored instead of presenting
/// them as events that happened (SPEC 8.4).
///
/// With a path filter the classifier also computes presentation membership:
/// the paths the commit changed against its first parent — its whole tree
/// for an initial root commit — matched against the filter roots by the
/// same component rule every presentation filter uses. The walk itself
/// stays unfiltered; an empty review commit a path-limited Git log would
/// hide stays in the timeline (SPEC 8.5).
fn classify_timeline_commit(
    walk: &mut HistoryWalk,
    commit: &str,
    path_filter: &[String],
) -> Result<ClassifiedCommit, OperationError> {
    let object = walk
        .commit_object(commit)
        .map_err(|error| history_walk_failed(error))?;
    let claimed = domain::parse_trailer_block(&object.message)
        .ok()
        .and_then(|trailers| {
            trailers
                .iter()
                .find(|trailer| trailer.token == domain::TRAILER_KIND)
                .and_then(|trailer| CommitKind::parse(&trailer.value))
        });
    let (kind, marker, ignored_reason) = match claimed {
        None => (TimelineKind::Ordinary, None, None),
        Some(_) => match walk.validate_marker(commit) {
            Ok(valid) => match timeline_review_verdict(walk, &valid)? {
                Some(reason) => (TimelineKind::Review, None, Some(reason)),
                None => (timeline_kind_of(valid.kind), Some(valid), None),
            },
            Err(MarkerError::Invalid(problem)) => (
                timeline_kind_of_claimed(&object.message),
                None,
                Some(problem.to_string()),
            ),
            Err(MarkerError::Git(error)) => return Err(OperationError::from(error)),
            Err(MarkerError::Io(error)) => return Err(history_inspection_failed(error)),
        },
    };
    let (subject, subject_truncated) = timeline_subject(&object.message);
    let matches_filter = if path_filter.is_empty() {
        None
    } else {
        Some(
            timeline_changed_paths(walk.git(), commit, &object)?
                .iter()
                .any(|path| passes_filter(path, path_filter)),
        )
    };
    Ok(ClassifiedCommit {
        commit: commit.to_string(),
        kind,
        marker,
        ignored_reason,
        subject,
        subject_truncated,
        committer_time: object.committer_time,
        matches_filter,
    })
}

/// The timeline label of one validated marker kind.
fn timeline_kind_of(kind: CommitKind) -> TimelineKind {
    match kind {
        CommitKind::Start => TimelineKind::Start,
        CommitKind::Draft => TimelineKind::Draft,
        CommitKind::Review => TimelineKind::Review,
        CommitKind::Restore => TimelineKind::Restore,
    }
}

/// The claimed kind of a message whose marker validation failed, for the
/// label an ignored entry keeps. A claim that no longer parses names no
/// kind, so the commit stays ordinary history.
fn timeline_kind_of_claimed(message: &[u8]) -> TimelineKind {
    domain::parse_trailer_block(message)
        .ok()
        .and_then(|trailers| {
            trailers
                .iter()
                .find(|trailer| trailer.token == domain::TRAILER_KIND)
                .and_then(|trailer| CommitKind::parse(&trailer.value))
        })
        .map_or(TimelineKind::Ordinary, timeline_kind_of)
}

/// Checks rule 6 of SPEC 8.4 for one validated review marker of the
/// timeline: the baseline it records is the one Section 8.5 selects at its
/// own candidate. `Some(reason)` means the review confers nothing and the
/// entry carries why; a process failure stays a typed operation failure.
fn timeline_review_verdict(
    walk: &mut HistoryWalk,
    marker: &ValidMarker,
) -> Result<Option<String>, OperationError> {
    if marker.kind != CommitKind::Review {
        return Ok(None);
    }
    match walk.review_records_its_own_baseline(marker, &marker.scope_digest) {
        Ok(true) => Ok(None),
        Ok(false) => Ok(Some(
            "the review records a baseline its own candidate does not select".to_string(),
        )),
        Err(BaselineError::Unavailable(_)) => Ok(Some(
            "the ancestry of the review supplies no anchor its recorded baseline could \
             reproduce"
                .to_string(),
        )),
        Err(BaselineError::Git(error)) => Err(OperationError::from(error)),
        Err(BaselineError::Io(error)) => Err(history_inspection_failed(error)),
    }
}

/// One repository-inspection failure of the timeline walk, as the typed
/// execution failure the envelope requires.
fn history_inspection_failed(error: io::Error) -> OperationError {
    OperationError::new("IO_FAILED", format!("cannot inspect history: {error}"))
}

/// One process failure of the prefetched history walk, as the typed
/// execution failure the envelope requires.
fn history_walk_failed(error: MarkerError) -> OperationError {
    match error {
        MarkerError::Git(inner) => OperationError::from(inner),
        MarkerError::Io(inner) => history_inspection_failed(inner),
        MarkerError::Invalid(problem) => OperationError::new(
            problem.public_code(),
            format!("the history walk rejected a commit: {problem}"),
        ),
    }
}

/// The bounded subject line of one commit message: the first line, capped
/// at [`MAX_TIMELINE_SUBJECT_CHARS`] characters with the truncation stated,
/// never a silent cut.
fn timeline_subject(message: &[u8]) -> (String, bool) {
    let text = String::from_utf8_lossy(message);
    let first = text.split('\n').next().unwrap_or("");
    if first.chars().count() <= MAX_TIMELINE_SUBJECT_CHARS {
        return (first.to_string(), false);
    }
    let mut bounded: String = first.chars().take(MAX_TIMELINE_SUBJECT_CHARS - 1).collect();
    bounded.push('\u{2026}');
    (bounded, true)
}

/// Bounds one ignored-marker reason the same way.
fn bounded_timeline_reason(reason: String) -> String {
    if reason.chars().count() <= MAX_TIMELINE_REASON_CHARS {
        return reason;
    }
    let mut bounded: String = reason.chars().take(MAX_TIMELINE_REASON_CHARS - 1).collect();
    bounded.push('\u{2026}');
    bounded
}

/// The paths one timeline commit changed against its first parent — its
/// whole tree for an initial root commit — for the presentation filter
/// only. The listing runs through the same bounded adapter call the change
/// manifest uses; it never narrows the walk itself.
fn timeline_changed_paths(
    git: &Git,
    commit: &str,
    object: &CommitObject,
) -> Result<Vec<OsString>, OperationError> {
    match object.parents.first() {
        Some(parent) => git
            .changed_between_commits(parent, commit)
            .map_err(OperationError::from),
        None => Ok(git
            .ls_tree(commit)
            .map_err(OperationError::from)?
            .into_iter()
            .map(|entry| entry.path)
            .collect()),
    }
}

/// One writing pass under construction: a run of consecutive validated
/// draft markers collapsing into one entry (SPEC 5.5). The entry carries
/// the newest checkpoint's facts and the count of the whole run; the
/// checkpoints themselves stay reachable in Git and list with
/// `--all-checkpoints`.
struct DraftRun {
    /// The newest checkpoint of the run, whose subject and marker the
    /// collapsed entry carries.
    newest: ClassifiedCommit,
    /// How many checkpoints the run covers.
    count: u64,
    /// Whether any checkpoint changed one of the filter roots.
    matched: bool,
}

impl DraftRun {
    /// Opens one run at its first — newest — checkpoint.
    fn new(newest: ClassifiedCommit) -> DraftRun {
        let matched = newest.matches();
        DraftRun {
            newest,
            count: 1,
            matched,
        }
    }

    /// Extends the run by one older checkpoint.
    fn extend(&mut self, older: ClassifiedCommit) {
        self.matched |= older.matches();
        self.count += 1;
    }
}

/// One selected timeline page.
struct TimelinePage {
    /// The entries of this page, newest first in ancestry order.
    entries: Vec<LogEntry>,
    /// Whether the timeline holds no further entry.
    complete: bool,
    /// The cursor of the next page; `None` when this page is complete.
    next_cursor: Option<String>,
}

/// Builds one page of the editorial timeline (SPEC 5.5, 6.3).
///
/// The page is cut from the entry sequence the contract produces — one
/// entry per commit, with consecutive validated drafts collapsed into one
/// writing pass unless the request lists every checkpoint — in Git
/// ancestry order, newest first, never clock order. The walk reads only as
/// far as one entry beyond the page needs, so a short displayed log costs
/// no whole-history scan and makes no old baseline disappear: marker
/// discovery lives in the unfiltered walks of SPEC 8.5 and 8.7, not here
/// (SPEC 6.3).
///
/// A path filter removes entries from the display only. A collapsed pass
/// survives the filter when any of its checkpoints changed one of the
/// roots; the cursor of the next page binds the filter, so paging a
/// filtered timeline stays consistent. A cursor that names a position this
/// timeline no longer holds — or that belongs to another walk, scope,
/// policy, filter, or limit — is one `STALE_CURSOR` refusal.
fn timeline_page(
    walk: &mut HistoryWalk,
    history: &[String],
    contract: &domain::LogCursorContext,
    start: u64,
    limit: u64,
) -> Result<TimelinePage, OperationError> {
    // One entry beyond the page decides completeness; more are never read.
    let needed = start.saturating_add(limit).saturating_add(1);
    let mut entries: Vec<LogEntry> = Vec::new();
    let mut run: Option<DraftRun> = None;
    for commit in history {
        if entries.len() as u64 >= needed {
            break;
        }
        let classified = classify_timeline_commit(walk, commit, &contract.path_filter)?;
        let collapses = !contract.all_checkpoints
            && classified
                .marker
                .as_ref()
                .is_some_and(|marker| marker.kind == CommitKind::Draft);
        if collapses {
            match &mut run {
                Some(open) => open.extend(classified),
                None => run = Some(DraftRun::new(classified)),
            }
            continue;
        }
        // Any other commit ends an open writing pass first, so every entry
        // of the page below is final when the loop stops.
        if let Some(open) = run.take()
            && open.matched
        {
            entries.push(timeline_entry(open.newest, open.count));
        }
        if classified.matches() {
            entries.push(timeline_entry(classified, 1));
        }
    }
    if let Some(open) = run.take()
        && open.matched
    {
        entries.push(timeline_entry(open.newest, open.count));
    }
    let total = entries.len() as u64;
    if start > 0 && total <= start {
        return Err(OperationError::new(
            "STALE_CURSOR",
            "the cursor names a position this timeline no longer holds; rerun the query \
             without a cursor for a fresh page",
        )
        .recovery(&["margin", "log", "--json"]));
    }
    let complete = total < needed;
    let page_end = start.saturating_add(limit).min(total) as usize;
    let page = entries.drain(start as usize..page_end).collect();
    let next_cursor = if complete {
        None
    } else {
        let next = start.saturating_add(limit);
        let mut claimed = contract.clone();
        claimed.offset = next;
        let digest = Identities::new(walk.git())
            .and_then(|identities| identities.log_cursor(&claimed))
            .map_err(OperationError::from)?;
        Some(format!("{next}-{}", digest.id()))
    };
    Ok(TimelinePage {
        entries: page,
        complete,
        next_cursor,
    })
}

/// Builds the timeline entry of one classified commit covering
/// `checkpoints` checkpoints.
fn timeline_entry(classified: ClassifiedCommit, checkpoints: u64) -> LogEntry {
    let marker = classified.marker.map(|marker| MarkerSummary {
        scope_digest: marker.scope_digest,
        baseline: marker.baseline,
        request: marker.request,
        plan: marker.plan,
        source: marker.source,
    });
    LogEntry {
        commit: classified.commit,
        kind: classified.kind,
        subject: classified.subject,
        subject_truncated: classified.subject_truncated,
        committer_time: classified.committer_time,
        checkpoints,
        marker,
        ignored_reason: classified.ignored_reason.map(bounded_timeline_reason),
    }
}

/// Verifies one timeline cursor against the digest of the current walk and
/// page contract (SPEC 6.3). Anything that does not match — another head,
/// scope, checkpoint policy, filter, or limit, a hand-edited offset or
/// digest — is one `STALE_CURSOR` refusal. Whether the position itself is
/// still held is decided by the walk that reads the page.
fn verify_log_cursor(
    git: &Git,
    cursor: &str,
    contract: &domain::LogCursorContext,
) -> Result<u64, OperationError> {
    let stale = |what: &str| {
        OperationError::new(
            "STALE_CURSOR",
            format!("the cursor {what}; rerun the query without a cursor for a fresh page"),
        )
        .recovery(&["margin", "log", "--json"])
    };
    let Some((offset, id)) = cursor.rsplit_once('-') else {
        return Err(stale("is malformed"));
    };
    let Ok(offset) = offset.parse::<u64>() else {
        return Err(stale("is malformed"));
    };
    // The digest covers the claimed position, so an edited offset demands a
    // digest its editor cannot compute (SPEC 6.3).
    let mut claimed = contract.clone();
    claimed.offset = offset;
    let digest = Identities::new(git)
        .and_then(|identities| identities.log_cursor(&claimed))
        .map_err(OperationError::from)?;
    if id != digest.id() {
        return Err(stale(
            "belongs to another timeline or page contract than this walk",
        ));
    }
    Ok(offset)
}

/// One reserved value that differs between the prepared metadata map and the
/// committed marker (SPEC 8.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetadataDifference {
    /// The reserved trailer name, or `parent` or `tree` for the structural
    /// comparisons.
    pub field: &'static str,
    /// The value prepared before the Git commit.
    pub expected: String,
    /// The value observed in history.
    pub actual: String,
}

/// The post-commit verification report of one marker commit whose observed
/// state did not match the prepared intent.
#[derive(Clone, Debug)]
pub struct MarkerVerification {
    /// Structural problems found by marker validation.
    pub problems: Vec<MarkerInvalid>,
    /// Reserved values that differ from the prepared map.
    pub differences: Vec<MetadataDifference>,
}

/// Failure of one marker commit, from an invalid prepared map through a
/// verified commit.
#[derive(Debug)]
pub enum MarkerCommitError {
    /// The prepared metadata map or message text is invalid.
    Invalid(domain::MetadataProblem),
    /// A Git process failed, including a hook that rejected the commit.
    Git(GitError),
    /// A temporary file could not be written.
    Io(io::Error),
    /// The real index tree differs from the intended result tree, so no
    /// commit was attempted.
    IndexTreeDiffers {
        /// The tree the index holds.
        observed: String,
        /// The tree the prepared map records.
        intended: String,
    },
    /// The commit exists but does not match the prepared map. It remains
    /// ordinary history and confers no approval (SPEC 9.6).
    Verification {
        /// The commit object ID that was created.
        commit: String,
        /// What did not match.
        report: MarkerVerification,
    },
}

impl MarkerCommitError {
    /// The SPEC 7.5 error code of this failure.
    pub fn public_code(&self) -> &'static str {
        match self {
            MarkerCommitError::Invalid(_) => "INVALID_ARGUMENT",
            MarkerCommitError::Git(error) => error.public_code(),
            MarkerCommitError::Io(_) => "IO_FAILED",
            MarkerCommitError::IndexTreeDiffers { .. } => "DIRTY_SCOPE",
            MarkerCommitError::Verification { .. } => "CONTRACT_VIOLATION",
        }
    }
}

impl fmt::Display for MarkerCommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MarkerCommitError::Invalid(problem) => write!(formatter, "{problem}"),
            MarkerCommitError::Git(error) => write!(formatter, "{error}"),
            MarkerCommitError::Io(error) => {
                write!(formatter, "cannot write the message file: {error}")
            }
            MarkerCommitError::IndexTreeDiffers { observed, intended } => write!(
                formatter,
                "the index tree is {observed} but the intended result tree is {intended}"
            ),
            MarkerCommitError::Verification { commit, report } => {
                writeln!(
                    formatter,
                    "the commit {commit} does not match its prepared metadata"
                )?;
                for problem in &report.problems {
                    writeln!(formatter, "- {problem}")?;
                }
                for difference in &report.differences {
                    writeln!(
                        formatter,
                        "- {}: expected {}, found {}",
                        difference.field, difference.expected, difference.actual
                    )?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for MarkerCommitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MarkerCommitError::Invalid(problem) => Some(problem),
            MarkerCommitError::Git(error) => Some(error),
            MarkerCommitError::Io(error) => Some(error),
            MarkerCommitError::IndexTreeDiffers { .. } => None,
            MarkerCommitError::Verification { .. } => None,
        }
    }
}

impl From<GitError> for MarkerCommitError {
    fn from(error: GitError) -> Self {
        MarkerCommitError::Git(error)
    }
}

impl From<io::Error> for MarkerCommitError {
    fn from(error: io::Error) -> Self {
        MarkerCommitError::Io(error)
    }
}

/// Commits the current index as one Margin marker (SPEC 8.3, 9.2).
///
/// The caller stages the intended content first; this operation only
/// records and verifies metadata. It proceeds in the order the trust model
/// requires:
///
/// 1. Validate the prepared map and assemble the exact message bytes, so an
///    explicit message can never inject reserved trailers.
/// 2. Verify the real index tree equals the intended `Margin-Result-Tree`.
/// 3. Commit through normal `git commit` with the exact message file. Hooks
///    stay in force and may reject or rewrite the commit.
/// 4. Read the actual commit back and compare every reserved trailer, the
///    parent, and the tree against the prepared map. Self-consistency of a
///    hook-rewritten marker with its own commit is not enough (SPEC 8.4).
///
/// On a verification failure the commit stays in history and the error
/// carries its object ID; nothing is deleted and no approval is conferred.
pub fn commit_marker(
    git: &Git,
    metadata: &CommitMetadata,
    text: &str,
) -> Result<ValidMarker, MarkerCommitError> {
    let message = metadata
        .commit_message(text)
        .map_err(MarkerCommitError::Invalid)?;
    let index_tree = git.write_tree()?;
    if index_tree != metadata.result_tree {
        return Err(MarkerCommitError::IndexTreeDiffers {
            observed: index_tree,
            intended: metadata.result_tree.clone(),
        });
    }

    let message_file = reserve_temp_file("message")?;
    let committed = fs::write(&message_file, &message)
        .map_err(MarkerCommitError::from)
        .and_then(|()| {
            git.commit_index(&message_file)
                .map_err(MarkerCommitError::from)
        });
    // The disposable message file never outlives the operation.
    let _ = fs::remove_file(&message_file);
    let commit = committed?;

    match validate_marker(git, &commit) {
        Ok(marker) => {
            let differences = intent_differences(&marker, metadata);
            if differences.is_empty() {
                Ok(marker)
            } else {
                Err(MarkerCommitError::Verification {
                    commit,
                    report: MarkerVerification {
                        problems: Vec::new(),
                        differences,
                    },
                })
            }
        }
        Err(MarkerError::Invalid(problem)) => Err(MarkerCommitError::Verification {
            commit,
            report: MarkerVerification {
                problems: vec![problem],
                differences: Vec::new(),
            },
        }),
        Err(other) => Err(MarkerCommitError::Git(match other {
            MarkerError::Git(error) => error,
            MarkerError::Io(error) => {
                return Err(MarkerCommitError::Io(error));
            }
            MarkerError::Invalid(_) => unreachable!("the match arm above took it"),
        })),
    }
}

/// Compares one validated marker against the prepared map, field by field.
fn intent_differences(marker: &ValidMarker, expected: &CommitMetadata) -> Vec<MetadataDifference> {
    let mut differences = Vec::new();
    let mut record = |field: &'static str, expected: &str, actual: &str| {
        if expected != actual {
            differences.push(MetadataDifference {
                field,
                expected: expected.to_string(),
                actual: actual.to_string(),
            });
        }
    };
    record(
        domain::TRAILER_KIND,
        expected.kind.as_str(),
        marker.kind.as_str(),
    );
    record(
        domain::TRAILER_SCOPE,
        &expected.scope_digest,
        &marker.scope_digest,
    );
    record(
        domain::TRAILER_SCOPE_SPEC,
        &expected.scope_spec,
        &marker.scope_spec,
    );
    record(
        domain::TRAILER_CANDIDATE,
        &expected.candidate,
        &marker.candidate,
    );
    record(
        domain::TRAILER_RESULT_TREE,
        &expected.result_tree,
        &marker.result_tree,
    );
    record(
        domain::TRAILER_BASELINE,
        expected.baseline.as_deref().unwrap_or(""),
        marker.baseline.as_deref().unwrap_or(""),
    );
    record(
        domain::TRAILER_REQUEST,
        expected.request.as_deref().unwrap_or(""),
        marker.request.as_deref().unwrap_or(""),
    );
    record(
        domain::TRAILER_PLAN,
        expected.plan.as_deref().unwrap_or(""),
        marker.plan.as_deref().unwrap_or(""),
    );
    record(
        domain::TRAILER_SOURCE,
        expected
            .source
            .as_ref()
            .map_or_else(String::new, RestoreSource::canonical_line)
            .as_str(),
        marker
            .source
            .as_ref()
            .map_or_else(String::new, RestoreSource::canonical_line)
            .as_str(),
    );
    differences
}

// ---------------------------------------------------------------------------
// Shared operations and effect reporting (SPEC 7.1, 13.2)
// ---------------------------------------------------------------------------

/// What one operation observed about the working tree, the index, the local
/// configuration, or the hook integration (SPEC 7.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservedEffect {
    /// The operation left the state as it found it.
    Unchanged,
    /// The operation changed the state.
    Changed,
    /// The operation can no longer say what happened to the state, for
    /// example after a timeout or a lost response. An unknown observation
    /// is never narrowed back to a known one.
    Unknown,
}

impl ObservedEffect {
    /// The JSON envelope value of this observation (SPEC 7.1).
    pub fn as_str(self) -> &'static str {
        match self {
            ObservedEffect::Unchanged => "unchanged",
            ObservedEffect::Changed => "changed",
            ObservedEffect::Unknown => "unknown",
        }
    }
}

/// What one operation observed about commit history (SPEC 7.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitEffect {
    /// The operation created no commit.
    None,
    /// The operation created one commit, named by `commit_oid`.
    Created,
    /// The operation matched an earlier receipt commit instead of writing a
    /// new one (SPEC 8.7), named by `commit_oid`.
    ExistingReceipt,
    /// The operation can no longer say whether a commit exists.
    Unknown,
}

impl CommitEffect {
    /// The JSON envelope value of this observation (SPEC 7.1).
    pub fn as_str(self) -> &'static str {
        match self {
            CommitEffect::None => "none",
            CommitEffect::Created => "created",
            CommitEffect::ExistingReceipt => "existing_receipt",
            CommitEffect::Unknown => "unknown",
        }
    }
}

/// What one operation observed about the repository itself (SPEC 7.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepositoryEffect {
    /// The operation left the repository as it found it.
    Unchanged,
    /// The operation created the repository.
    Created,
    /// The operation can no longer say what happened to the repository.
    Unknown,
}

impl RepositoryEffect {
    /// The JSON envelope value of this observation (SPEC 7.1).
    pub fn as_str(self) -> &'static str {
        match self {
            RepositoryEffect::Unchanged => "unchanged",
            RepositoryEffect::Created => "created",
            RepositoryEffect::Unknown => "unknown",
        }
    }
}

/// The observed effects of one operation (SPEC 7.1). Every operation
/// result carries one instance, including failed operations and queries
/// for which everything stayed unchanged.
///
/// An observation that was lost stays lost: `unknown` is never replaced by
/// a known value, because a mutation timeout must not default to
/// `unchanged` (SPEC 7.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Effects {
    /// The scoped and unscoped working files.
    pub working_tree: ObservedEffect,
    /// The real Git index.
    pub index: ObservedEffect,
    /// Commit history of the adopted branch.
    pub commit: CommitEffect,
    /// The commit the `commit` observation names, when there is one.
    pub commit_oid: Option<String>,
    /// The repository and its layout.
    pub repository: RepositoryEffect,
    /// Local Margin settings in the Git configuration (SPEC 8.2).
    pub configuration: ObservedEffect,
    /// Installed hook entries of an agent integration (SPEC 12.4).
    pub integration: ObservedEffect,
}

impl Effects {
    /// The effects of an operation that only observed: every field known
    /// and untouched, no commit created.
    pub fn unchanged() -> Effects {
        Effects {
            working_tree: ObservedEffect::Unchanged,
            index: ObservedEffect::Unchanged,
            commit: CommitEffect::None,
            commit_oid: None,
            repository: RepositoryEffect::Unchanged,
            configuration: ObservedEffect::Unchanged,
            integration: ObservedEffect::Unchanged,
        }
    }

    /// The effects of an operation whose outcome is unknown, such as a
    /// killed mutation process or a response lost to a broken pipe. No
    /// field may claim `unchanged` (SPEC 7.1, 9.6).
    pub fn unknown() -> Effects {
        Effects {
            working_tree: ObservedEffect::Unknown,
            index: ObservedEffect::Unknown,
            commit: CommitEffect::Unknown,
            commit_oid: None,
            repository: RepositoryEffect::Unknown,
            configuration: ObservedEffect::Unknown,
            integration: ObservedEffect::Unknown,
        }
    }

    /// Records what happened to the working tree.
    pub fn observe_working_tree(&mut self, effect: ObservedEffect) {
        keep_unknown(&mut self.working_tree, effect);
    }

    /// Records what happened to the index.
    pub fn observe_index(&mut self, effect: ObservedEffect) {
        keep_unknown(&mut self.index, effect);
    }

    /// Records what happened to commit history. A known commit carries
    /// its full object ID; a lost observation drops the identifier again.
    pub fn observe_commit(&mut self, effect: CommitEffect, commit_oid: Option<String>) {
        if self.commit != CommitEffect::Unknown {
            self.commit = effect;
            self.commit_oid = match effect {
                CommitEffect::Created | CommitEffect::ExistingReceipt => commit_oid,
                CommitEffect::None | CommitEffect::Unknown => None,
            };
        }
    }

    /// Records what happened to the repository.
    pub fn observe_repository(&mut self, effect: RepositoryEffect) {
        if self.repository != RepositoryEffect::Unknown {
            self.repository = effect;
        }
    }

    /// Records what happened to the local configuration.
    pub fn observe_configuration(&mut self, effect: ObservedEffect) {
        keep_unknown(&mut self.configuration, effect);
    }

    /// Records what happened to an installed integration.
    pub fn observe_integration(&mut self, effect: ObservedEffect) {
        keep_unknown(&mut self.integration, effect);
    }
}

/// Applies one observation unless the current one is already lost.
fn keep_unknown(current: &mut ObservedEffect, observed: ObservedEffect) {
    if *current != ObservedEffect::Unknown {
        *current = observed;
    }
}

/// One warning in an operation result (SPEC 7.1): a bounded observation
/// that never conceals a failed invariant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    /// The SPEC 7.5 code family the warning belongs to.
    pub code: &'static str,
    /// A bounded explanation.
    pub message: String,
}

/// The one typed primary error of a failed operation (SPEC 7.1, 7.5).
///
/// The code comes from the SPEC 7.5 vocabulary, `retryable` says whether
/// repeating the identical request can be useful and safe, and
/// `recovery_argv` is one suggestion as an argv array. Recovery is never a
/// shell script built from untrusted text (SPEC 7.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationError {
    /// The SPEC 7.5 error code.
    pub code: &'static str,
    /// A bounded explanation.
    pub message: String,
    /// Whether an identical retry can be useful and safe.
    pub retryable: bool,
    /// One recovery suggestion as an argv array, empty when none applies.
    pub recovery_argv: Vec<String>,
}

impl OperationError {
    /// One typed failure with a bounded message.
    pub fn new(code: &'static str, message: impl Into<String>) -> OperationError {
        OperationError {
            code,
            message: message.into(),
            retryable: false,
            recovery_argv: Vec::new(),
        }
    }

    /// Marks an identical retry as useful and safe (SPEC 7.5).
    pub fn retryable(mut self) -> OperationError {
        self.retryable = true;
        self
    }

    /// Attaches one recovery suggestion as an argv array (SPEC 7.5).
    pub fn recovery(mut self, argv: &[&str]) -> OperationError {
        self.recovery_argv = argv.iter().map(|word| (*word).to_string()).collect();
        self
    }
}

impl From<GitError> for OperationError {
    fn from(error: GitError) -> Self {
        OperationError {
            code: error.public_code(),
            message: error.to_string(),
            retryable: error.retryable(),
            recovery_argv: Vec::new(),
        }
    }
}

impl From<SettingsError> for OperationError {
    fn from(error: SettingsError) -> Self {
        OperationError {
            code: error.public_code(),
            message: error.to_string(),
            retryable: matches!(&error, SettingsError::Git(git) if git.retryable()),
            recovery_argv: Vec::new(),
        }
    }
}

impl From<ScopeResolutionError> for OperationError {
    fn from(error: ScopeResolutionError) -> Self {
        let retryable = match &error {
            ScopeResolutionError::Git(error) => error.retryable(),
            _ => false,
        };
        OperationError {
            code: error.public_code(),
            message: error.to_string(),
            retryable,
            recovery_argv: Vec::new(),
        }
    }
}

impl From<PolicyError> for OperationError {
    fn from(error: PolicyError) -> Self {
        let retryable = match &error {
            PolicyError::Git(error) => error.retryable(),
            _ => false,
        };
        OperationError {
            code: error.public_code(),
            message: error.to_string(),
            retryable,
            recovery_argv: Vec::new(),
        }
    }
}

impl From<BaselineError> for OperationError {
    fn from(error: BaselineError) -> Self {
        let retryable = match &error {
            BaselineError::Git(error) => error.retryable(),
            _ => false,
        };
        OperationError {
            code: error.public_code(),
            message: error.to_string(),
            retryable,
            recovery_argv: Vec::new(),
        }
    }
}

impl From<IdentityError> for OperationError {
    fn from(error: IdentityError) -> Self {
        let retryable = match &error {
            IdentityError::Git(error) => error.retryable(),
            _ => false,
        };
        OperationError {
            code: error.public_code(),
            message: error.to_string(),
            retryable,
            recovery_argv: Vec::new(),
        }
    }
}

impl From<PlanProblem> for OperationError {
    fn from(problem: PlanProblem) -> Self {
        OperationError {
            code: problem.public_code(),
            message: problem.to_string(),
            retryable: false,
            recovery_argv: Vec::new(),
        }
    }
}

impl From<MarkerCommitError> for OperationError {
    fn from(error: MarkerCommitError) -> Self {
        let retryable = match &error {
            MarkerCommitError::Git(error) => error.retryable(),
            _ => false,
        };
        OperationError {
            code: error.public_code(),
            message: error.to_string(),
            retryable,
            recovery_argv: Vec::new(),
        }
    }
}

/// One complete operation result (SPEC 7.1): semantic data on success or
/// one typed primary error on failure, warnings that never conceal a
/// failed invariant, and the observed effects, which are required even on
/// failure.
///
/// `ok` is derived, never stored, so it cannot disagree with the error
/// slot: an operation is successful exactly when it has no error.
#[derive(Clone, Debug)]
pub struct OperationOutcome<T> {
    /// Command-specific data on success, or the observed state on failure.
    pub data: Option<T>,
    /// Exactly one typed primary error on failure; `None` on success.
    pub error: Option<OperationError>,
    /// Warnings; empty when there are none.
    pub warnings: Vec<Warning>,
    /// What the operation observed; always present (SPEC 7.1).
    pub effects: Effects,
}

impl<T> OperationOutcome<T> {
    /// True only when the operation has a verified result, including an
    /// explicit no-op (SPEC 7.1).
    pub fn ok(&self) -> bool {
        self.error.is_none()
    }

    /// One successful result.
    pub fn success(data: T, effects: Effects) -> Self {
        OperationOutcome {
            data: Some(data),
            error: None,
            warnings: Vec::new(),
            effects,
        }
    }

    /// One successful result with warnings.
    pub fn success_with_warnings(data: T, warnings: Vec<Warning>, effects: Effects) -> Self {
        OperationOutcome {
            data: Some(data),
            error: None,
            warnings,
            effects,
        }
    }

    /// One failed result. The failure still carries its effects, because
    /// a refused operation must report what it observed (SPEC 7.1).
    pub fn failure(error: OperationError, effects: Effects) -> Self {
        OperationOutcome {
            data: None,
            error: Some(error),
            warnings: Vec::new(),
            effects,
        }
    }

    /// One failed result that still reports the state it observed, as the
    /// nullable observed-state object of SPEC 7.1.
    pub fn observed_failure(error: OperationError, data: T, effects: Effects) -> Self {
        OperationOutcome {
            data: Some(data),
            error: Some(error),
            warnings: Vec::new(),
            effects,
        }
    }
}

// ---------------------------------------------------------------------------
// Local settings (SPEC 8.2, 6.2)
// ---------------------------------------------------------------------------

/// Failure of one local-settings read or update (SPEC 8.2).
#[derive(Debug)]
pub enum SettingsError {
    /// A supplied setting value is invalid input.
    InvalidInput(SettingsValueError),
    /// A stored value does not match its documented form, so the effective
    /// settings cannot be reported honestly.
    Stored(SettingsValueError),
    /// A Git process failed.
    Git(GitError),
}

impl SettingsError {
    /// The SPEC 7.5 error code of this failure. A value a caller supplied
    /// is invalid input; a broken stored value is an unmet precondition of
    /// the repository, never something this build repairs silently.
    pub fn public_code(&self) -> &'static str {
        match self {
            SettingsError::InvalidInput(_) => "INVALID_ARGUMENT",
            SettingsError::Stored(_) => "INVALID_SETTINGS",
            SettingsError::Git(error) => error.public_code(),
        }
    }
}

impl fmt::Display for SettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SettingsError::InvalidInput(error) => write!(formatter, "{error}"),
            SettingsError::Stored(error) => {
                write!(formatter, "the local Margin settings are unusable: {error}")
            }
            SettingsError::Git(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for SettingsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SettingsError::InvalidInput(error) => Some(error),
            SettingsError::Stored(error) => Some(error),
            SettingsError::Git(error) => Some(error),
        }
    }
}

impl From<GitError> for SettingsError {
    fn from(error: GitError) -> Self {
        SettingsError::Git(error)
    }
}

/// Reads one documented setting and parses its value with `parse`. An unset
/// key is `None`; a value outside its documented form fails typed.
fn read_setting<T>(
    git: &Git,
    key: &'static str,
    parse: impl Fn(&str) -> Result<T, SettingsValueError>,
) -> Result<Option<T>, SettingsError> {
    let raw = git.config_value(key)?;
    match raw {
        Some(raw) => parse(&raw).map(Some).map_err(SettingsError::Stored),
        None => Ok(None),
    }
}

/// Reads the effective local Margin settings of `git`'s repository
/// (SPEC 8.2): every documented `margin.*` key through Git's configuration
/// interface, with the first-use defaults where none is stored.
///
/// The values read are the ones Git itself applies. Nothing is written and
/// no environment variable is consulted, so the result carries no
/// environment secrets.
pub fn read_local_settings(git: &Git) -> Result<LocalSettings, SettingsError> {
    // One process reads every key (SPEC 17.5); the parse of each value
    // stays the per-key parse the settings contract defines.
    let keys = [
        domain::SETTINGS_KEY_VERSION,
        domain::SETTINGS_KEY_BRANCH,
        domain::SETTINGS_KEY_SCOPE,
        domain::SETTINGS_KEY_AUTO_CHECKPOINT,
        domain::SETTINGS_KEY_CAPTURE_AFTER_EDIT,
        domain::SETTINGS_KEY_EDITOR_ARGV,
    ];
    let values = git
        .config_values(&keys)
        .map_err(SettingsError::Git)?;
    let setting = |key: &'static str| values.get(key).cloned();
    Ok(LocalSettings {
        version: parse_stored_setting(
            setting(domain::SETTINGS_KEY_VERSION),
            domain::parse_setting_version,
        )?,
        branch: parse_stored_setting(
            setting(domain::SETTINGS_KEY_BRANCH),
            domain::parse_setting_branch,
        )?,
        scope: parse_stored_setting(
            setting(domain::SETTINGS_KEY_SCOPE),
            domain::parse_setting_scope,
        )?,
        auto_checkpoint: parse_stored_setting(
            setting(domain::SETTINGS_KEY_AUTO_CHECKPOINT),
            |raw| domain::parse_setting_flag(domain::SETTINGS_KEY_AUTO_CHECKPOINT, raw),
        )?
        .unwrap_or(false),
        capture_after_edit: parse_stored_setting(
            setting(domain::SETTINGS_KEY_CAPTURE_AFTER_EDIT),
            |raw| domain::parse_setting_flag(domain::SETTINGS_KEY_CAPTURE_AFTER_EDIT, raw),
        )?
        .unwrap_or(false),
        editor_argv: parse_stored_setting(
            setting(domain::SETTINGS_KEY_EDITOR_ARGV),
            domain::parse_setting_argv,
        )?,
    })
}

/// Parses one stored setting value: absent stays absent, a present value
/// goes through its strict parser (SPEC 8.2).
fn parse_stored_setting<T>(
    raw: Option<String>,
    parse: impl Fn(&str) -> Result<T, SettingsValueError>,
) -> Result<Option<T>, SettingsError> {
    match raw {
        Some(raw) => parse(&raw).map(Some).map_err(SettingsError::Stored),
        None => Ok(None),
    }
}

/// Plans one explicit local-settings update without writing anything
/// (SPEC 8.2): validates every supplied value, reads every omitted key, and
/// names the exact writes in write order, including the first-write version
/// stamp. [`update_local_settings`] applies exactly this plan, and a dry run
/// reports it as the intended configuration.
fn plan_local_settings(
    git: &Git,
    update: &SettingsUpdate,
) -> Result<(LocalSettings, Vec<AppliedSetting>), SettingsError> {
    let stored_version = read_setting(
        git,
        domain::SETTINGS_KEY_VERSION,
        domain::parse_setting_version,
    )?;

    let mut writes: Vec<AppliedSetting> = Vec::new();
    let branch = match &update.branch {
        Some(supplied) => {
            let value =
                domain::setting_branch_value(supplied).map_err(SettingsError::InvalidInput)?;
            writes.push(applied_setting(domain::SETTINGS_KEY_BRANCH, value.clone()));
            Some(supplied.clone())
        }
        None => read_setting(
            git,
            domain::SETTINGS_KEY_BRANCH,
            domain::parse_setting_branch,
        )?,
    };
    let scope = match &update.scope {
        Some(supplied) => {
            let value =
                domain::setting_scope_value(supplied).map_err(SettingsError::InvalidInput)?;
            writes.push(applied_setting(domain::SETTINGS_KEY_SCOPE, value));
            Some(supplied.clone())
        }
        None => read_setting(git, domain::SETTINGS_KEY_SCOPE, domain::parse_setting_scope)?,
    };
    let auto_checkpoint = match update.auto_checkpoint {
        Some(supplied) => {
            writes.push(applied_setting(
                domain::SETTINGS_KEY_AUTO_CHECKPOINT,
                domain::setting_flag_value(supplied).to_string(),
            ));
            supplied
        }
        // First use captures nothing: an unset flag is off (SPEC 6.2).
        None => read_setting(git, domain::SETTINGS_KEY_AUTO_CHECKPOINT, |raw| {
            domain::parse_setting_flag(domain::SETTINGS_KEY_AUTO_CHECKPOINT, raw)
        })?
        .unwrap_or(false),
    };
    let capture_after_edit = match update.capture_after_edit {
        Some(supplied) => {
            writes.push(applied_setting(
                domain::SETTINGS_KEY_CAPTURE_AFTER_EDIT,
                domain::setting_flag_value(supplied).to_string(),
            ));
            supplied
        }
        None => read_setting(git, domain::SETTINGS_KEY_CAPTURE_AFTER_EDIT, |raw| {
            domain::parse_setting_flag(domain::SETTINGS_KEY_CAPTURE_AFTER_EDIT, raw)
        })?
        .unwrap_or(false),
    };
    let editor_argv = match &update.editor_argv {
        Some(supplied) => {
            let value =
                domain::setting_argv_value(supplied).map_err(SettingsError::InvalidInput)?;
            writes.push(applied_setting(domain::SETTINGS_KEY_EDITOR_ARGV, value));
            Some(supplied.clone())
        }
        None => read_setting(
            git,
            domain::SETTINGS_KEY_EDITOR_ARGV,
            domain::parse_setting_argv,
        )?,
    };

    // The first write of any setting records which settings contract the
    // stored values follow.
    if stored_version.is_none() && !writes.is_empty() {
        writes.insert(
            0,
            applied_setting(
                domain::SETTINGS_KEY_VERSION,
                domain::setting_version_value(),
            ),
        );
    }

    let version = if writes.is_empty() {
        stored_version
    } else {
        Some(domain::SETTINGS_VERSION)
    };
    Ok((
        LocalSettings {
            version,
            branch,
            scope,
            auto_checkpoint,
            capture_after_edit,
            editor_argv,
        },
        writes,
    ))
}

/// Applies one explicit local-settings update through Git's configuration
/// interface (SPEC 8.2, 6.2).
///
/// Only the settings `update` names are written, each in its documented
/// form, and every other configuration entry stays untouched, so an omitted
/// setting keeps its value. The first write of any setting stamps the
/// settings contract version. Every supplied value is validated, and every
/// omitted key is read, before the first write, so an invalid update or an
/// unreadable stored value changes nothing.
///
/// `effects` records what the writes did to the configuration as they
/// happen; a killed process leaves the observation `unknown` rather than
/// narrowed (SPEC 7.1, 9.6).
pub fn update_local_settings(
    git: &Git,
    update: &SettingsUpdate,
    effects: &mut Effects,
) -> Result<(LocalSettings, Vec<AppliedSetting>), SettingsError> {
    let (settings, writes) = plan_local_settings(git, update)?;
    for (written, write) in writes.iter().enumerate() {
        if let Err(error) = git.set_local_config(&write.key, &write.value) {
            if written == 0 && matches!(error, GitError::Timeout { .. }) {
                // A killed first write may or may not have stored its entry,
                // so nothing may be claimed. Once one write landed, the
                // configuration is known changed whatever the killed write
                // did (SPEC 7.1, 9.6).
                effects.observe_configuration(ObservedEffect::Unknown);
            }
            return Err(SettingsError::Git(error));
        }
        effects.observe_configuration(ObservedEffect::Changed);
    }
    Ok((settings, writes))
}

/// Builds one applied-setting record.
fn applied_setting(key: &str, value: String) -> AppliedSetting {
    AppliedSetting {
        key: key.to_string(),
        value,
    }
}

/// The local-settings update one adoption applies after its start commit
/// verifies (SPEC 8.2): the adopted branch and the canonical scope, plus
/// the explicit setup settings the request named. An omitted setup setting
/// keeps its value; first use captures nothing (SPEC 6.2).
fn adoption_settings(
    request: &InitRequest,
    branch_ref: &Option<String>,
    descriptor: &ScopeDescriptor,
) -> SettingsUpdate {
    SettingsUpdate {
        branch: branch_ref
            .as_deref()
            .map(|full| full.strip_prefix("refs/heads/").unwrap_or(full).to_string()),
        scope: Some(descriptor.clone()),
        auto_checkpoint: request.auto_checkpoint,
        capture_after_edit: request.capture_after_edit,
        editor_argv: request.editor_argv.clone(),
    }
}

/// The local-settings update one repeat initialization applies (SPEC 8.2):
/// the adoption update with the adopted branch and the canonical scope
/// removed whenever their stored values already match, so a repeat changes
/// only the settings it supplies and a repeat that supplies nothing writes
/// nothing. The retry of an initialization whose configuration write failed
/// after its start commit verified records both again, because nothing was
/// stored, while adopting that validated marker instead of committing a
/// second one.
fn repeat_settings(
    git: &Git,
    request: &InitRequest,
    branch_ref: &Option<String>,
    descriptor: &ScopeDescriptor,
) -> Result<SettingsUpdate, SettingsError> {
    let mut update = adoption_settings(request, branch_ref, descriptor);
    let stored = read_local_settings(git)?;
    if update.branch.as_deref() == stored.branch.as_deref() {
        update.branch = None;
    }
    if stored.scope.as_ref() == Some(descriptor) {
        update.scope = None;
    }
    Ok(update)
}

/// The request of one inspection query.
#[derive(Clone, Debug, Default)]
pub struct InspectRequest {
    /// The writing scope to inspect. `None` uses the stored scope of the
    /// local settings; with none stored, the query reports
    /// `NOT_INITIALIZED` (SPEC 8.2).
    pub scope: Option<ScopeInput>,
    /// Presentation filters of `status --path` (SPEC 6.3): one is a literal
    /// path or directory prefix, `.` covers the whole scope. A filter
    /// narrows only the reported path list; every count stays whole-scope.
    pub path_filter: Vec<String>,
}

/// The request of one scope initialization (SPEC 5.1, 6.2 `init`).
#[derive(Clone, Debug, Default)]
pub struct InitRequest {
    /// The writing scope to adopt; `None` reuses the stored scope.
    pub scope: Option<ScopeInput>,
    /// Whether initialization may create the repository itself.
    pub create_repository: bool,
    /// The expected branch (SPEC 5.1): in an existing repository the
    /// initialization refuses when the adopted branch differs; with
    /// `create_repository` the value names the initial branch.
    pub branch: Option<String>,
    /// Whether a new starting anchor is forced even when one exists
    /// (SPEC 8.2 `--new-start`).
    pub new_start: bool,
    /// The explicit automatic-checkpoint policy; `None` keeps the stored
    /// value, which is off on first use (SPEC 6.2).
    pub auto_checkpoint: Option<bool>,
    /// The explicit editor-exit capture policy; `None` keeps the stored
    /// value, which is off on first use (SPEC 6.2).
    pub capture_after_edit: Option<bool>,
    /// The explicit editor argv; `None` keeps the stored value.
    pub editor_argv: Option<Vec<String>>,
    /// Whether to report the intended effects without creating anything
    /// (SPEC 6.4).
    pub dry_run: bool,
}

/// The request of one scoped checkpoint (SPEC 6.2, 9.2).
#[derive(Clone, Debug)]
pub struct CheckpointRequest {
    /// The writing scope; `None` uses the stored scope.
    pub scope: Option<ScopeInput>,
    /// The explicit commit message; `None` uses the deterministic default.
    pub message: Option<String>,
    /// The guarded recovery mode that captures inspected staged content
    /// (SPEC 9.7 `--include-staged`).
    pub include_staged: bool,
    /// The expected `HEAD` commit of the guarded recovery capture (SPEC 9.7
    /// `--expect-head`); the capture refuses while `HEAD` holds another
    /// value. Required together with `include_staged`.
    pub expect_head: Option<String>,
    /// The expected index tree of the guarded recovery capture (SPEC 9.7
    /// `--expect-index`): the staged state one recovery inspection reported.
    pub expect_index: Option<String>,
    /// The expected scoped working fingerprint of the guarded recovery
    /// capture (SPEC 9.7 `--expect-working`), the `format:oid` value one
    /// recovery inspection reports.
    pub expect_working: Option<String>,
    /// Whether to report the intended effects without committing (SPEC 6.4).
    pub dry_run: bool,
}

/// The expected state of one guarded recovery capture (SPEC 9.7): the
/// three fingerprints `checkpoint --include-staged` rechecks at capture
/// time, validated once so the capture itself compares plain strings.
struct StagedCaptureExpectations {
    /// The commit `HEAD` must still hold.
    head: String,
    /// The tree the real index must still describe.
    index_tree: String,
    /// The `format:oid` fingerprint of the scoped working content.
    working: String,
}

/// One proved capture ready for the shared marker writer (SPEC 8.4, 9.2,
/// 9.7): the ordinary checkpoint and the guarded recovery capture hand
/// this one bundle to [`Operations::commit_proved_capture`], so both
/// commit, observe failures, and report effects through the same path.
struct ProvedCapture {
    /// The scope input of the request, for the post-failure observation.
    scope: Option<ScopeInput>,
    /// The prepared marker map the commit verifies itself against.
    metadata: CommitMetadata,
    /// The effective message text.
    text: String,
    /// The candidate the capture prepared against.
    candidate: String,
    /// The exact paths the capture saves, in path order.
    paths: Vec<OsString>,
    /// The intended full tree of the draft.
    tree: String,
    /// The scoped working-entry baseline the capture read before its
    /// first real mutation.
    working_before: BTreeMap<OsString, Option<WorkingEntry>>,
}

/// Validates the expected state of one checkpoint request (SPEC 9.7): the
/// guarded recovery capture carries exactly the three fingerprints a
/// recovery inspection reports — head, index tree, and scoped working
/// fingerprint — and an expected state belongs to no ordinary capture.
///
/// `Ok(None)` is one ordinary checkpoint; `Ok(Some)` is one guarded
/// recovery capture; `Err` is invalid input that refuses before any
/// inspection, with unchanged effects.
fn recovery_expectations(
    request: &CheckpointRequest,
) -> Result<Option<StagedCaptureExpectations>, OperationError> {
    match (
        request.include_staged,
        request.expect_head.as_deref(),
        request.expect_index.as_deref(),
        request.expect_working.as_deref(),
    ) {
        (false, None, None, None) => Ok(None),
        (true, Some(head), Some(index_tree), Some(working)) => {
            Ok(Some(StagedCaptureExpectations {
                head: head.to_string(),
                index_tree: index_tree.to_string(),
                working: working.to_string(),
            }))
        }
        (true, ..) => Err(OperationError::new(
            "INVALID_ARGUMENT",
            "the guarded recovery capture needs --expect-head, --expect-index, and \
             --expect-working together",
        )
        .recovery(&["margin", "doctor", "--json"])),
        (false, ..) => Err(OperationError::new(
            "INVALID_ARGUMENT",
            "an expected head, index, or working state belongs to the --include-staged \
             recovery capture only",
        )
        .recovery(&["margin", "doctor", "--json"])),
    }
}

/// The request of one review operation (SPEC 6.2, 7.3).
///
/// The plan arrives as the raw bytes of the `--input` document. Every rule
/// of the plan contract belongs to the one strict parser of the domain;
/// the interface layer only reads the bytes.
#[derive(Clone, Debug)]
pub struct ReviewRequest {
    /// The writing scope; `None` uses the stored scope.
    pub scope: Option<ScopeInput>,
    /// The raw review plan document (SPEC 7.3).
    pub plan: Vec<u8>,
    /// Whether to report the intended effects without committing (SPEC 6.4).
    pub dry_run: bool,
}

/// The request of one `diff` query (SPEC 6.2, 6.3): the presentation inputs
/// around the context every comparison shares.
///
/// The path filter, change selection, cursor, and limit are presentation
/// inputs only. None of them can change the review identity, the manifest,
/// the scope totals, or what finishing the review approves; a filter hides
/// detail, it never narrows the comparison (SPEC 6.3).
#[derive(Clone, Debug)]
pub struct DiffRequest {
    /// The writing scope; `None` uses the stored scope.
    pub scope: Option<ScopeInput>,
    /// Presentation filter: detail pages show only groups under one of
    /// these path roots. Empty shows the whole scope.
    pub path_filter: Vec<String>,
    /// The change identity whose detail one page shows; `None` pages through
    /// the groups in manifest order.
    pub change: Option<String>,
    /// The cursor one earlier page issued; `None` starts at the first page.
    pub cursor: Option<String>,
    /// Change groups per detail page; `None` uses
    /// [`DEFAULT_DIFF_PAGE_GROUPS`].
    pub limit: Option<u64>,
    /// Whether the candidate is one preview of the working tree instead of
    /// the committed candidate. A preview is never actionable (SPEC 6.2).
    pub working: bool,
}

/// The request of one historical document view (SPEC 6.2-6.3).
///
/// The path and the revision address one document at one immutable
/// revision; no writing scope is required, so a workspace can be inspected
/// before adoption. The bounds are presentation inputs only: they bound
/// what the view carries, never what the repository holds.
#[derive(Clone, Debug)]
pub struct ShowRequest {
    /// The repository-relative document path.
    pub path: String,
    /// The immutable revision to read (SPEC 4.1).
    pub revision: String,
    /// Largest number of content bytes to return; `None` uses the per-file
    /// ceiling of [`MAX_DOCUMENT_BYTES`] (SPEC 3.3).
    pub max_bytes: Option<u64>,
    /// Largest number of content lines to return; `None` returns every line
    /// the byte bound permits.
    pub max_lines: Option<u64>,
    /// Whether the caller asked for the exact bytes of the document instead
    /// of a bounded view (SPEC 6.3 `--format raw`). The export carries every
    /// byte, so no bound applies; an interface pairs it with the terminal
    /// protection the export contract requires.
    pub raw: bool,
}

/// The request of one editorial timeline query (SPEC 5.5, 6.2 `log`).
///
/// The path filter, the cursor, the limit, and the checkpoint display
/// policy are presentation inputs only. None of them can change which
/// commits the unfiltered walk of SPEC 8.5 sees, so a short displayed log
/// never makes an old baseline disappear (SPEC 6.3).
#[derive(Clone, Debug)]
pub struct LogRequest {
    /// The writing scope; `None` uses the stored scope.
    pub scope: Option<ScopeInput>,
    /// Presentation filter: the timeline shows only entries that changed
    /// one of these path roots. Empty shows every entry.
    pub path_filter: Vec<String>,
    /// The cursor one earlier page issued; `None` starts at the first page.
    pub cursor: Option<String>,
    /// Entries per page; `None` uses [`DEFAULT_LOG_PAGE_ENTRIES`].
    pub limit: Option<u64>,
    /// Whether the timeline lists every draft checkpoint instead of one
    /// entry per collapsed writing pass (SPEC 5.5).
    pub all_checkpoints: bool,
}

/// The kind label of one timeline entry (SPEC 5.5, 8.1): the four marker
/// kinds plus the ordinary history Margin never wrote. The label names what
/// a validated marker records; a claimed label no validation supports keeps
/// its kind with the reason it was ignored, never the meaning of a marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimelineKind {
    /// One validated `start` marker.
    Start,
    /// One validated `draft` marker, or one collapsed writing pass of them.
    Draft,
    /// One validated `review` marker.
    Review,
    /// One validated `restore` marker.
    Restore,
    /// Any commit that is no validated marker.
    Ordinary,
}

impl TimelineKind {
    /// The label every interface prints.
    pub fn as_str(self) -> &'static str {
        match self {
            TimelineKind::Start => "start",
            TimelineKind::Draft => "draft",
            TimelineKind::Review => "review",
            TimelineKind::Restore => "restore",
            TimelineKind::Ordinary => "ordinary",
        }
    }
}

impl fmt::Display for TimelineKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The reserved facts one validated marker contributes to its entry: the
/// scope it belongs to, the baseline a review completed, the request a
/// receipt records, and the source a restoration read.
#[derive(Clone, Debug)]
pub struct MarkerSummary {
    /// The `Margin-Scope` digest of the marker's own scope.
    pub scope_digest: String,
    /// The `Margin-Baseline` value. Review only.
    pub baseline: Option<String>,
    /// The `Margin-Request` value. Review and restore.
    pub request: Option<String>,
    /// The `Margin-Plan` value. Review and restore.
    pub plan: Option<String>,
    /// The `Margin-Source` value. Restore only.
    pub source: Option<RestoreSource>,
}

/// One entry of the editorial timeline (SPEC 5.5).
#[derive(Clone, Debug)]
pub struct LogEntry {
    /// The commit object ID. For a collapsed writing pass, its newest
    /// checkpoint.
    pub commit: String,
    /// The validated kind, or the claimed kind an ignored marker still
    /// names.
    pub kind: TimelineKind,
    /// The bounded subject line of the commit message.
    pub subject: String,
    /// Whether the subject bound dropped characters.
    pub subject_truncated: bool,
    /// The committer timestamp in seconds since the Unix epoch, when the
    /// header parses. The order of the timeline is ancestry, never the
    /// clock (SPEC 5.5).
    pub committer_time: Option<i64>,
    /// How many draft checkpoints this entry covers. One entry lists one
    /// checkpoint; a collapsed writing pass counts its run. The checkpoints
    /// stay reachable in Git and listable with `--all-checkpoints`.
    pub checkpoints: u64,
    /// The reserved facts of a validated marker; `None` when the commit is
    /// no validated marker.
    pub marker: Option<MarkerSummary>,
    /// Why a claimed marker was ignored, bounded. `None` for validated
    /// markers and ordinary commits.
    pub ignored_reason: Option<String>,
}

/// The semantic data of one timeline query (SPEC 5.5, 6.3): the commit the
/// walk starts from, the adopted scope the cursor binds, and one bounded
/// page of entries in ancestry order.
#[derive(Clone, Debug)]
pub struct LogOutcome {
    /// The commit the walk starts from — `HEAD` of the workspace. `None`
    /// when the branch is unborn and no timeline exists yet.
    pub head: Option<String>,
    /// The adopted scope identity, in `sha1:<oid>` form.
    pub scope_id: String,
    /// The object format of the repository (SPEC 7.1).
    pub object_format: ObjectFormat,
    /// The entries of this page, newest first in ancestry order.
    pub entries: Vec<LogEntry>,
    /// Whether the timeline holds no further entry.
    pub complete: bool,
    /// The cursor of the next page; `None` when this page is complete.
    pub next_cursor: Option<String>,
    /// Whether every draft checkpoint is listed instead of collapsed
    /// writing passes.
    pub all_checkpoints: bool,
}

/// The request of one historical restoration (SPEC 7.4, 9.5): the strict
/// plan document names the request, the pinned context, the one document,
/// and the source revision.
#[derive(Clone, Debug)]
pub struct RestoreRequest {
    /// The writing scope; `None` uses the stored scope.
    pub scope: Option<ScopeInput>,
    /// The restoration plan document of SPEC 7.4, exactly as read.
    pub plan: Vec<u8>,
    /// Whether to report the intended effects without committing (SPEC 6.4).
    pub dry_run: bool,
}

/// The request of one recovery inspection (SPEC 9.7).
#[derive(Clone, Debug, Default)]
pub struct RecoveryRequest {
    /// The writing scope; `None` uses the stored scope.
    pub scope: Option<ScopeInput>,
}

/// The semantic data of one inspection query: what the shared resolver,
/// guards, and baseline walk observe about one workspace.
#[derive(Clone, Debug)]
pub struct WorkspaceInspection {
    /// The object format of the inspected repository.
    pub object_format: ObjectFormat,
    /// The full ref `HEAD` points at; `None` while detached.
    pub branch: Option<String>,
    /// The commit `HEAD` points at; `None` while the branch is unborn.
    pub head: Option<String>,
    /// The effective local Margin settings, without environment secrets
    /// (SPEC 8.2).
    pub settings: LocalSettings,
    /// The resolved writing scope.
    pub scope: ScopeSummary,
    /// The baseline the first-parent walk selects at `HEAD` (SPEC 8.5).
    pub baseline: BaselineStatus,
    /// Repository and storage policy blockers in guard order (SPEC 9.1).
    pub policy_blockers: Vec<PolicyBlocker>,
    /// Whether the observed scoped working content is in the candidate
    /// (SPEC 4.2 preservation).
    pub preservation: PreservationState,
    /// Whether scoped differences remain against a valid approved baseline
    /// (SPEC 4.2 review).
    pub review: ReviewState,
    /// What a mutation is doing, or what must be inspected before the next
    /// one (SPEC 4.2 operation).
    pub operation: OperationState,
    /// Pending and uncaptured counts, each known or honestly null with one
    /// reason (SPEC 6.3).
    pub counts: StatusCounts,
    /// The path and size limits that govern this workspace (SPEC 3.3).
    pub limits: InspectionLimits,
}

/// Why one status count is `null` instead of a fabricated value (SPEC 6.3).
pub const COUNT_REASON_NO_CANDIDATE: &str = "no_candidate";
/// Why one status count is `null`: first-parent history anchors no valid
/// baseline, so no pending comparison exists (SPEC 6.3, 8.5).
pub const COUNT_REASON_BASELINE_UNAVAILABLE: &str = "baseline_unavailable";
/// Why one status count is `null`: the change grouping of SPEC 10.2 has
/// not computed the groups yet (SPEC 6.3).
pub const COUNT_REASON_GROUPING_UNAVAILABLE: &str = "grouping_unavailable";
/// Why one status count is `null`: storage transformation blockers make
/// the working bytes and the stored blobs incomparable (SPEC 8.8).
pub const COUNT_REASON_STORAGE_TRANSFORMATION: &str = "storage_transformation";

/// The pending and uncaptured counts of one inspection (SPEC 6.3).
///
/// Each expensive count is known or honestly `None` with one reason;
/// `None` is never replaced by a fabricated zero.
#[derive(Clone, Debug)]
pub struct StatusCounts {
    /// Scoped paths whose stored entry differs between the baseline and
    /// the candidate. `None` when no valid baseline exists.
    pub pending_files: Option<usize>,
    /// Why `pending_files` is unknown.
    pub pending_files_reason: Option<&'static str>,
    /// Change groups the pending files form. `None` until the grouping
    /// contract of SPEC 10.2 provides them.
    pub pending_groups: Option<usize>,
    /// Why `pending_groups` is unknown.
    pub pending_groups_reason: Option<&'static str>,
    /// Eligible scoped paths whose working content is not in the
    /// candidate; `None` when storage transformations make the comparison
    /// meaningless.
    pub uncaptured: Option<UncapturedPaths>,
    /// Why `uncaptured` is unknown.
    pub uncaptured_reason: Option<&'static str>,
}

/// The uncaptured scoped paths of one inspection: the whole-scope total
/// plus the list the presentation filter selected (SPEC 6.3, 7.2).
#[derive(Clone, Debug)]
pub struct UncapturedPaths {
    /// Every uncaptured path, independent of presentation filtering.
    pub total: usize,
    /// The uncaptured paths that pass the request's presentation filter.
    pub paths: Vec<OsString>,
}

/// The limits one inspection reports with the state (SPEC 3.3): fixed
/// configuration values of release 0.1, included in inspection results and
/// changed only through explicit user action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InspectionLimits {
    /// Largest document that still receives the full rendered comparison.
    pub rendered_comparison_bytes: u64,
    /// Largest eligible document at all; a larger file stays unapproved.
    pub document_bytes: u64,
}

impl InspectionLimits {
    /// The limits of release 0.1 (SPEC 3.3).
    pub fn current() -> InspectionLimits {
        InspectionLimits {
            rendered_comparison_bytes: MAX_RENDERED_COMPARISON_BYTES,
            document_bytes: MAX_DOCUMENT_BYTES,
        }
    }
}

/// One summarized writing scope: identity, path counts by state, and the
/// actionable blockers of SPEC 3.2.
#[derive(Clone, Debug)]
pub struct ScopeSummary {
    /// The full scope digest, such as `sha1:<oid>` (SPEC 8.6).
    pub digest: String,
    /// The canonical scope descriptor.
    pub descriptor: ScopeDescriptor,
    /// Tracked scoped paths with a working file present.
    pub tracked: usize,
    /// Tracked scoped paths whose working file is gone.
    pub tracked_deleted: usize,
    /// Untracked, non-ignored scoped paths eligible for capture.
    pub untracked: usize,
    /// Actionable blockers; each names a path (SPEC 3.2 rule 5).
    pub blockers: Vec<ScopeBlocker>,
}

impl ScopeSummary {
    /// Summarizes one resolved scope.
    fn of(resolved: &ResolvedScope) -> ScopeSummary {
        let (mut tracked, mut deleted, mut untracked) = (0, 0, 0);
        for state in resolved.paths.values() {
            match state {
                ScopedPathState::Tracked => tracked += 1,
                ScopedPathState::TrackedDeleted => deleted += 1,
                ScopedPathState::Untracked => untracked += 1,
            }
        }
        ScopeSummary {
            digest: resolved.digest.id(),
            descriptor: resolved.descriptor.clone(),
            tracked,
            tracked_deleted: deleted,
            untracked,
            blockers: resolved.blockers.clone(),
        }
    }
}

/// The baseline state of one workspace (SPEC 8.5).
#[derive(Clone, Debug)]
pub enum BaselineStatus {
    /// The walk anchored at this valid marker.
    Anchored(Box<ValidMarker>),
    /// No anchor can be established, for the reason given. The reason is
    /// reported, never worked around with an arbitrary initial commit.
    Unavailable(BaselineUnavailable),
    /// The branch is unborn, so no candidate exists to walk from.
    NoCandidate,
}

/// The semantic data of one completed initialization (SPEC 5.1, 8.2). A
/// repeat initialization that changed only settings creates no start
/// commit; `tree_reason` says why no tree was intended.
#[derive(Clone, Debug)]
pub struct InitializationOutcome {
    /// The start commit that anchors the scope, when one was created. A dry
    /// run and a repeat initialization create none.
    pub start_commit: Option<String>,
    /// Whether initialization created the repository itself.
    pub repository_created: bool,
    /// The local settings the operation applied, or, in a dry run over an
    /// existing repository, the exact writes it would apply in write order.
    pub settings: Vec<AppliedSetting>,
    /// Whether this was a dry run that created nothing (SPEC 6.4).
    pub dry_run: bool,
    /// The intended full result tree of the starting commit, when it could
    /// be computed; `None` with `tree_reason` otherwise (SPEC 6.4).
    pub expected_tree: Option<String>,
    /// Why `expected_tree` could not be computed.
    pub tree_reason: Option<&'static str>,
    /// The exact proposal of an initialization whose repository does not
    /// exist yet, so no tree could be computed (SPEC 6.4).
    pub proposed: Option<InitializationProposal>,
}

/// Why one initialization reports no intended tree (SPEC 6.4).
pub const TREE_REASON_REPOSITORY_NOT_CREATED: &str = "repository_not_created";

/// Why one initialization reports no intended tree: the scope already holds
/// a valid matching anchor, so the operation plans no start commit and
/// changes only settings (SPEC 8.2).
pub const TREE_REASON_ALREADY_ANCHORED: &str = "already_anchored";

/// The exact proposal of `init --create-repo --dry-run` for a repository
/// that does not exist yet (SPEC 6.4): the scope input and the explicit
/// settings as the interface supplied them, unchanged by any resolution the
/// unperformed repository creation would enable.
#[derive(Clone, Debug, Default)]
pub struct InitializationProposal {
    /// The scope input the interface supplied.
    pub scope: ScopeInput,
    /// The expected or initial branch name, when one was given.
    pub branch: Option<String>,
    /// Whether the real operation would create the repository.
    pub create_repository: bool,
    /// The explicit automatic-checkpoint policy.
    pub auto_checkpoint: Option<bool>,
    /// The explicit editor-exit capture policy.
    pub capture_after_edit: Option<bool>,
    /// The explicit editor argv.
    pub editor_argv: Option<Vec<String>>,
}

/// One local setting an operation applied (SPEC 8.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedSetting {
    /// The `margin.*` configuration key.
    pub key: String,
    /// The stored value.
    pub value: String,
}

/// The semantic data of one completed checkpoint (SPEC 9.2): the created
/// draft commit, the exact eligible paths it captured, and the candidate it
/// reports.
#[derive(Clone, Debug)]
pub struct CheckpointOutcome {
    /// The draft commit; `None` when scoped content was unchanged, which is
    /// an explicit no-op, not a failure, and when a dry run reports the
    /// intended capture without committing.
    pub commit: Option<String>,
    /// Whether eligible scoped content differed from the candidate.
    pub changed: bool,
    /// The candidate the checkpoint reports: the created draft commit, or
    /// the unchanged `HEAD` of a no-op (SPEC 9.2 `created:false`).
    pub candidate: String,
    /// The exact eligible paths the checkpoint captures: additions, edits,
    /// mode changes, and deletions, in path order (SPEC 9.2).
    pub paths: Vec<OsString>,
    /// Whether this was a dry run that created nothing (SPEC 6.4).
    pub dry_run: bool,
    /// The intended full tree of the draft; `None` for an explicit no-op.
    pub expected_tree: Option<String>,
}

/// The semantic data of one `diff` result (SPEC 7.2): the complete review
/// identity of the pinned comparison, the whole-scope totals, and one
/// bounded detail page. A decided plan reports the same identity through
/// [`ReviewDecision`] instead, with the intended result one disposable
/// index proved.
#[derive(Clone, Debug)]
pub struct ReviewOutcome {
    /// The deterministic identity of the complete pinned comparison.
    pub review_id: String,
    /// Version of patch generation, grouping, and canonicalization rules.
    pub engine_version: String,
    /// The effective Git version that contributed to this comparison
    /// contract.
    pub git_version: String,
    /// The full adopted branch ref the comparison ran on.
    pub branch_ref: String,
    /// The full scope digest, such as `sha1:<oid>` (SPEC 8.6).
    pub scope_id: String,
    /// The canonical scope descriptor.
    pub scope: ScopeDescriptor,
    /// Baseline B of the comparison.
    pub baseline_oid: String,
    /// Candidate C of the comparison. A working preview names its preview
    /// tree, because no working commit exists.
    pub candidate_oid: String,
    /// Digest of the exact scoped Git entries of C (SPEC 7.2, 8.6).
    pub candidate_scope_digest: String,
    /// Digest of all groups, paths, patch bytes, and ordering.
    pub manifest_digest: String,
    /// The bounded detail page this response carries, in manifest order.
    pub changes: Vec<ChangeDetail>,
    /// Complete file and group counts of the whole scope, independent of
    /// presentation filtering (SPEC 6.3, 7.2).
    pub scope_totals: ScopeTotals,
    /// How many of the scope's change groups this response omits: filtered
    /// out, beyond the page bound, or not the selected change.
    pub omitted_detail_count: usize,
    /// Whether all requested detail was returned.
    pub complete: bool,
    /// The cursor of the next detail page; `None` when this page is
    /// complete.
    pub next_cursor: Option<String>,
    /// Whether C is one preview of the working tree.
    pub working: bool,
    /// False for working previews, unsupported content states, or
    /// incomplete computation (SPEC 7.2). A bounded detail page stays
    /// actionable: the identity and the authoritative plan validation cover
    /// the whole scope, whatever one page displayed (SPEC 6.3).
    pub actionable: bool,
}

/// The whole-scope totals of one comparison (SPEC 6.3, 7.2): complete
/// counts that no path filter, detail limit, or cursor changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScopeTotals {
    /// Changed scoped files of the comparison, additions and deletions
    /// included.
    pub files: usize,
    /// Safe change groups those files form after grouping.
    pub groups: usize,
}

/// The `@@`-header coordinates of one canonical hunk of a change group
/// (SPEC 10.1). The ranges are the source ranges of SPEC 7.2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChangeRange {
    /// First line of the old side, one-based; zero when that side is empty.
    pub old_start: u64,
    /// Lines the hunk removes from the old side.
    pub old_count: u64,
    /// First line of the new side, one-based; zero when that side is empty.
    pub new_start: u64,
    /// Lines the hunk adds to the new side.
    pub new_count: u64,
}

/// One change group of one detail page (SPEC 7.2): its stable identity, its
/// path and kind, the source ranges it covers, why it is one decision, and
/// its bounded presentation.
#[derive(Clone, Debug)]
pub struct ChangeDetail {
    /// The change identity of this group within this review.
    pub change_id: String,
    /// Literal repository-relative path.
    pub path: String,
    /// The presence transition, as `added`, `deleted`, or `modified`.
    pub kind: &'static str,
    /// Storage mode in B; `None` for an addition.
    pub mode_before: Option<&'static str>,
    /// Storage mode in C; `None` for a deletion.
    pub mode_after: Option<&'static str>,
    /// Full blob object ID in B; `None` for an addition.
    pub oid_before: Option<String>,
    /// Full blob object ID in C; `None` for a deletion.
    pub oid_after: Option<String>,
    /// Whether the content change is opaque (SPEC 10.4).
    pub opaque: bool,
    /// Whether the group is the file's whole-content decision rather than
    /// one independent passage.
    pub whole_file: bool,
    /// Why these hunks form one decision (SPEC 10.3).
    pub reason: String,
    /// The complete canonical hunks of this group, in patch order.
    pub ranges: Vec<ChangeRange>,
    /// How many of the group's source ranges the presentation bound
    /// omitted; zero unless the group overflowed
    /// [`MAX_GROUP_PRESENTATION_BYTES`].
    pub ranges_omitted: usize,
    /// The bounded presentation of the group (SPEC 7.2, 10.4).
    pub presentation: BoundedPresentation,
}

/// The bounded presentation of one change group (SPEC 7.2, 10.4): the
/// heading, exact excerpts of both sides placed by line range, word hints,
/// and the content classes the reader cannot see. Excerpts and hints obey
/// their bounds with explicit truncation data.
#[derive(Clone, Debug)]
pub struct BoundedPresentation {
    /// A human-readable heading with section context.
    pub heading: String,
    /// The exact before excerpt, bounded.
    pub before: BoundedExcerpt,
    /// The exact after excerpt, bounded.
    pub after: BoundedExcerpt,
    /// Word-level hints where the line pairing is reliable, bounded.
    pub word_hints: Vec<markdown::WordHint>,
    /// How many word hints the bound omitted.
    pub word_hints_omitted: usize,
    /// Every byte difference is whitespace.
    pub whitespace_only: bool,
    /// A changed line carries link syntax.
    pub link_change: bool,
    /// A changed line carries trailing whitespace, a tab, or an invisible
    /// character.
    pub invisible_change: bool,
}

/// One exact source excerpt of a group, placed by line range and bounded by
/// [`MAX_PRESENTATION_EXCERPT_BYTES`] (SPEC 7.2).
///
/// Readable text carries the exact bytes as `text`. Opaque bytes carry one
/// bounded hexadecimal representation with the byte length, mirroring the
/// `show` contract for opaque content (SPEC 6.3).
#[derive(Clone, Debug)]
pub struct BoundedExcerpt {
    /// First line, one-based; zero when the side holds no line.
    pub first_line: u64,
    /// Last line, one-based; zero when the side holds no line.
    pub last_line: u64,
    /// Exact byte length of the complete excerpt before bounding.
    pub total_bytes: usize,
    /// Whether the bound truncated the excerpt.
    pub truncated: bool,
    /// The exact bytes while readable text stays inside the bound.
    pub text: Option<String>,
    /// Bounded hexadecimal of the leading bytes when the side is not valid
    /// UTF-8.
    pub hex: Option<String>,
}

/// The semantic data of one decided review plan (SPEC 6.4, 9.3-9.4): the
/// pinned comparison the plan decides, the decisions it records, and the
/// intended result one disposable index proved — the full result tree T and
/// the digest of its exact scoped entries.
///
/// A dry run reports exactly this and writes nothing. The application of a
/// keep-all plan (T031) commits the same proved tree and reports the
/// created commit through [`ReviewDecision::commit`], so both phases share
/// one decision contract; the restoring application (T032) extends the same
/// contract. An exact retry of a request that already completed reports the
/// verified receipt commit instead (SPEC 8.7): that response is assembled
/// from the recorded canonical scope, the immutable Git objects the receipt
/// names, and the context fields and decisions the matching fingerprint
/// pinned, so a present-day scope no new mutation could resolve never hides
/// it. The grouping-engine detail is not reconstructed for that response;
/// [`ReviewDecision::kept`] documents what that changes.
#[derive(Clone, Debug)]
pub struct ReviewDecision {
    /// Request identifier of the plan (SPEC 7.3).
    pub request_id: String,
    /// Identity of the pinned review this plan decides.
    pub review_id: String,
    /// Full adopted branch ref of the comparison.
    pub branch_ref: String,
    /// Full scope digest, such as `sha1:<oid>` (SPEC 8.6).
    pub scope_id: String,
    /// Baseline B of the comparison.
    pub baseline_oid: String,
    /// Candidate C of the comparison.
    pub candidate_oid: String,
    /// Digest of the exact scoped Git entries of C (SPEC 7.2, 8.6).
    pub candidate_scope_digest: String,
    /// The change identities restored. A fresh decision lists them in
    /// manifest order; an existing-receipt response lists the decisions the
    /// matching fingerprint pinned, sorted, because the grouping engine is
    /// not rerun for history (SPEC 8.7).
    pub restored: Vec<String>,
    /// How many groups of the manifest the plan keeps: `Some(count)` for a
    /// fresh decision over the pinned comparison, and `None` for an
    /// existing-receipt response (serialized as JSON `null`). A receipt
    /// records no group count, and acknowledging a completed request must
    /// not regenerate historical group identities with whatever engine the
    /// current build carries (SPEC 8.7), so the count is reported unknown
    /// rather than fabricated.
    pub kept: Option<usize>,
    /// The literal paths the decisions rewrite, sorted by path bytes. A
    /// fresh decision reads them from the proved staged delta; an
    /// existing-receipt response reads the scoped entries that differ
    /// between the verified candidate and result trees, so immutable
    /// objects alone answer (SPEC 8.7).
    pub paths: Vec<String>,
    /// The intended full result tree T (SPEC 9.3). Keep-all holds the
    /// candidate's own tree.
    pub expected_tree: String,
    /// Digest of the exact scoped Git entries of T (SPEC 7.2, 8.6).
    pub result_scope_digest: String,
    /// The effective commit message of the plan (SPEC 7.3).
    pub message: String,
    /// Whether this was a dry run that wrote nothing (SPEC 6.4).
    pub dry_run: bool,
    /// The verified review commit this application created (SPEC 9.4);
    /// `None` for a dry run, and for an application that matched an
    /// earlier receipt instead of writing a new one, which reports that
    /// receipt through `existing_receipt`.
    pub commit: Option<String>,
    /// The verified commit of the receipt this exact request already
    /// produced (SPEC 8.7); `None` when no valid reachable receipt holds
    /// the request. The current observed state stays separate: it is
    /// acceptable that `HEAD` advanced after the operation completed.
    pub existing_receipt: Option<String>,
}

/// The semantic data of one historical restoration (SPEC 7.4, 9.5): the
/// pinned context, the one restored document, the intended result one
/// disposable index proved, and the review baseline the restoration leaves
/// unchanged. A dry run reports the same data without the commit; an exact
/// retry reports the verified receipt of the earlier request instead
/// (SPEC 8.7).
#[derive(Clone, Debug)]
pub struct RestoreOutcome {
    /// Request identifier of the plan (SPEC 7.4).
    pub request_id: String,
    /// Full adopted branch ref of the workspace.
    pub branch_ref: String,
    /// Full scope digest, such as `sha1:<oid>` (SPEC 8.6).
    pub scope_id: String,
    /// The candidate C the restoration replaces (SPEC 9.5).
    pub candidate_oid: String,
    /// Digest of the exact scoped Git entries of C (SPEC 7.2, 8.6).
    pub candidate_scope_digest: String,
    /// The one restored document path.
    pub path: String,
    /// The immutable source revision the content comes from.
    pub revision_oid: String,
    /// The literal paths the restoration rewrites: the document when its
    /// content changes, nothing when it does not.
    pub paths: Vec<String>,
    /// The intended full result tree T (SPEC 9.5).
    pub expected_tree: String,
    /// Digest of the exact scoped Git entries of T (SPEC 7.2, 8.6).
    pub result_scope_digest: String,
    /// The review baseline, which a restoration leaves unchanged (SPEC 9.5):
    /// `None` when first-parent history anchors no valid baseline for the
    /// scope, which no restoration requires.
    pub baseline_oid: Option<String>,
    /// The effective commit message of the plan (SPEC 7.4).
    pub message: String,
    /// Whether this was a dry run that wrote nothing (SPEC 6.4).
    pub dry_run: bool,
    /// The verified restore commit this application created (SPEC 9.5);
    /// `None` for a dry run, and for an application that matched an earlier
    /// receipt instead of writing a new one, which reports that receipt
    /// through `existing_receipt`.
    pub commit: Option<String>,
    /// The verified commit of the receipt this exact request already
    /// produced (SPEC 8.7); `None` when no valid reachable receipt holds
    /// the request. The current observed state stays separate: it is
    /// acceptable that `HEAD` advanced after the operation completed.
    pub existing_receipt: Option<String>,
}

/// The semantic data of one recovery inspection (SPEC 9.7): the report
/// `doctor` returns. The query never mutates — every value below comes from
/// one read-only observation — and the report is the visible recovery
/// preview of that section: the exact head, index tree, and working
/// fingerprint the two recovery commands compare against, next to the
/// recovery argv Margin can assemble safely from those observations.
#[derive(Clone, Debug)]
pub struct RecoveryInspection {
    /// The object format of the inspected repository (SPEC 7.1).
    pub object_format: ObjectFormat,
    /// The commit `HEAD` points at; `None` while unborn.
    pub head: Option<String>,
    /// Whether first-parent history anchors the scope, and where.
    pub baseline: BaselineStatus,
    /// The tree the real index describes; `None` while conflicts stop it.
    pub index_tree: Option<String>,
    /// The digest of the scoped working content (SPEC 8.6); `None` when one
    /// scoped entry cannot honestly join a digest, for example a path a
    /// writer replaced with a symbolic link.
    pub scoped_working_digest: Option<Fingerprint>,
    /// The paths staged against `HEAD`, bounded like every path list the
    /// interfaces spell out.
    pub staged_paths: StagedPaths,
    /// Whether the history of the adopted scope holds a valid receipt
    /// (SPEC 8.7).
    pub receipt: ReceiptPresence,
    /// Bounded recovery argv suggestions (SPEC 9.7): argv arrays assembled
    /// from Margin's own observations, never one shell script and never
    /// text read out of the documents.
    pub recovery_argv: Vec<Vec<String>>,
}

/// The staged paths of one recovery inspection (SPEC 9.7): the whole staged
/// total plus the bounded list the report spells out, in the same shape as
/// the uncaptured paths of one status query (SPEC 6.3).
#[derive(Clone, Debug)]
pub struct StagedPaths {
    /// Every path staged against `HEAD`.
    pub total: usize,
    /// At most [`MAX_LISTED_PATHS`] of them, in Git's path order.
    pub paths: Vec<OsString>,
}

/// What the recovery scan of history established about a receipt
/// (SPEC 8.7, 9.7). `Absent` is an answer only a completed scan may give.
#[derive(Clone, Debug)]
pub enum ReceiptPresence {
    /// The newest valid receipt the first-parent history of the adopted
    /// scope holds. It stays boxed, because the common `Absent` return
    /// stays small.
    Present(Box<ReceiptFinding>),
    /// The complete walk established that no valid receipt is reachable.
    Absent,
    /// The history context no marker may depend on stopped the scan
    /// (SPEC 8.4 rule 7), so absence is not established. The finding names
    /// the mechanism, in the same words the baseline walk reports.
    Unavailable {
        /// What mechanism the context depends on.
        what: &'static str,
    },
}

/// One valid receipt the history holds (SPEC 8.7): the durable record that
/// one named request already completed, which `doctor` reports so a writer
/// whose response was lost can see the operation finished.
#[derive(Clone, Debug)]
pub struct ReceiptFinding {
    /// The verified receipt commit.
    pub commit: String,
    /// `review` or `restore`.
    pub kind: CommitKind,
    /// The scope digest the receipt records.
    pub scope_digest: String,
    /// The `Margin-Request` identifier the receipt records.
    pub request: String,
    /// The `Margin-Plan` fingerprint the receipt records; a valid marker of
    /// a receipt kind always carries one.
    pub plan: Option<String>,
}

/// The semantic data of one local-settings read or update (SPEC 8.2).
#[derive(Clone, Debug)]
pub struct SettingsOutcome {
    /// The effective settings after the operation, with the first-use
    /// defaults where nothing is stored.
    pub settings: LocalSettings,
    /// The entries the operation wrote, in write order. A read and an
    /// explicit no-op apply none.
    pub applied: Vec<AppliedSetting>,
}

/// The semantic data of one document view (SPEC 6.2-6.3): the immutable
/// revision the query pinned, the addressed path, and the content state
/// found there.
#[derive(Clone, Debug)]
pub struct ShowOutcome {
    /// The immutable commit the revision resolved to. Every read below ran
    /// against this canonical object ID, never the user string, so the
    /// answer cannot move while the repository does (SPEC 4.1, 14.1).
    pub revision_oid: String,
    /// The literal repository-relative path after normalization.
    pub path: String,
    /// The object format of the repository (SPEC 7.1).
    pub object_format: ObjectFormat,
    /// The content state of the document at the revision.
    pub content: DocumentContent,
}

/// The content of one document at an immutable revision (SPEC 6.3): an
/// explicit absence, an empty file, readable UTF-8, or opaque bytes — and
/// the exact bytes of an explicit raw export. Absence and emptiness stay
/// distinct states (SPEC 4.1), and each present kind names the blob that
/// holds it, so a reader can pin the bytes it saw.
#[derive(Clone, Debug)]
pub enum DocumentContent {
    /// No document exists at the path in this revision's tree. Absence is
    /// an answer, not a query failure.
    Missing,
    /// The document exists and holds zero bytes.
    Empty {
        /// The blob object ID, the empty blob.
        blob_oid: String,
        /// Storage mode of the entry.
        mode: EntryMode,
    },
    /// The document holds valid UTF-8, bounded for the view.
    Text(BoundedDocumentText),
    /// The document holds bytes that are not valid UTF-8, carried as one
    /// bounded hexadecimal view with byte length and truncation metadata.
    Opaque(BoundedDocumentHex),
    /// Every exact byte of the document, for the explicit raw export of
    /// SPEC 6.3. No bound applies to this variant.
    Exact {
        /// The blob object ID.
        blob_oid: String,
        /// Storage mode of the entry.
        mode: EntryMode,
        /// The exact bytes of the document.
        bytes: Vec<u8>,
    },
}

/// One bounded view of readable UTF-8 content (SPEC 6.3): the text the
/// bound returned with the true totals and the truncation flag.
#[derive(Clone, Debug)]
pub struct BoundedDocumentText {
    /// The blob object ID of the complete document.
    pub blob_oid: String,
    /// Storage mode of the entry.
    pub mode: EntryMode,
    /// The bounded text itself. JSON serialization escapes it as UTF-8;
    /// the text interface display-escapes terminal controls (SPEC 6.3,
    /// 14.2).
    pub text: String,
    /// Byte length of the complete document.
    pub total_bytes: usize,
    /// Bytes the bound returned.
    pub returned_bytes: usize,
    /// Lines of the complete document.
    pub total_lines: u64,
    /// Lines the bound returned.
    pub returned_lines: u64,
    /// Whether any byte or line was dropped by a bound.
    pub truncated: bool,
}

/// One bounded hexadecimal view of opaque content (SPEC 6.3): leading bytes
/// as hexadecimal with the true byte length and the truncation flag. The
/// explicit raw export returns the same document's exact bytes instead.
#[derive(Clone, Debug)]
pub struct BoundedDocumentHex {
    /// The blob object ID of the complete document.
    pub blob_oid: String,
    /// Storage mode of the entry.
    pub mode: EntryMode,
    /// The leading bytes as lowercase hexadecimal pairs, without separators.
    pub hex: String,
    /// Source bytes the hexadecimal view represents.
    pub returned_bytes: usize,
    /// Byte length of the complete document.
    pub total_bytes: usize,
    /// Whether the view truncated the document.
    pub truncated: bool,
}

/// The typed result of one inspection query.
pub type InspectResult = OperationOutcome<WorkspaceInspection>;
/// The typed result of one editorial timeline query.
pub type LogResult = OperationOutcome<LogOutcome>;
/// The typed result of one document view.
pub type ShowResult = OperationOutcome<ShowOutcome>;
/// The typed result of one local-settings read or update.
pub type SettingsResult = OperationOutcome<SettingsOutcome>;
/// The typed result of one scope initialization.
pub type InitResult = OperationOutcome<InitializationOutcome>;
/// The typed result of one scoped checkpoint.
pub type CheckpointResult = OperationOutcome<CheckpointOutcome>;
/// The typed result of one review phase.
pub type ReviewResult = OperationOutcome<ReviewOutcome>;
/// The typed result of one decided review plan: its dry-run validation, and
/// later its application.
pub type ReviewDecisionResult = OperationOutcome<ReviewDecision>;
/// The typed result of one historical restoration.
pub type RestoreResult = OperationOutcome<RestoreOutcome>;
/// The typed result of one recovery inspection.
pub type RecoveryResult = OperationOutcome<RecoveryInspection>;

// ---------------------------------------------------------------------------
// The test-only model-action trace (SPEC 15.7)
// ---------------------------------------------------------------------------

/// The environment variable that turns on the test-only model-action trace:
/// `MARGIN_MODEL_TRACE=<file>` appends one JSON object per model action an
/// operation actually takes, with the commit, tree, and request identities
/// that action carried (SPEC 15.7).
///
/// The trace is refinement evidence, never an input. No operation reads it
/// back, a write that fails is dropped silently, and one record is one
/// flushed append, so a killed process leaves exactly the actions it
/// completed. `formal/refinement.md` defines the record set, and
/// `tests/refinement_trace.rs` validates whole sequences against the
/// allowed transitions of `formal/MarginReview.tla` with an independent
/// checker that shares no code with this layer.
pub const MODEL_TRACE_VARIABLE: &str = "MARGIN_MODEL_TRACE";

/// One open model-action trace file (SPEC 15.7).
pub struct ModelTrace {
    /// The file every record appends to.
    target: PathBuf,
}

impl ModelTrace {
    /// Returns the trace the environment selects, or `None` when the
    /// variable is absent. This is the executable path; an in-process test
    /// injects its target through [`Operations::with_model_trace`], because
    /// the shared test process cannot swap the environment per case.
    pub fn from_env() -> Option<ModelTrace> {
        std::env::var_os(MODEL_TRACE_VARIABLE).map(|target| ModelTrace {
            target: PathBuf::from(target),
        })
    }

    /// Returns the trace that appends every record to `target`.
    pub fn to_file(target: impl Into<PathBuf>) -> ModelTrace {
        ModelTrace {
            target: target.into(),
        }
    }

    /// Appends one record. Tracing never fails an operation, so an
    /// unwritable target drops the record instead of changing a result.
    fn record(&self, fields: serde_json::Value) {
        let mut line = fields.to_string();
        line.push('\n');
        let _ = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.target)
            .and_then(|mut file| std::io::Write::write_all(&mut file, line.as_bytes()));
    }

    /// `Init`: one verified starting marker established the modeled initial
    /// state (SPEC 15.7).
    fn initial(&self, head: &str, tree: &str) {
        self.record(serde_json::json!({
            "action": "Init",
            "head": head,
            "tree": tree,
        }));
    }

    /// `Checkpoint(staged)`: one verified draft commit over `parent`.
    fn checkpoint(&self, include_staged: bool, head: &str, parent: &str, tree: &str) {
        self.record(serde_json::json!({
            "action": "Checkpoint",
            "include_staged": include_staged,
            "head": head,
            "parent": parent,
            "tree": tree,
        }));
    }

    /// `Submit` that accepted one complete plan and pinned its comparison:
    /// the candidate `head`, the baseline, and the groups the application
    /// will restore, in application order.
    fn submit_accepted(
        &self,
        mode: &str,
        request: &str,
        head: &str,
        baseline: &str,
        restore: &[String],
    ) {
        self.record(serde_json::json!({
            "action": "Submit",
            "mode": mode,
            "outcome": "accepted",
            "request": request,
            "head": head,
            "baseline": baseline,
            "restore": restore,
        }));
    }

    /// `Submit` that answered one already completed request with its
    /// verified receipt.
    fn submit_receipt(&self, request: &str, receipt: &str) {
        self.record(serde_json::json!({
            "action": "Submit",
            "outcome": "receipt",
            "request": request,
            "receipt": receipt,
        }));
    }

    /// `Submit` that refused one reused request identity whose plan differs.
    fn submit_conflict(&self, request: &str) {
        self.record(serde_json::json!({
            "action": "Submit",
            "outcome": "conflict",
            "request": request,
        }));
    }

    /// `ApplyGroup(p)`: one complete reverse unit applied to the working
    /// tree while the candidate stayed pinned at `head`.
    fn apply_group(&self, request: &str, group: &str, head: &str) {
        self.record(serde_json::json!({
            "action": "ApplyGroup",
            "request": request,
            "group": group,
            "head": head,
        }));
    }

    /// `UpdateIndex`: the applied index verified as the intended tree.
    fn update_index(&self, request: &str, head: &str, tree: &str) {
        self.record(serde_json::json!({
            "action": "UpdateIndex",
            "request": request,
            "head": head,
            "tree": tree,
        }));
    }

    /// `Publish`: one verified review commit over `parent`.
    fn publish(&self, request: &str, head: &str, parent: &str, tree: &str) {
        self.record(serde_json::json!({
            "action": "Publish",
            "request": request,
            "head": head,
            "parent": parent,
            "tree": tree,
        }));
    }

    /// `Respond`: one response that names its verified receipt.
    fn respond(&self, request: &str, receipt: &str) {
        self.record(serde_json::json!({
            "action": "Respond",
            "request": request,
            "receipt": receipt,
        }));
    }

    /// `Fail`: one accepted submission that failed inside its protocol and
    /// left recoverable state behind.
    fn fail(&self, request: &str) {
        self.record(serde_json::json!({
            "action": "Fail",
            "request": request,
        }));
    }

    /// `InspectRecovery`: one completed read-only recovery inspection.
    fn inspected_recovery(&self, head: Option<&str>) {
        self.record(serde_json::json!({
            "action": "InspectRecovery",
            "head": head,
        }));
    }
}

/// The shared operation layer (SPEC 13.2, 13.6).
///
/// The CLI serializes these results, the TUI renders them, and a hook
/// adapter translates the subset it needs. Every interface calls the same
/// operations, so no interface chooses its own guards, scope rules, or
/// approval meaning. Construction stays direct: one Git adapter, borrowed
/// for the lifetime of the layer.
pub struct Operations<'a> {
    /// The one Git process adapter every operation shares.
    git: &'a Git,
    /// The one prefetched history walk this process reuses (SPEC 17.5): a
    /// long-lived surface that observes every two seconds re-derives
    /// nothing it already proved, while every mutation reloads what it
    /// needs from the commits it just created. `None` until a walk first
    /// runs, because building one costs processes an operation that never
    /// walks should not pay.
    walk: std::sync::Mutex<Option<HistoryWalk>>,
    /// The test-only model-action trace of SPEC 15.7, when one is attached.
    trace: Option<ModelTrace>,
}

// ---------------------------------------------------------------------------
// The shared mutation lock (SPEC 9.1 step 2)
// ---------------------------------------------------------------------------

/// The scoped guard of the application mutation lock of one Git directory
/// (SPEC 9.1 step 2, 14.3).
///
/// One nonblocking operating-system lock on [`MUTATION_LOCK_FILE`]
/// serializes every Margin write of one repository: the working tree, the
/// index, the refs, and the local configuration are one shared state that
/// two concurrent captures could interleave over. The guard is held from
/// the mutation inspection through the postcondition inspection and the
/// settings persistence; dropping it releases the operating-system lock, so
/// every return path — success, refusal, or observed failure — ends with
/// the lock gone. The lock coordinates Margin processes only: it does not
/// lock Git itself, an editor, or an agent (SPEC 14.3). It is never
/// approximated by a stale PID file, because the lock lives exactly as
/// long as the open file description of the guard.
struct MutationGuard {
    /// The locked open file. Closing the descriptor would release the lock
    /// as well; the explicit unlock states the release where it happens.
    lock: fs::File,
}

impl Drop for MutationGuard {
    fn drop(&mut self) {
        let _ = self.lock.unlock();
    }
}

impl<'a> Operations<'a> {
    /// Builds the operation layer over `git`.
    pub fn new(git: &'a Git) -> Operations<'a> {
        Operations {
            git,
            walk: std::sync::Mutex::new(None),
            trace: ModelTrace::from_env(),
        }
    }

    /// The process's one history walk, created on first use and reused for
    /// the process's whole life (SPEC 17.5). The context answer is
    /// forgotten per observation so rule 7 stays fresh; commits, diffs,
    /// and digests are content-addressed and stay valid.
    fn history_walk(&self) -> std::sync::MutexGuard<'_, Option<HistoryWalk>> {
        let mut walk = self.walk.lock().expect("the walk is uncontended");
        if walk.is_none() {
            *walk = Some(HistoryWalk::new(self.git));
        }
        if let Some(walk) = walk.as_mut() {
            walk.forget_context();
        }
        walk
    }

    /// Builds the operation layer over `git` with one explicit model-action
    /// trace (SPEC 15.7), so an in-process test drives the same records the
    /// environment variable gives the executable.
    pub fn with_model_trace(git: &'a Git, trace: ModelTrace) -> Operations<'a> {
        Operations {
            git,
            walk: std::sync::Mutex::new(None),
            trace: Some(trace),
        }
    }

    /// The Git adapter this layer uses; interfaces never spawn Git itself.
    pub fn git(&self) -> &Git {
        self.git
    }

    /// Acquires the shared mutation lock of this Git directory (SPEC 9.1
    /// step 2).
    ///
    /// The lock file is created when absent and stays open for the lifetime
    /// of the returned guard. A competing Margin operation is refused
    /// `BUSY` with no intended effect; the identical retry is safe and
    /// useful once the owner finishes (SPEC 7.5). Opening or locking the
    /// file can still fail for ordinary filesystem reasons, which reports
    /// one typed execution failure instead of a serialization claim nobody
    /// holds.
    ///
    /// Bootstrap boundary: a repository that does not exist yet has no Git
    /// directory to lock, so `init --create-repo` creates the repository
    /// first and then acquires this lock for adoption and everything after
    /// it (SPEC 5.1). Two concurrent creations both run `git init` — an
    /// idempotent reinitialization — and then serialize here, where each
    /// operation's guards reject whatever the first one changed.
    ///
    /// Receipt lookup runs after this acquisition and before the
    /// present-day eligibility inspection below, so an already completed
    /// request still returns its receipt instead of a stale-state refusal
    /// (SPEC 9.1 step 3, 8.7). The review application reaches it through
    /// [`Operations::lookup_request_receipt`]; the restoring application
    /// and the historical restoration arrive with tasks T032 and T033, and
    /// the dry-run validation reports the same lookup without a lock,
    /// because it writes nothing.
    fn acquire_mutation_lock(&self) -> Result<MutationGuard, OperationError> {
        let path = self
            .git
            .git_path(MUTATION_LOCK_FILE)
            .map_err(OperationError::from)?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| lock_file_failure(&path, error))?;
        match lock.try_lock() {
            Ok(()) => Ok(MutationGuard { lock }),
            Err(fs::TryLockError::WouldBlock) => Err(busy_refusal()),
            Err(fs::TryLockError::Error(error)) => Err(lock_file_failure(&path, error)),
        }
    }

    /// The shared mutation inspection of every locked write (SPEC 9.1
    /// step 4): the repository and storage policy in guard order, then the
    /// adopted branch of the local settings. Returns the branch `HEAD` is
    /// attached to, the value the caller rechecks immediately before its
    /// first real mutation (SPEC 9.1 step 6).
    ///
    /// A detached `HEAD` keeps the repository policy's own refusal because
    /// the branch comparison runs after it. A `HEAD` on another branch than
    /// the configured one — even at the same commit — refuses
    /// `BRANCH_CHANGED` before any index write: Margin never switches or
    /// re-adopts a branch on its own, and one explicit `init` remains the
    /// path that adopts another branch (SPEC 8.2, INV-08). Without a
    /// configured branch there is nothing to compare.
    fn enforce_mutation_guards(
        &self,
        resolved: &ResolvedScope,
        settings: &LocalSettings,
    ) -> Result<Option<String>, OperationError> {
        let report =
            enforce_mutation_policy(self.git, Some(resolved)).map_err(OperationError::from)?;
        if !report.ok() {
            return Err(OperationError::new(
                report.blockers[0].public_code(),
                blockers_message(&report.blockers),
            ));
        }
        let branch_ref = self.git.head_symbolic_ref().map_err(OperationError::from)?;
        if let Some(blocker) = adopted_branch_mismatch(settings, branch_ref.as_deref()) {
            return Err(adopted_branch_refusal(&blocker));
        }
        Ok(branch_ref)
    }

    /// The shared mutation inspection with the one index exception of the
    /// explicit recovery paths (SPEC 9.1 step 4, 9.7): every guard of
    /// [`Operations::enforce_mutation_guards`] except the staged delta,
    /// which is the subject both recovery operations run over — an
    /// exact-plan resume commits the retained result that stays staged
    /// against its candidate, and a guarded staged capture adopts the
    /// inspected staged state itself.
    ///
    /// The exception admits the blocker, never the staging: each caller
    /// proves the whole index against its own pinned state, so unrelated
    /// or partial staged content still refuses there. Conflict entries,
    /// in-progress Git operations, storage transformations, a detached
    /// `HEAD`, and a branch other than the adopted one all refuse here as
    /// they do for every other write.
    fn enforce_mutation_guards_except_staged(
        &self,
        resolved: &ResolvedScope,
        settings: &LocalSettings,
    ) -> Result<Option<String>, OperationError> {
        let report =
            enforce_mutation_policy(self.git, Some(resolved)).map_err(OperationError::from)?;
        let blockers: Vec<PolicyBlocker> = report
            .blockers
            .into_iter()
            .filter(|blocker| !matches!(blocker, PolicyBlocker::StagedChanges { .. }))
            .collect();
        if !blockers.is_empty() {
            return Err(OperationError::new(
                blockers[0].public_code(),
                blockers_message(&blockers),
            ));
        }
        let branch_ref = self.git.head_symbolic_ref().map_err(OperationError::from)?;
        if let Some(blocker) = adopted_branch_mismatch(settings, branch_ref.as_deref()) {
            return Err(adopted_branch_refusal(&blocker));
        }
        Ok(branch_ref)
    }

    /// Finds the receipt of one decided request in the history reachable
    /// from the current candidate (SPEC 8.7, 9.1 step 3).
    ///
    /// The walk starts at `HEAD`: the adopted branch in every ordinary
    /// workspace, and the candidate a retry is checked against. A receipt
    /// the current history no longer reaches is not durably promised, so the
    /// expected-candidate guards of the caller still apply after an
    /// [`ReceiptLookup::Absent`]. An unborn branch holds no history at all,
    /// so it holds no receipt either.
    ///
    /// The scan runs before the present-day eligibility inspection, so
    /// dirty files, staged content, writer activity, and storage-policy
    /// blockers never hide an already validated receipt. A scan that cannot
    /// complete is one typed failure: it never claims the request absent,
    /// so a new receipt-producing mutation stays blocked (SPEC 8.7).
    fn lookup_request_receipt(
        &self,
        kind: CommitKind,
        request_id: &str,
        fingerprint: &str,
    ) -> Result<ReceiptLookup, OperationError> {
        let head = match self.git.head_commit() {
            Ok(Some(head)) => head,
            Ok(None) => return Ok(ReceiptLookup::Absent),
            Err(error) => return Err(OperationError::from(error)),
        };
        let mut walk = self.history_walk();
        let walk = walk.as_mut().expect("the constructor fills the walk");
        walk.find_receipt(&head, kind, request_id, fingerprint)
            .map_err(|error| {
            OperationError {
                code: error.public_code(),
                message: format!(
                    "the receipt scan did not complete, so the request {request_id} cannot be \
                     shown absent and the mutation stays blocked: {error}"
                ),
                retryable: error.retryable(),
                recovery_argv: Vec::new(),
            }
            .recovery(&["margin", "status", "--json"])
        })
    }

    /// Inspects one workspace (SPEC 13.2 `inspect_workspace`, 4.2, 6.3).
    ///
    /// The query reads the effective local settings, resolves the scope
    /// through the one resolver, runs the mutation policy guards read-only,
    /// and selects the baseline through the one walk. It then reports the
    /// three separate state dimensions with the pending and uncaptured
    /// counts: preservation against the candidate, review against the
    /// baseline, and the operation state the repository itself asks for.
    /// It creates no refs, index entries, commits, or file writes, and its
    /// effects report exactly that. Scope blockers, policy blockers, and an
    /// unavailable baseline are observations the data carries; they are not
    /// failures of the query. A branch other than the adopted one is the
    /// same kind of observation: one actionable blocker that names the
    /// explicit `init` that adopts the branch (SPEC 8.2).
    pub fn inspect_workspace(&self, request: &InspectRequest) -> InspectResult {
        if let Err(error) = require_work_tree(self.git) {
            return InspectResult::failure(error, Effects::unchanged());
        }
        let settings = match self.effective_settings() {
            Ok(settings) => settings,
            Err(error) => return InspectResult::failure(error, Effects::unchanged()),
        };
        let Some(input) = scope_input_of(request.scope.as_ref(), &settings) else {
            return InspectResult::failure(not_initialized(), Effects::unchanged());
        };
        let resolved = match resolve_scope(self.git, &input) {
            Ok(resolved) => resolved,
            Err(error) => {
                return InspectResult::failure(OperationError::from(error), Effects::unchanged());
            }
        };
        let mut policy_blockers = match enforce_mutation_policy(self.git, Some(&resolved)) {
            Ok(report) => report.blockers,
            Err(error) => {
                return InspectResult::failure(OperationError::from(error), Effects::unchanged());
            }
        };
        let object_format = match self.git.object_format() {
            Ok(format) => format,
            Err(error) => {
                return InspectResult::failure(OperationError::from(error), Effects::unchanged());
            }
        };
        let branch = match self.git.head_symbolic_ref() {
            Ok(branch) => branch,
            Err(error) => {
                return InspectResult::failure(OperationError::from(error), Effects::unchanged());
            }
        };
        // The adopted branch belongs to the workspace the query explains:
        // a branch other than the configured one is one actionable blocker,
        // never a query failure, and the read-only query switches nothing
        // (SPEC 8.2, 6.3).
        if let Some(blocker) = adopted_branch_mismatch(&settings, branch.as_deref()) {
            policy_blockers.push(blocker);
        }
        let head = match self.git.head_commit() {
            Ok(head) => head,
            Err(error) => {
                return InspectResult::failure(OperationError::from(error), Effects::unchanged());
            }
        };
        let baseline = match &head {
            Some(candidate) => {
                let mut guard = self.history_walk();
                let walk = guard.as_mut().expect("the constructor fills the walk");
                match walk.select_baseline(candidate, &resolved.digest) {
                    Ok(marker) => BaselineStatus::Anchored(Box::new(marker)),
                    Err(BaselineError::Unavailable(reason)) => {
                        BaselineStatus::Unavailable(reason)
                    }
                    Err(error) => {
                        return InspectResult::failure(
                            OperationError::from(error),
                            Effects::unchanged(),
                        );
                    }
                }
            },
            None => BaselineStatus::NoCandidate,
        };

        // Preservation (SPEC 4.2): the observed working content against the
        // selected candidate. Storage transformation blockers make the raw
        // bytes and the stored blobs incomparable, so the only honest
        // answer is `unknown` (SPEC 8.8).
        let transformed = storage_transformed(&policy_blockers);
        let (preservation, uncaptured, uncaptured_reason) = match (&head, transformed) {
            (Some(candidate), false) => {
                match uncaptured_paths_of(self.git, &resolved, candidate, &request.path_filter) {
                    Ok(uncaptured) => {
                        let state = if uncaptured.total == 0 {
                            PreservationState::Captured
                        } else {
                            PreservationState::Uncaptured
                        };
                        (state, Some(uncaptured), None)
                    }
                    Err(error) => {
                        return InspectResult::failure(
                            OperationError::from(error),
                            Effects::unchanged(),
                        );
                    }
                }
            }
            (Some(_), true) => (
                PreservationState::Unknown,
                None,
                Some(COUNT_REASON_STORAGE_TRANSFORMATION),
            ),
            // With no candidate, no scoped content is in any commit; the
            // paths that hold working bytes are the uncaptured ones.
            (None, _) => (
                PreservationState::Uncaptured,
                Some(uncaptured_without_candidate(
                    &resolved,
                    &request.path_filter,
                )),
                None,
            ),
        };

        // Review (SPEC 4.2): scoped differences against the valid baseline.
        // The count stays honest when no baseline exists: `None` with one
        // reason, never a fabricated zero (SPEC 6.3).
        let (pending_files, pending_files_reason) = match &baseline {
            BaselineStatus::Anchored(marker) => {
                let candidate = head.as_ref().expect("an anchored baseline has a candidate");
                match scoped_changed_paths(
                    self.git,
                    &resolved.descriptor,
                    &marker.commit,
                    candidate,
                ) {
                    Ok(paths) => (Some(paths.len()), None),
                    Err(error) => {
                        return InspectResult::failure(
                            OperationError::from(error),
                            Effects::unchanged(),
                        );
                    }
                }
            }
            BaselineStatus::Unavailable(_) => (None, Some(COUNT_REASON_BASELINE_UNAVAILABLE)),
            BaselineStatus::NoCandidate => (None, Some(COUNT_REASON_NO_CANDIDATE)),
        };
        let review = match (&baseline, pending_files) {
            (BaselineStatus::Anchored(marker), Some(0)) => {
                if marker.kind == CommitKind::Review {
                    ReviewState::Reviewed
                } else {
                    // The anchor is the starting point, and a starting
                    // point is a comparison anchor, never approval
                    // (SPEC 4.1).
                    ReviewState::StartingPoint
                }
            }
            (BaselineStatus::Anchored(_), Some(_)) => ReviewState::Pending,
            (BaselineStatus::Anchored(_), None) | (BaselineStatus::NoCandidate, _) => {
                ReviewState::Blocked
            }
            (BaselineStatus::Unavailable(_), _) => ReviewState::Blocked,
        };

        InspectResult::success(
            WorkspaceInspection {
                object_format,
                branch,
                head,
                settings,
                scope: ScopeSummary::of(&resolved),
                baseline,
                operation: operation_state_of(&policy_blockers),
                policy_blockers,
                preservation,
                review,
                counts: StatusCounts {
                    pending_files,
                    pending_files_reason,
                    pending_groups: None,
                    pending_groups_reason: Some(COUNT_REASON_GROUPING_UNAVAILABLE),
                    uncaptured,
                    uncaptured_reason,
                },
                limits: InspectionLimits::current(),
            },
            Effects::unchanged(),
        )
    }

    /// Reads the effective local Margin settings (SPEC 8.2, 6.2).
    ///
    /// Every interface that shows settings — the diagnostic status, the
    /// TUI, and a hook adapter — calls this one operation, so none of them
    /// reads Git configuration on its own. The query writes nothing and
    /// exposes no environment secrets.
    pub fn read_settings(&self) -> SettingsResult {
        if let Err(error) = require_work_tree(self.git) {
            return SettingsResult::failure(error, Effects::unchanged());
        }
        match read_local_settings(self.git) {
            Ok(settings) => SettingsResult::success(
                SettingsOutcome {
                    settings,
                    applied: Vec::new(),
                },
                Effects::unchanged(),
            ),
            Err(error) => {
                SettingsResult::failure(OperationError::from(error), Effects::unchanged())
            }
        }
    }

    /// Applies one explicit local-settings update (SPEC 8.2, 6.2).
    ///
    /// Every interface that changes settings — the setup inputs of `init`,
    /// the TUI, and a hook adapter — calls this one operation. An empty
    /// update is one explicit no-op; the settings task of initialization
    /// composes this operation with its own commit-first ordering. The
    /// write is one Margin write of the repository, so it takes the shared
    /// mutation lock and holds it through the write (SPEC 9.1 step 2).
    pub fn update_settings(&self, update: &SettingsUpdate) -> SettingsResult {
        if let Err(error) = require_work_tree(self.git) {
            return SettingsResult::failure(error, Effects::unchanged());
        }
        let _lock = match self.acquire_mutation_lock() {
            Ok(lock) => lock,
            Err(error) => return SettingsResult::failure(error, Effects::unchanged()),
        };
        let mut effects = Effects::unchanged();
        match update_local_settings(self.git, update, &mut effects) {
            Ok((settings, applied)) => {
                SettingsResult::success(SettingsOutcome { settings, applied }, effects)
            }
            Err(error) => SettingsResult::failure(OperationError::from(error), effects),
        }
    }

    /// Initializes one writing scope (SPEC 13.2 `initialize_scope`, 5.1).
    ///
    /// The operation adopts the repository of the adapter, or creates one
    /// through ordinary `git init` only when the interface explicitly asks
    /// (`--create-repo`); it never creates a Git identity and never switches
    /// the branch of an existing repository. `branch` asserts the adopted
    /// branch there and names the initial branch of a created repository.
    ///
    /// The guards refuse before anything is staged: existing staged changes,
    /// scope blockers, the repository and storage policies of the shared
    /// guard, the raw-byte identity profile, and a missing author identity.
    ///
    /// First use then captures the eligible scoped working content,
    /// deletions included, in exactly one verified `start` marker: a
    /// zero-parent commit with `Margin-Candidate: unborn` on an unborn
    /// branch, otherwise a child of the current `HEAD` that changes no
    /// out-of-scope entry and may be empty when the tree is already saved.
    /// Only after that commit verifies do the local settings persist
    /// (SPEC 8.2); a settings failure then reports the partial success with
    /// its commit. A capture that fails after its first real write keeps
    /// the partial state and withholds every setting: the failure reports
    /// the inspected head, index, and working files, and names a created
    /// commit with its object ID even when its metadata did not verify
    /// (SPEC 9.1 step 7, 9.6). A dry run computes the intended tree through
    /// a disposable index and reports the planned settings without writing;
    /// a repository that does not exist yet is proposed, never created
    /// (SPEC 6.4).
    ///
    /// Repeating the operation follows SPEC 8.2: with the same branch and
    /// scope already anchored by a valid matching marker and no
    /// `--new-start`, it changes only the settings the request supplies and
    /// adds no start commit, so changing the capture policy alone preserves
    /// the baseline. A new or changed scope, an unavailable anchor, or an
    /// explicit `--new-start` creates a fresh starting marker instead. The
    /// baseline walk never crosses a start marker, so scope A → B → A uses
    /// its newly established A anchor and never revives the older approval.
    ///
    /// The whole operation — the repeat path's settings write included —
    /// holds the shared mutation lock of SPEC 9.1 step 2, acquired after
    /// the repository is adopted or created and kept through the
    /// postcondition inspection and the settings persistence. Creation is
    /// the one bootstrap boundary outside the lock: no Git directory
    /// exists to lock until `git init` returns. A competing Margin
    /// operation is refused `BUSY` with no intended effects. The fresh
    /// capture proves its intended tree in one disposable index before the
    /// real index is staged, rechecks the branch, the scope, `HEAD`, the
    /// index, and the scoped working content immediately before staging
    /// (SPEC 9.1 step 6), and names the proved tree as its
    /// `Margin-Result-Tree`, so the shared marker writer compares the real
    /// staged tree with that result before committing.
    pub fn initialize_scope(&self, request: &InitRequest) -> InitResult {
        let mut effects = Effects::unchanged();
        let failure =
            |error: OperationError, effects: &Effects| InitResult::failure(error, effects.clone());

        // Adopt or create the repository (SPEC 5.1). An existing repository
        // is adopted as found; creation is one explicit request, and its
        // effect is reported even when a later step fails (SPEC 6.4).
        let inside = match self.git.inside_repository() {
            Ok(inside) => inside,
            Err(error) => return failure(OperationError::from(error), &effects),
        };
        let mut repository_created = false;
        if !inside {
            if !request.create_repository {
                return failure(no_repository(), &effects);
            }
            if request.dry_run {
                return self.proposed_initialization(request);
            }
            if let Err(error) = self.git.init_repository(request.branch.as_deref()) {
                return failure(OperationError::from(error), &effects);
            }
            repository_created = true;
            effects.observe_repository(RepositoryEffect::Created);
        }

        if let Err(error) = require_work_tree(self.git) {
            return failure(error, &effects);
        }

        // Adopt the current branch; `--branch` asserts it (SPEC 5.1).
        let branch_ref = match self.git.head_symbolic_ref() {
            Ok(branch_ref) => branch_ref,
            Err(error) => return failure(OperationError::from(error), &effects),
        };
        if let (Some(expected), Some(current)) = (&request.branch, &branch_ref) {
            let adopted = current.strip_prefix("refs/heads/").unwrap_or(current);
            if expected != adopted {
                return failure(branch_mismatch(expected, adopted), &effects);
            }
        }

        // Resolve the scope through the one resolver; a scope blocker is
        // never silently narrowed away by adoption (SPEC 3.2 rule 5).
        let input = match self.context_scope_input(request.scope.as_ref()) {
            Ok(input) => input,
            Err(error) => return failure(error, &effects),
        };
        let resolved = match resolve_scope(self.git, &input) {
            Ok(resolved) => resolved,
            Err(error) => return failure(OperationError::from(error), &effects),
        };
        if !resolved.blockers.is_empty() {
            return failure(scope_blockers_refusal(&resolved.blockers), &effects);
        }

        // SPEC 9.1 step 2: the shared mutation lock serializes this
        // initialization — the repeat path that changes only settings
        // included — with every other Margin write of the repository, and
        // is held through the postcondition inspection and the settings
        // persistence below. A repository that does not exist yet was
        // created above, outside the lock, because no Git directory
        // existed to lock first; adoption and everything after it are
        // serialized (SPEC 5.1). A dry run proposes without writing, so it
        // reserves nothing (SPEC 6.4).
        let _lock = if request.dry_run {
            None
        } else {
            match self.acquire_mutation_lock() {
                Ok(lock) => Some(lock),
                Err(error) => return failure(error, &effects),
            }
        };

        // The shared mutation guards (SPEC 9.1 step 4), the raw-byte profile
        // of SPEC 8.8, and the author identity Margin never writes itself.
        let report = match enforce_mutation_policy(self.git, Some(&resolved)) {
            Ok(report) => report,
            Err(error) => return failure(OperationError::from(error), &effects),
        };
        if !report.ok() {
            return failure(
                OperationError::new(
                    report.blockers[0].public_code(),
                    blockers_message(&report.blockers),
                ),
                &effects,
            );
        }
        match verify_identity_profile(self.git, &resolved) {
            Ok(blockers) if blockers.is_empty() => {}
            Ok(blockers) => {
                return failure(
                    OperationError::new(blockers[0].public_code(), blockers_message(&blockers)),
                    &effects,
                );
            }
            Err(error) => return failure(OperationError::from(error), &effects),
        }
        if let Err(error) = self.git.author_ident() {
            if matches!(error, GitError::Exit { .. }) {
                return failure(missing_identity(), &effects);
            }
            return failure(OperationError::from(error), &effects);
        }

        // The anchor decision of SPEC 8.2: a valid matching anchor at the
        // candidate without `--new-start` makes this a repeat initialization
        // that changes only the supplied settings. A new or changed scope,
        // an unavailable anchor, or an explicit `--new-start` falls through
        // to the fresh starting marker below, which resets comparison
        // without claiming approval. A context no marker can validate under
        // makes any anchor impossible, including one created here.
        let head = match self.git.head_commit() {
            Ok(head) => head,
            Err(error) => return failure(OperationError::from(error), &effects),
        };
        if let Some(candidate) = &head
            && !request.new_start
        {
            match select_baseline(self.git, candidate, &resolved.digest) {
                Ok(_) => {
                    return self.repeat_initialization(request, &branch_ref, &resolved, effects);
                }
                Err(BaselineError::Unavailable(BaselineUnavailable::Context { what })) => {
                    // No marker can validate under this context, so no
                    // anchor the initialization created could ever anchor
                    // anything either (SPEC 8.4 rule 7, 8.5).
                    let error = BaselineError::Unavailable(BaselineUnavailable::Context { what });
                    return failure(OperationError::from(error), &effects);
                }
                // The walk found no valid matching anchor: first use of this
                // scope, a changed scope, or a missing anchor.
                Err(BaselineError::Unavailable(_)) => {}
                Err(error) => return failure(OperationError::from(error), &effects),
            }
        }

        // The intended starting tree: the scoped working content staged over
        // the empty tree of an unborn branch or the candidate's tree. A dry
        // run computes it in one disposable index and writes nothing.
        let paths: Vec<OsString> = resolved.paths.keys().cloned().collect();
        if request.dry_run {
            let tree = match intended_start_tree(self.git, head.as_deref(), &paths) {
                Ok(tree) => tree,
                Err(error) => return failure(OperationError::from(error), &effects),
            };
            let update = adoption_settings(request, &branch_ref, &resolved.descriptor);
            let planned = match plan_local_settings(self.git, &update) {
                Ok((_settings, writes)) => writes,
                Err(error) => return failure(OperationError::from(error), &effects),
            };
            return InitResult::success(
                InitializationOutcome {
                    start_commit: None,
                    repository_created: false,
                    settings: planned,
                    dry_run: true,
                    expected_tree: Some(tree),
                    tree_reason: None,
                    proposed: None,
                },
                effects,
            );
        }

        // The working-entry baseline for post-failure observation (SPEC 9.1
        // step 7), read before the first real mutation.
        let working_before = match scoped_working_state(self.git, &resolved) {
            Ok(before) => before,
            Err(error) => return failure(OperationError::from(error), &effects),
        };

        // The intended starting tree, proved in one disposable index before
        // the real index is touched (SPEC 9.1 step 5): the commit names this
        // tree as its `Margin-Result-Tree`, and the shared marker writer
        // compares the real staged tree with that result before committing,
        // exactly as the checkpoint proof already does (SPEC 9.2).
        let tree = match intended_start_tree(self.git, head.as_deref(), &paths) {
            Ok(tree) => tree,
            Err(error) => return failure(OperationError::from(error), &effects),
        };

        // The precondition recheck immediately before the first real
        // mutation (SPEC 9.1 step 6): anything that moved refuses before
        // the real index is staged.
        if let Err(error) = self.recheck_initialization_preconditions(
            &input,
            &resolved,
            &branch_ref,
            head.as_deref(),
            &working_before,
        ) {
            return failure(error, &effects);
        }

        // Stage exactly the scoped set and capture the starting commit.
        if let Err(error) = self.git.stage_paths(&paths) {
            let error = OperationError::from(error);
            self.observe_capture_failure(
                &mut effects,
                head.as_deref(),
                &working_before,
                request.scope.as_ref(),
            );
            return failure(error, &effects);
        }
        effects.observe_index(ObservedEffect::Changed);
        let metadata = CommitMetadata {
            kind: CommitKind::Start,
            scope_digest: resolved.digest.id(),
            scope_spec: String::from_utf8(resolved.descriptor.canonical_bytes())
                .expect("the canonical descriptor is UTF-8 JSON"),
            candidate: head
                .as_deref()
                .map_or_else(|| domain::CANDIDATE_UNBORN.to_string(), str::to_string),
            result_tree: tree.clone(),
            baseline: None,
            request: None,
            plan: None,
            source: None,
        };
        let marker = match commit_marker(self.git, &metadata, "") {
            Ok(marker) => marker,
            Err(error) => {
                // Inspect what the failed capture left, then keep the commit
                // the failure itself names: a commit whose observed state
                // does not match the prepared map still exists as ordinary
                // history, while a killed commit process loses the outcome,
                // which is never narrowed back to `none` (SPEC 9.6, 7.1).
                let observed = self.observe_capture_failure(
                    &mut effects,
                    head.as_deref(),
                    &working_before,
                    request.scope.as_ref(),
                );
                let start = match &error {
                    MarkerCommitError::Verification { commit, .. } => {
                        effects.observe_commit(CommitEffect::Created, Some(commit.clone()));
                        Some(commit.clone())
                    }
                    MarkerCommitError::Git(GitError::Timeout { .. }) => {
                        effects.observe_commit(CommitEffect::Unknown, None);
                        None
                    }
                    _ => observed,
                };
                // The partial state stays and the settings adoption is
                // withheld: no configuration is written above an invalid
                // start marker (SPEC 8.2, 9.6).
                return InitResult::observed_failure(
                    OperationError::from(error),
                    InitializationOutcome {
                        start_commit: start,
                        repository_created,
                        settings: Vec::new(),
                        dry_run: false,
                        expected_tree: Some(tree.clone()),
                        tree_reason: None,
                        proposed: None,
                    },
                    effects,
                );
            }
        };
        effects.observe_commit(CommitEffect::Created, Some(marker.commit.clone()));
        if let Some(trace) = self.trace.as_ref() {
            trace.initial(&marker.commit, &marker.result_tree);
        }

        // Persist the settings only after the verified commit (SPEC 8.2); a
        // failure reports the partial success and shows the commit.
        let update = adoption_settings(request, &branch_ref, &resolved.descriptor);
        let observed = |settings: Vec<AppliedSetting>| InitializationOutcome {
            start_commit: Some(marker.commit.clone()),
            repository_created,
            settings,
            dry_run: false,
            expected_tree: Some(marker.result_tree.clone()),
            tree_reason: None,
            proposed: None,
        };
        match update_local_settings(self.git, &update, &mut effects) {
            Ok((_settings, applied)) => InitResult::success(observed(applied), effects),
            Err(error) => InitResult::observed_failure(
                OperationError::from(error),
                observed(Vec::new()),
                effects,
            ),
        }
    }

    /// The repeat half of initialization (SPEC 8.2): the resolved scope
    /// already holds a valid matching anchor, so the operation changes only
    /// the settings the request supplies through one shared settings update
    /// and adds no start commit.
    ///
    /// A dry run plans the same update without writing. The retry of an
    /// initialization whose configuration write failed adopts its validated
    /// starting marker here instead of committing a second one, because the
    /// walk selects that marker at the candidate. No commit is created, so
    /// the baseline — a review or a starting point — is preserved whatever
    /// settings change, and no approval is invented or rewritten.
    fn repeat_initialization(
        &self,
        request: &InitRequest,
        branch_ref: &Option<String>,
        resolved: &ResolvedScope,
        mut effects: Effects,
    ) -> InitResult {
        let failure =
            |error: OperationError, effects: &Effects| InitResult::failure(error, effects.clone());
        let update = match repeat_settings(self.git, request, branch_ref, &resolved.descriptor) {
            Ok(update) => update,
            Err(error) => return failure(OperationError::from(error), &effects),
        };
        let reported = |settings: Vec<AppliedSetting>| InitializationOutcome {
            start_commit: None,
            repository_created: false,
            settings,
            dry_run: request.dry_run,
            expected_tree: None,
            tree_reason: Some(TREE_REASON_ALREADY_ANCHORED),
            proposed: None,
        };
        if request.dry_run {
            return match plan_local_settings(self.git, &update) {
                Ok((_settings, writes)) => InitResult::success(reported(writes), effects),
                Err(error) => failure(OperationError::from(error), &effects),
            };
        }
        match update_local_settings(self.git, &update, &mut effects) {
            Ok((_settings, applied)) => InitResult::success(reported(applied), effects),
            Err(error) => failure(OperationError::from(error), &effects),
        }
    }

    /// The `init --create-repo --dry-run` answer for a repository that does
    /// not exist yet (SPEC 6.4): the exact proposal, no tree, no creation.
    fn proposed_initialization(&self, request: &InitRequest) -> InitResult {
        InitResult::success(
            InitializationOutcome {
                start_commit: None,
                repository_created: false,
                settings: Vec::new(),
                dry_run: true,
                expected_tree: None,
                tree_reason: Some(TREE_REASON_REPOSITORY_NOT_CREATED),
                proposed: Some(InitializationProposal {
                    scope: request.scope.clone().unwrap_or_default(),
                    branch: request.branch.clone(),
                    create_repository: true,
                    auto_checkpoint: request.auto_checkpoint,
                    capture_after_edit: request.capture_after_edit,
                    editor_argv: request.editor_argv.clone(),
                }),
            },
            Effects::unchanged(),
        )
    }

    /// The precondition recheck of one initialization immediately before the
    /// first real mutation (SPEC 9.1 step 6, 14.3): the scope with its
    /// path-safety rules, the repository and storage policy — including a
    /// clean index and any Git operation that started meanwhile — the branch
    /// the operation adopted, the `HEAD` the anchor decision walked from,
    /// and the scoped working content are read again and compared with the
    /// state this capture prepared against.
    ///
    /// Anything that moved refuses before the real index is staged, because
    /// the proved intended tree describes a repository that no longer
    /// exists. The refusal carries the guard code of the dimension that
    /// moved, so the writer sees the same vocabulary the guards would have
    /// produced (SPEC 16.5 stale state, 7.5).
    ///
    /// The adopted branch of the stored settings is deliberately not
    /// compared here: this operation is the one explicit path that adopts a
    /// branch (SPEC 8.2), so the branch it observed at adoption is the
    /// expectation instead.
    fn recheck_initialization_preconditions(
        &self,
        input: &ScopeInput,
        resolved: &ResolvedScope,
        branch_at_adoption: &Option<String>,
        head_at_inspection: Option<&str>,
        working_before: &BTreeMap<OsString, Option<WorkingEntry>>,
    ) -> Result<(), OperationError> {
        let fresh = resolve_scope(self.git, input).map_err(OperationError::from)?;
        if !fresh.blockers.is_empty() {
            return Err(scope_blockers_refusal(&fresh.blockers));
        }
        if fresh.paths != resolved.paths {
            return Err(scope_paths_moved("scope", &fresh.paths, &resolved.paths));
        }
        let report =
            enforce_mutation_policy(self.git, Some(&fresh)).map_err(OperationError::from)?;
        if !report.ok() {
            return Err(OperationError::new(
                report.blockers[0].public_code(),
                blockers_message(&report.blockers),
            ));
        }
        let branch = self.git.head_symbolic_ref().map_err(OperationError::from)?;
        if branch != *branch_at_adoption {
            return Err(branch_moved_refusal(branch_at_adoption, &branch));
        }
        let fresh_head = self.git.head_commit().map_err(OperationError::from)?;
        if fresh_head.as_deref() != head_at_inspection {
            return Err(stale_state_refusal(format!(
                "HEAD changed from {} to {} while the initialization was prepared; nothing was staged",
                head_at_inspection.unwrap_or("an unborn branch"),
                fresh_head.as_deref().unwrap_or("an unborn branch"),
            )));
        }
        let fresh_working = scoped_working_state(self.git, &fresh).map_err(OperationError::from)?;
        if &fresh_working != working_before {
            return Err(scope_paths_moved(
                "scoped working content",
                &fresh_working,
                working_before,
            ));
        }
        Ok(())
    }

    /// Creates one scoped draft checkpoint (SPEC 13.2
    /// `create_checkpoint`, 9.2).
    ///
    /// The shared guards refuse before anything is written: a dirty index,
    /// an incompatible Git operation, scope blockers, a storage
    /// transformation, a missing author identity, a branch other than the
    /// adopted one (SPEC 8.2), and an unborn branch, which no draft can
    /// anchor because only a start marker may be a root commit (SPEC 8.4
    /// rule 3).
    ///
    /// The capture enumerates the exact eligible scoped changes —
    /// additions, edits, mode changes, and deletions, the same set
    /// [`Operations::inspect_workspace`] reports uncaptured — and proves the
    /// intended tree in one disposable index: the staged delta names
    /// exactly those paths, and every staged entry equals the observed mode
    /// and raw bytes. Unchanged content is the documented no-op of
    /// `created:false` with the current candidate and no effects. A dry run
    /// validates the same way and reports the intended tree without
    /// writing (SPEC 6.4).
    ///
    /// The real capture stages only the explicit literal path set through
    /// Git's NUL-delimited path input — never `git add .`, `commit -a`, or
    /// a pre-existing staged selection — and commits through the shared
    /// marker writer, which verifies the real index tree and then the
    /// actual commit's parent, tree, and every reserved trailer (SPEC 8.4).
    ///
    /// The whole capture holds the shared mutation lock of SPEC 9.1 step 2,
    /// acquired before the mutation inspection and kept through the
    /// postcondition inspection; a competing Margin operation is refused
    /// `BUSY` with no intended effects. Immediately before the staging, the
    /// recheck of SPEC 9.1 step 6 reads the scope with its path-safety
    /// rules, the shared policy guards, the branch, `HEAD`, the index, and
    /// the scoped working content again, and refuses before staging when
    /// any of them moved while the capture was prepared.
    /// The draft changes no out-of-scope entry and confers no approval: the
    /// baseline a review needs stays whatever the walk selects. Afterwards
    /// the operation re-examines the working tree, so a writer who changed
    /// a file after it was staged sees a truthful warning about the newer
    /// uncaptured content, never a claim that those bytes are saved. A
    /// failed staging or commit reports the inspected head, index, and
    /// working files: a commit that exists despite the failure is named
    /// with its object ID, a killed process keeps its outcome unknown, and
    /// the staged changes stay behind with the candidate anchor, so the
    /// next automatic checkpoint stops on that staged state (SPEC 9.2,
    /// 9.1 step 7, 9.6).
    ///
    /// One request with `--include-staged` and its three expected
    /// fingerprints leaves this algorithm for the private
    /// `capture_staged_recovery`, the guarded recovery capture of SPEC 9.7
    /// and the one narrow exception to the clean-index rule above.
    pub fn create_checkpoint(&self, request: &CheckpointRequest) -> CheckpointResult {
        // The guarded recovery capture of SPEC 9.7 is the one narrow path
        // that adopts inspected staged content; the normal algorithm below
        // never consumes a pre-existing staged selection.
        match recovery_expectations(request) {
            Ok(None) => {}
            Ok(Some(expectations)) => return self.capture_staged_recovery(request, &expectations),
            Err(error) => return CheckpointResult::failure(error, Effects::unchanged()),
        }
        let mut effects = Effects::unchanged();
        let failure = |error: OperationError, effects: &Effects| {
            CheckpointResult::failure(error, effects.clone())
        };

        // SPEC 9.1 step 1: the workspace probe, the input, and the one scope
        // resolver. A checkpoint holds no request receipt to look up: its
        // idempotence is the no-op of unchanged content, not durable
        // request deduplication (SPEC 8.7), so the eligibility inspection
        // below needs no earlier receipt pass.
        if let Err(error) = require_work_tree(self.git) {
            return failure(error, &effects);
        }
        let settings = match self.effective_settings() {
            Ok(settings) => settings,
            Err(error) => return failure(error, &effects),
        };
        let input = match scope_input_of(request.scope.as_ref(), &settings) {
            Some(input) => input,
            None => return failure(not_initialized(), &effects),
        };
        let resolved = match resolve_scope(self.git, &input) {
            Ok(resolved) => resolved,
            Err(error) => return failure(OperationError::from(error), &effects),
        };
        if !resolved.blockers.is_empty() {
            return failure(scope_blockers_refusal(&resolved.blockers), &effects);
        }
        // SPEC 9.1 step 2: the shared mutation lock serializes this capture
        // with every other Margin write of the repository and is held
        // through the postcondition inspection below. A dry run writes
        // nothing, so it reserves nothing (SPEC 6.4).
        let _lock = if request.dry_run {
            None
        } else {
            match self.acquire_mutation_lock() {
                Ok(lock) => Some(lock),
                Err(error) => return failure(error, &effects),
            }
        };
        // SPEC 9.1 step 4: the shared mutation inspection in guard order.
        let branch_ref = match self.enforce_mutation_guards(&resolved, &settings) {
            Ok(branch_ref) => branch_ref,
            Err(error) => return failure(error, &effects),
        };
        if let Err(error) = self.git.author_ident() {
            if matches!(error, GitError::Exit { .. }) {
                return failure(missing_identity(), &effects);
            }
            return failure(OperationError::from(error), &effects);
        }
        // A draft has exactly one parent, so only a born branch supplies the
        // candidate a checkpoint captures against; the one root commit
        // Margin creates is the start marker (SPEC 8.4 rule 3).
        let candidate = match self.git.head_commit() {
            Ok(Some(candidate)) => candidate,
            Ok(None) => {
                return failure(
                    OperationError::new(
                        "BASELINE_UNAVAILABLE",
                        "the branch is unborn, so no candidate exists to checkpoint against",
                    )
                    .recovery(&["margin", "init", "--json"]),
                    &effects,
                );
            }
            Err(error) => return failure(OperationError::from(error), &effects),
        };

        // The exact eligible scoped changes (SPEC 9.2).
        let changes = match scoped_working_changes(self.git, &resolved, &candidate) {
            Ok(changes) => changes,
            Err(error) => return failure(OperationError::from(error), &effects),
        };
        if changes.is_empty() {
            // Unchanged content is the explicit no-op, never a failure.
            return CheckpointResult::success(
                CheckpointOutcome {
                    commit: None,
                    changed: false,
                    candidate,
                    paths: Vec::new(),
                    dry_run: request.dry_run,
                    expected_tree: None,
                },
                effects,
            );
        }
        let paths: Vec<OsString> = changes.keys().cloned().collect();

        // The intended tree, proven in one disposable index (SPEC 9.1
        // step 5).
        let tree = match intended_draft_tree(self.git, &candidate, &changes) {
            Ok(tree) => tree,
            Err(error) => return failure(OperationError::from(error), &effects),
        };
        let metadata = CommitMetadata {
            kind: CommitKind::Draft,
            scope_digest: resolved.digest.id(),
            scope_spec: String::from_utf8(resolved.descriptor.canonical_bytes())
                .expect("the canonical descriptor is UTF-8 JSON"),
            candidate: candidate.clone(),
            result_tree: tree.clone(),
            baseline: None,
            request: None,
            plan: None,
            source: None,
        };
        // An invalid explicit message refuses before anything is staged, so
        // a bad message can never leave staged content behind (SPEC 7.3).
        let text = request.message.as_deref().unwrap_or("");
        if let Err(problem) = metadata.commit_message(text) {
            return failure(
                OperationError::new(problem.public_code(), problem.to_string()),
                &effects,
            );
        }

        // A dry run reports the intended capture and writes nothing
        // (SPEC 6.4).
        if request.dry_run {
            return CheckpointResult::success(
                CheckpointOutcome {
                    commit: None,
                    changed: true,
                    candidate,
                    paths,
                    dry_run: true,
                    expected_tree: Some(tree),
                },
                effects,
            );
        }

        // The precondition recheck immediately before the first real
        // mutation (SPEC 9.1 step 6): every dimension the capture prepared
        // against is read again, and anything that moved refuses before
        // staging.
        if let Err(error) = self.recheck_capture_preconditions(
            &input,
            &resolved,
            &settings,
            &branch_ref,
            &candidate,
            &changes,
        ) {
            return failure(error, &effects);
        }

        // The working-entry baseline for post-failure observation (SPEC 9.1
        // step 7), assembled without hashing any file twice.
        let working_before = match working_baseline(self.git, &resolved, &candidate, &changes) {
            Ok(before) => before,
            Err(error) => return failure(OperationError::from(error), &effects),
        };

        // Stage exactly the literal path set (SPEC 9.2).
        if let Err(error) = self.git.stage_paths(&paths) {
            let error = OperationError::from(error);
            self.observe_capture_failure(
                &mut effects,
                Some(&candidate),
                &working_before,
                request.scope.as_ref(),
            );
            return failure(error, &effects);
        }
        effects.observe_index(ObservedEffect::Changed);

        // Commit through the shared marker writer, then report the observed
        // working state after the commit: both the ordinary checkpoint and
        // the guarded recovery capture below share this one path, so both
        // report effects and failures through the same observation policy
        // (SPEC 8.4, 9.2).
        let capture = ProvedCapture {
            scope: request.scope.clone(),
            metadata,
            text: text.to_string(),
            candidate: candidate.clone(),
            paths,
            tree,
            working_before: working_before.clone(),
        };
        let (marker, data) = match self.commit_proved_capture(&mut effects, capture) {
            Ok(finished) => finished,
            Err(failed) => {
                let (error, observed) = *failed;
                return CheckpointResult::observed_failure(error, observed, effects);
            }
        };
        if let Some(trace) = self.trace.as_ref() {
            trace.checkpoint(false, &marker.commit, &candidate, &marker.result_tree);
        }
        let warnings = self.observe_capture_success(
            &mut effects,
            request.scope.as_ref(),
            &marker.commit,
            &working_before,
        );
        if warnings.is_empty() {
            CheckpointResult::success(data, effects)
        } else {
            CheckpointResult::success_with_warnings(data, warnings, effects)
        }
    }

    /// Commits one proved capture through the shared marker writer (SPEC
    /// 8.4, 9.2, 9.6): the real index tree, the parent, the result tree,
    /// and every reserved trailer are verified against the prepared map.
    ///
    /// A failure inspects what it left and keeps the commit the failure
    /// itself names: a commit whose observed state does not match the
    /// prepared map still exists — it stays ordinary history and confers
    /// no approval. A killed commit process loses the outcome, which is
    /// never narrowed back to `none`. Every other failure, such as a hook
    /// rejection or an output limit reached after the commit, reports the
    /// candidate anchor, the inspected commit when one exists, and the
    /// exact observed effects.
    ///
    /// `capture.working_before` is the scoped working-entry baseline the
    /// capture read before its first real mutation, the comparison
    /// [`Operations::observe_capture_failure`] reports against.
    fn commit_proved_capture(
        &self,
        effects: &mut Effects,
        capture: ProvedCapture,
    ) -> Result<(ValidMarker, CheckpointOutcome), Box<(OperationError, CheckpointOutcome)>> {
        let observed = |commit: Option<String>| CheckpointOutcome {
            commit,
            changed: true,
            candidate: capture.candidate.clone(),
            paths: capture.paths.clone(),
            dry_run: false,
            expected_tree: Some(capture.tree.clone()),
        };
        let marker = match commit_marker(self.git, &capture.metadata, &capture.text) {
            Ok(marker) => marker,
            Err(error) => {
                let inspected = self.observe_capture_failure(
                    effects,
                    Some(&capture.candidate),
                    &capture.working_before,
                    capture.scope.as_ref(),
                );
                match &error {
                    MarkerCommitError::Verification { commit, .. } => {
                        effects.observe_commit(CommitEffect::Created, Some(commit.clone()));
                    }
                    MarkerCommitError::Git(GitError::Timeout { .. }) => {
                        effects.observe_commit(CommitEffect::Unknown, None);
                    }
                    _ => {}
                }
                let commit = match &error {
                    MarkerCommitError::Verification { commit, .. } => Some(commit.clone()),
                    MarkerCommitError::Git(GitError::Timeout { .. }) => None,
                    _ => inspected,
                };
                return Err(Box::new((OperationError::from(error), observed(commit))));
            }
        };
        effects.observe_commit(CommitEffect::Created, Some(marker.commit.clone()));
        let data = CheckpointOutcome {
            commit: Some(marker.commit.clone()),
            changed: true,
            candidate: marker.commit.clone(),
            paths: capture.paths,
            dry_run: false,
            expected_tree: Some(marker.result_tree.clone()),
        };
        Ok((marker, data))
    }

    /// The post-success observation of one verified draft commit (SPEC 7.1,
    /// 9.2, 9.6): the final scoped working state is read from the repository
    /// and compared with `before`, the baseline the capture read before its
    /// first real mutation — the same comparison
    /// [`Operations::observe_capture_failure`] reports against — so a hook
    /// that changed scoped working bytes after the staging reports `changed`
    /// working files next to its `DIRTY_SCOPE` warning, and an inspection
    /// that itself fails reports `unknown`, never a claim of unchanged state.
    ///
    /// The warnings of one verified draft commit: a writer — or a hook — may
    /// have changed a scoped file after the capture read it, or created a
    /// scoped path that did not exist when the scope resolved, so the
    /// response reports the newer uncaptured content instead of calling
    /// those bytes saved. An inspection that fails explains why it could
    /// not.
    fn observe_capture_success(
        &self,
        effects: &mut Effects,
        scope: Option<&ScopeInput>,
        commit: &str,
        before: &BTreeMap<OsString, Option<WorkingEntry>>,
    ) -> Vec<Warning> {
        let mut warnings = Vec::new();
        let rescoped = self
            .context_scope_input(scope)
            .map_err(|error| format!("{}: {}", error.code, error.message))
            .and_then(|input| resolve_scope(self.git, &input).map_err(|error| error.to_string()));
        let scope = match rescoped {
            Ok(scope) => scope,
            Err(detail) => {
                // The verified commit stands; its final working scope cannot
                // be inspected, so the working effect is lost with it.
                effects.observe_working_tree(ObservedEffect::Unknown);
                warnings.push(Warning {
                    code: "GIT_FAILED",
                    message: format!(
                        "the draft commit {commit} verified, but the working scope after it \
                         could not be inspected: {detail}",
                    ),
                });
                return warnings;
            }
        };
        match scoped_working_changes(self.git, &scope, commit) {
            Ok(newer) if !newer.is_empty() => warnings.push(Warning {
                code: "DIRTY_SCOPE",
                message: format!(
                    "the draft commit {commit} saved the staged content, but {} scoped \
                     path(s) hold newer uncaptured content: {}",
                    newer.len(),
                    listed_paths(&newer.keys().cloned().collect::<Vec<OsString>>()),
                ),
            }),
            Ok(_) => {}
            Err(error) => warnings.push(Warning {
                code: "GIT_FAILED",
                message: format!(
                    "the draft commit {commit} verified, but the working state after it \
                     could not be inspected: {error}",
                ),
            }),
        }
        // The scoped working content against the baseline the capture read:
        // the fresh resolution observes a hook-created scoped path too, and
        // an entry that differs from that baseline is a real working-file
        // change, never a saved one. Absence and a missing key are the same
        // working state — a committed deletion leaves the fresh scope
        // without its path while the baseline holds that path as absent —
        // so [`present_working_entries`] compares the present entries only.
        let inspected = scoped_working_state(self.git, &scope);
        effects.observe_working_tree(match inspected {
            Ok(after) if present_working_entries(&after) == present_working_entries(before) => {
                ObservedEffect::Unchanged
            }
            Ok(_) => ObservedEffect::Changed,
            Err(_) => ObservedEffect::Unknown,
        });
        warnings
    }

    /// The guarded recovery capture of staged content (SPEC 9.7): one
    /// `checkpoint --include-staged --expect-head C --expect-index T
    /// --expect-working W` request.
    ///
    /// An interrupted application can leave its proved result staged
    /// against the candidate, and the writer who lost the plan captures
    /// that state as one unreviewed draft instead of guessing prior
    /// selections. The capture is guarded from every side:
    ///
    /// - every guard of the shared mutation inspection except the staged
    ///   delta itself, which is the subject here: conflict entries,
    ///   in-progress Git operations, storage transformations, a detached
    ///   `HEAD`, and a foreign branch refuse as they do for every write,
    ///   and the whole capture holds the shared mutation lock
    /// - every staged change must lie in scope, and the scoped index must
    ///   equal the inspected scoped working content, so the draft saves
    ///   exactly the bytes a recovery inspection saw
    /// - all three expected fingerprints must still match — `HEAD` still
    ///   C, the real index tree still T, the scoped working fingerprint
    ///   still W — so a state that moved since `doctor` reported it
    ///   refuses `STALE_STATE` with nothing written
    ///
    /// Nothing is staged, unstaged, or restaged: the shared marker writer
    /// commits the index exactly as it stands, verifies the result tree,
    /// and appends one ordinary draft commit that confers no approval —
    /// the baseline a review needs stays whatever the walk selects, and
    /// the writer opens a fresh review over the new candidate. A dry run
    /// validates the same way and reports the intended tree without
    /// writing; the index tree is observed through one disposable copy of
    /// the index, so even the validation leaves the real index bytes
    /// alone (SPEC 6.4).
    fn capture_staged_recovery(
        &self,
        request: &CheckpointRequest,
        expectations: &StagedCaptureExpectations,
    ) -> CheckpointResult {
        let mut effects = Effects::unchanged();
        let failure = |error: OperationError, effects: &Effects| {
            CheckpointResult::failure(error, effects.clone())
        };
        let stale = |detail: String| {
            OperationError::new("STALE_STATE", detail).recovery(&["margin", "doctor", "--json"])
        };

        // SPEC 9.1 step 1: the workspace probe, the input, and the one
        // scope resolver. A recovery capture holds no request receipt to
        // look up (SPEC 8.7): the three expected fingerprints are the state
        // it pins, so its retry is one stale-state refusal, not a second
        // commit.
        if let Err(error) = require_work_tree(self.git) {
            return failure(error, &effects);
        }
        let settings = match self.effective_settings() {
            Ok(settings) => settings,
            Err(error) => return failure(error, &effects),
        };
        let input = match scope_input_of(request.scope.as_ref(), &settings) {
            Some(input) => input,
            None => return failure(not_initialized(), &effects),
        };
        let resolved = match resolve_scope(self.git, &input) {
            Ok(resolved) => resolved,
            Err(error) => return failure(OperationError::from(error), &effects),
        };
        if !resolved.blockers.is_empty() {
            return failure(scope_blockers_refusal(&resolved.blockers), &effects);
        }
        // SPEC 9.1 step 2: the shared mutation lock serializes this capture
        // with every other Margin write of the repository. A dry run writes
        // nothing, so it reserves nothing (SPEC 6.4).
        let _lock = if request.dry_run {
            None
        } else {
            match self.acquire_mutation_lock() {
                Ok(lock) => Some(lock),
                Err(error) => return failure(error, &effects),
            }
        };
        // SPEC 9.1 step 4 with the one index exception of SPEC 9.7: every
        // shared guard except the staged delta, which is the subject of
        // the capture, then the adopted branch and the author identity.
        let branch_ref = match self.enforce_mutation_guards_except_staged(&resolved, &settings) {
            Ok(branch_ref) => branch_ref,
            Err(error) => return failure(error, &effects),
        };
        if let Err(error) = self.git.author_ident() {
            if matches!(error, GitError::Exit { .. }) {
                return failure(missing_identity(), &effects);
            }
            return failure(OperationError::from(error), &effects);
        }
        // The candidate the expected head pins; a draft has exactly one
        // parent, so only a born branch supplies it (SPEC 8.4 rule 3).
        let candidate = match self.git.head_commit() {
            Ok(Some(candidate)) => candidate,
            Ok(None) => {
                return failure(
                    OperationError::new(
                        "BASELINE_UNAVAILABLE",
                        "the branch is unborn, so no candidate exists to recover against",
                    )
                    .recovery(&["margin", "init", "--json"]),
                    &effects,
                );
            }
            Err(error) => return failure(OperationError::from(error), &effects),
        };
        if candidate != expectations.head {
            return failure(
                stale(format!(
                    "the recovery capture requires HEAD at the expected commit {}, but HEAD \
                     holds {candidate}",
                    expectations.head,
                )),
                &effects,
            );
        }

        // The real index and its tree (SPEC 9.7): the guards refused the
        // conflict stages, and the tree is observed through one disposable
        // copy so the validation itself writes nothing.
        let index = match self.git.ls_files_stage() {
            Ok(index) => index,
            Err(error) => return failure(OperationError::from(error), &effects),
        };
        let index_tree = match self.observed_index_tree() {
            Ok(tree) => tree,
            Err(error) => return failure(error, &effects),
        };
        if index_tree != expectations.index_tree {
            return failure(
                stale(format!(
                    "the recovery capture requires the index tree {}, but the index holds \
                     {index_tree}",
                    expectations.index_tree,
                )),
                &effects,
            );
        }

        // Every staged change in scope (SPEC 9.7): the capture adopts the
        // writing it can pin completely, so staged content outside the
        // scope keeps the ordinary Git inspection guidance of that
        // section, never a silent partial adoption.
        let staged: Vec<OsString> = match self.git.staged_against(&candidate) {
            Ok(staged) => staged,
            Err(error) => return failure(OperationError::from(error), &effects),
        };
        let outside: Vec<OsString> = staged
            .iter()
            .filter(|path| !resolved.paths.contains_key(*path))
            .cloned()
            .collect();
        if !outside.is_empty() {
            return failure(
                OperationError::new(
                    "STAGED_CHANGES",
                    format!(
                        "the recovery capture adopts only staged content inside the writing \
                         scope; {} staged path(s) lie outside it: {}",
                        outside.len(),
                        listed_paths(&outside),
                    ),
                )
                .recovery(&["git", "diff", "--cached", "--name-status"]),
                &effects,
            );
        }

        // The scoped working fingerprint (SPEC 9.7): the inspected content
        // the capture must still find.
        let working = match scoped_working_state(self.git, &resolved) {
            Ok(working) => working,
            Err(error) => return failure(OperationError::from(error), &effects),
        };
        let working_fingerprint = match working_content_digest(self.git, &resolved) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                return failure(
                    OperationError::new(error.public_code(), error.to_string()),
                    &effects,
                );
            }
        };
        if working_fingerprint.id() != expectations.working {
            return failure(
                stale(format!(
                    "the recovery capture requires the scoped working fingerprint {}, but the \
                     working tree holds {}",
                    expectations.working,
                    working_fingerprint.id(),
                )),
                &effects,
            );
        }

        // The scoped index against the inspected scoped working content
        // (SPEC 9.7): the draft must save exactly the bytes the inspection
        // saw, so every scoped path's stage-zero entry — mode and raw
        // bytes — equals its working entry. A path the writer edited after
        // staging it, an unstaged edit, or an unstaged deletion each keep
        // the capture refused: that newer content belongs to the next
        // ordinary checkpoint, never to a guess about it.
        let scoped_index: BTreeMap<OsString, (String, String)> = index
            .iter()
            .filter(|entry| entry.stage == 0 && resolved.paths.contains_key(&entry.path))
            .map(|entry| (entry.path.clone(), (entry.mode.clone(), entry.oid.clone())))
            .collect();
        let differing: Vec<OsString> = working
            .iter()
            .filter(|(path, entry)| match (entry, scoped_index.get(*path)) {
                (Some(working_entry), Some((mode, oid))) => {
                    *mode != working_entry.mode.as_str() || *oid != working_entry.oid
                }
                (None, None) => false,
                _ => true,
            })
            .map(|(path, _)| path.clone())
            .collect();
        if !differing.is_empty() {
            return failure(
                OperationError::new(
                    "DIRTY_SCOPE",
                    format!(
                        "the scoped index does not match the scoped working content, so the \
                         recovery capture cannot save exactly one inspected state: {} path(s) \
                         hold staged and working entries that differ ({})",
                        differing.len(),
                        listed_paths(&differing),
                    ),
                )
                .recovery(&["margin", "doctor", "--json"]),
                &effects,
            );
        }

        // Unchanged content is the explicit no-op, never a failure: with
        // every fingerprint matching and the scoped index equal to the
        // scoped working content, an empty staged delta means the candidate
        // already holds that exact state (SPEC 9.2).
        if staged.is_empty() {
            return CheckpointResult::success(
                CheckpointOutcome {
                    commit: None,
                    changed: false,
                    candidate,
                    paths: Vec::new(),
                    dry_run: request.dry_run,
                    expected_tree: None,
                },
                effects,
            );
        }

        // The prepared marker map (SPEC 8.3): the index tree exactly as it
        // stands is the intended result, and no staging intervenes. An
        // invalid explicit message refuses before anything is committed,
        // so a bad message can never change this state (SPEC 7.3).
        let metadata = CommitMetadata {
            kind: CommitKind::Draft,
            scope_digest: resolved.digest.id(),
            scope_spec: String::from_utf8(resolved.descriptor.canonical_bytes())
                .expect("the canonical descriptor is UTF-8 JSON"),
            candidate: candidate.clone(),
            result_tree: index_tree.clone(),
            baseline: None,
            request: None,
            plan: None,
            source: None,
        };
        let text = request.message.as_deref().unwrap_or("");
        if let Err(problem) = metadata.commit_message(text) {
            return failure(
                OperationError::new(problem.public_code(), problem.to_string()),
                &effects,
            );
        }

        // A dry run reports the intended capture and writes nothing
        // (SPEC 6.4).
        if request.dry_run {
            return CheckpointResult::success(
                CheckpointOutcome {
                    commit: None,
                    changed: true,
                    candidate,
                    paths: staged,
                    dry_run: true,
                    expected_tree: Some(index_tree),
                },
                effects,
            );
        }

        // The precondition recheck immediately before the one real
        // mutation (SPEC 9.1 step 6, 9.7).
        if let Err(error) = self.recheck_staged_capture_preconditions(
            &input,
            &resolved,
            &settings,
            &branch_ref,
            &candidate,
            &index_tree,
        ) {
            return failure(error, &effects);
        }

        // Commit the index exactly as it stands: the staged state becomes
        // one unreviewed draft, and the post-commit inspection reports any
        // newer uncaptured content a concurrent writer or hook created.
        let capture = ProvedCapture {
            scope: request.scope.clone(),
            metadata,
            text: text.to_string(),
            candidate: candidate.clone(),
            paths: staged,
            tree: index_tree,
            working_before: working.clone(),
        };
        let (marker, data) = match self.commit_proved_capture(&mut effects, capture) {
            Ok(finished) => finished,
            Err(failed) => {
                let (error, observed) = *failed;
                return CheckpointResult::observed_failure(error, observed, effects);
            }
        };
        if let Some(trace) = self.trace.as_ref() {
            trace.checkpoint(true, &marker.commit, &candidate, &marker.result_tree);
        }
        effects.observe_index(ObservedEffect::Changed);
        let warnings = self.observe_capture_success(
            &mut effects,
            request.scope.as_ref(),
            &marker.commit,
            &working,
        );
        if warnings.is_empty() {
            CheckpointResult::success(data, effects)
        } else {
            CheckpointResult::success_with_warnings(data, warnings, effects)
        }
    }

    /// The precondition recheck of one guarded recovery capture immediately
    /// before its one real mutation (SPEC 9.1 step 6, 9.7): the scope with
    /// its path-safety rules and its fresh path set, the shared guards with
    /// their index exception, the branch, `HEAD`, and the real index tree
    /// are read again and compared with the state the capture validated
    /// against.
    ///
    /// The scoped working content is deliberately absent from this recheck:
    /// the commit reads the index alone, so newer working bytes cannot
    /// corrupt the captured state, and the post-commit inspection reports
    /// them as newer uncaptured content instead (SPEC 9.2). The shared
    /// marker writer verifies the real index tree once more inside the
    /// commit itself.
    fn recheck_staged_capture_preconditions(
        &self,
        input: &ScopeInput,
        resolved: &ResolvedScope,
        settings: &LocalSettings,
        branch_at_guard: &Option<String>,
        candidate: &str,
        index_tree: &str,
    ) -> Result<(), OperationError> {
        // Path safety and the scope's path set (SPEC 14.3).
        let fresh = resolve_scope(self.git, input).map_err(OperationError::from)?;
        if !fresh.blockers.is_empty() {
            return Err(scope_blockers_refusal(&fresh.blockers));
        }
        if fresh.paths != resolved.paths {
            return Err(scope_paths_moved("scope", &fresh.paths, &resolved.paths));
        }
        let branch = self.enforce_mutation_guards_except_staged(&fresh, settings)?;
        if branch != *branch_at_guard {
            return Err(branch_moved_refusal(branch_at_guard, &branch));
        }
        let fresh_head = self.git.head_commit().map_err(OperationError::from)?;
        if fresh_head.as_deref() != Some(candidate) {
            return Err(stale_state_refusal(format!(
                "HEAD changed from {candidate} to {} while the recovery capture was prepared; \
                 nothing was committed",
                fresh_head.as_deref().unwrap_or("an unborn branch"),
            )));
        }
        let fresh_tree = self.observed_index_tree()?;
        if fresh_tree != index_tree {
            return Err(stale_state_refusal(format!(
                "the index tree changed from {index_tree} to {fresh_tree} while the recovery \
                 capture was prepared; nothing was committed",
            )));
        }
        Ok(())
    }

    /// The precondition recheck of one checkpoint immediately before the
    /// first real mutation (SPEC 9.1 step 6, 14.3): the scope with its
    /// path-safety rules and its fresh path set, the shared policy guards,
    /// the branch, `HEAD`, the real index, and the scoped working content
    /// are read again and compared with the state this capture prepared
    /// against.
    ///
    /// Anything that moved refuses before staging: the proved intended tree
    /// describes a repository that no longer exists, and committing against
    /// it would either store bytes the proof never saw or append to a
    /// candidate that is no longer `HEAD`. The refusals carry the guard
    /// codes of whatever dimension moved — a branch switch is
    /// `BRANCH_CHANGED`, a staged entry is `STAGED_CHANGES`, a moved
    /// candidate is `STALE_STATE`, and moved scoped content is
    /// `DIRTY_SCOPE` — so the writer sees the same vocabulary the guards
    /// would have produced (SPEC 16.5 stale state, 7.5).
    fn recheck_capture_preconditions(
        &self,
        input: &ScopeInput,
        resolved: &ResolvedScope,
        settings: &LocalSettings,
        branch_at_guard: &Option<String>,
        candidate: &str,
        changes: &BTreeMap<OsString, Option<WorkingEntry>>,
    ) -> Result<(), OperationError> {
        // Path safety and the scope's path set (SPEC 14.3): a scoped path
        // that became a symlink, a nested repository, or one more eligible
        // untracked file all change what the one resolver returns.
        let fresh = resolve_scope(self.git, input).map_err(OperationError::from)?;
        if !fresh.blockers.is_empty() {
            return Err(scope_blockers_refusal(&fresh.blockers));
        }
        if fresh.paths != resolved.paths {
            return Err(scope_paths_moved("scope", &fresh.paths, &resolved.paths));
        }
        // The shared policy guards once more — the index included — and the
        // branch the guards observed, because an unstored branch switch
        // with no configured adoption would otherwise land the draft on
        // another branch (SPEC 9.1 step 4).
        let branch = self.enforce_mutation_guards(&fresh, settings)?;
        if branch != *branch_at_guard {
            return Err(branch_moved_refusal(branch_at_guard, &branch));
        }
        // The candidate the prepared map names must still be `HEAD`.
        let fresh_head = self.git.head_commit().map_err(OperationError::from)?;
        if fresh_head.as_deref() != Some(candidate) {
            return Err(stale_state_refusal(format!(
                "HEAD changed from {candidate} to {} while the checkpoint was prepared; nothing was staged",
                fresh_head.as_deref().unwrap_or("an unborn branch"),
            )));
        }
        self.recheck_clean_index(candidate)?;
        // The scoped working content, re-enumerated with the fresh scope and
        // compared entry by entry with the prepared capture set.
        let fresh_changes =
            scoped_working_changes(self.git, &fresh, candidate).map_err(OperationError::from)?;
        if &fresh_changes != changes {
            return Err(scope_paths_moved(
                "scoped working content",
                &fresh_changes,
                changes,
            ));
        }
        Ok(())
    }

    /// The precondition recheck of one receipt-producing application — a
    /// review or a restoration — immediately before its first real mutation
    /// (SPEC 9.1 step 6): the scope with its path-safety rules and its fresh
    /// path set, the shared policy guards, the branch, `HEAD`, the real
    /// index, and the scoped working content are read again and compared
    /// with the state this application prepared against.
    ///
    /// Anything that moved refuses before the commit: the proved intended
    /// tree describes a repository that no longer exists, and a commit
    /// appended to a moved candidate would record a result the walk can no
    /// longer reproduce. The refusals carry the guard codes of whatever
    /// dimension moved, so the writer sees the same vocabulary the shared
    /// inspection would have produced (SPEC 16.5 stale state, 7.5).
    fn recheck_candidate_preconditions(
        &self,
        input: &ScopeInput,
        resolved: &ResolvedScope,
        settings: &LocalSettings,
        branch_at_guard: &Option<String>,
        candidate: &str,
    ) -> Result<(), OperationError> {
        // Path safety and the scope's path set (SPEC 14.3): a scoped path
        // that became a symlink, a nested repository, or one more eligible
        // untracked file all change what the one resolver returns.
        let fresh = resolve_scope(self.git, input).map_err(OperationError::from)?;
        if !fresh.blockers.is_empty() {
            return Err(scope_blockers_refusal(&fresh.blockers));
        }
        if fresh.paths != resolved.paths {
            return Err(scope_paths_moved("scope", &fresh.paths, &resolved.paths));
        }
        // The shared policy guards once more — the index included — and the
        // branch the guards observed (SPEC 9.1 step 4).
        let branch = self.enforce_mutation_guards(&fresh, settings)?;
        if branch != *branch_at_guard {
            return Err(branch_moved_refusal(branch_at_guard, &branch));
        }
        // The candidate the prepared map names must still be `HEAD`.
        let fresh_head = self.git.head_commit().map_err(OperationError::from)?;
        if fresh_head.as_deref() != Some(candidate) {
            return Err(stale_state_refusal(format!(
                "HEAD changed from {candidate} to {} while the review was prepared; nothing was \
                 committed",
                fresh_head.as_deref().unwrap_or("an unborn branch"),
            )));
        }
        // The real index must still equal C (SPEC 9.4): a staged entry that
        // arrived while the application was prepared refuses here, before
        // the shared marker writer reads the index tree.
        self.recheck_clean_index(candidate)?;
        // The scoped working content stays captured: newer uncaptured bytes
        // belong to the next checkpoint, never to this decision (SPEC 9.3).
        let fresh_changes =
            scoped_working_changes(self.git, &fresh, candidate).map_err(OperationError::from)?;
        if !fresh_changes.is_empty() {
            return Err(dirty_scope_refusal(&fresh_changes, "review"));
        }
        Ok(())
    }

    /// The precondition recheck immediately before the first real mutation
    /// (SPEC 9.1 step 6): the index must still be clean against the
    /// candidate the checkpoint prepared against.
    fn recheck_clean_index(&self, candidate: &str) -> Result<(), OperationError> {
        let staged = self
            .git
            .staged_against(candidate)
            .map_err(OperationError::from)?;
        let conflicted = self
            .git
            .ls_files_stage()
            .map_err(OperationError::from)?
            .into_iter()
            .any(|entry| entry.stage > 0);
        if !staged.is_empty() || conflicted {
            return Err(OperationError::new(
                "STAGED_CHANGES",
                "the index changed while the checkpoint was prepared",
            )
            .recovery(&["margin", "status", "--json"]));
        }
        Ok(())
    }

    /// What the operation can say about the real index after a capture
    /// failed: inspected against the tree the capture started from, never
    /// assumed (SPEC 9.1 step 7). An unborn anchor compares against the
    /// empty index a first use starts from.
    fn observed_index_state(&self, anchor: Option<&str>) -> ObservedEffect {
        let staged = match anchor {
            Some(treeish) => self
                .git
                .staged_against(treeish)
                .map(|paths| !paths.is_empty()),
            None => self.git.ls_files_stage().map(|entries| !entries.is_empty()),
        };
        match staged {
            Ok(false) => ObservedEffect::Unchanged,
            Ok(true) => ObservedEffect::Changed,
            Err(_) => ObservedEffect::Unknown,
        }
    }

    /// The tree the real index describes, observed without touching one
    /// real index byte (SPEC 6.4, 9.7): the index file is copied to one
    /// disposable path and `write-tree` runs against the copy, so the
    /// recovery inspection, the guarded recovery capture, and its dry run
    /// read the same tree while the real index keeps its bytes, cache
    /// included. A missing index file is the empty index Git itself reads,
    /// so the disposable copy is seeded empty in that case, never created
    /// by one write against the real one.
    ///
    /// The copy never outlives the call, and no ref, working file, or
    /// stored blob changes: only the disposable copy and the tree object
    /// `write-tree` stores, which is the temporary Git object SPEC 6.4
    /// allows a read-only computation to create.
    fn observed_index_tree(&self) -> Result<String, OperationError> {
        let source = self.git.git_path("index").map_err(PolicyError::from)?;
        let temp = reserve_temp_index().map_err(PolicyError::Io)?;
        let tree = (|| {
            let mut probe = self.git.clone();
            probe.set_env_override("GIT_INDEX_FILE", &temp);
            match fs::metadata(&source) {
                Ok(_) => fs::copy(&source, &temp)
                    .map(|_| ())
                    .map_err(PolicyError::Io)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    probe.read_tree_empty().map_err(PolicyError::from)?
                }
                Err(error) => return Err(PolicyError::Io(error)),
            }
            probe.write_tree().map_err(PolicyError::from)
        })();
        // The disposable index never outlives the observation, on success
        // or failure.
        let _ = fs::remove_file(&temp);
        tree.map_err(OperationError::from)
    }

    /// Inspects what a failed capture actually left behind (SPEC 9.1 step 7,
    /// 9.6, 14.1): the branch head, the real index, and the scoped working
    /// content are read from the repository, never assumed from the intended
    /// plan. The primary error stays untouched; an inspection that itself
    /// fails reports `unknown` for that field instead of claiming unchanged
    /// state.
    ///
    /// `anchor` is the `HEAD` the capture started from, `None` on an unborn
    /// branch, and `before` is the scoped working-entry baseline the capture
    /// read. The returned commit is the one the inspection found at `HEAD`
    /// when the branch moved past its anchor, so a commit that exists despite
    /// the failure is reported with its object ID: a nonzero or interrupted
    /// commit process does not prove that no commit exists.
    fn observe_capture_failure(
        &self,
        effects: &mut Effects,
        anchor: Option<&str>,
        before: &BTreeMap<OsString, Option<WorkingEntry>>,
        scope: Option<&ScopeInput>,
    ) -> Option<String> {
        // The branch head first: it names the commit the response reports.
        let mut observed = None;
        match self.git.head_commit() {
            Ok(Some(head)) if !anchor.is_some_and(|anchor| head == anchor) => {
                effects.observe_commit(CommitEffect::Created, Some(head.clone()));
                observed = Some(head);
            }
            // The head the capture started from still stands.
            Ok(_) => effects.observe_commit(CommitEffect::None, None),
            Err(_) => effects.observe_commit(CommitEffect::Unknown, None),
        }

        // The real index against the capture's starting tree: a staging that
        // partially wrote, or staged content a failed commit left behind.
        effects.observe_index(self.observed_index_state(anchor));

        // The scoped working content hooks or a concurrent writer may have
        // changed. The scope resolves again so a hook-created scoped path is
        // observed too; an entry that differs from the baseline the capture
        // read is a real working-file change, never a saved one.
        let inspected = self
            .context_scope_input(scope)
            .and_then(|input| resolve_scope(self.git, &input).map_err(OperationError::from))
            .and_then(|fresh| scoped_working_state(self.git, &fresh).map_err(OperationError::from));
        effects.observe_working_tree(match inspected {
            Ok(after) if &after == before => ObservedEffect::Unchanged,
            Ok(_) => ObservedEffect::Changed,
            Err(_) => ObservedEffect::Unknown,
        });
        observed
    }

    /// Builds one pinned review comparison (SPEC 13.2 `build_review`, 6.3,
    /// 7.2).
    ///
    /// The comparison is whole-scope and immutable: B is the baseline the
    /// first-parent walk selects at `HEAD`, C is the committed candidate,
    /// and the one grouping engine of SPEC 10.2 turns them into the
    /// manifest every identity covers. The path filter, change selection,
    /// cursor, and limit of the request are presentation inputs only; they
    /// choose which bounded detail page this one result carries, and no
    /// page can change the identity, the totals, or what finishing the
    /// review approves.
    ///
    /// A detail page holds at most `limit` groups — 50 by default — or
    /// [`MAX_DIFF_PAGE_BYTES`] of response content, whichever is reached
    /// first. Each page reports `complete`, `next_cursor`, the whole-scope
    /// totals, and `omitted_detail_count`. A cursor is bound to the review
    /// identity and the page contract through one domain-separated digest;
    /// a cursor from another review, another contract, or a hand edit is
    /// `STALE_CURSOR` (SPEC 6.3).
    ///
    /// `--working` replaces C with one preview of the working tree: exactly
    /// the tree a checkpoint would create, computed through one disposable
    /// index, so the preview is deterministic for the same working content.
    /// A preview never becomes a commit, moves no ref, and reports
    /// `actionable:false`, because no pinned candidate exists to review
    /// (SPEC 6.2).
    ///
    /// The query observes only. It reads trees, blobs, and diffs through
    /// one disposable index when it previews; refs, the real index, and
    /// working bytes stay unchanged, so the effects are unchanged.
    pub fn build_review(&self, request: &DiffRequest) -> ReviewResult {
        let pinned = match self.pinned_review(request.scope.as_ref(), request.working) {
            Ok(pinned) => pinned,
            Err(error) => return ReviewResult::failure(error, Effects::unchanged()),
        };
        let PinnedReview {
            resolved,
            branch_ref,
            manifest,
            candidate_scope_digest,
        } = pinned;

        // Every group of the whole scope becomes one detail before any
        // paging decision, so the page can only hide, never narrow.
        let details: Vec<ChangeDetail> = manifest
            .files
            .iter()
            .flat_map(|file| file.groups.iter().map(|group| change_detail(file, group)))
            .collect();
        let totals = ScopeTotals {
            files: manifest.files.len(),
            groups: details.len(),
        };
        let review_id = manifest.review_id.id();

        let limit = request.limit.unwrap_or(domain::DEFAULT_DIFF_PAGE_GROUPS);
        let page = match select_detail_page(
            self.git,
            &details,
            &domain::CursorContext {
                review_oid: review_id.clone(),
                working: request.working,
                path_filter: request.path_filter.clone(),
                limit,
                offset: 0,
            },
            request,
        ) {
            Ok(page) => page,
            Err(error) => return ReviewResult::failure(error, Effects::unchanged()),
        };

        let omitted = totals.groups - page.changes.len();
        ReviewResult::success(
            ReviewOutcome {
                review_id,
                engine_version: manifest.engine_version,
                git_version: manifest.git_version,
                branch_ref,
                scope_id: resolved.digest.id(),
                scope: resolved.descriptor,
                baseline_oid: manifest.baseline_oid,
                candidate_oid: manifest.candidate_oid,
                candidate_scope_digest: candidate_scope_digest.id(),
                manifest_digest: manifest.manifest_digest.id(),
                changes: page.changes,
                scope_totals: totals,
                omitted_detail_count: omitted,
                complete: page.complete,
                next_cursor: page.next_cursor,
                working: request.working,
                actionable: !request.working,
            },
            Effects::unchanged(),
        )
    }

    /// Shows one document at an immutable revision (SPEC 6.2-6.3, 13.2).
    ///
    /// The query needs no writing scope, so a workspace can be read before
    /// adoption. The revision resolves to exactly one commit before any
    /// content read — through the adapter's end-of-options boundary and
    /// commit-type validation — and every later Git call names that
    /// canonical object ID, never the user string, so the answer cannot
    /// move while the repository does (SPEC 4.1, 14.1).
    ///
    /// The tree of that commit decides the content state explicitly
    /// (SPEC 6.3): a path the tree holds nowhere is absence as data; a
    /// present blob of zero bytes is an empty document, a different state
    /// (SPEC 4.1); a readable UTF-8 blob returns one bounded text view; and
    /// a blob that is not valid UTF-8 returns one bounded hexadecimal view
    /// with the true byte length and truncation metadata. A path that names
    /// a directory, a symbolic link, or a gitlink addresses no document, so
    /// the query refuses it as one invalid input instead of guessing a
    /// kind; nothing is silently treated as reviewed content (SPEC 3.2
    /// rule 5).
    ///
    /// The byte and line bounds bound the view only; the totals report the
    /// complete document. An explicit raw export returns every exact byte
    /// with no bound applied, and a raw export of an absent path refuses
    /// rather than write an empty stream that would impersonate an empty
    /// document (SPEC 6.3). The query observes only: no ref, index,
    /// working file, or setting changes.
    pub fn show_document(&self, request: &ShowRequest) -> ShowResult {
        let failure = |error: OperationError| ShowResult::failure(error, Effects::unchanged());
        if let Err(error) = require_work_tree(self.git) {
            return failure(error);
        }
        let path = match domain::parse_document_path(&request.path) {
            Ok(path) => path,
            Err(error) => {
                return failure(OperationError::new(error.public_code(), error.to_string()));
            }
        };
        let revision_oid = match resolve_immutable_revision(self.git, &request.revision) {
            Ok(oid) => oid,
            Err(error) => return failure(error),
        };
        let object_format = match self.git.object_format() {
            Ok(format) => format,
            Err(error) => return failure(OperationError::from(error)),
        };
        let entries = match self.git.ls_tree(&revision_oid) {
            Ok(entries) => entries,
            Err(error) => return failure(OperationError::from(error)),
        };
        let wanted = os_string_from_bytes(path.as_bytes().to_vec());
        let entry = entries
            .iter()
            .find(|entry| entry.path == wanted)
            .map(|entry| (entry.mode.clone(), entry.oid.clone()));
        let Some((mode, blob_oid)) = entry else {
            // A path below one directory of the tree names no one document;
            // a path the tree holds nowhere is absence as data.
            let directory = format!("{path}/");
            if entries.iter().any(|entry| {
                entry
                    .path
                    .as_encoded_bytes()
                    .starts_with(directory.as_bytes())
            }) {
                return failure(OperationError::new(
                    "INVALID_ARGUMENT",
                    format!("the path {path:?} names a directory, not one document"),
                ));
            }
            if request.raw {
                return failure(OperationError::new(
                    "INVALID_ARGUMENT",
                    format!(
                        "no document exists at {path:?} in {revision_oid}, so there are no exact \
                         bytes to export"
                    ),
                ));
            }
            return ShowResult::success(
                ShowOutcome {
                    revision_oid,
                    path,
                    object_format,
                    content: DocumentContent::Missing,
                },
                Effects::unchanged(),
            );
        };
        let mode = match mode.as_str() {
            "100644" => EntryMode::Regular,
            "100755" => EntryMode::Executable,
            SYMLINK_MODE => {
                return failure(OperationError::new(
                    "INVALID_ARGUMENT",
                    format!("the path {path:?} stores a symbolic link, not one document"),
                ));
            }
            GITLINK_MODE => {
                return failure(OperationError::new(
                    "INVALID_ARGUMENT",
                    format!("the path {path:?} stores a gitlink, not one document"),
                ));
            }
            other => {
                return failure(OperationError::new(
                    "INVALID_ARGUMENT",
                    format!("the path {path:?} stores mode {other}, not one regular document"),
                ));
            }
        };
        let bytes = match self.git.read_blob(&blob_oid) {
            Ok(bytes) => bytes,
            Err(error) => return failure(OperationError::from(error)),
        };
        let content = if request.raw {
            DocumentContent::Exact {
                blob_oid,
                mode,
                bytes,
            }
        } else if bytes.is_empty() {
            DocumentContent::Empty { blob_oid, mode }
        } else if let Ok(text) = std::str::from_utf8(&bytes) {
            DocumentContent::Text(bounded_document_text(
                &blob_oid,
                mode,
                text,
                request.max_bytes,
                request.max_lines,
            ))
        } else {
            DocumentContent::Opaque(bounded_document_hex(
                &blob_oid,
                mode,
                &bytes,
                request.max_bytes,
            ))
        };
        ShowResult::success(
            ShowOutcome {
                revision_oid,
                path,
                object_format,
                content,
            },
            Effects::unchanged(),
        )
    }

    /// Lists the ancestry-based editorial timeline (SPEC 13.2
    /// `list_history`, 5.5, 6.3).
    ///
    /// The query adopts the stored scope and starts from `HEAD`; an unborn
    /// branch holds no timeline, which is one empty completed page rather
    /// than an error, because absence is an answer here as in a document
    /// view. The walk itself is the same unfiltered first-parent ancestry
    /// the baseline selection of SPEC 8.5 uses, with no pathspec — an empty
    /// review commit a path-limited Git log would hide stays in the
    /// timeline — and no presentation limit ever narrows what the walks of
    /// marker discovery see, so a short displayed log makes no old baseline
    /// disappear.
    ///
    /// Every entry carries its validated kind — start, draft, review, or
    /// restore — through the marker rules of SPEC 8.4, including rule 6 for
    /// a review, so a forged or hook-altered marker keeps its claimed kind
    /// with the reason it was ignored and never appears as an event that
    /// conferred approval. Consecutive validated drafts collapse into one
    /// writing pass entry that counts its checkpoints; they stay reachable
    /// in Git and list again with `all_checkpoints`. Order is ancestry,
    /// never the committer clock, which each entry still reports exactly.
    ///
    /// The path filter, cursor, and limit are presentation inputs only: a
    /// filter hides entries that changed none of its roots, and a cursor
    /// binds the head, scope, checkpoint policy, filter, and limit it was
    /// issued under, so any changed context is one `STALE_CURSOR`. The
    /// query observes only (SPEC 6.3).
    pub fn list_history(&self, request: &LogRequest) -> LogResult {
        let failure = |error: OperationError| LogResult::failure(error, Effects::unchanged());
        let resolved = match self.query_context(request.scope.as_ref()) {
            Ok(resolved) => resolved,
            Err(error) => return failure(error),
        };
        let scope_id = resolved.digest.id();
        let object_format = match self.git.object_format() {
            Ok(format) => format,
            Err(error) => return failure(OperationError::from(error)),
        };
        let head = match self.git.head_commit() {
            Ok(Some(head)) => head,
            Ok(None) => {
                // No candidate exists, so no entry can either; the page is
                // honestly empty and complete, and issues no cursor.
                return LogResult::success(
                    LogOutcome {
                        head: None,
                        scope_id,
                        object_format,
                        entries: Vec::new(),
                        complete: true,
                        next_cursor: None,
                        all_checkpoints: request.all_checkpoints,
                    },
                    Effects::unchanged(),
                );
            }
            Err(error) => return failure(OperationError::from(error)),
        };
        let limit = request
            .limit
            .unwrap_or(domain::DEFAULT_LOG_PAGE_ENTRIES)
            .max(1);
        let contract = domain::LogCursorContext {
            head: head.clone(),
            scope_id: scope_id.clone(),
            all_checkpoints: request.all_checkpoints,
            path_filter: request.path_filter.clone(),
            limit,
            offset: 0,
        };
        let start = match &request.cursor {
            Some(raw) => match verify_log_cursor(self.git, raw, &contract) {
                Ok(offset) => offset,
                Err(error) => return failure(error),
            },
            None => 0,
        };
        let mut guard = self.history_walk();
        let mut walk = guard.as_mut().expect("the constructor fills the walk");
        let history = match walk.own_history(&head) {
            Ok(history) => history,
            Err(error) => return failure(history_walk_failed(error)),
        };
        let page = match timeline_page(&mut walk, &history, &contract, start, limit) {
            Ok(page) => page,
            Err(error) => return failure(error),
        };
        LogResult::success(
            LogOutcome {
                head: Some(head),
                scope_id,
                object_format,
                entries: page.entries,
                complete: page.complete,
                next_cursor: page.next_cursor,
                all_checkpoints: request.all_checkpoints,
            },
            Effects::unchanged(),
        )
    }

    /// Validates one decided review plan and constructs its intended result
    /// (SPEC 13.2 `validate_review`, 6.4, 9.3).
    ///
    /// The document parses under the strict input contract, and the receipt
    /// lookup of SPEC 8.7 runs next, before any present-day stale check
    /// (SPEC 9.1 step 3): an exact retry of a request that already completed
    /// returns the verified receipt commit as `existing_receipt`, so dirty
    /// files, staged content, writer activity, storage-policy blockers, and
    /// a candidate that moved since never hide a receipt the history still
    /// holds. A request ID reused under a different fingerprint conflicts
    /// with `REQUEST_ID_CONFLICT`, and a scan that cannot complete blocks
    /// instead of guessing the request absent.
    ///
    /// Without a receipt, the plan's context must name the pinned comparison
    /// the workspace holds, and its decisions must cover every change of
    /// that comparison exactly once, so unknown or stale groups refuse
    /// before anything is computed. The state validation then runs the
    /// shared guards read-only — scope, storage policy, index, and the
    /// adopted branch — and requires the committed candidate with no scoped
    /// working differences a review needs to be actionable.
    ///
    /// The intended result is proved in one disposable Git index seeded
    /// from C: each restored group's complete patch reverse-applies, one
    /// file's groups latest source position first, and `write-tree` yields
    /// the full intended tree T with a staged delta that names exactly the
    /// restored paths. The result reports T, the exact scoped result of T,
    /// and the decisions it came from.
    ///
    /// The dry run observes only: it moves no ref, writes no working file,
    /// touches neither the real index nor the approval state, and cannot
    /// promise that commit hooks succeed later. Its result stays advisory
    /// until the application rechecks every precondition (SPEC 6.4).
    pub fn validate_review(&self, request: &ReviewRequest) -> ReviewDecisionResult {
        let failure =
            |error: OperationError| ReviewDecisionResult::failure(error, Effects::unchanged());

        // SPEC 9.1 steps 1 and 3: the workspace probe and the strict parse
        // and canonicalization of the input, both before any present-day
        // eligibility inspection.
        if let Err(error) = require_work_tree(self.git) {
            return failure(error);
        }
        let plan = match ReviewPlan::parse(&request.plan) {
            Ok(plan) => plan,
            Err(problem) => return failure(problem.into()),
        };
        let fingerprint = match Identities::new(self.git)
            .and_then(|identities| identities.plan(&plan.identity()))
        {
            Ok(fingerprint) => fingerprint,
            Err(error) => return failure(OperationError::from(error)),
        };

        // SPEC 8.7, 9.1 step 3: the receipt lookup precedes every
        // stale-state check below, so an already completed request still
        // returns its verified receipt instead of a refusal.
        match self.lookup_request_receipt(CommitKind::Review, &plan.request_id, &fingerprint.id()) {
            Ok(ReceiptLookup::Found(receipt)) => {
                return self.existing_receipt_decision(&receipt, &plan, true, Effects::unchanged());
            }
            Ok(ReceiptLookup::Conflict {
                commit, recorded, ..
            }) => {
                return failure(request_id_conflict(&plan.request_id, &commit, &recorded));
            }
            Ok(ReceiptLookup::Absent) => {}
            Err(error) => return failure(error),
        }

        let pinned = match self.validated_plan(request.scope.as_ref(), &plan) {
            Ok(pinned) => pinned,
            Err(error) => return failure(error),
        };
        let PinnedReview {
            resolved,
            branch_ref,
            manifest,
            candidate_scope_digest,
        } = pinned;

        // SPEC 6.4, 9.1 step 4: the shared state validation, read-only
        // here because a dry run reserves neither the branch nor the
        // working files (SPEC 9.3).
        let settings = match self.effective_settings() {
            Ok(settings) => settings,
            Err(error) => return failure(error),
        };
        if let Err(error) = self.enforce_mutation_guards(&resolved, &settings) {
            return failure(error);
        }
        // A review is actionable only against a committed candidate with no
        // scoped working differences (SPEC 9.3): newer uncaptured bytes
        // belong to the next checkpoint, never to this decision.
        match scoped_working_changes(self.git, &resolved, &manifest.candidate_oid) {
            Ok(changes) if changes.is_empty() => {}
            Ok(changes) => return failure(dirty_scope_refusal(&changes, "review")),
            Err(error) => return failure(OperationError::from(error)),
        }

        // The intended result tree T, proved in one disposable index
        // (SPEC 9.3). Coverage already refused every unknown identity, so
        // each decision names one complete group of the pinned manifest.
        let decided: BTreeSet<&str> = plan.restore.iter().map(String::as_str).collect();
        let intended = match intended_review_tree(self.git, &manifest, &decided) {
            Ok(intended) => intended,
            Err(error) => return failure(OperationError::from(error)),
        };
        let result_scope_digest = match scoped_content_digest(self.git, &intended.tree, &resolved) {
            Ok(digest) => digest,
            Err(error) => {
                return failure(OperationError::new(error.public_code(), error.to_string()));
            }
        };

        let restored: Vec<String> = manifest
            .groups()
            .filter(|group| decided.contains(group.change_id.id().as_str()))
            .map(|group| group.change_id.id())
            .collect();
        let kept = manifest.groups().count() - restored.len();
        ReviewDecisionResult::success(
            ReviewDecision {
                request_id: plan.request_id,
                review_id: manifest.review_id.id(),
                branch_ref,
                scope_id: resolved.digest.id(),
                baseline_oid: manifest.baseline_oid,
                candidate_oid: manifest.candidate_oid,
                candidate_scope_digest: candidate_scope_digest.id(),
                kept: Some(kept),
                restored,
                paths: intended.paths,
                expected_tree: intended.tree,
                result_scope_digest: result_scope_digest.id(),
                message: plan.message,
                dry_run: true,
                commit: None,
                existing_receipt: None,
            },
            Effects::unchanged(),
        )
    }

    /// Builds the decision one exact retry reports (SPEC 8.7): the verified
    /// receipt commit, plus the response data of the request itself,
    /// assembled from the recorded canonical scope and immutable Git
    /// objects alone.
    ///
    /// The fingerprint match proved the retry carries the same context
    /// fields, decisions, and message as the plan that produced the
    /// receipt, so the plan supplies them. The receipt supplies what marker
    /// validation proved against the commit itself: its candidate, its
    /// baseline, its result tree, and its recorded scope descriptor, and
    /// the scoped digests and rewritten paths are computed from the
    /// candidate and result trees through that one recorded descriptor.
    ///
    /// Nothing here resolves the present-day scope, traverses a working
    /// path, or reruns the grouping engine over the historical comparison:
    /// a scope root that became a symlink, vanished, or became unreadable
    /// blocks a new mutation but never hides the receipt a completed
    /// request already holds (SPEC 8.7). Acknowledging a valid receipt
    /// therefore depends on neither the current grouping engine nor the
    /// present working tree. The engine-derived group count is not
    /// reconstructed: a receipt records no such count, and a changed
    /// engine must not regenerate historical group identities merely to
    /// acknowledge completion, so [`ReviewDecision::kept`] is `None` —
    /// unknown, never a fabricated zero — while the decisions the
    /// fingerprint pinned report as `restored` and the rewritten scoped
    /// paths come from the verified trees.
    ///
    /// The observed state stays separate in the effects: nothing is written
    /// here, and it is acceptable that `HEAD` advanced after the operation
    /// completed. The dry-run validation reaches this without the lock; the
    /// application reaches it under the lock and reports the matched
    /// receipt as its commit observation, so one shared build answers both.
    fn existing_receipt_decision(
        &self,
        receipt: &ValidMarker,
        plan: &ReviewPlan,
        dry_run: bool,
        effects: Effects,
    ) -> ReviewDecisionResult {
        let failure =
            |error: OperationError| ReviewDecisionResult::failure(error, Effects::unchanged());

        // The decisions the fingerprint pinned, in one canonical order:
        // sorting keeps the response identical for any plan document whose
        // sorted decisions hash to the same fingerprint.
        let mut restored = plan.restore.clone();
        restored.sort();

        // The immutable tree facts through the recorded descriptor (SPEC
        // 8.6): the scoped entries of the verified candidate and result
        // trees. A failure here means the objects the receipt depends on
        // are no longer readable, which is one explicit refusal naming the
        // receipt — never silence and never a fallback to absence.
        let digest_of = |revision: &str, what: &str| -> Result<Fingerprint, OperationError> {
            recorded_content_digest(self.git, revision, &receipt.scope).map_err(|error| {
                OperationError::new(
                    error.public_code(),
                    format!(
                        "the receipt {} cannot be recomputed from history: its {what} is \
                         unreadable: {error}",
                        receipt.commit,
                    ),
                )
            })
        };
        let candidate_scope_digest = match digest_of(&receipt.candidate, "candidate") {
            Ok(digest) => digest,
            Err(error) => return failure(error),
        };
        let result_scope_digest = match digest_of(&receipt.result_tree, "result tree") {
            Ok(digest) => digest,
            Err(error) => return failure(error),
        };
        let paths = match recorded_tree_difference(
            self.git,
            &receipt.candidate,
            &receipt.result_tree,
            &receipt.scope,
        ) {
            Ok(paths) => paths,
            Err(error) => {
                return failure(OperationError::new(
                    error.public_code(),
                    format!(
                        "the receipt {} cannot be recomputed from history: its changed scoped \
                     entries are unreadable: {error}",
                        receipt.commit,
                    ),
                ));
            }
        };

        ReviewDecisionResult::success(
            ReviewDecision {
                request_id: plan.request_id.clone(),
                review_id: plan.review_id.clone(),
                branch_ref: plan.branch_ref.clone(),
                scope_id: receipt.scope_digest.clone(),
                baseline_oid: receipt
                    .baseline
                    .clone()
                    .unwrap_or_else(|| plan.baseline_oid.clone()),
                candidate_oid: receipt.candidate.clone(),
                candidate_scope_digest: candidate_scope_digest.id(),
                kept: None,
                restored,
                paths,
                // The receipt recorded the intended result tree, and marker
                // validation proved the commit holds exactly it (SPEC 8.4
                // rule 4).
                expected_tree: receipt.result_tree.clone(),
                result_scope_digest: result_scope_digest.id(),
                message: plan.message.clone(),
                dry_run,
                commit: None,
                existing_receipt: Some(receipt.commit.clone()),
            },
            effects,
        )
    }

    /// Applies one complete review plan (SPEC 13.2 `apply_review`, 9.4).
    ///
    /// The keep-all half of the contract runs here: a complete plan that
    /// restores nothing verifies the real index equals C — the shared
    /// marker writer compares `write-tree` against the intended tree before
    /// committing — and then creates one normal empty review commit with
    /// `--allow-empty` and the expected-tree marker. No file is rewritten,
    /// so the working tree and the index stay as the guards proved them,
    /// and the displayed baseline advances only when the shared writer
    /// verifies the actual parent, tree, and reserved trailers against the
    /// prepared map (SPEC 8.4, 9.4).
    ///
    /// The restoring half of SPEC 9.4 runs after the same guards: the
    /// selected complete reverse patches apply through the real index with
    /// [`Operations::apply_selected_review`], which checks each unit
    /// against the real index and working tree, applies it with the one
    /// explicit whitespace policy the preview used, and verifies that the
    /// real index equals the preview tree T, that the staged delta stays
    /// exactly the restored scoped paths, and that the scoped working
    /// content equals the expected result R. Only then does the shared
    /// marker writer commit the already validated index, without restaging
    /// from the working tree and without `commit --only`.
    ///
    /// The guards run in the order of the shared sequence: the workspace
    /// probe and the strict parse first, then the mutation lock, then the
    /// receipt lookup of SPEC 8.7 before any present-day stale check, so an
    /// exact retry of a completed request returns its verified receipt
    /// commit — reported as an `existing_receipt` observation, never as a
    /// second review commit — while a reused request ID under another
    /// fingerprint conflicts. The pinned comparison then validates the
    /// plan's identities, the shared mutation guards inspect the branch,
    /// the index, and the storage profile, and a review is actionable only
    /// against the committed candidate with no scoped working differences
    /// (SPEC 9.3). The intended tree is proved once more in one disposable
    /// index under the lock, the preconditions are rechecked immediately
    /// before the first real mutation, and a failed application or commit
    /// reports the observed state — applied bytes, staged entries, and the
    /// commit a verification failure still names — with recovery guidance,
    /// never a claimed rollback (SPEC 9.6).
    pub fn apply_review(&self, request: &ReviewRequest) -> ReviewDecisionResult {
        // A dry run reports the intended effects without committing (SPEC
        // 6.4): the validation is exactly that report, so no interface can
        // reach a commit through the application with the flag set.
        if request.dry_run {
            return self.validate_review(request);
        }
        let mut effects = Effects::unchanged();
        let failure = |error: OperationError, effects: &Effects| {
            ReviewDecisionResult::failure(error, effects.clone())
        };

        // SPEC 9.1 steps 1 and 3: the workspace probe and the strict parse
        // and canonicalization of the input, before any present-day
        // eligibility inspection.
        if let Err(error) = require_work_tree(self.git) {
            return failure(error, &effects);
        }
        let plan = match ReviewPlan::parse(&request.plan) {
            Ok(plan) => plan,
            Err(problem) => return failure(problem.into(), &effects),
        };
        let fingerprint = match Identities::new(self.git)
            .and_then(|identities| identities.plan(&plan.identity()))
        {
            Ok(fingerprint) => fingerprint,
            Err(error) => return failure(OperationError::from(error), &effects),
        };

        // SPEC 9.1 step 2: the shared mutation lock serializes this
        // application with every other Margin write of the repository and
        // is held through the postcondition inspection below.
        let _lock = match self.acquire_mutation_lock() {
            Ok(lock) => lock,
            Err(error) => return failure(error, &effects),
        };

        // SPEC 8.7, 9.1 step 3: the receipt lookup precedes every
        // stale-state check below, so an already completed request returns
        // its verified receipt instead of a refusal or a second commit.
        match self.lookup_request_receipt(CommitKind::Review, &plan.request_id, &fingerprint.id()) {
            Ok(ReceiptLookup::Found(receipt)) => {
                effects.observe_commit(CommitEffect::ExistingReceipt, Some(receipt.commit.clone()));
                if let Some(trace) = self.trace.as_ref() {
                    trace.submit_receipt(&plan.request_id, &receipt.commit);
                }
                return self.existing_receipt_decision(&receipt, &plan, false, effects);
            }
            Ok(ReceiptLookup::Conflict {
                commit, recorded, ..
            }) => {
                if let Some(trace) = self.trace.as_ref() {
                    trace.submit_conflict(&plan.request_id);
                }
                return failure(
                    request_id_conflict(&plan.request_id, &commit, &recorded),
                    &effects,
                );
            }
            Ok(ReceiptLookup::Absent) => {}
            Err(error) => return failure(error, &effects),
        }

        // The pinned comparison and the plan's coverage of it (SPEC 7.3,
        // 9.3): unknown or stale identities refuse before anything is
        // computed.
        let pinned = match self.validated_plan(request.scope.as_ref(), &plan) {
            Ok(pinned) => pinned,
            Err(error) => return failure(error, &effects),
        };
        let PinnedReview {
            resolved,
            branch_ref,
            manifest,
            candidate_scope_digest,
        } = pinned;

        // SPEC 9.1 step 4: the shared mutation inspection in guard order,
        // plus the author identity Margin never writes itself.
        let settings = match self.effective_settings() {
            Ok(settings) => settings,
            Err(error) => return failure(error, &effects),
        };
        let input = match scope_input_of(request.scope.as_ref(), &settings) {
            Some(input) => input,
            None => return failure(not_initialized(), &effects),
        };
        let branch_at_guard = match self.enforce_mutation_guards(&resolved, &settings) {
            Ok(branch_ref) => branch_ref,
            Err(error) => return failure(error, &effects),
        };
        if let Err(error) = self.git.author_ident() {
            if matches!(error, GitError::Exit { .. }) {
                return failure(missing_identity(), &effects);
            }
            return failure(OperationError::from(error), &effects);
        }
        // A review is actionable only against the committed candidate with
        // no scoped working differences (SPEC 9.3): newer uncaptured bytes
        // belong to the next checkpoint, never to this decision.
        match scoped_working_changes(self.git, &resolved, &manifest.candidate_oid) {
            Ok(changes) if changes.is_empty() => {}
            Ok(changes) => {
                return failure(dirty_scope_refusal(&changes, "review"), &effects);
            }
            Err(error) => return failure(OperationError::from(error), &effects),
        }

        // The intended result tree T (SPEC 9.1 step 5, 9.3): the same
        // disposable-index proof the dry run ran, now under the lock.
        // Keep-all restores nothing, so the proof seeds the disposable
        // index from C and yields C's own full tree with an empty staged
        // delta; a restoring selection reverse-applies exactly its selected
        // complete units, latest source position first.
        let decided: BTreeSet<&str> = plan.restore.iter().map(String::as_str).collect();
        let intended = match intended_review_tree(self.git, &manifest, &decided) {
            Ok(intended) => intended,
            Err(error) => return failure(OperationError::from(error), &effects),
        };
        // The scoped digest of T names the result the decision approves:
        // for keep-all T is C's own tree, so this equals the candidate's
        // scoped digest (SPEC 7.2).
        let result_scope_digest = match scoped_content_digest(self.git, &intended.tree, &resolved) {
            Ok(digest) => digest,
            Err(error) => {
                return failure(
                    OperationError::new(error.public_code(), error.to_string()),
                    &effects,
                );
            }
        };
        // The decisions the response reports, in the manifest's own order,
        // and the count of the groups the plan keeps.
        let restored: Vec<String> = manifest
            .groups()
            .filter(|group| decided.contains(group.change_id.id().as_str()))
            .map(|group| group.change_id.id())
            .collect();
        let kept = manifest.groups().count() - restored.len();

        // The prepared marker map (SPEC 8.3): the review names C as its
        // candidate and parent, B as its baseline, T as its result tree,
        // and the request and plan fingerprint as its receipt. An invalid
        // effective message refuses before anything is committed, exactly
        // as the checkpoint contract does (SPEC 7.3).
        let metadata = CommitMetadata {
            kind: CommitKind::Review,
            scope_digest: resolved.digest.id(),
            scope_spec: String::from_utf8(resolved.descriptor.canonical_bytes())
                .expect("the canonical descriptor is UTF-8 JSON"),
            candidate: manifest.candidate_oid.clone(),
            result_tree: intended.tree.clone(),
            baseline: Some(manifest.baseline_oid.clone()),
            request: Some(plan.request_id.clone()),
            plan: Some(fingerprint.id()),
            source: None,
        };
        if let Err(problem) = metadata.commit_message(&plan.message) {
            return failure(
                OperationError::new(problem.public_code(), problem.to_string()),
                &effects,
            );
        }

        // The working-entry baseline for post-failure observation (SPEC 9.1
        // step 7), read before the first real mutation.
        let working_before = match scoped_working_state(self.git, &resolved) {
            Ok(before) => before,
            Err(error) => return failure(OperationError::from(error), &effects),
        };

        // The precondition recheck immediately before the first real
        // mutation (SPEC 9.1 step 6): for keep-all the commit itself is
        // that mutation; for a restoring plan the reverse application of
        // SPEC 9.4 step 2 is, and the real index and the scoped working
        // bytes still have to equal C when it starts.
        if let Err(error) = self.recheck_candidate_preconditions(
            &input,
            &resolved,
            &settings,
            &branch_at_guard,
            &manifest.candidate_oid,
        ) {
            return failure(error, &effects);
        }

        // The plan is accepted and pinned: the modeled protocol begins here
        // (SPEC 15.7 `Submit`), with the identities the conformance check
        // validates — the candidate, the baseline, and the groups the
        // application restores, in the one order the proof applies them.
        if let Some(trace) = self.trace.as_ref() {
            let restore: Vec<String> = selected_reverse_units(&manifest, &decided)
                .into_iter()
                .map(|(_, change_id, _)| change_id.id())
                .collect();
            trace.submit_accepted(
                "apply",
                &plan.request_id,
                &manifest.candidate_oid,
                &manifest.baseline_oid,
                &restore,
            );
        }

        // The decision data every return path below reports, from the
        // prepared context alone: the preview tree, the decided groups, and
        // the scoped result the decision approves.
        let observed = |commit: Option<String>,
                        restored: &[String],
                        kept: usize,
                        result_scope_digest: &Fingerprint| {
            ReviewDecision {
                request_id: plan.request_id.clone(),
                review_id: manifest.review_id.id(),
                branch_ref: branch_ref.clone(),
                scope_id: resolved.digest.id(),
                baseline_oid: manifest.baseline_oid.clone(),
                candidate_oid: manifest.candidate_oid.clone(),
                candidate_scope_digest: candidate_scope_digest.id(),
                kept: Some(kept),
                restored: restored.to_vec(),
                paths: intended.paths.clone(),
                expected_tree: intended.tree.clone(),
                // The scoped result of T the decision approves (SPEC 9.3):
                // the candidate's own scoped digest for keep-all, R's for a
                // restoring selection.
                result_scope_digest: result_scope_digest.id(),
                message: plan.message.clone(),
                dry_run: false,
                commit,
                existing_receipt: None,
            }
        };

        // The restoring half of SPEC 9.4: the selected complete reverse
        // patches apply through the guarded Git index path, and the real
        // result must equal the proved preview before the commit may run.
        // A failure at that boundary leaves real state behind, so the state
        // is inspected and reported instead of being called a rollback.
        if !plan.restore.is_empty() {
            if let Err(error) = self.apply_selected_review(
                &resolved,
                &manifest,
                &decided,
                &intended,
                &plan.request_id,
            ) {
                if let Some(trace) = self.trace.as_ref() {
                    trace.fail(&plan.request_id);
                }
                let inspected = self.observe_capture_failure(
                    &mut effects,
                    Some(&manifest.candidate_oid),
                    &working_before,
                    request.scope.as_ref(),
                );
                return ReviewDecisionResult::observed_failure(
                    error,
                    observed(inspected, &restored, kept, &result_scope_digest),
                    effects,
                );
            }
            // The application rewrote the scoped working bytes to R and
            // staged exactly them (SPEC 9.4 step 2); the commit below
            // verifies both again against the prepared map.
            effects.observe_working_tree(ObservedEffect::Changed);
            effects.observe_index(ObservedEffect::Changed);
        }

        // One normal review commit over the verified index (SPEC 9.4):
        // keep-all commits empty with `--allow-empty`; a restoring plan
        // commits the already validated index without restaging from the
        // working tree and without `commit --only`. The shared marker
        // writer compares the real index tree with T and verifies the
        // actual parent, tree, and every reserved trailer against the
        // prepared map before the baseline may advance (SPEC 8.4).
        let marker = match commit_marker(self.git, &metadata, &plan.message) {
            Ok(marker) => marker,
            Err(error) => {
                // Inspect what the failed application left, then keep the
                // commit the failure itself names: a commit whose observed
                // state does not match the prepared map still exists as
                // ordinary history and confers no approval, while a killed
                // commit process loses the outcome, which is never
                // narrowed back to `none` (SPEC 9.6, 7.1).
                if let Some(trace) = self.trace.as_ref() {
                    trace.fail(&plan.request_id);
                }
                let inspected = self.observe_capture_failure(
                    &mut effects,
                    Some(&manifest.candidate_oid),
                    &working_before,
                    request.scope.as_ref(),
                );
                match &error {
                    MarkerCommitError::Verification { commit, .. } => {
                        effects.observe_commit(CommitEffect::Created, Some(commit.clone()));
                    }
                    MarkerCommitError::Git(GitError::Timeout { .. }) => {
                        effects.observe_commit(CommitEffect::Unknown, None);
                    }
                    _ => {}
                }
                let commit = match &error {
                    MarkerCommitError::Verification { commit, .. } => Some(commit.clone()),
                    MarkerCommitError::Git(GitError::Timeout { .. }) => None,
                    _ => inspected,
                };
                return ReviewDecisionResult::observed_failure(
                    OperationError::from(error),
                    observed(commit, &restored, kept, &result_scope_digest),
                    effects,
                );
            }
        };
        effects.observe_commit(CommitEffect::Created, Some(marker.commit.clone()));
        if let Some(trace) = self.trace.as_ref() {
            trace.publish(
                &plan.request_id,
                &marker.commit,
                &manifest.candidate_oid,
                &marker.result_tree,
            );
        }

        // The observed state after the verified commit (SPEC 9.1 step 7,
        // 9.2): a hook or a concurrent writer may have changed scoped
        // working bytes or staged entries after the commit the markers
        // proved. The response reports that newer content as observed, and
        // never as reviewed, while the verified commit stands.
        let warnings = self.observe_receipt_completion(
            request.scope.as_ref(),
            "review",
            &marker.commit,
            &intended.tree,
            &mut effects,
        );
        if let Some(trace) = self.trace.as_ref() {
            trace.respond(&plan.request_id, &marker.commit);
        }
        let data = observed(
            Some(marker.commit.clone()),
            &restored,
            kept,
            &result_scope_digest,
        );
        if warnings.is_empty() {
            ReviewDecisionResult::success(data, effects)
        } else {
            ReviewDecisionResult::success_with_warnings(data, warnings, effects)
        }
    }

    /// Applies the selected reverse patches of one restoring review
    /// through the real Git index (SPEC 9.4 steps 1-3) and verifies the
    /// real result against the proved preview.
    ///
    /// Each selected unit is first checked and then applied with
    /// `git -c apply.ignoreWhitespace=false apply --index --reverse
    /// --whitespace=nowarn` — the one explicit whitespace policy the
    /// applicability check and the disposable-index preview share — in the
    /// proof's own order, latest source position first, so the application
    /// replays exactly what the preview proved and never regenerates a
    /// different patch, restages working files, or broadens a selected
    /// group.
    ///
    /// Afterwards the real index tree must equal T, the staged delta must
    /// name exactly the restored scoped paths — so nothing out of scope
    /// moved and no restored group changed nothing — and the scoped working
    /// content must equal the result R the preview intends. Any failure at
    /// these boundaries leaves applied units, staged entries, or restored
    /// working bytes behind: the caller inspects and reports that state,
    /// and this method never claims a rollback or grants approval to an
    /// unmatched result (SPEC 9.6, task T032).
    fn apply_selected_review(
        &self,
        resolved: &ResolvedScope,
        manifest: &GroupedManifest,
        decided: &BTreeSet<&str>,
        intended: &IntendedReview,
        request: &str,
    ) -> Result<(), OperationError> {
        let expected_delta: BTreeSet<OsString> =
            intended.paths.iter().map(OsString::from).collect();
        for (path, change_id, unit) in selected_reverse_units(manifest, decided) {
            // SPEC 9.4 step 1: the applicability check against the real
            // index and working tree, under the guards that just proved
            // both equal C.
            if let Err(error) = self.git.apply_patch_index(unit, true, true) {
                return Err(partial_application_failure(
                    matches!(error, GitError::Timeout { .. })
                        .then_some("OUTCOME_UNKNOWN")
                        .unwrap_or("PARTIAL_MUTATION"),
                    format!(
                        "the reverse patch of the restored passage {} of {path:?} does not \
                         apply to the real index and working tree: {error}",
                        change_id.id()
                    ),
                ));
            }
            // SPEC 9.4 step 2: the real application, one complete unit at a
            // time in the proof's order.
            if let Err(error) = self.git.apply_patch_index(unit, true, false) {
                return Err(partial_application_failure(
                    matches!(error, GitError::Timeout { .. })
                        .then_some("OUTCOME_UNKNOWN")
                        .unwrap_or("PARTIAL_MUTATION"),
                    format!(
                        "the reverse patch of the restored passage {} of {path:?} failed to \
                         apply to the real index and working tree: {error}",
                        change_id.id()
                    ),
                ));
            }
            if let Some(trace) = self.trace.as_ref() {
                trace.apply_group(request, &change_id.id(), &manifest.candidate_oid);
            }
        }

        // SPEC 9.4 step 3: the real index tree equals T, the staged delta
        // stays exactly the restored scoped paths, and the scoped working
        // content equals R.
        let index_tree = self.git.write_tree().map_err(OperationError::from)?;
        if index_tree != intended.tree {
            return Err(partial_application_failure(
                "PARTIAL_MUTATION",
                format!(
                    "the applied review holds the index tree {index_tree} instead of the proved \
                     result tree {}; the committed result would not match the preview",
                    intended.tree
                ),
            ));
        }
        let staged: BTreeSet<OsString> = self
            .git
            .staged_against(&manifest.candidate_oid)
            .map_err(OperationError::from)?
            .into_iter()
            .collect();
        if staged != expected_delta {
            let unexpected: Vec<OsString> = staged.difference(&expected_delta).cloned().collect();
            let missing: Vec<OsString> = expected_delta.difference(&staged).cloned().collect();
            return Err(partial_application_failure(
                "PARTIAL_MUTATION",
                format!(
                    "the staged delta of the applied review disagrees with the restored \
                     passages: {} path(s) staged outside them ({}), {} restored path(s) staged \
                     nothing ({})",
                    unexpected.len(),
                    listed_paths(&unexpected),
                    missing.len(),
                    listed_paths(&missing),
                ),
            ));
        }
        let working = scoped_working_state(self.git, resolved).map_err(OperationError::from)?;
        let expected = intended_working_state(self.git, resolved, &intended.tree)
            .map_err(OperationError::from)?;
        if working != expected {
            let mut moved: Vec<OsString> = working
                .keys()
                .chain(expected.keys())
                .filter(|path| working.get(*path) != expected.get(*path))
                .cloned()
                .collect();
            moved.sort();
            return Err(partial_application_failure(
                "PARTIAL_MUTATION",
                format!(
                    "the scoped working content does not equal the reviewed result after the \
                     application: {} path(s) differ ({}); nothing was committed",
                    moved.len(),
                    listed_paths(&moved),
                ),
            ));
        }
        if let Some(trace) = self.trace.as_ref() {
            trace.update_index(request, &manifest.candidate_oid, &intended.tree);
        }
        Ok(())
    }

    /// Inspects the workspace after one verified receipt commit — a review
    /// or a restoration — (SPEC 9.1 step 7): the reserved markers proved the
    /// commit itself, but a hook or a concurrent writer may still have
    /// changed the scoped working bytes or staged entries after it. The
    /// observation rules are the ones the capture operations share: the
    /// newer content is reported as observed, never as reviewed, and the
    /// verified commit stands.
    ///
    /// The expected working content is the scoped content of the intended
    /// tree T, so any byte a successful hook wrote after the commit surfaces
    /// here as changed effects and one warning, without unmaking the result
    /// that was committed.
    fn observe_receipt_completion(
        &self,
        scope: Option<&ScopeInput>,
        operation: &str,
        commit: &str,
        intended_tree: &str,
        effects: &mut Effects,
    ) -> Vec<Warning> {
        let mut warnings = Vec::new();
        let rescoped = self
            .context_scope_input(scope)
            .map_err(|error| format!("{}: {}", error.code, error.message))
            .and_then(|input| resolve_scope(self.git, &input).map_err(|error| error.to_string()));
        match rescoped {
            Ok(fresh) => {
                let observed = scoped_working_state(self.git, &fresh);
                let expected = intended_working_state(self.git, &fresh, intended_tree);
                match (observed, expected) {
                    (Ok(observed), Ok(expected)) => {
                        let moved: Vec<OsString> = observed
                            .keys()
                            .chain(expected.keys())
                            .filter(|path| observed.get(*path) != expected.get(*path))
                            .cloned()
                            .collect();
                        if !moved.is_empty() {
                            effects.observe_working_tree(ObservedEffect::Changed);
                            warnings.push(Warning {
                                code: "DIRTY_SCOPE",
                                message: format!(
                                    "the {operation} commit {commit} verified the committed \
                                     result, but {} scoped path(s) hold newer uncaptured \
                                     content: {}",
                                    moved.len(),
                                    listed_paths(&moved),
                                ),
                            });
                        }
                    }
                    _ => warnings.push(Warning {
                        code: "GIT_FAILED",
                        message: format!(
                            "the {operation} commit {commit} verified, but the working state \
                             after it could not be inspected",
                        ),
                    }),
                }
                // Entries staged after the commit stay staged: the operation
                // neither consumed nor discarded them.
                match self.git.staged_against(commit) {
                    Ok(staged) if !staged.is_empty() => {
                        effects.observe_index(ObservedEffect::Changed);
                        warnings.push(Warning {
                            code: "STAGED_CHANGES",
                            message: format!(
                                "the {operation} commit {commit} verified, but {} path(s) are \
                                 staged now: {}",
                                staged.len(),
                                listed_paths(&staged),
                            ),
                        });
                    }
                    Ok(_) => {}
                    Err(_) => warnings.push(Warning {
                        code: "GIT_FAILED",
                        message: format!(
                            "the {operation} commit {commit} verified, but the index after it \
                             could not be inspected",
                        ),
                    }),
                }
            }
            Err(detail) => warnings.push(Warning {
                code: "GIT_FAILED",
                message: format!(
                    "the {operation} commit {commit} verified, but the working scope after it \
                     could not be inspected: {detail}",
                ),
            }),
        }
        warnings
    }

    /// Resumes one retained review plan (SPEC 13.2 `resume_review`, 9.7).
    ///
    /// The retained-plan recovery path of SPEC 9.7: the writer still holds
    /// the plan, and the repository may still hold the result one
    /// interrupted application proved — the index at the intended tree T,
    /// the scoped working bytes at the reviewed result R, and the
    /// candidate C still at `HEAD`. The resume recomputes T from the plan
    /// alone, through the same disposable-index proof the dry run and the
    /// application share, and compares that proved result against the
    /// repository. It commits only when every dimension matches: `HEAD`
    /// still C, the real index exactly T, the scoped working content
    /// exactly R, no staged delta outside the restored passages, the
    /// adopted branch, the scope, and the plan's decisions over the
    /// recomputed manifest. An unmatched partial state refuses before the
    /// commit; the resume never reapplies a patch and never approves state
    /// it did not prove.
    ///
    /// This is newly validated explicit approval, not automatic crash
    /// recovery. The shared sequence of SPEC 9.1 still runs: the workspace
    /// probe and the strict parse, the mutation lock — the resume commits —
    /// and the receipt lookup of SPEC 8.7 before any present-day stale
    /// check, so a resume of a request that already completed returns its
    /// verified receipt instead of a refusal or a second commit, while a
    /// reused request ID under another fingerprint conflicts. The mutation
    /// guards carry the one index exception of SPEC 9.7: the retained
    /// result stays staged against C, so the staged-changes blocker alone
    /// does not refuse here, every other blocker still does, and the
    /// retained-state inspection proves the whole index tree against the
    /// recomputed T instead of trusting the blocker's own path list.
    ///
    /// A keep-all plan holds the candidate's own tree as T, so its exact
    /// retained state is the clean workspace, and resuming it is one
    /// explicitly validated empty review commit (SPEC 9.4). A dry run
    /// reports the same verification without the commit (SPEC 6.4).
    pub fn resume_review(&self, request: &ReviewRequest) -> ReviewDecisionResult {
        let mut effects = Effects::unchanged();
        let failure = |error: OperationError, effects: &Effects| {
            ReviewDecisionResult::failure(error, effects.clone())
        };

        // SPEC 9.1 steps 1 and 3: the workspace probe and the strict parse
        // and canonicalization of the input, before any present-day
        // eligibility inspection.
        if let Err(error) = require_work_tree(self.git) {
            return failure(error, &effects);
        }
        let plan = match ReviewPlan::parse(&request.plan) {
            Ok(plan) => plan,
            Err(problem) => return failure(problem.into(), &effects),
        };
        let fingerprint = match Identities::new(self.git)
            .and_then(|identities| identities.plan(&plan.identity()))
        {
            Ok(fingerprint) => fingerprint,
            Err(error) => return failure(OperationError::from(error), &effects),
        };

        // SPEC 9.1 step 2: the shared mutation lock serializes the commit
        // the resume may create with every other Margin write of the
        // repository. A dry run reserves neither the branch nor the
        // working files (SPEC 6.4), so it takes no lock.
        let _lock = if request.dry_run {
            None
        } else {
            match self.acquire_mutation_lock() {
                Ok(lock) => Some(lock),
                Err(error) => return failure(error, &effects),
            }
        };

        // SPEC 8.7, 9.7: the receipt lookup precedes every stale-state
        // check below, so a resume of a request that already completed
        // returns its verified receipt instead of a refusal or a second
        // commit. A dry run reports the matched receipt the way the
        // validation does: as data, with the effects of an operation that
        // wrote nothing (SPEC 6.4).
        match self.lookup_request_receipt(CommitKind::Review, &plan.request_id, &fingerprint.id()) {
            Ok(ReceiptLookup::Found(receipt)) => {
                if !request.dry_run {
                    effects.observe_commit(
                        CommitEffect::ExistingReceipt,
                        Some(receipt.commit.clone()),
                    );
                    if let Some(trace) = self.trace.as_ref() {
                        trace.submit_receipt(&plan.request_id, &receipt.commit);
                    }
                }
                return self.existing_receipt_decision(&receipt, &plan, request.dry_run, effects);
            }
            Ok(ReceiptLookup::Conflict {
                commit, recorded, ..
            }) => {
                if !request.dry_run {
                    if let Some(trace) = self.trace.as_ref() {
                        trace.submit_conflict(&plan.request_id);
                    }
                }
                return failure(
                    request_id_conflict(&plan.request_id, &commit, &recorded),
                    &effects,
                );
            }
            Ok(ReceiptLookup::Absent) => {}
            Err(error) => return failure(error, &effects),
        }

        // SPEC 9.7 requires the current `HEAD` to still equal the candidate
        // C the plan pins, so that dimension refuses first, with both object
        // IDs named, before the pinned comparison recomputes anything.
        let fresh_head = match self.git.head_commit() {
            Ok(head) => head,
            Err(error) => return failure(OperationError::from(error), &effects),
        };
        if fresh_head.as_deref() != Some(plan.candidate_oid.as_str()) {
            return failure(
                OperationError::new(
                    "STALE_REVIEW",
                    format!(
                        "the resume requires HEAD at the candidate {} of the retained plan, but \
                         HEAD holds {}",
                        plan.candidate_oid,
                        fresh_head.as_deref().unwrap_or("an unborn branch"),
                    ),
                )
                .recovery(&["margin", "doctor", "--json"]),
                &effects,
            );
        }

        // The pinned comparison and the plan's coverage of it (SPEC 7.3,
        // 9.3): recomputed from the history the plan pins, so the intended
        // tree below is the plan's own proved result, never an observation
        // of whatever the workspace holds.
        let pinned = match self.validated_plan(request.scope.as_ref(), &plan) {
            Ok(pinned) => pinned,
            Err(error) => return failure(error, &effects),
        };
        let PinnedReview {
            resolved,
            branch_ref,
            manifest,
            candidate_scope_digest,
        } = pinned;

        // SPEC 9.1 step 4 with the one index exception of SPEC 9.7: the
        // repository and storage policy in guard order, then the adopted
        // branch, then the author identity Margin never writes itself.
        let settings = match self.effective_settings() {
            Ok(settings) => settings,
            Err(error) => return failure(error, &effects),
        };
        let input = match scope_input_of(request.scope.as_ref(), &settings) {
            Some(input) => input,
            None => return failure(not_initialized(), &effects),
        };
        let branch_at_guard = match self.enforce_resume_guards(&resolved, &settings) {
            Ok(branch_ref) => branch_ref,
            Err(error) => return failure(error, &effects),
        };
        if let Err(error) = self.git.author_ident() {
            if matches!(error, GitError::Exit { .. }) {
                return failure(missing_identity(), &effects);
            }
            return failure(OperationError::from(error), &effects);
        }

        // The recomputed intended tree T (SPEC 9.3, 9.7): one disposable
        // index seeded from C reverse-applies the plan's selected complete
        // units. The real index and the working files are never touched,
        // so the resume never reapplies a patch.
        let decided: BTreeSet<&str> = plan.restore.iter().map(String::as_str).collect();
        let intended = match intended_review_tree(self.git, &manifest, &decided) {
            Ok(intended) => intended,
            Err(error) => return failure(OperationError::from(error), &effects),
        };
        let result_scope_digest = match scoped_content_digest(self.git, &intended.tree, &resolved) {
            Ok(digest) => digest,
            Err(error) => {
                return failure(
                    OperationError::new(error.public_code(), error.to_string()),
                    &effects,
                );
            }
        };
        let restored: Vec<String> = manifest
            .groups()
            .filter(|group| decided.contains(group.change_id.id().as_str()))
            .map(|group| group.change_id.id())
            .collect();
        let kept = manifest.groups().count() - restored.len();

        // The retained-state inspection of SPEC 9.7: every dimension the
        // resume commits against, proved against the recomputed result.
        if let Err(error) =
            self.verify_retained_review_state(&resolved, &manifest.candidate_oid, &intended)
        {
            return failure(error, &effects);
        }

        // The prepared marker map (SPEC 8.3), exactly the map the
        // interrupted application would have committed: C as the candidate
        // and parent, B as the baseline, T as the result tree, and the
        // request and plan fingerprint as the receipt. An invalid effective
        // message refuses before anything is committed (SPEC 7.3).
        let metadata = CommitMetadata {
            kind: CommitKind::Review,
            scope_digest: resolved.digest.id(),
            scope_spec: String::from_utf8(resolved.descriptor.canonical_bytes())
                .expect("the canonical descriptor is UTF-8 JSON"),
            candidate: manifest.candidate_oid.clone(),
            result_tree: intended.tree.clone(),
            baseline: Some(manifest.baseline_oid.clone()),
            request: Some(plan.request_id.clone()),
            plan: Some(fingerprint.id()),
            source: None,
        };
        if let Err(problem) = metadata.commit_message(&plan.message) {
            return failure(
                OperationError::new(problem.public_code(), problem.to_string()),
                &effects,
            );
        }

        // The decision data every return path below reports, from the
        // prepared context alone: the recomputed tree, the decided groups,
        // and the scoped result the decision approves.
        let observed = |commit: Option<String>, dry_run: bool| ReviewDecision {
            request_id: plan.request_id.clone(),
            review_id: manifest.review_id.id(),
            branch_ref: branch_ref.clone(),
            scope_id: resolved.digest.id(),
            baseline_oid: manifest.baseline_oid.clone(),
            candidate_oid: manifest.candidate_oid.clone(),
            candidate_scope_digest: candidate_scope_digest.id(),
            kept: Some(kept),
            restored: restored.clone(),
            paths: intended.paths.clone(),
            expected_tree: intended.tree.clone(),
            result_scope_digest: result_scope_digest.id(),
            message: plan.message.clone(),
            dry_run,
            commit,
            existing_receipt: None,
        };

        // A dry run is the verification half alone (SPEC 6.4): the retained
        // state the inspection just proved, reported without the commit.
        if request.dry_run {
            return ReviewDecisionResult::success(observed(None, true), Effects::unchanged());
        }

        // The working-entry baseline for post-failure observation (SPEC 9.1
        // step 7), read before the commit: the resume rewrites nothing, so
        // this equals R the inspection proved.
        let working_before = match scoped_working_state(self.git, &resolved) {
            Ok(before) => before,
            Err(error) => return failure(OperationError::from(error), &effects),
        };

        // The precondition recheck immediately before the only real
        // mutation of the resume (SPEC 9.1 step 6): the scope, the branch,
        // the candidate, and the retained result must all still hold when
        // the commit starts.
        if let Err(error) = self.recheck_resume_preconditions(
            &input,
            &resolved,
            &settings,
            &branch_at_guard,
            &manifest.candidate_oid,
            &intended,
        ) {
            return failure(error, &effects);
        }

        // The retained plan is accepted and pinned: the modeled protocol
        // restarts at its commit phase, because the retained result already
        // passed the application this resume never repeats (SPEC 15.7
        // `Submit` in resume mode).
        if let Some(trace) = self.trace.as_ref() {
            trace.submit_accepted(
                "resume",
                &plan.request_id,
                &manifest.candidate_oid,
                &manifest.baseline_oid,
                &restored,
            );
        }

        // The one real mutation of the resume (SPEC 9.7): a normal review
        // commit over the already validated index — no application, no
        // restaging from the working tree, no `commit --only`. The shared
        // marker writer compares the real index tree with T and verifies
        // the actual parent, tree, and every reserved trailer against the
        // prepared map before the baseline may advance (SPEC 8.4).
        let marker = match commit_marker(self.git, &metadata, &plan.message) {
            Ok(marker) => marker,
            Err(error) => {
                // Inspect what the failed commit left, then keep the commit
                // the failure itself names: a commit whose observed state
                // does not match the prepared map still exists as ordinary
                // history and confers no approval, while a killed commit
                // process loses the outcome, which is never narrowed back
                // to `none` (SPEC 9.6, 7.1). The retained state itself
                // stays behind for the next explicit recovery.
                if let Some(trace) = self.trace.as_ref() {
                    trace.fail(&plan.request_id);
                }
                let inspected = self.observe_resume_failure(
                    &mut effects,
                    &manifest.candidate_oid,
                    &intended.tree,
                    &working_before,
                    request.scope.as_ref(),
                );
                match &error {
                    MarkerCommitError::Verification { commit, .. } => {
                        effects.observe_commit(CommitEffect::Created, Some(commit.clone()));
                    }
                    MarkerCommitError::Git(GitError::Timeout { .. }) => {
                        effects.observe_commit(CommitEffect::Unknown, None);
                    }
                    _ => {}
                }
                let commit = match &error {
                    MarkerCommitError::Verification { commit, .. } => Some(commit.clone()),
                    MarkerCommitError::Git(GitError::Timeout { .. }) => None,
                    _ => inspected,
                };
                return ReviewDecisionResult::observed_failure(
                    OperationError::from(error),
                    observed(commit, false),
                    effects,
                );
            }
        };
        effects.observe_commit(CommitEffect::Created, Some(marker.commit.clone()));
        if let Some(trace) = self.trace.as_ref() {
            trace.publish(
                &plan.request_id,
                &marker.commit,
                &manifest.candidate_oid,
                &marker.result_tree,
            );
        }

        // The observed state after the verified commit (SPEC 9.1 step 7,
        // 9.2): a hook or a concurrent writer may have changed scoped
        // working bytes or staged entries after the commit the markers
        // proved. The response reports that newer content as observed, and
        // never as reviewed, while the verified commit stands.
        let warnings = self.observe_receipt_completion(
            request.scope.as_ref(),
            "review",
            &marker.commit,
            &intended.tree,
            &mut effects,
        );
        if let Some(trace) = self.trace.as_ref() {
            trace.respond(&plan.request_id, &marker.commit);
        }
        let data = observed(Some(marker.commit.clone()), false);
        if warnings.is_empty() {
            ReviewDecisionResult::success(data, effects)
        } else {
            ReviewDecisionResult::success_with_warnings(data, warnings, effects)
        }
    }

    /// The shared mutation inspection of the resume (SPEC 9.1 step 4, 9.7):
    /// every guard of [`Operations::enforce_mutation_guards`] with the one
    /// explicit index exception of SPEC 9.7 — an exact-plan resume runs
    /// over the retained result, which stays staged against the candidate.
    ///
    /// The exception admits the blocker, never the staging: the
    /// retained-state inspection proves the whole index tree against the
    /// recomputed T, so unrelated or partial staged content still refuses
    /// there. Conflict entries, in-progress Git operations, storage
    /// transformations, a detached `HEAD`, and a branch other than the
    /// adopted one all refuse here as they do for every other write.
    fn enforce_resume_guards(
        &self,
        resolved: &ResolvedScope,
        settings: &LocalSettings,
    ) -> Result<Option<String>, OperationError> {
        self.enforce_mutation_guards_except_staged(resolved, settings)
    }

    /// Proves the retained state of one resumed plan against the recomputed
    /// intended result (SPEC 9.7): `HEAD` still the candidate C, the staged
    /// delta exactly the restored passages, the real index tree exactly T,
    /// and the scoped working content exactly R.
    ///
    /// Every mismatch refuses before the commit with one typed stale
    /// refusal that names the dimension and the observed value, because the
    /// resume approves only the plan's own proved result: a partial
    /// application, a later writer, a moved candidate, or unrelated staged
    /// content each keep the decision unmade, and the recovery inspection
    /// of `doctor` remains the path that explains the observed state
    /// (SPEC 9.6, 9.7).
    fn verify_retained_review_state(
        &self,
        resolved: &ResolvedScope,
        candidate: &str,
        intended: &IntendedReview,
    ) -> Result<(), OperationError> {
        let retained = |dimension: String| {
            OperationError::new("STALE_REVIEW", dimension).recovery(&["margin", "doctor", "--json"])
        };

        // The candidate the prepared map names must still be `HEAD`.
        let fresh_head = self.git.head_commit().map_err(OperationError::from)?;
        if fresh_head.as_deref() != Some(candidate) {
            return Err(retained(format!(
                "the resume requires HEAD at the candidate {candidate} of the retained plan, but \
                 HEAD holds {}",
                fresh_head.as_deref().unwrap_or("an unborn branch"),
            )));
        }

        // No unrelated staged delta (SPEC 9.7): the staged paths are exactly
        // the restored passages the plan rewrites.
        let staged: BTreeSet<OsString> = self
            .git
            .staged_against(candidate)
            .map_err(OperationError::from)?
            .into_iter()
            .collect();
        let expected: BTreeSet<OsString> = intended.paths.iter().map(OsString::from).collect();
        if staged != expected {
            let unexpected: Vec<OsString> = staged.difference(&expected).cloned().collect();
            let missing: Vec<OsString> = expected.difference(&staged).cloned().collect();
            return Err(retained(format!(
                "the staged delta disagrees with the retained plan: {} path(s) staged outside \
                 its restored passages ({}), {} restored path(s) staged nothing ({})",
                unexpected.len(),
                listed_paths(&unexpected),
                missing.len(),
                listed_paths(&missing),
            )));
        }

        // The real index is exactly T (SPEC 9.7): conflict entries stop the
        // inspection as one observed mismatch, never a fallback that could
        // read the state as absent.
        if self
            .git
            .ls_files_stage()
            .map_err(OperationError::from)?
            .iter()
            .any(|entry| entry.stage > 0)
        {
            return Err(retained(
                "the index holds conflict entries, so it cannot equal the intended result tree \
                 of the retained plan"
                    .to_string(),
            ));
        }
        let index_tree = self.git.write_tree().map_err(OperationError::from)?;
        if index_tree != intended.tree {
            return Err(retained(format!(
                "the index holds the tree {index_tree} instead of the intended result tree {} of \
                 the retained plan",
                intended.tree,
            )));
        }

        // The scoped working content is exactly R (SPEC 9.7): the same
        // comparison the application verifies after the real apply, read
        // from the immutable entries of T alone.
        let working = scoped_working_state(self.git, resolved).map_err(OperationError::from)?;
        let expected_working = intended_working_state(self.git, resolved, &intended.tree)
            .map_err(OperationError::from)?;
        if working != expected_working {
            let mut moved: Vec<OsString> = working
                .keys()
                .chain(expected_working.keys())
                .filter(|path| working.get(*path) != expected_working.get(*path))
                .cloned()
                .collect();
            moved.sort();
            return Err(retained(format!(
                "the scoped working content does not equal the reviewed result of the retained \
                 plan: {} path(s) differ ({})",
                moved.len(),
                listed_paths(&moved),
            )));
        }
        Ok(())
    }

    /// The precondition recheck immediately before the resume's one real
    /// mutation (SPEC 9.1 step 6, 9.7): the scope, the branch, the
    /// candidate, and the retained result must all still hold when the
    /// commit starts.
    fn recheck_resume_preconditions(
        &self,
        input: &ScopeInput,
        resolved: &ResolvedScope,
        settings: &LocalSettings,
        branch_at_guard: &Option<String>,
        candidate: &str,
        intended: &IntendedReview,
    ) -> Result<(), OperationError> {
        // Path safety and the scope's path set (SPEC 14.3): a scoped path
        // that became a symlink, a nested repository, or one more eligible
        // untracked file all change what the one resolver returns.
        let fresh = resolve_scope(self.git, input).map_err(OperationError::from)?;
        if !fresh.blockers.is_empty() {
            return Err(scope_blockers_refusal(&fresh.blockers));
        }
        if fresh.paths != resolved.paths {
            return Err(scope_paths_moved("scope", &fresh.paths, &resolved.paths));
        }
        // The shared policy guards once more — the index exception of the
        // resume included — and the branch the guards observed.
        let branch = self.enforce_resume_guards(&fresh, settings)?;
        if branch != *branch_at_guard {
            return Err(branch_moved_refusal(branch_at_guard, &branch));
        }
        // The retained state itself, proved once more against the
        // recomputed result.
        self.verify_retained_review_state(&fresh, candidate, intended)
    }

    /// Inspects the workspace after the resume's commit failed (SPEC 9.7,
    /// 9.6): the resume itself staged nothing and rewrote nothing, so the
    /// retained result is what the inspection compares — the index against
    /// the recomputed T, the scoped working bytes against the baseline the
    /// inspection read before the commit — and the head names any commit a
    /// failed process still created. The refusal that follows reports this
    /// observed state and never claims a rollback (SPEC 9.6): the retained
    /// result stays exactly where it was for the next explicit resume.
    fn observe_resume_failure(
        &self,
        effects: &mut Effects,
        candidate: &str,
        intended_tree: &str,
        before: &BTreeMap<OsString, Option<WorkingEntry>>,
        scope: Option<&ScopeInput>,
    ) -> Option<String> {
        // The branch head first: it names the commit the response reports.
        let mut observed = None;
        match self.git.head_commit() {
            Ok(Some(head)) if head != candidate => {
                effects.observe_commit(CommitEffect::Created, Some(head.clone()));
                observed = Some(head);
            }
            // The head the resume started from still stands.
            Ok(_) => effects.observe_commit(CommitEffect::None, None),
            Err(_) => effects.observe_commit(CommitEffect::Unknown, None),
        }

        // The retained index: the resume stages nothing, so the index
        // stayed exactly T unless a concurrent writer staged something new.
        effects.observe_index(match self.git.write_tree() {
            Ok(tree) if tree == intended_tree => ObservedEffect::Unchanged,
            Ok(_) => ObservedEffect::Changed,
            Err(_) => ObservedEffect::Unknown,
        });

        // The scoped working content: unchanged while it still equals the
        // baseline the inspection read, which equals R the resume proved.
        let inspected = self
            .context_scope_input(scope)
            .and_then(|input| resolve_scope(self.git, &input).map_err(OperationError::from))
            .and_then(|fresh| scoped_working_state(self.git, &fresh).map_err(OperationError::from));
        effects.observe_working_tree(match inspected {
            Ok(after) if &after == before => ObservedEffect::Unchanged,
            Ok(_) => ObservedEffect::Changed,
            Err(_) => ObservedEffect::Unknown,
        });
        observed
    }

    /// Validates one already parsed plan against the pinned comparison the
    /// workspace holds (SPEC 7.3, 9.3): every context field, then the
    /// coverage of the decisions over the complete change manifest. Nothing
    /// here writes. The pinned comparison returns alone, so the caller
    /// continues from the one build every phase shares.
    fn validated_plan(
        &self,
        scope: Option<&ScopeInput>,
        plan: &ReviewPlan,
    ) -> Result<PinnedReview, OperationError> {
        let pinned = self.pinned_review(scope, false)?;
        if let Some(error) = stale_plan_error(plan, &pinned) {
            return Err(error);
        }
        let known: BTreeSet<String> = pinned
            .manifest
            .groups()
            .map(|group| group.change_id.id())
            .collect();
        domain::validate_plan_coverage(plan, &known)?;
        Ok(pinned)
    }

    /// Builds the pinned comparison every review phase shares (SPEC 7.2,
    /// 9.3): B from the first-parent walk, C from the committed head or
    /// the working preview, the full branch ref the identity names, the
    /// one grouped manifest, and the scoped candidate digest.
    ///
    /// `build_review` pages this into bounded detail; plan validation reads
    /// it as the universe of change identities one plan must decide. Both
    /// share one build, so no second comparison path exists (SPEC 13.6).
    /// The query observes only: it reads trees, blobs, and diffs through
    /// one disposable index when it previews, and refs, the real index,
    /// and working bytes stay unchanged.
    fn pinned_review(
        &self,
        scope: Option<&ScopeInput>,
        working: bool,
    ) -> Result<PinnedReview, OperationError> {
        let resolved = self.query_context(scope)?;
        let head = match self.git.head_commit() {
            Ok(Some(head)) => head,
            Ok(None) => {
                return Err(OperationError::new(
                    "BASELINE_UNAVAILABLE",
                    "the branch is unborn, so no candidate exists to compare",
                )
                .recovery(&["margin", "init", "--json"]));
            }
            Err(error) => return Err(error.into()),
        };
        let mut guard = self.history_walk();
        let walk = guard.as_mut().expect("the constructor fills the walk");
        let baseline = match walk.select_baseline(&head, &resolved.digest) {
            Ok(marker) => marker,
            Err(BaselineError::Unavailable(reason)) => {
                return Err(
                    OperationError::new("BASELINE_UNAVAILABLE", reason.to_string())
                        .recovery(&["margin", "status", "--json"]),
                );
            }
            Err(error) => return Err(error.into()),
        };

        // The full ref the identity names (SPEC 7.2): where `HEAD` is
        // attached, which is the adopted branch in every ordinary
        // workspace; a detached `HEAD` falls back to the stored adopted
        // branch, and a scope without either keeps the deterministic name
        // `HEAD` so the identity still exists.
        let branch_ref = match self.git.head_symbolic_ref() {
            Ok(Some(full)) => full,
            _ => self
                .effective_settings()
                .ok()
                .and_then(|settings| settings.branch)
                .map(|branch| format!("refs/heads/{branch}"))
                .unwrap_or_else(|| "HEAD".to_string()),
        };

        // C: the committed candidate, or the working preview one checkpoint
        // would create (SPEC 6.2 `--working`).
        let candidate = if working {
            working_preview_tree(self.git, &resolved, &head)?
        } else {
            head
        };

        let manifest = build_grouped_manifest(
            self.git,
            &branch_ref,
            &baseline.commit,
            &candidate,
            &resolved.descriptor,
        )
        .map_err(|error| OperationError::new(error.public_code(), error.to_string()))?;
        let candidate_scope_digest = scoped_content_digest(self.git, &candidate, &resolved)
            .map_err(|error| OperationError::new(error.public_code(), error.to_string()))?;

        Ok(PinnedReview {
            resolved,
            branch_ref,
            manifest,
            candidate_scope_digest,
        })
    }

    /// Restores one document from adopted history (SPEC 13.2
    /// `restore_document`, 7.4, 9.5).
    ///
    /// The plan document parses under the strict input contract of SPEC 7.4,
    /// and the receipt lookup of SPEC 8.7 runs next, before any present-day
    /// stale check (SPEC 9.1 step 3): an exact retry of a request that
    /// already completed returns the verified receipt commit as
    /// `existing_receipt` — assembled from the recorded canonical scope and
    /// immutable Git objects alone, so dirty files, staged content, writer
    /// activity, storage-policy blockers, a moved candidate, and a scope no
    /// new mutation could resolve never hide a receipt the history still
    /// holds. A request ID reused under a different fingerprint conflicts
    /// with `REQUEST_ID_CONFLICT`, and a scan that cannot complete blocks
    /// instead of guessing the request absent.
    ///
    /// Without a receipt, the plan's context must name the workspace it
    /// pins: the adopted branch, the scope digest, and the expected
    /// candidate `HEAD` still holds. The source revision must resolve to
    /// committed content of the adopted history — the unfiltered
    /// first-parent ancestry of the candidate, itself included — and the
    /// plan's path must name one restorable document inside the scope: a
    /// regular or executable blob entry, or absence (SPEC 7.4). The shared
    /// guards then require the clean scoped working content, the clean
    /// index, and the committed candidate of SPEC 9.5.
    ///
    /// The intended result is proved in one disposable Git index seeded
    /// from C: the patch the fixed diff contract emits for the one document
    /// forward-applies, and `write-tree` yields the full intended tree T
    /// with a staged delta that names exactly the document. The application
    /// replays exactly those patch bytes through the guarded apply/index
    /// path of SPEC 9.4 — check, apply, verify the real index equals T and
    /// the scoped working content equals the result — before the shared
    /// marker writer commits the validated index with the `restore`
    /// trailers of SPEC 8.3, `Margin-Source` included.
    ///
    /// The commit is a `restore` receipt, never a review marker: the
    /// baseline the first-parent walk selects stays unchanged, so the
    /// restored content is pending review, and the replaced candidate
    /// stays reachable as the parent. A restoration that changes no
    /// content still commits one receipt per new request, so its retry
    /// semantics stay durable (SPEC 7.4). A dry run reports the proved
    /// intended result and writes nothing (SPEC 6.4).
    pub fn restore_document(&self, request: &RestoreRequest) -> RestoreResult {
        // A dry run reports the intended effects without committing (SPEC
        // 6.4): it is exactly the validation half below, without the lock
        // and without the application, so no interface can reach a commit
        // through the dry-run flag.
        if request.dry_run {
            return self.validate_restore(request);
        }
        let mut effects = Effects::unchanged();
        let failure = |error: OperationError, effects: &Effects| {
            RestoreResult::failure(error, effects.clone())
        };

        // SPEC 9.1 steps 1 and 3: the workspace probe and the strict parse
        // and canonicalization of the input, before any present-day
        // eligibility inspection.
        if let Err(error) = require_work_tree(self.git) {
            return failure(error, &effects);
        }
        let plan = match RestorePlan::parse(&request.plan) {
            Ok(plan) => plan,
            Err(problem) => return failure(problem.into(), &effects),
        };
        let fingerprint = match Identities::new(self.git)
            .and_then(|identities| identities.restore_plan(&plan.identity()))
        {
            Ok(fingerprint) => fingerprint,
            Err(error) => return failure(OperationError::from(error), &effects),
        };

        // SPEC 9.1 step 2: the shared mutation lock serializes this
        // restoration with every other Margin write of the repository and
        // is held through the postcondition inspection below.
        let _lock = match self.acquire_mutation_lock() {
            Ok(lock) => lock,
            Err(error) => return failure(error, &effects),
        };

        // SPEC 8.7, 9.1 step 3: the receipt lookup precedes every
        // stale-state check below, so an already completed request returns
        // its verified receipt instead of a refusal or a second commit.
        match self.lookup_request_receipt(CommitKind::Restore, &plan.request_id, &fingerprint.id())
        {
            Ok(ReceiptLookup::Found(receipt)) => {
                effects.observe_commit(CommitEffect::ExistingReceipt, Some(receipt.commit.clone()));
                return self.existing_restore_response(&receipt, &plan, false, effects);
            }
            Ok(ReceiptLookup::Conflict {
                commit, recorded, ..
            }) => {
                return failure(
                    request_id_conflict(&plan.request_id, &commit, &recorded),
                    &effects,
                );
            }
            Ok(ReceiptLookup::Absent) => {}
            Err(error) => return failure(error, &effects),
        }

        // The pinned context, the shared guards, and the proved intended
        // result (SPEC 9.1 steps 4 and 5), now under the lock.
        let pinned = match self.validated_restore(request.scope.as_ref(), &plan) {
            Ok(pinned) => pinned,
            Err(error) => return failure(error, &effects),
        };
        let PinnedRestore {
            resolved,
            branch_ref,
            guard_branch,
            candidate_oid,
            candidate_scope_digest,
            revision_oid,
            intended,
            baseline_oid,
        } = pinned;
        let result_scope_digest = match scoped_content_digest(self.git, &intended.tree, &resolved) {
            Ok(digest) => digest,
            Err(error) => {
                return failure(
                    OperationError::new(error.public_code(), error.to_string()),
                    &effects,
                );
            }
        };

        // The prepared marker map (SPEC 8.3): the restoration names C as its
        // candidate and parent, T as its result tree, the request and plan
        // fingerprint as its receipt, and the document and revision as its
        // source. An invalid effective message refuses before anything is
        // committed, exactly as the checkpoint contract does (SPEC 7.3).
        let metadata = CommitMetadata {
            kind: CommitKind::Restore,
            scope_digest: resolved.digest.id(),
            scope_spec: String::from_utf8(resolved.descriptor.canonical_bytes())
                .expect("the canonical descriptor is UTF-8 JSON"),
            candidate: candidate_oid.clone(),
            result_tree: intended.tree.clone(),
            baseline: None,
            request: Some(plan.request_id.clone()),
            plan: Some(fingerprint.id()),
            source: Some(RestoreSource {
                path: plan.path.clone(),
                revision: revision_oid.clone(),
            }),
        };
        if let Err(problem) = metadata.commit_message(&plan.message) {
            return failure(
                OperationError::new(problem.public_code(), problem.to_string()),
                &effects,
            );
        }

        // The working-entry baseline for post-failure observation (SPEC 9.1
        // step 7), read before the first real mutation.
        let working_before = match scoped_working_state(self.git, &resolved) {
            Ok(before) => before,
            Err(error) => return failure(OperationError::from(error), &effects),
        };

        // The precondition recheck immediately before the first real
        // mutation (SPEC 9.1 step 6): for a content-changing restoration the
        // forward application below is that mutation; for a no-content-change
        // restoration the commit itself is.
        let settings = match self.effective_settings() {
            Ok(settings) => settings,
            Err(error) => return failure(error, &effects),
        };
        let input = match scope_input_of(request.scope.as_ref(), &settings) {
            Some(input) => input,
            None => return failure(not_initialized(), &effects),
        };
        if let Err(error) = self.recheck_candidate_preconditions(
            &input,
            &resolved,
            &settings,
            &guard_branch,
            &candidate_oid,
        ) {
            return failure(error, &effects);
        }

        // The restoration data every return path below reports, from the
        // prepared context alone.
        let observed = |commit: Option<String>,
                        existing_receipt: Option<String>,
                        result_scope_digest: &Fingerprint| RestoreOutcome {
            request_id: plan.request_id.clone(),
            branch_ref: branch_ref.clone(),
            scope_id: resolved.digest.id(),
            candidate_oid: candidate_oid.clone(),
            candidate_scope_digest: candidate_scope_digest.id(),
            path: plan.path.clone(),
            revision_oid: revision_oid.clone(),
            paths: intended.paths.clone(),
            expected_tree: intended.tree.clone(),
            result_scope_digest: result_scope_digest.id(),
            baseline_oid: baseline_oid.clone(),
            message: plan.message.clone(),
            dry_run: false,
            commit,
            existing_receipt,
        };

        // The guarded apply path of SPEC 9.4, replaying exactly the proved
        // patch bytes forward. A failure at that boundary leaves real state
        // behind, so the state is inspected and reported instead of being
        // called a rollback (SPEC 9.6).
        if let Err(error) =
            self.apply_selected_restore(&input, &resolved, &candidate_oid, &plan.path, &intended)
        {
            let inspected = self.observe_capture_failure(
                &mut effects,
                Some(&candidate_oid),
                &working_before,
                request.scope.as_ref(),
            );
            return RestoreResult::observed_failure(
                error,
                observed(inspected, None, &result_scope_digest),
                effects,
            );
        }
        if !intended.patch.is_empty() {
            // The application rewrote the document's working bytes and staged
            // exactly it (SPEC 9.4 step 2); the commit below verifies both
            // again against the prepared map.
            effects.observe_working_tree(ObservedEffect::Changed);
            effects.observe_index(ObservedEffect::Changed);
        }

        // One normal restore commit over the verified index (SPEC 9.5): the
        // shared marker writer compares the real index tree with T and
        // verifies the actual parent, tree, and every reserved trailer
        // against the prepared map. A no-content-change restoration commits
        // empty, which `git commit --allow-empty` records as one receipt.
        let marker = match commit_marker(self.git, &metadata, &plan.message) {
            Ok(marker) => marker,
            Err(error) => {
                // Inspect what the failed application left, then keep the
                // commit the failure itself names: a commit whose observed
                // state does not match the prepared map still exists as
                // ordinary history and confers nothing, while a killed
                // commit process loses the outcome, which is never narrowed
                // back to `none` (SPEC 9.6, 7.1).
                let inspected = self.observe_capture_failure(
                    &mut effects,
                    Some(&candidate_oid),
                    &working_before,
                    request.scope.as_ref(),
                );
                match &error {
                    MarkerCommitError::Verification { commit, .. } => {
                        effects.observe_commit(CommitEffect::Created, Some(commit.clone()));
                    }
                    MarkerCommitError::Git(GitError::Timeout { .. }) => {
                        effects.observe_commit(CommitEffect::Unknown, None);
                    }
                    _ => {}
                }
                let commit = match &error {
                    MarkerCommitError::Verification { commit, .. } => Some(commit.clone()),
                    MarkerCommitError::Git(GitError::Timeout { .. }) => None,
                    _ => inspected,
                };
                return RestoreResult::observed_failure(
                    OperationError::from(error),
                    observed(commit, None, &result_scope_digest),
                    effects,
                );
            }
        };
        effects.observe_commit(CommitEffect::Created, Some(marker.commit.clone()));

        // The observed state after the verified commit (SPEC 9.1 step 7): a
        // hook or a concurrent writer may have changed scoped working bytes
        // or staged entries after the commit the markers proved. The newer
        // content is reported as observed and never as reviewed, while the
        // verified receipt stands.
        let warnings = self.observe_receipt_completion(
            request.scope.as_ref(),
            "restore",
            &marker.commit,
            &intended.tree,
            &mut effects,
        );
        let data = observed(Some(marker.commit), None, &result_scope_digest);
        if warnings.is_empty() {
            RestoreResult::success(data, effects)
        } else {
            RestoreResult::success_with_warnings(data, warnings, effects)
        }
    }

    /// The validation half of one historical restoration (SPEC 6.4, 9.5):
    /// every check the application runs before its first real mutation, with
    /// the proved intended result, written nowhere.
    ///
    /// The receipt lookup still precedes the present-day stale checks
    /// (SPEC 8.7), so one dry run of an already completed request reports
    /// that request's verified receipt, exactly as the application would.
    fn validate_restore(&self, request: &RestoreRequest) -> RestoreResult {
        let failure = |error: OperationError| RestoreResult::failure(error, Effects::unchanged());

        if let Err(error) = require_work_tree(self.git) {
            return failure(error);
        }
        let plan = match RestorePlan::parse(&request.plan) {
            Ok(plan) => plan,
            Err(problem) => return failure(problem.into()),
        };
        let fingerprint = match Identities::new(self.git)
            .and_then(|identities| identities.restore_plan(&plan.identity()))
        {
            Ok(fingerprint) => fingerprint,
            Err(error) => return failure(OperationError::from(error)),
        };
        match self.lookup_request_receipt(CommitKind::Restore, &plan.request_id, &fingerprint.id())
        {
            Ok(ReceiptLookup::Found(receipt)) => {
                return self.existing_restore_response(&receipt, &plan, true, Effects::unchanged());
            }
            Ok(ReceiptLookup::Conflict {
                commit, recorded, ..
            }) => {
                return failure(request_id_conflict(&plan.request_id, &commit, &recorded));
            }
            Ok(ReceiptLookup::Absent) => {}
            Err(error) => return failure(error),
        }

        let pinned = match self.validated_restore(request.scope.as_ref(), &plan) {
            Ok(pinned) => pinned,
            Err(error) => return failure(error),
        };
        let result_scope_digest =
            match scoped_content_digest(self.git, &pinned.intended.tree, &pinned.resolved) {
                Ok(digest) => digest,
                Err(error) => {
                    return failure(OperationError::new(error.public_code(), error.to_string()));
                }
            };
        RestoreResult::success(
            RestoreOutcome {
                request_id: plan.request_id,
                branch_ref: pinned.branch_ref,
                scope_id: pinned.resolved.digest.id(),
                candidate_oid: pinned.candidate_oid,
                candidate_scope_digest: pinned.candidate_scope_digest.id(),
                path: plan.path,
                revision_oid: pinned.revision_oid,
                paths: pinned.intended.paths,
                expected_tree: pinned.intended.tree,
                result_scope_digest: result_scope_digest.id(),
                baseline_oid: pinned.baseline_oid,
                message: plan.message,
                dry_run: true,
                commit: None,
                existing_receipt: None,
            },
            Effects::unchanged(),
        )
    }

    /// The pinned restoration context of one plan (SPEC 7.4, 9.1 steps 4
    /// and 5): the resolved scope, the full branch ref, the committed
    /// candidate the plan pins, the resolved source revision, the proved
    /// intended result, and the baseline the restoration leaves unchanged.
    ///
    /// Every context field must match the workspace the plan pins, so a
    /// plan written against another branch, scope, or candidate is stale,
    /// never silently reused. The source revision must resolve to committed
    /// content and belong to the adopted history — the unfiltered
    /// first-parent ancestry of the candidate, itself included (SPEC 7.4).
    /// The shared mutation guards require the storage profile, the adopted
    /// branch, the clean index, and the clean scoped working content of
    /// SPEC 9.5, and the intended result is proved in one disposable index.
    fn validated_restore(
        &self,
        scope: Option<&ScopeInput>,
        plan: &RestorePlan,
    ) -> Result<PinnedRestore, OperationError> {
        let settings = self.effective_settings()?;
        let input = scope_input_of(scope, &settings).ok_or_else(not_initialized)?;
        let resolved = resolve_scope(self.git, &input).map_err(OperationError::from)?;
        if !resolved.blockers.is_empty() {
            return Err(scope_blockers_refusal(&resolved.blockers));
        }
        if plan.scope_id != resolved.digest.id() {
            return Err(stale_restore_plan("scope_id"));
        }
        // The full ref the identity names (SPEC 7.2): where `HEAD` is
        // attached, which is the adopted branch in every ordinary workspace;
        // a detached `HEAD` falls back to the stored adopted branch, and a
        // scope without either keeps the deterministic name `HEAD`.
        let branch_ref = match self.git.head_symbolic_ref()? {
            Some(full) => full,
            None => settings
                .branch
                .as_ref()
                .map(|branch| format!("refs/heads/{branch}"))
                .unwrap_or_else(|| "HEAD".to_string()),
        };
        if plan.branch_ref != branch_ref {
            return Err(stale_restore_plan("branch_ref"));
        }
        // A restoration replaces exactly one committed candidate: the plan's
        // expected candidate (SPEC 7.4).
        let candidate_oid = match self.git.head_commit()? {
            Some(head) => head,
            None => {
                return Err(OperationError::new(
                    "BASELINE_UNAVAILABLE",
                    "the branch is unborn, so no candidate exists to restore against",
                )
                .recovery(&["margin", "init", "--json"]));
            }
        };
        if plan.candidate_oid != candidate_oid {
            return Err(stale_restore_plan("candidate_oid"));
        }

        // The plan's path must name one document inside the adopted scope
        // (SPEC 7.4): the descriptor's literal boundaries decide, so a
        // document no longer tracked still restores, while a path outside
        // the scope never does.
        if !resolved.descriptor.contains_path(plan.path.as_bytes()) {
            return Err(OperationError::new(
                "INVALID_ARGUMENT",
                format!(
                    "the path {:?} lies outside the writing scope, so no restoration may \
                     write it",
                    plan.path,
                ),
            ));
        }

        // The source revision must resolve to available committed content
        // and belong to the adopted history (SPEC 7.4): the same unfiltered
        // first-parent ancestry the baseline walk and the receipt scan use,
        // the candidate itself included. The strict parser already pinned
        // the value to one full object ID, so resolution validates the
        // commit exists rather than reinterpreting the user string.
        let revision_oid = resolve_immutable_revision(self.git, &plan.revision)?;
        let adopted = self.git.first_parent_history(&candidate_oid)?;
        if !adopted.iter().any(|commit| commit == &revision_oid) {
            return Err(OperationError::new(
                "INVALID_ARGUMENT",
                format!(
                    "the source revision {revision_oid} belongs to no adopted history of the \
                     candidate {candidate_oid}: release 0.1 restores only from the unfiltered \
                     first-parent ancestry of the candidate",
                ),
            ));
        }

        // SPEC 9.1 step 4: the shared mutation inspection in guard order,
        // plus the author identity Margin never writes itself.
        let guard_branch = self.enforce_mutation_guards(&resolved, &settings)?;
        if let Err(error) = self.git.author_ident() {
            if matches!(error, GitError::Exit { .. }) {
                return Err(missing_identity());
            }
            return Err(OperationError::from(error));
        }
        // SPEC 9.5: clean scoped working content. Newer uncaptured bytes
        // belong to the next checkpoint, never to this restoration.
        match scoped_working_changes(self.git, &resolved, &candidate_oid)? {
            changes if changes.is_empty() => {}
            changes => return Err(dirty_scope_refusal(&changes, "restoration")),
        }

        // The proved intended result (SPEC 9.1 step 5, 9.5).
        let intended = intended_restore_tree(self.git, &candidate_oid, &revision_oid, &plan.path)?;
        let candidate_scope_digest = scoped_content_digest(self.git, &candidate_oid, &resolved)
            .map_err(|error| OperationError::new(error.public_code(), error.to_string()))?;

        // The baseline the restoration leaves unchanged (SPEC 9.5): the one
        // walk of SPEC 8.5 answers, and no anchor is a reportable state, not
        // a failure — a restoration approves nothing.
        let mut guard = self.history_walk();
        let walk = guard.as_mut().expect("the constructor fills the walk");
        let baseline_oid = match walk.select_baseline(&candidate_oid, &resolved.digest) {
            Ok(marker) => Some(marker.commit),
            Err(BaselineError::Unavailable(_)) => None,
            Err(error) => return Err(error.into()),
        };

        Ok(PinnedRestore {
            resolved,
            branch_ref,
            guard_branch,
            candidate_oid,
            candidate_scope_digest,
            revision_oid,
            intended,
            baseline_oid,
        })
    }

    /// Applies the proved restoration patch through the real Git index and
    /// working tree (SPEC 9.4 steps 1-3, 9.5) and verifies the real result
    /// against the proved preview.
    ///
    /// The same explicit whitespace policy the applicability check and the
    /// disposable-index preview used carries the exact patch bytes forward:
    /// the application replays what the preview proved and never regenerates
    /// a different patch or broadens the restored document.
    ///
    /// Afterwards the real index tree must equal T, the staged delta must
    /// name exactly the restored document, and the scoped working content
    /// — read through a freshly resolved scope, so a document the
    /// restoration resurrected joins the comparison — must equal the result
    /// the preview intends. Any failure at these boundaries leaves applied
    /// entries or restored working bytes behind: the caller inspects and
    /// reports that state, and this method never claims a rollback or
    /// grants a receipt to an unmatched result (SPEC 9.6).
    fn apply_selected_restore(
        &self,
        input: &ScopeInput,
        resolved: &ResolvedScope,
        candidate: &str,
        path: &str,
        intended: &IntendedRestore,
    ) -> Result<(), OperationError> {
        let expected_delta: BTreeSet<OsString> =
            intended.paths.iter().map(OsString::from).collect();
        if !intended.patch.is_empty() {
            // SPEC 9.4 step 1: the applicability check against the real
            // index and working tree, under the guards that just proved both
            // equal C.
            if let Err(error) = self.git.apply_patch_index(&intended.patch, false, true) {
                return Err(partial_application_failure(
                    matches!(error, GitError::Timeout { .. })
                        .then_some("OUTCOME_UNKNOWN")
                        .unwrap_or("PARTIAL_MUTATION"),
                    format!(
                        "the restoration patch of {path:?} does not apply to the real index and \
                         working tree: {error}",
                    ),
                ));
            }
            // SPEC 9.4 step 2: the real application of the proved bytes.
            if let Err(error) = self.git.apply_patch_index(&intended.patch, false, false) {
                return Err(partial_application_failure(
                    matches!(error, GitError::Timeout { .. })
                        .then_some("OUTCOME_UNKNOWN")
                        .unwrap_or("PARTIAL_MUTATION"),
                    format!(
                        "the restoration patch of {path:?} failed to apply to the real index and \
                         working tree: {error}",
                    ),
                ));
            }
        }

        // SPEC 9.4 step 3: the real index tree equals T and the staged delta
        // stays exactly the restored document.
        let index_tree = self.git.write_tree().map_err(OperationError::from)?;
        if index_tree != intended.tree {
            return Err(partial_application_failure(
                "PARTIAL_MUTATION",
                format!(
                    "the applied restoration holds the index tree {index_tree} instead of the \
                     proved result tree {}; the committed result would not match the preview",
                    intended.tree,
                ),
            ));
        }
        let staged: BTreeSet<OsString> = self
            .git
            .staged_against(candidate)
            .map_err(OperationError::from)?
            .into_iter()
            .collect();
        if staged != expected_delta {
            let unexpected: Vec<OsString> = staged.difference(&expected_delta).cloned().collect();
            let missing: Vec<OsString> = expected_delta.difference(&staged).cloned().collect();
            return Err(partial_application_failure(
                "PARTIAL_MUTATION",
                format!(
                    "the staged delta of the applied restoration disagrees with the restored \
                     document: {} path(s) staged outside it ({}), {} restored path(s) staged \
                     nothing ({})",
                    unexpected.len(),
                    listed_paths(&unexpected),
                    missing.len(),
                    listed_paths(&missing),
                ),
            ));
        }
        // The scoped working content equals the result R (SPEC 9.4 step 3),
        // read through a freshly resolved scope so a resurrected document —
        // one the pre-application scope could not list because no tracked
        // or working entry held it — joins the comparison on both sides.
        let fresh = resolve_scope(self.git, input).map_err(OperationError::from)?;
        if !fresh.blockers.is_empty() {
            return Err(scope_blockers_refusal(&fresh.blockers));
        }
        // A content-changing restoration legitimately moves exactly the one
        // document in the scope's path set: a resurrected document joins it
        // and a deleted one leaves it. Any other path that joined or left
        // while the application ran is a moved scope, not a restoration
        // effect.
        let document = OsString::from(path);
        let joined_or_left = |paths: &BTreeMap<OsString, ScopedPathState>| {
            paths
                .keys()
                .filter(|entry| *entry != &document && !resolved.paths.contains_key(*entry))
                .count()
                + resolved
                    .paths
                    .keys()
                    .filter(|entry| *entry != &document && !paths.contains_key(*entry))
                    .count()
        };
        if joined_or_left(&fresh.paths) > 0 {
            return Err(scope_paths_moved("scope", &fresh.paths, &resolved.paths));
        }
        let working = scoped_working_state(self.git, &fresh).map_err(OperationError::from)?;
        let expected = intended_working_state(self.git, &fresh, &intended.tree)
            .map_err(OperationError::from)?;
        if working != expected {
            let mut moved: Vec<OsString> = working
                .keys()
                .chain(expected.keys())
                .filter(|path| working.get(*path) != expected.get(*path))
                .cloned()
                .collect();
            moved.sort();
            return Err(partial_application_failure(
                "PARTIAL_MUTATION",
                format!(
                    "the scoped working content does not equal the restored result after the \
                     application: {} path(s) differ ({}); nothing was committed",
                    moved.len(),
                    listed_paths(&moved),
                ),
            ));
        }
        Ok(())
    }

    /// Builds the response one exact restoration retry reports (SPEC 8.7):
    /// the verified receipt commit, plus the response data of the request
    /// itself, assembled from the recorded canonical scope and immutable Git
    /// objects alone.
    ///
    /// The fingerprint match proved the retry carries the same context
    /// fields, the document, the revision, and the message as the plan that
    /// produced the receipt. The receipt supplies what marker validation
    /// proved against the commit itself: its candidate, its result tree,
    /// and its recorded source and scope, and the scoped digests and
    /// rewritten paths are computed from the candidate and result trees
    /// through that one recorded descriptor.
    ///
    /// Nothing here resolves the present-day scope or traverses a working
    /// path: a scope root that became a symlink, vanished, or became
    /// unreadable blocks a new mutation but never hides the receipt a
    /// completed request already holds (SPEC 8.7). The observed state stays
    /// separate in the effects; it is acceptable that `HEAD` advanced after
    /// the operation completed.
    fn existing_restore_response(
        &self,
        receipt: &ValidMarker,
        plan: &RestorePlan,
        dry_run: bool,
        effects: Effects,
    ) -> RestoreResult {
        let failure = |error: OperationError| RestoreResult::failure(error, Effects::unchanged());

        // The immutable tree facts through the recorded descriptor (SPEC
        // 8.6). A failure here means the objects the receipt depends on are
        // no longer readable, which is one explicit refusal naming the
        // receipt — never silence and never a fallback to absence.
        let digest_of = |revision: &str, what: &str| -> Result<Fingerprint, OperationError> {
            recorded_content_digest(self.git, revision, &receipt.scope).map_err(|error| {
                OperationError::new(
                    error.public_code(),
                    format!(
                        "the receipt {} cannot be recomputed from history: its {what} is \
                         unreadable: {error}",
                        receipt.commit,
                    ),
                )
            })
        };
        let candidate_scope_digest = match digest_of(&receipt.candidate, "candidate") {
            Ok(digest) => digest,
            Err(error) => return failure(error),
        };
        let result_scope_digest = match digest_of(&receipt.result_tree, "result tree") {
            Ok(digest) => digest,
            Err(error) => return failure(error),
        };
        let paths = match recorded_tree_difference(
            self.git,
            &receipt.candidate,
            &receipt.result_tree,
            &receipt.scope,
        ) {
            Ok(paths) => paths,
            Err(error) => {
                return failure(OperationError::new(
                    error.public_code(),
                    format!(
                        "the receipt {} cannot be recomputed from history: its changed scoped \
                         entries are unreadable: {error}",
                        receipt.commit,
                    ),
                ));
            }
        };
        // The source the verified marker records (SPEC 8.3): the document
        // and revision the fingerprinted plan pinned.
        let source = receipt.source.clone().unwrap_or_else(|| RestoreSource {
            path: plan.path.clone(),
            revision: plan.revision.clone(),
        });
        // The baseline the completed request left unchanged (SPEC 9.5),
        // answered through the one walk under the recorded scope digest. No
        // anchor is a reportable state, not a failure.
        let mut guard = self.history_walk();
        let walk = guard.as_mut().expect("the constructor fills the walk");
        let baseline_oid = match walk
            .select_baseline_matching(&receipt.candidate, &receipt.scope_digest)
        {
            Ok(marker) => Some(marker.commit),
            Err(BaselineError::Unavailable(_)) => None,
            Err(error) => return failure(error.into()),
        };

        RestoreResult::success(
            RestoreOutcome {
                request_id: plan.request_id.clone(),
                branch_ref: plan.branch_ref.clone(),
                scope_id: receipt.scope_digest.clone(),
                candidate_oid: receipt.candidate.clone(),
                candidate_scope_digest: candidate_scope_digest.id(),
                path: source.path.clone(),
                revision_oid: source.revision.clone(),
                paths,
                expected_tree: receipt.result_tree.clone(),
                result_scope_digest: result_scope_digest.id(),
                baseline_oid,
                message: plan.message.clone(),
                dry_run,
                commit: None,
                existing_receipt: Some(receipt.commit.clone()),
            },
            effects,
        )
    }

    /// Inspects recovery state (SPEC 13.2 `inspect_recovery`, 9.7): the
    /// report `doctor` returns.
    ///
    /// The query never mutates. The scope resolves through the one resolver,
    /// the baseline through the one walk, the working fingerprint through the
    /// one content digest of SPEC 8.6, and the receipt through the same scan
    /// [`find_receipt`] runs, so no interface judges recovery state on its
    /// own (SPEC 13.6). The index tree comes from one disposable copy of
    /// the real index through [`Operations::observed_index_tree`]: plain
    /// `git write-tree` rewrites the index it addresses to store the
    /// computed tree cache, optional locks off or not, so the query reads
    /// the same tree without changing one real index byte, and the tree
    /// stays `None` while conflict stages stop the computation.
    ///
    /// Blockers and unavailable anchors are observations the data carries,
    /// not failures of the query, because `doctor` is exactly the command a
    /// writer runs while the workspace looks wrong. An observation that
    /// cannot be made honest — a Git read that fails, a receipt scan that
    /// cannot complete — fails the whole query typed instead of narrowing
    /// the report to the parts that worked.
    pub fn inspect_recovery(&self, request: &RecoveryRequest) -> RecoveryResult {
        let failure = |error: OperationError| RecoveryResult::failure(error, Effects::unchanged());
        let resolved = match self.query_context(request.scope.as_ref()) {
            Ok(resolved) => resolved,
            Err(error) => return failure(error),
        };
        let object_format = match self.git.object_format() {
            Ok(format) => format,
            Err(error) => return failure(OperationError::from(error)),
        };
        let head = match self.git.head_commit() {
            Ok(head) => head,
            Err(error) => return failure(OperationError::from(error)),
        };
        let baseline = match &head {
            Some(candidate) => {
                let mut guard = self.history_walk();
                let walk = guard.as_mut().expect("the constructor fills the walk");
                match walk.select_baseline(candidate, &resolved.digest) {
                    Ok(marker) => BaselineStatus::Anchored(Box::new(marker)),
                    Err(BaselineError::Unavailable(reason)) => {
                        BaselineStatus::Unavailable(reason)
                    }
                    Err(error) => return failure(OperationError::from(error)),
                }
            }
            None => BaselineStatus::NoCandidate,
        };

        // The index tree: conflict stages stop `write-tree`, and that state
        // is one observation the report carries as `None`, never a failure
        // of the query (SPEC 9.7).
        let index = match self.git.ls_files_stage() {
            Ok(index) => index,
            Err(error) => return failure(OperationError::from(error)),
        };
        let index_tree = if index.iter().any(|entry| entry.stage > 0) {
            None
        } else {
            match self.observed_index_tree() {
                Ok(tree) => Some(tree),
                Err(error) => return failure(error),
            }
        };

        // The staged delta, bounded like every path list the interfaces
        // spell out (SPEC 7.6).
        let staged = match staged_paths_against_head(self.git, &index) {
            Ok(staged) => staged,
            Err(error) => return failure(OperationError::from(error)),
        };
        let staged_paths = StagedPaths {
            total: staged.len(),
            paths: list_paths(&staged),
        };

        // The working-scope fingerprint (SPEC 8.6). One entry that cannot
        // honestly join a digest — a symbolic link a writer put at a scoped
        // path, for example — makes the fingerprint unknown instead of
        // narrowed, and the recovery argv below refuses to build on it.
        let scoped_working_digest = match working_content_digest(self.git, &resolved) {
            Ok(digest) => Some(digest),
            Err(IdentityError::UnsupportedEntry { .. }) => None,
            Err(error) => return failure(OperationError::from(error)),
        };

        // The receipt (SPEC 8.7): whether the history of this scope already
        // holds the durable record of one completed request.
        let receipt = match &head {
            Some(candidate) => {
                let mut guard = self.history_walk();
                let walk = guard.as_mut().expect("the constructor fills the walk");
                match walk.find_latest_receipt(candidate, &resolved.digest.id()) {
                    Ok(presence) => presence,
                    Err(error) => {
                        return failure(
                            OperationError {
                                code: error.public_code(),
                                message: format!(
                                    "the receipt scan did not complete, so the report cannot \
                                     show a receipt absent: {error}"
                                ),
                                retryable: error.retryable(),
                                recovery_argv: Vec::new(),
                            }
                            .recovery(&["margin", "status", "--json"]),
                        );
                    }
                }
            }
            // An unborn branch holds no history at all, so it holds no
            // receipt either; the empty domain establishes that.
            None => ReceiptPresence::Absent,
        };

        // The recovery argv of SPEC 9.7, assembled from the observations
        // above only: argv arrays, never one shell script, and never text
        // read out of the documents (SPEC 7.5).
        let recovery_argv = recovery_argv_of(
            &head,
            &baseline,
            index_tree.as_deref(),
            &staged_paths,
            scoped_working_digest.as_ref(),
        );

        if let Some(trace) = self.trace.as_ref() {
            trace.inspected_recovery(head.as_deref());
        }
        RecoveryResult::success(
            RecoveryInspection {
                object_format,
                head,
                baseline,
                index_tree,
                scoped_working_digest,
                staged_paths,
                receipt,
                recovery_argv,
            },
            Effects::unchanged(),
        )
    }

    /// The context every mutation shares before it writes (SPEC 9.1): the
    /// workspace probe, the one scope resolver, the one mutation policy
    /// guard, and the adopted branch of the local settings, in guard order.
    ///
    /// A refusal reports the primary blocker code with a complete bounded
    /// diagnosis. Because a refusal writes nothing, the caller reports
    /// unchanged effects rather than intended ones.
    ///
    /// The adopted branch runs after the repository policy, so a detached
    /// `HEAD` keeps its own refusal. A `HEAD` on another branch than the
    /// configured one — even at the same commit — refuses `BRANCH_CHANGED`
    /// before any index write: Margin never switches or re-adopts a branch
    /// on its own, and one explicit `init` remains the path that adopts
    /// another branch (SPEC 8.2, INV-08). Without a configured branch there
    /// is nothing to compare, and the context keeps its prior verdict.
    ///
    /// Operations that hold the mutation lock split this context at the
    /// lock instead — step 1, the lock, then
    /// [`Operations::enforce_mutation_guards`] — so the same guards run in
    /// the same order under the serialization the lock provides.
    /// The context one query shares: the workspace probe and the one scope
    /// resolver. Queries observe blockers as data; they never enforce
    /// mutation policy as a refusal.
    fn query_context(&self, scope: Option<&ScopeInput>) -> Result<ResolvedScope, OperationError> {
        require_work_tree(self.git)?;
        let input = self.context_scope_input(scope)?;
        resolve_scope(self.git, &input).map_err(OperationError::from)
    }

    /// The effective local settings, read through the shared engine.
    fn effective_settings(&self) -> Result<LocalSettings, OperationError> {
        read_local_settings(self.git).map_err(OperationError::from)
    }

    /// The scope input one shared context uses: the explicit input when the
    /// interface supplies one, otherwise the stored scope of the effective
    /// settings (SPEC 8.2). A stored scope re-enters through the one
    /// resolver, so no interface resolves configuration on its own.
    fn context_scope_input(
        &self,
        scope: Option<&ScopeInput>,
    ) -> Result<ScopeInput, OperationError> {
        let settings = self.effective_settings()?;
        scope_input_of(scope, &settings).ok_or_else(not_initialized)
    }
}

/// The scope input one operation adopts: the explicit input when given,
/// otherwise the stored scope of `settings`. `None` means no scope is
/// configured at all.
fn scope_input_of(scope: Option<&ScopeInput>, settings: &LocalSettings) -> Option<ScopeInput> {
    scope.cloned().or_else(|| {
        settings
            .scope
            .as_ref()
            .map(|descriptor| descriptor.to_scope_input())
    })
}

/// Says whether one policy blocker set holds a storage transformation of
/// the `identity-v1` profile (SPEC 8.8). Under a transformation the raw
/// working bytes and the stored blobs are not universally interchangeable,
/// so a preservation comparison against a candidate cannot be honest.
fn storage_transformed(blockers: &[PolicyBlocker]) -> bool {
    blockers.iter().any(|blocker| {
        matches!(
            blocker,
            PolicyBlocker::AutoCrlfConversion { .. }
                | PolicyBlocker::TextNormalization { .. }
                | PolicyBlocker::EolNormalization { .. }
                | PolicyBlocker::ContentFilter { .. }
                | PolicyBlocker::WorkingTreeEncoding { .. }
                | PolicyBlocker::IdentExpansion { .. }
                | PolicyBlocker::ByteTransformation { .. }
        )
    })
}

/// The adopted-branch mismatch of one workspace (SPEC 8.2, 9.1 step 4):
/// the branch `HEAD` is attached to, compared with the branch the local
/// settings adopted.
///
/// `None` means no mismatch: no branch is configured, `HEAD` is detached
/// — the repository policy's own refusal, not a mismatch — or the attached
/// branch is the adopted one. The comparison is by branch name, because
/// two branches at the same commit are still different places to append
/// history to (INV-08).
fn adopted_branch_mismatch(
    settings: &LocalSettings,
    branch_ref: Option<&str>,
) -> Option<PolicyBlocker> {
    let adopted = settings.branch.as_deref()?;
    let current = branch_ref?;
    let current = current.strip_prefix("refs/heads/").unwrap_or(current);
    if current == adopted {
        None
    } else {
        Some(PolicyBlocker::BranchMismatch {
            adopted: adopted.to_string(),
            current: current.to_string(),
        })
    }
}

/// The operation dimension of one inspection (SPEC 4.2): staged changes,
/// unresolved conflicts, and in-progress Git operations are repository
/// state a mutation cannot proceed through, so the workspace asks for
/// inspection before the next one. Every other observation, including
/// detached `HEAD` and unsupported layouts, stays a reported blocker while
/// the operation itself is idle.
fn operation_state_of(blockers: &[PolicyBlocker]) -> OperationState {
    let recovery = blockers.iter().any(|blocker| {
        matches!(
            blocker,
            PolicyBlocker::StagedChanges { .. }
                | PolicyBlocker::UnresolvedConflict { .. }
                | PolicyBlocker::ActiveMerge
                | PolicyBlocker::ActiveRebase
                | PolicyBlocker::ActiveCherryPick
                | PolicyBlocker::ActiveRevert
                | PolicyBlocker::ActiveBisect
                | PolicyBlocker::ActiveSequencer
        )
    });
    if recovery {
        OperationState::RecoveryRequired
    } else {
        OperationState::Idle
    }
}

/// Assembles the bounded recovery argv suggestions of one recovery
/// inspection (SPEC 9.7) from the observations the report itself carries.
///
/// Both documented recovery paths appear exactly when the report can name
/// what they compare against: the retained plan needs a candidate and a
/// valid anchor to return to, and the guarded staged capture needs the
/// observed head, index tree, and working fingerprint — the values
/// `--expect-head`, `--expect-index`, and `--expect-working` recheck at
/// capture time. Staged content the report cannot pin down that completely
/// gets the ordinary Git inspection guidance of SPEC 9.7's closing
/// paragraph instead of a suggestion assembled on a guess. Every suggestion
/// is one argv array (SPEC 7.5) — never a shell script, never text read out
/// of the documents — and no suggestion mutates anything by itself.
fn recovery_argv_of(
    head: &Option<String>,
    baseline: &BaselineStatus,
    index_tree: Option<&str>,
    staged: &StagedPaths,
    working: Option<&Fingerprint>,
) -> Vec<Vec<String>> {
    if staged.total == 0 {
        // Nothing is staged, so no interrupted application waits: the report
        // itself is the answer and no recovery command is suggested.
        return Vec::new();
    }
    let mut argv = Vec::new();
    if let (Some(head), Some(tree), Some(working)) = (head.as_deref(), index_tree, working) {
        argv.push(vec![
            "margin".to_string(),
            "checkpoint".to_string(),
            "--include-staged".to_string(),
            "--expect-head".to_string(),
            head.to_string(),
            "--expect-index".to_string(),
            tree.to_string(),
            "--expect-working".to_string(),
            working.id(),
            "--json".to_string(),
        ]);
        if matches!(baseline, BaselineStatus::Anchored(_)) {
            argv.push(vec![
                "margin".to_string(),
                "review".to_string(),
                "--resume".to_string(),
                "--input".to_string(),
                "plan.json".to_string(),
                "--json".to_string(),
            ]);
        }
    }
    if argv.is_empty() {
        argv.push(vec![
            "git".to_string(),
            "diff".to_string(),
            "--cached".to_string(),
            "--name-status".to_string(),
        ]);
    }
    argv
}

/// Computes the eligible scoped paths whose working content is not in
/// `candidate` (SPEC 4.2 preservation, 6.3 counts).
///
/// The query is [`scoped_working_changes`] with the presentation filter
/// applied, so what `status` reports uncaptured is exactly what one
/// checkpoint captures (SPEC 9.2).
fn uncaptured_paths_of(
    git: &Git,
    scope: &ResolvedScope,
    candidate: &str,
    filter: &[String],
) -> Result<UncapturedPaths, PolicyError> {
    let changes = scoped_working_changes(git, scope, candidate)?;
    Ok(filtered_uncaptured(changes.into_keys().collect(), filter))
}

/// One working entry of a changed scoped path (SPEC 8.6): the storage mode
/// and the raw-byte blob identity of the file the working tree holds.
#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkingEntry {
    /// The storage mode Git records for the working file.
    mode: EntryMode,
    /// The blob object ID of the raw working bytes, hashed by Git itself
    /// under the identity profile.
    oid: String,
}

/// The exact eligible scoped changes against one candidate (SPEC 4.2
/// preservation, 9.2 capture): every scoped path whose presence, storage
/// mode, or raw bytes differ from the candidate's tree entry, mapped to
/// the working entry it holds. A `None` value is a deletion.
///
/// The inexpensive path asks Git which paths differ — the index against the
/// candidate through `diff-index`, the working tree against the index
/// through `diff-files` — so the index stat cache answers for every
/// unchanged file without reading its content (SPEC 6.3). Untracked,
/// deleted, and flag-hidden scoped paths join the flagged set directly:
/// `assume-unchanged` and `skip-worktree` suppress a path in both diff
/// calls, so the flags themselves flag the path (SPEC 3.1). Each flagged
/// path is then compared exactly against the candidate's tree entry,
/// because a working file can match the candidate while it differs from
/// the index, and vice versa. A path Git did not flag matches the
/// candidate: its index entry equals the candidate's entry and its working
/// bytes equal the index entry. The raw bytes of every flagged present
/// path are hashed by batched processes, never one process per document
/// (SPEC 17.5).
fn scoped_working_changes(
    git: &Git,
    scope: &ResolvedScope,
    candidate: &str,
) -> Result<BTreeMap<OsString, Option<WorkingEntry>>, PolicyError> {
    // A path hidden by assume-unchanged or skip-worktree never appears in
    // either diff list, whatever its working content holds, so those paths
    // join the flagged set directly and the comparison below reads their
    // bytes against the candidate entry (SPEC 3.1).
    let hidden: BTreeSet<OsString> = git
        .hidden_index_paths()?
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    let flagged: BTreeSet<OsString> = git
        .staged_against(candidate)?
        .into_iter()
        .chain(git.changed_against_index()?)
        .chain(hidden)
        .collect();
    let candidate_entries: BTreeMap<OsString, (String, String)> = git
        .ls_tree(candidate)?
        .into_iter()
        .filter(|entry| {
            scope
                .descriptor
                .contains_path(entry.path.as_encoded_bytes())
        })
        .map(|entry| (entry.path, (entry.mode, entry.oid)))
        .collect();
    let repo_root = git.toplevel()?;

    // The working entry of every flagged scoped path: absence for a deleted
    // path or one that vanished after the scope resolved, the filesystem
    // metadata otherwise.
    let mut flagged_scoped: Vec<(OsString, Option<fs::Metadata>)> = Vec::new();
    for (path, state) in &scope.paths {
        let flagged = flagged.contains(path)
            || matches!(
                state,
                ScopedPathState::Untracked | ScopedPathState::TrackedDeleted
            );
        if !flagged {
            continue;
        }
        let metadata = if *state == ScopedPathState::TrackedDeleted {
            None
        } else {
            match fs::symlink_metadata(repo_root.join(path)) {
                Ok(metadata) => Some(metadata),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(PolicyError::Io(error)),
            }
        };
        flagged_scoped.push((path.clone(), metadata));
    }

    // One batched call hashes the raw bytes of every present flagged path.
    let present: Vec<OsString> = flagged_scoped
        .iter()
        .filter(|(_, metadata)| metadata.is_some())
        .map(|(path, _)| path.clone())
        .collect();
    let blobs: BTreeMap<OsString, String> = present
        .iter()
        .cloned()
        .zip(git.hash_object_paths(&present)?)
        .collect();

    let mut changes = BTreeMap::new();
    for (path, metadata) in flagged_scoped {
        if !working_differs_from_candidate(&path, metadata.as_ref(), &blobs, &candidate_entries) {
            continue;
        }
        let entry = metadata.map(|metadata| WorkingEntry {
            mode: working_mode(&metadata),
            oid: blobs
                .get(&path)
                .expect("one batch hashed every present flagged path")
                .clone(),
        });
        changes.insert(path, entry);
    }
    Ok(changes)
}

/// The working entry of every scoped path, absence included (SPEC 9.1
/// step 7): the baseline against which a failed capture's working-tree
/// effects are compared. Present files are hashed by Git itself in batched
/// processes, so one first use can afford the pass over the whole scope.
fn scoped_working_state(
    git: &Git,
    scope: &ResolvedScope,
) -> Result<BTreeMap<OsString, Option<WorkingEntry>>, PolicyError> {
    let repo_root = git.toplevel()?;
    let mut present: Vec<(OsString, fs::Metadata)> = Vec::new();
    let mut state = BTreeMap::new();
    for path in scope.paths.keys() {
        match fs::symlink_metadata(repo_root.join(path)) {
            Ok(metadata) => present.push((path.clone(), metadata)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                state.insert(path.clone(), None);
            }
            Err(error) => return Err(PolicyError::Io(error)),
        }
    }
    let paths: Vec<OsString> = present.iter().map(|(path, _)| path.clone()).collect();
    let blobs: BTreeMap<OsString, String> = paths
        .iter()
        .cloned()
        .zip(git.hash_object_paths(&paths)?)
        .collect();
    for (path, metadata) in present {
        let entry = WorkingEntry {
            mode: working_mode(&metadata),
            oid: blobs
                .get(&path)
                .expect("one batch hashed every present scoped path")
                .clone(),
        };
        state.insert(path, Some(entry));
    }
    Ok(state)
}

/// The scoped working entries one intended result tree defines (SPEC 9.4
/// step 3): the expected R of a restoring application, read from the
/// immutable entries of T alone. Absence is an expected entry, so a
/// restored deletion must leave the working tree without the file, and a
/// kept path keeps the candidate's own entry because T differs from C
/// exactly on the restored paths.
fn intended_working_state(
    git: &Git,
    scope: &ResolvedScope,
    tree: &str,
) -> Result<BTreeMap<OsString, Option<WorkingEntry>>, PolicyError> {
    let stored: BTreeMap<OsString, (String, String)> = git
        .ls_tree(tree)?
        .into_iter()
        .filter(|entry| {
            scope
                .descriptor
                .contains_path(entry.path.as_encoded_bytes())
        })
        .map(|entry| (entry.path, (entry.mode, entry.oid)))
        .collect();
    let mut expected = BTreeMap::new();
    for path in scope.paths.keys() {
        let entry = stored.get(path).and_then(|(mode, oid)| {
            Some(WorkingEntry {
                mode: EntryMode::parse(mode)?,
                oid: oid.clone(),
            })
        });
        expected.insert(path.clone(), entry);
    }
    Ok(expected)
}

/// The working-entry baseline of one draft checkpoint (SPEC 9.1 step 7):
/// the enumerated changes already carry the entries the staging will read,
/// and every other scoped path still holds its candidate entry. One tree
/// listing supplies those, so the hot capture path hashes no file twice.
fn working_baseline(
    git: &Git,
    scope: &ResolvedScope,
    candidate: &str,
    changes: &BTreeMap<OsString, Option<WorkingEntry>>,
) -> Result<BTreeMap<OsString, Option<WorkingEntry>>, PolicyError> {
    let stored: BTreeMap<OsString, (String, String)> = git
        .ls_tree(candidate)?
        .into_iter()
        .filter(|entry| {
            scope
                .descriptor
                .contains_path(entry.path.as_encoded_bytes())
        })
        .map(|entry| (entry.path, (entry.mode, entry.oid)))
        .collect();
    let mut baseline = changes.clone();
    for path in scope.paths.keys() {
        if baseline.contains_key(path) {
            continue;
        }
        let entry = stored.get(path).and_then(|(mode, oid)| {
            Some(WorkingEntry {
                mode: EntryMode::parse(mode)?,
                oid: oid.clone(),
            })
        });
        baseline.insert(path.clone(), entry);
    }
    Ok(baseline)
}

/// The present working entries of one scoped state, keyed by path: absence
/// (`None`) and a path missing from the state map are the same working
/// state, because a committed deletion leaves the freshly resolved scope
/// without its path while the baseline holds that path as absent. Two
/// states describe the same working tree exactly when their present entries
/// match.
fn present_working_entries(
    state: &BTreeMap<OsString, Option<WorkingEntry>>,
) -> BTreeMap<&OsString, &WorkingEntry> {
    state
        .iter()
        .filter_map(|(path, entry)| entry.as_ref().map(|entry| (path, entry)))
        .collect()
}

/// The uncaptured paths of a workspace with no candidate (SPEC 4.2): no
/// commit holds the scoped content, so every scoped path that has working
/// bytes is uncaptured. A deleted path carries no content to preserve.
fn uncaptured_without_candidate(scope: &ResolvedScope, filter: &[String]) -> UncapturedPaths {
    let uncaptured: Vec<OsString> = scope
        .paths
        .iter()
        .filter(|(_, state)| **state != ScopedPathState::TrackedDeleted)
        .map(|(path, _)| path.clone())
        .collect();
    filtered_uncaptured(uncaptured, filter)
}

/// Applies the presentation filter to one uncaptured path list and keeps
/// the whole-scope total beside it (SPEC 6.3, 7.2).
fn filtered_uncaptured(paths: Vec<OsString>, filter: &[String]) -> UncapturedPaths {
    let total = paths.len();
    let paths = paths
        .into_iter()
        .filter(|path| passes_filter(path, filter))
        .collect();
    UncapturedPaths { total, paths }
}

/// Says whether one repository-relative path passes a presentation filter
/// (SPEC 6.3): each filter is a literal path or directory prefix, `.` and
/// an empty filter list pass every path. A prefix matches only on path
/// component boundaries, so `notes` matches `notes/a.md` and never
/// `notes-old/b.md`.
fn passes_filter(path: &OsStr, filter: &[String]) -> bool {
    if filter.is_empty() {
        return true;
    }
    let bytes = path.as_encoded_bytes();
    filter.iter().any(|root| {
        let root = root.trim_end_matches('/');
        if root == "." || root.is_empty() {
            return true;
        }
        let root = root.as_bytes();
        bytes == root || bytes.starts_with(root) && bytes.get(root.len()) == Some(&b'/')
    })
}

/// Says whether one scoped path's working entry differs from the
/// candidate's tree entry: presence, storage mode, and raw bytes, hashed
/// by Git itself under the identity profile (SPEC 8.6). A `None` metadata
/// means the working tree holds absence; `blobs` carries the raw-byte
/// object IDs of the present flagged paths.
fn working_differs_from_candidate(
    path: &OsStr,
    metadata: Option<&fs::Metadata>,
    blobs: &BTreeMap<OsString, String>,
    candidate_entries: &BTreeMap<OsString, (String, String)>,
) -> bool {
    let Some(stored) = candidate_entries.get(path) else {
        // Working content no candidate entry holds.
        return metadata.is_some();
    };
    let Some(metadata) = metadata else {
        // Absence matches only an absent candidate entry.
        return true;
    };
    if stored.0 != working_mode(metadata).as_str() {
        return true;
    }
    blobs.get(path).map(|oid| oid != &stored.1).unwrap_or(true)
}

/// Computes and proves the intended tree of one draft checkpoint through
/// one disposable Git index (SPEC 9.1 step 5, 9.2): the index starts from
/// the candidate's tree, then stages exactly the changed paths with the
/// working content they hold.
///
/// The proof runs entirely in the disposable index: the staged delta
/// against the candidate names exactly the enumerated changes — no
/// out-of-scope entry and no enumerated change silently lost — and every
/// staged entry equals the mode and the raw bytes the enumeration
/// observed, which is Git's own staging pipeline testifying that it would
/// store the working content unchanged. The real index, the refs, and the
/// working files stay untouched; only disposable Git objects and one
/// temporary index file exist, and the temporary index never outlives the
/// call, whatever the result.
fn intended_draft_tree(
    git: &Git,
    candidate: &str,
    changes: &BTreeMap<OsString, Option<WorkingEntry>>,
) -> Result<String, DraftProofError> {
    let temp_index = reserve_temp_index().map_err(PolicyError::from)?;
    let proof = prove_intended_tree(git, &temp_index, candidate, changes);
    // The disposable index never outlives the proof, on success or failure.
    let _ = fs::remove_file(&temp_index);
    proof
}

/// One run of the intended-tree proof inside the disposable index at
/// `temp_index` (SPEC 9.1 step 5, 9.2).
fn prove_intended_tree(
    git: &Git,
    temp_index: &Path,
    candidate: &str,
    changes: &BTreeMap<OsString, Option<WorkingEntry>>,
) -> Result<String, DraftProofError> {
    let mut probe = git.clone();
    probe.set_env_override("GIT_INDEX_FILE", temp_index);
    probe.read_tree(candidate)?;
    let paths: Vec<OsString> = changes.keys().cloned().collect();
    probe.stage_paths(&paths)?;

    // The staged delta names exactly the enumerated changes: no
    // out-of-scope entry, and no enumerated change silently lost
    // (SPEC 9.2).
    let delta: BTreeSet<OsString> = probe.staged_against(candidate)?.into_iter().collect();
    let unexpected = delta.iter().find(|path| !changes.contains_key(*path));
    let missing = changes.keys().find(|path| !delta.contains(*path));
    if let Some(path) = unexpected.or(missing) {
        return Err(DraftProofError::MovedWorkingTree { path: path.clone() });
    }

    // Every staged entry equals the observed mode and raw bytes (SPEC 8.8):
    // Git's staging pipeline itself testifies that it would store the
    // working content unchanged.
    let staged: BTreeMap<OsString, (String, String)> = probe
        .ls_files_stage()?
        .into_iter()
        .filter(|entry| entry.stage == 0)
        .map(|entry| (entry.path, (entry.mode, entry.oid)))
        .collect();
    for (path, entry) in changes {
        match (entry, staged.get(path)) {
            (Some(working), Some(stored)) => {
                if stored.0 != working.mode.as_str() || stored.1 != working.oid {
                    return Err(DraftProofError::TransformedEntry { path: path.clone() });
                }
            }
            (None, None) => {}
            (Some(_), None) | (None, Some(_)) => {
                return Err(DraftProofError::MovedWorkingTree { path: path.clone() });
            }
        }
    }
    Ok(probe.write_tree()?)
}

/// Failure of one draft's intended-tree proof (SPEC 9.1 step 5, 9.2).
///
/// Every case is found before the first real write, so the refusal carries
/// unchanged effects.
#[derive(Debug)]
enum DraftProofError {
    /// A Git process or the filesystem failed before the proof could
    /// decide.
    Inspection(PolicyError),
    /// The disposable staging does not name exactly the enumerated changes,
    /// so the working tree moved while the proof ran.
    MovedWorkingTree {
        /// The path whose staging no longer matches the enumeration.
        path: OsString,
    },
    /// Staging through Git would store an entry that differs from the
    /// enumerated mode and raw bytes (SPEC 8.8).
    TransformedEntry {
        /// The path whose staging transforms its content.
        path: OsString,
    },
}

impl DraftProofError {
    /// The SPEC 7.5 error code of this refusal.
    fn public_code(&self) -> &'static str {
        match self {
            DraftProofError::Inspection(error) => error.public_code(),
            DraftProofError::MovedWorkingTree { .. } => "DIRTY_SCOPE",
            DraftProofError::TransformedEntry { .. } => "UNSUPPORTED_REPOSITORY",
        }
    }
}

impl fmt::Display for DraftProofError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn quoted(path: &OsStr) -> String {
            format!("{:?}", path.to_string_lossy())
        }
        match self {
            DraftProofError::Inspection(error) => write!(formatter, "{error}"),
            DraftProofError::MovedWorkingTree { path } => write!(
                formatter,
                "the scoped working content changed while the checkpoint was prepared: {}",
                quoted(path)
            ),
            DraftProofError::TransformedEntry { path } => write!(
                formatter,
                "staging {} would store different bytes or modes than the working file holds",
                quoted(path)
            ),
        }
    }
}

impl std::error::Error for DraftProofError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DraftProofError::Inspection(error) => Some(error),
            _ => None,
        }
    }
}

impl From<PolicyError> for DraftProofError {
    fn from(error: PolicyError) -> Self {
        DraftProofError::Inspection(error)
    }
}

impl From<GitError> for DraftProofError {
    fn from(error: GitError) -> Self {
        DraftProofError::Inspection(PolicyError::Git(error))
    }
}

impl From<DraftProofError> for OperationError {
    fn from(error: DraftProofError) -> Self {
        OperationError::new(error.public_code(), error.to_string())
    }
}

/// One bounded comma-separated path listing for a diagnostic message
/// (SPEC 7.6): at most [`MAX_LISTED_PATHS`] paths, then a remaining count.
fn listed_paths(paths: &[OsString]) -> String {
    let mut listed: Vec<String> = list_paths(paths)
        .iter()
        .map(|path| format!("{:?}", path.to_string_lossy()))
        .collect();
    if paths.len() > listed.len() {
        let remaining = paths.len() - listed.len();
        listed.push(format!("and {remaining} more"));
    }
    listed.join(", ")
}

/// The scoped paths whose stored entry differs between two commits
/// (SPEC 3.2 rule 4): additions, deletions, mode changes, and content
/// changes over the union of both scoped path sets, without rename
/// detection. Both revisions must be validated canonical commit object
/// IDs from the adapter.
fn scoped_changed_paths(
    git: &Git,
    descriptor: &ScopeDescriptor,
    baseline: &str,
    candidate: &str,
) -> Result<Vec<OsString>, PolicyError> {
    Ok(git
        .changed_between_commits(baseline, candidate)?
        .into_iter()
        .filter(|path| descriptor.contains_path(path.as_encoded_bytes()))
        .collect())
}

/// Probes that `git` addresses one local working tree (SPEC 3.1).
///
/// The probes fail with a nonzero exit exactly when no repository encloses
/// the directory, which is `NOT_INITIALIZED`: `margin init` can create a
/// repository there (SPEC 5.1).
fn require_work_tree(git: &Git) -> Result<(), OperationError> {
    if git.is_bare_repository().map_err(outside_any_repository)? {
        return Err(OperationError::new(
            "UNSUPPORTED_REPOSITORY",
            PolicyBlocker::BareRepository.to_string(),
        ));
    }
    if !git.is_inside_work_tree().map_err(outside_any_repository)? {
        return Err(OperationError::new(
            "UNSUPPORTED_REPOSITORY",
            PolicyBlocker::NotInWorkTree.to_string(),
        ));
    }
    Ok(())
}

/// Maps one failed repository discovery probe to `NOT_INITIALIZED`.
fn outside_any_repository(error: GitError) -> OperationError {
    match error {
        GitError::Exit { .. } => OperationError::new(
            "NOT_INITIALIZED",
            format!("this directory lies inside no Git repository: {error}"),
        )
        .recovery(&["margin", "init", "--json"]),
        other => OperationError::from(other),
    }
}

/// The typed failure for an operation that needs a configured writing
/// scope and has none (SPEC 7.5 `NOT_INITIALIZED`).
fn not_initialized() -> OperationError {
    OperationError::new(
        "NOT_INITIALIZED",
        "no writing scope is configured for this repository",
    )
    .recovery(&["margin", "init", "--json"])
}

/// The typed failure of initialization outside any repository without
/// `--create-repo` (SPEC 5.1).
fn no_repository() -> OperationError {
    OperationError::new(
        "NOT_INITIALIZED",
        "this directory lies inside no Git repository",
    )
    .recovery(&["margin", "init", "--create-repo", "--scope", ".", "--json"])
}

/// The typed failure of one expected-branch assertion (SPEC 5.1): Margin
/// adopts the current branch and never switches it automatically.
fn branch_mismatch(expected: &str, adopted: &str) -> OperationError {
    OperationError::new(
        "BRANCH_CHANGED",
        format!("the adopted branch is {adopted}, but --branch expects {expected}"),
    )
    .recovery(&["margin", "status", "--json"])
}

/// The typed refusal of one mutation outside the adopted branch (SPEC 9.1
/// step 4, 8.2): Margin never switches or re-adopts a branch on its own, so
/// the writer adopts the current branch through one explicit `init`.
fn adopted_branch_refusal(blocker: &PolicyBlocker) -> OperationError {
    OperationError::new(blocker.public_code(), blocker.to_string())
        .recovery(&["margin", "init", "--json"])
}

/// The typed `BUSY` refusal of the shared mutation lock (SPEC 9.1 step 2):
/// another Margin operation owns this repository, so nothing was changed
/// and the identical retry is both safe and useful once the owner finishes
/// (SPEC 7.5).
fn busy_refusal() -> OperationError {
    OperationError::new(
        "BUSY",
        "another Margin operation is writing to this repository; nothing was changed",
    )
    .retryable()
    .recovery(&["margin", "status", "--json"])
}

/// The typed failure of the lock file itself: it could not be opened or
/// locked, so no serialization claim can be made and nothing was written.
fn lock_file_failure(path: &Path, error: io::Error) -> OperationError {
    OperationError::new(
        "IO_FAILED",
        format!("cannot lock {}: {error}", path.display()),
    )
}

/// The typed refusal of a capture whose candidate moved (SPEC 9.1 step 6,
/// 16.5 stale state): `HEAD` changed between the guard inspection and the
/// first real mutation, so the proved intended tree describes a repository
/// that no longer exists and nothing was staged.
fn stale_state_refusal(detail: impl Into<String>) -> OperationError {
    OperationError::new("STALE_STATE", detail).recovery(&["margin", "status", "--json"])
}

/// The typed refusal of a capture whose branch moved while it was prepared
/// (SPEC 9.1 step 6, 8.2, INV-08): Margin never appends history to a branch
/// it did not adopt, and one explicit `init` adopts the new one.
fn branch_moved_refusal(prepared: &Option<String>, observed: &Option<String>) -> OperationError {
    let named = |branch: &Option<String>| {
        branch
            .as_deref()
            .and_then(|r#ref| r#ref.strip_prefix("refs/heads/"))
            .unwrap_or("a detached HEAD")
            .to_string()
    };
    OperationError::new(
        "BRANCH_CHANGED",
        format!(
            "the branch changed from {} to {} while the capture was prepared; nothing was staged",
            named(prepared),
            named(observed),
        ),
    )
    .recovery(&["margin", "init", "--json"])
}

/// The typed `DIRTY_SCOPE` refusal of a capture whose scoped paths moved
/// between the guard inspection and the first real mutation (SPEC 9.1
/// step 6, 16.5 stale state): the same condition the intended-tree proof
/// reports, found before the real index is staged instead, so nothing was
/// written. `what` names the dimension that moved.
fn scope_paths_moved<V: PartialEq>(
    what: &str,
    fresh: &BTreeMap<OsString, V>,
    prepared: &BTreeMap<OsString, V>,
) -> OperationError {
    let mut moved: BTreeSet<OsString> = BTreeSet::new();
    for path in fresh.keys().chain(prepared.keys()) {
        if fresh.get(path) != prepared.get(path) {
            moved.insert(path.clone());
        }
    }
    let listed: Vec<OsString> = moved.into_iter().collect();
    OperationError::new(
        "DIRTY_SCOPE",
        format!(
            "the {what} changed while the capture was prepared: {}; nothing was staged",
            listed_paths(&listed)
        ),
    )
    .recovery(&["margin", "status", "--json"])
}

/// The typed `DIRTY_SCOPE` refusal of a review whose scoped working content
/// differs from the candidate (SPEC 9.3): the decision would commit against
/// bytes nobody pinned, so the newer content belongs to the next checkpoint
/// instead of this review. Nothing was written.
fn dirty_scope_refusal(
    changes: &BTreeMap<OsString, Option<WorkingEntry>>,
    operation: &str,
) -> OperationError {
    let listed: Vec<OsString> = changes.keys().cloned().collect();
    OperationError::new(
        "DIRTY_SCOPE",
        format!(
            "{} scoped path(s) hold uncaptured differences against the candidate; checkpoint \
             them before the {operation}: {}",
            changes.len(),
            listed_paths(&listed)
        ),
    )
    .recovery(&["margin", "checkpoint", "--json"])
}

/// The typed failure of a restoring review whose real application stopped
/// short of its proved preview (SPEC 9.4 step 2, 9.6): applied units, staged
/// entries, or restored working bytes may remain, so the refusal reports
/// the observed state the caller inspected and never claims a rollback,
/// replays the patch, or grants approval to an unmatched result.
///
/// `OUTCOME_UNKNOWN` replaces the code when the process outcome itself is
/// unknown, such as an apply that exceeded its time budget; every other
/// failure is the `PARTIAL_MUTATION` of SPEC 7.5.
fn partial_application_failure(code: &'static str, detail: impl Into<String>) -> OperationError {
    OperationError::new(code, detail).recovery(&["margin", "status", "--json"])
}

/// The typed failure of a missing author and committer identity (SPEC 5.1):
/// Margin never creates an identity on the writer's behalf.
fn missing_identity() -> OperationError {
    OperationError::new(
        "MISSING_IDENTITY",
        "Git holds no author and committer identity; set user.name and user.email in your Git configuration",
    )
    .recovery(&["git", "config", "--help"])
}

/// One bounded message for a scope-blocker refusal, in the same shape as
/// the policy-blocker message.
fn scope_blockers_message(blockers: &[ScopeBlocker]) -> String {
    let mut listed: Vec<String> = blockers.iter().map(|blocker| blocker.to_string()).collect();
    if listed.len() > MAX_LISTED_PATHS {
        let remaining = listed.len() - MAX_LISTED_PATHS;
        listed.truncate(MAX_LISTED_PATHS);
        listed.push(format!("and {remaining} more"));
    }
    listed.join("; ")
}

/// The typed refusal of an adoption over actionable scope blockers: the
/// blocked paths stay outside the scoped set instead of being reviewed
/// silently (SPEC 3.2 rule 5).
fn scope_blockers_refusal(blockers: &[ScopeBlocker]) -> OperationError {
    OperationError::new("UNSUPPORTED_REPOSITORY", scope_blockers_message(blockers))
}

/// One bounded message for a policy refusal: every blocker in guard
/// order, with at most [`MAX_LISTED_PATHS`] of them spelled out before a
/// remaining count.
fn blockers_message(blockers: &[PolicyBlocker]) -> String {
    let mut listed: Vec<String> = blockers.iter().map(|blocker| blocker.to_string()).collect();
    if listed.len() > MAX_LISTED_PATHS {
        let remaining = listed.len() - MAX_LISTED_PATHS;
        listed.truncate(MAX_LISTED_PATHS);
        listed.push(format!("and {remaining} more"));
    }
    listed.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assignment(path: &str, attribute: &str, value: &str) -> AttributeAssignment {
        AttributeAssignment {
            path: OsString::from(path),
            attribute: attribute.to_string(),
            value: value.to_string(),
        }
    }

    #[test]
    fn git_boolean_values_parse() {
        assert_eq!(config_bool("true"), Some(true));
        assert_eq!(config_bool("INPUT"), None);
        assert_eq!(config_bool("false"), Some(false));
        assert_eq!(config_bool(" off "), Some(false));
        assert_eq!(config_bool(""), Some(false));
        // An unparseable value is treated as a conversion in the caller, so
        // a broken configuration never passes the guard.
        assert_eq!(config_bool("banana"), None);
    }

    #[test]
    fn explicit_non_text_markings_stay_allowed() {
        assert_eq!(
            attribute_violation(&assignment("a.md", "text", "unspecified")),
            None
        );
        assert_eq!(
            attribute_violation(&assignment("a.md", "text", "unset")),
            None
        );
        assert_eq!(
            attribute_violation(&assignment("a.md", "working-tree-encoding", "unset")),
            None
        );
        // Unknown attributes are not part of the storage profile.
        assert_eq!(
            attribute_violation(&assignment("a.md", "diff", " set")),
            None
        );
    }

    #[test]
    fn transformation_attributes_report_their_value() {
        assert_eq!(
            attribute_violation(&assignment("a.md", "text", "auto")),
            Some(PolicyBlocker::TextNormalization {
                path: OsString::from("a.md"),
                value: "auto".to_string()
            })
        );
        assert_eq!(
            attribute_violation(&assignment("a.md", "eol", "crlf")),
            Some(PolicyBlocker::EolNormalization {
                path: OsString::from("a.md"),
                value: "crlf".to_string()
            })
        );
        assert_eq!(
            attribute_violation(&assignment("a.md", "filter", "lfs")),
            Some(PolicyBlocker::ContentFilter {
                path: OsString::from("a.md"),
                filter: "lfs".to_string()
            })
        );
        assert_eq!(
            attribute_violation(&assignment("a.md", "working-tree-encoding", "UTF-16")),
            Some(PolicyBlocker::WorkingTreeEncoding {
                path: OsString::from("a.md"),
                encoding: "UTF-16".to_string()
            })
        );
        assert_eq!(
            attribute_violation(&assignment("a.md", "ident", "set")),
            Some(PolicyBlocker::IdentExpansion {
                path: OsString::from("a.md")
            })
        );
    }

    #[test]
    fn blocker_codes_follow_the_exit_classes() {
        assert_eq!(
            PolicyBlocker::StagedChanges {
                count: 0,
                paths: Vec::new()
            }
            .public_code(),
            "STAGED_CHANGES"
        );
        assert_eq!(
            PolicyBlocker::ActiveMerge.public_code(),
            "UNSUPPORTED_REPOSITORY"
        );
        assert_eq!(
            PolicyBlocker::AutoCrlfConversion {
                value: "true".to_string()
            }
            .public_code(),
            "UNSUPPORTED_REPOSITORY"
        );
        assert_eq!(
            PolicyBlocker::BranchMismatch {
                adopted: "writing".to_string(),
                current: "other".to_string()
            }
            .public_code(),
            "BRANCH_CHANGED"
        );
    }

    #[test]
    fn the_adopted_branch_compares_names_not_contexts() {
        let mut settings = LocalSettings::first_use();
        settings.branch = Some("writing".to_string());
        // The adopted branch matches its full ref form.
        assert_eq!(
            adopted_branch_mismatch(&settings, Some("refs/heads/writing")),
            None
        );
        // A detached HEAD is the repository policy's own refusal, and no
        // configured branch leaves nothing to compare.
        assert_eq!(adopted_branch_mismatch(&settings, None), None);
        settings.branch = None;
        assert_eq!(
            adopted_branch_mismatch(&settings, Some("refs/heads/other")),
            None
        );
        // Another branch name mismatches even at the same commit, because
        // the names decide where history appends (INV-08).
        settings.branch = Some("writing".to_string());
        match adopted_branch_mismatch(&settings, Some("refs/heads/other")) {
            Some(PolicyBlocker::BranchMismatch { adopted, current }) => {
                assert_eq!(adopted, "writing");
                assert_eq!(current, "other");
            }
            other => panic!("another branch must mismatch: {other:?}"),
        }
    }

    #[test]
    fn effect_values_match_the_json_envelope() {
        assert_eq!(ObservedEffect::Unchanged.as_str(), "unchanged");
        assert_eq!(ObservedEffect::Changed.as_str(), "changed");
        assert_eq!(ObservedEffect::Unknown.as_str(), "unknown");
        assert_eq!(CommitEffect::None.as_str(), "none");
        assert_eq!(CommitEffect::Created.as_str(), "created");
        assert_eq!(CommitEffect::ExistingReceipt.as_str(), "existing_receipt");
        assert_eq!(CommitEffect::Unknown.as_str(), "unknown");
        assert_eq!(RepositoryEffect::Unchanged.as_str(), "unchanged");
        assert_eq!(RepositoryEffect::Created.as_str(), "created");
        assert_eq!(RepositoryEffect::Unknown.as_str(), "unknown");
    }

    #[test]
    fn lost_observations_never_narrow() {
        let mut effects = Effects::unknown();
        effects.observe_working_tree(ObservedEffect::Unchanged);
        effects.observe_index(ObservedEffect::Changed);
        effects.observe_commit(CommitEffect::Created, Some("0".repeat(40)));
        effects.observe_repository(RepositoryEffect::Created);
        effects.observe_configuration(ObservedEffect::Changed);
        effects.observe_integration(ObservedEffect::Unchanged);
        // A mutation timeout must not default to unchanged (SPEC 7.1), and
        // a lost observation cannot come back as a known one.
        assert_eq!(effects, Effects::unknown());
    }

    #[test]
    fn commit_observations_carry_their_object_id() {
        let mut effects = Effects::unchanged();
        let oid = format!("a{}", "0".repeat(39));
        effects.observe_commit(CommitEffect::Created, Some(oid.clone()));
        assert_eq!(effects.commit, CommitEffect::Created);
        assert_eq!(effects.commit_oid.as_deref(), Some(oid.as_str()));
        effects.observe_commit(CommitEffect::ExistingReceipt, Some(oid.clone()));
        assert_eq!(effects.commit, CommitEffect::ExistingReceipt);
        assert_eq!(effects.commit_oid.as_deref(), Some(oid.as_str()));
        // Losing the observation drops the identifier again: the outcome
        // names no commit it cannot vouch for.
        effects.observe_commit(CommitEffect::Unknown, Some(oid.clone()));
        assert_eq!(effects.commit, CommitEffect::Unknown);
        assert_eq!(effects.commit_oid, None);
    }

    #[test]
    fn recovery_argv_never_goes_beyond_its_observations() {
        let head = Some(format!("a{}", "1".repeat(39)));
        let tree = format!("b{}", "2".repeat(39));
        let working = Fingerprint {
            object_format: ObjectFormat::Sha1,
            oid: format!("c{}", "3".repeat(39)),
        };
        let staged = |total: usize| StagedPaths {
            total,
            paths: Vec::new(),
        };
        let unanchored = BaselineStatus::Unavailable(BaselineUnavailable::Exhausted { walked: 3 });
        let guidance = vec![vec![
            "git".to_string(),
            "diff".to_string(),
            "--cached".to_string(),
            "--name-status".to_string(),
        ]];

        // Nothing staged: no interrupted application waits, so no recovery
        // command is suggested (SPEC 9.7).
        assert!(
            recovery_argv_of(&head, &unanchored, Some(&tree), &staged(0), Some(&working))
                .is_empty()
        );

        // Complete observations without an anchor name the one recovery
        // path they support: the guarded staged capture, and no resume.
        assert_eq!(
            recovery_argv_of(&head, &unanchored, Some(&tree), &staged(2), Some(&working)),
            vec![vec![
                "margin".to_string(),
                "checkpoint".to_string(),
                "--include-staged".to_string(),
                "--expect-head".to_string(),
                head.clone().unwrap(),
                "--expect-index".to_string(),
                tree.clone(),
                "--expect-working".to_string(),
                working.id(),
                "--json".to_string(),
            ]]
        );

        // A missing observation — an unborn branch with no head to expect,
        // a conflicted index with no tree, a working set with no honest
        // fingerprint — leaves the ordinary Git inspection guidance of
        // SPEC 9.7 instead of a suggestion built on a guess.
        assert_eq!(
            recovery_argv_of(&None, &unanchored, Some(&tree), &staged(2), Some(&working)),
            guidance
        );
        assert_eq!(
            recovery_argv_of(&head, &unanchored, None, &staged(2), Some(&working)),
            guidance
        );
        assert_eq!(
            recovery_argv_of(&head, &unanchored, Some(&tree), &staged(2), None),
            guidance
        );
    }

    #[test]
    fn receipt_refusals_carry_their_public_codes() {
        // A reused request ID is conflicting context, exit class 3
        // (SPEC 7.5), and the refusal names both the request and the
        // receipt that already holds it.
        let conflict = request_id_conflict(
            "review-decision-42",
            &"1".repeat(40),
            &Some(format!("sha1:{}", "2".repeat(40))),
        );
        assert_eq!(conflict.code, "REQUEST_ID_CONFLICT");
        assert!(conflict.message.contains("review-decision-42"));
        assert!(conflict.message.contains(&"1".repeat(40)));
        // A receipt without a plan fingerprint still names its absence.
        let unrecorded = request_id_conflict("restore-01", &"3".repeat(40), &None);
        assert!(unrecorded.message.contains("none"));

        // An interrupted scan keeps the code of the process failure it
        // stopped at, because the scan itself adds no new failure class.
        let timeout = ReceiptScanError::Git(GitError::Timeout {
            argv: vec!["rev-list".to_string()],
            budget: std::time::Duration::from_millis(1),
        });
        assert_eq!(timeout.public_code(), "OUTCOME_UNKNOWN");
        assert!(!timeout.retryable());
        assert_eq!(
            ReceiptScanError::Io(io::Error::new(io::ErrorKind::NotFound, "gone")).public_code(),
            "IO_FAILED"
        );
    }

    #[test]
    fn outcomes_derive_success_from_the_error_slot() {
        let success = OperationOutcome::<u8>::success(1, Effects::unchanged());
        assert!(success.ok());
        assert!(success.error.is_none());
        assert_eq!(success.data, Some(1));

        let failure =
            OperationOutcome::observed_failure(not_initialized(), 2, Effects::unchanged());
        assert!(!failure.ok());
        let error = failure.error.as_ref().unwrap();
        assert_eq!(error.code, "NOT_INITIALIZED");
        assert!(!error.retryable);
        assert_eq!(
            error.recovery_argv,
            vec![
                "margin".to_string(),
                "init".to_string(),
                "--json".to_string()
            ]
        );
        // The observed state stays visible on failure (SPEC 7.1).
        assert_eq!(failure.data, Some(2));
        assert_eq!(failure.effects, Effects::unchanged());
    }

    #[test]
    fn process_failures_keep_their_retryability() {
        let spawn = OperationError::from(GitError::Io {
            context: "spawn git".to_string(),
            source: io::Error::other("gone"),
        });
        assert_eq!(spawn.code, "IO_FAILED");
        assert!(spawn.retryable);

        let timeout = OperationError::from(GitError::Timeout {
            argv: vec!["commit".to_string()],
            budget: std::time::Duration::from_millis(1),
        });
        assert_eq!(timeout.code, "OUTCOME_UNKNOWN");
        assert!(!timeout.retryable);
    }

    #[test]
    fn blocker_messages_stay_bounded() {
        let blockers: Vec<PolicyBlocker> =
            std::iter::repeat_n(PolicyBlocker::ActiveMerge, MAX_LISTED_PATHS + 5).collect();
        let message = blockers_message(&blockers);
        assert!(message.contains("a merge is in progress"));
        assert!(message.ends_with("and 5 more"));
    }

    #[test]
    fn presentation_filters_match_only_on_components() {
        let none: Vec<String> = Vec::new();
        let notes = vec!["notes".to_string()];
        for path in ["notes/a.md", "notes/deep/b.md", "notes"] {
            assert!(passes_filter(OsStr::new(path), &none), "{path} passes");
            assert!(
                passes_filter(OsStr::new(path), &notes),
                "{path} is in notes"
            );
            assert!(
                passes_filter(
                    OsStr::new(path),
                    &["other".to_string(), "notes/".to_string()]
                ),
                "{path} passes one of several filters"
            );
            assert!(
                passes_filter(OsStr::new(path), &[".".to_string()]),
                "{path} is in the whole scope"
            );
        }
        // A prefix that cuts a component in half matches nothing, and an
        // unrelated name stays unrelated.
        for path in ["notes-old/b.md", "drafts/b.md", "notes.md"] {
            assert!(
                !passes_filter(OsStr::new(path), &notes),
                "{path} is not in notes"
            );
        }
        assert!(passes_filter(
            OsStr::new("notes/a.md"),
            &["notes/a.md".to_string()]
        ));
    }

    #[test]
    fn the_operation_dimension_follows_recovery_evidence_only() {
        let idle: &[PolicyBlocker] = &[
            PolicyBlocker::DetachedHead,
            PolicyBlocker::LinkedWorktree,
            PolicyBlocker::ShallowClone,
            PolicyBlocker::SparseCheckout,
        ];
        assert_eq!(operation_state_of(idle), OperationState::Idle);
        for blocker in [
            PolicyBlocker::StagedChanges {
                count: 1,
                paths: vec![OsString::from("notes/a.md")],
            },
            PolicyBlocker::UnresolvedConflict {
                count: 1,
                paths: vec![OsString::from("notes/a.md")],
            },
            PolicyBlocker::ActiveMerge,
            PolicyBlocker::ActiveRebase,
            PolicyBlocker::ActiveCherryPick,
            PolicyBlocker::ActiveRevert,
            PolicyBlocker::ActiveBisect,
            PolicyBlocker::ActiveSequencer,
        ] {
            assert_eq!(
                operation_state_of(std::slice::from_ref(&blocker)),
                OperationState::RecoveryRequired,
                "{blocker} asks for inspection"
            );
        }
    }
}
