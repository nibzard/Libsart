//! Rendering of the terminal surface (SPEC 11.1).
//!
//! Rendering is one pure pass over the shell: [`draw`] prepares the
//! layout for the frame size, then fills the rectangles the layout
//! module computed. Nothing here reads the repository, and nothing here
//! writes state the event loop depends on. The palette stays restrained:
//! dim text for chrome, weight for hierarchy, and words for every state
//! a reader must not deduce from hue (SPEC 11.4). Color-off mode draws
//! the same surface with no decoration at all, so the words and symbols
//! carry everything on their own.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, widgets};

use crate::tui::actions::Action;
use crate::tui::history::PreviewState;
use crate::tui::layout::{self, WidthClass};
use crate::tui::shell::{Focus, Overlay, Shell, View};
use crate::tui::wrap::{self, RowKind};

/// The styles of one frame (SPEC 11.4). Color-off mode replaces every
/// style with the plain one, so a terminal or an assistive setup that
/// cannot carry decoration still reads every state from its words and
/// symbols. The palette stays restrained: one hue per role, basic
/// terminal colors only, so the writer's own theme keeps deciding the
/// exact shades.
#[derive(Clone, Copy)]
struct Palette {
    /// The style of chrome rows.
    dim: Style,
    /// The style of emphasized rows.
    bold: Style,
    selected: Style,
    word: Style,
    /// The style of headings: view titles, group headers, Markdown
    /// headings.
    heading: Style,
    /// The style of after-side passages.
    added: Style,
    /// The style of before-side passages.
    removed: Style,
    /// The style of states the writer must notice.
    warning: Style,
}

impl Palette {
    /// The palette of one run.
    fn of(shell: &Shell<'_>) -> Palette {
        if shell.color_off() {
            Palette {
                dim: Style::new(),
                bold: Style::new(),
                selected: Style::new(),
                word: Style::new(),
                heading: Style::new(),
                added: Style::new(),
                removed: Style::new(),
                warning: Style::new(),
            }
        } else {
            Palette {
                dim: Style::new().dim(),
                bold: Style::new().bold(),
                selected: Style::new().bold().reversed().fg(Color::Blue),
                word: Style::new().bold().reversed(),
                heading: Style::new().bold().fg(Color::Blue),
                added: Style::new().fg(Color::Green),
                removed: Style::new().fg(Color::Red),
                warning: Style::new().fg(Color::Yellow),
            }
        }
    }

    /// The style one row role draws in (SPEC 11.4).
    fn style(&self, kind: RowKind) -> Style {
        match kind {
            RowKind::Text => Style::new(),
            RowKind::Heading => self.heading,
            RowKind::Added => self.added,
            RowKind::Removed => self.removed,
            RowKind::Meta => self.dim,
            RowKind::Warning => self.warning,
            RowKind::Emphasis => self.selected,
        }
    }
}

/// Draws one whole frame of the surface.
pub fn draw(shell: &mut Shell<'_>, frame: &mut Frame<'_>) {
    let area = frame.area();
    shell.prepare_layout(area.width, area.height);
    let shell = &*shell;
    if shell.width_class() == WidthClass::TooSmall {
        draw_too_small(frame, area, shell);
        return;
    }
    let geometry = layout::frame_layout_with_navigator(area, !shell.sidebar_hidden());
    let palette = Palette::of(shell);
    draw_header(frame, geometry.header, shell, palette);
    draw_tabs(frame, geometry.tabs, shell, palette);
    if let Some(navigator) = geometry.navigator {
        draw_navigator(frame, navigator, shell, palette);
    }
    match shell.overlay() {
        Some(Overlay::Menu { .. }) => draw_menu(frame, geometry.reading, shell, palette),
        Some(Overlay::Help) => draw_help(frame, geometry.reading, shell, palette),
        Some(Overlay::Picker { .. }) => draw_picker(frame, geometry.reading, shell, palette),
        Some(Overlay::WorkspaceSearch) => {
            draw_workspace_search(frame, geometry.reading, shell, palette)
        }
        // The search field keeps the current view visible behind it; the
        // context row carries the query (SPEC 11.3).
        _ => match shell.view() {
            View::Read => draw_reading(frame, geometry.reading, shell, palette),
            View::Changes => draw_changes(frame, geometry.reading, shell, palette),
            View::History => draw_history(frame, geometry.reading, shell, palette),
        },
    }
    draw_context(frame, geometry.context, shell, palette);
    draw_footer(frame, geometry.footer, shell, palette);
}

/// Draws the one compact status header: the surface, the place, and the
/// preservation state of the whole scope (SPEC 11.1).
fn draw_header(frame: &mut Frame<'_>, area: Rect, shell: &Shell<'_>, palette: Palette) {
    let workspace = shell.workspace();
    let place = if workspace.adopted && !workspace.scope_roots.is_empty() {
        workspace.scope_roots.clone()
    } else {
        workspace.folder.clone()
    };
    let mut left = format!("margin {place}");
    if !shell.width_class().reduces_metadata() {
        if let Some(branch) = &workspace.branch {
            left.push_str(&format!(" · {branch}"));
        }
        if let Some(head) = &workspace.head {
            left.push_str(&format!(" @ {}", short(head)));
        }
    }
    let split = area.width / 2;
    let (left_area, right_area) = (
        Rect {
            width: split,
            ..area
        },
        Rect {
            x: area.x + split,
            width: area.width - split,
            ..area
        },
    );
    frame.render_widget(
        Paragraph::new(fit_to(&left, left_area.width.saturating_sub(2) as usize)),
        left_area,
    );
    frame.render_widget(
        Paragraph::new(workspace.preservation_sentence()).style(palette.dim),
        right_area,
    );
}

/// Persistent tabs keep the active mode visible even without color.
fn draw_tabs(frame: &mut Frame<'_>, area: Rect, shell: &Shell<'_>, palette: Palette) {
    let mut spans = Vec::new();
    for (key, view) in [(1, View::Read), (2, View::Changes), (3, View::History)] {
        let active = shell.view() == view;
        let label = if active {
            format!(" [{key} {}] ", view.label())
        } else {
            format!("  {key} {}  ", view.label())
        };
        spans.push(Span::styled(
            label,
            if active {
                palette.selected
            } else {
                palette.dim
            },
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The compact sidebar nests the active document's parsed headings.
fn draw_navigator(frame: &mut Frame<'_>, area: Rect, shell: &Shell<'_>, palette: Palette) {
    let focused = shell.focus() == Focus::Documents;
    let mut lines = vec![Line::styled(
        format!(
            "Documents ({}){}",
            shell.documents().len(),
            if focused { " · focus" } else { "" }
        ),
        if focused {
            palette.heading
        } else {
            palette.dim
        },
    )];
    let entries = shell.navigator_entries();
    let current = if focused {
        shell.navigator_cursor()
    } else {
        entries
            .iter()
            .position(|&(doc, heading)| {
                doc == shell.selection() && heading == shell.active_heading()
            })
            .unwrap_or(shell.selection())
    };
    let height = area.height.saturating_sub(2) as usize;
    let start = current.saturating_sub(height.saturating_sub(1));
    let base_level = shell.headings().iter().map(|h| h.level).min().unwrap_or(1);
    for (index, &(document, heading)) in entries.iter().enumerate().skip(start).take(height) {
        let active = index == current;
        let marker = if active { "›" } else { " " };
        let text = match heading {
            Some(h) => {
                let h = &shell.headings()[h];
                format!(
                    "{marker} {}{}",
                    "  ".repeat((h.level - base_level).min(4) + 1),
                    h.title
                )
            }
            None => format!(
                "{marker} {} {}",
                if document == shell.selection() && !shell.outline_folded() {
                    "▾"
                } else {
                    "▸"
                },
                shell.documents()[document]
            ),
        };
        lines.push(Line::styled(
            fit_to(&text, area.width.saturating_sub(1) as usize),
            if active {
                palette.selected
            } else if heading.is_some() {
                palette.dim
            } else {
                Style::new()
            },
        ));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// A row's word ranges survive wrapping; no marker characters enter the source.
fn display_line(row: &wrap::DisplayRow, palette: Palette) -> Line<'static> {
    let mut spans = Vec::new();
    let mut cursor = 0;
    for range in &row.highlights {
        if range.start > cursor {
            spans.push(Span::styled(
                row.text[cursor..range.start].to_string(),
                palette.style(row.kind),
            ));
        }
        spans.push(Span::styled(
            row.text[range.clone()].to_string(),
            palette.style(row.kind).patch(palette.word),
        ));
        cursor = range.end;
    }
    spans.push(Span::styled(
        row.text[cursor..].to_string(),
        palette.style(row.kind),
    ));
    Line::from(spans)
}

/// Draws the Read view: the title breadcrumb, the saved state of this
/// document, and the wrapped reading column (SPEC 5.4, 11.1).
fn draw_reading(frame: &mut Frame<'_>, area: Rect, shell: &Shell<'_>, palette: Palette) {
    let Some(document) = shell.document() else {
        let empty = if shell.workspace().adopted {
            "The writing scope holds no documents."
        } else {
            "This folder holds no Markdown documents yet."
        };
        frame.render_widget(Paragraph::new(empty).style(palette.dim), area);
        return;
    };
    // Before adoption, the reading column itself offers the concise setup
    // action of first use, wrapped to the measure (SPEC 5.1).
    let mut body_top = area.y + 1;
    if !shell.workspace().adopted {
        let action = format!(
            "Set up Margin to preserve this folder: {}",
            shell.workspace().setup_action(shell.documents())
        );
        let introduction = wrap::wrap_text(&action, area.width.max(1));
        let held = introduction.len() as u16;
        let held = held + 1; // One blank row below the action.
        let introduction_area = Rect {
            height: held.min(area.height.saturating_sub(1)),
            y: body_top,
            ..area
        };
        let lines: Vec<Line> = introduction
            .iter()
            .map(|row| Line::styled(row.text.clone(), palette.dim))
            .collect();
        frame.render_widget(Paragraph::new(Text::from(lines)), introduction_area);
        body_top = body_top.saturating_add(held);
    }
    let title = Rect { height: 1, ..area };
    draw_document_title(
        frame,
        title,
        &document.path,
        document.saved.label(),
        shell,
        palette,
    );
    let body = Rect {
        y: body_top,
        height: area.bottom().saturating_sub(body_top),
        ..area
    };
    render_reading_body(frame, body, shell, palette);
}

/// Draws the title row of the displayed document: the path, the section
/// breadcrumb, and the saved label in words (SPEC 11.1, 11.4).
fn draw_document_title(
    frame: &mut Frame<'_>,
    area: Rect,
    path: &str,
    label: &str,
    shell: &Shell<'_>,
    palette: Palette,
) {
    let status = if shell.reading_source() {
        format!("{label} · source")
    } else {
        label.to_string()
    };
    let status_width =
        (unicode_width::UnicodeWidthStr::width(status.as_str()) as u16).min(area.width / 2);
    let left_width = area.width.saturating_sub(status_width + 2);
    let title = shell
        .current_section()
        .map(|section| format!("{path} › {section}"))
        .unwrap_or_else(|| path.to_string());
    frame.render_widget(
        Paragraph::new(fit_to(&title, left_width as usize)).style(palette.heading),
        Rect {
            width: left_width,
            ..area
        },
    );
    frame.render_widget(
        Paragraph::new(status).style(palette.dim),
        Rect {
            x: area.right().saturating_sub(status_width),
            width: status_width,
            ..area
        },
    );
}

/// Draws the wrapped rows of the reading column, each in the role its
/// composed line carries (SPEC 11.4).
fn render_reading_body(frame: &mut Frame<'_>, area: Rect, shell: &Shell<'_>, palette: Palette) {
    let rows = shell.display_rows();
    let offset = shell.reading_offset();
    let shown = &rows[offset.min(rows.len())..];
    let mut lines = Vec::with_capacity(area.height as usize);
    for row in shown.iter().take(area.height as usize) {
        lines.push(display_line(row, palette));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// Draws the Changes view (SPEC 5.2): the review sentence, then the single
/// reading column of every change group the detail pages returned — the
/// heading, the exact passages or source of both sides, and the label that
/// names each group's state in words (SPEC 11.4).
fn draw_changes(frame: &mut Frame<'_>, area: Rect, shell: &Shell<'_>, palette: Palette) {
    let workspace = shell.workspace();
    let title = Rect { height: 1, ..area };
    frame.render_widget(
        Paragraph::new(workspace.review_sentence()).style(palette.bold),
        title,
    );
    let body = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1),
        ..area
    };
    let mut lines = Vec::new();
    if !workspace.adopted {
        lines.push(Line::styled(
            "No review exists: Margin is not set up for this folder.",
            palette.warning,
        ));
        lines.push(Line::styled(
            "The setup action in the Read view adopts it.",
            palette.dim,
        ));
        frame.render_widget(Paragraph::new(Text::from(lines)), body);
        return;
    }
    let Some(changes) = shell.changes() else {
        lines.push(Line::styled(
            "The comparison is unavailable right now.",
            palette.warning,
        ));
        if let Some(note) = &workspace.note {
            lines.push(Line::styled(fit_to(note, area.width as usize), palette.dim));
        }
        frame.render_widget(Paragraph::new(Text::from(lines)), body);
        return;
    };
    if shell.finish().is_some() {
        // The finish summary replaces the column while it stands: the
        // entire scope, the intended result, and the explicit keep-remaining
        // intent (SPEC 5.2). The footer keeps naming the two keys that
        // answer it, so no action is lost to a short frame.
        for row in shell.finish_rows().iter().take(body.height as usize) {
            lines.push(display_line(row, palette));
        }
        frame.render_widget(Paragraph::new(Text::from(lines)), body);
        return;
    }
    let rows = changes.rows();
    let offset = changes.offset();
    let shown = &rows[offset.min(rows.len())..];
    for row in shown.iter().take(body.height as usize) {
        lines.push(display_line(row, palette));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), body);
}

/// Draws the History view (SPEC 5.5): the editorial timeline in ancestry
/// order, with local timestamps and optional revision details, or the
/// standing restoration preview that `Enter` answers.
fn draw_history(frame: &mut Frame<'_>, area: Rect, shell: &Shell<'_>, palette: Palette) {
    let workspace = shell.workspace();
    let mut lines = vec![Line::styled("Document history", palette.heading)];
    if let Some(document) = shell.document() {
        lines.push(Line::styled(
            format!("{} · Enter previews restoration", document.path),
            palette.dim,
        ));
    }
    if !workspace.adopted {
        lines.push(Line::styled(
            "No timeline exists: Margin is not set up for this folder.",
            palette.warning,
        ));
        lines.push(Line::styled(
            "The setup action in the Read view adopts it.",
            palette.dim,
        ));
        frame.render_widget(Paragraph::new(Text::from(lines)), area);
        return;
    }
    let Some(history) = shell.history() else {
        lines.push(Line::styled(
            "The timeline is unavailable right now.",
            palette.warning,
        ));
        if let Some(note) = &workspace.note {
            lines.push(Line::styled(fit_to(note, area.width as usize), palette.dim));
        }
        frame.render_widget(Paragraph::new(Text::from(lines)), area);
        return;
    };
    if history.rows().is_empty() {
        lines.push(Line::from("No history yet."));
        frame.render_widget(Paragraph::new(Text::from(lines)), area);
        return;
    }
    // The composed column: the timeline, or the preview that stands over
    // it (SPEC 5.5), each row in the role its composed line carries.
    let rows = history.display_rows();
    let offset = history.offset();
    let shown = &rows[offset.min(rows.len())..];
    for row in shown
        .iter()
        .take(area.height.saturating_sub(lines.len() as u16) as usize)
    {
        lines.push(display_line(row, palette));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// Draws the action menu (SPEC 11.3 `:`): every action, its binding, and
/// the reason in words when the current context cannot run it. The window
/// follows the selected entry, so every action stays reachable at every
/// height.
fn draw_menu(frame: &mut Frame<'_>, area: Rect, shell: &Shell<'_>, palette: Palette) {
    let Some(Overlay::Menu { selection }) = shell.overlay() else {
        return;
    };
    let entries = shell.menu();
    let mut text =
        String::from("Action menu\nEnter runs the selected action; j/k choose; Esc closes\n\n");
    let mut starts = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        starts.push(text.lines().count() as u32 + 1);
        text.push_str(&entry.line(index == *selection));
        text.push('\n');
    }
    let rows = wrap::wrap_text(&text, area.width.max(1));
    let starts: Vec<usize> = starts
        .iter()
        .map(|line| wrap::row_for_anchor(&rows, *line))
        .collect();
    let selected = starts.get(*selection).copied().unwrap_or(0);
    let height = area.height as usize;
    let top = selected.saturating_sub(height.saturating_sub(1));
    let shown = &rows[top.min(rows.len())..];
    let mut lines = Vec::with_capacity(height);
    for row in shown.iter().take(height) {
        // The first source line of the page is the overlay's title.
        let style = if row.source_line == 1 {
            palette.heading
        } else {
            Style::new()
        };
        lines.push(Line::styled(row.text.clone(), style));
    }
    if shown.len() > height {
        lines.push(Line::styled(
            format!("and {} more rows; j/k scrolls", shown.len() - height),
            palette.dim,
        ));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// Draws the help overlay (SPEC 11.3 `?`): the contextual key list and
/// the current blockers, each in words.
fn draw_help(frame: &mut Frame<'_>, area: Rect, shell: &Shell<'_>, palette: Palette) {
    let mut text = String::from("Margin keys\n\n");
    for action in Action::ALL {
        text.push_str(&format!("  {} · {}\n", action.binding(), action.name()));
    }
    text.push_str("\nCurrent blockers\n");
    let blockers = shell.blockers();
    if blockers.is_empty() {
        text.push_str("  none\n");
    } else {
        for blocker in blockers {
            text.push_str(&format!("  {blocker}\n"));
        }
    }
    text.push_str("\nEsc closes help\n");
    let rows = wrap::wrap_text(&text, area.width.max(1));
    let mut lines = Vec::with_capacity(area.height as usize);
    for row in rows.iter().take(area.height as usize) {
        // The first source line of the page is the overlay's title.
        let style = if row.source_line == 1 {
            palette.heading
        } else {
            Style::new()
        };
        lines.push(Line::styled(row.text.clone(), style));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// Draws the document picker (SPEC 11.3 `f`): the filter and every
/// document it matches. The first match is the document `Enter` opens.
fn draw_picker(frame: &mut Frame<'_>, area: Rect, shell: &Shell<'_>, palette: Palette) {
    let Some(Overlay::Picker { filter }) = shell.overlay() else {
        return;
    };
    let matches = shell.picker_matches(filter);
    let mut text = format!(
        "Document picker\n{} of {} documents match; Enter opens the first\n\n",
        matches.len(),
        shell.documents().len(),
    );
    for (index, path) in matches.iter().enumerate() {
        let marker = if index == 0 { "› " } else { "  " };
        text.push_str(&format!("{marker}{path}\n"));
    }
    text.push_str("\nType to filter · Esc closes\n");
    let rows = wrap::wrap_text(&text, area.width.max(1));
    let mut lines = Vec::with_capacity(area.height as usize);
    for row in rows.iter().take(area.height as usize) {
        // The first source line of the page is the overlay's title.
        let style = if row.source_line == 1 {
            palette.heading
        } else {
            Style::new()
        };
        lines.push(Line::styled(row.text.clone(), style));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn draw_workspace_search(frame: &mut Frame<'_>, area: Rect, shell: &Shell<'_>, palette: Palette) {
    let search = shell.workspace_search();
    let status = if search.searching() {
        "Searching… · Esc cancels".to_string()
    } else if let Some(results) = &search.results {
        results.summary()
    } else {
        "Type a literal query · Enter searches".to_string()
    };
    let mut lines = vec![
        Line::styled("Search all documents · working source", palette.heading),
        Line::styled(
            format!(
                "{} listed documents · case-insensitive",
                shell.documents().len()
            ),
            palette.dim,
        ),
    ];
    for row in wrap::wrap_text(&status, area.width.max(1)) {
        lines.push(Line::raw(row.text));
    }
    lines.push(Line::raw(""));
    if let Some(results) = &search.results {
        let visible = (usize::from(area.height).saturating_sub(lines.len()) / 2).max(1);
        let start = search.selection.saturating_sub(visible.saturating_sub(1));
        for (index, hit) in results.hits.iter().enumerate().skip(start).take(visible) {
            let selected = index == search.selection;
            lines.push(Line::styled(
                fit_to(
                    &format!(
                        "{}{}:{}",
                        if selected { "› " } else { "  " },
                        crate::cli::display_escape(&hit.path),
                        hit.line
                    ),
                    area.width as usize,
                ),
                if selected {
                    palette.selected
                } else {
                    palette.dim
                },
            ));
            lines.push(Line::raw(fit_to(
                &format!("  {}", hit.excerpt),
                area.width as usize,
            )));
        }
        if results.hits.is_empty() {
            lines.push(Line::raw("No matches in the searched text."));
        }
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// Draws the retained context row: the standing text-entry field, the
/// document, the source anchor, and the review state of the scope (SPEC
/// 5.4). One standing notice takes the whole row instead, and an
/// instruction the writer must read is never clipped by the counts beside
/// it.
fn draw_context(frame: &mut Frame<'_>, area: Rect, shell: &Shell<'_>, palette: Palette) {
    if let Some(notice) = shell.notice() {
        frame.render_widget(Paragraph::new(notice.to_string()), area);
        return;
    }
    if let Some(field) = context_field(shell) {
        frame.render_widget(Paragraph::new(field).style(palette.bold), area);
        return;
    }
    let left = match shell.document() {
        Some(document) => {
            let anchor = shell
                .current_anchor()
                .map(|anchor| anchor.line)
                .unwrap_or(1);
            let mut left = format!("{} · line {anchor}", document.path);
            if document.truncated {
                left.push_str(" · long document, view bounded");
            }
            left
        }
        None => String::from("no document"),
    };
    let left = match shell.view() {
        View::Read => left,
        View::Changes => shell
            .changes()
            .and_then(|c| {
                c.selected_group().map(|g| {
                    format!(
                        "{} · passage {} / {}",
                        g.path,
                        c.selection() + 1,
                        c.groups().len()
                    )
                })
            })
            .unwrap_or(left),
        View::History => shell
            .history()
            .map(|h| format!("Version {} / {}", h.selection() + 1, h.rows().len()))
            .unwrap_or(left),
    };
    let right = match (shell.view(), shell.changes()) {
        // In the Changes view, the plan the keys change states itself:
        // every decision and the scope it still has to cover, unvisited
        // groups and documents included (SPEC 5.2).
        (View::Changes, Some(changes)) if shell.width_class().reduces_metadata() => {
            changes.unvisited_sentence()
        }
        (View::Changes, Some(changes)) => changes.counts_sentence(),
        _ if shell.width_class().reduces_metadata() => shell.workspace().review_sentence_short(),
        _ => shell.workspace().review_sentence(),
    };
    let split = area.width / 2;
    let (left_area, right_area) = (
        Rect {
            width: split,
            ..area
        },
        Rect {
            x: area.x + split,
            width: area.width - split,
            ..area
        },
    );
    frame.render_widget(
        Paragraph::new(fit_to(&left, left_area.width.saturating_sub(2) as usize)),
        left_area,
    );
    frame.render_widget(Paragraph::new(right).style(palette.dim), right_area);
}

/// The context row of a standing text-entry field (SPEC 11.3): the text
/// the writer typed, with a visible caret and the keys that answer it.
fn context_field(shell: &Shell<'_>) -> Option<String> {
    match shell.overlay() {
        Some(Overlay::WorkspaceSearch) => Some(format!(
            "all files: {}▏ · Enter {} · ↑/↓ select · Ctrl+F reruns · Esc closes",
            crate::cli::display_escape(&shell.workspace_search().query),
            if shell
                .workspace_search()
                .results
                .as_ref()
                .is_some_and(|r| !r.hits.is_empty())
            {
                "opens"
            } else {
                "searches"
            },
        )),
        Some(Overlay::Search { query }) => Some(format!(
            "search: {query}▏ · Enter finds the next match · Esc closes"
        )),
        Some(Overlay::Note { text, .. }) => Some(format!(
            "Your note: {}▏ · Enter saves · Esc cancels",
            crate::cli::display_escape(text)
        )),
        Some(Overlay::Picker { filter }) => {
            let matches = shell.picker_matches(filter).len();
            Some(format!(
                "find: {filter}▏ · {matches} document{} match · Enter opens the first · Esc closes",
                if matches == 1 { "" } else { "s" },
            ))
        }
        _ => None,
    }
}

/// Draws the action footer: the focused element and the keys this
/// context answers (SPEC 11.3). Focus is stated in words, so it is
/// visible, never inferred from decoration alone.
fn draw_footer(frame: &mut Frame<'_>, area: Rect, shell: &Shell<'_>, palette: Palette) {
    let reduced = area.width < 100;
    let views = if reduced {
        "1·2·3 views"
    } else {
        "1 Read 2 Changes 3 History"
    };
    let (left, right) = match shell.overlay() {
        Some(Overlay::Menu { .. }) => (
            views,
            if reduced {
                "j/k · Enter run · Esc close"
            } else {
                "j/k choose · Enter runs · Esc closes"
            },
        ),
        Some(Overlay::Help) => (
            views,
            if reduced {
                "Esc close help"
            } else {
                "Esc or ? closes help"
            },
        ),
        Some(Overlay::Picker { .. }) => (
            views,
            if reduced {
                "type · Enter open · Esc close"
            } else {
                "type filters · Enter opens the first · Esc closes"
            },
        ),
        Some(Overlay::WorkspaceSearch) => (
            views,
            "type query · ↑/↓ select · Enter search/open · Esc close",
        ),
        Some(Overlay::Search { .. }) => (
            views,
            if reduced {
                "type · Enter find · Esc close"
            } else {
                "type the query · Enter finds · Esc closes"
            },
        ),
        Some(Overlay::Note { .. }) => (views, "type note · Enter saves · Esc cancels"),
        None => match (shell.view(), reduced, shell.finish().is_some()) {
            // While the finish summary stands, the footer names the two keys
            // that answer it, so the action stays reachable at every size
            // (SPEC 5.2, 11.2).
            (View::Changes, false, true) => {
                ("2 Changes", "Enter keep remaining and finish · Esc cancel")
            }
            (View::Changes, true, true) => ("2 Changes", "Enter finish · Esc cancel"),
            // While a restoration preview stands, its labeled action owns the
            // footer (SPEC 5.5, 11.2).
            (View::History, false, _) => {
                match shell.history().map(|history| history.preview_state()) {
                    Some(PreviewState::SaveOffer) => (
                        "3 History",
                        "s save draft then preview · Enter preview · Esc cancel",
                    ),
                    Some(PreviewState::Ready) => {
                        ("3 History", "Enter restore this document · Esc cancel")
                    }
                    Some(PreviewState::Refused) => {
                        ("3 History", "Esc cancel the preview · j/k read it")
                    }
                    _ => (
                        "1 Read 2 Changes 3 History",
                        "j/k · Enter preview · d details · a checkpoints · ? help · q quit",
                    ),
                }
            }
            (View::History, true, _) => {
                match shell.history().map(|history| history.preview_state()) {
                    Some(PreviewState::SaveOffer) => ("3 History", "s save · Esc cancel"),
                    Some(PreviewState::Ready) => ("3 History", "Enter restore · Esc cancel"),
                    Some(PreviewState::Refused) => ("3 History", "Esc cancel · j/k read"),
                    _ => (
                        "1·2·3 views",
                        "j/k · Enter preview · d details · ? · q quit",
                    ),
                }
            }
            (View::Changes, false, false) => (
                "1 Read 2 Changes 3 History",
                "n/p group · K keep · r restore · u undo · d details · Enter finish · ? help",
            ),
            (View::Changes, true, false) => {
                ("2 Changes", "n/p · K keep · r restore · Enter · ? · q quit")
            }
            (_, false, _) => (
                "1 Read 2 Changes 3 History",
                "j/k scroll · Tab outline · [ sidebar · s source · / local · Ctrl+F all files · ? help · q quit",
            ),
            (_, true, _) => ("1·2·3 views", "j/k · s source · f find · / · ? · q quit"),
        },
    };
    let _ = left; // Tabs above now own mode switching.
    let focus = match shell.focus() {
        Focus::Documents => "document list",
        Focus::Actions => "action menu",
        Focus::Reading => match shell.view() {
            View::Read => "reading",
            View::Changes => "changes",
            View::History => "history",
        },
    };
    let right = if shell.focus() == Focus::Documents && shell.overlay().is_none() {
        "j/k navigate · Enter read · ←/→ fold · [ hide · Tab focus"
    } else if shell.view() == View::Read
        && shell.overlay().is_none()
        && !shell.navigator_visible()
        && area.width >= 100
    {
        "j/k scroll · [ sidebar · s source · f find · / local · Ctrl+F all files · ? help · q quit"
    } else {
        right
    };
    let prefix = format!("focus {focus} · ");
    let left_width =
        (unicode_width::UnicodeWidthStr::width(prefix.as_str()) as u16).min(area.width);
    frame.render_widget(
        Paragraph::new(prefix).style(palette.dim),
        Rect {
            width: left_width,
            ..area
        },
    );
    frame.render_widget(
        Paragraph::new(right),
        Rect {
            x: area.x + left_width,
            width: area.width - left_width,
            ..area
        },
    );
}

/// Draws the minimum-size message (SPEC 11.2): the useful minimum, the
/// current size, and the keys that still work.
fn draw_too_small(frame: &mut Frame<'_>, area: Rect, shell: &Shell<'_>) {
    let notice = shell
        .notice()
        .map(|notice| format!("{notice}\n\n"))
        .unwrap_or_default();
    let message = format!(
        "{notice}\
         Terminal too small for Margin.\n\
         Margin needs at least {} columns and {} rows.\n\
         This terminal has {} columns and {} rows.\n\
         \n\
         Resize the terminal, or press q to quit.",
        layout::MIN_COLUMNS,
        layout::MIN_ROWS,
        area.width,
        area.height,
    );
    frame.render_widget(
        Paragraph::new(message)
            .alignment(ratatui::layout::Alignment::Center)
            .wrap(widgets::Wrap { trim: false }),
        area,
    );
}

/// The short form of one object identity: the first seven characters, with
/// a digest scheme such as `sha1:` kept whole so the short form stays
/// honest about what it abbreviates.
pub(crate) fn short(identity: &str) -> String {
    match identity.split_once(':') {
        Some((scheme, rest)) => format!("{scheme}:{}", &rest[..rest.len().min(7)]),
        None => identity[..identity.len().min(7)].to_string(),
    }
}

/// Fits one path or note to `columns` display characters.
fn fit_to(text: &str, columns: usize) -> String {
    let mut owned = text.to_string();
    if owned.chars().count() > columns {
        owned = owned.chars().take(columns.saturating_sub(1)).collect();
        owned.push('…');
    }
    owned
}
