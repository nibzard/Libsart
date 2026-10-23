//! Pure scope and review rules, types, and invariants.
//!
//! This module holds the canonical writing-scope model of SPEC 3.2 and the
//! canonical identities of SPEC 8.6. It owns the rules that need no process,
//! filesystem, or terminal:
//!
//! - validating and normalizing user-supplied roots and suffixes
//! - assembling the canonical scope descriptor
//! - producing the domain-separated canonical bytes of every digest input
//! - deciding which repository paths a canonical scope contains
//! - assembling the canonical bytes of the content, manifest, review,
//!   change, and plan identities
//! - splitting and strictly parsing the complete source patches of the
//!   fixed diff contract into the one authoritative change model of
//!   SPEC 10.1
//! - assembling and strictly parsing the reserved commit trailers of SPEC 8.3
//! - validating, encoding, and strictly parsing the documented
//!   `margin.*` local-setting values of SPEC 8.2
//! - strictly parsing the review plan document of SPEC 7.3 and checking
//!   its decisions against the change manifest of one pinned review
//!
//! The application layer supplies directory information from the filesystem
//! and computes each identity through Git's object hashing (SPEC 8.6).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Version of the scope descriptor contract (SPEC 3.2).
pub const SCOPE_DESCRIPTOR_VERSION: u32 = 1;

/// Fixed path policy of release 0.1: literal UTF-8 roots, no glob language.
pub const PATH_POLICY_LITERAL_UTF8_V1: &str = "literal-utf8-v1";

/// Fixed storage policy of release 0.1 (SPEC 8.8).
pub const STORAGE_POLICY_IDENTITY_V1: &str = "identity-v1";

/// Domain separation placed in front of the canonical descriptor bytes
/// before hashing (SPEC 8.6). The string names the object kind and the
/// descriptor version, so a scope digest can never collide with another
/// hashed contract by accident.
pub const SCOPE_DIGEST_DOMAIN: &[u8] = b"margin-scope-v1\0";

/// Canonical entry for the repository root directory.
const ROOT_ENTRY: &str = "./";

/// User-supplied scope before validation (SPEC 6.2 `--scope` inputs).
///
/// Roots and suffixes arrive as raw strings from the interface layer. Every
/// value must be valid UTF-8 because the canonical descriptor is JSON and
/// release 0.1 defines no other path encoding (SPEC 3.3).
#[derive(Clone, Debug, Default)]
pub struct ScopeInput {
    /// Include roots, such as `content/` or `README.md`.
    pub include: Vec<String>,
    /// Exclude roots subtracted from the included set.
    pub exclude: Vec<String>,
    /// Allowed file suffixes, such as `.md`.
    pub suffixes: Vec<String>,
}

/// One validated scope root: normalized components plus whether the user
/// spelled it with a trailing slash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedRoot {
    /// Path components in order, without separators. An empty vector is the
    /// repository root.
    pub components: Vec<String>,
    /// The user ended the root with `/`, naming a directory.
    pub spelled_as_dir: bool,
}

/// A validated scope before the filesystem annotates directory roots.
#[derive(Clone, Debug)]
pub struct ParsedScope {
    /// Validated include roots.
    pub include: Vec<ParsedRoot>,
    /// Validated exclude roots.
    pub exclude: Vec<ParsedRoot>,
    /// Canonical suffixes: ASCII lowercase, sorted, deduplicated.
    pub suffixes: Vec<String>,
}

/// One canonical root entry after filesystem annotation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootEntry {
    /// Normalized path components; empty for the repository root.
    pub components: Vec<String>,
    /// The root names a directory and therefore matches every path below it.
    pub is_dir_root: bool,
}

/// The canonical scope descriptor of SPEC 3.2.
///
/// Serialization uses the fixed field order `v`, `include`, `exclude`,
/// `suffixes`, `path_policy`, `storage_policy` with no insignificant
/// whitespace, matching the specification example byte for byte.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeDescriptor {
    /// Descriptor contract version; see [`SCOPE_DESCRIPTOR_VERSION`].
    #[serde(rename = "v")]
    pub version: u32,
    /// Canonical include roots, sorted by UTF-8 bytes. Directory roots end
    /// with `/`; the repository root is `./`.
    pub include: Vec<String>,
    /// Canonical exclude roots, sorted by UTF-8 bytes.
    pub exclude: Vec<String>,
    /// Allowed suffixes, ASCII lowercase, sorted, deduplicated.
    pub suffixes: Vec<String>,
    /// Fixed path policy identifier.
    pub path_policy: String,
    /// Fixed storage policy identifier.
    pub storage_policy: String,
}

/// Why user-supplied scope input is invalid (SPEC 3.2 rule 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeError {
    /// A root or suffix is not valid UTF-8 (SPEC 3.3).
    NonUtf8 {
        /// What kind of value failed.
        kind: &'static str,
        /// The raw bytes that were rejected.
        bytes: Vec<u8>,
    },
    /// A root is empty.
    EmptyRoot,
    /// A root is absolute rather than repository-relative.
    AbsoluteRoot {
        /// The rejected root.
        root: String,
    },
    /// A root contains `..` and escapes the repository.
    ParentEscape {
        /// The rejected root.
        root: String,
    },
    /// A root contains a NUL byte.
    NulByte {
        /// The rejected root.
        root: String,
    },
    /// A symlink was found in a user-supplied root (SPEC 3.2 rule 1).
    SymlinkRoot {
        /// The rejected root.
        root: String,
        /// The path that is a symbolic link.
        at: String,
    },
    /// A root runs below an existing non-directory.
    RootBelowNonDirectory {
        /// The rejected root.
        root: String,
        /// The existing file that blocks the root.
        at: String,
    },
    /// A suffix does not start with `.` or contains a separator.
    InvalidSuffix {
        /// The rejected suffix.
        suffix: String,
    },
    /// No include root remains after parsing.
    EmptyInclude,
}

impl ScopeError {
    /// The SPEC 7.5 error code of this rejection.
    pub fn public_code(&self) -> &'static str {
        // Every variant is a user-input shape problem at this layer.
        "INVALID_ARGUMENT"
    }
}

impl std::fmt::Display for ScopeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScopeError::NonUtf8 { kind, bytes } => write!(
                formatter,
                "the {kind} is not valid UTF-8: {:?}",
                String::from_utf8_lossy(bytes)
            ),
            ScopeError::EmptyRoot => write!(formatter, "a scope root is empty"),
            ScopeError::AbsoluteRoot { root } => {
                write!(formatter, "the scope root {root:?} is absolute")
            }
            ScopeError::ParentEscape { root } => {
                write!(formatter, "the scope root {root:?} escapes the repository")
            }
            ScopeError::NulByte { root } => {
                write!(formatter, "the scope root {root:?} contains a NUL byte")
            }
            ScopeError::SymlinkRoot { root, at } => write!(
                formatter,
                "the scope root {root:?} passes through the symbolic link {at:?}"
            ),
            ScopeError::RootBelowNonDirectory { root, at } => write!(
                formatter,
                "the scope root {root:?} runs below the file {at:?}"
            ),
            ScopeError::InvalidSuffix { suffix } => {
                write!(
                    formatter,
                    "the suffix {suffix:?} must start with . and contain no separator"
                )
            }
            ScopeError::EmptyInclude => write!(formatter, "the scope has no include root"),
        }
    }
}

impl std::error::Error for ScopeError {}

/// Converts one raw value, rejecting bytes that are not valid UTF-8.
fn require_utf8(kind: &'static str, raw: &OsString) -> Result<String, ScopeError> {
    match raw.to_str() {
        Some(text) => Ok(text.to_string()),
        None => Err(ScopeError::NonUtf8 {
            kind,
            bytes: raw.as_encoded_bytes().to_vec(),
        }),
    }
}

impl ScopeInput {
    /// Builds scope input from raw interface values. Non-UTF-8 values are
    /// rejected explicitly with their bytes (SPEC 3.3).
    pub fn from_os(
        include: &[OsString],
        exclude: &[OsString],
        suffixes: &[OsString],
    ) -> Result<Self, ScopeError> {
        Ok(ScopeInput {
            include: include
                .iter()
                .map(|root| require_utf8("include root", root))
                .collect::<Result<_, _>>()?,
            exclude: exclude
                .iter()
                .map(|root| require_utf8("exclude root", root))
                .collect::<Result<_, _>>()?,
            suffixes: suffixes
                .iter()
                .map(|suffix| require_utf8("suffix", suffix))
                .collect::<Result<_, _>>()?,
        })
    }
}

/// Validates and normalizes one root (SPEC 3.2 rule 2).
///
/// Separators and redundant `.` components disappear, filename case is
/// preserved, `..`, absolute paths, and NUL are rejected, and the repository
/// root becomes an empty component vector.
pub fn parse_root(raw: &str) -> Result<ParsedRoot, ScopeError> {
    if raw.contains('\0') {
        return Err(ScopeError::NulByte {
            root: raw.to_string(),
        });
    }
    if raw.is_empty() {
        return Err(ScopeError::EmptyRoot);
    }
    if raw.starts_with('/') {
        return Err(ScopeError::AbsoluteRoot {
            root: raw.to_string(),
        });
    }
    let mut components = Vec::new();
    for component in raw.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            return Err(ScopeError::ParentEscape {
                root: raw.to_string(),
            });
        }
        components.push(component.to_string());
    }
    Ok(ParsedRoot {
        // The repository root is always a directory.
        spelled_as_dir: raw.ends_with('/') || components.is_empty(),
        components,
    })
}

/// Validates and normalizes one suffix.
///
/// A suffix must start with `.`, contain no separator, and is canonicalized
/// to ASCII lowercase because suffix matching is ASCII case-insensitive
/// (SPEC 3.2 rule 2).
pub fn parse_suffix(raw: &str) -> Result<String, ScopeError> {
    let invalid =
        raw.is_empty() || !raw.starts_with('.') || raw.contains('/') || raw.contains('\0');
    if invalid {
        return Err(ScopeError::InvalidSuffix {
            suffix: raw.to_string(),
        });
    }
    Ok(raw
        .chars()
        .map(|character| character.to_ascii_lowercase())
        .collect())
}

/// Why one document path cannot address one repository file (SPEC 6.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DocumentPathError {
    /// The path is empty, or every component disappears in normalization.
    EmptyPath,
    /// The path is absolute rather than repository-relative.
    AbsolutePath {
        /// The rejected path.
        path: String,
    },
    /// The path contains `..` and escapes the repository.
    ParentEscape {
        /// The rejected path.
        path: String,
    },
    /// The path ends with a separator, so it names a directory shape rather
    /// than one document.
    TrailingSeparator {
        /// The rejected path.
        path: String,
    },
    /// The path contains a NUL byte.
    NulByte {
        /// The rejected path.
        path: String,
    },
}

impl DocumentPathError {
    /// The SPEC 7.5 error code of this rejection.
    pub fn public_code(&self) -> &'static str {
        // Every variant is a user-input shape problem at this layer.
        "INVALID_ARGUMENT"
    }
}

impl std::fmt::Display for DocumentPathError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DocumentPathError::EmptyPath => {
                write!(formatter, "the document path is empty")
            }
            DocumentPathError::AbsolutePath { path } => {
                write!(formatter, "the document path {path:?} is absolute")
            }
            DocumentPathError::ParentEscape { path } => {
                write!(
                    formatter,
                    "the document path {path:?} escapes the repository"
                )
            }
            DocumentPathError::TrailingSeparator { path } => write!(
                formatter,
                "the document path {path:?} ends with a separator, so it names no one document"
            ),
            DocumentPathError::NulByte { path } => {
                write!(formatter, "the document path {path:?} contains a NUL byte")
            }
        }
    }
}

impl std::error::Error for DocumentPathError {}

/// Parses one repository-relative document path (SPEC 4.1, 6.2).
///
/// A document addresses one file, so the same normalization the scope roots
/// use applies — separators and redundant `.` components disappear, filename
/// case is preserved — while a trailing separator is rejected: it names a
/// directory shape, not one document. Absolute paths, `..`, and NUL are
/// rejected exactly as they are for scope roots, and a path that normalizes
/// to nothing addresses the repository root rather than a document.
pub fn parse_document_path(raw: &str) -> Result<String, DocumentPathError> {
    if raw.contains('\0') {
        return Err(DocumentPathError::NulByte {
            path: raw.to_string(),
        });
    }
    if raw.is_empty() {
        return Err(DocumentPathError::EmptyPath);
    }
    if raw.starts_with('/') {
        return Err(DocumentPathError::AbsolutePath {
            path: raw.to_string(),
        });
    }
    if raw.ends_with('/') {
        return Err(DocumentPathError::TrailingSeparator {
            path: raw.to_string(),
        });
    }
    let mut components = Vec::new();
    for component in raw.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            return Err(DocumentPathError::ParentEscape {
                path: raw.to_string(),
            });
        }
        components.push(component);
    }
    if components.is_empty() {
        return Err(DocumentPathError::EmptyPath);
    }
    Ok(components.join("/"))
}

/// Validates a whole scope input into its parsed form.
pub fn parse_scope(input: &ScopeInput) -> Result<ParsedScope, ScopeError> {
    let parse_each = |values: &[String]| -> Result<Vec<ParsedRoot>, ScopeError> {
        values.iter().map(|root| parse_root(root)).collect()
    };
    let mut suffixes: Vec<String> = Vec::new();
    for suffix in &input.suffixes {
        suffixes.push(parse_suffix(suffix)?);
    }
    suffixes.sort();
    suffixes.dedup();
    Ok(ParsedScope {
        include: parse_each(&input.include)?,
        exclude: parse_each(&input.exclude)?,
        suffixes,
    })
}

impl ScopeDescriptor {
    /// Assembles the canonical descriptor from annotated roots and parsed
    /// suffixes (SPEC 3.2 rule 2).
    ///
    /// Entries are rendered in canonical form, sorted by UTF-8 bytes, and
    /// duplicate or redundant roots are removed: a directory root absorbs
    /// every root below it, and the repository root absorbs everything.
    pub fn assemble(
        include: &[RootEntry],
        exclude: &[RootEntry],
        suffixes: &[String],
    ) -> Result<Self, ScopeError> {
        let include = canonical_roots(include);
        let exclude = canonical_roots(exclude);
        if include.is_empty() {
            return Err(ScopeError::EmptyInclude);
        }
        // Suffixes are canonicalized here as well, not only in
        // [`parse_suffix`], so no caller can assemble a non-canonical
        // descriptor.
        let mut suffixes: Vec<String> = suffixes
            .iter()
            .map(|suffix| {
                suffix
                    .chars()
                    .map(|character| character.to_ascii_lowercase())
                    .collect()
            })
            .collect();
        suffixes.sort();
        suffixes.dedup();
        Ok(ScopeDescriptor {
            version: SCOPE_DESCRIPTOR_VERSION,
            include,
            exclude,
            suffixes,
            path_policy: PATH_POLICY_LITERAL_UTF8_V1.to_string(),
            storage_policy: STORAGE_POLICY_IDENTITY_V1.to_string(),
        })
    }

    /// The canonical descriptor bytes: fixed field order and no insignificant
    /// whitespace (SPEC 8.6).
    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_string(self)
            .expect("the descriptor serializes")
            .into_bytes()
    }

    /// The bytes hashed for the scope digest: domain separation followed by
    /// the canonical descriptor bytes (SPEC 8.6).
    pub fn digest_input_bytes(&self) -> Vec<u8> {
        let mut bytes = SCOPE_DIGEST_DOMAIN.to_vec();
        bytes.extend_from_slice(&self.canonical_bytes());
        bytes
    }

    /// Says whether one repository-relative path belongs to the scope.
    ///
    /// A directory root contains every path below it; a root without a
    /// trailing slash contains exactly that path. Exclude roots subtract
    /// from the included set, and the suffix filter applies last. Matching
    /// works on raw bytes so unusual names need no interpretation.
    pub fn contains_path(&self, path: &[u8]) -> bool {
        self.suffix_matches(path)
            && self.include.iter().any(|root| root_contains(root, path))
            && !self.exclude.iter().any(|root| root_contains(root, path))
    }

    /// Says whether one directory entry, spelled with a trailing `/`, lies
    /// inside the scope boundaries. The suffix filter does not apply because
    /// a directory has no suffix; callers use this to detect scope blockers
    /// such as nested repositories (SPEC 3.2 rule 5).
    pub fn contains_directory(&self, entry: &[u8]) -> bool {
        debug_assert!(entry.last() == Some(&b'/'), "directory entries end with /");
        self.include
            .iter()
            .any(|root| root == ROOT_ENTRY || dir_root_contains(root, entry))
            && !self
                .exclude
                .iter()
                .any(|root| dir_root_contains(root, entry))
    }

    /// Says whether the path ends with an allowed suffix. The comparison is
    /// ASCII case-insensitive; an empty suffix list matches nothing.
    pub fn suffix_matches(&self, path: &[u8]) -> bool {
        self.suffixes
            .iter()
            .any(|suffix| path.ends_with_ascii_case_insensitive(suffix.as_bytes()))
    }

    /// Says whether this descriptor is one release 0.1 can interpret: the
    /// supported version and the fixed path and storage policies (SPEC 3.2
    /// and 8.8). A structurally valid descriptor of another version stays
    /// inspectable but never anchors a marker.
    pub fn is_supported(&self) -> bool {
        self.version == SCOPE_DESCRIPTOR_VERSION
            && self.path_policy == PATH_POLICY_LITERAL_UTF8_V1
            && self.storage_policy == STORAGE_POLICY_IDENTITY_V1
    }

    /// Re-canonicalizes a parsed descriptor. Returns `None` when a field
    /// cannot take part in canonical form at all.
    ///
    /// Serialization alone preserves whatever the parsed value said, so a
    /// non-canonical value would hash itself and pass a digest check.
    /// Re-running the canonical rules is what lets validation prove a value
    /// is the canonical form of itself (SPEC 8.4 rule 2).
    pub fn recanonicalized(&self) -> Option<ScopeDescriptor> {
        let roots = |rendered: &[String]| -> Option<Vec<RootEntry>> {
            rendered
                .iter()
                .map(|root| {
                    if root == ROOT_ENTRY {
                        return Some(RootEntry {
                            components: Vec::new(),
                            is_dir_root: true,
                        });
                    }
                    let is_dir_root = root.ends_with('/');
                    let body = root.strip_suffix('/').unwrap_or(root);
                    if body.is_empty() || body.split('/').any(str::is_empty) {
                        return None;
                    }
                    Some(RootEntry {
                        components: body.split('/').map(str::to_string).collect(),
                        is_dir_root,
                    })
                })
                .collect()
        };
        ScopeDescriptor::assemble(
            &roots(&self.include)?,
            &roots(&self.exclude)?,
            &self.suffixes,
        )
        .ok()
    }
}

/// Renders, sorts, and prunes roots into canonical entries.
fn canonical_roots(entries: &[RootEntry]) -> Vec<String> {
    let mut roots: Vec<String> = entries
        .iter()
        .map(|entry| render_root(&entry.components, entry.is_dir_root))
        .collect();
    roots.sort();
    roots.dedup();
    // The repository root absorbs every other root.
    if roots.iter().any(|root| root == ROOT_ENTRY) {
        return vec![ROOT_ENTRY.to_string()];
    }
    let directory_roots: Vec<String> = roots
        .iter()
        .filter(|root| root.ends_with('/'))
        .cloned()
        .collect();
    roots
        .into_iter()
        .filter(|root| {
            !directory_roots
                .iter()
                .any(|directory| directory != root && root.starts_with(directory.as_str()))
        })
        .collect()
}

/// Renders one root in canonical form.
fn render_root(components: &[String], is_dir_root: bool) -> String {
    if components.is_empty() {
        return ROOT_ENTRY.to_string();
    }
    let mut rendered = components.join("/");
    if is_dir_root {
        rendered.push('/');
    }
    rendered
}

/// Says whether one canonical root contains one path.
fn root_contains(root: &str, path: &[u8]) -> bool {
    if root == ROOT_ENTRY {
        return true;
    }
    let root_bytes = root.as_bytes();
    match root_bytes.last() {
        Some(b'/') => path.starts_with(root_bytes),
        // A root without a trailing slash is an exact file path.
        _ => path == root_bytes,
    }
}

/// Says whether one canonical directory root contains one directory entry.
fn dir_root_contains(root: &str, entry: &[u8]) -> bool {
    root.ends_with('/') && entry.starts_with(root.as_bytes())
}

/// ASCII case-insensitive `ends_with` for byte strings.
trait EndsAsciiCaseInsensitive {
    fn ends_with_ascii_case_insensitive(&self, suffix: &[u8]) -> bool;
}

impl EndsAsciiCaseInsensitive for [u8] {
    fn ends_with_ascii_case_insensitive(&self, suffix: &[u8]) -> bool {
        self.len() >= suffix.len() && self[self.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
    }
}

// ---------------------------------------------------------------------------
// Canonical identities (SPEC 8.6)
// ---------------------------------------------------------------------------

/// Domain separation placed in front of the canonical content-digest bytes
/// (SPEC 8.6).
pub const CONTENT_DIGEST_DOMAIN: &[u8] = b"margin-content-v1\0";

/// Domain separation placed in front of the canonical manifest-digest bytes
/// (SPEC 8.6).
pub const MANIFEST_DIGEST_DOMAIN: &[u8] = b"margin-manifest-v1\0";

/// Domain separation placed in front of the canonical review-identity bytes
/// (SPEC 8.6).
pub const REVIEW_ID_DOMAIN: &[u8] = b"margin-review-v1\0";

/// Domain separation placed in front of the canonical change-identity bytes
/// (SPEC 8.6).
pub const CHANGE_ID_DOMAIN: &[u8] = b"margin-change-v1\0";

/// Domain separation placed in front of the canonical plan-fingerprint bytes
/// (SPEC 8.6).
pub const PLAN_FINGERPRINT_DOMAIN: &[u8] = b"margin-plan-v1\0";

/// Domain separation placed in front of the canonical cursor-digest bytes
/// (SPEC 6.3). A cursor binds one page position to the review identity and
/// page contract it was issued under.
pub const CURSOR_DOMAIN: &[u8] = b"margin-cursor-v1\0";

/// Domain separation placed in front of the canonical timeline-cursor bytes
/// (SPEC 6.3). A `log` cursor binds one page position to the editorial
/// timeline identity and page contract it was issued under, so it can never
/// be confused with a detail-page cursor.
pub const LOG_CURSOR_DOMAIN: &[u8] = b"margin-log-cursor-v1\0";

/// Domain separation placed in front of the canonical review-context-key
/// bytes (SPEC 8.6, 13.3). The key names one whole pinned comparison
/// without its manifest digest, so a rebuildable cache entry can be found
/// before the manifest it would replace has been derived.
pub const REVIEW_CACHE_KEY_DOMAIN: &[u8] = b"margin-review-cache-v1\0";

/// Version of the engine contract: the patch generation, grouping, and
/// canonicalization rules of SPEC 10.1 that produced a comparison. Every
/// review identity covers it, so a changed engine regenerates every review
/// and change identity (SPEC 7.2 and 8.6).
pub const ENGINE_VERSION: &str = "1";

/// Builds the canonical bytes of one identity input (SPEC 8.6).
///
/// The writer is injective: every text and byte value carries its byte
/// length as an 8-byte big-endian prefix, so adjacent values can never
/// merge and raw patch bytes need no escaping. Field order is fixed by each
/// identity's encoding function, and arrays are written sorted only where
/// their order is not semantic. No floating-point value exists in any
/// identity.
#[derive(Debug)]
struct CanonicalWriter {
    bytes: Vec<u8>,
}

impl CanonicalWriter {
    /// Starts one input with its domain-separation prefix.
    fn new(domain: &[u8]) -> Self {
        CanonicalWriter {
            bytes: domain.to_vec(),
        }
    }

    /// Writes one length-prefixed raw value.
    fn bytes(&mut self, value: &[u8]) {
        self.bytes
            .extend_from_slice(&(value.len() as u64).to_be_bytes());
        self.bytes.extend_from_slice(value);
    }

    /// Writes one length-prefixed UTF-8 value.
    fn text(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    /// Writes one unsigned integer field as 8 big-endian bytes.
    fn count(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    /// Writes one Boolean field as one byte.
    fn flag(&mut self, value: bool) {
        self.bytes.push(u8::from(value));
    }

    /// Finishes the input.
    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

/// The storage mode of one scoped content entry: the two modes Git records
/// for regular files. A symbolic link and a gitlink never join a content
/// digest; both are scope blockers (SPEC 3.2 rule 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryMode {
    /// Mode `100644`.
    Regular,
    /// Mode `100755`.
    Executable,
}

impl EntryMode {
    /// The six octal digits Git records for the mode.
    pub fn as_str(self) -> &'static str {
        match self {
            EntryMode::Regular => "100644",
            EntryMode::Executable => "100755",
        }
    }

    /// Parses one `git ls-files --stage` or `git ls-tree` mode field.
    /// Returns `None` for every mode that is not a regular file.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "100644" => Some(EntryMode::Regular),
            "100755" => Some(EntryMode::Executable),
            _ => None,
        }
    }
}

/// One `(path, mode, blob_oid)` entry of a scoped content digest (SPEC 8.6).
///
/// For stored content the object ID is the blob of the tree entry. For
/// working content it is the hash of the raw working bytes under the
/// identity profile. Presence is membership in the entry set: an absent
/// path contributes no entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentEntry {
    /// Literal repository-relative path, valid UTF-8.
    pub path: String,
    /// Storage mode of the entry.
    pub mode: EntryMode,
    /// Full lowercase hex blob object ID.
    pub blob_oid: String,
}

impl ContentEntry {
    /// Sort key: the path's UTF-8 bytes, the array order of SPEC 3.2 rule 2.
    fn sort_key(&self) -> &[u8] {
        self.path.as_bytes()
    }
}

/// The canonical bytes of a scoped content digest input: the entry count
/// followed by every `(path, mode, blob_oid)` entry sorted by path bytes
/// (SPEC 8.6). The entry set itself distinguishes presence from absence.
pub fn content_digest_input_bytes(entries: &[ContentEntry]) -> Vec<u8> {
    let mut sorted: Vec<&ContentEntry> = entries.iter().collect();
    sorted.sort_by(|left, right| left.sort_key().cmp(right.sort_key()));
    let mut writer = CanonicalWriter::new(CONTENT_DIGEST_DOMAIN);
    writer.count(sorted.len() as u64);
    for entry in sorted {
        writer.text(&entry.path);
        writer.text(entry.mode.as_str());
        writer.text(&entry.blob_oid);
    }
    writer.finish()
}

/// One complete change group of the canonical manifest: a literal path and
/// the original complete patch units of that group in generated order
/// (SPEC 8.6 and 10.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeGroup {
    /// Literal repository-relative path, valid UTF-8.
    pub path: String,
    /// Original complete patch units, such as complete hunks or whole-file
    /// patches, keeping their exact Git diff bytes.
    pub patch_units: Vec<Vec<u8>>,
}

/// The canonical bytes of a manifest digest input: every group in its
/// manifest order, each with its literal path and complete patch units
/// (SPEC 7.2 and 8.6). The group ordering is semantic, so it stays part of
/// the input.
pub fn manifest_digest_input_bytes(groups: &[ChangeGroup]) -> Vec<u8> {
    let mut writer = CanonicalWriter::new(MANIFEST_DIGEST_DOMAIN);
    writer.count(groups.len() as u64);
    for group in groups {
        writer.text(&group.path);
        writer.count(group.patch_units.len() as u64);
        for unit in &group.patch_units {
            writer.bytes(unit);
        }
    }
    writer.finish()
}

/// The pinned comparison one review identity covers (SPEC 7.2 and 8.6):
/// branch ref, baseline B, candidate C, the canonical scope descriptor, the
/// engine version, the effective Git version, and the complete manifest
/// digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewContext {
    /// Full adopted branch ref, such as `refs/heads/writing`.
    pub branch_ref: String,
    /// Baseline commit B.
    pub baseline_oid: String,
    /// Candidate commit C.
    pub candidate_oid: String,
    /// The canonical scope descriptor.
    pub scope: ScopeDescriptor,
    /// Engine contract version; see [`ENGINE_VERSION`].
    pub engine_version: String,
    /// Effective Git version that contributed to the comparison contract.
    pub git_version: String,
    /// Digest of the complete change manifest.
    pub manifest_digest: String,
}

/// The canonical bytes of a review identity input (SPEC 8.6).
pub fn review_id_input_bytes(context: &ReviewContext) -> Vec<u8> {
    let mut writer = CanonicalWriter::new(REVIEW_ID_DOMAIN);
    writer.text(&context.branch_ref);
    writer.text(&context.baseline_oid);
    writer.text(&context.candidate_oid);
    writer.bytes(&context.scope.canonical_bytes());
    writer.text(&context.engine_version);
    writer.text(&context.git_version);
    writer.text(&context.manifest_digest);
    writer.finish()
}

/// The comparison context every field of a review identity covers except
/// the manifest digest (SPEC 7.2, 8.6): one whole pinned comparison before
/// its groups were derived. A rebuildable cache entry is keyed by exactly
/// these fields, because they identify the derivation inputs — the same
/// branch, baseline, candidate, scope, engine, and Git version always
/// derive the same manifest (SPEC 8.6, 13.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewContextKey {
    /// Full adopted branch ref, such as `refs/heads/writing`.
    pub branch_ref: String,
    /// Baseline commit B.
    pub baseline_oid: String,
    /// Candidate commit C.
    pub candidate_oid: String,
    /// The canonical scope descriptor.
    pub scope: ScopeDescriptor,
    /// Engine contract version; see [`ENGINE_VERSION`].
    pub engine_version: String,
    /// Effective Git version that contributed to the comparison contract.
    pub git_version: String,
}

impl From<&ReviewContext> for ReviewContextKey {
    fn from(context: &ReviewContext) -> Self {
        ReviewContextKey {
            branch_ref: context.branch_ref.clone(),
            baseline_oid: context.baseline_oid.clone(),
            candidate_oid: context.candidate_oid.clone(),
            scope: context.scope.clone(),
            engine_version: context.engine_version.clone(),
            git_version: context.git_version.clone(),
        }
    }
}

/// The canonical bytes of one review-context key (SPEC 8.6, 13.3).
///
/// The encoding reuses the review identity's field order under its own
/// domain tag, so a key can never be confused with a review identity or a
/// manifest digest, and no key collides with another context.
pub fn review_cache_key_input_bytes(key: &ReviewContextKey) -> Vec<u8> {
    let mut writer = CanonicalWriter::new(REVIEW_CACHE_KEY_DOMAIN);
    writer.text(&key.branch_ref);
    writer.text(&key.baseline_oid);
    writer.text(&key.candidate_oid);
    writer.bytes(&key.scope.canonical_bytes());
    writer.text(&key.engine_version);
    writer.text(&key.git_version);
    writer.finish()
}

/// The parts of one change group a change identity covers (SPEC 8.6): the
/// review it belongs to, the literal path, the original complete patch
/// units, and the group's position in the manifest ordering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeIdentity {
    /// Object ID of the review identity this change belongs to.
    pub review_oid: String,
    /// Literal repository-relative path.
    pub path: String,
    /// Original complete patch units in generated order.
    pub patch_units: Vec<Vec<u8>>,
    /// Zero-based position of the group in the manifest ordering.
    pub group_index: u64,
}

/// The canonical bytes of a change identity input (SPEC 8.6).
pub fn change_id_input_bytes(change: &ChangeIdentity) -> Vec<u8> {
    let mut writer = CanonicalWriter::new(CHANGE_ID_DOMAIN);
    writer.text(&change.review_oid);
    writer.text(&change.path);
    writer.count(change.patch_units.len() as u64);
    for unit in &change.patch_units {
        writer.bytes(unit);
    }
    writer.count(change.group_index);
    writer.finish()
}

/// The context one detail-page cursor is bound to (SPEC 6.3): the review
/// identity it was issued under and the page contract that defined the page
/// sequence. A cursor used with any changed field is stale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorContext {
    /// The review identity the cursor was issued under, in `sha1:<oid>`
    /// form.
    pub review_oid: String,
    /// Whether the cursor pages through one working preview.
    pub working: bool,
    /// The presentation path filter, in the order the request supplied it.
    pub path_filter: Vec<String>,
    /// The change-group limit the page sequence was issued under.
    pub limit: u64,
    /// The zero-based position, in filtered detail order, the cursor
    /// continues from.
    pub offset: u64,
}

/// The canonical bytes of a cursor digest input (SPEC 6.3). The digest is a
/// context identifier that lets a stateless caller be told its cursor no
/// longer matches the comparison it paginates.
pub fn cursor_digest_input_bytes(context: &CursorContext) -> Vec<u8> {
    let mut writer = CanonicalWriter::new(CURSOR_DOMAIN);
    writer.text(&context.review_oid);
    writer.flag(context.working);
    writer.count(context.path_filter.len() as u64);
    for filter in &context.path_filter {
        writer.text(filter);
    }
    writer.count(context.limit);
    writer.count(context.offset);
    writer.finish()
}

/// The context one timeline page cursor is bound to (SPEC 6.3): the commit
/// the timeline starts from, the adopted scope, the checkpoint display
/// policy, the presentation filter, and the page limit. A cursor used with
/// any changed field is stale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogCursorContext {
    /// The commit the walk starts from: the candidate of the workspace.
    pub head: String,
    /// The adopted scope identity, in `sha1:<oid>` form.
    pub scope_id: String,
    /// Whether the timeline lists every draft checkpoint instead of
    /// collapsed writing passes (SPEC 5.5).
    pub all_checkpoints: bool,
    /// The presentation path filter, in the order the request supplied it.
    pub path_filter: Vec<String>,
    /// The entry limit the page sequence was issued under.
    pub limit: u64,
    /// The zero-based position, in the entry order the contract produces,
    /// the cursor continues from.
    pub offset: u64,
}

/// The canonical bytes of one timeline cursor digest input (SPEC 6.3). The
/// digest is a context identifier that lets a stateless caller be told its
/// cursor no longer matches the timeline it paginates.
pub fn log_cursor_digest_input_bytes(context: &LogCursorContext) -> Vec<u8> {
    let mut writer = CanonicalWriter::new(LOG_CURSOR_DOMAIN);
    writer.text(&context.head);
    writer.text(&context.scope_id);
    writer.flag(context.all_checkpoints);
    writer.count(context.path_filter.len() as u64);
    for filter in &context.path_filter {
        writer.text(filter);
    }
    writer.count(context.limit);
    writer.count(context.offset);
    writer.finish()
}

/// The decision mode of a review plan. Exactly one mode is required, and
/// the mode is part of the plan fingerprint (SPEC 7.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecisionMode {
    /// `keep_remaining` is `true`: every change not restored is kept.
    KeepRemaining,
    /// An explicit `keep` array names every other change identity.
    KeepExplicit {
        /// Sorted change identities the plan keeps.
        keep: Vec<String>,
    },
}

/// The parts of one review plan its fingerprint covers (SPEC 7.3 and 8.6):
/// the operation kind, every context field, the sorted decisions, the
/// decision mode, and the effective message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanIdentity {
    /// Operation kind, such as `review`.
    pub operation: String,
    /// Request identifier.
    pub request_id: String,
    /// Review identity the plan is pinned to.
    pub review_id: String,
    /// Full adopted branch ref.
    pub branch_ref: String,
    /// Scope identity digest.
    pub scope_id: String,
    /// Baseline commit B.
    pub baseline_oid: String,
    /// Candidate commit C.
    pub candidate_oid: String,
    /// Change identities to restore. The order is not semantic.
    pub restore: Vec<String>,
    /// Exactly one decision mode.
    pub decision_mode: DecisionMode,
    /// The effective commit message: the explicit message or the
    /// deterministic default.
    pub message: String,
}

/// Sorts one decision array. Plan decisions carry no order (SPEC 7.3).
fn sorted_decisions(values: &[String]) -> Vec<&str> {
    let mut sorted: Vec<&str> = values.iter().map(String::as_str).collect();
    sorted.sort();
    sorted
}

/// The canonical bytes of a plan fingerprint input (SPEC 7.3 and 8.6). The
/// restore and keep arrays are sorted, because plan decisions carry no
/// order.
pub fn plan_fingerprint_input_bytes(plan: &PlanIdentity) -> Vec<u8> {
    let mut writer = CanonicalWriter::new(PLAN_FINGERPRINT_DOMAIN);
    writer.text(&plan.operation);
    writer.text(&plan.request_id);
    writer.text(&plan.review_id);
    writer.text(&plan.branch_ref);
    writer.text(&plan.scope_id);
    writer.text(&plan.baseline_oid);
    writer.text(&plan.candidate_oid);
    writer.count(plan.restore.len() as u64);
    for change in sorted_decisions(&plan.restore) {
        writer.text(change);
    }
    match &plan.decision_mode {
        DecisionMode::KeepRemaining => writer.flag(false),
        DecisionMode::KeepExplicit { keep } => {
            writer.flag(true);
            writer.count(keep.len() as u64);
            for change in sorted_decisions(keep) {
                writer.text(change);
            }
        }
    }
    writer.text(&plan.message);
    writer.finish()
}

// ---------------------------------------------------------------------------
// Review plan v1 (SPEC 7.3)
// ---------------------------------------------------------------------------

/// Version of the review plan input contract (SPEC 7.3).
pub const REVIEW_PLAN_VERSION: u32 = 1;

/// The operation kind a review plan fingerprint names (SPEC 7.3, 8.6). The
/// historical restoration plan of SPEC 7.4 keeps its own kind.
pub const REVIEW_PLAN_OPERATION: &str = "review";

/// Every field one review plan document may carry (SPEC 7.3), in the order
/// the specification lists them. The strict parser, the shipped JSON
/// Schema, and the drift test between the two all read this one list, so
/// the input contract has exactly one definition (SPEC 7.3, 13.6).
pub const REVIEW_PLAN_FIELDS: &[&str] = &[
    "schema_version",
    "request_id",
    "review_id",
    "branch_ref",
    "scope_id",
    "baseline_oid",
    "candidate_oid",
    "restore",
    "keep_remaining",
    "keep",
    "message",
];

/// The fields every plan must carry (SPEC 7.3). `keep_remaining` and `keep`
/// hold exactly one decision mode between them, and an omitted `message`
/// uses the deterministic default subject, so none of the three is
/// individually required.
pub const REVIEW_PLAN_REQUIRED_FIELDS: &[&str] = &[
    "schema_version",
    "request_id",
    "review_id",
    "branch_ref",
    "scope_id",
    "baseline_oid",
    "candidate_oid",
    "restore",
];

/// Largest plan document one bounded read accepts (SPEC 3.3, 7.6). The
/// acceptance envelope of 1,000 scoped documents leaves room for one full
/// change identity per group plus every context field.
pub const MAX_PLAN_BYTES: usize = 4 * 1024 * 1024;

/// Largest number of change identities one plan diagnostic spells out
/// before a remaining count takes over (SPEC 7.6).
pub const MAX_LISTED_PLAN_CHANGES: usize = 5;

/// One complete parsed review plan (SPEC 7.3).
///
/// Parsing is strict and total: a document either satisfies every rule the
/// input contract states, or it refuses with one [`PlanProblem`] and
/// nothing of it survives. The two values the document does not spell out —
/// the effective message and the one decision mode — are resolved here, so
/// the plan fingerprint of SPEC 8.6 covers exactly what the writer decided.
///
/// Decision arrays are sorted on parse, because plan decisions carry no
/// order. Coverage against a real review is a separate check; see
/// [`validate_plan_coverage`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewPlan {
    /// Request identifier, 1–128 ASCII characters.
    pub request_id: String,
    /// Identity of the pinned review this plan decides.
    pub review_id: String,
    /// Full branch ref of that review.
    pub branch_ref: String,
    /// Scope identity digest of that review.
    pub scope_id: String,
    /// Baseline commit B.
    pub baseline_oid: String,
    /// Candidate commit C.
    pub candidate_oid: String,
    /// Change identities to restore, sorted.
    pub restore: Vec<String>,
    /// The one decision mode.
    pub decision_mode: DecisionMode,
    /// The effective message: the explicit text or the deterministic
    /// default subject of a review commit.
    pub message: String,
}

impl ReviewPlan {
    /// Parses one complete review plan document (SPEC 7.3).
    ///
    /// The document must be one JSON object with known keys only, each key
    /// stated once, valid UTF-8, and the supported contract version. Every
    /// context field must carry its documented form: a request identifier
    /// of letters, digits, `.`, `_`, `:`, and `-`; full identity strings
    /// for the review and scope; full object IDs for B and C; a full branch
    /// ref. Exactly one decision mode must be stated, no identity may
    /// appear twice in one array, and no identity may be both restored and
    /// kept. An explicit message obeys the shared free-text rule.
    pub fn parse(raw: &[u8]) -> Result<ReviewPlan, PlanProblem> {
        // The map access hands over every entry the document states, so a
        // repeated key stays visible here instead of silently overwriting
        // its earlier value the way an object map would.
        let fields =
            serde_json::from_slice::<PlanFieldList>(raw).map_err(|error| PlanProblem::NotJson {
                why: error.to_string(),
            })?;
        let mut values: BTreeMap<&'static str, serde_json::Value> = BTreeMap::new();
        for (key, value) in fields.0 {
            let Some(field) = REVIEW_PLAN_FIELDS
                .iter()
                .copied()
                .find(|known| *known == key)
            else {
                return Err(PlanProblem::UnknownField { field: key });
            };
            if values.insert(field, value).is_some() {
                return Err(PlanProblem::DuplicateField {
                    field: field.to_string(),
                });
            }
        }

        let schema_version = plan_number(
            "schema_version",
            plan_present(&mut values, "schema_version")?,
        )?;
        if schema_version != u64::from(REVIEW_PLAN_VERSION) {
            return Err(PlanProblem::UnsupportedVersion {
                observed: schema_version,
            });
        }
        let request_id = plan_string("request_id", plan_present(&mut values, "request_id")?)?;
        let review_id = plan_string("review_id", plan_present(&mut values, "review_id")?)?;
        let branch_ref = plan_string("branch_ref", plan_present(&mut values, "branch_ref")?)?;
        let scope_id = plan_string("scope_id", plan_present(&mut values, "scope_id")?)?;
        let baseline_oid = plan_string("baseline_oid", plan_present(&mut values, "baseline_oid")?)?;
        let candidate_oid =
            plan_string("candidate_oid", plan_present(&mut values, "candidate_oid")?)?;
        let restore = plan_string_array("restore", plan_present(&mut values, "restore")?)?;
        let keep_remaining = plan_bool("keep_remaining", values.remove("keep_remaining"))?;
        let keep = match values.remove("keep") {
            Some(value) => Some(plan_string_array("keep", value)?),
            None => None,
        };
        let message = plan_optional_string("message", values.remove("message"))?;

        // Every context field must carry its documented form (SPEC 7.3).
        for (field, valid, expected) in [
            (
                "request_id",
                is_request_id(&request_id),
                "1 to 128 characters of letters, digits, '.', '_', ':', and '-'",
            ),
            (
                "review_id",
                is_fingerprint_id(&review_id),
                "a full Margin identity",
            ),
            (
                "branch_ref",
                is_branch_ref(&branch_ref),
                "a full refs/ name or HEAD",
            ),
            (
                "scope_id",
                is_fingerprint_id(&scope_id),
                "a full Margin identity",
            ),
            (
                "baseline_oid",
                is_full_oid(&baseline_oid),
                "a full object ID",
            ),
            (
                "candidate_oid",
                is_full_oid(&candidate_oid),
                "a full object ID",
            ),
        ] {
            if !valid {
                return Err(PlanProblem::InvalidValue {
                    field,
                    why: format!("the value is not {expected}"),
                });
            }
        }
        plan_decision_ids("restore", &restore)?;
        if let Some(keep) = &keep {
            plan_decision_ids("keep", keep)?;
        }

        // Exactly one decision mode (SPEC 7.3): `keep_remaining: true`, or
        // one explicit `keep` array. A missing or false flag without that
        // array decides nothing, and both together decide twice.
        let decision_mode = match (keep_remaining, keep) {
            (None, None) | (Some(false), None) => return Err(PlanProblem::MissingDecision),
            (Some(true), Some(_)) => return Err(PlanProblem::ConflictingDecision),
            (Some(true), None) => DecisionMode::KeepRemaining,
            (None | Some(false), Some(keep)) => DecisionMode::KeepExplicit { keep },
        };

        // Decision arrays are sorted and free of repeats, and no identity
        // may be both restored and kept (SPEC 7.3).
        let restore = plan_unique(restore)?;
        let decision_mode = match decision_mode {
            DecisionMode::KeepRemaining => decision_mode,
            DecisionMode::KeepExplicit { keep } => {
                let keep = plan_unique(keep)?;
                let restored: BTreeSet<&str> = restore.iter().map(String::as_str).collect();
                for id in &keep {
                    if restored.contains(id.as_str()) {
                        return Err(PlanProblem::OverlappingDecision { id: id.clone() });
                    }
                }
                DecisionMode::KeepExplicit { keep }
            }
        };

        // An omitted message uses the deterministic default subject of a
        // review commit, and an explicit one shares the free-text rule of
        // every other commit message (SPEC 7.3).
        let message = match message {
            Some(text) => free_text_message(CommitKind::Review, &text)
                .map_err(|problem| PlanProblem::Message { problem })?,
            None => CommitKind::Review.default_subject().to_string(),
        };

        Ok(ReviewPlan {
            request_id,
            review_id,
            branch_ref,
            scope_id,
            baseline_oid,
            candidate_oid,
            restore,
            decision_mode,
            message,
        })
    }

    /// The fingerprint input of this plan (SPEC 7.3, 8.6): the operation
    /// kind, every context field, the sorted decisions, the one decision
    /// mode, and the effective message.
    pub fn identity(&self) -> PlanIdentity {
        PlanIdentity {
            operation: REVIEW_PLAN_OPERATION.to_string(),
            request_id: self.request_id.clone(),
            review_id: self.review_id.clone(),
            branch_ref: self.branch_ref.clone(),
            scope_id: self.scope_id.clone(),
            baseline_oid: self.baseline_oid.clone(),
            candidate_oid: self.candidate_oid.clone(),
            restore: self.restore.clone(),
            decision_mode: self.decision_mode.clone(),
            message: self.message.clone(),
        }
    }
}

/// Checks one parsed plan against the complete change manifest it must
/// decide (SPEC 7.3): every identity must be one the pinned review issued,
/// and an explicit `keep` array must name every change the plan does not
/// restore.
///
/// `known` is the set of change identities of that review. A `keep_remaining`
/// plan needs no completeness check of its own, because the mode itself
/// covers every change the plan does not restore.
pub fn validate_plan_coverage(
    plan: &ReviewPlan,
    known: &BTreeSet<String>,
) -> Result<(), PlanProblem> {
    for id in plan.restore.iter().chain(match &plan.decision_mode {
        DecisionMode::KeepRemaining => [].iter(),
        DecisionMode::KeepExplicit { keep } => keep.iter(),
    }) {
        if !known.contains(id) {
            return Err(PlanProblem::UnknownChange { id: id.clone() });
        }
    }
    if let DecisionMode::KeepExplicit { keep } = &plan.decision_mode {
        let decided: BTreeSet<&str> = plan
            .restore
            .iter()
            .chain(keep)
            .map(String::as_str)
            .collect();
        let missing: Vec<String> = known
            .iter()
            .filter(|id| !decided.contains(id.as_str()))
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Err(PlanProblem::IncompleteCoverage { missing });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Historical restoration plan v1 (SPEC 7.4)
// ---------------------------------------------------------------------------

/// Version of the historical restoration plan input contract (SPEC 7.4).
pub const RESTORE_PLAN_VERSION: u32 = 1;

/// The operation kind a restoration plan fingerprint names (SPEC 7.4, 8.6).
/// A request identifier is scoped to the operation kind, so a review request
/// and a restoration request never collide (SPEC 7.3).
pub const RESTORE_PLAN_OPERATION: &str = "restore";

/// Every field one restoration plan document may carry (SPEC 7.4), in the
/// order the specification lists them. The strict parser reads this one
/// list, so the input contract has exactly one definition (SPEC 13.6).
pub const RESTORE_PLAN_FIELDS: &[&str] = &[
    "schema_version",
    "request_id",
    "branch_ref",
    "scope_id",
    "candidate_oid",
    "path",
    "revision",
    "message",
];

/// The fields every restoration plan must carry (SPEC 7.4). An omitted
/// `message` uses the deterministic default subject, so it alone is not
/// required.
pub const RESTORE_PLAN_REQUIRED_FIELDS: &[&str] = &[
    "schema_version",
    "request_id",
    "branch_ref",
    "scope_id",
    "candidate_oid",
    "path",
    "revision",
];

/// One complete parsed historical restoration plan (SPEC 7.4).
///
/// Parsing is strict and total like the review plan contract: a document
/// either satisfies every rule the input contract states, or it refuses with
/// one [`PlanProblem`] and nothing of it survives. The plan names exactly
/// one scoped document and the adopted-history revision its content comes
/// from. Whether that revision really belongs to the adopted history, and
/// whether the path holds one restorable document there, is a separate
/// check of the application layer, exactly as plan coverage is for a review
/// (SPEC 9.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestorePlan {
    /// Request identifier, 1–128 ASCII characters.
    pub request_id: String,
    /// Full adopted branch ref of the workspace the plan pins.
    pub branch_ref: String,
    /// Scope identity digest of the adopted scope.
    pub scope_id: String,
    /// The expected candidate C the restoration replaces.
    pub candidate_oid: String,
    /// The one scoped document to restore, in normalized form.
    pub path: String,
    /// The full commit object ID the restored content comes from.
    pub revision: String,
    /// The effective message: the explicit text or the deterministic
    /// default subject of a restore commit.
    pub message: String,
}

impl RestorePlan {
    /// Parses one complete restoration plan document (SPEC 7.4).
    ///
    /// The document must be one JSON object with known keys only, each key
    /// stated once, valid UTF-8, and the supported contract version. Every
    /// context field must carry its documented form: a request identifier of
    /// letters, digits, `.`, `_`, `:`, and `-`; a full identity string for
    /// the scope; full object IDs for the candidate and the source revision;
    /// a full branch ref; and one normalized repository-relative document
    /// path. An explicit message obeys the shared free-text rule.
    pub fn parse(raw: &[u8]) -> Result<RestorePlan, PlanProblem> {
        // The map access hands over every entry the document states, so a
        // repeated key stays visible here instead of silently overwriting
        // its earlier value the way an object map would.
        let fields =
            serde_json::from_slice::<PlanFieldList>(raw).map_err(|error| PlanProblem::NotJson {
                why: error.to_string(),
            })?;
        let mut values: BTreeMap<&'static str, serde_json::Value> = BTreeMap::new();
        for (key, value) in fields.0 {
            let Some(field) = RESTORE_PLAN_FIELDS
                .iter()
                .copied()
                .find(|known| *known == key)
            else {
                return Err(PlanProblem::UnknownField { field: key });
            };
            if values.insert(field, value).is_some() {
                return Err(PlanProblem::DuplicateField {
                    field: field.to_string(),
                });
            }
        }

        let schema_version = plan_number(
            "schema_version",
            plan_present(&mut values, "schema_version")?,
        )?;
        if schema_version != u64::from(RESTORE_PLAN_VERSION) {
            return Err(PlanProblem::UnsupportedVersion {
                observed: schema_version,
            });
        }
        let request_id = plan_string("request_id", plan_present(&mut values, "request_id")?)?;
        let branch_ref = plan_string("branch_ref", plan_present(&mut values, "branch_ref")?)?;
        let scope_id = plan_string("scope_id", plan_present(&mut values, "scope_id")?)?;
        let candidate_oid =
            plan_string("candidate_oid", plan_present(&mut values, "candidate_oid")?)?;
        let path = plan_string("path", plan_present(&mut values, "path")?)?;
        let revision = plan_string("revision", plan_present(&mut values, "revision")?)?;
        let message = plan_optional_string("message", values.remove("message"))?;

        // Every context field must carry its documented form (SPEC 7.4).
        for (field, valid, expected) in [
            (
                "request_id",
                is_request_id(&request_id),
                "1 to 128 characters of letters, digits, '.', '_', ':', and '-'",
            ),
            (
                "branch_ref",
                is_branch_ref(&branch_ref),
                "a full refs/ name or HEAD",
            ),
            (
                "scope_id",
                is_fingerprint_id(&scope_id),
                "a full Margin identity",
            ),
            (
                "candidate_oid",
                is_full_oid(&candidate_oid),
                "a full object ID",
            ),
            ("revision", is_full_oid(&revision), "a full object ID"),
        ] {
            if !valid {
                return Err(PlanProblem::InvalidValue {
                    field,
                    why: format!("the value is not {expected}"),
                });
            }
        }
        // The one document path shares the literal-path rules of `show`
        // (SPEC 6.3): normalized, repository-relative, and no escape.
        let path = parse_document_path(&path).map_err(|error| PlanProblem::InvalidValue {
            field: "path",
            why: error.to_string(),
        })?;

        // An omitted message uses the deterministic default subject of a
        // restore commit, and an explicit one shares the free-text rule of
        // every other commit message (SPEC 7.4, 7.3).
        let message = match message {
            Some(text) => free_text_message(CommitKind::Restore, &text)
                .map_err(|problem| PlanProblem::Message { problem })?,
            None => CommitKind::Restore.default_subject().to_string(),
        };

        Ok(RestorePlan {
            request_id,
            branch_ref,
            scope_id,
            candidate_oid,
            path,
            revision,
            message,
        })
    }

    /// The fingerprint input of this plan (SPEC 7.4, 8.6): the operation
    /// kind, every context field, the one document and revision, and the
    /// effective message.
    pub fn identity(&self) -> RestoreIdentity {
        RestoreIdentity {
            operation: RESTORE_PLAN_OPERATION.to_string(),
            request_id: self.request_id.clone(),
            branch_ref: self.branch_ref.clone(),
            scope_id: self.scope_id.clone(),
            candidate_oid: self.candidate_oid.clone(),
            path: self.path.clone(),
            revision: self.revision.clone(),
            message: self.message.clone(),
        }
    }
}

/// The parts of one restoration plan its fingerprint covers (SPEC 7.4 and
/// 8.6): the operation kind, every context field, the one document path and
/// source revision, and the effective message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreIdentity {
    /// Operation kind, `restore`.
    pub operation: String,
    /// Request identifier.
    pub request_id: String,
    /// Full adopted branch ref.
    pub branch_ref: String,
    /// Scope identity digest.
    pub scope_id: String,
    /// The expected candidate C.
    pub candidate_oid: String,
    /// The one document to restore.
    pub path: String,
    /// The source revision the content comes from.
    pub revision: String,
    /// The effective commit message: the explicit message or the
    /// deterministic default.
    pub message: String,
}

/// The canonical bytes of one restoration plan fingerprint input (SPEC 7.4
/// and 8.6). The plan-fingerprint domain is shared with review plans; the
/// operation kind and the field layout separate the two fingerprints.
pub fn restore_plan_fingerprint_input_bytes(plan: &RestoreIdentity) -> Vec<u8> {
    let mut writer = CanonicalWriter::new(PLAN_FINGERPRINT_DOMAIN);
    writer.text(&plan.operation);
    writer.text(&plan.request_id);
    writer.text(&plan.branch_ref);
    writer.text(&plan.scope_id);
    writer.text(&plan.candidate_oid);
    writer.text(&plan.path);
    writer.text(&plan.revision);
    writer.text(&plan.message);
    writer.finish()
}

/// Why one review plan cannot become a review (SPEC 7.3, 7.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanProblem {
    /// The document is not one JSON object: invalid UTF-8, broken syntax,
    /// trailing content, or another JSON kind.
    NotJson {
        /// The parser's bounded explanation.
        why: String,
    },
    /// A field the contract does not define (SPEC 7.6: inputs are strict).
    UnknownField {
        /// The rejected key.
        field: String,
    },
    /// One key stated twice.
    DuplicateField {
        /// The repeated key.
        field: String,
    },
    /// A required field is absent.
    MissingField {
        /// The absent field.
        field: &'static str,
    },
    /// A field carries the wrong JSON shape.
    WrongShape {
        /// The malformed field.
        field: &'static str,
        /// What the contract expects.
        expected: &'static str,
    },
    /// The document names an unsupported contract version.
    UnsupportedVersion {
        /// The version the document stated.
        observed: u64,
    },
    /// A field value violates its documented form.
    InvalidValue {
        /// The invalid field.
        field: &'static str,
        /// Why the value is rejected.
        why: String,
    },
    /// No decision mode: neither `keep_remaining: true` nor a `keep` array.
    MissingDecision,
    /// Two decision modes at once.
    ConflictingDecision,
    /// One change identity appears twice in one decision array.
    DuplicateDecision {
        /// The repeated identity.
        id: String,
    },
    /// One change identity is both restored and kept.
    OverlappingDecision {
        /// The identity decided twice.
        id: String,
    },
    /// The message violates the shared free-text rule.
    Message {
        /// The broken rule.
        problem: MetadataProblem,
    },
    /// A change identity the pinned review never issued.
    UnknownChange {
        /// The unknown identity.
        id: String,
    },
    /// The explicit `keep` array does not name every other change.
    IncompleteCoverage {
        /// The identities no decision covers, sorted.
        missing: Vec<String>,
    },
}

impl PlanProblem {
    /// The SPEC 7.5 error code of this problem. Every structural fault is
    /// one invalid plan; only a decision against a real review can name an
    /// unknown change.
    pub fn public_code(&self) -> &'static str {
        match self {
            PlanProblem::UnknownChange { .. } => "UNKNOWN_CHANGE",
            _ => "INVALID_PLAN",
        }
    }
}

impl fmt::Display for PlanProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanProblem::NotJson { why } => {
                write!(formatter, "the plan is not one JSON object: {why}")
            }
            PlanProblem::UnknownField { field } => {
                write!(
                    formatter,
                    "the field {field} is not part of the plan contract"
                )
            }
            PlanProblem::DuplicateField { field } => {
                write!(formatter, "the field {field} is stated twice")
            }
            PlanProblem::MissingField { field } => {
                write!(formatter, "the required field {field} is absent")
            }
            PlanProblem::WrongShape { field, expected } => {
                write!(formatter, "the field {field} is not {expected}")
            }
            PlanProblem::UnsupportedVersion { observed } => write!(
                formatter,
                "the plan version {observed} is unsupported; this build reads version {REVIEW_PLAN_VERSION}"
            ),
            PlanProblem::InvalidValue { field, why } => {
                write!(formatter, "the field {field} is invalid: {why}")
            }
            PlanProblem::MissingDecision => write!(
                formatter,
                "the plan states no decision mode: keep_remaining must be true, or keep must list every other change"
            ),
            PlanProblem::ConflictingDecision => write!(
                formatter,
                "the plan states two decision modes: keep_remaining and keep cannot both appear"
            ),
            PlanProblem::DuplicateDecision { id } => {
                write!(formatter, "the change {id} is decided twice in one array")
            }
            PlanProblem::OverlappingDecision { id } => {
                write!(formatter, "the change {id} is both restored and kept")
            }
            PlanProblem::Message { problem } => problem.fmt(formatter),
            PlanProblem::UnknownChange { id } => {
                write!(
                    formatter,
                    "the change {id} is not part of the pinned review"
                )
            }
            PlanProblem::IncompleteCoverage { missing } => {
                let total = missing.len();
                let listed: Vec<&str> = missing
                    .iter()
                    .take(MAX_LISTED_PLAN_CHANGES)
                    .map(String::as_str)
                    .collect();
                let changes = if total == 1 { "change" } else { "changes" };
                if total > MAX_LISTED_PLAN_CHANGES {
                    write!(
                        formatter,
                        "the keep array leaves {total} {changes} undecided, including {} and {} more",
                        listed.join(", "),
                        total - MAX_LISTED_PLAN_CHANGES
                    )
                } else {
                    write!(
                        formatter,
                        "the keep array leaves {total} {changes} undecided: {}",
                        listed.join(", ")
                    )
                }
            }
        }
    }
}

impl std::error::Error for PlanProblem {}

/// Says whether one value is a branch ref of SPEC 7.2: a full `refs/…`
/// name, or the detached fallback `HEAD`, without whitespace or control
/// characters.
pub fn is_branch_ref(value: &str) -> bool {
    if value == "HEAD" {
        return true;
    }
    let Some(name) = value.strip_prefix("refs/") else {
        return false;
    };
    !name.is_empty() && !name.bytes().any(|byte| byte <= b' ' || byte == 0x7f)
}

/// One JSON object read as an ordered field list, so a repeated key stays
/// visible instead of silently overwriting its earlier value (SPEC 7.3
/// rejects duplicate keys).
struct PlanFieldList(Vec<(String, serde_json::Value)>);

impl<'de> Deserialize<'de> for PlanFieldList {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct PlanFieldVisitor;

        impl<'de> serde::de::Visitor<'de> for PlanFieldVisitor {
            type Value = PlanFieldList;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("one JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut fields = Vec::with_capacity(map.size_hint().unwrap_or(0).min(16));
                while let Some((key, value)) = map.next_entry::<String, serde_json::Value>()? {
                    fields.push((key, value));
                }
                Ok(PlanFieldList(fields))
            }
        }

        deserializer.deserialize_map(PlanFieldVisitor)
    }
}

/// Takes one required field out of the collected values.
fn plan_present(
    values: &mut BTreeMap<&'static str, serde_json::Value>,
    field: &'static str,
) -> Result<serde_json::Value, PlanProblem> {
    values
        .remove(field)
        .ok_or(PlanProblem::MissingField { field })
}

/// Decodes one required JSON string field.
fn plan_string(field: &'static str, value: serde_json::Value) -> Result<String, PlanProblem> {
    match value {
        serde_json::Value::String(text) => Ok(text),
        _ => Err(PlanProblem::WrongShape {
            field,
            expected: "a JSON string",
        }),
    }
}

/// Decodes one optional JSON string field. An explicit `null` is a wrong
/// shape, because inputs are strict about presence (SPEC 7.6).
fn plan_optional_string(
    field: &'static str,
    value: Option<serde_json::Value>,
) -> Result<Option<String>, PlanProblem> {
    match value {
        None => Ok(None),
        Some(serde_json::Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(PlanProblem::WrongShape {
            field,
            expected: "a JSON string",
        }),
    }
}

/// Decodes one required JSON number field.
fn plan_number(field: &'static str, value: serde_json::Value) -> Result<u64, PlanProblem> {
    match value {
        serde_json::Value::Number(number) => number.as_u64().ok_or(PlanProblem::WrongShape {
            field,
            expected: "a nonnegative integer",
        }),
        _ => Err(PlanProblem::WrongShape {
            field,
            expected: "a nonnegative integer",
        }),
    }
}

/// Decodes one optional JSON boolean field.
fn plan_bool(
    field: &'static str,
    value: Option<serde_json::Value>,
) -> Result<Option<bool>, PlanProblem> {
    match value {
        None => Ok(None),
        Some(serde_json::Value::Bool(flag)) => Ok(Some(flag)),
        Some(_) => Err(PlanProblem::WrongShape {
            field,
            expected: "a JSON boolean",
        }),
    }
}

/// Decodes one JSON array of strings.
fn plan_string_array(
    field: &'static str,
    value: serde_json::Value,
) -> Result<Vec<String>, PlanProblem> {
    match value {
        serde_json::Value::Array(items) => {
            let mut ids = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    serde_json::Value::String(text) => ids.push(text),
                    _ => {
                        return Err(PlanProblem::WrongShape {
                            field,
                            expected: "an array of JSON strings",
                        });
                    }
                }
            }
            Ok(ids)
        }
        _ => Err(PlanProblem::WrongShape {
            field,
            expected: "an array of JSON strings",
        }),
    }
}

/// Checks that every identity of one decision array is a full identity
/// string (SPEC 7.3).
fn plan_decision_ids(field: &'static str, ids: &[String]) -> Result<(), PlanProblem> {
    for id in ids {
        if !is_fingerprint_id(id) {
            return Err(PlanProblem::InvalidValue {
                field,
                why: format!("the change identity {id} is not a full Margin identity"),
            });
        }
    }
    Ok(())
}

/// Sorts one decision array and refuses a repeated identity (SPEC 7.3).
fn plan_unique(mut ids: Vec<String>) -> Result<Vec<String>, PlanProblem> {
    ids.sort();
    for pair in ids.windows(2) {
        if pair[0] == pair[1] {
            return Err(PlanProblem::DuplicateDecision {
                id: pair[0].clone(),
            });
        }
    }
    Ok(ids)
}

/// The JSON Schema of one review plan document (SPEC 7.3), generated from
/// the same field list the strict parser reads.
///
/// The schema ships for callers that author plans. It documents the input
/// contract; it is never a second validator. Margin validates every plan
/// through [`ReviewPlan::parse`] alone, and the release tests fail when the
/// shipped schema and the parser disagree (SPEC 7.3, 13.6).
pub fn review_plan_json_schema() -> String {
    let identity = serde_json::json!({
        "type": "string",
        "pattern": "^sha1:[0-9a-f]{40}$|^sha256:[0-9a-f]{64}$",
    });
    let oid = serde_json::json!({
        "type": "string",
        "pattern": "^[0-9a-f]{40}$|^[0-9a-f]{64}$",
    });
    let change = serde_json::json!({
        "type": "string",
        "pattern": "^sha1:[0-9a-f]{40}$|^sha256:[0-9a-f]{64}$",
    });
    let schema = serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://margin.invalid/schemas/review-plan-v1.json",
        "title": "Margin review plan v1",
        "description": "One complete decision set over a pinned Margin review. Inputs are strict: unknown fields, duplicate keys, invalid identifiers, missing or doubled decision modes, and incomplete coverage all fail validation.",
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "schema_version": { "const": REVIEW_PLAN_VERSION },
            "request_id": {
                "type": "string",
                "minLength": 1,
                "maxLength": 128,
                "pattern": "^[A-Za-z0-9._:-]+$"
            },
            "review_id": identity.clone(),
            "branch_ref": {
                "type": "string",
                "pattern": "^(refs/[^\\u0000-\\u0020\\u007f]+|HEAD)$"
            },
            "scope_id": identity,
            "baseline_oid": oid.clone(),
            "candidate_oid": oid,
            "restore": { "type": "array", "items": change.clone(), "uniqueItems": true },
            "keep_remaining": { "type": "boolean" },
            "keep": { "type": "array", "items": change, "uniqueItems": true },
            "message": { "type": "string", "maxLength": MAX_MESSAGE_BYTES }
        },
        "required": REVIEW_PLAN_REQUIRED_FIELDS,
        "oneOf": [
            {
                "required": ["keep_remaining"],
                "properties": { "keep_remaining": { "const": true } },
                "not": { "required": ["keep"] }
            },
            {
                "required": ["keep"],
                "properties": { "keep_remaining": { "const": false } }
            }
        ]
    });
    serde_json::to_string_pretty(&schema).expect("the schema serializes")
}

// ---------------------------------------------------------------------------
// Canonical source changes (SPEC 10.1)
// ---------------------------------------------------------------------------

/// The presence transition of one scoped path between B and C (SPEC 3.2
/// rule 6 and 10.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    /// The path exists in C and not in B: a whole-file addition. Rename
    /// detection stays off, so a renamed file appears as one deletion plus
    /// one addition.
    Added,
    /// The path exists in B and not in C: a whole-file deletion.
    Deleted,
    /// The path exists in both revisions with different content or mode.
    Modified,
}

impl ChangeKind {
    /// The wire name of the kind in review results (SPEC 7.2).
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeKind::Added => "added",
            ChangeKind::Deleted => "deleted",
            ChangeKind::Modified => "modified",
        }
    }
}

/// One complete hunk of a source patch (SPEC 10.1): the original hunk
/// coordinates of both revisions.
///
/// The ranges are the `@@`-header coordinates of Git's unified diff. A start
/// of zero with a count of zero marks an empty side, as in `@@ -0,0 +1 @@`
/// for a new file. Presentation derives positions from these ranges without
/// rewriting the patch bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceHunk {
    /// First line of the old side, one-based; zero when that side is empty.
    pub old_start: u64,
    /// Number of lines the hunk removes from the old side.
    pub old_count: u64,
    /// First line of the new side, one-based; zero when that side is empty.
    pub new_start: u64,
    /// Number of lines the hunk adds to the new side.
    pub new_count: u64,
    /// Byte offset of the `@@` line inside the file patch.
    pub offset: usize,
    /// Length in bytes of the complete hunk: the `@@` line through the last
    /// body line, including any `\\ No newline at end of file` marker.
    pub length: usize,
}

/// The complete canonical patch of one changed file (SPEC 10.1): the exact
/// bytes Git emitted under the fixed diff contract, with the structure every
/// later stage derives from it.
///
/// `bytes` is the ledger. It holds the original complete file header and
/// every complete hunk or binary block, unchanged. The other fields are
/// views into that ledger, so counts, presentation, plan validation, and
/// mutation all derive from one model instead of a second diff (SPEC 13.6).
/// Storage modes and object identifiers come from the immutable tree
/// entries of the comparison (SPEC 10.2 step 1), not from this patch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourcePatch {
    /// The presence transition stated by the patch header.
    pub kind: ChangeKind,
    /// Whether the content change is opaque: Git emitted a binary patch
    /// instead of readable hunks (SPEC 3.3 and 10.4). Opaque changes are
    /// whole-file decisions.
    pub opaque: bool,
    /// Every complete hunk in patch order. Mode-only and opaque changes
    /// hold none.
    pub hunks: Vec<SourceHunk>,
    /// The complete original patch bytes of this file, from its `diff --git`
    /// header line through the byte before the next file's header.
    pub bytes: Vec<u8>,
}

/// Why one patch stream or section cannot join the canonical change model
/// (SPEC 10.1). Every variant is a contract violation: the fixed diff
/// settings make anything else impossible, so the caller refuses the
/// manifest instead of guessing (SPEC 7.5 `CONTRACT_VIOLATION`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourcePatchError {
    /// The stream holds content before its first `diff --git` line.
    LeadingContent {
        /// The offending line.
        line: Vec<u8>,
    },
    /// A section is empty or does not start with a `diff --git` header.
    MissingHeader,
    /// A structural header line does not belong to the fixed contract,
    /// such as a rename or copy record that disabled rename detection
    /// excludes, or a line that follows a completed hunk.
    UnexpectedHeader {
        /// The offending line.
        line: Vec<u8>,
    },
    /// A mode line did not name one of the two regular file modes.
    UnsupportedMode {
        /// The offending line.
        line: Vec<u8>,
    },
    /// A `@@` hunk header could not be parsed.
    MalformedHunkHeader {
        /// The offending line.
        line: Vec<u8>,
    },
    /// A hunk body ended before its declared line counts were satisfied,
    /// or held a line that is not a hunk body line.
    IncompleteHunk {
        /// The offending line.
        line: Vec<u8>,
    },
    /// A `GIT binary patch` region did not hold a `literal` or `delta`
    /// payload block.
    MalformedBinary {
        /// The offending line.
        line: Vec<u8>,
    },
}

impl SourcePatchError {
    /// The SPEC 7.5 error code of this failure.
    pub fn public_code(&self) -> &'static str {
        "CONTRACT_VIOLATION"
    }
}

impl fmt::Display for SourcePatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourcePatchError::LeadingContent { line } => write!(
                formatter,
                "content before the first diff --git line: {:?}",
                String::from_utf8_lossy(line)
            ),
            SourcePatchError::MissingHeader => {
                write!(formatter, "a patch section has no diff --git header")
            }
            SourcePatchError::UnexpectedHeader { line } if line.is_empty() => write!(
                formatter,
                "a structural line outside the fixed diff contract ended the section"
            ),
            SourcePatchError::UnexpectedHeader { line } => write!(
                formatter,
                "a structural line outside the fixed diff contract: {:?}",
                String::from_utf8_lossy(line)
            ),
            SourcePatchError::UnsupportedMode { line } => write!(
                formatter,
                "a mode line outside the two regular file modes: {:?}",
                String::from_utf8_lossy(line)
            ),
            SourcePatchError::MalformedHunkHeader { line } => write!(
                formatter,
                "an unparsable @@ hunk header: {:?}",
                String::from_utf8_lossy(line)
            ),
            SourcePatchError::IncompleteHunk { line } if line.is_empty() => write!(
                formatter,
                "a hunk body ended before its declared line counts were satisfied"
            ),
            SourcePatchError::IncompleteHunk { line } => write!(
                formatter,
                "a hunk body disagrees with its declared counts at {:?}",
                String::from_utf8_lossy(line)
            ),
            SourcePatchError::MalformedBinary { line } if line.is_empty() => write!(
                formatter,
                "a binary patch ended without a literal or delta block"
            ),
            SourcePatchError::MalformedBinary { line } => write!(
                formatter,
                "a binary patch without a literal or delta block at {:?}",
                String::from_utf8_lossy(line)
            ),
        }
    }
}

impl std::error::Error for SourcePatchError {}

/// The exact `diff --git` header line Git emits for `path` under the fixed
/// diff contract (SPEC 10.1): `diff --git a/<path> b/<path>`, where each
/// combined prefix and path is one C-quoted token when the path holds a
/// double quote, a backslash, or a control byte. Other bytes, including
/// non-ASCII UTF-8, stay literal because the contract pins
/// `core.quotePath=false`.
///
/// Margin knows every changed path before it parses a stream, so it
/// associates each section by comparing the section's first line with this
/// exact line. That keeps odd names literal data instead of re-deriving a
/// path from the header (SPEC 3.3 and 14.1).
pub fn expected_diff_header(path: &str) -> Vec<u8> {
    let mut line = Vec::with_capacity(path.len() + 24);
    line.extend_from_slice(b"diff --git ");
    push_prefixed_name(b"a/", path, &mut line);
    line.push(b' ');
    push_prefixed_name(b"b/", path, &mut line);
    line
}

/// Writes `prefix` plus `path` as one token, C-quoted as a whole when Git
/// would quote that combined name.
fn push_prefixed_name(prefix: &[u8], path: &str, out: &mut Vec<u8>) {
    let mut combined = Vec::with_capacity(prefix.len() + path.len());
    combined.extend_from_slice(prefix);
    combined.extend_from_slice(path.as_bytes());
    if needs_c_quoting(&combined) {
        push_c_quoted(&combined, out);
    } else {
        out.extend_from_slice(&combined);
    }
}

/// Says whether Git C-quotes `bytes`: quoting is needed for a double quote,
/// a backslash, or any control byte. With `core.quotePath=false`, bytes
/// above 0x7f stay literal.
fn needs_c_quoting(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .any(|&byte| matches!(byte, b'"' | b'\\') || byte < 0x20 || byte == 0x7f)
}

/// Writes `bytes` in Git's C-quoted form: one double-quoted token with the
/// standard escapes and three-digit octal for the remaining control bytes.
fn push_c_quoted(bytes: &[u8], out: &mut Vec<u8>) {
    out.push(b'"');
    for &byte in bytes {
        match byte {
            0x07 => out.extend_from_slice(b"\\a"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x0b => out.extend_from_slice(b"\\v"),
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            byte if byte < 0x20 || byte == 0x7f => {
                out.extend_from_slice(format!("\\{byte:03o}").as_bytes());
            }
            byte => out.push(byte),
        }
    }
    out.push(b'"');
}

/// Yields every line of a byte string with its offset and without its
/// trailing newline. The final line may lack a newline.
struct Lines<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> Lines<'a> {
    /// Prepares iteration over `data`.
    fn new(data: &'a [u8]) -> Self {
        Lines { data, offset: 0 }
    }
}

impl<'a> Iterator for Lines<'a> {
    type Item = (usize, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.data.len() {
            return None;
        }
        let start = self.offset;
        match self.data[start..].iter().position(|&byte| byte == b'\n') {
            Some(position) => {
                self.offset = start + position + 1;
                Some((start, &self.data[start..start + position]))
            }
            None => {
                self.offset = self.data.len();
                Some((start, &self.data[start..]))
            }
        }
    }
}

/// Splits one patch stream into its per-file sections (SPEC 10.1). A
/// section starts at a `diff --git a/` or `diff --git "a/` line and runs to
/// the next section or the end of the stream, so each returned slice holds
/// one file's complete patch bytes.
///
/// No hunk or binary line can forge that prefix: a hunk body line starts
/// with a space, `+`, `-`, or `\`, and a binary payload line cannot hold
/// the spaces, slashes, or quote the prefix needs.
pub fn split_patch_stream(stream: &[u8]) -> Result<Vec<&[u8]>, SourcePatchError> {
    let mut starts: Vec<usize> = Vec::new();
    for (offset, line) in Lines::new(stream) {
        if is_diff_git_header(line) {
            starts.push(offset);
        } else if starts.is_empty() {
            return Err(SourcePatchError::LeadingContent {
                line: line.to_vec(),
            });
        }
    }
    let mut sections = Vec::with_capacity(starts.len());
    for (index, start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(stream.len());
        sections.push(&stream[*start..end]);
    }
    Ok(sections)
}

/// Says whether `line` is the `diff --git` header of a section. The fixed
/// `a/` source prefix appears literally or opens a quoted name.
fn is_diff_git_header(line: &[u8]) -> bool {
    line.starts_with(b"diff --git a/") || line.starts_with(b"diff --git \"a/")
}

/// The state between the structural regions of one file patch.
enum PatchRegion {
    /// Before the first hunk: mode, index, and `---`/`+++` lines.
    Header,
    /// Inside the unified-diff hunks.
    Hunks,
    /// At a binary block header: `literal <n>` or `delta <n>`.
    BinaryHeader,
    /// Inside one binary block's payload.
    BinaryData,
}

/// One hunk under construction.
struct OpenHunk {
    old_start: u64,
    old_count: u64,
    new_start: u64,
    new_count: u64,
    old_remaining: u64,
    new_remaining: u64,
    offset: usize,
    /// Offset one past the last byte consumed for this hunk so far.
    end: usize,
}

impl OpenHunk {
    /// The completed hunk.
    fn finish(self) -> SourceHunk {
        SourceHunk {
            old_start: self.old_start,
            old_count: self.old_count,
            new_start: self.new_start,
            new_count: self.new_count,
            offset: self.offset,
            length: self.end - self.offset,
        }
    }
}

/// Parses one per-file section of the fixed diff contract into the
/// canonical change model (SPEC 10.1).
///
/// The parser is strict on purpose. The fixed engine settings make every
/// documented shape the only possibility, so any other byte sequence is a
/// contract violation the caller reports instead of a guess. It validates
/// that every mode line names a regular file, that each hunk body supplies
/// exactly the line counts its `@@` header declares, and that a binary
/// patch holds at least one payload block. Hunk bytes, whitespace, final
/// newlines, and binary payloads stay verbatim in [`SourcePatch::bytes`].
pub fn parse_source_patch(section: &[u8]) -> Result<SourcePatch, SourcePatchError> {
    let mut lines = Lines::new(section);
    let Some((_, header)) = lines.next() else {
        return Err(SourcePatchError::MissingHeader);
    };
    if !is_diff_git_header(header) {
        return Err(SourcePatchError::MissingHeader);
    }

    let mut kind: Option<ChangeKind> = None;
    let mut hunks: Vec<SourceHunk> = Vec::new();
    let mut open: Option<OpenHunk> = None;
    let mut opaque = false;
    let mut binary_block_seen = false;
    let mut region = PatchRegion::Header;

    for (offset, line) in lines {
        // The end of the line's bytes, including its newline when present.
        let line_end =
            offset + line.len() + usize::from(section.get(offset + line.len()) == Some(&b'\n'));
        match region {
            PatchRegion::Header => match header_line_kind(line) {
                HeaderLine::OldMode => assign_mode(line, b"old mode ")?,
                HeaderLine::NewMode => assign_mode(line, b"new mode ")?,
                HeaderLine::DeletedFileMode => {
                    set_kind(kind, ChangeKind::Deleted, line)?;
                    kind = Some(ChangeKind::Deleted);
                    assign_mode(line, b"deleted file mode ")?;
                }
                HeaderLine::NewFileMode => {
                    set_kind(kind, ChangeKind::Added, line)?;
                    kind = Some(ChangeKind::Added);
                    assign_mode(line, b"new file mode ")?;
                }
                HeaderLine::Index => validate_index_line(line)?,
                HeaderLine::Names => {}
                HeaderLine::Binary => {
                    opaque = true;
                    region = PatchRegion::BinaryHeader;
                }
                HeaderLine::Hunk => {
                    let parsed = parse_hunk_header(line)?;
                    open = Some(open_hunk(parsed, offset, line_end));
                    region = PatchRegion::Hunks;
                }
                HeaderLine::Unexpected => {
                    return Err(SourcePatchError::UnexpectedHeader {
                        line: line.to_vec(),
                    });
                }
            },
            PatchRegion::Hunks => {
                let counts_pending = open
                    .as_ref()
                    .is_some_and(|hunk| hunk.old_remaining > 0 || hunk.new_remaining > 0);
                if counts_pending || line.first() == Some(&b'\\') {
                    let hunk = open.as_mut().expect("a body line implies an open hunk");
                    match line.first() {
                        Some(b' ') => {
                            hunk.old_remaining = hunk.old_remaining.saturating_sub(1);
                            hunk.new_remaining = hunk.new_remaining.saturating_sub(1);
                        }
                        Some(b'-') => hunk.old_remaining = hunk.old_remaining.saturating_sub(1),
                        Some(b'+') => hunk.new_remaining = hunk.new_remaining.saturating_sub(1),
                        Some(b'\\') => {}
                        _ => {
                            return Err(SourcePatchError::IncompleteHunk {
                                line: line.to_vec(),
                            });
                        }
                    }
                    hunk.end = line_end;
                } else {
                    let finished = open.take().expect("the hunks region implies an open hunk");
                    hunks.push(finished.finish());
                    if line.starts_with(b"@@ -") {
                        let parsed = parse_hunk_header(line)?;
                        open = Some(open_hunk(parsed, offset, line_end));
                    } else {
                        return Err(SourcePatchError::UnexpectedHeader {
                            line: line.to_vec(),
                        });
                    }
                }
            }
            PatchRegion::BinaryHeader => {
                if line.starts_with(b"literal ") || line.starts_with(b"delta ") {
                    binary_block_seen = true;
                    region = PatchRegion::BinaryData;
                } else {
                    return Err(SourcePatchError::MalformedBinary {
                        line: line.to_vec(),
                    });
                }
            }
            PatchRegion::BinaryData => {
                // A payload line belongs to the open block; an empty line
                // closes it, and another block header may follow.
                if line.is_empty() {
                    region = PatchRegion::BinaryHeader;
                }
            }
        }
    }

    if let Some(hunk) = open {
        if hunk.old_remaining > 0 || hunk.new_remaining > 0 {
            return Err(SourcePatchError::IncompleteHunk { line: Vec::new() });
        }
        hunks.push(hunk.finish());
    }
    if matches!(region, PatchRegion::BinaryHeader) && !binary_block_seen {
        return Err(SourcePatchError::MalformedBinary { line: Vec::new() });
    }

    Ok(SourcePatch {
        kind: kind.unwrap_or(ChangeKind::Modified),
        opaque,
        hunks,
        bytes: section.to_vec(),
    })
}

/// Opens one hunk from its parsed header at `offset`.
fn open_hunk(parsed: (u64, u64, u64, u64), offset: usize, end: usize) -> OpenHunk {
    OpenHunk {
        old_start: parsed.0,
        old_count: parsed.1,
        new_start: parsed.2,
        new_count: parsed.3,
        old_remaining: parsed.1,
        new_remaining: parsed.3,
        offset,
        end,
    }
}

/// The structural header lines of the fixed contract (SPEC 10.1).
enum HeaderLine {
    OldMode,
    NewMode,
    DeletedFileMode,
    NewFileMode,
    Index,
    Names,
    Binary,
    Hunk,
    Unexpected,
}

/// Classifies one line of the header region.
fn header_line_kind(line: &[u8]) -> HeaderLine {
    if line.starts_with(b"old mode ") {
        HeaderLine::OldMode
    } else if line.starts_with(b"new mode ") {
        HeaderLine::NewMode
    } else if line.starts_with(b"deleted file mode ") {
        HeaderLine::DeletedFileMode
    } else if line.starts_with(b"new file mode ") {
        HeaderLine::NewFileMode
    } else if line.starts_with(b"index ") {
        HeaderLine::Index
    } else if line.starts_with(b"--- ") || line.starts_with(b"+++ ") {
        HeaderLine::Names
    } else if line == b"GIT binary patch" {
        HeaderLine::Binary
    } else if line.starts_with(b"@@ -") {
        HeaderLine::Hunk
    } else {
        HeaderLine::Unexpected
    }
}

/// Records one mode line, refusing every mode that is not a regular file.
fn assign_mode(line: &[u8], prefix: &[u8]) -> Result<(), SourcePatchError> {
    let raw = line
        .strip_prefix(prefix)
        .expect("the classifier matched the prefix");
    if parse_entry_mode(raw).is_none() {
        return Err(SourcePatchError::UnsupportedMode {
            line: line.to_vec(),
        });
    }
    Ok(())
}

/// Parses one mode field of a patch line. The two regular file modes are
/// ASCII, so this is the byte form of [`EntryMode::parse`].
fn parse_entry_mode(bytes: &[u8]) -> Option<EntryMode> {
    EntryMode::parse(std::str::from_utf8(bytes).ok()?)
}

/// Accepts one presence transition, refusing contradictory headers.
fn set_kind(
    current: Option<ChangeKind>,
    next: ChangeKind,
    line: &[u8],
) -> Result<(), SourcePatchError> {
    if current.is_some() && current != Some(next) {
        return Err(SourcePatchError::UnexpectedHeader {
            line: line.to_vec(),
        });
    }
    Ok(())
}

/// Validates one `index <oid>..<oid>[ <mode>]` line. The object
/// identifiers stay in the patch bytes verbatim; only the optional mode
/// suffix must name a regular file.
fn validate_index_line(line: &[u8]) -> Result<(), SourcePatchError> {
    let rest = line
        .strip_prefix(b"index ")
        .expect("the classifier matched the prefix");
    let Some((_, tail)) = split_once_byte(rest, b'.') else {
        return Err(SourcePatchError::UnexpectedHeader {
            line: line.to_vec(),
        });
    };
    if !tail.starts_with(b".") {
        return Err(SourcePatchError::UnexpectedHeader {
            line: line.to_vec(),
        });
    }
    if let Some((_, mode)) = split_once_byte(rest, b' ')
        && parse_entry_mode(mode).is_none()
    {
        return Err(SourcePatchError::UnsupportedMode {
            line: line.to_vec(),
        });
    }
    Ok(())
}

/// Splits `bytes` at its first `separator`, without the separator.
fn split_once_byte(bytes: &[u8], separator: u8) -> Option<(&[u8], &[u8])> {
    bytes
        .iter()
        .position(|&byte| byte == separator)
        .map(|position| (&bytes[..position], &bytes[position + 1..]))
}

/// Parses one `@@ -<start>[,<count>] +<start>[,<count>] @@` hunk header.
fn parse_hunk_header(line: &[u8]) -> Result<(u64, u64, u64, u64), SourcePatchError> {
    let error = || SourcePatchError::MalformedHunkHeader {
        line: line.to_vec(),
    };
    let Some(rest) = line.strip_prefix(b"@@ -") else {
        return Err(error());
    };
    let (old_start, old_count, rest) = parse_hunk_range(rest).ok_or_else(error)?;
    let Some(rest) = rest.strip_prefix(b" +") else {
        return Err(error());
    };
    let (new_start, new_count, rest) = parse_hunk_range(rest).ok_or_else(error)?;
    if !rest.starts_with(b" @@") {
        return Err(error());
    }
    Ok((old_start, old_count, new_start, new_count))
}

/// Parses `<start>[,<count>]`, returning the range and the unread bytes.
/// A missing count means one line.
fn parse_hunk_range(bytes: &[u8]) -> Option<(u64, u64, &[u8])> {
    let start_end = digits_end(bytes)?;
    let start = parse_digits(&bytes[..start_end])?;
    if bytes.get(start_end) == Some(&b',') {
        let count_end = digits_end(&bytes[start_end + 1..])? + start_end + 1;
        let count = parse_digits(&bytes[start_end + 1..count_end])?;
        Some((start, count, &bytes[count_end..]))
    } else {
        Some((start, 1, &bytes[start_end..]))
    }
}

/// The offset one past the leading ASCII digits of `bytes`.
fn digits_end(bytes: &[u8]) -> Option<usize> {
    let end = bytes
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(bytes.len());
    (end > 0).then_some(end)
}

/// Parses ASCII digits as one `u64`.
fn parse_digits(bytes: &[u8]) -> Option<u64> {
    bytes.iter().try_fold(0u64, |value, &digit| {
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(digit - b'0')))
    })
}

// ---------------------------------------------------------------------------
// Local settings (SPEC 8.2, 6.2)
// ---------------------------------------------------------------------------

/// Version of the local settings contract (SPEC 8.2 `margin.version`).
pub const SETTINGS_VERSION: u32 = 1;

/// `margin.version`: the settings contract the stored values follow.
pub const SETTINGS_KEY_VERSION: &str = "margin.version";
/// `margin.branch`: the adopted branch.
pub const SETTINGS_KEY_BRANCH: &str = "margin.branch";
/// `margin.scope`: the canonical scope descriptor in its documented JSON
/// form.
pub const SETTINGS_KEY_SCOPE: &str = "margin.scope";
/// `margin.autoCheckpoint`: whether configured capture boundaries
/// checkpoint automatically.
pub const SETTINGS_KEY_AUTO_CHECKPOINT: &str = "margin.autoCheckpoint";
/// `margin.captureAfterEdit`: whether the configured editor exiting
/// captures a draft.
pub const SETTINGS_KEY_CAPTURE_AFTER_EDIT: &str = "margin.captureAfterEdit";
/// `margin.editorArgv`: the editor argv in its documented JSON form.
pub const SETTINGS_KEY_EDITOR_ARGV: &str = "margin.editorArgv";

/// Largest encoded value one setting may hold. Every stored value is one
/// bounded line, so reading a configuration file back can never turn into
/// an unbounded parse.
pub const MAX_SETTING_BYTES: usize = 4 * 1024;

/// Largest excerpt of a rejected value one error message echoes.
const EXCERPT_CHARS: usize = 48;

/// The effective local Margin settings of one repository (SPEC 8.2).
///
/// Reading fills each field from its documented `margin.*` key. Both
/// capture policies are off until a stored value turns them on, so first
/// use captures nothing on its own (SPEC 6.2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalSettings {
    /// The stored settings contract version; `None` before the first write.
    pub version: Option<u32>,
    /// The adopted branch; `None` until initialization records one.
    pub branch: Option<String>,
    /// The adopted canonical scope descriptor.
    pub scope: Option<ScopeDescriptor>,
    /// Whether configured capture boundaries checkpoint automatically.
    pub auto_checkpoint: bool,
    /// Whether the configured editor exiting captures a draft.
    pub capture_after_edit: bool,
    /// The configured editor argv; `None` until one is set explicitly.
    pub editor_argv: Option<Vec<String>>,
}

impl LocalSettings {
    /// The settings of a repository Margin never wrote to: every stored
    /// value absent and both capture policies off (SPEC 6.2).
    pub fn first_use() -> LocalSettings {
        LocalSettings::default()
    }
}

/// Explicit local-settings changes one interface requests (SPEC 6.2, 8.2).
///
/// Every field is optional and `None` means "not supplied": an omitted
/// setting keeps its stored value, so a repeated initialization changes
/// only the settings it names. Opposing flags conflict at the interface,
/// never here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingsUpdate {
    /// The adopted branch.
    pub branch: Option<String>,
    /// The adopted canonical scope descriptor.
    pub scope: Option<ScopeDescriptor>,
    /// Whether configured capture boundaries checkpoint automatically.
    pub auto_checkpoint: Option<bool>,
    /// Whether the configured editor exiting captures a draft.
    pub capture_after_edit: Option<bool>,
    /// The configured editor argv.
    pub editor_argv: Option<Vec<String>>,
}

impl SettingsUpdate {
    /// Says whether the update names no setting at all. Such an update is
    /// one explicit no-op: nothing is written, not even the version stamp.
    pub fn is_empty(&self) -> bool {
        self.branch.is_none()
            && self.scope.is_none()
            && self.auto_checkpoint.is_none()
            && self.capture_after_edit.is_none()
            && self.editor_argv.is_none()
    }
}

/// Why one settings value does not match its documented form (SPEC 8.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsValueError {
    /// The value is not the documented form of the key.
    Malformed {
        /// The documented key the value belongs to.
        key: &'static str,
        /// Why the value was rejected.
        why: String,
    },
    /// `margin.version` holds a settings contract this build cannot
    /// interpret, such as one written by a newer Margin.
    UnsupportedVersion {
        /// The bounded stored value.
        value: String,
    },
}

impl SettingsValueError {
    /// One `Malformed` rejection.
    fn malformed(key: &'static str, why: impl Into<String>) -> SettingsValueError {
        SettingsValueError::Malformed {
            key,
            why: why.into(),
        }
    }
}

impl fmt::Display for SettingsValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SettingsValueError::Malformed { key, why } => {
                write!(formatter, "the {key} value is invalid: {why}")
            }
            SettingsValueError::UnsupportedVersion { value } => write!(
                formatter,
                "margin.version holds {value:?}, a settings contract this build cannot interpret"
            ),
        }
    }
}

impl std::error::Error for SettingsValueError {}

/// Renders one bounded excerpt of a rejected value.
fn excerpt(raw: &str) -> String {
    let mut text: String = raw.chars().take(EXCERPT_CHARS).collect();
    if raw.chars().count() > EXCERPT_CHARS {
        text.push('…');
    }
    text
}

/// Rejects a value that leaves the bounded form every setting uses.
fn require_bounded(key: &'static str, raw: &str) -> Result<(), SettingsValueError> {
    if raw.len() > MAX_SETTING_BYTES {
        return Err(SettingsValueError::malformed(
            key,
            format!(
                "the value is {} bytes, more than the {}-byte setting limit",
                raw.len(),
                MAX_SETTING_BYTES
            ),
        ));
    }
    Ok(())
}

/// Returns one bounded value unchanged.
fn bounded_setting(key: &'static str, value: &str) -> Result<String, SettingsValueError> {
    require_bounded(key, value)?;
    Ok(value.to_string())
}

/// Encodes one branch name into its stored form. A branch stays one line:
/// an empty name or one with a control character is rejected.
pub fn setting_branch_value(branch: &str) -> Result<String, SettingsValueError> {
    if branch.is_empty() {
        return Err(SettingsValueError::malformed(
            SETTINGS_KEY_BRANCH,
            "the branch name is empty",
        ));
    }
    if branch.chars().any(char::is_control) {
        return Err(SettingsValueError::malformed(
            SETTINGS_KEY_BRANCH,
            format!(
                "the branch name {:?} contains a control character",
                excerpt(branch)
            ),
        ));
    }
    bounded_setting(SETTINGS_KEY_BRANCH, branch)
}

/// Parses the stored `margin.branch` value.
pub fn parse_setting_branch(raw: &str) -> Result<String, SettingsValueError> {
    setting_branch_value(raw)
}

/// Encodes one capture-policy flag into its stored form.
pub fn setting_flag_value(enabled: bool) -> &'static str {
    if enabled { "true" } else { "false" }
}

/// Parses one stored capture-policy flag. Only the two documented spellings
/// are accepted; Git's wider boolean vocabulary is not part of the
/// documented form (SPEC 8.2).
pub fn parse_setting_flag(key: &'static str, raw: &str) -> Result<bool, SettingsValueError> {
    match raw {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(SettingsValueError::malformed(
            key,
            format!(
                "{:?} is not one of the documented spellings true or false",
                excerpt(other)
            ),
        )),
    }
}

/// Encodes one canonical scope descriptor into its documented JSON form
/// (SPEC 8.2): the same canonical single line the `Margin-Scope-Spec`
/// trailer carries. A descriptor outside release 0.1, or one not in
/// canonical form, is rejected rather than stored in a form a later read
/// cannot reproduce.
pub fn setting_scope_value(descriptor: &ScopeDescriptor) -> Result<String, SettingsValueError> {
    if !descriptor.is_supported() {
        return Err(SettingsValueError::malformed(
            SETTINGS_KEY_SCOPE,
            "the descriptor is not a release 0.1 scope",
        ));
    }
    if descriptor.recanonicalized().as_ref() != Some(descriptor) {
        return Err(SettingsValueError::malformed(
            SETTINGS_KEY_SCOPE,
            "the descriptor is not in canonical form",
        ));
    }
    let value = String::from_utf8(descriptor.canonical_bytes())
        .expect("the canonical descriptor is UTF-8 JSON");
    bounded_setting(SETTINGS_KEY_SCOPE, &value)
}

/// Parses the stored `margin.scope` value into its descriptor. The value
/// must be exactly the canonical form: it parses strictly, describes a
/// release 0.1 scope, and equals its own canonical bytes.
pub fn parse_setting_scope(raw: &str) -> Result<ScopeDescriptor, SettingsValueError> {
    require_bounded(SETTINGS_KEY_SCOPE, raw)?;
    let descriptor: ScopeDescriptor = serde_json::from_str(raw).map_err(|error| {
        SettingsValueError::malformed(
            SETTINGS_KEY_SCOPE,
            format!("the value is not a scope descriptor: {error}"),
        )
    })?;
    if !descriptor.is_supported() {
        return Err(SettingsValueError::malformed(
            SETTINGS_KEY_SCOPE,
            "the descriptor is not a release 0.1 scope",
        ));
    }
    if descriptor.canonical_bytes() != raw.as_bytes() {
        return Err(SettingsValueError::malformed(
            SETTINGS_KEY_SCOPE,
            "the value is not the canonical descriptor form",
        ));
    }
    Ok(descriptor)
}

/// Encodes one editor argv into its documented JSON form (SPEC 8.2, 12.5):
/// a JSON array of strings holding the executable and its arguments. An
/// argv needs at least the executable.
pub fn setting_argv_value(argv: &[String]) -> Result<String, SettingsValueError> {
    if argv.is_empty() {
        return Err(SettingsValueError::malformed(
            SETTINGS_KEY_EDITOR_ARGV,
            "the editor argv is empty; it needs at least the executable",
        ));
    }
    let value = serde_json::to_string(argv).expect("a string array serializes");
    bounded_setting(SETTINGS_KEY_EDITOR_ARGV, &value)
}

/// Parses the stored `margin.editorArgv` value.
pub fn parse_setting_argv(raw: &str) -> Result<Vec<String>, SettingsValueError> {
    require_bounded(SETTINGS_KEY_EDITOR_ARGV, raw)?;
    let argv: Vec<String> = serde_json::from_str(raw).map_err(|error| {
        SettingsValueError::malformed(
            SETTINGS_KEY_EDITOR_ARGV,
            format!("the value is not a JSON array of strings: {error}"),
        )
    })?;
    if argv.is_empty() {
        return Err(SettingsValueError::malformed(
            SETTINGS_KEY_EDITOR_ARGV,
            "the editor argv is empty; it needs at least the executable",
        ));
    }
    let canonical = serde_json::to_string(&argv).expect("a string array serializes");
    if canonical != raw {
        return Err(SettingsValueError::malformed(
            SETTINGS_KEY_EDITOR_ARGV,
            "the value is not the canonical array form",
        ));
    }
    Ok(argv)
}

/// Encodes the settings contract version into its stored form.
pub fn setting_version_value() -> String {
    SETTINGS_VERSION.to_string()
}

/// Parses the stored `margin.version` value. Only the version this build
/// writes is interpretable; any other number is a contract this build must
/// refuse rather than narrow.
pub fn parse_setting_version(raw: &str) -> Result<u32, SettingsValueError> {
    require_bounded(SETTINGS_KEY_VERSION, raw)?;
    let version: u32 = raw.parse().map_err(|_| {
        SettingsValueError::malformed(
            SETTINGS_KEY_VERSION,
            format!("{:?} is not a settings contract version", excerpt(raw)),
        )
    })?;
    if version != SETTINGS_VERSION {
        return Err(SettingsValueError::UnsupportedVersion {
            value: excerpt(raw),
        });
    }
    Ok(version)
}

impl ScopeDescriptor {
    /// The user-style scope input that resolves back to this descriptor
    /// (SPEC 8.2). A stored scope re-enters through the same resolver as an
    /// explicit one, so no second resolution path exists.
    pub fn to_scope_input(&self) -> ScopeInput {
        ScopeInput {
            include: self.include.clone(),
            exclude: self.exclude.clone(),
            suffixes: self.suffixes.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Commit trailer contract (SPEC 8.3)
// ---------------------------------------------------------------------------

/// Version of the reserved commit trailer contract (SPEC 8.3).
pub const COMMIT_TRAILER_VERSION: u32 = 1;

/// Reserved trailer carrying the trailer contract version.
pub const TRAILER_VERSION: &str = "Margin-Version";
/// Reserved trailer carrying the editorial commit kind.
pub const TRAILER_KIND: &str = "Margin-Kind";
/// Reserved trailer carrying the canonical scope digest.
pub const TRAILER_SCOPE: &str = "Margin-Scope";
/// Reserved trailer carrying the canonical single-line scope descriptor.
pub const TRAILER_SCOPE_SPEC: &str = "Margin-Scope-Spec";
/// Reserved trailer carrying the prior candidate commit, or `unborn`.
pub const TRAILER_CANDIDATE: &str = "Margin-Candidate";
/// Reserved trailer carrying the intended full result tree.
pub const TRAILER_RESULT_TREE: &str = "Margin-Result-Tree";
/// Reserved trailer carrying the review baseline B. Review only.
pub const TRAILER_BASELINE: &str = "Margin-Baseline";
/// Reserved trailer carrying the review completion flag. Review only.
pub const TRAILER_REVIEW: &str = "Margin-Review";
/// Reserved trailer carrying the request identifier. Review and restore.
pub const TRAILER_REQUEST: &str = "Margin-Request";
/// Reserved trailer carrying the plan fingerprint. Review and restore.
pub const TRAILER_PLAN: &str = "Margin-Plan";
/// Reserved trailer carrying the restoration source. Restore only.
pub const TRAILER_SOURCE: &str = "Margin-Source";

/// The only supported value of [`TRAILER_REVIEW`] (SPEC 8.3).
pub const REVIEW_COMPLETE: &str = "complete";

/// The candidate value of a start marker created on an unborn branch.
pub const CANDIDATE_UNBORN: &str = "unborn";

/// Largest free-text portion of one commit message (SPEC 7.3 bounds an
/// explicit message to 4 KiB).
pub const MAX_MESSAGE_BYTES: usize = 4 * 1024;

/// Largest document that still receives the full rendered comparison
/// (SPEC 3.3): one mebibyte.
pub const MAX_RENDERED_COMPARISON_BYTES: u64 = 1024 * 1024;

/// Largest eligible document at all (SPEC 3.3): a larger file stays
/// unapproved with `LIMIT_EXCEEDED` instead of being silently omitted.
pub const MAX_DOCUMENT_BYTES: u64 = 16 * 1024 * 1024;

/// Default number of change groups in one `diff` detail page (SPEC 6.3).
pub const DEFAULT_DIFF_PAGE_GROUPS: u64 = 50;

/// Default number of entries in one `log` page (SPEC 6.3).
pub const DEFAULT_LOG_PAGE_ENTRIES: u64 = 20;

/// Largest `diff` detail page: 256 KiB of response body content, reached
/// first when groups are large (SPEC 6.3). The bound covers detail content
/// only; it never changes the review identity or plan validation.
pub const MAX_DIFF_PAGE_BYTES: usize = 256 * 1024;

/// Largest exact excerpt one side of a change presentation carries into one
/// detail page (SPEC 7.2 bounded presentation): 64 KiB. A larger excerpt
/// truncates and reports its original byte length, never silently.
pub const MAX_PRESENTATION_EXCERPT_BYTES: usize = 64 * 1024;

/// Largest number of word hints one change presentation carries into one
/// detail page (SPEC 7.2 bounded presentation). Hints beyond the bound are
/// counted as omitted, never silently dropped.
pub const MAX_PRESENTATION_WORD_HINTS: usize = 200;

/// Largest complete detail one change group may carry (SPEC 7.2 bounded
/// presentation): 192 KiB. A group whose hints or ranges overflow the
/// budget drops them from the end and reports the omitted count, so one
/// page always fits [`MAX_DIFF_PAGE_BYTES`] even when a single group fills
/// it.
pub const MAX_GROUP_PRESENTATION_BYTES: usize = 192 * 1024;

/// Largest number of source bytes the bounded hexadecimal view of opaque
/// content carries (SPEC 6.3): 64 KiB, the same budget as one presentation
/// excerpt. A larger opaque document truncates the view and reports its
/// true byte length, never silently; the explicit raw export of the same
/// document still returns every byte.
pub const MAX_SHOW_HEX_BYTES: usize = 64 * 1024;

/// The editorial kind of one Margin commit (SPEC 8.1 and 8.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitKind {
    /// `start`: the starting scoped draft captured at adoption.
    Start,
    /// `draft`: an unreviewed checkpoint of scoped writing.
    Draft,
    /// `review`: a completed review that records its intended result.
    Review,
    /// `restore`: an explicit restoration of earlier content.
    Restore,
}

impl CommitKind {
    /// The `Margin-Kind` trailer value.
    pub fn as_str(self) -> &'static str {
        match self {
            CommitKind::Start => "start",
            CommitKind::Draft => "draft",
            CommitKind::Review => "review",
            CommitKind::Restore => "restore",
        }
    }

    /// Parses one `Margin-Kind` trailer value.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "start" => Some(CommitKind::Start),
            "draft" => Some(CommitKind::Draft),
            "review" => Some(CommitKind::Review),
            "restore" => Some(CommitKind::Restore),
            _ => None,
        }
    }

    /// The deterministic default subject when no explicit message is given.
    pub fn default_subject(self) -> &'static str {
        match self {
            CommitKind::Start => "margin: capture starting point",
            CommitKind::Draft => "margin: draft checkpoint",
            CommitKind::Review => "margin: review complete",
            CommitKind::Restore => "margin: restore document",
        }
    }
}

impl fmt::Display for CommitKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The preservation dimension of one workspace (SPEC 4.2): whether the
/// observed scoped working content is in the selected candidate.
///
/// The value is never one overloaded flag. A document can be saved and
/// unreviewed; a previously reviewed commit can coexist with newer unsaved
/// work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreservationState {
    /// Every scoped working path holds the bytes and mode the candidate
    /// stores.
    Captured,
    /// At least one scoped working path holds content the candidate does
    /// not store, or the candidate stores content the working tree no
    /// longer holds.
    Uncaptured,
    /// Storage transformation blockers make the raw working bytes and the
    /// stored blobs incomparable, so neither claim can be made honestly.
    Unknown,
}

impl PreservationState {
    /// The SPEC 4.2 value of this state.
    pub fn as_str(self) -> &'static str {
        match self {
            PreservationState::Captured => "captured",
            PreservationState::Uncaptured => "uncaptured",
            PreservationState::Unknown => "unknown",
        }
    }
}

impl fmt::Display for PreservationState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The review dimension of one workspace (SPEC 4.2): whether scoped
/// differences remain against a valid approved baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewState {
    /// A valid baseline anchors the scope and no scoped difference remains,
    /// and the anchor is the starting point, so nothing is approved yet.
    StartingPoint,
    /// A valid review marker anchors the scope and no scoped difference
    /// remains against it.
    Reviewed,
    /// Scoped differences remain between the candidate and the valid
    /// baseline, whatever the anchor kind is.
    Pending,
    /// No valid baseline exists to review against, so the question cannot
    /// be answered.
    Blocked,
}

impl ReviewState {
    /// The SPEC 4.2 value of this state.
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewState::StartingPoint => "starting_point",
            ReviewState::Reviewed => "reviewed",
            ReviewState::Pending => "pending",
            ReviewState::Blocked => "blocked",
        }
    }
}

impl fmt::Display for ReviewState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The operation dimension of one workspace (SPEC 4.2): what a mutation is
/// doing, or what must be inspected before the next one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationState {
    /// No mutation is outstanding and no repository state asks for
    /// inspection.
    Idle,
    /// A mutation is validating its preconditions. Reported by the
    /// mutation operations themselves; a query never reports it.
    Validating,
    /// A mutation is applying its intended changes. Reported by the
    /// mutation operations themselves; a query never reports it.
    Applying,
    /// A mutation is committing its result. Reported by the mutation
    /// operations themselves; a query never reports it.
    Committing,
    /// Observable repository state — staged changes, unresolved conflicts,
    /// or an in-progress Git operation — must be inspected before the next
    /// mutation.
    RecoveryRequired,
    /// A mutation lost its outcome and only receipt or recovery inspection
    /// can settle it. Reported by the mutation operations themselves and
    /// the recovery inspection.
    OutcomeUnknown,
}

impl OperationState {
    /// The SPEC 4.2 value of this state.
    pub fn as_str(self) -> &'static str {
        match self {
            OperationState::Idle => "idle",
            OperationState::Validating => "validating",
            OperationState::Applying => "applying",
            OperationState::Committing => "committing",
            OperationState::RecoveryRequired => "recovery_required",
            OperationState::OutcomeUnknown => "outcome_unknown",
        }
    }
}

impl fmt::Display for OperationState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The restoration source recorded by one `restore` marker: the revision a
/// document is restored from and its literal path (SPEC 8.3 `Margin-Source`).
///
/// The canonical single-line form is JSON with the fixed field order `path`,
/// `revision` and no insignificant whitespace, so a path containing spaces,
/// quotes, or newlines stays one trailer value (SPEC 8.6 canonical encoding).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreSource {
    /// Literal repository-relative path of the restored document.
    pub path: String,
    /// Full commit object ID the restored content comes from.
    pub revision: String,
}

impl RestoreSource {
    /// The canonical single-line trailer value.
    pub fn canonical_line(&self) -> String {
        serde_json::to_string(self).expect("the restoration source serializes")
    }

    /// Parses one `Margin-Source` trailer value.
    pub fn parse_line(raw: &str) -> Result<Self, String> {
        serde_json::from_str(raw).map_err(|error| error.to_string())
    }
}

/// The reserved trailer map of one Margin commit (SPEC 8.3): the expected
/// metadata prepared before a Git commit, and the parsed metadata of a
/// committed marker. One struct serves both roles, so verification compares
/// prepared intent against observed history field by field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitMetadata {
    /// The `Margin-Kind` value.
    pub kind: CommitKind,
    /// The `Margin-Scope` value: the full scope identity of SPEC 8.6.
    pub scope_digest: String,
    /// The `Margin-Scope-Spec` value: the canonical descriptor bytes.
    pub scope_spec: String,
    /// The `Margin-Candidate` value: the parent commit, or `unborn` for the
    /// initial root start commit.
    pub candidate: String,
    /// The `Margin-Result-Tree` value: the intended full tree T.
    pub result_tree: String,
    /// The `Margin-Baseline` value. Review only.
    pub baseline: Option<String>,
    /// The `Margin-Request` value. Review and restore.
    pub request: Option<String>,
    /// The `Margin-Plan` value: the full plan fingerprint. Review and
    /// restore.
    pub plan: Option<String>,
    /// The `Margin-Source` value. Restore only.
    pub source: Option<RestoreSource>,
}

/// Why a prepared metadata map or free-text message cannot become a commit
/// (SPEC 7.3 and 8.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetadataProblem {
    /// A trailer this commit kind requires is absent.
    MissingField {
        /// The reserved trailer name.
        field: &'static str,
    },
    /// A trailer reserved for another commit kind is present.
    ForbiddenField {
        /// The reserved trailer name.
        field: &'static str,
        /// The kind being prepared.
        kind: CommitKind,
    },
    /// One field value has the wrong shape.
    InvalidValue {
        /// The reserved trailer name.
        field: &'static str,
        /// Why the value is rejected.
        why: &'static str,
    },
    /// The scope-spec value is not the canonical descriptor form.
    NonCanonicalScopeSpec,
    /// The free-text message injects a reserved `Margin-` trailer line
    /// (SPEC 7.3).
    ReservedTrailerInjection {
        /// The rejected line.
        line: String,
    },
    /// The free-text message exceeds the explicit-message bound.
    OversizedMessage {
        /// The observed byte count.
        bytes: usize,
    },
}

impl MetadataProblem {
    /// The SPEC 7.5 error code of this problem.
    pub fn public_code(&self) -> &'static str {
        "INVALID_ARGUMENT"
    }
}

impl fmt::Display for MetadataProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MetadataProblem::MissingField { field } => {
                write!(formatter, "the reserved trailer {field} is required")
            }
            MetadataProblem::ForbiddenField { field, kind } => write!(
                formatter,
                "the reserved trailer {field} belongs to another commit kind than {kind}"
            ),
            MetadataProblem::InvalidValue { field, why } => {
                write!(formatter, "the trailer {field} is invalid: {why}")
            }
            MetadataProblem::NonCanonicalScopeSpec => write!(
                formatter,
                "the scope specification is not the canonical descriptor form"
            ),
            MetadataProblem::ReservedTrailerInjection { line } => write!(
                formatter,
                "the message line {line:?} injects a reserved Margin trailer"
            ),
            MetadataProblem::OversizedMessage { bytes } => write!(
                formatter,
                "the explicit message is {bytes} bytes, more than {MAX_MESSAGE_BYTES}"
            ),
        }
    }
}

impl std::error::Error for MetadataProblem {}

/// Says whether one value is a full lowercase hex object ID of either
/// documented width (SPEC 7.1).
pub fn is_full_oid(value: &str) -> bool {
    let bytes = value.as_bytes();
    (bytes.len() == 40 || bytes.len() == 64)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

/// Says whether one value is a full identity string of SPEC 8.6: the storage
/// object format, a colon, and the full object ID of that format's width.
pub fn is_fingerprint_id(value: &str) -> bool {
    match value.split_once(':') {
        Some(("sha1", oid)) => oid.len() == 40 && is_full_oid(oid),
        Some(("sha256", oid)) => oid.len() == 64 && is_full_oid(oid),
        _ => false,
    }
}

/// Says whether one value is a request identifier of SPEC 7.3: 1 to 128
/// ASCII characters drawn from letters, digits, `.`, `_`, `:`, and `-`.
pub fn is_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

/// The effective free-text portion of one commit message (SPEC 7.3):
/// trailing newlines removed, the deterministic default subject of `kind`
/// when the text is empty, the explicit 4 KiB bound enforced, and no line
/// that could inject a reserved `Margin-` trailer.
///
/// The checkpoint argument, the review plan `message` field, and the
/// marker writer share this one rule, so no interface can accept text
/// another refuses (SPEC 13.6).
pub fn free_text_message(kind: CommitKind, text: &str) -> Result<String, MetadataProblem> {
    let trimmed = text.trim_end_matches('\n');
    if trimmed.is_empty() {
        return Ok(kind.default_subject().to_string());
    }
    if trimmed.len() > MAX_MESSAGE_BYTES {
        return Err(MetadataProblem::OversizedMessage {
            bytes: trimmed.len(),
        });
    }
    if trimmed.contains('\0') || trimmed.contains('\r') {
        return Err(MetadataProblem::InvalidValue {
            field: "message",
            why: "the message contains a NUL or carriage return",
        });
    }
    for line in trimmed.split('\n') {
        if line.starts_with("Margin-") {
            return Err(MetadataProblem::ReservedTrailerInjection {
                line: line.to_string(),
            });
        }
    }
    Ok(trimmed.to_string())
}

impl CommitMetadata {
    /// Validates the prepared map and assembles the exact commit message:
    /// the free-text portion, one blank line, then every reserved trailer in
    /// canonical table order (SPEC 8.3).
    ///
    /// An empty `text` uses the deterministic default subject. Explicit text
    /// is bounded and can never contribute a reserved `Margin-` trailer line,
    /// so the trailer block always carries exactly the prepared metadata.
    pub fn commit_message(&self, text: &str) -> Result<Vec<u8>, MetadataProblem> {
        self.validate()?;
        let mut message = String::new();
        message.push_str(&self.message_text(text)?);
        message.push_str("\n\n");
        for (name, value) in self.trailer_lines() {
            message.push_str(name);
            message.push_str(": ");
            message.push_str(&value);
            message.push('\n');
        }
        Ok(message.into_bytes())
    }

    /// The free-text portion after checks, with trailing newlines removed.
    fn message_text(&self, text: &str) -> Result<String, MetadataProblem> {
        free_text_message(self.kind, text)
    }

    /// The reserved trailer lines with their values, in the order of the
    /// SPEC 8.3 table.
    fn trailer_lines(&self) -> Vec<(&'static str, String)> {
        let mut lines = vec![
            (TRAILER_VERSION, COMMIT_TRAILER_VERSION.to_string()),
            (TRAILER_KIND, self.kind.as_str().to_string()),
            (TRAILER_SCOPE, self.scope_digest.clone()),
            (TRAILER_SCOPE_SPEC, self.scope_spec.clone()),
            (TRAILER_CANDIDATE, self.candidate.clone()),
            (TRAILER_RESULT_TREE, self.result_tree.clone()),
        ];
        if self.kind == CommitKind::Review {
            // `validate` rejects a review without its baseline first.
            let baseline = self
                .baseline
                .clone()
                .expect("a review marker carries its baseline");
            lines.push((TRAILER_BASELINE, baseline));
            lines.push((TRAILER_REVIEW, REVIEW_COMPLETE.to_string()));
        }
        if let Some(request) = &self.request {
            lines.push((TRAILER_REQUEST, request.clone()));
        }
        if let Some(plan) = &self.plan {
            lines.push((TRAILER_PLAN, plan.clone()));
        }
        if let Some(source) = &self.source {
            lines.push((TRAILER_SOURCE, source.canonical_line()));
        }
        lines
    }

    /// Checks every field of the prepared map against the contract.
    fn validate(&self) -> Result<(), MetadataProblem> {
        if !is_fingerprint_id(&self.scope_digest) {
            return Err(MetadataProblem::InvalidValue {
                field: TRAILER_SCOPE,
                why: "the scope digest is not a full Margin identity",
            });
        }
        let descriptor = self.parsed_scope_spec()?;
        if !descriptor.is_supported() {
            return Err(MetadataProblem::InvalidValue {
                field: TRAILER_SCOPE_SPEC,
                why: "the descriptor names an unsupported version or policy",
            });
        }
        match descriptor.recanonicalized() {
            Some(canonical) if canonical.canonical_bytes() == self.scope_spec.as_bytes() => {}
            _ => return Err(MetadataProblem::NonCanonicalScopeSpec),
        }
        if self.candidate != CANDIDATE_UNBORN && !is_full_oid(&self.candidate) {
            return Err(MetadataProblem::InvalidValue {
                field: TRAILER_CANDIDATE,
                why: "the candidate is not a full object ID or unborn",
            });
        }
        if !is_full_oid(&self.result_tree) {
            return Err(MetadataProblem::InvalidValue {
                field: TRAILER_RESULT_TREE,
                why: "the result tree is not a full object ID",
            });
        }
        self.validate_receipt_fields()
    }

    /// Checks the per-kind receipt fields against the SPEC 8.3 table.
    fn validate_receipt_fields(&self) -> Result<(), MetadataProblem> {
        let receipt = matches!(self.kind, CommitKind::Review | CommitKind::Restore);
        let forbidden = |field: &'static str, present: bool| -> Result<(), MetadataProblem> {
            if present {
                Err(MetadataProblem::ForbiddenField {
                    field,
                    kind: self.kind,
                })
            } else {
                Ok(())
            }
        };
        let check = |field: &'static str,
                     value: Option<&String>,
                     shape: fn(&str) -> bool,
                     why: &'static str|
         -> Result<(), MetadataProblem> {
            match (receipt, value) {
                (true, Some(value)) if shape(value) => Ok(()),
                (true, Some(_)) => Err(MetadataProblem::InvalidValue { field, why }),
                (true, None) => Err(MetadataProblem::MissingField { field }),
                (false, Some(_)) => Err(MetadataProblem::ForbiddenField {
                    field,
                    kind: self.kind,
                }),
                (false, None) => Ok(()),
            }
        };
        check(
            TRAILER_REQUEST,
            self.request.as_ref(),
            is_request_id,
            "the request identifier is not 1 to 128 allowed ASCII characters",
        )?;
        check(
            TRAILER_PLAN,
            self.plan.as_ref(),
            is_fingerprint_id,
            "the plan fingerprint is not a full Margin identity",
        )?;
        match self.kind {
            CommitKind::Review => {
                check(
                    TRAILER_BASELINE,
                    self.baseline.as_ref(),
                    is_full_oid,
                    "the baseline is not a full object ID",
                )?;
                forbidden(TRAILER_SOURCE, self.source.is_some())
            }
            CommitKind::Restore => {
                forbidden(TRAILER_BASELINE, self.baseline.is_some())?;
                let Some(source) = &self.source else {
                    return Err(MetadataProblem::MissingField {
                        field: TRAILER_SOURCE,
                    });
                };
                if !is_full_oid(&source.revision) {
                    return Err(MetadataProblem::InvalidValue {
                        field: TRAILER_SOURCE,
                        why: "the source revision is not a full object ID",
                    });
                }
                let path = &source.path;
                if path.is_empty()
                    || path.starts_with('/')
                    || path.split('/').any(|component| component == "..")
                    || path.contains('\0')
                {
                    return Err(MetadataProblem::InvalidValue {
                        field: TRAILER_SOURCE,
                        why: "the source path is not a literal repository-relative path",
                    });
                }
                Ok(())
            }
            CommitKind::Start | CommitKind::Draft => {
                forbidden(TRAILER_BASELINE, self.baseline.is_some())?;
                forbidden(TRAILER_SOURCE, self.source.is_some())
            }
        }
    }

    /// Parses the scope-spec value into a descriptor.
    fn parsed_scope_spec(&self) -> Result<ScopeDescriptor, MetadataProblem> {
        serde_json::from_str(&self.scope_spec).map_err(|_| MetadataProblem::NonCanonicalScopeSpec)
    }
}

/// One trailer line of a commit message trailer block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trailer {
    /// The trailer token, such as `Margin-Version` or `Signed-off-by`.
    pub token: String,
    /// The exact single-line value.
    pub value: String,
}

/// Why a commit message holds no strictly parsable trailer block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrailerParseError {
    /// The message is empty or its final paragraph is not a trailer block.
    NoTrailerBlock,
    /// One line of the final paragraph is not a `Token: value` trailer.
    MalformedLine {
        /// The rejected line.
        line: Vec<u8>,
    },
}

impl fmt::Display for TrailerParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrailerParseError::NoTrailerBlock => {
                write!(formatter, "the message has no trailer block")
            }
            TrailerParseError::MalformedLine { line } => write!(
                formatter,
                "the line {:?} is not a `Token: value` trailer",
                String::from_utf8_lossy(line)
            ),
        }
    }
}

impl std::error::Error for TrailerParseError {}

/// Parses the trailer block of one commit message (SPEC 8.3).
///
/// The block is the final paragraph: the bytes after the last blank line.
/// Every line of it must be a `Token: value` trailer with an ASCII token of
/// letters, digits, and hyphens, one colon, one space, and a UTF-8 value.
/// Folded continuation lines and any non-trailer line make the whole block
/// unparsable, so a marker can never borrow a neighboring paragraph.
pub fn parse_trailer_block(message: &[u8]) -> Result<Vec<Trailer>, TrailerParseError> {
    let trimmed = trim_trailing_newlines(message);
    if trimmed.is_empty() {
        return Err(TrailerParseError::NoTrailerBlock);
    }
    // The block must be its own final paragraph: a message of only one
    // paragraph is free text, whatever its lines look like.
    let Some(position) = trimmed.windows(2).rposition(|window| window == b"\n\n") else {
        return Err(TrailerParseError::NoTrailerBlock);
    };
    let block = &trimmed[position + 2..];
    if block.is_empty() {
        return Err(TrailerParseError::NoTrailerBlock);
    }
    block
        .split(|&byte| byte == b'\n')
        .map(parse_trailer_line)
        .collect()
}

/// Removes every trailing newline.
fn trim_trailing_newlines(message: &[u8]) -> &[u8] {
    let mut end = message.len();
    while end > 0 && message[end - 1] == b'\n' {
        end -= 1;
    }
    &message[..end]
}

/// Parses one trailer line strictly.
fn parse_trailer_line(line: &[u8]) -> Result<Trailer, TrailerParseError> {
    let malformed = || TrailerParseError::MalformedLine {
        line: line.to_vec(),
    };
    if line.contains(&0) || line.contains(&b'\r') {
        return Err(malformed());
    }
    let Some(separator) = line.iter().position(|&byte| byte == b':') else {
        return Err(malformed());
    };
    let (token, value) = (&line[..separator], &line[separator + 1..]);
    if token.is_empty() || !token[0].is_ascii_alphabetic() {
        return Err(malformed());
    }
    if !token
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
    {
        return Err(malformed());
    }
    // Exactly one space separates the colon from the value.
    let value = value.strip_prefix(b" ").ok_or_else(malformed)?;
    let token = String::from_utf8(token.to_vec()).map_err(|_| malformed())?;
    let value = String::from_utf8(value.to_vec()).map_err(|_| malformed())?;
    Ok(Trailer { token, value })
}

/// Why the reserved trailers of one committed message do not satisfy the
/// SPEC 8.3 contract (SPEC 8.4 rule 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrailerProblem {
    /// A required reserved trailer is absent.
    Missing {
        /// The reserved trailer name.
        name: &'static str,
    },
    /// A reserved trailer appears more than once.
    Duplicate {
        /// The reserved trailer name.
        name: &'static str,
        /// How many times it appears.
        count: usize,
    },
    /// An unknown trailer claims the reserved `Margin-` namespace.
    UnknownReserved {
        /// The unknown trailer name.
        name: String,
    },
    /// The version trailer names an unsupported contract version.
    UnsupportedVersion {
        /// The observed value.
        found: String,
    },
    /// The kind trailer names no supported commit kind.
    UnknownKind {
        /// The observed value.
        found: String,
    },
    /// One reserved value has the wrong shape.
    InvalidValue {
        /// The reserved trailer name.
        name: &'static str,
        /// Why the value is rejected.
        why: &'static str,
    },
    /// A trailer reserved for another commit kind is present.
    ForbiddenTrailer {
        /// The reserved trailer name.
        name: &'static str,
        /// The observed commit kind.
        kind: CommitKind,
    },
}

impl TrailerProblem {
    /// The SPEC 7.5 error code of this problem.
    pub fn public_code(&self) -> &'static str {
        "CONTRACT_VIOLATION"
    }
}

impl fmt::Display for TrailerProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrailerProblem::Missing { name } => {
                write!(formatter, "the reserved trailer {name} is missing")
            }
            TrailerProblem::Duplicate { name, count } => write!(
                formatter,
                "the reserved trailer {name} appears {count} times"
            ),
            TrailerProblem::UnknownReserved { name } => write!(
                formatter,
                "the unknown trailer {name} claims the reserved Margin namespace"
            ),
            TrailerProblem::UnsupportedVersion { found } => write!(
                formatter,
                "the trailer contract version {found:?} is not supported"
            ),
            TrailerProblem::UnknownKind { found } => {
                write!(formatter, "the commit kind {found:?} is not supported")
            }
            TrailerProblem::InvalidValue { name, why } => {
                write!(formatter, "the trailer {name} is invalid: {why}")
            }
            TrailerProblem::ForbiddenTrailer { name, kind } => write!(
                formatter,
                "the trailer {name} belongs to another commit kind than {kind}"
            ),
        }
    }
}

impl std::error::Error for TrailerProblem {}

/// Extracts and validates the reserved metadata of one committed trailer
/// block (SPEC 8.4 rule 1): every required trailer appears exactly once with
/// a well-shaped value, every trailer of another kind is absent, and no
/// unknown name claims the `Margin-` namespace. Unrelated trailers such as a
/// `Signed-off-by` a hook appended stay allowed.
pub fn reserved_metadata(trailers: &[Trailer]) -> Result<CommitMetadata, TrailerProblem> {
    let mut version: Vec<&str> = Vec::new();
    let mut kind: Vec<&str> = Vec::new();
    let mut scope: Vec<&str> = Vec::new();
    let mut scope_spec: Vec<&str> = Vec::new();
    let mut candidate: Vec<&str> = Vec::new();
    let mut result_tree: Vec<&str> = Vec::new();
    let mut baseline: Vec<&str> = Vec::new();
    let mut review: Vec<&str> = Vec::new();
    let mut request: Vec<&str> = Vec::new();
    let mut plan: Vec<&str> = Vec::new();
    let mut source: Vec<&str> = Vec::new();
    for trailer in trailers {
        match trailer.token.as_str() {
            TRAILER_VERSION => version.push(&trailer.value),
            TRAILER_KIND => kind.push(&trailer.value),
            TRAILER_SCOPE => scope.push(&trailer.value),
            TRAILER_SCOPE_SPEC => scope_spec.push(&trailer.value),
            TRAILER_CANDIDATE => candidate.push(&trailer.value),
            TRAILER_RESULT_TREE => result_tree.push(&trailer.value),
            TRAILER_BASELINE => baseline.push(&trailer.value),
            TRAILER_REVIEW => review.push(&trailer.value),
            TRAILER_REQUEST => request.push(&trailer.value),
            TRAILER_PLAN => plan.push(&trailer.value),
            TRAILER_SOURCE => source.push(&trailer.value),
            other if other.starts_with("Margin-") => {
                return Err(TrailerProblem::UnknownReserved {
                    name: other.to_string(),
                });
            }
            _ => {}
        }
    }

    fn exactly_once<'a>(name: &'static str, values: &[&'a str]) -> Result<&'a str, TrailerProblem> {
        match values {
            [only] => Ok(only),
            [] => Err(TrailerProblem::Missing { name }),
            found => Err(TrailerProblem::Duplicate {
                name,
                count: found.len(),
            }),
        }
    }
    let found_version = exactly_once(TRAILER_VERSION, &version)?;
    if found_version != COMMIT_TRAILER_VERSION.to_string() {
        return Err(TrailerProblem::UnsupportedVersion {
            found: found_version.to_string(),
        });
    }
    let found_kind = exactly_once(TRAILER_KIND, &kind)?;
    let parsed_kind = CommitKind::parse(found_kind).ok_or_else(|| TrailerProblem::UnknownKind {
        found: found_kind.to_string(),
    })?;

    let receipt = matches!(parsed_kind, CommitKind::Review | CommitKind::Restore);
    let optional = |name: &'static str,
                    values: &[&str],
                    shape: fn(&str) -> bool,
                    why: &'static str|
     -> Result<Option<String>, TrailerProblem> {
        match (receipt, values) {
            (false, []) => Ok(None),
            (true, []) => Err(TrailerProblem::Missing { name }),
            (true, [only]) if shape(only) => Ok(Some((*only).to_string())),
            (true, [_]) => Err(TrailerProblem::InvalidValue { name, why }),
            (true, found) => Err(TrailerProblem::Duplicate {
                name,
                count: found.len(),
            }),
            (false, _) => Err(TrailerProblem::ForbiddenTrailer {
                name,
                kind: parsed_kind,
            }),
        }
    };
    let parsed_request = optional(
        TRAILER_REQUEST,
        &request,
        is_request_id,
        "the request identifier is not 1 to 128 allowed ASCII characters",
    )?;
    let parsed_plan = optional(
        TRAILER_PLAN,
        &plan,
        is_fingerprint_id,
        "the plan fingerprint is not a full Margin identity",
    )?;

    let parsed_scope = exactly_once(TRAILER_SCOPE, &scope)?;
    if !is_fingerprint_id(parsed_scope) {
        return Err(TrailerProblem::InvalidValue {
            name: TRAILER_SCOPE,
            why: "the scope digest is not a full Margin identity",
        });
    }
    let parsed_spec = exactly_once(TRAILER_SCOPE_SPEC, &scope_spec)?;
    let descriptor: ScopeDescriptor =
        serde_json::from_str(parsed_spec).map_err(|_| TrailerProblem::InvalidValue {
            name: TRAILER_SCOPE_SPEC,
            why: "the value is not a scope descriptor",
        })?;
    if !descriptor.is_supported() {
        return Err(TrailerProblem::InvalidValue {
            name: TRAILER_SCOPE_SPEC,
            why: "the descriptor names an unsupported version or policy",
        });
    }
    match descriptor.recanonicalized() {
        Some(canonical) if canonical.canonical_bytes() == parsed_spec.as_bytes() => {}
        _ => {
            return Err(TrailerProblem::InvalidValue {
                name: TRAILER_SCOPE_SPEC,
                why: "the descriptor is not in canonical form",
            });
        }
    }
    let parsed_candidate = exactly_once(TRAILER_CANDIDATE, &candidate)?;
    if parsed_candidate != CANDIDATE_UNBORN && !is_full_oid(parsed_candidate) {
        return Err(TrailerProblem::InvalidValue {
            name: TRAILER_CANDIDATE,
            why: "the candidate is not a full object ID or unborn",
        });
    }
    let parsed_tree = exactly_once(TRAILER_RESULT_TREE, &result_tree)?;
    if !is_full_oid(parsed_tree) {
        return Err(TrailerProblem::InvalidValue {
            name: TRAILER_RESULT_TREE,
            why: "the result tree is not a full object ID",
        });
    }

    let (parsed_baseline, parsed_source) = match parsed_kind {
        CommitKind::Review => {
            match review.as_slice() {
                [only] if *only == REVIEW_COMPLETE => {}
                [] => {
                    return Err(TrailerProblem::Missing {
                        name: TRAILER_REVIEW,
                    });
                }
                [_] => {
                    return Err(TrailerProblem::InvalidValue {
                        name: TRAILER_REVIEW,
                        why: "the only supported value is complete",
                    });
                }
                found => {
                    return Err(TrailerProblem::Duplicate {
                        name: TRAILER_REVIEW,
                        count: found.len(),
                    });
                }
            }
            if !source.is_empty() {
                return Err(TrailerProblem::ForbiddenTrailer {
                    name: TRAILER_SOURCE,
                    kind: parsed_kind,
                });
            }
            let baseline = exactly_once(TRAILER_BASELINE, &baseline)?;
            if !is_full_oid(baseline) {
                return Err(TrailerProblem::InvalidValue {
                    name: TRAILER_BASELINE,
                    why: "the baseline is not a full object ID",
                });
            }
            (Some(baseline.to_string()), None)
        }
        CommitKind::Restore => {
            if !review.is_empty() {
                return Err(TrailerProblem::ForbiddenTrailer {
                    name: TRAILER_REVIEW,
                    kind: parsed_kind,
                });
            }
            if !baseline.is_empty() {
                return Err(TrailerProblem::ForbiddenTrailer {
                    name: TRAILER_BASELINE,
                    kind: parsed_kind,
                });
            }
            let found_source = exactly_once(TRAILER_SOURCE, &source)?;
            let parsed = RestoreSource::parse_line(found_source).map_err(|_| {
                TrailerProblem::InvalidValue {
                    name: TRAILER_SOURCE,
                    why: "the value is not a restoration source",
                }
            })?;
            (None, Some(parsed))
        }
        CommitKind::Start | CommitKind::Draft => {
            if !review.is_empty() {
                return Err(TrailerProblem::ForbiddenTrailer {
                    name: TRAILER_REVIEW,
                    kind: parsed_kind,
                });
            }
            if !baseline.is_empty() {
                return Err(TrailerProblem::ForbiddenTrailer {
                    name: TRAILER_BASELINE,
                    kind: parsed_kind,
                });
            }
            if !source.is_empty() {
                return Err(TrailerProblem::ForbiddenTrailer {
                    name: TRAILER_SOURCE,
                    kind: parsed_kind,
                });
            }
            (None, None)
        }
    };

    Ok(CommitMetadata {
        kind: parsed_kind,
        scope_digest: parsed_scope.to_string(),
        scope_spec: parsed_spec.to_string(),
        candidate: parsed_candidate.to_string(),
        result_tree: parsed_tree.to_string(),
        baseline: parsed_baseline,
        request: parsed_request,
        plan: parsed_plan,
        source: parsed_source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(components: &[&str], spelled_as_dir: bool) -> ParsedRoot {
        ParsedRoot {
            components: components.iter().map(|part| part.to_string()).collect(),
            spelled_as_dir,
        }
    }

    fn entry(components: &[&str], is_dir_root: bool) -> RootEntry {
        RootEntry {
            components: components.iter().map(|part| part.to_string()).collect(),
            is_dir_root,
        }
    }

    #[test]
    fn the_three_state_dimensions_use_the_spec_vocabulary() {
        // SPEC 4.2 names every value; the strings are the public contract.
        assert_eq!(PreservationState::Captured.as_str(), "captured");
        assert_eq!(PreservationState::Uncaptured.as_str(), "uncaptured");
        assert_eq!(PreservationState::Unknown.as_str(), "unknown");
        assert_eq!(ReviewState::StartingPoint.as_str(), "starting_point");
        assert_eq!(ReviewState::Reviewed.as_str(), "reviewed");
        assert_eq!(ReviewState::Pending.as_str(), "pending");
        assert_eq!(ReviewState::Blocked.as_str(), "blocked");
        assert_eq!(OperationState::Idle.as_str(), "idle");
        assert_eq!(OperationState::Validating.as_str(), "validating");
        assert_eq!(OperationState::Applying.as_str(), "applying");
        assert_eq!(OperationState::Committing.as_str(), "committing");
        assert_eq!(
            OperationState::RecoveryRequired.as_str(),
            "recovery_required"
        );
        assert_eq!(OperationState::OutcomeUnknown.as_str(), "outcome_unknown");
        assert_eq!(
            PreservationState::Uncaptured.to_string(),
            "uncaptured".to_string()
        );
        assert_eq!(ReviewState::Pending.to_string(), "pending".to_string());
        assert_eq!(
            OperationState::OutcomeUnknown.to_string(),
            "outcome_unknown".to_string()
        );
    }

    #[test]
    fn the_document_limits_match_the_spec_values() {
        assert_eq!(MAX_RENDERED_COMPARISON_BYTES, 1024 * 1024);
        assert_eq!(MAX_DOCUMENT_BYTES, 16 * 1024 * 1024);
        assert_eq!(MAX_SHOW_HEX_BYTES, 64 * 1024);
    }

    #[test]
    fn roots_normalize_and_reject_escapes() {
        assert_eq!(parse_root("content").unwrap(), root(&["content"], false));
        assert_eq!(parse_root("./content/").unwrap(), root(&["content"], true));
        assert_eq!(parse_root("a//b/./").unwrap(), root(&["a", "b"], true));
        assert_eq!(parse_root(".").unwrap(), root(&[], true));
        // Filename case is preserved, and a backslash stays literal data.
        assert_eq!(
            parse_root("Mixed/Case\\Slash").unwrap(),
            root(&["Mixed", "Case\\Slash"], false)
        );

        assert_eq!(
            parse_root(".."),
            Err(ScopeError::ParentEscape { root: "..".into() })
        );
        assert_eq!(
            parse_root("a/../b"),
            Err(ScopeError::ParentEscape {
                root: "a/../b".into()
            })
        );
        assert_eq!(
            parse_root("/etc"),
            Err(ScopeError::AbsoluteRoot {
                root: "/etc".into()
            })
        );
        assert_eq!(parse_root(""), Err(ScopeError::EmptyRoot));
        assert_eq!(
            parse_root("a\0b"),
            Err(ScopeError::NulByte {
                root: "a\0b".into()
            })
        );
    }

    #[test]
    fn document_paths_normalize_and_reject_escapes() {
        assert_eq!(parse_document_path("notes/a.md").unwrap(), "notes/a.md");
        assert_eq!(parse_document_path("./notes//a.md").unwrap(), "notes/a.md");
        // Filename case is preserved, and spaces, quotes, dashes, and
        // Unicode stay literal data (SPEC 3.3).
        assert_eq!(
            parse_document_path("drafts/My Draft \"v2\".md").unwrap(),
            "drafts/My Draft \"v2\".md"
        );
        assert_eq!(
            parse_document_path("草稿/-leading-dash.md").unwrap(),
            "草稿/-leading-dash.md"
        );

        assert_eq!(
            parse_document_path("notes/"),
            Err(DocumentPathError::TrailingSeparator {
                path: "notes/".into()
            })
        );
        assert_eq!(
            parse_document_path("notes/../a.md"),
            Err(DocumentPathError::ParentEscape {
                path: "notes/../a.md".into()
            })
        );
        assert_eq!(
            parse_document_path("/etc/hosts"),
            Err(DocumentPathError::AbsolutePath {
                path: "/etc/hosts".into()
            })
        );
        assert_eq!(parse_document_path(""), Err(DocumentPathError::EmptyPath));
        assert_eq!(parse_document_path("."), Err(DocumentPathError::EmptyPath));
        assert_eq!(
            parse_document_path("a\0b"),
            Err(DocumentPathError::NulByte {
                path: "a\0b".into()
            })
        );
    }

    #[test]
    fn suffixes_require_a_dot_and_lose_case() {
        assert_eq!(parse_suffix(".MD").unwrap(), ".md");
        assert_eq!(parse_suffix(".markdown").unwrap(), ".markdown");
        assert_eq!(
            parse_suffix("md"),
            Err(ScopeError::InvalidSuffix {
                suffix: "md".into()
            })
        );
        assert_eq!(
            parse_suffix(".a/b"),
            Err(ScopeError::InvalidSuffix {
                suffix: ".a/b".into()
            })
        );
        assert_eq!(
            parse_suffix(""),
            Err(ScopeError::InvalidSuffix { suffix: "".into() })
        );
    }

    #[test]
    fn canonical_roots_sort_prune_and_absorb() {
        let descriptor = ScopeDescriptor::assemble(
            &[
                entry(&["content", "deep"], true),
                entry(&["content"], true),
                entry(&["Zebra"], true),
                entry(&["apple"], true),
                entry(&["exact", "file.md"], false),
            ],
            &[entry(&["content", "private"], true)],
            &[".md".to_string()],
        )
        .unwrap();
        // Uppercase sorts before lowercase in UTF-8 byte order, and the
        // `content/` root absorbs `content/deep/` but not the unrelated
        // exact-file root.
        assert_eq!(
            descriptor.include,
            vec!["Zebra/", "apple/", "content/", "exact/file.md"]
        );
        assert_eq!(descriptor.exclude, vec!["content/private/"]);

        let whole = ScopeDescriptor::assemble(
            &[entry(&["content"], true), entry(&[], true)],
            &[],
            &[".md".to_string()],
        )
        .unwrap();
        assert_eq!(whole.include, vec!["./"]);
        assert_eq!(
            ScopeDescriptor::assemble(&[], &[], &[".md".to_string()]).unwrap_err(),
            ScopeError::EmptyInclude
        );
    }

    #[test]
    fn canonical_bytes_match_the_specification_example() {
        let descriptor = ScopeDescriptor::assemble(
            &[entry(&["content"], true)],
            &[entry(&["content", "private"], true)],
            &[".markdown".to_string(), ".md".to_string()],
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(descriptor.canonical_bytes()).unwrap(),
            "{\"v\":1,\"include\":[\"content/\"],\"exclude\":[\"content/private/\"],\
             \"suffixes\":[\".markdown\",\".md\"],\"path_policy\":\"literal-utf8-v1\",\
             \"storage_policy\":\"identity-v1\"}"
        );
        // Domain separation precedes the canonical bytes.
        let digest_input = descriptor.digest_input_bytes();
        assert!(digest_input.starts_with(SCOPE_DIGEST_DOMAIN));
        assert_eq!(
            &digest_input[SCOPE_DIGEST_DOMAIN.len()..],
            &descriptor.canonical_bytes()
        );
        // Suffix order and duplicates are canonicalized away.
        let again = ScopeDescriptor::assemble(
            &[entry(&["content"], true)],
            &[],
            &[".MD".to_string(), ".markdown".to_string()],
        )
        .unwrap();
        assert_eq!(again.suffixes, vec![".markdown", ".md"]);
    }

    #[test]
    fn paths_match_roots_excludes_and_suffixes() {
        let scope = ScopeDescriptor::assemble(
            &[entry(&["content"], true), entry(&["README.md"], false)],
            &[
                entry(&["content", "private"], true),
                entry(&["content", "secret.md"], false),
            ],
            &[".md".to_string()],
        )
        .unwrap();

        assert!(scope.contains_path(b"content/article.md"));
        assert!(scope.contains_path(b"README.md"));
        // Suffix matching is ASCII case-insensitive on both sides.
        assert!(scope.contains_path(b"content/Upper.MD"));
        assert!(!scope.contains_path(b"content/note.txt"));
        assert!(!scope.contains_path(b"content"));
        assert!(!scope.contains_path(b"contentitious/x.md"));
        assert!(!scope.contains_path(b"README"));
        // Exclude roots subtract directories and exact files.
        assert!(!scope.contains_path(b"content/private/p.md"));
        assert!(!scope.contains_path(b"content/secret.md"));

        let whole =
            ScopeDescriptor::assemble(&[entry(&[], true)], &[], &[".md".to_string()]).unwrap();
        assert!(whole.contains_path(b"deep/name.md"));

        let no_suffixes = ScopeDescriptor::assemble(&[entry(&[], true)], &[], &[]).unwrap();
        assert!(
            !no_suffixes.contains_path(b"any.md"),
            "no allowed suffix matches nothing"
        );
    }

    #[test]
    fn directory_entries_match_boundaries_without_suffixes() {
        let scope = ScopeDescriptor::assemble(
            &[entry(&["content"], true)],
            &[entry(&["content", "private"], true)],
            &[".md".to_string()],
        )
        .unwrap();
        assert!(scope.contains_directory(b"content/nested/"));
        assert!(scope.contains_directory(b"content/"));
        assert!(!scope.contains_directory(b"other/nested/"));
        assert!(!scope.contains_directory(b"content/private/nested/"));

        let file_root = ScopeDescriptor::assemble(
            &[entry(&["content", "one.md"], false)],
            &[],
            &[".md".to_string()],
        )
        .unwrap();
        assert!(!file_root.contains_directory(b"content/"));
    }

    #[test]
    fn non_utf8_input_is_rejected_with_its_bytes() {
        #[cfg(unix)]
        let bad = {
            use std::os::unix::ffi::OsStringExt;
            OsString::from_vec(b"\xff/x".to_vec())
        };
        #[cfg(not(unix))]
        let bad = OsString::from("\u{fffd}/x");
        let error = ScopeInput::from_os(&[bad], &[], &[]).unwrap_err();
        assert_eq!(
            error,
            ScopeError::NonUtf8 {
                kind: "include root",
                bytes: b"\xff/x".to_vec()
            }
        );
        assert_eq!(error.public_code(), "INVALID_ARGUMENT");
    }

    fn content_entry(path: &str, mode: EntryMode, blob_oid: &str) -> ContentEntry {
        ContentEntry {
            path: path.to_string(),
            mode,
            blob_oid: blob_oid.to_string(),
        }
    }

    const OID_A: &str = "0123456789abcdef0123456789abcdef01234567";
    const OID_B: &str = "89abcdef0123456789abcdef0123456789abcdef";
    /// A full 64-hex SHA-256 object ID.
    const OID_WIDE: &str = "0123456789abcdef0123456789abcdef01234567\
                            89abcdef0123456789abcdef";

    #[test]
    fn canonical_writer_never_merges_adjacent_values() {
        // Two texts cannot merge into one, and a text cannot slide into a
        // count, so different field boundaries always give different bytes.
        let mut one = CanonicalWriter::new(b"d\0");
        one.text("ab");
        one.text("c");
        let mut two = CanonicalWriter::new(b"d\0");
        two.text("a");
        two.text("bc");
        assert_ne!(one.finish(), two.finish());

        let mut three = CanonicalWriter::new(b"d\0");
        three.text("a");
        three.count(0x63);
        let mut four = CanonicalWriter::new(b"d\0");
        four.text("a");
        four.text("c");
        assert_ne!(three.finish(), four.finish());
    }

    #[test]
    fn content_entries_sort_by_path_bytes() {
        let sorted = content_digest_input_bytes(&[
            content_entry("b.md", EntryMode::Regular, OID_A),
            content_entry("A.md", EntryMode::Regular, OID_A),
        ]);
        // Uppercase `A` sorts before lowercase `b` in UTF-8 byte order.
        let expected = {
            let mut writer = CanonicalWriter::new(CONTENT_DIGEST_DOMAIN);
            writer.count(2);
            writer.text("A.md");
            writer.text("100644");
            writer.text(OID_A);
            writer.text("b.md");
            writer.text("100644");
            writer.text(OID_A);
            writer.finish()
        };
        assert_eq!(sorted, expected);
    }

    #[test]
    fn content_digest_covers_every_entry_field() {
        let base = || vec![content_entry("a.md", EntryMode::Regular, OID_A)];
        let digest_of = |entries: Vec<ContentEntry>| content_digest_input_bytes(&entries);
        let base_bytes = digest_of(base());

        // Path, mode, and blob identity each change the input, and adding
        // or removing an entry changes it through the entry set.
        assert_ne!(
            digest_of(vec![content_entry("b.md", EntryMode::Regular, OID_A)]),
            base_bytes
        );
        assert_ne!(
            digest_of(vec![content_entry("a.md", EntryMode::Executable, OID_A)]),
            base_bytes
        );
        assert_ne!(
            digest_of(vec![content_entry("a.md", EntryMode::Regular, OID_B)]),
            base_bytes
        );
        assert_ne!(
            digest_of(vec![
                content_entry("a.md", EntryMode::Regular, OID_A),
                content_entry("b.md", EntryMode::Regular, OID_B)
            ]),
            base_bytes
        );
        assert_ne!(content_digest_input_bytes(&[]), base_bytes);
    }

    #[test]
    fn manifest_digest_keeps_group_order() {
        let group = |path: &str| ChangeGroup {
            path: path.to_string(),
            patch_units: vec![b"@@ -1 +1 @@\n-old\n+new\n".to_vec()],
        };
        let left = manifest_digest_input_bytes(&[group("a.md"), group("b.md")]);
        let right = manifest_digest_input_bytes(&[group("b.md"), group("a.md")]);
        // Group ordering is semantic, so the swap changes the input.
        assert_ne!(left, right);
        assert_eq!(
            left,
            manifest_digest_input_bytes(&[group("a.md"), group("b.md")])
        );
    }

    #[test]
    fn review_and_change_inputs_carry_their_contract() {
        let scope = ScopeDescriptor::assemble(
            &[RootEntry {
                components: vec!["content".to_string()],
                is_dir_root: true,
            }],
            &[],
            &[".md".to_string()],
        )
        .unwrap();
        let context = ReviewContext {
            branch_ref: "refs/heads/writing".to_string(),
            baseline_oid: OID_A.to_string(),
            candidate_oid: OID_B.to_string(),
            scope: scope.clone(),
            engine_version: ENGINE_VERSION.to_string(),
            git_version: "2.51.1".to_string(),
            manifest_digest: OID_A.to_string(),
        };
        let review = review_id_input_bytes(&context);
        assert!(review.starts_with(REVIEW_ID_DOMAIN));
        // The full canonical descriptor bytes are part of the identity.
        let descriptor_bytes = scope.canonical_bytes();
        let needle: Vec<u8> = {
            let mut prefix = (descriptor_bytes.len() as u64).to_be_bytes().to_vec();
            prefix.extend_from_slice(&descriptor_bytes);
            prefix
        };
        assert!(
            review.windows(needle.len()).any(|window| window == needle),
            "the review input contains the canonical descriptor bytes"
        );

        // Changing any one covered field regenerates the input.
        for changed in [
            review_id_input_bytes(&ReviewContext {
                branch_ref: "refs/heads/other".to_string(),
                ..context.clone()
            }),
            review_id_input_bytes(&ReviewContext {
                baseline_oid: OID_B.to_string(),
                ..context.clone()
            }),
            review_id_input_bytes(&ReviewContext {
                candidate_oid: OID_A.to_string(),
                ..context.clone()
            }),
            review_id_input_bytes(&ReviewContext {
                engine_version: "2".to_string(),
                ..context.clone()
            }),
            review_id_input_bytes(&ReviewContext {
                git_version: "2.52.0".to_string(),
                ..context.clone()
            }),
            review_id_input_bytes(&ReviewContext {
                manifest_digest: OID_B.to_string(),
                ..context.clone()
            }),
        ] {
            assert_ne!(changed, review);
        }
        // A different scope descriptor regenerates it too.
        let mut other_scope = context.clone();
        other_scope.scope.suffixes = vec![".markdown".to_string()];
        assert_ne!(review_id_input_bytes(&other_scope), review);
        assert_eq!(review_id_input_bytes(&context), review);

        let change = ChangeIdentity {
            review_oid: OID_A.to_string(),
            path: "a.md".to_string(),
            patch_units: vec![b"patch".to_vec()],
            group_index: 0,
        };
        let change_bytes = change_id_input_bytes(&change);
        assert!(change_bytes.starts_with(CHANGE_ID_DOMAIN));
        for changed in [
            change_id_input_bytes(&ChangeIdentity {
                review_oid: OID_B.to_string(),
                ..change.clone()
            }),
            change_id_input_bytes(&ChangeIdentity {
                path: "b.md".to_string(),
                ..change.clone()
            }),
            change_id_input_bytes(&ChangeIdentity {
                patch_units: vec![b"patch".to_vec(), b"more".to_vec()],
                ..change.clone()
            }),
            change_id_input_bytes(&ChangeIdentity {
                group_index: 1,
                ..change.clone()
            }),
        ] {
            assert_ne!(changed, change_bytes);
        }
        assert_eq!(change_id_input_bytes(&change), change_bytes);
    }

    #[test]
    fn the_review_context_key_covers_the_comparison_without_the_manifest() {
        let scope = ScopeDescriptor::assemble(
            &[RootEntry {
                components: vec!["content".to_string()],
                is_dir_root: true,
            }],
            &[],
            &[".md".to_string()],
        )
        .unwrap();
        let context = ReviewContext {
            branch_ref: "refs/heads/writing".to_string(),
            baseline_oid: OID_A.to_string(),
            candidate_oid: OID_B.to_string(),
            scope,
            engine_version: ENGINE_VERSION.to_string(),
            git_version: "2.51.1".to_string(),
            manifest_digest: OID_A.to_string(),
        };
        let key = ReviewContextKey::from(&context);
        let bytes = review_cache_key_input_bytes(&key);
        assert!(
            bytes.starts_with(REVIEW_CACHE_KEY_DOMAIN),
            "the cache key stays in its own domain"
        );
        assert!(
            !bytes.starts_with(REVIEW_ID_DOMAIN),
            "the review identity domain never prefixes it"
        );
        // The same comparison with a different manifest digest keeps the
        // same key: the key names the derivation inputs, not the result.
        let mut other_digest = context.clone();
        other_digest.manifest_digest = OID_B.to_string();
        assert_eq!(
            review_cache_key_input_bytes(&ReviewContextKey::from(&other_digest)),
            bytes
        );
        // Changing any one derivation input regenerates the key, so no
        // entry of one comparison can be found under another (SPEC 13.3).
        for changed in [
            review_cache_key_input_bytes(&ReviewContextKey {
                branch_ref: "refs/heads/other".to_string(),
                ..key.clone()
            }),
            review_cache_key_input_bytes(&ReviewContextKey {
                baseline_oid: OID_B.to_string(),
                ..key.clone()
            }),
            review_cache_key_input_bytes(&ReviewContextKey {
                candidate_oid: OID_A.to_string(),
                ..key.clone()
            }),
            review_cache_key_input_bytes(&ReviewContextKey {
                engine_version: "2".to_string(),
                ..key.clone()
            }),
            review_cache_key_input_bytes(&ReviewContextKey {
                git_version: "2.52.0".to_string(),
                ..key.clone()
            }),
        ] {
            assert_ne!(changed, bytes);
        }
        let mut other_scope = key.clone();
        other_scope.scope.suffixes = vec![".markdown".to_string()];
        assert_ne!(review_cache_key_input_bytes(&other_scope), bytes);
        assert_eq!(review_cache_key_input_bytes(&key), bytes);
    }

    #[test]
    fn the_timeline_cursor_covers_its_whole_page_contract() {
        let context = LogCursorContext {
            head: OID_A.to_string(),
            scope_id: OID_B.to_string(),
            all_checkpoints: false,
            path_filter: vec!["content/".to_string()],
            limit: 20,
            offset: 0,
        };
        let bytes = log_cursor_digest_input_bytes(&context);
        assert!(
            bytes.starts_with(LOG_CURSOR_DOMAIN),
            "the timeline cursor stays in its own domain"
        );
        assert!(
            !bytes.starts_with(CURSOR_DOMAIN),
            "a detail-page cursor domain never prefixes it"
        );
        // Changing any one covered field regenerates the digest, so a
        // cursor from another walk, scope, policy, filter, limit, or
        // position cannot match (SPEC 6.3).
        for changed in [
            log_cursor_digest_input_bytes(&LogCursorContext {
                head: OID_B.to_string(),
                ..context.clone()
            }),
            log_cursor_digest_input_bytes(&LogCursorContext {
                scope_id: OID_A.to_string(),
                ..context.clone()
            }),
            log_cursor_digest_input_bytes(&LogCursorContext {
                all_checkpoints: true,
                ..context.clone()
            }),
            log_cursor_digest_input_bytes(&LogCursorContext {
                path_filter: vec![],
                ..context.clone()
            }),
            log_cursor_digest_input_bytes(&LogCursorContext {
                path_filter: vec!["content/".to_string(), "notes/".to_string()],
                ..context.clone()
            }),
            log_cursor_digest_input_bytes(&LogCursorContext {
                limit: 21,
                ..context.clone()
            }),
            log_cursor_digest_input_bytes(&LogCursorContext {
                offset: 1,
                ..context.clone()
            }),
        ] {
            assert_ne!(changed, bytes);
        }
        assert_eq!(log_cursor_digest_input_bytes(&context), bytes);
    }

    #[test]
    fn plan_fingerprint_sorts_decisions_and_covers_every_field() {
        let plan = PlanIdentity {
            operation: "review".to_string(),
            request_id: "request-1".to_string(),
            review_id: OID_A.to_string(),
            branch_ref: "refs/heads/writing".to_string(),
            scope_id: OID_B.to_string(),
            baseline_oid: OID_A.to_string(),
            candidate_oid: OID_B.to_string(),
            restore: ["b", "a"].iter().map(|value| value.to_string()).collect(),
            decision_mode: DecisionMode::KeepRemaining,
            message: "keep the tone".to_string(),
        };
        let bytes = plan_fingerprint_input_bytes(&plan);
        assert!(bytes.starts_with(PLAN_FINGERPRINT_DOMAIN));

        // Decision order is not semantic.
        let mut shuffled = plan.clone();
        shuffled.restore = vec!["a".to_string(), "b".to_string()];
        assert_eq!(plan_fingerprint_input_bytes(&shuffled), bytes);

        for changed in [
            plan_fingerprint_input_bytes(&PlanIdentity {
                operation: "restore".to_string(),
                ..plan.clone()
            }),
            plan_fingerprint_input_bytes(&PlanIdentity {
                request_id: "request-2".to_string(),
                ..plan.clone()
            }),
            plan_fingerprint_input_bytes(&PlanIdentity {
                review_id: OID_B.to_string(),
                ..plan.clone()
            }),
            plan_fingerprint_input_bytes(&PlanIdentity {
                branch_ref: "refs/heads/other".to_string(),
                ..plan.clone()
            }),
            plan_fingerprint_input_bytes(&PlanIdentity {
                scope_id: OID_A.to_string(),
                ..plan.clone()
            }),
            plan_fingerprint_input_bytes(&PlanIdentity {
                baseline_oid: OID_B.to_string(),
                ..plan.clone()
            }),
            plan_fingerprint_input_bytes(&PlanIdentity {
                candidate_oid: OID_A.to_string(),
                ..plan.clone()
            }),
            plan_fingerprint_input_bytes(&PlanIdentity {
                restore: vec!["a".to_string()],
                ..plan.clone()
            }),
            plan_fingerprint_input_bytes(&PlanIdentity {
                decision_mode: DecisionMode::KeepExplicit {
                    keep: vec![OID_A.to_string()],
                },
                ..plan.clone()
            }),
            plan_fingerprint_input_bytes(&PlanIdentity {
                message: "keep the tone!".to_string(),
                ..plan.clone()
            }),
        ] {
            assert_ne!(changed, bytes);
        }

        // An explicit keep array is sorted too.
        let explicit = PlanIdentity {
            restore: Vec::new(),
            decision_mode: DecisionMode::KeepExplicit {
                keep: vec!["b".to_string(), "a".to_string()],
            },
            ..plan.clone()
        };
        let mut explicit_sorted = explicit.clone();
        if let DecisionMode::KeepExplicit { keep } = &mut explicit_sorted.decision_mode {
            keep.sort();
        }
        assert_eq!(
            plan_fingerprint_input_bytes(&explicit_sorted),
            plan_fingerprint_input_bytes(&explicit)
        );
    }

    /// One plan document that satisfies every rule of the input contract.
    /// Each override holds one rendered JSON member value; an empty value
    /// removes the field.
    fn plan_document(overrides: &[(&str, String)]) -> String {
        let mut fields: BTreeMap<&str, String> = BTreeMap::from([
            ("schema_version", "1".to_string()),
            ("request_id", "\"review-introduction-01\"".to_string()),
            ("review_id", format!("{:?}", format!("sha1:{OID_A}"))),
            ("branch_ref", "\"refs/heads/writing\"".to_string()),
            ("scope_id", format!("{:?}", format!("sha1:{OID_B}"))),
            ("baseline_oid", format!("{OID_A:?}")),
            ("candidate_oid", format!("{OID_B:?}")),
            (
                "restore",
                format!(
                    "[{:?}, {:?}]",
                    format!("sha1:{OID_A}"),
                    format!("sha1:{OID_B}")
                ),
            ),
            ("keep_remaining", "true".to_string()),
        ]);
        for (field, value) in overrides {
            if value.is_empty() {
                fields.remove(field);
            } else {
                fields.insert(field, value.clone());
            }
        }
        let rendered: Vec<String> = fields
            .into_iter()
            .map(|(field, value)| format!("{field:?}: {value}"))
            .collect();
        format!("{{{}}}", rendered.join(","))
    }

    /// The change identities one fixture review issued.
    fn known_changes() -> BTreeSet<String> {
        BTreeSet::from([format!("sha1:{OID_A}"), format!("sha1:{OID_B}")])
    }

    #[test]
    fn review_plans_parse_strictly_and_resolve_their_own_defaults() {
        let plan = ReviewPlan::parse(plan_document(&[]).as_bytes())
            .expect("the well-formed document parses");
        assert_eq!(plan.request_id, "review-introduction-01");
        assert_eq!(plan.review_id, format!("sha1:{OID_A}"));
        assert_eq!(plan.branch_ref, "refs/heads/writing");
        assert_eq!(
            plan.restore,
            vec![format!("sha1:{OID_A}"), format!("sha1:{OID_B}")]
        );
        assert_eq!(plan.decision_mode, DecisionMode::KeepRemaining);
        // An omitted message resolves to the deterministic default subject
        // of a review commit, so the fingerprint covers one message either
        // way (SPEC 7.3).
        assert_eq!(plan.message, CommitKind::Review.default_subject());
        assert_eq!(
            validate_plan_coverage(&plan, &known_changes()),
            Ok(()),
            "keep_remaining covers every change the plan does not restore"
        );

        // A request identifier may hold every allowed punctuation mark, and
        // the member order of the document carries no meaning.
        let punctuated = ReviewPlan::parse(
            plan_document(&[("request_id", "\"a.1:b-c_d\"".to_string())]).as_bytes(),
        )
        .expect("every allowed punctuation mark parses");
        assert_eq!(punctuated.request_id, "a.1:b-c_d");
        let reordered = ReviewPlan::parse(
            format!(
                "{{\"keep_remaining\": true, \"restore\": [{:?}, {:?}], \
                 \"candidate_oid\": {OID_B:?}, \"baseline_oid\": {OID_A:?}, \
                 \"scope_id\": {:?}, \"branch_ref\": \"refs/heads/writing\", \
                 \"review_id\": {:?}, \"request_id\": \"review-introduction-01\", \
                 \"schema_version\": 1}}",
                format!("sha1:{OID_A}"),
                format!("sha1:{OID_B}"),
                format!("sha1:{OID_B}"),
                format!("sha1:{OID_A}"),
            )
            .as_bytes(),
        )
        .expect("the reversed document parses");
        assert_eq!(reordered.restore, plan.restore);
        assert_eq!(reordered.decision_mode, plan.decision_mode);
        assert_eq!(
            plan_fingerprint_input_bytes(&reordered.identity()),
            plan_fingerprint_input_bytes(&plan.identity()),
            "member order changes neither the plan nor its fingerprint"
        );

        // An explicit keep array must name every other change.
        let explicit = ReviewPlan::parse(
            plan_document(&[
                ("restore", format!("[{:?}]", format!("sha1:{OID_A}"))),
                ("keep_remaining", String::new()),
                ("keep", format!("[{:?}]", format!("sha1:{OID_B}"))),
            ])
            .as_bytes(),
        )
        .expect("one complete explicit keep array parses");
        assert_eq!(
            explicit.decision_mode,
            DecisionMode::KeepExplicit {
                keep: vec![format!("sha1:{OID_B}")]
            }
        );
        assert_eq!(validate_plan_coverage(&explicit, &known_changes()), Ok(()));

        // A keep array that leaves one change undecided is incomplete
        // coverage, and an identity the review never issued is unknown.
        let incomplete = ReviewPlan::parse(
            plan_document(&[
                ("restore", format!("[{:?}]", format!("sha1:{OID_A}"))),
                ("keep_remaining", String::new()),
                ("keep", "[]".to_string()),
            ])
            .as_bytes(),
        )
        .expect("the document itself is well formed");
        assert_eq!(
            validate_plan_coverage(&incomplete, &known_changes()),
            Err(PlanProblem::IncompleteCoverage {
                missing: vec![format!("sha1:{OID_B}")]
            })
        );
        // An identity the review never issued is unknown, even though the
        // document itself is well formed.
        let stranger = format!("sha1:{}", OID_A.replace('0', "f"));
        let unknown =
            ReviewPlan::parse(plan_document(&[("restore", format!("[{stranger:?}]"))]).as_bytes())
                .expect("the document itself is well formed");
        assert_eq!(
            validate_plan_coverage(&unknown, &known_changes()),
            Err(PlanProblem::UnknownChange { id: stranger })
        );
    }

    /// One restoration plan document that satisfies every rule of the input
    /// contract (SPEC 7.4). Each override holds one rendered JSON member
    /// value; an empty value removes the field.
    fn restore_plan_document(overrides: &[(&str, String)]) -> String {
        let mut fields: BTreeMap<&str, String> = BTreeMap::from([
            ("schema_version", "1".to_string()),
            ("request_id", "\"restore-ending-01\"".to_string()),
            ("branch_ref", "\"refs/heads/writing\"".to_string()),
            ("scope_id", format!("{:?}", format!("sha1:{OID_B}"))),
            ("candidate_oid", format!("{OID_B:?}")),
            ("path", "\"drafts/My Draft.md\"".to_string()),
            ("revision", format!("{OID_A:?}")),
        ]);
        for (field, value) in overrides {
            if value.is_empty() {
                fields.remove(field);
            } else {
                fields.insert(field, value.clone());
            }
        }
        let rendered: Vec<String> = fields
            .into_iter()
            .map(|(field, value)| format!("{field:?}: {value}"))
            .collect();
        format!("{{{}}}", rendered.join(","))
    }

    #[test]
    fn restoration_plans_parse_strictly_and_pin_one_document() {
        let plan = RestorePlan::parse(restore_plan_document(&[]).as_bytes())
            .expect("the well-formed document parses");
        assert_eq!(plan.request_id, "restore-ending-01");
        assert_eq!(plan.branch_ref, "refs/heads/writing");
        assert_eq!(plan.scope_id, format!("sha1:{OID_B}"));
        assert_eq!(plan.candidate_oid, OID_B);
        assert_eq!(plan.path, "drafts/My Draft.md", "literal paths survive");
        assert_eq!(plan.revision, OID_A);
        // An omitted message resolves to the deterministic default subject
        // of a restore commit, so the fingerprint covers one message either
        // way (SPEC 7.4).
        assert_eq!(plan.message, CommitKind::Restore.default_subject());

        // Every structural fault refuses with its documented problem
        // (SPEC 7.4, 7.6): inputs are strict.
        let cases: Vec<(&str, String, PlanProblem)> = vec![
            (
                "an unknown field",
                restore_plan_document(&[("restore", "[]".to_string())]),
                PlanProblem::UnknownField {
                    field: "restore".to_string(),
                },
            ),
            (
                "a missing document",
                restore_plan_document(&[("path", String::new())]),
                PlanProblem::MissingField { field: "path" },
            ),
            (
                "a wrong member type",
                restore_plan_document(&[("revision", "12".to_string())]),
                PlanProblem::WrongShape {
                    field: "revision",
                    expected: "a JSON string",
                },
            ),
            (
                "an unsupported version",
                restore_plan_document(&[("schema_version", "2".to_string())]),
                PlanProblem::UnsupportedVersion { observed: 2 },
            ),
            (
                "a revision that is no object ID",
                restore_plan_document(&[("revision", "\"HEAD~1\"".to_string())]),
                PlanProblem::InvalidValue {
                    field: "revision",
                    why: "the value is not a full object ID".to_string(),
                },
            ),
            (
                "a document path that escapes",
                restore_plan_document(&[("path", "\"../outside.md\"".to_string())]),
                PlanProblem::InvalidValue {
                    field: "path",
                    why: "the document path \"../outside.md\" escapes the repository".to_string(),
                },
            ),
            (
                "a message that injects a reserved trailer",
                restore_plan_document(&[("message", "\"line\\nMargin-Kind: review\"".to_string())]),
                PlanProblem::Message {
                    problem: MetadataProblem::ReservedTrailerInjection {
                        line: "Margin-Kind: review".to_string(),
                    },
                },
            ),
        ];
        for (what, document, expected) in cases {
            match RestorePlan::parse(document.as_bytes()) {
                Err(problem) => assert_eq!(problem, expected, "{what} refuses exactly"),
                Ok(parsed) => panic!("{what} must refuse, not parse {parsed:?}"),
            }
        }

        // The fingerprint covers every context field, the document, the
        // revision, and the effective message (SPEC 8.6).
        let bytes = restore_plan_fingerprint_input_bytes(&plan.identity());
        assert!(bytes.starts_with(PLAN_FINGERPRINT_DOMAIN));
        for changed in [
            RestoreIdentity {
                operation: "review".to_string(),
                ..plan.identity()
            },
            RestoreIdentity {
                path: "other.md".to_string(),
                ..plan.identity()
            },
            RestoreIdentity {
                revision: OID_B.to_string(),
                ..plan.identity()
            },
            RestoreIdentity {
                message: "explicit".to_string(),
                ..plan.identity()
            },
        ] {
            assert_ne!(restore_plan_fingerprint_input_bytes(&changed), bytes);
        }
        assert_eq!(
            restore_plan_fingerprint_input_bytes(&plan.identity()),
            bytes
        );
    }

    #[test]
    fn review_plans_refuse_every_structural_fault() {
        let change = format!("sha1:{OID_A}");
        let cases: Vec<(&str, String, PlanProblem)> = vec![
            (
                "an unknown field",
                plan_document(&[("approved_by", "\"the agent\"".to_string())]),
                PlanProblem::UnknownField {
                    field: "approved_by".to_string(),
                },
            ),
            (
                "a duplicate key",
                plan_document(&[]).replacen(
                    "\"branch_ref\"",
                    &format!("\"candidate_oid\": {OID_B:?}, \"branch_ref\""),
                    1,
                ),
                PlanProblem::DuplicateField {
                    field: "candidate_oid".to_string(),
                },
            ),
            (
                "a missing field",
                plan_document(&[("restore", String::new())]),
                PlanProblem::MissingField { field: "restore" },
            ),
            (
                "an unsupported version",
                plan_document(&[("schema_version", "2".to_string())]),
                PlanProblem::UnsupportedVersion { observed: 2 },
            ),
            (
                "a wrong shape",
                plan_document(&[("restore", "\"all\"".to_string())]),
                PlanProblem::WrongShape {
                    field: "restore",
                    expected: "an array of JSON strings",
                },
            ),
            (
                "an invalid request identifier",
                plan_document(&[("request_id", "\"review introduction\"".to_string())]),
                PlanProblem::InvalidValue {
                    field: "request_id",
                    why: "the value is not 1 to 128 characters of letters, digits, '.', '_', ':', and '-'"
                        .to_string(),
                },
            ),
            (
                "an invalid review identity",
                plan_document(&[("review_id", "\"review-1\"".to_string())]),
                PlanProblem::InvalidValue {
                    field: "review_id",
                    why: "the value is not a full Margin identity".to_string(),
                },
            ),
            (
                "an invalid branch ref",
                plan_document(&[("branch_ref", "\"writing\"".to_string())]),
                PlanProblem::InvalidValue {
                    field: "branch_ref",
                    why: "the value is not a full refs/ name or HEAD".to_string(),
                },
            ),
            (
                "a repeated decision",
                plan_document(&[(
                    "restore",
                    format!("[{change:?}, {change:?}]"),
                )]),
                PlanProblem::DuplicateDecision { id: change.clone() },
            ),
            (
                "an overlapping decision",
                plan_document(&[
                    ("restore", format!("[{change:?}]")),
                    ("keep_remaining", String::new()),
                    ("keep", format!("[{change:?}]")),
                ]),
                PlanProblem::OverlappingDecision { id: change },
            ),
            (
                "no decision mode",
                plan_document(&[("keep_remaining", String::new())]),
                PlanProblem::MissingDecision,
            ),
            (
                "a false flag without a keep array",
                plan_document(&[("keep_remaining", "false".to_string())]),
                PlanProblem::MissingDecision,
            ),
            (
                "two decision modes",
                plan_document(&[("keep", format!("[{:?}]", format!("sha1:{OID_B}")))]),
                PlanProblem::ConflictingDecision,
            ),
            (
                "a reserved trailer in the message",
                plan_document(&[("message", "\"Keep it\\nMargin-Kind: restore\"".to_string())]),
                PlanProblem::Message {
                    problem: MetadataProblem::ReservedTrailerInjection {
                        line: "Margin-Kind: restore".to_string(),
                    },
                },
            ),
            (
                "an oversized message",
                plan_document(&[("message", format!("{:?}", "K".repeat(4097)))]),
                PlanProblem::Message {
                    problem: MetadataProblem::OversizedMessage { bytes: 4097 },
                },
            ),
        ];
        for (what, document, expected) in cases {
            assert_eq!(
                ReviewPlan::parse(document.as_bytes()),
                Err(expected),
                "{what}"
            );
        }

        // Not one JSON object at all, in each way that happens.
        for (what, raw) in [
            ("an array", "[1, 2, 3]".to_string()),
            ("a scalar", "\"keep everything\"".to_string()),
            (
                "truncated text",
                "{\"schema_version\": 1, \"request_id\": \"rev".to_string(),
            ),
            ("empty bytes", String::new()),
            ("trailing content", format!("{}}}", plan_document(&[]))),
        ] {
            let problem = ReviewPlan::parse(raw.as_bytes())
                .expect_err(what)
                .public_code();
            assert_eq!(problem, "INVALID_PLAN", "{what}");
            assert!(
                matches!(
                    ReviewPlan::parse(raw.as_bytes()),
                    Err(PlanProblem::NotJson { .. })
                ),
                "{what} is not one JSON object"
            );
        }

        // Invalid UTF-8 bytes refuse before any string is built.
        let mut raw = plan_document(&[]).into_bytes();
        let at = raw
            .windows(b"review-introduction".len())
            .position(|window| window == b"review-introduction")
            .expect("the identifier is in the document");
        raw.insert(at + 6, 0xff);
        assert!(matches!(
            ReviewPlan::parse(&raw),
            Err(PlanProblem::NotJson { .. })
        ));
    }

    #[test]
    fn plan_problems_carry_the_public_error_codes() {
        // Every structural fault is one invalid plan; only a decision
        // against a real review can name an unknown change (SPEC 7.5).
        assert_eq!(PlanProblem::MissingDecision.public_code(), "INVALID_PLAN");
        assert_eq!(
            PlanProblem::UnknownChange {
                id: format!("sha1:{OID_A}")
            }
            .public_code(),
            "UNKNOWN_CHANGE"
        );
        // The message keeps its bound even when many changes are missing.
        let missing: Vec<String> = (0..40).map(|_| format!("sha1:{OID_A}")).collect();
        let text = PlanProblem::IncompleteCoverage { missing }.to_string();
        assert!(text.contains("40 changes undecided"), "{text}");
        assert!(text.contains("more"), "{text}");
    }

    #[test]
    fn the_parsed_plan_feeds_the_plan_fingerprint() {
        // The fingerprint covers the effective message and the one decision
        // mode, so the parsed document and the identity cannot disagree.
        let document = plan_document(&[]);
        let plan = ReviewPlan::parse(document.as_bytes()).unwrap();
        let fingerprint = plan_fingerprint_input_bytes(&plan.identity());
        assert!(fingerprint.starts_with(PLAN_FINGERPRINT_DOMAIN));

        // An explicit message equal to the default subject changes nothing.
        let explicit_default = ReviewPlan::parse(
            plan_document(&[(
                "message",
                format!("{:?}", CommitKind::Review.default_subject()),
            )])
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            plan_fingerprint_input_bytes(&explicit_default.identity()),
            fingerprint
        );

        for changed in [
            plan_document(&[("request_id", "\"review-introduction-02\"".to_string())]),
            plan_document(&[("review_id", format!("{:?}", format!("sha1:{OID_B}")))]),
            plan_document(&[("branch_ref", "\"refs/heads/other\"".to_string())]),
            plan_document(&[("scope_id", format!("{:?}", format!("sha1:{OID_A}")))]),
            plan_document(&[("baseline_oid", format!("{OID_B:?}"))]),
            plan_document(&[("candidate_oid", format!("{OID_A:?}"))]),
            plan_document(&[("message", "\"Keep the shorter opening\"".to_string())]),
        ] {
            let other = ReviewPlan::parse(changed.as_bytes()).unwrap();
            assert_ne!(
                plan_fingerprint_input_bytes(&other.identity()),
                fingerprint,
                "a changed context field or message changes the fingerprint"
            );
        }

        // A changed decision mode changes it too.
        let mode = ReviewPlan::parse(
            plan_document(&[
                ("restore", "[]".to_string()),
                ("keep_remaining", String::new()),
                (
                    "keep",
                    format!(
                        "[{:?}, {:?}]",
                        format!("sha1:{OID_A}"),
                        format!("sha1:{OID_B}")
                    ),
                ),
            ])
            .as_bytes(),
        )
        .unwrap();
        assert_ne!(plan_fingerprint_input_bytes(&mode.identity()), fingerprint);
    }

    #[test]
    fn regular_file_modes_parse_and_render() {
        assert_eq!(EntryMode::parse("100644"), Some(EntryMode::Regular));
        assert_eq!(EntryMode::parse("100755"), Some(EntryMode::Executable));
        // A symlink, a gitlink, a directory, and junk all refuse.
        assert_eq!(EntryMode::parse("120000"), None);
        assert_eq!(EntryMode::parse("160000"), None);
        assert_eq!(EntryMode::parse("040000"), None);
        assert_eq!(EntryMode::parse("644"), None);
        assert_eq!(EntryMode::Regular.as_str(), "100644");
        assert_eq!(EntryMode::Executable.as_str(), "100755");
    }

    /// One well-formed metadata map of each commit kind.
    fn metadata(kind: CommitKind) -> CommitMetadata {
        CommitMetadata {
            kind,
            scope_digest: format!("sha1:{OID_A}"),
            scope_spec: String::from_utf8(
                ScopeDescriptor::assemble(&[entry(&["content"], true)], &[], &[".md".to_string()])
                    .unwrap()
                    .canonical_bytes(),
            )
            .unwrap(),
            candidate: OID_B.to_string(),
            result_tree: OID_A.to_string(),
            baseline: None,
            request: None,
            plan: None,
            source: None,
        }
    }

    #[test]
    fn commit_messages_carry_the_reserved_trailer_block() {
        let mut start = metadata(CommitKind::Start);
        start.candidate = CANDIDATE_UNBORN.to_string();
        let message = String::from_utf8(start.commit_message("first words").unwrap()).unwrap();
        assert_eq!(
            message,
            "first words\n\
             \n\
             Margin-Version: 1\n\
             Margin-Kind: start\n\
             Margin-Scope: sha1:0123456789abcdef0123456789abcdef01234567\n\
             Margin-Scope-Spec: {\"v\":1,\"include\":[\"content/\"],\"exclude\":[],\
             \"suffixes\":[\".md\"],\"path_policy\":\"literal-utf8-v1\",\
             \"storage_policy\":\"identity-v1\"}\n\
             Margin-Candidate: unborn\n\
             Margin-Result-Tree: 0123456789abcdef0123456789abcdef01234567\n"
        );

        // An empty text uses the deterministic default subject.
        let draft = metadata(CommitKind::Draft);
        let message = String::from_utf8(draft.commit_message("").unwrap()).unwrap();
        assert!(message.starts_with("margin: draft checkpoint\n\n"));
        assert!(
            message.ends_with("Margin-Result-Tree: 0123456789abcdef0123456789abcdef01234567\n")
        );

        // A review adds its baseline, completion flag, and receipt fields.
        let mut review = metadata(CommitKind::Review);
        review.baseline = Some(OID_A.to_string());
        review.request = Some("review-introduction-01".to_string());
        review.plan = Some(format!("sha1:{OID_B}"));
        let message = String::from_utf8(review.commit_message("keep the tone").unwrap()).unwrap();
        let lines: Vec<&str> = message.lines().collect();
        assert_eq!(
            &lines[lines.len() - 10..],
            &[
                "Margin-Version: 1",
                "Margin-Kind: review",
                "Margin-Scope: sha1:0123456789abcdef0123456789abcdef01234567",
                "Margin-Scope-Spec: {\"v\":1,\"include\":[\"content/\"],\"exclude\":[],\
                 \"suffixes\":[\".md\"],\"path_policy\":\"literal-utf8-v1\",\
                 \"storage_policy\":\"identity-v1\"}",
                "Margin-Candidate: 89abcdef0123456789abcdef0123456789abcdef",
                "Margin-Result-Tree: 0123456789abcdef0123456789abcdef01234567",
                "Margin-Baseline: 0123456789abcdef0123456789abcdef01234567",
                "Margin-Review: complete",
                "Margin-Request: review-introduction-01",
                "Margin-Plan: sha1:89abcdef0123456789abcdef0123456789abcdef",
            ]
        );
    }

    #[test]
    fn review_and_restore_maps_require_their_receipt_fields() {
        // A review without its receipt fields is refused.
        assert_eq!(
            metadata(CommitKind::Review).commit_message("m"),
            Err(MetadataProblem::MissingField {
                field: TRAILER_REQUEST
            })
        );
        let mut review = metadata(CommitKind::Review);
        review.baseline = Some(OID_A.to_string());
        review.request = Some("request 1".to_string());
        assert_eq!(
            review.commit_message("m"),
            Err(MetadataProblem::InvalidValue {
                field: TRAILER_REQUEST,
                why: "the request identifier is not 1 to 128 allowed ASCII characters",
            })
        );
        review.request = Some("review:01.1_a-b".to_string());
        assert_eq!(
            review.commit_message("m"),
            Err(MetadataProblem::MissingField {
                field: TRAILER_PLAN
            })
        );
        review.plan = Some(format!("sha1:{OID_A}"));
        review.source = Some(RestoreSource {
            path: "content/a.md".to_string(),
            revision: OID_B.to_string(),
        });
        assert_eq!(
            review.commit_message("m"),
            Err(MetadataProblem::ForbiddenField {
                field: TRAILER_SOURCE,
                kind: CommitKind::Review,
            })
        );
        review.source = None;
        assert!(review.commit_message("m").is_ok());

        // A restore needs the source, and no baseline or review flag.
        assert_eq!(
            metadata(CommitKind::Restore).commit_message("m"),
            Err(MetadataProblem::MissingField {
                field: TRAILER_REQUEST
            })
        );
        let mut restore = metadata(CommitKind::Restore);
        restore.request = Some("restore:1".to_string());
        restore.plan = Some(format!("sha256:{OID_WIDE}"));
        assert_eq!(
            restore.commit_message("m"),
            Err(MetadataProblem::MissingField {
                field: TRAILER_SOURCE
            })
        );
        restore.source = Some(RestoreSource {
            path: "content/a.md".to_string(),
            revision: OID_A.to_string(),
        });
        restore.baseline = Some(OID_B.to_string());
        assert_eq!(
            restore.commit_message("m"),
            Err(MetadataProblem::ForbiddenField {
                field: TRAILER_BASELINE,
                kind: CommitKind::Restore,
            })
        );
        restore.baseline = None;
        assert!(restore.commit_message("m").is_ok());

        // Start and draft accept no receipt fields at all.
        assert!(metadata(CommitKind::Start).commit_message("m").is_ok());
        assert!(metadata(CommitKind::Draft).commit_message("m").is_ok());
        let mut draft = metadata(CommitKind::Draft);
        draft.request = Some("draft:1".to_string());
        assert_eq!(
            draft.commit_message("m"),
            Err(MetadataProblem::ForbiddenField {
                field: TRAILER_REQUEST,
                kind: CommitKind::Draft,
            })
        );
    }

    #[test]
    fn prepared_values_must_have_the_documented_shapes() {
        let mut map = metadata(CommitKind::Draft);
        map.scope_digest = OID_A.to_string();
        assert_eq!(
            map.commit_message("m"),
            Err(MetadataProblem::InvalidValue {
                field: TRAILER_SCOPE,
                why: "the scope digest is not a full Margin identity",
            })
        );
        map.scope_digest = format!("sha1:{OID_A}");
        map.candidate = "main".to_string();
        assert_eq!(
            map.commit_message("m"),
            Err(MetadataProblem::InvalidValue {
                field: TRAILER_CANDIDATE,
                why: "the candidate is not a full object ID or unborn",
            })
        );
        map.candidate = OID_B.to_string();
        map.result_tree = "short".to_string();
        assert_eq!(
            map.commit_message("m"),
            Err(MetadataProblem::InvalidValue {
                field: TRAILER_RESULT_TREE,
                why: "the result tree is not a full object ID",
            })
        );
        // Either documented width is a full object ID.
        map.result_tree = OID_WIDE.to_string();
        assert!(map.commit_message("m").is_ok());
        map.result_tree = OID_A.to_string();

        // A non-canonical scope specification is refused even when it parses.
        let descriptor =
            ScopeDescriptor::assemble(&[entry(&["content"], true)], &[], &[".md".to_string()])
                .unwrap();
        map.scope_spec =
            "{\"include\":[\"content/\"],\"v\":1,\"exclude\":[],\"suffixes\":[\".md\"],\
             \"path_policy\":\"literal-utf8-v1\",\"storage_policy\":\"identity-v1\"}"
                .to_string();
        assert_eq!(
            map.commit_message("m"),
            Err(MetadataProblem::NonCanonicalScopeSpec)
        );
        map.scope_spec = String::from_utf8(descriptor.canonical_bytes()).unwrap();
        assert!(map.commit_message("m").is_ok());
    }

    #[test]
    fn explicit_messages_cannot_inject_reserved_trailers() {
        let map = metadata(CommitKind::Draft);
        for bad in [
            "keep all\n\nMargin-Review: complete",
            "Margin-Kind: review",
            "notes\nMargin-Future: value",
        ] {
            assert_eq!(
                map.commit_message(bad),
                Err(MetadataProblem::ReservedTrailerInjection {
                    line: bad
                        .lines()
                        .find(|line| line.starts_with("Margin-"))
                        .unwrap()
                        .to_string(),
                })
            );
        }
        // The bound of SPEC 7.3 applies to the free-text portion.
        let oversized = "x".repeat(MAX_MESSAGE_BYTES + 1);
        assert_eq!(
            map.commit_message(&oversized),
            Err(MetadataProblem::OversizedMessage {
                bytes: MAX_MESSAGE_BYTES + 1
            })
        );
        assert_eq!(
            map.commit_message("carriage\rreturn"),
            Err(MetadataProblem::InvalidValue {
                field: "message",
                why: "the message contains a NUL or carriage return",
            })
        );
        // A message that is only newlines falls back to the default subject.
        let message = String::from_utf8(map.commit_message("\n\n").unwrap()).unwrap();
        assert!(message.starts_with("margin: draft checkpoint\n"));
    }

    #[test]
    fn trailer_blocks_parse_strictly() {
        let trailers = parse_trailer_block(
            b"subject\n\nbody line\n\nMargin-Version: 1\nSigned-off-by: A <a@b.invalid>\n\n\n",
        )
        .unwrap();
        assert_eq!(
            trailers,
            vec![
                Trailer {
                    token: "Margin-Version".to_string(),
                    value: "1".to_string(),
                },
                Trailer {
                    token: "Signed-off-by".to_string(),
                    value: "A <a@b.invalid>".to_string(),
                },
            ]
        );

        assert_eq!(
            parse_trailer_block(b"no trailers here\n"),
            Err(TrailerParseError::NoTrailerBlock)
        );
        assert_eq!(
            parse_trailer_block(b""),
            Err(TrailerParseError::NoTrailerBlock)
        );
        // A folded continuation line is not one strict trailer.
        assert_eq!(
            parse_trailer_block(b"s\n\nMargin-Version: 1\n  folded\n"),
            Err(TrailerParseError::MalformedLine {
                line: b"  folded".to_vec()
            })
        );
        // No space after the colon, a digit-first token, and a stray tab all
        // refuse.
        for line in [
            &b"s\n\nMargin-Version:1"[..],
            b"s\n\n2fast: value\n",
            b"s\n\nBad\tToken: value\n",
        ] {
            assert!(
                parse_trailer_block(line).is_err(),
                "the message {:?} must not parse",
                String::from_utf8_lossy(line)
            );
        }
    }

    #[test]
    fn committed_trailers_extract_into_one_map() {
        let mut map = metadata(CommitKind::Review);
        map.baseline = Some(OID_A.to_string());
        map.request = Some("review-1".to_string());
        map.plan = Some(format!("sha1:{OID_B}"));
        let message = map.commit_message("review it").unwrap();
        let mut trailers = parse_trailer_block(&message).unwrap();
        // A hook-appended sign-off stays allowed and is ignored.
        trailers.push(Trailer {
            token: "Signed-off-by".to_string(),
            value: "A <a@b.invalid>".to_string(),
        });
        assert_eq!(reserved_metadata(&trailers).unwrap(), map);

        // Exact counts: a duplicate version and an unknown reserved name
        // both refuse, whatever their values are.
        let mut duplicated = trailers.clone();
        duplicated.push(Trailer {
            token: "Margin-Version".to_string(),
            value: "1".to_string(),
        });
        assert_eq!(
            reserved_metadata(&duplicated),
            Err(TrailerProblem::Duplicate {
                name: TRAILER_VERSION,
                count: 2,
            })
        );
        let mut unknown = trailers.clone();
        unknown.push(Trailer {
            token: "Margin-Future".to_string(),
            value: "1".to_string(),
        });
        assert_eq!(
            reserved_metadata(&unknown),
            Err(TrailerProblem::UnknownReserved {
                name: "Margin-Future".to_string()
            })
        );

        // Omitting the request from a review marker fails the count rule.
        let mut missing = trailers.clone();
        missing.retain(|trailer| trailer.token != TRAILER_REQUEST);
        assert_eq!(
            reserved_metadata(&missing),
            Err(TrailerProblem::Missing {
                name: TRAILER_REQUEST
            })
        );

        // An unsupported version and an unknown kind refuse.
        let mut future = trailers.clone();
        future[0].value = "2".to_string();
        assert_eq!(
            reserved_metadata(&future),
            Err(TrailerProblem::UnsupportedVersion {
                found: "2".to_string()
            })
        );
        let mut unkinded = trailers;
        unkinded[1].value = "snapshot".to_string();
        assert_eq!(
            reserved_metadata(&unkinded),
            Err(TrailerProblem::UnknownKind {
                found: "snapshot".to_string()
            })
        );
    }

    #[test]
    fn committed_maps_enforce_their_value_shapes() {
        let map = metadata(CommitKind::Draft);
        let trailers = parse_trailer_block(&map.commit_message("m").unwrap()).unwrap();
        let with = |name: &str, value: String| {
            let mut changed = trailers.clone();
            for trailer in &mut changed {
                if trailer.token == name {
                    trailer.value = value.clone();
                }
            }
            changed
        };

        // A scope specification that is not the canonical form refuses.
        assert_eq!(
            reserved_metadata(&with(
                TRAILER_SCOPE_SPEC,
                map.scope_spec.replace(".md", ".MD")
            )),
            Err(TrailerProblem::InvalidValue {
                name: TRAILER_SCOPE_SPEC,
                why: "the descriptor is not in canonical form",
            })
        );
        assert_eq!(
            reserved_metadata(&with(TRAILER_SCOPE, OID_A.to_string())),
            Err(TrailerProblem::InvalidValue {
                name: TRAILER_SCOPE,
                why: "the scope digest is not a full Margin identity",
            })
        );
        assert_eq!(
            reserved_metadata(&with(TRAILER_CANDIDATE, "main".to_string())),
            Err(TrailerProblem::InvalidValue {
                name: TRAILER_CANDIDATE,
                why: "the candidate is not a full object ID or unborn",
            })
        );
        assert_eq!(
            reserved_metadata(&with(TRAILER_RESULT_TREE, "short".to_string())),
            Err(TrailerProblem::InvalidValue {
                name: TRAILER_RESULT_TREE,
                why: "the result tree is not a full object ID",
            })
        );

        // Receipt trailers of another kind refuse even with valid values.
        let mut foreign = trailers.clone();
        foreign.push(Trailer {
            token: TRAILER_BASELINE.to_string(),
            value: OID_A.to_string(),
        });
        assert_eq!(
            reserved_metadata(&foreign),
            Err(TrailerProblem::ForbiddenTrailer {
                name: TRAILER_BASELINE,
                kind: CommitKind::Draft,
            })
        );
    }

    #[test]
    fn restoration_sources_use_one_canonical_line() {
        let source = RestoreSource {
            path: "content/line \"in\" name\nsecond.md".to_string(),
            revision: OID_A.to_string(),
        };
        let line = source.canonical_line();
        assert_eq!(
            line,
            "{\"path\":\"content/line \\\"in\\\" name\\nsecond.md\",\
             \"revision\":\"0123456789abcdef0123456789abcdef01234567\"}"
        );
        assert_eq!(RestoreSource::parse_line(&line).unwrap(), source);
        // The line stays one line, so it can never fold a trailer.
        assert!(!line.contains('\n'));
        assert!(RestoreSource::parse_line("{\"path\":\"a\"}").is_err());
    }

    #[test]
    fn identity_and_request_shapes_follow_their_contracts() {
        assert!(is_full_oid(OID_A));
        assert!(is_full_oid(OID_WIDE));
        assert!(!is_full_oid(&OID_A[..39]));
        assert!(!is_full_oid(&OID_A.to_uppercase()));
        assert!(is_fingerprint_id(&format!("sha1:{OID_A}")));
        assert!(is_fingerprint_id(&format!("sha256:{OID_WIDE}")));
        // The format names the width: a mismatched pair refuses.
        assert!(!is_fingerprint_id(&format!("sha1:{OID_WIDE}")));
        assert!(!is_fingerprint_id(OID_A));
        assert!(is_request_id("a"));
        assert!(is_request_id(&"x".repeat(128)));
        assert!(is_request_id("review-01.2_a:b"));
        assert!(!is_request_id(""));
        assert!(!is_request_id(&"x".repeat(129)));
        assert!(!is_request_id("review 01"));
        assert!(!is_request_id("täglich"));
    }

    #[test]
    fn first_use_settings_capture_nothing() {
        let settings = LocalSettings::first_use();
        assert_eq!(settings.version, None);
        assert_eq!(settings.branch, None);
        assert_eq!(settings.scope, None);
        // First use leaves both capture policies off (SPEC 6.2).
        assert!(!settings.auto_checkpoint);
        assert!(!settings.capture_after_edit);
        assert_eq!(settings.editor_argv, None);
        assert!(SettingsUpdate::default().is_empty());
    }

    #[test]
    fn setting_values_round_trip_through_their_documented_forms() {
        let descriptor = ScopeDescriptor::assemble(
            &[entry(&["content"], true)],
            &[entry(&["content", "private"], true)],
            &[".md".to_string()],
        )
        .unwrap();
        let scope = setting_scope_value(&descriptor).unwrap();
        assert_eq!(parse_setting_scope(&scope).unwrap(), descriptor);
        assert_eq!(parse_setting_branch("writing").unwrap(), "writing");
        assert!(parse_setting_flag(SETTINGS_KEY_AUTO_CHECKPOINT, "true").unwrap());
        assert!(!parse_setting_flag(SETTINGS_KEY_AUTO_CHECKPOINT, "false").unwrap());
        let argv = setting_argv_value(&["nvim".to_string(), "--".to_string()]).unwrap();
        assert_eq!(argv, "[\"nvim\",\"--\"]");
        assert_eq!(
            parse_setting_argv(&argv).unwrap(),
            vec!["nvim".to_string(), "--".to_string()]
        );
        assert_eq!(setting_version_value(), "1");
        assert_eq!(parse_setting_version("1").unwrap(), SETTINGS_VERSION);
    }

    #[test]
    fn stored_values_outside_the_documented_form_are_refused() {
        // Only the two documented flag spellings parse.
        for raw in ["yes", "on", "1", "", " true"] {
            assert!(
                parse_setting_flag(SETTINGS_KEY_AUTO_CHECKPOINT, raw).is_err(),
                "{raw:?} is not a documented flag spelling"
            );
        }
        // A pretty-printed or reordered descriptor is not the canonical form.
        let descriptor =
            ScopeDescriptor::assemble(&[entry(&["content"], true)], &[], &[".md".to_string()])
                .unwrap();
        let canonical = setting_scope_value(&descriptor).unwrap();
        let spread = canonical.replace(',', ", ");
        assert!(parse_setting_scope(&spread).is_err());
        assert!(parse_setting_scope("{\"v\":1}").is_err());
        // An argv needs at least the executable and stays one canonical
        // array form.
        assert!(parse_setting_argv("[]").is_err());
        assert!(parse_setting_argv("\"nvim\"").is_err());
        assert!(parse_setting_argv("[ \"nvim\" ]").is_err());
        assert!(setting_argv_value(&[]).is_err());
        // A branch stays one non-empty line.
        assert!(setting_branch_value("").is_err());
        assert!(setting_branch_value("two\nlines").is_err());
        // Only the version this build writes is interpretable.
        assert_eq!(
            parse_setting_version("2"),
            Err(SettingsValueError::UnsupportedVersion {
                value: "2".to_string()
            })
        );
        assert!(parse_setting_version("0").is_err());
        assert!(parse_setting_version("one").is_err());
    }

    #[test]
    fn oversized_setting_values_are_refused() {
        let huge = "x".repeat(MAX_SETTING_BYTES + 1);
        assert!(setting_branch_value(&huge).is_err());
        assert!(parse_setting_version(&huge).is_err());
        let error = parse_setting_argv(&format!("[\"{huge}\"]")).unwrap_err();
        match error {
            SettingsValueError::Malformed { key, .. } => assert_eq!(key, SETTINGS_KEY_EDITOR_ARGV),
            other => panic!("an oversized value is malformed: {other:?}"),
        }
        // The echoed value stays bounded even for a huge input.
        let message = setting_branch_value("two\nlines").unwrap_err().to_string();
        assert!(message.chars().count() <= EXCERPT_CHARS * 2 + 80);
    }

    #[test]
    fn a_stored_scope_reenters_as_user_input() {
        let descriptor = ScopeDescriptor::assemble(
            &[entry(&["content"], true), entry(&["README.md"], false)],
            &[entry(&["content", "private"], true)],
            &[".md".to_string()],
        )
        .unwrap();
        let input = descriptor.to_scope_input();
        assert_eq!(input.include, vec!["README.md", "content/"]);
        assert_eq!(input.exclude, vec!["content/private/"]);
        assert_eq!(input.suffixes, vec![".md"]);
        // The re-entered input canonicalizes back to the same descriptor.
        let parsed = parse_scope(&input).unwrap();
        assert_eq!(
            ScopeDescriptor::assemble(
                &[entry(&["content"], true), entry(&["README.md"], false)],
                &[entry(&["content", "private"], true)],
                &parsed.suffixes
            )
            .unwrap(),
            descriptor
        );
    }
}
