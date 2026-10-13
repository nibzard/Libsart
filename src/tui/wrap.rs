//! Soft wrapping and source-anchor arithmetic for the reading column.
//!
//! The Read view wraps text itself instead of delegating to a widget, so
//! the interface always knows which source line a displayed row came from.
//! That mapping is what lets a resize restore the reading position from a
//! source anchor plus a best-effort display offset instead of a rendered
//! row number (SPEC 11.2), and it keeps the reading measure an exact cell
//! width (SPEC 11.1). Every composed line also carries one semantic role;
//! the role picks the color the surface draws it in, and the words and
//! symbols of the line keep carrying every state on their own (SPEC 11.4).

use unicode_width::UnicodeWidthChar as _;
use unicode_width::UnicodeWidthStr;

/// The semantic role of one composed line. The role picks the style the
/// surface draws the line in; the words and symbols of the line carry
/// every state on their own, so color-off mode loses nothing (SPEC 11.4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RowKind {
    /// Body text with no role beyond itself.
    #[default]
    Text,
    /// A heading: a view title, a group header, a Markdown heading.
    Heading,
    /// An after-side passage of one comparison.
    Added,
    /// A before-side passage of one comparison.
    Removed,
    /// Chrome inside a column: coordinates, reasons, key hints.
    Meta,
    /// A state the writer must notice: stale, unavailable, bounded.
    Warning,
    /// The selected row of one list.
    Emphasis,
}

/// One composed line of one column, before wrapping: the text and the
/// role the surface draws it in (SPEC 11.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComposedLine {
    /// The text of the line.
    pub text: String,
    /// The role the surface draws the line in.
    pub kind: RowKind,
}

impl ComposedLine {
    /// One line of one role.
    fn new(text: impl Into<String>, kind: RowKind) -> ComposedLine {
        ComposedLine {
            text: text.into(),
            kind,
        }
    }

    /// One body-text line.
    pub fn text(text: impl Into<String>) -> ComposedLine {
        Self::new(text, RowKind::Text)
    }

    /// One heading line.
    pub fn heading(text: impl Into<String>) -> ComposedLine {
        Self::new(text, RowKind::Heading)
    }

    /// One after-side passage line.
    pub fn added(text: impl Into<String>) -> ComposedLine {
        Self::new(text, RowKind::Added)
    }

    /// One before-side passage line.
    pub fn removed(text: impl Into<String>) -> ComposedLine {
        Self::new(text, RowKind::Removed)
    }

    /// One chrome line inside a column.
    pub fn meta(text: impl Into<String>) -> ComposedLine {
        Self::new(text, RowKind::Meta)
    }

    /// One line of a state the writer must notice.
    pub fn warning(text: impl Into<String>) -> ComposedLine {
        Self::new(text, RowKind::Warning)
    }

    /// One selected-row line.
    pub fn emphasis(text: impl Into<String>) -> ComposedLine {
        Self::new(text, RowKind::Emphasis)
    }
}

/// One soft-wrapped display row: the text to draw and the one-based source
/// line it came from. A long source line maps to several rows that share
/// its number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayRow {
    /// The one-based line of the source document this row displays.
    pub source_line: u32,
    /// The wrapped text of this row. It never occupies more cells than the
    /// wrap width.
    pub text: String,
    /// The role the surface draws this row in, taken from the composed
    /// line it came from.
    pub kind: RowKind,
}

/// Wraps `text` to at most `width` terminal cells per row.
///
/// Wrapping is greedy: a row takes whole words while they fit, a word that
/// is wider than the whole width is split at the cell boundary, and the
/// spaces at a break are dropped. A width of zero is treated as one cell,
/// so no input can produce an unterminated loop.
pub fn wrap_text(text: &str, width: u16) -> Vec<DisplayRow> {
    wrap_each(text.lines().map(|line| (line, RowKind::Text)), width)
}

/// Wraps one Markdown document to at most `width` cells per row, and marks
/// every row of one `#` heading line as a heading, wrapped continuations
/// included. The marker itself stays visible, so the role only reinforces
/// what the text already states (SPEC 11.4).
pub fn wrap_document(text: &str, width: u16) -> Vec<DisplayRow> {
    wrap_each(
        text.lines()
            .map(|line| (line, if line.starts_with('#') { RowKind::Heading } else { RowKind::Text })),
        width,
    )
}

/// Wraps the composed lines of one column to at most `width` cells per
/// row. Every row one composed line produces keeps that line's role.
pub fn wrap_rows(lines: &[ComposedLine], width: u16) -> Vec<DisplayRow> {
    wrap_each(lines.iter().map(|line| (line.text.as_str(), line.kind)), width)
}

/// Wraps `(text, role)` pairs, one source line per pair.
fn wrap_each<'a, I>(lines: I, width: u16) -> Vec<DisplayRow>
where
    I: Iterator<Item = (&'a str, RowKind)>,
{
    let width = (width as usize).max(1);
    let mut rows = Vec::new();
    for (number, (line, kind)) in lines.enumerate() {
        let source_line = number as u32 + 1;
        wrap_line(line, width, source_line, kind, &mut rows);
    }
    if rows.is_empty() {
        rows.push(DisplayRow {
            source_line: 1,
            text: String::new(),
            kind: RowKind::Text,
        });
    }
    rows
}

/// Wraps one source line into `rows`.
fn wrap_line(line: &str, width: usize, source_line: u32, kind: RowKind, rows: &mut Vec<DisplayRow>) {
    let mut current = String::new();
    let mut cells = 0usize;
    for (slice, is_spaces) in tokens(line) {
        let slice_cells = UnicodeWidthStr::width(slice);
        if cells + slice_cells > width {
            if is_spaces {
                // The space run itself does not fit: it is the break. A
                // run at the start of the row is dropped, never kept as an
                // over-wide blank row.
                if cells > 0 {
                    push_row(rows, source_line, kind, &current);
                    current.clear();
                    cells = 0;
                }
                continue;
            }
            if slice_cells > width {
                // One word wider than the row: split it at the cells that
                // remain, then fill fresh rows with the rest.
                if !current.is_empty() {
                    push_row(rows, source_line, kind, &current);
                }
                let parts = hard_split(slice, width);
                for part in &parts[..parts.len() - 1] {
                    push_row(rows, source_line, kind, part);
                }
                current = parts[parts.len() - 1].clone();
                cells = UnicodeWidthStr::width(current.as_str());
                continue;
            }
            // The next word fits a fresh row: break before it.
            if !current.is_empty() {
                push_row(rows, source_line, kind, &current);
            }
            current = slice.to_string();
            cells = slice_cells;
            continue;
        }
        current.push_str(slice);
        cells += slice_cells;
    }
    push_row(rows, source_line, kind, &current);
}

/// Emits one row with its trailing spaces removed.
fn push_row(rows: &mut Vec<DisplayRow>, source_line: u32, kind: RowKind, text: &str) {
    rows.push(DisplayRow {
        source_line,
        kind,
        text: text.trim_end_matches(' ').to_string(),
    });
}

/// Splits `word` into pieces of at most `width` cells. The input is one
/// word without spaces that is wider than `width`.
fn hard_split(word: &str, width: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut cells = 0usize;
    for character in word.chars() {
        let character_cells = character.width().unwrap_or(0);
        if cells + character_cells > width && !current.is_empty() {
            parts.push(std::mem::take(&mut current));
            cells = 0;
        }
        current.push(character);
        cells += character_cells;
    }
    parts.push(current);
    parts
}

/// Splits one line into runs of spaces and runs of non-space characters.
/// The classification works on bytes because the UTF-8 continuation bytes
/// are never the space byte.
fn tokens(line: &str) -> Vec<(&str, bool)> {
    let bytes = line.as_bytes();
    let mut tokens = Vec::new();
    let mut start = 0usize;
    for index in 1..=bytes.len() {
        let at_end = index == bytes.len();
        let changes = !at_end && (bytes[index] == b' ') != (bytes[start] == b' ');
        if at_end || changes {
            tokens.push((&line[start..index], bytes[start] == b' '));
            start = index;
        }
    }
    tokens
}

/// Finds the first row that displays `anchor` or any later source line.
///
/// A resize restores the reading position with this search: the anchor is
/// the source line the reader was on, not a rendered row number, so
/// re-wrapping at a new width lands on the same passage.
pub fn row_for_anchor(rows: &[DisplayRow], anchor: u32) -> usize {
    rows.iter()
        .position(|row| row.source_line >= anchor)
        .unwrap_or(rows.len().saturating_sub(1))
}

/// Counts the rows that display `anchor`'s source line, starting at `row`.
/// The result bounds how far a best-effort display offset may drift after
/// a resize.
pub fn rows_of_source(rows: &[DisplayRow], row: usize, anchor: u32) -> usize {
    let mut count = 0usize;
    for display in &rows[row..] {
        if display.source_line != anchor {
            break;
        }
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources(rows: &[DisplayRow]) -> Vec<u32> {
        rows.iter().map(|row| row.source_line).collect()
    }

    #[test]
    fn short_lines_stay_one_row_each() {
        let rows = wrap_text("one\ntwo\n", 20);
        assert_eq!(sources(&rows), [1, 2]);
        assert_eq!(rows[0].text, "one");
        assert_eq!(rows[1].text, "two");
    }

    #[test]
    fn words_wrap_at_spaces_and_drop_them() {
        let rows = wrap_text("alpha beta gamma", 11);
        let texts: Vec<&str> = rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(texts, ["alpha beta", "gamma"]);
        assert_eq!(sources(&rows), [1, 1]);
    }

    #[test]
    fn a_word_wider_than_the_row_splits_at_cells() {
        let rows = wrap_text("abcdefghij", 4);
        let texts: Vec<&str> = rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(texts, ["abcd", "efgh", "ij"]);
    }

    #[test]
    fn empty_text_is_one_empty_row() {
        let rows = wrap_text("", 10);
        assert_eq!(sources(&rows), [1]);
        assert_eq!(rows[0].text, "");
    }

    #[test]
    fn blank_source_lines_keep_their_rows() {
        let rows = wrap_text("a\n\nb", 10);
        assert_eq!(sources(&rows), [1, 2, 3]);
        assert_eq!(rows[1].text, "");
    }

    #[test]
    fn zero_width_is_treated_as_one_cell() {
        let rows = wrap_text("ab", 0);
        let texts: Vec<&str> = rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(texts, ["a", "b"]);
    }

    #[test]
    fn wide_characters_count_their_cells() {
        // Four CJK characters occupy eight cells, so a width of five splits
        // after two of them.
        let rows = wrap_text("文字文字", 5);
        let texts: Vec<&str> = rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(texts, ["文字", "文字"]);
    }

    #[test]
    fn anchors_resolve_forward_to_rows() {
        let rows = wrap_text("one two three\nfour", 8);
        // Line 1 wraps into two rows; source line 2 starts at row 2.
        let texts: Vec<&str> = rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(texts, ["one two", "three", "four"]);
        assert_eq!(row_for_anchor(&rows, 2), 2);
        // An anchor past the end clamps to the last row.
        assert_eq!(row_for_anchor(&rows, 9), rows.len() - 1);
        assert_eq!(rows_of_source(&rows, 0, 1), 2);
    }

    #[test]
    fn wrapped_rows_keep_the_kind_of_their_composed_line() {
        let lines = vec![
            ComposedLine::heading("Group one"),
            ComposedLine::removed("before words that wrap"),
            ComposedLine::added("after"),
            ComposedLine::text("body"),
        ];
        let rows = wrap_rows(&lines, 11);
        let kinds: Vec<RowKind> = rows.iter().map(|row| row.kind).collect();
        assert_eq!(
            kinds,
            [
                RowKind::Heading,
                // The removed line wrapped; every piece stays removed.
                RowKind::Removed,
                RowKind::Removed,
                RowKind::Removed,
                RowKind::Added,
                RowKind::Text,
            ],
        );
        let texts: Vec<&str> = rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(
            texts,
            ["Group one", "before", "words that", "wrap", "after", "body"]
        );
    }

    #[test]
    fn documents_mark_hash_headings_including_wrapped_pieces() {
        let rows = wrap_document("# A long heading that wraps\n\nbody text", 12);
        let kinds: Vec<RowKind> = rows.iter().map(|row| row.kind).collect();
        assert_eq!(
            kinds,
            [
                RowKind::Heading,
                RowKind::Heading,
                RowKind::Heading,
                RowKind::Text, // The blank line.
                RowKind::Text,
            ],
        );
        let texts: Vec<&str> = rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(texts, ["# A long", "heading that", "wraps", "", "body text"]);
    }
}
