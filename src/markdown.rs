//! Source context and safe presentation of Markdown changes (SPEC 10.2-10.5).
//!
//! This module is the presentation layer of the comparison pipeline. It
//! parses the readable Markdown of both compared versions with
//! `pulldown-cmark` offset iteration and derives, without ever rewriting
//! source bytes:
//!
//! - the structural blocks of one version, as source line ranges
//!   (SPEC 10.2 step 3);
//! - the conservative change groups of one file: which complete canonical
//!   hunks form one passage, which hunks a shared structure couples, and
//!   when independence cannot be established so the whole file is one
//!   decision (SPEC 10.3);
//! - the per-group presentation: a heading with section context, exact
//!   before/after source excerpts, word-level hints where the line pairing
//!   is reliable, and flags that expose link and invisible-byte changes
//!   (SPEC 10.4-10.5).
//!
//! The Git patch stays the one ledger. This module reads the hunk
//! coordinates and body lines the canonical change model already parsed;
//! it returns group proposals and presentation data, never a second diff.

use std::collections::{BTreeMap, BTreeSet};

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag};

use crate::domain::SourceHunk;

/// The largest heading string one presentation carries. Longer section
/// titles truncate with an ellipsis character.
const MAX_HEADING_CHARS: usize = 96;

// ---------------------------------------------------------------------------
// Structure of one version (SPEC 10.2 step 3)
// ---------------------------------------------------------------------------

/// One structural block of one Markdown version, placed by source line
/// range. The range covers the block's own bytes only: a fenced block
/// includes its fence lines, and a list the whole list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockUnit {
    /// The structural kind.
    pub kind: BlockKind,
    /// First source line, one-based.
    pub first_line: u64,
    /// Last source line, one-based, inclusive.
    pub last_line: u64,
    /// The collected inline text of a heading, without its markers.
    pub heading_text: Option<String>,
}

/// The structural kind of one block (SPEC 10.3-10.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    /// A YAML front-matter region.
    FrontMatter,
    /// An ATX or setext heading.
    Heading,
    /// A paragraph of prose.
    Paragraph,
    /// A list, including nested items.
    List,
    /// A fenced or indented code block.
    CodeBlock {
        /// A fenced block carries fence lines; an indented block does not.
        fenced: bool,
    },
    /// A table.
    Table,
    /// A block quotation, including nested content.
    Quote,
    /// A raw HTML block.
    Html,
}

impl BlockKind {
    /// The user-facing label of this kind in headings and reasons.
    pub fn label(self) -> &'static str {
        match self {
            BlockKind::FrontMatter => "front matter",
            BlockKind::Heading => "heading",
            BlockKind::Paragraph => "paragraph",
            BlockKind::List => "list",
            BlockKind::CodeBlock { fenced: true } => "fenced code block",
            BlockKind::CodeBlock { fenced: false } => "indented code block",
            BlockKind::Table => "table",
            BlockKind::Quote => "quotation",
            BlockKind::Html => "raw HTML block",
        }
    }

    /// Says whether this kind is an indivisible supported structure whose
    /// hunks are reviewed together (SPEC 10.3). Paragraphs and headings are
    /// not: prose passages follow the complete hunks, and a heading is its
    /// own passage with hierarchy context. Lists, quotations, and tables
    /// are structural labels but not coupling units: their inner items,
    /// blocks, and rows are the review units, so two distant edits of one
    /// list stay separate decisions, each presented with the structure
    /// visible.
    fn is_indivisible(self) -> bool {
        matches!(
            self,
            BlockKind::FrontMatter | BlockKind::CodeBlock { .. } | BlockKind::Html
        )
    }

    /// Says whether this kind names a supported structure in group reasons
    /// and presentation labels (SPEC 10.3-10.4).
    fn is_structural(self) -> bool {
        matches!(
            self,
            BlockKind::FrontMatter
                | BlockKind::List
                | BlockKind::CodeBlock { .. }
                | BlockKind::Table
                | BlockKind::Quote
                | BlockKind::Html
        )
    }
}

/// The structure of one Markdown version: its blocks in source order and
/// the byte offset each line starts at.
#[derive(Clone, Debug, Default)]
pub struct DocumentStructure {
    /// Every outermost block in source order.
    pub blocks: Vec<BlockUnit>,
    /// Byte offset each line starts at; entry `i` is line `i + 1`.
    pub line_starts: Vec<usize>,
}

/// The parser configuration of the presentation layer. Tables, footnotes,
/// task lists, and YAML front matter are on; every one of them changes
/// which blocks exist, so the choice is fixed for all comparisons.
fn parser_options() -> Options {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_YAML_STYLE_METADATA_BLOCKS);
    options
}

/// Parses the structure of one version. Every position is a source offset
/// of the exact bytes; nothing is normalized or re-serialized.
pub fn analyze(text: &str) -> DocumentStructure {
    let line_starts = line_starts(text.as_bytes());
    let mut blocks: Vec<BlockUnit> = Vec::new();
    let mut open_tags: Vec<Tag> = Vec::new();
    let mut open_heading: Option<usize> = None;
    for (event, range) in Parser::new_ext(text, parser_options()).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                if open_tags.is_empty()
                    && let Some(kind) = outermost_kind(&tag)
                {
                    blocks.push(BlockUnit {
                        kind,
                        first_line: line_of(&line_starts, range.start),
                        last_line: line_of(&line_starts, range.end.saturating_sub(1)),
                        heading_text: None,
                    });
                    if kind == BlockKind::Heading {
                        open_heading = Some(blocks.len() - 1);
                    }
                }
                open_tags.push(tag);
            }
            Event::End(_) => {
                open_tags.pop();
                if open_tags.is_empty() {
                    open_heading = None;
                }
            }
            // Only inline children exist inside a heading, so any text of
            // an open heading extends its title.
            Event::Text(value) => {
                if let Some(index) = open_heading {
                    blocks[index]
                        .heading_text
                        .get_or_insert_with(String::new)
                        .push_str(&value);
                }
            }
            _ => {}
        }
    }
    DocumentStructure {
        blocks,
        line_starts,
    }
}

/// The block kind of one outermost container, or `None` for the inner
/// containers that only appear nested inside another block.
fn outermost_kind(tag: &Tag) -> Option<BlockKind> {
    match tag {
        Tag::Paragraph => Some(BlockKind::Paragraph),
        Tag::Heading { .. } => Some(BlockKind::Heading),
        Tag::List(_) => Some(BlockKind::List),
        Tag::CodeBlock(kind) => Some(BlockKind::CodeBlock {
            fenced: !matches!(kind, CodeBlockKind::Indented),
        }),
        Tag::Table(_) => Some(BlockKind::Table),
        Tag::BlockQuote(_) => Some(BlockKind::Quote),
        Tag::HtmlBlock => Some(BlockKind::Html),
        Tag::MetadataBlock(_) => Some(BlockKind::FrontMatter),
        // Inner containers: items, rows, cells, and the blocks inside a
        // quotation, a footnote, or a definition list. The outermost
        // container already covers their range.
        Tag::Item
        | Tag::TableHead
        | Tag::TableRow
        | Tag::TableCell
        | Tag::FootnoteDefinition(_)
        | Tag::DefinitionList
        | Tag::DefinitionListTitle
        | Tag::DefinitionListDefinition => None,
        // Inline containers never open a block of their own.
        _ => None,
    }
}

/// The byte offsets each line of `bytes` starts at. The first line always
/// starts at zero, so an empty document has exactly one entry.
fn line_starts(bytes: &[u8]) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(
        bytes
            .iter()
            .enumerate()
            .filter(|&(_, &byte)| byte == b'\n')
            .map(|(index, _)| index + 1),
    );
    starts
}

/// The one-based line that byte `offset` belongs to. A newline belongs to
/// the line it terminates.
fn line_of(line_starts: &[usize], offset: usize) -> u64 {
    line_starts
        .iter()
        .filter(|&&start| start <= offset)
        .count()
        .max(1) as u64
}

/// The byte span of lines `first` through `last`, both one-based and
/// inclusive, including every terminated line's newline.
fn line_span(text: &[u8], line_starts: &[usize], first: u64, last: u64) -> std::ops::Range<usize> {
    let start = line_starts
        .get(first as usize - 1)
        .copied()
        .unwrap_or(text.len());
    let end = line_starts
        .get(last as usize)
        .copied()
        .unwrap_or(text.len());
    start..end.max(start)
}

// ---------------------------------------------------------------------------
// Changed lines of the canonical hunks (SPEC 10.1)
// ---------------------------------------------------------------------------

/// The changed lines of one canonical hunk, with their one-based line
/// numbers in each version. Context lines are not kept: grouping follows
/// the lines that actually changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HunkLines {
    /// Zero-based index of the hunk inside the file's patch.
    pub index: usize,
    /// Removed lines: B line number and the line without its `-` prefix.
    pub removed: Vec<(u64, String)>,
    /// Added lines: C line number and the line without its `+` prefix.
    pub added: Vec<(u64, String)>,
}

/// Derives the changed lines of every hunk from the file's complete patch
/// bytes and its parsed hunks. The hunk bodies of UTF-8 content are UTF-8,
/// and the canonical model already validated each body against its
/// declared counts.
pub fn hunk_lines(patch: &[u8], hunks: &[SourceHunk]) -> Vec<HunkLines> {
    let mut result = Vec::with_capacity(hunks.len());
    for (index, hunk) in hunks.iter().enumerate() {
        let body = &patch[hunk.offset..hunk.offset + hunk.length];
        let mut lines = HunkLines {
            index,
            ..HunkLines::default()
        };
        let mut old_line = hunk.old_start;
        let mut new_line = hunk.new_start;
        let mut first = true;
        for (_, line) in raw_lines(body) {
            if first {
                // The `@@` header itself.
                first = false;
                continue;
            }
            match line.first() {
                Some(b' ') => {
                    old_line = old_line.saturating_add(1);
                    new_line = new_line.saturating_add(1);
                }
                Some(b'-') => {
                    lines.removed.push((old_line, decode_body_line(&line[1..])));
                    old_line = old_line.saturating_add(1);
                }
                Some(b'+') => {
                    lines.added.push((new_line, decode_body_line(&line[1..])));
                    new_line = new_line.saturating_add(1);
                }
                _ => {}
            }
        }
        result.push(lines);
    }
    result
}

/// Decodes one hunk body line. The compared file is valid UTF-8, so its
/// patch lines are too; a byte sequence that is not would be a contract
/// violation rather than data to guess about.
fn decode_body_line(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("the hunk body of UTF-8 content is UTF-8")
}

/// Yields every line of a byte string with its offset, without its trailing
/// newline.
fn raw_lines(data: &[u8]) -> RawLines<'_> {
    RawLines { data, offset: 0 }
}

/// Iterator behind [`raw_lines`].
struct RawLines<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> Iterator for RawLines<'a> {
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

// ---------------------------------------------------------------------------
// Safe change groups (SPEC 10.3)
// ---------------------------------------------------------------------------

/// The grouping proposal of one changed file: independent passages of
/// complete hunks, or the whole file as one decision with its reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileGrouping {
    /// The file is one decision. The reason is shown when the group is
    /// larger than an expected passage.
    WholeFile {
        /// Why independence cannot be established, or why the unit is the
        /// file.
        reason: String,
    },
    /// Independent passages whose complete hunks partition the patch.
    Groups {
        /// The passages in source order.
        groups: Vec<ProposedGroup>,
    },
}

/// One proposed passage: complete canonical hunks reviewed and restored
/// together, with the reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProposedGroup {
    /// Zero-based hunk indexes, ascending.
    pub hunks: Vec<usize>,
    /// Why these hunks form one passage.
    pub reason: String,
}

/// Groups the complete hunks of one readable file into safe passages
/// (SPEC 10.3).
///
/// Both versions are the exact source text. The rules, most conservative
/// first:
///
/// 1. A changed reference-link definition, footnote definition, or footnote
///    reference makes the whole document one decision: the relation between
///    a label and its uses cannot be separated from both versions alone.
/// 2. A touched raw-HTML block whose tags do not balance is an unparseable
///    structural change, so the whole file is the decision.
/// 3. Hunks that modify the same indivisible structure — a list, a code
///    block, a table, a quotation, a front-matter region, a raw HTML block
///    — in either version are coupled.
/// 4. A passage removed in one hunk and inserted in another is one coupled
///    move; a plausible visual alignment is not evidence for an
///    independent restoration.
///
/// Everything else is prose: each complete hunk is one passage, presented
/// with paragraph context. No hunk is ever split.
pub fn group_changes(before: &str, after: &str, hunks: &[HunkLines]) -> FileGrouping {
    if hunks.is_empty() {
        return FileGrouping::WholeFile {
            reason: "no readable hunks: the file is one decision".to_string(),
        };
    }

    // Rule 1: reference-link and footnote relations.
    let relation_reason = concat!(
        "reference-link or footnote relation: independence cannot be ",
        "established from both versions, so the whole document is one ",
        "decision"
    );
    for (_, line) in hunks
        .iter()
        .flat_map(|hunk| hunk.removed.iter().chain(hunk.added.iter()))
    {
        if is_reference_or_footnote_line(line) || contains_footnote_reference(line) {
            return FileGrouping::WholeFile {
                reason: relation_reason.to_string(),
            };
        }
    }

    let before_structure = analyze(before);
    let after_structure = analyze(after);

    // The blocks each hunk touches, in each version.
    let touched_before = touched_blocks(hunks, &before_structure, |hunk| &hunk.removed);
    let touched_after = touched_blocks(hunks, &after_structure, |hunk| &hunk.added);

    // Rule 2: unbalanced raw HTML in a touched HTML block.
    for (structure, touched, text) in [
        (&before_structure, &touched_before, before),
        (&after_structure, &touched_after, after),
    ] {
        for blocks in touched {
            if html_breaks_balance(structure, blocks, text) {
                return FileGrouping::WholeFile {
                    reason: concat!(
                        "unparseable structural change: the raw HTML of a touched ",
                        "block does not balance, so the whole file is one decision"
                    )
                    .to_string(),
                };
            }
        }
    }

    // Rule 3: hunks that touch the same indivisible block — a code block,
    // a front-matter region, or a raw HTML block.
    let mut unions = UnionFind::new(hunks.len());
    let mut block_owners: BTreeMap<(Side, usize), usize> = BTreeMap::new();
    for (side, structure, touched) in [
        (Side::Before, &before_structure, &touched_before),
        (Side::After, &after_structure, &touched_after),
    ] {
        for (index, blocks) in touched.iter().enumerate() {
            for block in blocks {
                if !structure.blocks[*block].kind.is_indivisible() {
                    continue;
                }
                match block_owners.get(&(side, *block)) {
                    Some(owner) => unions.union(*owner, index),
                    None => {
                        block_owners.insert((side, *block), index);
                    }
                }
            }
        }
    }

    // Rule 4: a passage removed in one hunk and inserted in another.
    let mut move_coupled = vec![false; hunks.len()];
    for (left, hunk) in hunks.iter().enumerate() {
        let removed = significant_lines(&hunk.removed);
        if removed.is_empty() {
            continue;
        }
        for (right, other) in hunks.iter().enumerate() {
            if left != right && !significant_lines(&other.added).is_disjoint(&removed) {
                unions.union(left, right);
                move_coupled[left] = true;
                move_coupled[right] = true;
            }
        }
    }

    // One group per connected component, in source order.
    let mut order: Vec<usize> = Vec::new();
    let mut members: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for index in 0..hunks.len() {
        let root = unions.find(index);
        if !members.contains_key(&root) {
            order.push(root);
        }
        members.entry(root).or_default().push(index);
    }
    let groups = order
        .into_iter()
        .map(|root| {
            let indexes = &members[&root];
            let mut kinds: Vec<BlockKind> = Vec::new();
            for &index in indexes {
                let sides = [
                    (&before_structure, &touched_before[index]),
                    (&after_structure, &touched_after[index]),
                ];
                for (structure, touched) in sides {
                    for &block in touched {
                        let kind = structure.blocks[block].kind;
                        if kind.is_structural() && !kinds.contains(&kind) {
                            kinds.push(kind);
                        }
                    }
                }
            }
            let moved = indexes.iter().any(|&index| move_coupled[index]);
            let first_kind = indexes.iter().find_map(|&index| {
                hunks[index]
                    .added
                    .first()
                    .and_then(|(line, _)| block_kind_at(&after_structure, *line))
                    .or_else(|| {
                        hunks[index]
                            .removed
                            .first()
                            .and_then(|(line, _)| block_kind_at(&before_structure, *line))
                    })
            });
            ProposedGroup {
                hunks: indexes.clone(),
                reason: group_reason(moved, &kinds, first_kind),
            }
        })
        .collect();
    FileGrouping::Groups { groups }
}

/// The side of a comparison a block belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Side {
    /// The baseline version B.
    Before,
    /// The candidate version C.
    After,
}

/// The reason one group exists, from its coupling and the structures it
/// touches (SPEC 10.3).
fn group_reason(moved: bool, indivisible: &[BlockKind], first_kind: Option<BlockKind>) -> String {
    if moved && indivisible.len() > 1 {
        "coupled move and structures: distant hunks form one decision".to_string()
    } else if moved {
        concat!(
            "coupled move: a passage removed in one hunk and inserted in ",
            "another is one decision"
        )
        .to_string()
    } else if indivisible.len() > 1 {
        let labels: Vec<&str> = indivisible.iter().map(|kind| kind.label()).collect();
        format!(
            "coupled structures: {} change together",
            labels.join(" and ")
        )
    } else if let Some(kind) = indivisible.first() {
        match kind {
            BlockKind::FrontMatter => "front-matter region: the metadata block changes as one unit",
            BlockKind::List => {
                "complete list block: hunks that modify one list are reviewed together"
            }
            BlockKind::CodeBlock { fenced: true } => {
                "complete fenced code block: hunks that modify one block are reviewed together"
            }
            BlockKind::CodeBlock { fenced: false } => {
                "complete indented code block: the indented run is one unit"
            }
            BlockKind::Table => {
                "complete table block: hunks that modify one table are reviewed together"
            }
            BlockKind::Quote => {
                "complete quotation block: hunks that modify one quote are reviewed together"
            }
            BlockKind::Html => "raw HTML block: the element and its attributes are one decision",
            BlockKind::Heading | BlockKind::Paragraph => {
                "one structure: hunks that modify it are reviewed together"
            }
        }
        .to_string()
    } else if first_kind == Some(BlockKind::Heading) {
        "heading passage: the heading changes with its hierarchy context".to_string()
    } else {
        "one passage: an independent paragraph edit".to_string()
    }
}

/// The blocks each hunk touches on one side.
fn touched_blocks<'h>(
    hunks: &'h [HunkLines],
    structure: &DocumentStructure,
    side: impl Fn(&'h HunkLines) -> &'h Vec<(u64, String)>,
) -> Vec<Vec<usize>> {
    hunks
        .iter()
        .map(|hunk| {
            side(hunk)
                .iter()
                .flat_map(|(line, _)| blocks_at(structure, *line))
                .collect::<BTreeSet<usize>>()
                .into_iter()
                .collect()
        })
        .collect()
}

/// The kind of the block that contains one line, when any does.
fn block_kind_at(structure: &DocumentStructure, line: u64) -> Option<BlockKind> {
    structure
        .blocks
        .iter()
        .find(|block| line >= block.first_line && line <= block.last_line)
        .map(|block| block.kind)
}

/// The block indexes that contain one line.
fn blocks_at(structure: &DocumentStructure, line: u64) -> Vec<usize> {
    structure
        .blocks
        .iter()
        .enumerate()
        .filter(|(_, block)| line >= block.first_line && line <= block.last_line)
        .map(|(index, _)| index)
        .collect()
}

/// A small union-find over hunk indexes.
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(count: usize) -> Self {
        UnionFind {
            parent: (0..count).collect(),
        }
    }

    fn find(&mut self, index: usize) -> usize {
        let mut root = index;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        let mut current = index;
        while self.parent[current] != root {
            let next = self.parent[current];
            self.parent[current] = root;
            current = next;
        }
        root
    }

    fn union(&mut self, left: usize, right: usize) {
        let (left_root, right_root) = (self.find(left), self.find(right));
        if left_root != right_root {
            self.parent[right_root] = left_root;
        }
    }
}

/// The lines that can carry a move: non-blank text only.
fn significant_lines(lines: &[(u64, String)]) -> BTreeSet<&str> {
    lines
        .iter()
        .map(|(_, line)| line.as_str())
        .filter(|line| !line.trim().is_empty())
        .collect()
}

/// Says whether one line is a link reference definition or a footnote
/// definition, conservatively: up to three leading spaces, a non-empty
/// bracketed label, and a colon right after the closing bracket. A false
/// positive only enlarges the decision to the whole document.
fn is_reference_or_footnote_line(line: &str) -> bool {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return false;
    }
    let rest = &line[indent..];
    let Some(label_end) = rest.find(']') else {
        return false;
    };
    rest.starts_with('[') && label_end >= 2 && rest[label_end + 1..].starts_with(':')
}

/// Says whether one line carries a footnote reference such as `[^1]`.
fn contains_footnote_reference(line: &str) -> bool {
    match line.find("[^") {
        Some(start) => line[start + 2..].contains(']'),
        None => false,
    }
}

/// Says whether any of `blocks` is a raw HTML block whose tags do not
/// balance.
fn html_breaks_balance(structure: &DocumentStructure, blocks: &[usize], text: &str) -> bool {
    blocks.iter().any(|&index| {
        let block = &structure.blocks[index];
        if block.kind != BlockKind::Html {
            return false;
        }
        let span = line_span(
            text.as_bytes(),
            &structure.line_starts,
            block.first_line,
            block.last_line,
        );
        let block_text = &text[span];
        contains_tag_syntax(block_text) && !html_is_balanced(block_text)
    })
}

/// Says whether `text` contains something that reads like an HTML tag or
/// comment: `<` followed by a letter, a slash, an exclamation mark, or a
/// question mark.
fn contains_tag_syntax(text: &str) -> bool {
    text.char_indices().any(|(index, character)| {
        character == '<'
            && text[index + 1..]
                .chars()
                .next()
                .is_some_and(|next| next.is_ascii_alphabetic() || matches!(next, '/' | '!' | '?'))
    })
}

/// The HTML elements that never take a closing tag.
const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Says whether the raw HTML tags of `text` balance. This is a display
/// arbiter, not an HTML validator: an unmatched, mismatched, or
/// unterminated tag means the block cannot be aligned to structure safely,
/// which enlarges the decision. Markdown text that merely looks like a
/// tag, such as an autolink `<https://host/>`, is not a tag here.
fn html_is_balanced(text: &str) -> bool {
    let characters: Vec<char> = text.chars().collect();
    let mut position = 0;
    let mut open: Vec<String> = Vec::new();
    while let Some(offset) = characters[position..].iter().position(|&c| c == '<') {
        let start = position + offset;
        let rest = &characters[start + 1..];
        if rest.first() == Some(&'!') {
            let skip_to = if rest.starts_with(&['!', '-', '-']) {
                find_sequence(&characters[start + 4..], &['-', '-', '>'])
                    .map(|end| start + 4 + end + 3)
            } else {
                rest.iter()
                    .position(|&c| c == '>')
                    .map(|end| start + 1 + end + 1)
            };
            match skip_to {
                Some(next) => position = next,
                None => return false,
            }
            continue;
        }
        if rest.first() == Some(&'?') {
            match rest.iter().position(|&c| c == '>') {
                Some(end) => position = start + 1 + end + 1,
                None => return false,
            }
            continue;
        }
        let closing = rest.first() == Some(&'/');
        let name_rest = if closing { &rest[1..] } else { rest };
        let name_end = name_rest
            .iter()
            .position(|c| !(c.is_ascii_alphanumeric() || *c == '-'))
            .unwrap_or(name_rest.len());
        let name: String = name_rest[..name_end].iter().collect();
        if name.is_empty() || !name.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
            // Not a tag, as with an autolink; keep scanning after the `<`.
            position = start + 1;
            continue;
        }
        let after_name = name_rest.get(name_end);
        if !closing && !matches!(after_name, Some(' ') | Some('\t') | Some('>') | Some('/')) {
            // A name that other characters immediately follow, as in an
            // autolink destination, is not markup.
            position = start + 1;
            continue;
        }
        // Find the tag's end, honoring quoted attribute values.
        let mut cursor = start + 1 + usize::from(closing) + name_end;
        let mut quote: Option<char> = None;
        let mut closed = false;
        while cursor < characters.len() {
            let character = characters[cursor];
            match quote {
                Some(open_quote) if character == open_quote => quote = None,
                Some(_) => {}
                None if character == '"' || character == '\'' => quote = Some(character),
                None if character == '>' => {
                    closed = true;
                    break;
                }
                None => {}
            }
            cursor += 1;
        }
        if !closed {
            return false;
        }
        let self_closed = characters[cursor - 1] == '/';
        position = cursor + 1;
        let lower = name.to_ascii_lowercase();
        if closing {
            match open.pop() {
                Some(pending) if pending == lower => {}
                _ => return false,
            }
        } else if !self_closed && !VOID_ELEMENTS.contains(&lower.as_str()) {
            open.push(lower);
        }
    }
    open.is_empty()
}

/// The first index where `sequence` appears in `characters`.
fn find_sequence(characters: &[char], sequence: &[char]) -> Option<usize> {
    characters
        .windows(sequence.len())
        .position(|window| window == sequence)
}

// ---------------------------------------------------------------------------
// Presentation (SPEC 10.2 steps 5 and 7, 10.4-10.5)
// ---------------------------------------------------------------------------

/// Why one presentation covers the whole file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentationScope {
    /// One passage of the document.
    Passage,
    /// The whole document is one decision.
    WholeFile,
    /// An added file: the before side is absent.
    AddedFile,
    /// A deleted file: the after side is absent.
    DeletedFile,
}

/// The bounded presentation of one change group (SPEC 7.2, 10.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupPresentation {
    /// A human-readable heading with section context.
    pub heading: String,
    /// The exact before excerpt.
    pub before: SourceExcerpt,
    /// The exact after excerpt.
    pub after: SourceExcerpt,
    /// Word-level hints where the line pairing is reliable.
    pub word_hints: Vec<WordHint>,
    /// Every byte difference is whitespace.
    pub whitespace_only: bool,
    /// A changed line carries link syntax.
    pub link_change: bool,
    /// A changed line carries trailing whitespace, a tab, or an invisible
    /// character.
    pub invisible_change: bool,
}

/// An exact source excerpt of one group, placed by line range.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceExcerpt {
    /// First line, one-based; zero when the side holds no line.
    pub first_line: u64,
    /// Last line, one-based; zero when the side holds no line.
    pub last_line: u64,
    /// The exact bytes of those lines, newlines included.
    pub bytes: Vec<u8>,
}

/// One word-level hint: the changed middles of a paired line, with the
/// shared context around them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WordHint {
    /// The paired line in B.
    pub before_line: u64,
    /// The paired line in C.
    pub after_line: u64,
    /// The shared text before the change.
    pub prefix: String,
    /// What B held between prefix and suffix.
    pub removed: String,
    /// What C holds between prefix and suffix.
    pub added: String,
    /// The shared text after the change.
    pub suffix: String,
}

/// Builds the presentation of one group from the exact source of both
/// versions. The excerpts are byte slices of the sources; nothing is
/// rewrapped, normalized, or re-serialized (SPEC 10.5).
pub fn group_presentation(
    scope: PresentationScope,
    before: &str,
    after: &str,
    before_structure: &DocumentStructure,
    after_structure: &DocumentStructure,
    hunks: &[HunkLines],
    group_hunks: &[usize],
) -> GroupPresentation {
    let selected: Vec<&HunkLines> = group_hunks.iter().map(|&index| &hunks[index]).collect();

    // A passage covers its changed lines; a whole-file decision previews
    // the complete side (SPEC 10.4).
    let (before_span, after_span) = match scope {
        PresentationScope::Passage => (
            changed_span(&selected, |hunk| &hunk.removed),
            changed_span(&selected, |hunk| &hunk.added),
        ),
        _ => (whole_span(before), whole_span(after)),
    };
    let before_excerpt = excerpt(before, before_structure, before_span);
    let after_excerpt = excerpt(after, after_structure, after_span);

    let removed: Vec<&(u64, String)> = selected
        .iter()
        .flat_map(|hunk| hunk.removed.iter())
        .collect();
    let added: Vec<&(u64, String)> = selected.iter().flat_map(|hunk| hunk.added.iter()).collect();
    let changed: Vec<&str> = removed
        .iter()
        .chain(added.iter())
        .map(|(_, line)| line.as_str())
        .collect();

    GroupPresentation {
        heading: heading(
            scope,
            before_structure,
            after_structure,
            before_span,
            after_span,
            &selected,
        ),
        before: before_excerpt,
        after: after_excerpt,
        word_hints: word_hints(&removed, &added),
        whitespace_only: without_whitespace_lines(&removed) == without_whitespace_lines(&added),
        link_change: changed.iter().any(|line| {
            line.contains("](")
                || line.contains("][")
                || contains_footnote_reference(line)
                || is_reference_or_footnote_line(line)
        }),
        invisible_change: changed.iter().any(|line| {
            *line != line.trim_end() || line.contains('\t') || line.chars().any(is_invisible)
        }),
    }
}

/// The span of every line of one side; `(0, 0)` when the side is empty.
fn whole_span(text: &str) -> SideSpan {
    if text.is_empty() {
        (0, 0)
    } else {
        (1, line_count(text.as_bytes()))
    }
}

/// The presentation of a file whose content is opaque: no readable text to
/// align, so the exact bytes and their line counts are the presentation
/// (SPEC 10.4).
pub fn opaque_presentation(
    scope: PresentationScope,
    before: Option<&[u8]>,
    after: Option<&[u8]>,
) -> GroupPresentation {
    let opaque_side = |bytes: Option<&[u8]>| SourceExcerpt {
        first_line: u64::from(bytes.is_some()),
        last_line: line_count(bytes.unwrap_or_default()),
        bytes: bytes.unwrap_or_default().to_vec(),
    };
    GroupPresentation {
        heading: match scope {
            PresentationScope::AddedFile => "Added document (opaque content)".to_string(),
            PresentationScope::DeletedFile => "Deleted document (opaque content)".to_string(),
            _ => "Whole document (opaque content)".to_string(),
        },
        before: opaque_side(before),
        after: opaque_side(after),
        word_hints: Vec::new(),
        whitespace_only: false,
        link_change: false,
        invisible_change: true,
    }
}

/// The number of lines of `bytes`: every newline-terminated line plus a
/// final unterminated line.
fn line_count(bytes: &[u8]) -> u64 {
    let newlines = bytes.iter().filter(|&&byte| byte == b'\n').count() as u64;
    if bytes.is_empty() || bytes.last() == Some(&b'\n') {
        newlines
    } else {
        newlines + 1
    }
}

/// The first and last changed line of the selected hunks of one side;
/// `(0, 0)` when the side holds no changed line.
type SideSpan = (u64, u64);

/// The line span one side of a group covers, from the first through the
/// last changed line of its hunks.
fn changed_span(
    selected: &[&HunkLines],
    side: impl Fn(&HunkLines) -> &Vec<(u64, String)>,
) -> SideSpan {
    let mut span: Option<SideSpan> = None;
    for hunk in selected {
        if let (Some(&(first, _)), Some(&(last, _))) = (side(hunk).first(), side(hunk).last()) {
            span = Some(match span {
                None => (first, last),
                Some((low, high)) => (low.min(first), high.max(last)),
            });
        }
    }
    span.unwrap_or((0, 0))
}

/// The exact excerpt of one side, sliced by line span.
fn excerpt(text: &str, structure: &DocumentStructure, span: SideSpan) -> SourceExcerpt {
    if span.1 == 0 {
        return SourceExcerpt::default();
    }
    let range = line_span(text.as_bytes(), &structure.line_starts, span.0, span.1);
    SourceExcerpt {
        first_line: span.0,
        last_line: span.1,
        bytes: text.as_bytes()[range].to_vec(),
    }
}

/// Builds the heading of one group: a unit label, the nearest heading above
/// it in C or B, and — for a passage — the line range of each side.
fn heading(
    scope: PresentationScope,
    before_structure: &DocumentStructure,
    after_structure: &DocumentStructure,
    before_span: SideSpan,
    after_span: SideSpan,
    selected: &[&HunkLines],
) -> String {
    let label = match scope {
        PresentationScope::WholeFile => "Whole document",
        PresentationScope::AddedFile => "Added document",
        PresentationScope::DeletedFile => "Deleted document",
        PresentationScope::Passage => {
            let unit = selected
                .iter()
                .find_map(|hunk| first_changed(&hunk.added, after_structure))
                .or_else(|| {
                    selected
                        .iter()
                        .find_map(|hunk| first_changed(&hunk.removed, before_structure))
                });
            unit.unwrap_or("passage")
        }
    };
    let mut heading = label.to_string();
    if scope == PresentationScope::Passage {
        let section = after_span
            .0
            .checked_sub(1)
            .and_then(|line| heading_above(after_structure, line))
            .or_else(|| {
                before_span
                    .0
                    .checked_sub(1)
                    .and_then(|line| heading_above(before_structure, line))
            });
        if let Some(section) = section {
            heading.push_str(" in ");
            heading.push_str(&section);
        }
        heading.push_str(&format!(
            " (B lines {}-{}, C lines {}-{})",
            before_span.0, before_span.1, after_span.0, after_span.1
        ));
    }
    truncate_heading(&heading)
}

/// The block label of the outermost block that contains a hunk's first
/// changed line on one side.
fn first_changed(lines: &[(u64, String)], structure: &DocumentStructure) -> Option<&'static str> {
    let line = lines.first()?.0;
    structure
        .blocks
        .iter()
        .find(|block| line >= block.first_line && line <= block.last_line)
        .map(|block| block.kind.label())
}

/// The quoted text of the nearest heading at or above one line.
fn heading_above(structure: &DocumentStructure, line: u64) -> Option<String> {
    structure
        .blocks
        .iter()
        .rev()
        .find(|block| block.kind == BlockKind::Heading && block.last_line <= line)
        .and_then(|block| block.heading_text.as_ref())
        .map(|title| format!("“{title}”"))
}

/// Keeps one heading inside its bound.
fn truncate_heading(heading: &str) -> String {
    if heading.chars().count() <= MAX_HEADING_CHARS {
        return heading.to_string();
    }
    let mut cut: String = heading.chars().take(MAX_HEADING_CHARS - 1).collect();
    cut.push('…');
    cut
}

/// The word-level hints of one group, where the removed-to-added line
/// pairing is reliable: equal counts on both sides, and no removed middle
/// that another pair's added middle repeats, which would make the pairing
/// an arbitrary alignment of moved text.
fn word_hints(removed: &[&(u64, String)], added: &[&(u64, String)]) -> Vec<WordHint> {
    if removed.is_empty() || removed.len() != added.len() {
        return Vec::new();
    }
    let middles: Vec<(String, String, String)> = removed
        .iter()
        .zip(added.iter())
        .map(|((_, before), (_, after))| split_common_prefix(before, after))
        .collect();
    for (index, (_, _removed_middle, added_middle)) in middles.iter().enumerate() {
        if added_middle.is_empty() {
            continue;
        }
        let ambiguous = middles
            .iter()
            .enumerate()
            .any(|(other, (_, other_removed, _))| other != index && other_removed == added_middle);
        if ambiguous {
            return Vec::new();
        }
    }
    middles
        .iter()
        .zip(removed.iter().zip(added.iter()))
        .filter_map(
            |((prefix, _, _), ((before_line, before), (after_line, after)))| {
                let before_chars: Vec<char> = before.chars().collect();
                let after_chars: Vec<char> = after.chars().collect();
                let prefix_len = prefix.chars().count();
                let suffix_len = before_chars[prefix_len..]
                    .iter()
                    .rev()
                    .zip(after_chars[prefix_len..].iter().rev())
                    .take_while(|(left, right)| left == right)
                    .count();
                if prefix_len == 0 && suffix_len == 0 {
                    // No shared boundary localizes anything.
                    return None;
                }
                let removed_middle: String = before_chars
                    [prefix_len..before_chars.len() - suffix_len]
                    .iter()
                    .collect();
                let added_middle: String = after_chars[prefix_len..after_chars.len() - suffix_len]
                    .iter()
                    .collect();
                if removed_middle.is_empty() && added_middle.is_empty() {
                    return None;
                }
                Some(WordHint {
                    before_line: *before_line,
                    after_line: *after_line,
                    prefix: prefix.clone(),
                    removed: removed_middle,
                    added: added_middle,
                    suffix: before_chars[before_chars.len() - suffix_len..]
                        .iter()
                        .collect(),
                })
            },
        )
        .collect()
}

/// Splits one paired line into its common character prefix and the two
/// middles that follow, before suffix trimming.
fn split_common_prefix(before: &str, after: &str) -> (String, String, String) {
    let before_chars: Vec<char> = before.chars().collect();
    let after_chars: Vec<char> = after.chars().collect();
    let mut prefix = 0;
    while prefix < before_chars.len()
        && prefix < after_chars.len()
        && before_chars[prefix] == after_chars[prefix]
    {
        prefix += 1;
    }
    (
        before_chars[..prefix].iter().collect(),
        before_chars[prefix..].iter().collect(),
        after_chars[prefix..].iter().collect(),
    )
}

/// One side's changed text with every whitespace character removed.
fn without_whitespace_lines(lines: &[&(u64, String)]) -> String {
    lines
        .iter()
        .flat_map(|(_, line)| line.chars().filter(|c| !c.is_whitespace()))
        .collect()
}

/// Says whether one character is invisible in a terminal: a control
/// character other than tab, a bidirectional or isolation control, or a
/// zero-width character.
fn is_invisible(character: char) -> bool {
    let code = character as u32;
    (character.is_control() && character != '\t')
        || (0x200B..=0x200F).contains(&code)
        || (0x202A..=0x202E).contains(&code)
        || (0x2060..=0x2069).contains(&code)
        || code == 0xFEFF
}

/// Renders one source line so invisible bytes stay reviewable (SPEC 10.4
/// and 10.5): invisible control characters appear as `<U+XXXX>`, and the
/// trailing whitespace run becomes one visible mark per character. The
/// source bytes stay the ledger; this is display only.
pub fn reveal_source(line: &str) -> String {
    let body_end = line.trim_end_matches([' ', '\t']).len();
    let mut revealed = String::with_capacity(line.len());
    for character in line[..body_end].chars() {
        if is_invisible(character) {
            revealed.push_str(&format!("<U+{:04X}>", character as u32));
        } else {
            revealed.push(character);
        }
    }
    for character in line[body_end..].chars() {
        revealed.push(if character == ' ' { '·' } else { '⇥' });
    }
    revealed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wraps two passages in enough filler lines that edits to them form
    /// two hunks. Both sides share the filler text.
    fn distant(top: &str, bottom: &str) -> String {
        let filler: Vec<String> = (1..=12).map(|index| format!("filler {index:02}")).collect();
        format!("{top}\n\n{}\n\n{bottom}\n", filler.join("\n"))
    }

    /// Derives [`HunkLines`] values from recorded line pairs, for inputs
    /// whose exact patch shape the test does not care about.
    fn hunk(index: usize, removed: &[(u64, &str)], added: &[(u64, &str)]) -> HunkLines {
        HunkLines {
            index,
            removed: removed
                .iter()
                .map(|(line, text)| (*line, text.to_string()))
                .collect(),
            added: added
                .iter()
                .map(|(line, text)| (*line, text.to_string()))
                .collect(),
        }
    }

    #[test]
    fn structures_carry_source_line_ranges() {
        let text =
            "---\ntitle: Road\n---\n\nIntro paragraph.\n\n## Results\n\n| a |\n| --- |\n| b |\n";
        let structure = analyze(text);
        let kinds: Vec<(BlockKind, u64, u64)> = structure
            .blocks
            .iter()
            .map(|block| (block.kind, block.first_line, block.last_line))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (BlockKind::FrontMatter, 1, 3),
                (BlockKind::Paragraph, 5, 5),
                (BlockKind::Heading, 7, 7),
                (BlockKind::Table, 9, 11),
            ]
        );
        assert_eq!(structure.blocks[2].heading_text.as_deref(), Some("Results"));
        // The exact line starts are part of the structure.
        assert_eq!(structure.line_starts[4], 21);
    }

    #[test]
    fn distant_list_items_stay_independent_but_one_fence_couples() {
        // One list of 24 items, edited at item 2 and item 23: Git emits two
        // hunks. The list names the structure of each passage, but the two
        // items are separate decisions, as the reviewed corpus records for
        // distant edits of one list family.
        let items: Vec<String> = (1..=24).map(|index| format!("- item {index:02}")).collect();
        let before = items.join("\n") + "\n";
        let mut edited: Vec<String> = items.clone();
        edited[1] = "- item 02 revised".to_string();
        edited[22] = "- item 23 revised".to_string();
        let after = edited.join("\n") + "\n";
        let hunks = vec![
            hunk(0, &[(2, "- item 02")], &[(2, "- item 02 revised")]),
            hunk(1, &[(23, "- item 23")], &[(23, "- item 23 revised")]),
        ];
        let FileGrouping::Groups { groups } = group_changes(&before, &after, &hunks) else {
            panic!("two list edits must form passages");
        };
        assert_eq!(
            groups.len(),
            2,
            "distant items of one list stay independent"
        );
        assert_eq!(groups[0].hunks, vec![0]);
        assert_eq!(groups[1].hunks, vec![1]);
        assert!(groups[0].reason.contains("list"), "{}", groups[0].reason);

        // One fenced code block edited in two distant lines is one
        // decision: the block has no inner review unit (SPEC 10.4).
        let code: Vec<String> = (1..=24).map(|index| format!("line {index:02}")).collect();
        let fence = |body: String| format!("```\n{body}```\n", body = body);
        let mut edited_code = code.clone();
        edited_code[1] = "line 02 revised".to_string();
        edited_code[22] = "line 23 revised".to_string();
        let hunks = vec![
            hunk(0, &[(3, "line 02")], &[(3, "line 02 revised")]),
            hunk(1, &[(24, "line 23")], &[(24, "line 23 revised")]),
        ];
        let FileGrouping::Groups { groups } = group_changes(
            &fence(code.join("\n") + "\n"),
            &fence(edited_code.join("\n") + "\n"),
            &hunks,
        ) else {
            panic!("two fence edits must form passages");
        };
        assert_eq!(groups.len(), 1, "one fenced block couples its two hunks");
        assert_eq!(groups[0].hunks, vec![0, 1]);
        assert!(groups[0].reason.contains("fenced"));
    }

    #[test]
    fn two_lists_stay_independent() {
        let before = distant("- first\n- second", "- one\n- two");
        let after = distant("- alpha\n- beta", "- uno\n- dos");
        let hunks = vec![
            hunk(
                0,
                &[(1, "- first"), (2, "- second")],
                &[(1, "- alpha"), (2, "- beta")],
            ),
            hunk(
                1,
                &[(16, "- one"), (17, "- two")],
                &[(16, "- uno"), (17, "- dos")],
            ),
        ];
        let FileGrouping::Groups { groups } = group_changes(&before, &after, &hunks) else {
            panic!("independent lists must form passages");
        };
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].hunks, vec![0]);
        assert_eq!(groups[1].hunks, vec![1]);
    }

    #[test]
    fn a_moved_passage_couples_its_two_hunks() {
        let before = distant("PARA sits up here", "and a word edit");
        let after = distant(
            "filler goes here",
            "and a word edit edited\nPARA sits up here",
        );
        let hunks = vec![
            hunk(0, &[(1, "PARA sits up here")], &[(1, "filler goes here")]),
            hunk(
                1,
                &[(16, "and a word edit")],
                &[(16, "and a word edit edited"), (17, "PARA sits up here")],
            ),
        ];
        let FileGrouping::Groups { groups } = group_changes(&before, &after, &hunks) else {
            panic!("a move must form passages");
        };
        assert_eq!(
            groups.len(),
            1,
            "the deletion and insertion are one decision"
        );
        assert_eq!(groups[0].hunks, vec![0, 1]);
        assert!(groups[0].reason.contains("coupled move"));
    }

    #[test]
    fn reference_and_footnote_relations_fall_back_to_the_whole_file() {
        let before = "Use the [guide][1].\n\n[1]: /old.md\n";
        let after = "Use the [guide][1].\n\n[1]: /new.md\n";
        let hunks = vec![hunk(0, &[(3, "[1]: /old.md")], &[(3, "[1]: /new.md")])];
        let FileGrouping::WholeFile { reason } = group_changes(before, after, &hunks) else {
            panic!("a changed definition must fall back to the whole file");
        };
        assert!(reason.contains("reference-link or footnote relation"));

        // A changed footnote reference falls back as well.
        let hunks = vec![hunk(0, &[(1, "A note.[^1]")], &[(1, "A note.[^1] again")])];
        assert!(matches!(
            group_changes(before, after, &hunks),
            FileGrouping::WholeFile { .. }
        ));
    }

    #[test]
    fn unbalanced_raw_html_falls_back_to_the_whole_file() {
        let balanced = "<div class=\"note\">\ninner\n</div>\n";
        let balanced_after = "<div class=\"warning\">\ninner\n</div>\n";
        let hunks = vec![hunk(
            0,
            &[(1, "<div class=\"note\">")],
            &[(1, "<div class=\"warning\">")],
        )];
        assert!(matches!(
            group_changes(balanced, balanced_after, &hunks),
            FileGrouping::Groups { .. }
        ));

        let malformed = "<div><span>mixed markup\n";
        let malformed_after = "<div><span class=\"x\">mixed markup\n";
        let FileGrouping::WholeFile { reason } = group_changes(malformed, malformed_after, &hunks)
        else {
            panic!("unbalanced HTML must fall back to the whole file");
        };
        assert!(reason.contains("unparseable structural change"));
    }

    #[test]
    fn presentation_reports_exact_offsets_and_word_hints() {
        let before = "# Guide\n\nSee the [changelog](/old-road.md) for details.\n";
        let after = "# Guide\n\nSee the [changelog](/new-road.md) for details.\n";
        let hunks = vec![hunk(
            0,
            &[(3, "See the [changelog](/old-road.md) for details.")],
            &[(3, "See the [changelog](/new-road.md) for details.")],
        )];
        let before_structure = analyze(before);
        let after_structure = analyze(after);
        let presentation = group_presentation(
            PresentationScope::Passage,
            before,
            after,
            &before_structure,
            &after_structure,
            &hunks,
            &[0],
        );

        // The heading names the enclosing section, and the excerpts are
        // exact slices of the sources.
        assert!(presentation.heading.contains("Guide"));
        assert!(presentation.heading.contains("paragraph"));
        assert_eq!(
            presentation.before.bytes,
            b"See the [changelog](/old-road.md) for details.\n"
        );
        assert_eq!(
            presentation.after.bytes,
            b"See the [changelog](/new-road.md) for details.\n"
        );
        assert_eq!(
            (
                presentation.before.first_line,
                presentation.before.last_line
            ),
            (3, 3)
        );

        // The word hint localizes the destination change and leaves the
        // label inside the shared context.
        assert_eq!(presentation.word_hints.len(), 1);
        let hint = &presentation.word_hints[0];
        assert_eq!(hint.prefix, "See the [changelog](/");
        assert_eq!(hint.removed, "old");
        assert_eq!(hint.added, "new");
        assert_eq!(hint.suffix, "-road.md) for details.");
        assert!(presentation.link_change);
        assert!(!presentation.whitespace_only);
    }

    #[test]
    fn whitespace_and_invisible_changes_carry_markers() {
        let before = "verse line\nnext line\n";
        let after = "verse line\u{20}\nnext line\t\n";
        let hunks = vec![hunk(
            0,
            &[(1, "verse line"), (2, "next line")],
            &[(1, "verse line "), (2, "next line\t")],
        )];
        let structure = analyze(before);
        let presentation = group_presentation(
            PresentationScope::Passage,
            before,
            after,
            &structure,
            &structure,
            &hunks,
            &[0],
        );
        assert!(presentation.whitespace_only, "only whitespace changed");
        assert!(presentation.invisible_change);

        // Reveal makes the invisible bytes reviewable without touching the
        // ledger bytes.
        assert_eq!(reveal_source("plain line"), "plain line");
        assert_eq!(reveal_source("trailing  "), "trailing··");
        assert_eq!(reveal_source("tab\tend\t"), "tab\tend⇥");
        assert_eq!(reveal_source("run\u{1b}[31m"), "run<U+001B>[31m");
        assert_eq!(reveal_source("bidi\u{202b}"), "bidi<U+202B>");
    }

    #[test]
    fn reordered_lines_get_no_word_hints() {
        // Every removed middle equals another pair's added middle, so the
        // pairing is an arbitrary alignment of moved text.
        let hunks = vec![hunk(
            0,
            &[(1, "- alpha"), (2, "- beta"), (3, "- gamma")],
            &[(1, "- beta"), (2, "- gamma"), (3, "- alpha")],
        )];
        let structure = analyze("- alpha\n- beta\n- gamma\n");
        let presentation = group_presentation(
            PresentationScope::Passage,
            "- alpha\n- beta\n- gamma\n",
            "- beta\n- gamma\n- alpha\n",
            &structure,
            &structure,
            &hunks,
            &[0],
        );
        assert!(presentation.word_hints.is_empty());
    }

    #[test]
    fn headings_and_front_matter_group_by_structure() {
        // A heading edit is its own passage with hierarchy context.
        let before = "# Report\n\n## Results\n\nBody.\n";
        let after = "# Report\n\n### Results\n\nBody.\n";
        let hunks = vec![hunk(0, &[(3, "## Results")], &[(3, "### Results")])];
        let FileGrouping::Groups { groups } = group_changes(before, after, &hunks) else {
            panic!("a heading edit must form a passage");
        };
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].reason,
            "heading passage: the heading changes with its hierarchy context"
        );

        // A front-matter edit is the metadata region.
        let before = "---\ntitle: Old\n---\n\nBody.\n";
        let after = "---\ntitle: New\n---\n\nBody.\n";
        let hunks = vec![hunk(0, &[(2, "title: Old")], &[(2, "title: New")])];
        let FileGrouping::Groups { groups } = group_changes(before, after, &hunks) else {
            panic!("a front-matter edit must form a passage");
        };
        assert!(groups[0].reason.contains("front-matter"));
    }

    #[test]
    fn hunk_lines_split_bodies_by_prefix() {
        let patch = b"diff --git a/x.md b/x.md\nindex aaa..bbb 100644\n--- a/x.md\n+++ b/x.md\n\
@@ -1,3 +1,3 @@\n context\n-removed\n+added\n more\n\
@@ -9,2 +10,2 @@\n tail\n-final\n\\ No newline at end of file\n+final\n";
        let parsed = crate::domain::parse_source_patch(patch).expect("the patch parses");
        let lines = hunk_lines(patch, &parsed.hunks);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].removed, vec![(2, "removed".to_string())]);
        assert_eq!(lines[0].added, vec![(2, "added".to_string())]);
        assert_eq!(lines[1].removed, vec![(10, "final".to_string())]);
        assert_eq!(lines[1].added, vec![(11, "final".to_string())]);
    }
}
