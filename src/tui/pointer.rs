//! Targets recorded from the last drawn frame, in terminal cells.
use super::shell::{Focus, View};
use crossterm::event::{Event, MouseButton, MouseEventKind};
use ratatui::layout::Rect;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    CommandPalette,
    Requests,
    RequestAction(crossterm::event::KeyCode),
    NavigatorEntry(usize),
    SourceLine(u32),
    SourceOffset(usize),
    PickerEntry(usize),
    ViewTab(View),
    ChangeGroup(usize),
    Revision(usize),
    MenuEntry(usize),
    SearchHit(usize),
    Pane(Focus),
}
#[derive(Default)]
pub struct PointerTargets {
    regions: Vec<(Rect, Target)>,
    frame: Option<Rect>,
}
impl PointerTargets {
    pub fn clear(&mut self) {
        self.regions.clear();
        self.frame = None;
    }
    pub fn begin(&mut self, frame: Rect) {
        self.clear();
        self.frame = Some(frame);
    }
    pub fn contains(&self, column: u16, row: u16) -> bool {
        self.frame
            .is_some_and(|r| column >= r.x && column < r.right() && row >= r.y && row < r.bottom())
    }
    pub fn record(&mut self, area: Rect, target: Target) {
        if area.width > 0 && area.height > 0 {
            self.regions.push((area, target));
        }
    }
    /// Clamp an active source drag to the rendered editor cells.
    pub fn source_nearest(&self, column: u16, row: u16) -> Option<(usize, i32)> {
        let regions: Vec<_> = self
            .regions
            .iter()
            .filter_map(|(r, t)| match t {
                Target::SourceOffset(o) => Some((r, *o)),
                _ => None,
            })
            .collect();
        let top = regions.iter().map(|(r, _)| r.y).min()?;
        let bottom = regions.iter().map(|(r, _)| r.bottom()).max()?;
        let (_, offset) = regions.into_iter().min_by_key(|(r, _)| {
            let y = row.clamp(r.y, r.bottom().saturating_sub(1));
            let x = column.clamp(r.x, r.right().saturating_sub(1));
            u32::from(row.abs_diff(y)) * 65536 + u32::from(column.abs_diff(x))
        })?;
        Some((
            offset,
            if row < top {
                -1
            } else if row >= bottom {
                1
            } else {
                0
            },
        ))
    }
    pub fn at(&self, column: u16, row: u16) -> Option<Target> {
        self.regions
            .iter()
            .rev()
            .find(|(r, _)| column >= r.x && column < r.right() && row >= r.y && row < r.bottom())
            .map(|(_, t)| *t)
    }
}
/// Ignore unimplemented pointer events before shell fingerprinting or redraw scheduling.
pub fn ignored(event: &Event) -> bool {
    matches!(event, Event::Mouse(mouse) if !matches!(mouse.kind,
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown |
        MouseEventKind::Down(MouseButton::Left | MouseButton::Right)))
}
