//! Rendering of the terminal surface (SPEC 11.1).
//!
//! Rendering is one pass over the shell: [`draw`] prepares the
//! layout for the frame size, then fills the rectangles the layout
//! module computed. The renderer records typed pointer targets from the
//! rows it draws; input uses only the last completed frame. The palette stays
//! restrained:
//! dim text for chrome, weight for hierarchy, and words for every state
//! a reader must not deduce from hue (SPEC 11.4). Color-off mode draws
//! the same surface with no decoration at all, so the words and symbols
//! carry everything on their own.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, widgets};
use unicode_segmentation::UnicodeSegmentation;

use crate::tui::history::PreviewState;
use crate::tui::layout::{self, WidthClass};
use crate::tui::pointer::Target;
use crate::tui::shell::{Focus, Overlay, Shell, View};
use crate::tui::wrap::{self, RowKind};

fn pointer_row(shell: &Shell<'_>, area: Rect, text: &str, target: Target) {
    if text.trim().is_empty() {
        return;
    }
    let width = unicode_width::UnicodeWidthStr::width(text).min(area.width as usize) as u16;
    shell.record_pointer_target(Rect::new(area.x, area.y, width, 1), target);
}

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
    shell.clear_pointer_targets();
    let area = frame.area();
    shell.prepare_layout(area.width, area.height);
    let shell = &*shell;
    if shell.width_class() == WidthClass::TooSmall {
        draw_too_small(frame, area, shell);
        return;
    }
    let geometry = shell.frame_layout(area);
    shell.begin_pointer_targets(area);
    shell.record_pointer_target(
        geometry.reading,
        Target::Pane(if matches!(shell.overlay(), Some(Overlay::Menu { .. })) {
            Focus::Actions
        } else {
            if shell.reference_only() {
                Focus::Reference
            } else {
                Focus::Reading
            }
        }),
    );
    if let Some(navigator) = geometry.navigator {
        shell.record_pointer_target(navigator, Target::Pane(Focus::Documents));
    }
    let palette = Palette::of(shell);
    draw_header(frame, geometry.header, shell, palette);
    draw_tabs(frame, geometry.tabs, shell, palette);
    if let Some(progress) = shell.progress_label() {
        frame.render_widget(
            Paragraph::new(crate::cli::display_escape(&progress))
                .style(palette.dim)
                .wrap(widgets::Wrap { trim: false }),
            Rect {
                y: area.y + 2,
                height: shell.progress_rows(area.width),
                ..area
            },
        );
    }
    if let Some(navigator) = geometry.navigator {
        draw_navigator(frame, navigator, shell, palette);
    }
    if shell.overlay() == Some(&Overlay::Help) {
        draw_help(frame, geometry.reading, shell, palette);
    } else if let Some(panel) = shell.request_panel() {
        draw_scrolled_text(frame, geometry.reading, panel, shell.request_scroll);
    } else if let Some((path, prompt)) = shell.agent_prompt() {
        frame.render_widget(
            Paragraph::new(format!(
                "Ask agent: {path}\nEnter queues · Esc cancels\n\n{}",
                crate::cli::display_escape(prompt)
            )),
            geometry.reading,
        );
    } else if let Some(editor) = shell.source_editor() {
        if let Some(super::editor::EditorDialog::Comparison { text, offset, .. }) = &editor.dialog {
            draw_scrolled_text(frame, geometry.reading, text, *offset);
        } else {
            draw_source_editor(frame, geometry.reading, editor, shell, palette);
        }
    } else {
        match shell.overlay() {
            Some(Overlay::SavedReviews { .. }) => {
                draw_saved_reviews(frame, geometry.reading, shell, palette)
            }
            Some(Overlay::Menu { .. }) => draw_menu(frame, geometry.reading, shell, palette),
            Some(Overlay::Help) => draw_help(frame, geometry.reading, shell, palette),
            Some(Overlay::Picker { .. }) => draw_picker(frame, geometry.reading, shell, palette),
            Some(Overlay::WorkspaceSearch) => {
                draw_workspace_search(frame, geometry.reading, shell, palette)
            }
            // The search field keeps the current view visible behind it; the
            // context row carries the query (SPEC 11.3).
            _ if shell.reference_only() => draw_reference(frame, geometry.reading, shell, palette),
            _ => match shell.view() {
                View::Read => draw_reading(frame, geometry.reading, shell, palette),
                View::Changes => draw_changes(frame, geometry.reading, shell, palette),
                View::History => draw_history(frame, geometry.reading, shell, palette),
            },
        }
    }
    if let Some(area) = geometry.reference {
        draw_reference(frame, area, shell, palette);
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
    let mut column = area.x;
    for (key, view) in [(1, View::Read), (2, View::Changes), (3, View::History)] {
        let active = shell.view() == view;
        let label = if active {
            format!(" [{key} {}] ", view.label())
        } else {
            format!("  {key} {}  ", view.label())
        };
        let width = unicode_width::UnicodeWidthStr::width(label.as_str()) as u16;
        shell.record_pointer_target(
            Rect::new(
                column,
                area.y,
                width.min(area.right().saturating_sub(column)),
                area.height,
            ),
            Target::ViewTab(view),
        );
        column = column.saturating_add(width);
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
        pointer_row(
            shell,
            Rect::new(
                area.x,
                area.y + lines.len() as u16,
                area.width.saturating_sub(1),
                1,
            ),
            &text,
            Target::NavigatorEntry(index),
        );
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
    if shell.discovery_truncated() {
        // The bound is a first-class row, never a quietly short list
        // (SPEC 11.3): the reader knows the discovery stopped at its
        // document limit.
        lines.push(Line::styled(
            fit_to(
                &format!(
                    "First {} documents; adopt a scope for the full list",
                    crate::tui::shell::MAX_DISCOVERED_DOCUMENTS
                ),
                area.width as usize,
            ),
            palette.warning,
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

fn draw_reference(frame: &mut Frame<'_>, area: Rect, shell: &Shell<'_>, palette: Palette) {
    let Some(reference) = shell.reference() else {
        return;
    };
    shell.record_pointer_target(area, Target::Pane(Focus::Reference));
    let title = format!(
        "{}Reference · read-only · {} · {}",
        if shell.focus() == Focus::Reference {
            "› "
        } else {
            ""
        },
        reference.label(),
        reference.descriptor.path
    );
    frame.render_widget(
        Paragraph::new(crate::cli::display_escape(&title)).style(
            if shell.focus() == Focus::Reference {
                palette.selected
            } else {
                palette.heading
            },
        ),
        Rect { height: 1, ..area },
    );
    let mut lines = vec![];
    if let Some(error) = &reference.unavailable {
        lines.push(Line::styled(
            crate::cli::display_escape(error),
            palette.warning,
        ));
        lines.push(Line::raw(": Refresh reference or Choose reference…"));
    }
    if reference.changed {
        lines.push(Line::styled(
            "Source changed since pinning · : Refresh reference",
            palette.warning,
        ));
    }
    if reference.truncated {
        lines.push(Line::raw("Reference truncated at 256 KiB"));
    }
    let held = lines.len();
    lines.extend(
        reference
            .rows
            .iter()
            .skip(reference.offset)
            .take((area.height as usize).saturating_sub(held + 1))
            .map(|r| Line::styled(r.text.clone(), palette.style(r.kind))),
    );
    frame.render_widget(
        Paragraph::new(Text::from(lines)),
        Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(1),
            ..area
        },
    );
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
    for (index, row) in shown.iter().take(area.height as usize).enumerate() {
        pointer_row(
            shell,
            Rect::new(area.x, area.y + index as u16, area.width, 1),
            &row.text,
            Target::SourceLine(row.source_line),
        );
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
        for row in shell
            .finish_rows()
            .iter()
            .skip(
                shell
                    .finish_scroll
                    .min(shell.finish_rows().len().saturating_sub(1)),
            )
            .take(body.height as usize)
        {
            lines.push(display_line(row, palette));
        }
        frame.render_widget(Paragraph::new(Text::from(lines)), body);
        return;
    }
    let rows = changes.rows();
    let offset = changes.offset();
    let shown = &rows[offset.min(rows.len())..];
    for (index, row) in shown.iter().take(body.height as usize).enumerate() {
        if let Some(group) = changes.group_at_row(offset + index) {
            pointer_row(
                shell,
                Rect::new(body.x, body.y + index as u16, body.width, 1),
                &row.text,
                Target::ChangeGroup(group),
            );
        }
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
    let top = lines.len();
    for (index, row) in shown
        .iter()
        .take(area.height.saturating_sub(top as u16) as usize)
        .enumerate()
    {
        if let Some(revision) = history.revision_at_row(offset + index) {
            pointer_row(
                shell,
                Rect::new(area.x, area.y + top as u16 + index as u16, area.width, 1),
                &row.text,
                Target::Revision(revision),
            );
        }
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
    let palette_state = shell.palette();
    let menu = shell.menu();
    let mut lines = vec![
        Line::styled("Find anything · Action menu", palette.heading),
        Line::raw(format!(
            "{}▏",
            query_tail(
                &crate::cli::display_escape(&palette_state.query),
                area.width.saturating_sub(1) as usize
            )
        )),
    ];
    let state = if palette_state.ready() {
        "↑/↓ select · Enter opens · Esc closes"
    } else {
        "Query edited or matching · Enter computes; later Enter opens"
    };
    lines.push(Line::raw(state));
    if let Some(metadata) = &palette_state.metadata {
        lines.extend(
            wrap::wrap_text(&metadata.summary(), area.width)
                .into_iter()
                .map(|row| Line::raw(row.text)),
        );
    }
    if palette_state.additional > 0 {
        lines.push(Line::raw(format!(
            "{} additional matching results omitted; narrow the query",
            palette_state.additional
        )));
    }
    if palette_state.ready() && palette_state.hits.is_empty() {
        lines.push(Line::raw("No matching actions or documents"));
    }
    let held = lines.len();
    let mut result_rows = Vec::new();
    let mut starts = Vec::new();
    let mut targets = Vec::new();
    for (index, hit) in palette_state.hits.iter().enumerate() {
        starts.push(result_rows.len());
        let e = &hit.entry;
        let disabled = e
            .action
            .and_then(|a| menu.iter().find(|m| m.action == a))
            .and_then(|m| m.disabled.as_ref());
        let shortcut = e.action.map_or(String::new(), |a| a.binding());
        let label = if shortcut.is_empty() {
            e.label.clone()
        } else {
            format!("{shortcut} · {}", e.label)
        };
        let line = format!(
            "{} {} · {}{}{}",
            if index == *selection { "›" } else { " " },
            e.kind.label(),
            label,
            if e.path.is_empty() {
                String::new()
            } else {
                format!(" · {}:{} · {}", e.path, e.line, e.context)
            },
            disabled.map_or(String::new(), |r| format!(" — unavailable: {r}"))
        );
        let row_start = result_rows.len();
        result_rows.extend(
            wrap::wrap_text(&crate::cli::display_escape(&line), area.width)
                .into_iter()
                .map(|r| {
                    Line::styled(
                        r.text,
                        if index == *selection {
                            palette.selected
                        } else {
                            Style::new()
                        },
                    )
                }),
        );
        targets.extend(std::iter::repeat_n(index, result_rows.len() - row_start));
    }
    let available = (area.height as usize).saturating_sub(held);
    let start = starts.get(*selection).copied().unwrap_or(0);
    let end = starts
        .get(selection + 1)
        .copied()
        .unwrap_or(result_rows.len());
    let top = if end > available {
        end.saturating_sub(available).min(start)
    } else {
        0
    };
    for (row, index) in targets.iter().skip(top).take(available).enumerate() {
        shell.record_pointer_target(
            Rect::new(area.x, area.y + (held + row) as u16, area.width, 1),
            Target::MenuEntry(*index),
        );
    }
    lines.extend(result_rows.into_iter().skip(top).take(available));
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn draw_saved_reviews(frame: &mut Frame<'_>, area: Rect, shell: &Shell<'_>, palette: Palette) {
    let Some(Overlay::SavedReviews {
        selection,
        inspect,
        confirm,
    }) = shell.overlay()
    else {
        return;
    };
    let mut lines = vec![
        "Saved reviews · local choices, never approval".to_string(),
        "Enter resume · i inspect · d discard · PgUp/PgDn read · n fresh · Esc leave".into(),
    ];
    if shell.saved_reviews_omitted() {
        lines.push("Additional saved records may be omitted (bounded enumeration)".into());
    }
    for (i, entry) in shell.saved_reviews().iter().enumerate() {
        if *inspect && i != *selection {
            continue;
        }
        let marker = if i == *selection { "›" } else { " " };
        match &entry.record {
            Ok(r) => {
                let kept = r
                    .decisions
                    .iter()
                    .filter(|c| c.decision == super::changes::Decision::Keep)
                    .count();
                lines.push(format!(
                    "{marker} {} · C {} · updated {} ms UTC · {kept} keep / {} restore · {}",
                    r.identity.branch,
                    short(&r.identity.candidate),
                    r.updated_ms,
                    r.decisions.len() - kept,
                    entry.completed.as_ref().map_or_else(
                        || shell.saved_compatibility(r),
                        |receipt| format!("Completed review · receipt {}", short(receipt))
                    )
                ));
                if i == *selection && *inspect {
                    lines.push(format!(
                        "ID {} · repository {} · adoption {} · B {} · review {} · scope {}",
                        r.id,
                        r.identity.repository,
                        r.identity.adoption,
                        r.identity.baseline,
                        r.identity.review,
                        r.identity.scope
                    ));
                    for c in &r.decisions {
                        lines.push(format!(
                            "{:?} · {} · {} · {}",
                            c.decision, c.path, c.heading, c.id
                        ));
                    }
                }
            }
            Err(e) => lines.push(format!("{marker} {} · {e}", entry.id)),
        }
    }
    if *confirm {
        lines.insert(1, "Discard selected saved review? Enter confirms deletion of this draft only; Esc leaves it intact.".into());
    }
    let text = lines
        .iter()
        .map(|s| crate::cli::display_escape(s))
        .collect::<Vec<_>>()
        .join("\n");
    let rows = wrap::wrap_text(&text, area.width);
    // Keep the selected record reachable even with a long inspection.
    let top = if *inspect {
        shell.saved_offset()
    } else {
        selection.saturating_sub(area.height.saturating_sub(5) as usize)
    };
    frame.render_widget(
        Paragraph::new(Text::from(
            rows.iter()
                .skip(top)
                .take(area.height as usize)
                .map(|r| Line::styled(r.text.clone(), palette.dim))
                .collect::<Vec<_>>(),
        )),
        area,
    );
}

fn draw_scrolled_text(frame: &mut Frame<'_>, area: Rect, text: &str, offset: usize) {
    let rows = wrap::dialog_rows(text, area.width);
    let top = offset.min(rows.len().saturating_sub(1));
    frame.render_widget(
        Paragraph::new(
            rows.iter()
                .skip(top)
                .take(area.height as usize)
                .map(|r| Line::raw(r.text.clone()))
                .collect::<Vec<_>>(),
        ),
        area,
    );
}

/// Help uses independent dialog state, including when opened from editing.
fn draw_help(frame: &mut Frame<'_>, area: Rect, shell: &Shell<'_>, _palette: Palette) {
    let text = shell.help_text();
    let title = format!(
        "Help · {} · / {}{}",
        if shell.help_all {
            "all shortcuts"
        } else {
            "current context"
        },
        crate::cli::display_escape(&shell.help_query),
        if shell.help_filtering { "▏" } else { "" }
    );
    frame.render_widget(
        Paragraph::new(title),
        Rect::new(area.x, area.y, area.width, area.height.min(1)),
    );
    draw_scrolled_text(
        frame,
        Rect::new(
            area.x,
            area.y + 1,
            area.width,
            area.height.saturating_sub(1),
        ),
        &text,
        shell.help_scroll,
    );
}

fn draw_picker(frame: &mut Frame<'_>, area: Rect, shell: &Shell<'_>, palette: Palette) {
    let Some(Overlay::Picker { filter }) = shell.overlay() else {
        return;
    };
    let paths: Vec<String> = shell
        .picker_matches(filter)
        .into_iter()
        .map(str::to_owned)
        .collect();
    let selected = shell.picker_index(&paths);
    let title = format!(
        "Document picker · {} of {} documents match\n{}▏ · arrows select · Enter opens selected",
        paths.len(),
        shell.documents().len(),
        crate::cli::display_escape(filter)
    );
    frame.render_widget(
        Paragraph::new(title).style(palette.heading),
        Rect::new(area.x, area.y, area.width, area.height.min(2)),
    );
    let body = Rect::new(
        area.x,
        area.y + area.height.min(3),
        area.width,
        area.height.saturating_sub(3),
    );
    if paths.is_empty() {
        frame.render_widget(Paragraph::new("No documents match"), body);
        return;
    }
    let mut rows = Vec::new();
    let mut selected_row = 0;
    for (index, path) in paths.iter().enumerate() {
        if index == selected {
            selected_row = rows.len();
        }
        for row in wrap::wrap_text(
            &format!(
                "{} {}",
                if index == selected { "›" } else { " " },
                crate::cli::display_escape(path)
            ),
            body.width.max(1),
        ) {
            rows.push((index, row.text));
        }
    }
    let selected_end = rows
        .iter()
        .rposition(|(index, _)| *index == selected)
        .map_or(selected_row + 1, |i| i + 1);
    let top = selected_end
        .saturating_sub(body.height as usize)
        .min(selected_row);
    let lines = rows
        .iter()
        .skip(top)
        .take(body.height as usize)
        .enumerate()
        .map(|(y, (index, text))| {
            pointer_row(
                shell,
                Rect::new(body.x, body.y + y as u16, body.width, 1),
                text,
                Target::PickerEntry(*index),
            );
            Line::raw(text.clone())
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), body);
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
            for (position, line) in lines.iter().enumerate().skip(lines.len() - 2) {
                let y = area.y.saturating_add(position as u16);
                if y < area.bottom() {
                    let width = line.width().min(area.width as usize) as u16;
                    shell.record_pointer_target(
                        Rect::new(area.x, y, width, 1),
                        Target::SearchHit(index),
                    );
                }
            }
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
    if let Some(error) = shell.autosave_notice() {
        frame.render_widget(Paragraph::new(error).style(palette.warning), area);
        return;
    }
    if let Some(notice) = shell.notice() {
        frame.render_widget(Paragraph::new(crate::cli::display_escape(notice)), area);
        return;
    }
    if let Some(error) = shell.history_notice() {
        frame.render_widget(Paragraph::new(error).style(palette.warning), area);
        return;
    }
    if let Some(activity) = shell.source_editor().and_then(|e| e.activity.as_ref()) {
        frame.render_widget(Paragraph::new(activity.as_str()), area);
        return;
    }
    if let Some(pending) = shell.pending_preview() {
        // The pending target labels itself in the context row (SPEC
        // 11.4): the reason stands while the content-dependent actions
        // give it, with or without one key press to raise it.
        frame.render_widget(Paragraph::new(pending).style(palette.warning), area);
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
        (View::Read, _) if !shell.agent_status.is_empty() => shell.agent_status.clone(),
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
                "find: {filter}▏ · {matches} document{} match · Enter opens selected · Esc closes",
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
    if shell.overlay() == Some(&Overlay::Help) {
        frame.render_widget(
            Paragraph::new(if shell.help_filtering {
                "Type to filter · Enter search · Esc stop filtering"
            } else {
                "↑/↓ scroll · / search · a all keys · Esc close"
            }),
            area,
        );
        return;
    }
    if matches!(shell.overlay(), Some(Overlay::Menu { .. })) {
        frame.render_widget(
            Paragraph::new("focus action menu · type · ↑/↓ · Enter opens · Esc closes"),
            area,
        );
        return;
    }
    if shell.agent_prompt().is_some() {
        frame.render_widget(
            Paragraph::new("Enter Queue request · Esc / Ctrl+C Cancel · ordinary keys enter text"),
            area,
        );
        return;
    }
    if shell.request_panel().is_some() {
        frame.render_widget(
            Paragraph::new("↑/↓ PgUp/PgDn scroll · Esc Close requests · q Quit"),
            area,
        );
        return;
    }
    if let Some(editor) = shell.source_editor() {
        if let Some(dialog) = &editor.dialog {
            let hint = match dialog {
                super::editor::EditorDialog::SaveChoices => "s Save · d Discard buffer · Esc Stay",
                super::editor::EditorDialog::Comparison {
                    kind: super::editor::ComparisonKind::Disk,
                    ..
                } => "↑/↓ Scroll · Ctrl+B Keep my text · Esc Edit",
                super::editor::EditorDialog::Comparison {
                    kind: super::editor::ComparisonKind::Agent,
                    ..
                } => "↑/↓ Scroll · F3 Use proposal · F4 Keep mine · Esc Edit",
                super::editor::EditorDialog::Comparison { .. } => {
                    "↑/↓ Scroll · Ctrl+D Compare disk · Esc Edit"
                }
            };
            frame.render_widget(Paragraph::new(hint), area);
            return;
        }
        frame.render_widget(
            Paragraph::new("EDITING · Esc Read · F2 Ask · F1 Help"),
            area,
        );
        return;
    }
    if shell.finish().is_some() {
        let text = if area.width >= 80 {
            "Enter keep remaining and finish · Esc cancel · ] undecided · ↑/↓ scroll"
        } else {
            "Enter keep remaining · Esc cancel · ] undecided · ↑/↓ scroll"
        };
        frame.render_widget(Paragraph::new(text), area);
        return;
    }
    let reduced = area.width < 100;
    let views = if reduced {
        "1·2·3 views"
    } else {
        "1 Read 2 Changes 3 History"
    };
    let (left, right) = match shell.overlay() {
        Some(Overlay::SavedReviews { .. }) => (
            views,
            "↑/↓ choose · Enter resume/confirm · i inspect · d discard · n fresh · Esc close",
        ),
        Some(Overlay::Menu { .. }) => (
            views,
            if reduced {
                "type · ↑/↓ · Enter open · Esc close"
            } else {
                "type query · ↑/↓ select · Enter opens · Esc closes"
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
                "type filters · ↑/↓ select · Enter opens · Esc closes"
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
            (View::Changes, false, true) => (
                "2 Changes",
                "Enter keep remaining and finish · ] undecided · Esc cancel",
            ),
            (View::Changes, true, true) => (
                "2 Changes",
                "Enter keep remaining · ] undecided · Esc cancel",
            ),
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
                "e edit · j/k scroll · Tab outline · s source · / local · Ctrl+F all files · ? help · q quit",
            ),
            (_, true, _) => ("1·2·3 views", "j/k · s source · f find · / · ? · q quit"),
        },
    };
    let _ = left; // Tabs above now own mode switching.
    let focus = match shell.focus() {
        Focus::Documents => "document list",
        Focus::Actions => "action menu",
        Focus::Reference => "reference · read-only",
        Focus::Reading => match shell.view() {
            View::Read => "reading",
            View::Changes => "changes",
            View::History => "history",
        },
    };
    let right = if shell.focus() == Focus::Reference {
        "j/k scroll · s source · / search · d identity · Esc Back to review · : actions"
    } else if shell.focus() == Focus::Documents && shell.overlay().is_none() {
        "j/k navigate · Enter read · ←/→ fold · [ hide · Tab focus"
    } else if shell.view() == View::Read
        && shell.overlay().is_none()
        && !shell.navigator_visible()
        && area.width >= 100
    {
        "e edit · j/k scroll · f find · s source · / local · Ctrl+F all files · ? help · q quit"
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
    let available = area.width.saturating_sub(left_width) as usize;
    let right = fit_action_hints(right, available, shell.overlay().is_none());
    frame.render_widget(
        Paragraph::new(right),
        Rect {
            x: area.x + left_width,
            width: area.width - left_width,
            ..area
        },
    );
}

/// Drop complete optional hints, retaining discovery and exit controls.
fn fit_action_hints(hints: &str, width: usize, discovery: bool) -> String {
    use unicode_width::UnicodeWidthStr;
    if hints.width() <= width {
        return hints.into();
    }
    let mut selected = Vec::new();
    let reserved = if discovery {
        "? help · q quit"
    } else {
        "Esc close"
    };
    let budget = width.saturating_sub(reserved.width() + 3);
    for hint in hints.split(" · ") {
        if hint.contains("? help") || hint.contains("q quit") || hint.contains("Esc") {
            continue;
        }
        let candidate = if selected.is_empty() {
            hint.to_string()
        } else {
            format!("{} · {hint}", selected.join(" · "))
        };
        if candidate.width() <= budget {
            selected.push(hint);
        }
    }
    selected.push(reserved);
    selected.join(" · ")
}

/// Draws the minimum-size message (SPEC 11.2): the useful minimum, the
/// current size, and the keys that still work.
fn draw_too_small(frame: &mut Frame<'_>, area: Rect, shell: &Shell<'_>) {
    let notice = shell
        .notice()
        .map(|notice| format!("{}\n\n", crate::cli::display_escape(notice)))
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
        Some((scheme, rest)) => format!("{scheme}:{}", rest.chars().take(7).collect::<String>()),
        None => identity.chars().take(7).collect(),
    }
}

fn query_tail(text: &str, cells: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut width = 0;
    let mut tail = Vec::new();
    for c in text.chars().rev() {
        let next = c.width().unwrap_or(0);
        if width + next > cells.saturating_sub(1) {
            break;
        }
        tail.push(c);
        width += next;
    }
    let clipped = tail.len() < text.chars().count();
    let mut shown = if clipped {
        "…".to_string()
    } else {
        String::new()
    };
    shown.extend(tail.into_iter().rev());
    shown
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

fn draw_source_editor(
    frame: &mut Frame<'_>,
    area: Rect,
    editor: &super::editor::Editor,
    shell: &Shell<'_>,
    palette: Palette,
) {
    let line = editor.text[..editor.cursor]
        .bytes()
        .filter(|b| *b == b'\n')
        .count();
    let title = if editor.dialog.is_some() {
        "Buffer choices · s Save · d Discard · Esc Stay".to_string()
    } else {
        format!(
            "EDITING · {} · {}",
            if editor.autosave_error.is_some() {
                "Autosave paused"
            } else if editor.dirty() {
                "Saving…"
            } else {
                "Saved"
            },
            crate::cli::display_escape(&editor.base.path)
        )
    };
    let title = if let Some(activity) = &editor.activity {
        format!("{title} · {activity}")
    } else {
        title
    };
    let body = widgets::Block::default()
        .borders(widgets::Borders::ALL)
        .title(title);
    let inner = body.inner(area);
    frame.render_widget(body, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let height = if editor.conflict.is_some() {
        (inner.height / 2).max(1)
    } else {
        inner.height
    };
    let previous = editor.viewport_top.get();
    let top = if editor.manual_scroll {
        previous
    } else if line < previous {
        line
    } else if line >= previous + height as usize {
        line.saturating_sub(height as usize - 1)
    } else {
        previous
    };
    editor.viewport_top.set(top);
    let selection = editor.selection();
    let mut offset = 0;
    let mut rows = Vec::new();
    let line_start = editor.text[..editor.cursor]
        .rfind('\n')
        .map_or(0, |i| i + 1);
    let prefix = &editor.text[line_start..editor.cursor];
    let plain_ascii = |text: &str| text.bytes().all(|b| (b' '..=b'~').contains(&b));
    let cursor_x: usize = if plain_ascii(prefix) {
        prefix.len()
    } else {
        prefix.graphemes(true).map(source_width).sum()
    };
    let left = cursor_x.saturating_sub(inner.width as usize - 1);
    for (number, source) in editor
        .text
        .split_inclusive('\n')
        .chain(if editor.text.ends_with('\n') || editor.text.is_empty() {
            Some("")
        } else {
            None
        })
        .enumerate()
    {
        if number >= top + height as usize {
            break;
        }
        if number >= top {
            let mut spans = Vec::new();
            let source_text = source.trim_end_matches(['\r', '\n']);
            let screen_y = inner.y + (number - top) as u16;
            let mut cell = 0usize;
            if plain_ascii(source_text) {
                cell = source_text.len();
                for byte in left.min(cell)..(left + inner.width as usize).min(cell) {
                    shell.record_pointer_target(
                        Rect::new(inner.x + (byte - left) as u16, screen_y, 1, 1),
                        Target::SourceOffset(offset + byte),
                    );
                }
            } else {
                for (byte, grapheme) in source_text.grapheme_indices(true) {
                    let width = source_width(grapheme);
                    let start = cell.max(left);
                    let end = (cell + width).min(left + inner.width as usize);
                    if end > start {
                        shell.record_pointer_target(
                            Rect::new(
                                inner.x + (start - left) as u16,
                                screen_y,
                                (end - start) as u16,
                                1,
                            ),
                            Target::SourceOffset(offset + byte),
                        );
                    }
                    cell += width;
                    if cell >= left + inner.width as usize {
                        break;
                    }
                }
            }
            let padding = cell.saturating_sub(left).min(inner.width as usize);
            shell.record_pointer_target(
                Rect::new(
                    inner.x + padding as u16,
                    screen_y,
                    inner.width - padding as u16,
                    1,
                ),
                Target::SourceOffset(offset + source_text.len()),
            );
            if plain_ascii(source_text) {
                let start = left.min(source_text.len());
                let end = (left + inner.width as usize).min(source_text.len());
                let selected_start = selection.start.saturating_sub(offset).clamp(start, end);
                let selected_end = selection.end.saturating_sub(offset).clamp(start, end);
                rows.push(Line::from(vec![
                    Span::raw(source_text[start..selected_start].to_string()),
                    Span::styled(
                        source_text[selected_start..selected_end].to_string(),
                        Style::new().reversed(),
                    ),
                    Span::raw(source_text[selected_end..end].to_string()),
                ]));
                offset += source.len();
                continue;
            }
            let mut visual = 0;
            for (index, grapheme) in source.trim_end_matches(['\r', '\n']).grapheme_indices(true) {
                let width = source_width(grapheme);
                let end = visual + width;
                if visual >= left + inner.width as usize {
                    break;
                }
                if end > left {
                    let selected = selection.contains(&(offset + index));
                    let text = if visual < left {
                        " ".repeat(end - left)
                    } else {
                        source_grapheme(grapheme)
                    };
                    spans.push(Span::styled(
                        text,
                        if selected {
                            Style::new().reversed()
                        } else {
                            Style::new()
                        },
                    ));
                }
                visual = end;
            }
            rows.push(Line::from(spans));
        }
        offset += source.len();
    }
    frame.render_widget(
        Paragraph::new(rows),
        Rect::new(inner.x, inner.y, inner.width, height),
    );
    if let Some(conflict) = &editor.conflict {
        frame.render_widget(
            Paragraph::new(format!(
                "{}\nF6 Inspect details · Ctrl+D Compare disk · F5 Save/discard/stay",
                conflict
                    .lines()
                    .map(crate::cli::display_escape)
                    .collect::<Vec<_>>()
                    .join("\n")
            ))
            .style(palette.warning),
            Rect::new(
                inner.x,
                inner.y + height,
                inner.width,
                inner.height - height,
            ),
        );
    }
    if !editor.dialog.is_some() && line >= top && line < top + height as usize {
        frame.set_cursor_position((
            inner.x + (cursor_x - left).min(inner.width as usize - 1) as u16,
            inner.y + (line - top) as u16,
        ));
    }
}

fn source_grapheme(grapheme: &str) -> String {
    if grapheme == "\t" {
        "    ".into()
    } else if grapheme.chars().any(char::is_control) {
        grapheme.chars().flat_map(char::escape_default).collect()
    } else {
        grapheme.into()
    }
}

fn source_width(grapheme: &str) -> usize {
    if grapheme == "\t" {
        4
    } else if grapheme.chars().any(char::is_control) {
        grapheme.chars().flat_map(char::escape_default).count()
    } else {
        unicode_width::UnicodeWidthStr::width(grapheme)
    }
}
