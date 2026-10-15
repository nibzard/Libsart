// ABOUTME: Reading presentation of GFM tables for the Margin Read view.
// ABOUTME: Pipe rows become aligned grids inside the reading measure, and every row keeps its source anchor.

//! Reading presentation of GFM tables (SPEC 11.1, 11.2, 11.4).
//!
//! The Read view shows an outermost GFM table as an aligned grid instead
//! of raw pipe rows: cells keep their exact source text, padded to the
//! width of their column; the delimiter row becomes one rule of
//! box-drawing characters; and the colons of the delimiter row decide
//! where the padding sits. A table wider than the reading measure wraps
//! inside its cells, never past them. Every display row keeps the
//! one-based source line of the row it presents, so anchors, search, and
//! resize restores keep working; source mode (`s`) keeps the exact pipe
//! rows. A table nested inside a quotation or a list stays source text,
//! because its prefix markers belong to the outer block.

use std::ops::Range;

use pulldown_cmark::{Alignment, Event, Options, Parser, Tag};
use unicode_width::UnicodeWidthStr;

use crate::tui::wrap::{DisplayRow, RowKind, wrap_pieces};

/// The separator drawn between two columns of a row.
const CELL_JOIN: &str = " │ ";

/// The pieces that join two column runs of the header rule.
const RULE_JOIN: &str = "─┼─";

/// Where the padding of one column sits.
#[derive(Clone, Copy)]
enum Align {
    Left,
    Center,
    Right,
}

/// One outermost GFM table: its inclusive source line range, the
/// alignment of each column, and the source span of every cell in
/// document order, row after row.
pub struct Table {
    /// First source line, one-based.
    pub first: u32,
    /// Last source line, one-based, inclusive.
    pub last: u32,
    /// The alignment of each column.
    aligns: Vec<Align>,
    /// The byte span of every cell, the header cells first.
    cells: Vec<Range<usize>>,
}

/// Scans `text` for its outermost GFM tables, in source order. A table
/// nested inside a quotation or a list stays with its outer block.
pub fn scan(text: &str) -> Vec<Table> {
    let starts = line_starts(text.as_bytes());
    let mut tables: Vec<Table> = Vec::new();
    let mut open: Vec<Tag<'_>> = Vec::new();
    let mut current: Option<Table> = None;
    for (event, range) in Parser::new_ext(text, Options::ENABLE_TABLES).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                if open.is_empty()
                    && let Tag::Table(alignments) = &tag
                {
                    current = Some(Table {
                        first: line_of(&starts, range.start),
                        last: line_of(&starts, range.end.saturating_sub(1)),
                        aligns: alignments.iter().map(alignment_of).collect(),
                        cells: Vec::new(),
                    });
                }
                if let Some(table) = current.as_mut()
                    && matches!(&tag, Tag::TableCell)
                {
                    table.cells.push(range.clone());
                }
                open.push(tag);
            }
            Event::End(_) => {
                open.pop();
                if open.is_empty()
                    && let Some(table) = current.take()
                {
                    tables.push(table);
                }
            }
            _ => {}
        }
    }
    tables
}

impl Table {
    /// The display rows of this table, each anchored to the source line
    /// of the row it presents. The header draws first, the rule replaces
    /// the delimiter row, and every body row follows. A scan that lost
    /// every cell falls back to wrapped source rows.
    pub fn rows(&self, text: &str, width: u16, comments: &[Range<usize>]) -> Vec<DisplayRow> {
        let width = width.max(1) as usize;
        let starts = line_starts(text.as_bytes());
        let mut row_lines: Vec<u32> = Vec::new();
        let mut source_rows: Vec<Vec<String>> = Vec::new();
        for span in &self.cells {
            // A cell that pads a row short of its columns carries an empty
            // span at the start of the next line; its text is nothing and
            // the row padding fills its column, so it adds no row.
            if span.start == span.end && starts.binary_search(&span.start).is_ok() {
                continue;
            }
            let line = line_of(&starts, span.start);
            if row_lines.last() != Some(&line) {
                row_lines.push(line);
                source_rows.push(Vec::new());
            }
            source_rows
                .last_mut()
                .expect("a cell always starts a row")
                .push(cell_text(text, span, comments));
        }
        let Some(header_line) = row_lines.first().copied() else {
            return self.source_rows(text, width);
        };
        let columns = self.aligns.len().max(1);
        let mut aligns = self.aligns.clone();
        aligns.resize(columns, Align::Left);
        for cells in &mut source_rows {
            cells.truncate(columns);
            cells.resize(columns, String::new());
        }

        let naturals = column_maxima(&source_rows, UnicodeWidthStr::width);
        let floors = column_maxima(&source_rows, longest_word);
        let available = width.saturating_sub(3 * (columns - 1)).max(1);
        let widths = column_widths(&naturals, &floors, available);

        let mut display: Vec<DisplayRow> = Vec::new();
        for (position, (line, cells)) in row_lines.iter().zip(&source_rows).enumerate() {
            let wrapped: Vec<Vec<String>> = cells
                .iter()
                .zip(&widths)
                .map(|(cell, column)| wrap_pieces(cell, *column))
                .collect();
            let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
            let kind = if position == 0 {
                RowKind::Heading
            } else {
                RowKind::Text
            };
            for step in 0..height {
                let mut row = String::new();
                for column in 0..columns {
                    if column > 0 {
                        row.push_str(CELL_JOIN);
                    }
                    let piece = wrapped
                        .get(column)
                        .and_then(|pieces| pieces.get(step))
                        .map(String::as_str)
                        .unwrap_or("");
                    row.push_str(&padded(piece, widths[column], aligns[column]));
                }
                display.push(DisplayRow {
                    source_line: *line,
                    text: row.trim_end_matches(' ').to_string(),
                    kind,
                    highlights: Vec::new(),
                });
            }
            if position == 0 {
                display.push(DisplayRow {
                    source_line: header_line + 1,
                    text: rule(&widths),
                    kind: RowKind::Meta,
                    highlights: Vec::new(),
                });
            }
        }
        display
    }

    /// The wrapped source rows of the block, when no grid came out of
    /// the scan.
    fn source_rows(&self, text: &str, width: usize) -> Vec<DisplayRow> {
        let starts = line_starts(text.as_bytes());
        let start = starts
            .get(self.first as usize - 1)
            .copied()
            .unwrap_or(text.len());
        let end = starts
            .get(self.last as usize)
            .copied()
            .unwrap_or(text.len())
            .max(start);
        text[start..end]
            .lines()
            .enumerate()
            .flat_map(|(index, line)| {
                wrap_pieces(line.trim_end_matches('\r'), width)
                    .into_iter()
                    .map(move |piece| DisplayRow {
                        source_line: self.first + index as u32,
                        text: piece,
                        kind: RowKind::Text,
                        highlights: Vec::new(),
                    })
            })
            .collect()
    }
}

/// The padding placement one column alignment states; a colon-less
/// delimiter cell keeps its text on the left.
fn alignment_of(alignment: &Alignment) -> Align {
    match alignment {
        Alignment::Center => Align::Center,
        Alignment::Right => Align::Right,
        Alignment::None | Alignment::Left => Align::Left,
    }
}

/// The largest value of `measure` in one column of every row.
fn column_maxima(rows: &[Vec<String>], measure: impl Fn(&str) -> usize) -> Vec<usize> {
    let columns = rows.first().map(Vec::len).unwrap_or(0);
    (0..columns)
        .map(|column| {
            rows.iter()
                .map(|cells| measure(&cells[column]))
                .max()
                .unwrap_or(0)
        })
        .collect()
}

/// The width of the longest space-free run of one cell.
fn longest_word(cell: &str) -> usize {
    cell.split(' ')
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0)
}

/// The width each column receives inside `available` cells. Every column
/// holds at least its longest word; the space that remains goes to the
/// columns by how much more they want, the largest remainder first.
/// When the longest words alone exceed the space, they scale down to
/// their share and the wrapper splits words, as it does for prose.
fn column_widths(naturals: &[usize], floors: &[usize], available: usize) -> Vec<usize> {
    let floor_sum: usize = floors.iter().sum();
    if floor_sum > available {
        let mut widths: Vec<usize> = floors
            .iter()
            .map(|floor| (floor * available / floor_sum).max(1))
            .collect();
        let mut excess = widths.iter().sum::<usize>().saturating_sub(available);
        while excess > 0 {
            let widest = widths
                .iter()
                .enumerate()
                .max_by_key(|(index, width)| (*width, std::cmp::Reverse(*index)))
                .map(|(index, _)| index)
                .expect("a table has at least one column");
            if widths[widest] <= 1 {
                break;
            }
            widths[widest] -= 1;
            excess -= 1;
        }
        return widths;
    }
    let mut widths = floors.to_vec();
    let spare = available - floor_sum;
    let wants: Vec<usize> = naturals
        .iter()
        .zip(floors)
        .map(|(natural, floor)| natural.saturating_sub(*floor))
        .collect();
    let want_sum: usize = wants.iter().sum();
    if want_sum == 0 || spare == 0 {
        return widths;
    }
    let mut shares: Vec<usize> = wants
        .iter()
        .map(|want| (*want).min(want * spare / want_sum))
        .collect();
    let mut leftover = spare - shares.iter().sum::<usize>();
    let mut order: Vec<usize> = (0..wants.len()).collect();
    order.sort_by_key(|&index| std::cmp::Reverse(wants[index] * spare % want_sum));
    while leftover > 0 {
        let mut gave = false;
        for &index in &order {
            if leftover == 0 {
                break;
            }
            if shares[index] < wants[index] {
                shares[index] += 1;
                leftover -= 1;
                gave = true;
            }
        }
        if !gave {
            break;
        }
    }
    for (index, share) in shares.iter().enumerate() {
        widths[index] += share;
    }
    widths
}

/// One cell piece padded to its column width; the alignment of the
/// column decides where the padding sits.
fn padded(piece: &str, width: usize, align: Align) -> String {
    let fill = width.saturating_sub(UnicodeWidthStr::width(piece));
    let left = match align {
        Align::Left => 0,
        Align::Right => fill,
        Align::Center => fill / 2,
    };
    format!("{}{piece}{}", " ".repeat(left), " ".repeat(fill - left))
}

/// The header rule: one dash run per column, joined under the row
/// separators.
fn rule(widths: &[usize]) -> String {
    widths
        .iter()
        .map(|width| "─".repeat(*width))
        .collect::<Vec<_>>()
        .join(RULE_JOIN)
}

/// The display text of one cell: the exact source bytes of its span,
/// with hidden comment spans removed and an escaped pipe shown as the
/// pipe it means.
fn cell_text(text: &str, span: &Range<usize>, comments: &[Range<usize>]) -> String {
    let cell = text.get(span.clone()).unwrap_or("");
    let shown = without_comments(cell, comments, span.start);
    unescape_pipes(shown.trim())
}

/// The cell text with every hidden comment span removed. Comment spans
/// are absolute byte offsets; `base` is the absolute offset of `cell`.
fn without_comments(cell: &str, comments: &[Range<usize>], base: usize) -> String {
    let end = base + cell.len();
    let mut shown = String::with_capacity(cell.len());
    let mut cursor = base;
    for comment in comments
        .iter()
        .filter(|span| span.end > base && span.start < end)
    {
        if comment.start > cursor {
            shown.push_str(&cell[cursor - base..comment.start.min(end) - base]);
        }
        cursor = cursor.max(comment.end.min(end));
    }
    shown.push_str(&cell[(cursor - base).min(cell.len())..]);
    shown
}

/// Shows `\|` as the pipe it means; every other character stays.
fn unescape_pipes(cell: &str) -> String {
    let mut text = String::with_capacity(cell.len());
    let mut escaped = false;
    for character in cell.chars() {
        if escaped {
            escaped = false;
            if character != '|' {
                text.push('\\');
            }
            text.push(character);
        } else if character == '\\' {
            escaped = true;
        } else {
            text.push(character);
        }
    }
    if escaped {
        text.push('\\');
    }
    text
}

/// The byte offsets each line of `bytes` starts at; entry `i` is line
/// `i + 1`.
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

/// The one-based line that byte `offset` belongs to.
fn line_of(starts: &[usize], offset: usize) -> u32 {
    starts
        .iter()
        .filter(|&&start| start <= offset)
        .count()
        .max(1) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::wrap::reading_document;

    fn texts(page: &[DisplayRow]) -> Vec<&str> {
        page.iter().map(|row| row.text.as_str()).collect()
    }

    #[test]
    fn a_table_renders_as_an_aligned_grid_with_anchored_rows() {
        let text = "# Guide\n\n| Feature | State |\n| --- | --- |\n| tables | aligned |\n| lists | prose |\n";
        let tables = scan(text);
        assert_eq!(tables.len(), 1);
        assert_eq!((tables[0].first, tables[0].last), (3, 6));
        let page = tables[0].rows(text, 40, &[]);
        assert_eq!(
            texts(&page),
            vec![
                "Feature │ State",
                "────────┼────────",
                "tables  │ aligned",
                "lists   │ prose",
            ]
        );
        let lines: Vec<u32> = page.iter().map(|row| row.source_line).collect();
        assert_eq!(lines, vec![3, 4, 5, 6]);
        let kinds: Vec<RowKind> = page.iter().map(|row| row.kind).collect();
        assert_eq!(
            kinds,
            vec![
                RowKind::Heading,
                RowKind::Meta,
                RowKind::Text,
                RowKind::Text
            ]
        );
    }

    #[test]
    fn delimiter_colons_move_the_padding() {
        let text = "| Item | Price |\n| :--- | ---: |\n| bolt | 1.25 |\n| nut | 22.50 |\n";
        let page = scan(text)[0].rows(text, 40, &[]);
        assert_eq!(
            texts(&page),
            vec![
                "Item │ Price",
                "─────┼──────",
                "bolt │  1.25",
                "nut  │ 22.50",
            ]
        );
    }

    #[test]
    fn a_wide_table_wraps_inside_its_cells() {
        let text = "| wide words here | second column |\n| --- | --- |\n| a | b |\n";
        let page = scan(text)[0].rows(text, 18, &[]);
        assert_eq!(
            texts(&page),
            vec![
                "wide    │ second",
                "words   │ column",
                "here    │",
                "────────┼─────────",
                "a       │ b",
            ]
        );
        for row in &page {
            assert!(
                unicode_width::UnicodeWidthStr::width(row.text.as_str()) <= 18,
                "row {:?} stays inside the measure",
                row.text
            );
        }
    }

    #[test]
    fn escaped_pipes_stay_in_one_cell_and_display_as_pipes() {
        let text = "| a \\| b | c |\n| --- | --- |\n| d | e |\n";
        let page = scan(text)[0].rows(text, 40, &[]);
        assert_eq!(texts(&page), vec!["a | b │ c", "──────┼──", "d     │ e",]);
    }

    #[test]
    fn inline_comments_hide_inside_cells() {
        let text = "# T\n\n| name | note |\n| --- | --- |\n| keep <!-- gone --> this | ok |\n";
        let page = reading_document(text, 40, false);
        let shown = texts(&page).join("\n");
        assert!(!shown.contains("gone"), "{shown}");
        assert!(shown.contains("keep"), "{shown}");
        assert!(shown.contains("this"), "{shown}");
        assert!(shown.contains("─┼─"), "{shown}");
    }

    #[test]
    fn nested_tables_stay_source_text() {
        let text = "> | a | b |\n> | --- | --- |\n> | c | d |\n";
        assert!(scan(text).is_empty());
        let page = reading_document(text, 40, false);
        let shown = texts(&page).join("\n");
        assert!(shown.contains("> | a | b |"), "{shown}");
        assert!(!shown.contains("─┼─"), "{shown}");
    }

    #[test]
    fn the_reading_document_path_anchors_and_keeps_source_mode() {
        let text = "# T\n| a | b |\n| --- | --- |\n| c | d |\n\nbody\n";
        let page = reading_document(text, 40, false);
        let rule = page
            .iter()
            .find(|row| row.text.contains('┼'))
            .expect("a rule");
        assert_eq!(rule.source_line, 3);
        let header = page
            .iter()
            .find(|row| row.text == "a │ b")
            .expect("a header row");
        assert_eq!(header.source_line, 2);
        let raw = reading_document(text, 40, true);
        assert!(raw.iter().any(|row| row.text == "| --- | --- |"));
        assert!(!raw.iter().any(|row| row.text.contains('┼')));
    }

    #[test]
    fn rows_without_edge_pipes_and_with_missing_cells_align() {
        let text = "a | b\n--- | ---\nonly\n";
        let page = scan(text)[0].rows(text, 20, &[]);
        assert_eq!(texts(&page), vec!["a    │ b", "─────┼──", "only │"]);
    }

    #[test]
    fn wide_characters_count_their_cells() {
        let text = "| 文章 | state |\n| --- | --- |\n| 一 | ok |\n";
        let page = scan(text)[0].rows(text, 30, &[]);
        assert_eq!(
            texts(&page),
            vec!["文章 │ state", "─────┼──────", "一   │ ok",]
        );
    }
}
