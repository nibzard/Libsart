//! A bounded source buffer. Cursor positions are UTF-8 boundaries.
use crate::editing::{MAX_SOURCE_BYTES, Version};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

pub fn help_text() -> String {
    let mut text = String::from(
        "Editor keys\nArrows/Home/End/PgUp/PgDn · Move through source\nShift + movement · Select source\nCtrl+Left/Right · Move by Unicode word\nCtrl+Home/End · Start/end of document\nCtrl+A · Select all source\nCtrl+Z / Ctrl+Y · Undo / redo typing\n",
    );
    for action in super::actions::EditorAction::ALL {
        text.push_str(&format!("{} · {}\n", action.binding(), action.name()));
    }
    text.push_str("Ctrl+Shift+A · Ask agent (alias for F2)\nClick · Place source cursor\nShift-click · Extend source selection\nWheel · Scroll without moving cursor\n");
    text
}

pub const AUTOSAVE_DELAY: std::time::Duration = std::time::Duration::from_millis(750);
const HISTORY_BYTES: usize = 8 * 1024 * 1024;
#[derive(Clone, Debug)]
struct Snapshot {
    text: String,
    cursor: usize,
    anchor: Option<usize>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComparisonKind {
    Disk,
    Agent,
    Information,
}

#[derive(Clone, Debug)]
pub enum EditorDialog {
    SaveChoices,
    Comparison {
        kind: ComparisonKind,
        text: String,
        offset: usize,
    },
}

#[derive(Clone, Debug)]
pub struct Editor {
    pub base: Version,
    pub text: String,
    pub cursor: usize,
    pub anchor: Option<usize>,
    pub dialog: Option<EditorDialog>,
    pub viewport_top: std::cell::Cell<usize>,
    pub manual_scroll: bool,
    pub conflict: Option<String>,
    pub disk_comparison: Option<Version>,
    pub recovery_id: String,
    pub rejected: bool,
    pub last_edit: std::time::Instant,
    pub autosave_error: Option<String>,
    pub activity: Option<String>,
    pub conflicting_request: Option<String>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
}
impl Editor {
    pub fn new(base: Version) -> Self {
        Self {
            text: base.text.clone(),
            base,
            cursor: 0,
            anchor: None,
            dialog: None,
            viewport_top: std::cell::Cell::new(0),
            manual_scroll: false,
            conflict: None,
            disk_comparison: None,
            rejected: false,
            last_edit: std::time::Instant::now(),
            autosave_error: None,
            activity: None,
            conflicting_request: None,
            recovery_id: format!("draft-{}", crate::editing::id()),
            undo: vec![],
            redo: vec![],
        }
    }
    pub fn autosave_due(&self, now: std::time::Instant) -> bool {
        self.dirty()
            && self.dialog.is_none()
            && self.autosave_error.is_none()
            && now.saturating_duration_since(self.last_edit) >= AUTOSAVE_DELAY
    }
    pub fn saved(&mut self, base: Version) {
        self.base = base;
        self.autosave_error = None;
        self.disk_comparison = None;
        self.conflict = None;
    }
    /// External changes update a clean buffer and become one undo step.
    pub fn accept_disk(&mut self, base: Version) {
        if self.text != base.text {
            self.remember();
        }
        self.text = base.text.clone();
        self.cursor = self.cursor.min(self.text.len());
        while !self.text.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
        self.anchor = None;
        self.saved(base);
    }
    pub fn dirty(&self) -> bool {
        self.text != self.base.text
    }
    pub fn selection(&self) -> std::ops::Range<usize> {
        let anchor = self.anchor.unwrap_or(self.cursor);
        anchor.min(self.cursor)..anchor.max(self.cursor)
    }
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
            anchor: self.anchor,
        }
    }
    fn restore(&mut self, state: Snapshot) {
        self.text = state.text;
        self.cursor = state.cursor;
        self.anchor = state.anchor;
        self.last_edit = std::time::Instant::now();
    }
    fn remember(&mut self) {
        self.undo.push(self.snapshot());
        self.redo.clear();
        while self.undo.len() > 1
            && (self.undo.len() > 512
                || self.undo.iter().map(|s| s.text.len()).sum::<usize>() > HISTORY_BYTES)
        {
            self.undo.remove(0);
        }
    }
    pub fn insert(&mut self, text: &str) {
        self.manual_scroll = false;
        let range = self.selection();
        self.rejected = false;
        if self.text.len() - range.len() + text.len() > MAX_SOURCE_BYTES {
            self.rejected = true;
            return;
        }
        self.remember();
        self.last_edit = std::time::Instant::now();
        self.text.replace_range(range.clone(), text);
        self.cursor = range.start + text.len();
        self.anchor = None;
        if let Some((end, _)) = self
            .text
            .grapheme_indices(true)
            .find(|(i, _)| *i >= self.cursor)
        {
            self.cursor = end;
        } else {
            self.cursor = self.text.len();
        }
    }
    fn previous(&self) -> usize {
        if self.text[..self.cursor].ends_with("\r\n") {
            return self.cursor - 2;
        }
        self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }
    fn next(&self) -> usize {
        if self.text[self.cursor..].starts_with("\r\n") {
            return self.cursor + 2;
        }
        self.cursor
            + self.text[self.cursor..]
                .graphemes(true)
                .next()
                .map_or(0, str::len)
    }
    pub fn position(&self) -> (usize, usize) {
        let line = self.text[..self.cursor]
            .bytes()
            .filter(|b| *b == b'\n')
            .count();
        let start = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        (line, self.text[start..self.cursor].graphemes(true).count())
    }
    pub fn place_cursor(&mut self, offset: usize, extend: bool) {
        let offset = self
            .text
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .chain(std::iter::once(self.text.len()))
            .take_while(|i| *i <= offset)
            .last()
            .unwrap_or(0);
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
        self.cursor = offset;
    }

    pub fn scroll(&mut self, direction: i32) {
        let last = self.text.bytes().filter(|b| *b == b'\n').count();
        self.viewport_top.set(
            self.viewport_top
                .get()
                .saturating_add_signed(direction as isize * 3)
                .min(last),
        );
        self.manual_scroll = true;
    }

    pub fn key(&mut self, key: &KeyEvent) {
        self.manual_scroll = false;
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if control {
            match key.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Home | KeyCode::End => {
                    let target = match key.code {
                        KeyCode::Home => 0,
                        KeyCode::End => self.text.len(),
                        KeyCode::Left => self
                            .text
                            .unicode_word_indices()
                            .map(|(i, _)| i)
                            .take_while(|i| *i < self.cursor)
                            .last()
                            .unwrap_or(0),
                        _ => self
                            .text
                            .unicode_word_indices()
                            .map(|(i, _)| i)
                            .find(|i| *i > self.cursor)
                            .unwrap_or(self.text.len()),
                    };
                    self.place_cursor(target, shift);
                }
                KeyCode::Char('a') => {
                    self.anchor = Some(0);
                    self.cursor = self.text.len();
                }
                KeyCode::Char('z') => {
                    if let Some(s) = self.undo.pop() {
                        self.redo.push(self.snapshot());
                        self.restore(s);
                    }
                }
                KeyCode::Char('y') => {
                    if let Some(s) = self.redo.pop() {
                        self.undo.push(self.snapshot());
                        self.restore(s);
                    }
                }
                _ => {}
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::ALT) {
            return;
        }
        let movement = matches!(
            key.code,
            KeyCode::Left
                | KeyCode::Right
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::PageUp
                | KeyCode::PageDown
        );
        if movement {
            if shift && self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
            if !shift {
                self.anchor = None;
            }
            let (line, column) = self.position();
            match key.code {
                KeyCode::Left => self.cursor = self.previous(),
                KeyCode::Right => self.cursor = self.next(),
                KeyCode::Home => {
                    self.cursor = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1)
                }
                KeyCode::End => {
                    self.cursor = self.text[self.cursor..]
                        .find('\n')
                        .map_or(self.text.len(), |i| self.cursor + i);
                    if self.text[..self.cursor].ends_with('\r') {
                        self.cursor -= 1;
                    }
                }
                _ => {
                    let delta = if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
                        15
                    } else {
                        1
                    };
                    let target = if matches!(key.code, KeyCode::Up | KeyCode::PageUp) {
                        line.saturating_sub(delta)
                    } else {
                        line + delta
                    };
                    let start = if target == 0 {
                        0
                    } else {
                        self.text
                            .match_indices('\n')
                            .nth(target - 1)
                            .map_or(self.text.len(), |(i, _)| i + 1)
                    };
                    self.cursor = start
                        + self.text[start..]
                            .split('\n')
                            .next()
                            .unwrap_or("")
                            .trim_end_matches('\r')
                            .grapheme_indices(true)
                            .nth(column)
                            .map_or_else(
                                || {
                                    self.text[start..]
                                        .split('\n')
                                        .next()
                                        .unwrap_or("")
                                        .trim_end_matches('\r')
                                        .len()
                                },
                                |(i, _)| i,
                            );
                }
            }
        } else {
            match key.code {
                KeyCode::Char(c) => self.insert(&c.to_string()),
                KeyCode::Enter => self.insert(if self.base.text.contains("\r\n") {
                    "\r\n"
                } else {
                    "\n"
                }),
                KeyCode::Tab => self.insert("\t"),
                KeyCode::Backspace | KeyCode::Delete => {
                    if self.selection().is_empty() {
                        self.anchor = Some(if key.code == KeyCode::Backspace {
                            self.previous()
                        } else {
                            self.next()
                        });
                    }
                    if !self.selection().is_empty() {
                        self.insert("");
                    }
                }
                _ => {}
            }
        }
    }
}

/// One bounded background save. The input thread keeps accepting text.
pub struct SaveWorker {
    replies: std::sync::mpsc::Receiver<SaveReply>,
    handle: Option<std::thread::JoinHandle<()>>,
}
pub struct SaveReply {
    pub recovery_id: String,
    pub base: Version,
    pub result: Result<Version, crate::app::OperationError>,
}
impl SaveWorker {
    pub fn start(git: crate::git::Git, editor: &Editor) -> Self {
        let base = editor.base.clone();
        let text = editor.text.clone();
        let recovery_id = editor.recovery_id.clone();
        let (sender, replies) = std::sync::mpsc::sync_channel(1);
        let handle = std::thread::spawn(move || {
            let ops = crate::app::Operations::new(&git);
            let result = ops.save_document(&base, &text);
            let _ = sender.send(SaveReply {
                recovery_id,
                base,
                result,
            });
        });
        Self {
            replies,
            handle: Some(handle),
        }
    }
    pub fn receive(&self) -> Option<SaveReply> {
        self.replies.try_recv().ok()
    }
    pub fn finish(&self) -> Option<SaveReply> {
        self.replies.recv().ok()
    }
}
impl Drop for SaveWorker {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Show the changed line span from each version. This comparison is linear.
pub fn comparison(current: &str, disk: &str) -> String {
    let current_lines: Vec<&str> = current.split_inclusive('\n').collect();
    let disk_lines: Vec<&str> = disk.split_inclusive('\n').collect();
    let prefix = current_lines
        .iter()
        .zip(&disk_lines)
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = current_lines[prefix..]
        .iter()
        .rev()
        .zip(disk_lines[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    if prefix == current_lines.len() && prefix == disk_lines.len() {
        return "Your text matches the compared disk version.".into();
    }
    format!(
        "Disk comparison · changed span starts at line {}\n{} common opening lines and {} common closing lines omitted\n\nYour text\n{}\n\nCompared disk version\n{}\n\nCtrl+B keeps your text against this exact disk version. Autosave then resumes.\nCtrl+D compares disk again · Esc returns to editing",
        prefix + 1,
        prefix,
        suffix,
        current_lines[prefix..current_lines.len() - suffix].concat(),
        disk_lines[prefix..disk_lines.len() - suffix].concat()
    )
}
