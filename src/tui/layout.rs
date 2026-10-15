//! The responsive layout contract of the terminal surface (SPEC 11.1-11.2).
//!
//! One pure function turns a terminal size into the geometry of a frame,
//! and one classification turns it into the layout policy the size earns.
//! Rendering and position restoration share this module, so a resize and
//! the redraw it triggers can never disagree about where the reading
//! column is.

use ratatui::layout::Rect;

/// The smallest terminal the surface stays useful at (SPEC 11.2).
pub const MIN_COLUMNS: u16 = 60;
/// The smallest row count the surface stays useful at (SPEC 11.2).
pub const MIN_ROWS: u16 = 16;
/// The reading measure of the document column, in terminal cells
/// (SPEC 11.1).
pub const READING_MEASURE: u16 = 76;
/// The width of the optional narrow document list (SPEC 11.2).
pub const NAVIGATOR_WIDTH: u16 = 28;

/// The layout policy one terminal width earns (SPEC 11.2).
///
/// At least 100 columns show the optional narrow document list beside the
/// document column; a desk of at least 140 columns is the tmux layout of
/// SPEC 12.6 and needs no different Margin geometry. Below 100 columns
/// the list disappears. Below 80 the surface keeps one compact column
/// with reduced metadata. Below [`MIN_COLUMNS`] or [`MIN_ROWS`] the
/// surface explains the minimum useful size and keeps quit and resize.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WidthClass {
    /// At least 100 columns: the document list plus the reading column.
    List,
    /// 80 to 99 columns: one document surface.
    Single,
    /// 60 to 79 columns: one surface with reduced metadata.
    Compact,
    /// Below the useful minimum: the size message only.
    TooSmall,
}

impl WidthClass {
    /// Says whether this policy shows the document list.
    pub fn shows_navigator(self) -> bool {
        self == WidthClass::List
    }

    /// Says whether this policy reduces the metadata it shows.
    pub fn reduces_metadata(self) -> bool {
        self == WidthClass::Compact
    }
}

/// Classifies one terminal size.
pub fn classify(columns: u16, rows: u16) -> WidthClass {
    if columns < MIN_COLUMNS || rows < MIN_ROWS {
        WidthClass::TooSmall
    } else if columns < 80 {
        WidthClass::Compact
    } else if columns < 100 {
        WidthClass::Single
    } else {
        WidthClass::List
    }
}

/// The geometry of one frame: the rectangles every view renders into.
#[derive(Clone, Copy, Debug)]
pub struct FrameLayout {
    /// The one compact status header row (SPEC 11.1).
    pub header: Rect,
    /// Persistent view tabs.
    pub tabs: Rect,
    /// The optional document list; absent when the width hides it.
    pub navigator: Option<Rect>,
    /// The reading column, already capped at the reading measure and
    /// centered in the space the header, footer, and list leave.
    pub reading: Rect,
    /// Optional explicitly pinned second prose column.
    pub reference: Option<Rect>,
    /// The retained document and source context row (SPEC 5.4).
    pub context: Rect,
    /// The action footer row.
    pub footer: Rect,
}

/// Computes the geometry of one whole terminal `area`.
///
/// The function is total: any size produces a layout, and a size below the
/// useful minimum produces one whose reading area is empty, which the
/// renderer replaces with the minimum-size message.
pub fn frame_layout(area: Rect) -> FrameLayout {
    frame_layout_with_navigator(area, true)
}

/// Computes a frame with an explicitly collapsed navigator.
pub fn frame_layout_with_navigator(area: Rect, navigator_visible: bool) -> FrameLayout {
    let class = classify(area.width, area.height);
    let header = Rect { height: 1, ..area };
    let footer = Rect {
        y: area.bottom().saturating_sub(1),
        height: 1,
        ..area
    };
    let context = Rect {
        y: area.bottom().saturating_sub(2),
        height: 1,
        ..area
    };
    let tabs = Rect {
        y: area.y + 1,
        height: 1,
        ..area
    };
    let body_top = area.y + 2;
    let body_bottom = area.bottom().saturating_sub(2).max(body_top);
    let body = Rect {
        y: body_top,
        height: body_bottom.saturating_sub(body_top),
        ..area
    };
    let mut navigator = None;
    let mut column = body;
    if navigator_visible && class.shows_navigator() && body.width > NAVIGATOR_WIDTH + 12 {
        navigator = Some(Rect {
            width: NAVIGATOR_WIDTH,
            height: body.height,
            ..body
        });
        // One blank column separates the list from the reading column
        // instead of a drawn rule (SPEC 11.1).
        column = Rect {
            x: body.x + NAVIGATOR_WIDTH + 1,
            width: body.width - NAVIGATOR_WIDTH - 1,
            height: body.height,
            ..body
        };
    }
    let measure = READING_MEASURE.min(column.width.saturating_sub(2)).max(1);
    let gutter = column.width.saturating_sub(measure) / 2;
    let reading = Rect {
        x: column.x + gutter,
        y: column.y,
        width: measure,
        height: column.height,
    };
    FrameLayout {
        header,
        tabs,
        navigator,
        reading,
        reference: None,
        context,
        footer,
    }
}

/// Pinned companions deliberately hide the navigator; never three columns.
pub fn frame_layout_with_reference(area: Rect, navigator: bool, reference: bool) -> FrameLayout {
    let mut frame = frame_layout_with_navigator(area, navigator && !reference);
    if reference && area.width >= 140 && classify(area.width, area.height) != WidthClass::TooSmall {
        let body = Rect {
            y: area.y + 2,
            height: area.height.saturating_sub(4),
            ..area
        };
        let measure = READING_MEASURE.min(body.width.saturating_sub(1) / 2);
        let gutter = body.width.saturating_sub(measure * 2 + 1) / 2;
        frame.reading = Rect {
            x: body.x + gutter,
            width: measure,
            ..body
        };
        frame.reference = Some(Rect {
            x: frame.reading.right() + 1,
            width: measure,
            ..body
        });
    }
    frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Position;

    #[test]
    fn pinned_prose_has_one_separator_and_a_shared_capped_measure() {
        for width in [140, 154, 160, 200, 300] {
            let area = Rect::new(3, 2, width, 30);
            let frame = frame_layout_with_reference(area, true, true);
            let reference = frame.reference.unwrap();
            assert!(frame.navigator.is_none());
            assert_eq!(reference.x, frame.reading.right() + 1);
            assert_eq!(reference.width, frame.reading.width);
            assert!(reference.width <= READING_MEASURE);
            assert!(reference.right() <= area.right());
            assert_eq!(reference.height, frame.reading.height);
        }
        assert!(
            frame_layout_with_reference(Rect::new(0, 0, 139, 30), true, true)
                .reference
                .is_none()
        );
    }

    #[test]
    fn the_width_classes_follow_the_spec_table() {
        assert_eq!(classify(140, 30), WidthClass::List);
        assert_eq!(classify(100, 30), WidthClass::List);
        assert_eq!(classify(99, 30), WidthClass::Single);
        assert_eq!(classify(80, 30), WidthClass::Single);
        assert_eq!(classify(79, 30), WidthClass::Compact);
        assert_eq!(classify(60, 30), WidthClass::Compact);
        assert_eq!(classify(59, 30), WidthClass::TooSmall);
        assert_eq!(classify(80, 15), WidthClass::TooSmall);
    }

    #[test]
    fn wide_frames_show_the_list_and_cap_the_measure() {
        let layout = frame_layout(Rect::new(0, 0, 120, 30));
        let navigator = layout.navigator.expect("120 columns show the list");
        assert_eq!(navigator.width, NAVIGATOR_WIDTH);
        assert_eq!(layout.reading.width, READING_MEASURE);
        assert!(
            layout.reading.x > navigator.right(),
            "the reading column starts after the list"
        );
    }

    #[test]
    fn medium_frames_drop_the_list() {
        let layout = frame_layout(Rect::new(0, 0, 90, 24));
        assert!(layout.navigator.is_none());
        assert_eq!(layout.reading.width, 76);
        // The reading column is centered with a gutter on each side.
        assert_eq!(layout.reading.x, 7);
    }

    #[test]
    fn narrow_frames_shrink_the_measure() {
        let layout = frame_layout(Rect::new(0, 0, 70, 20));
        assert!(layout.navigator.is_none());
        assert_eq!(layout.reading.width, 68);
    }

    #[test]
    fn a_zero_sized_frame_is_total() {
        // A pseudo-terminal can report no size at all before the first
        // resize; the geometry must stay defined for any input.
        let layout = frame_layout(Rect::new(0, 0, 0, 0));
        assert_eq!(classify(0, 0), WidthClass::TooSmall);
        assert!(layout.navigator.is_none());
        assert_eq!(layout.reading.width, 1);
    }

    #[test]
    fn tiny_frames_stay_a_single_surface() {
        let area = Rect::new(0, 0, 50, 12);
        let layout = frame_layout(area);
        assert_eq!(classify(area.width, area.height), WidthClass::TooSmall);
        assert!(layout.navigator.is_none());
        // The renderer replaces the whole frame with the minimum-size
        // message, so only the header row is guaranteed to exist.
        assert_eq!(layout.header.height, 1);
    }

    #[test]
    fn every_rect_stays_inside_the_frame() {
        for (columns, rows) in [(200, 50), (100, 16), (80, 20), (60, 30), (45, 45)] {
            let area = Rect::new(0, 0, columns, rows);
            let layout = frame_layout(area);
            for rect in [layout.header, layout.reading, layout.context, layout.footer] {
                let top_left = Position::new(rect.x, rect.y);
                let bottom_right = Position::new(
                    rect.right().saturating_sub(1),
                    rect.bottom().saturating_sub(1),
                );
                assert!(
                    area.contains(top_left) && area.contains(bottom_right),
                    "{columns}x{rows}: {rect:?} must stay inside {area:?}"
                );
            }
        }
    }
}
