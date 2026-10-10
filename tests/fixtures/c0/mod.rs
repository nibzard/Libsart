//! The C0 Markdown correctness corpus (SPEC 16.7 and 17.2).
//!
//! C0 is the hand-reviewed fixture set for source comparison, grouping,
//! formatting, Unicode handling, and adversarial content. Every fixture is
//! one document pair (B, C) plus the expectations a correct implementation
//! must reproduce:
//!
//! - exact source changes: the file-level change kind, whether Git emits a
//!   binary patch, and every canonical hunk range under the fixed contract
//!   of SPEC 10.1;
//! - expected grouping or fallback: which complete hunks form independent
//!   passages, or why the whole file is one decision (SPEC 10.3);
//! - expected result trees: the exact bytes of keep-all (C), restore-all
//!   (B), and each single-group restoration, from which stock Git derives
//!   the expected tree;
//! - reader-visible differences: what a person must still be able to see,
//!   because a rendering that hides a destination or hard-break change
//!   fails (SPEC 16.7).
//!
//! The expectations are frozen data, independent of the manifest and
//! grouping code. `tests/c0_corpus.rs` re-derives every recorded value from
//! stock Git on each run, and the restoration slice reuses these fixtures
//! for exact keep/restore checks (SPEC 16.4, task T025).

pub mod adversarial;
pub mod links;
pub mod punctuation;
pub mod structure;
pub mod transitions;
pub mod whitespace;

/// The content classes that SPEC 16.7 requires the corpus to span. The
/// names are slugs of the classes listed there; `moved-paragraphs` covers
/// "duplicate/moved paragraphs".
pub const REQUIRED_CLASSES: &[&str] = &[
    "punctuation",
    "reflow",
    "hard-breaks",
    "tabs",
    "trailing-spaces",
    "final-newline",
    "lists",
    "nested-quotes",
    "headings",
    "links",
    "reference-links",
    "footnotes",
    "tables",
    "code",
    "front-matter",
    "raw-html",
    "moved-paragraphs",
    "multilingual",
    "combining-marks",
    "emoji",
    "bidirectional-controls",
    "terminal-escapes",
    "opaque-bytes",
];

/// Additional classes the corpus carries beyond the required list. They
/// cover the whole-file decision units of SPEC 10.3 and 10.4 that are not
/// content classes: line-ending conversion, file presence, and file mode.
pub const EXTRA_CLASSES: &[&str] = &["line-endings", "file-presence", "file-mode"];

/// The fixture modules in review order.
pub const MODULES: &[&[Fixture]] = &[
    punctuation::FIXTURES,
    whitespace::FIXTURES,
    structure::FIXTURES,
    links::FIXTURES,
    adversarial::FIXTURES,
    transitions::FIXTURES,
];

/// Returns the number of reviewed source pairs in the corpus.
pub fn count() -> usize {
    MODULES.iter().map(|module| module.len()).sum()
}

/// Iterates over every fixture in the corpus.
pub fn iter() -> impl Iterator<Item = &'static Fixture> {
    MODULES.iter().flat_map(|module| module.iter())
}

/// Returns the fixture with `id`, if present.
pub fn by_id(id: &str) -> Option<&'static Fixture> {
    iter().find(|fixture| fixture.id == id)
}

/// The exact bytes of one side of a pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Content {
    /// Valid UTF-8 text.
    Text(&'static str),
    /// Bytes that are not valid UTF-8; opaque content per SPEC 10.4.
    Opaque(&'static [u8]),
}

impl Content {
    /// Text content.
    pub const fn text(body: &'static str) -> Self {
        Content::Text(body)
    }

    /// Opaque content.
    pub const fn opaque(bytes: &'static [u8]) -> Self {
        Content::Opaque(bytes)
    }

    /// The exact bytes of this content.
    pub fn bytes(&self) -> &'static [u8] {
        match *self {
            Content::Text(body) => body.as_bytes(),
            Content::Opaque(bytes) => bytes,
        }
    }
}

/// One side of a source pair: a present file with its bytes and mode, or an
/// absent file. Only the executable bit of `mode` is tracked by Git, so
/// comparisons use that bit alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// The file exists with these bytes and permission bits.
    Present { content: Content, mode: u32 },
    /// The file does not exist.
    Absent,
}

impl Side {
    /// An absent file.
    pub const ABSENT: Side = Side::Absent;

    /// A present file with text content.
    pub const fn present_text(body: &'static str, mode: u32) -> Side {
        Side::Present {
            content: Content::Text(body),
            mode,
        }
    }

    /// A present file with opaque content.
    pub const fn present_opaque(bytes: &'static [u8], mode: u32) -> Side {
        Side::Present {
            content: Content::Opaque(bytes),
            mode,
        }
    }

    /// The exact bytes of this side, or `None` when the file is absent.
    pub fn bytes(&self) -> Option<&'static [u8]> {
        match self {
            Side::Present { content, .. } => Some(content.bytes()),
            Side::Absent => None,
        }
    }

    /// The permission bits of this side, or `None` when the file is absent.
    pub fn mode(&self) -> Option<u32> {
        match self {
            Side::Present { mode, .. } => Some(*mode),
            Side::Absent => None,
        }
    }
}

/// One canonical hunk of the fixed-contract patch: the numeric ranges of
/// the `@@` header. Any section hint after the second `@@` is display
/// decoration and is not recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HunkRange {
    /// First old line.
    pub old_start: u32,
    /// Old line count.
    pub old_count: u32,
    /// First new line.
    pub new_start: u32,
    /// New line count.
    pub new_count: u32,
}

impl HunkRange {
    /// A hunk range read from a `@@` header.
    pub const fn new(old_start: u32, old_count: u32, new_start: u32, new_count: u32) -> Self {
        HunkRange {
            old_start,
            old_count,
            new_start,
            new_count,
        }
    }
}

/// The file-level kind of the canonical change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileChange {
    /// Content lines changed; at least one hunk is present.
    Modified,
    /// The file exists only in C.
    Added,
    /// The file exists only in B.
    Deleted,
    /// Only the permission bits changed; there is no hunk.
    ModeOnly,
}

/// The recorded exact source changes of one pair (SPEC 10.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExpectedChanges {
    /// The file-level change kind.
    pub change: FileChange,
    /// Whether Git emits a `GIT binary patch` body instead of hunks.
    pub binary: bool,
    /// Every canonical hunk, in patch order.
    pub hunks: &'static [HunkRange],
}

/// One safe change group: complete canonical hunks that must be reviewed
/// and restored together (SPEC 10.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Group {
    /// Why these hunks form one passage; shown when a group is larger than
    /// expected.
    pub reason: &'static str,
    /// One-based canonical hunk numbers, ascending, in patch order.
    pub hunks: &'static [usize],
    /// The exact file bytes after restoring only this group and keeping
    /// every other change.
    pub restore: Content,
}

/// The expected grouping, or the whole-file fallback with its reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expectation {
    /// Independent groups whose hunks partition the whole patch.
    Groups {
        /// The passages of this pair.
        groups: &'static [Group],
    },
    /// The whole file is one decision (SPEC 10.3).
    WholeFile {
        /// Why independence cannot be established or the unit is the file.
        reason: &'static str,
    },
}

impl Expectation {
    /// Independent groups.
    pub const fn groups(groups: &'static [Group]) -> Self {
        Expectation::Groups { groups }
    }

    /// A whole-file decision.
    pub const fn whole_file(reason: &'static str) -> Self {
        Expectation::WholeFile { reason }
    }
}

/// One reviewed source pair of the corpus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fixture {
    /// Stable identifier, unique in the corpus.
    pub id: &'static str,
    /// One-line description of the change.
    pub title: &'static str,
    /// Content classes this fixture covers.
    pub classes: &'static [&'static str],
    /// Repository-relative document path. Fixture paths stay single-segment
    /// so the expected result trees derive from one flat tree.
    pub path: &'static str,
    /// The B side: the reviewed baseline document.
    pub before: Side,
    /// The C side: the candidate document.
    pub after: Side,
    /// The exact source changes between B and C.
    pub changes: ExpectedChanges,
    /// The expected grouping or whole-file fallback.
    pub expectation: Expectation,
    /// What a reader must still be able to perceive after the change.
    pub reader: &'static str,
}
