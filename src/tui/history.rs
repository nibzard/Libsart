//! The History view: the editorial timeline and the restoration preview
//! (SPEC 5.5, 11.3; task T044).
//!
//! The view reads one page of the timeline through the shared `log`
//! operation, so starts, drafts, reviews, and restorations arrive labeled
//! and in ancestry order — never clock order — and a collapsed writing
//! pass keeps its checkpoint count stated while every checkpoint stays
//! listable through the `a` toggle (SPEC 5.5).
//!
//! Whole-document restoration is two explicit steps (SPEC 5.5). `Enter`
//! opens a preview against the current candidate C: the document at the
//! selected revision beside the document at C, with missing and empty kept
//! distinct, plus the intended result one read-only dry run of the shared
//! `restore` operation proved. Only the second `Enter` applies, and it
//! hands the same plan document `margin restore --input` reads to that
//! operation, so both surfaces agree on tree, marker, errors, and effects
//! (SPEC 7.4, 16.9). A restoration approves nothing else: the review
//! baseline stays unchanged, and the Changes view reviews the result.
//!
//! When uncaptured scoped changes exist, the shared operation refuses the
//! preview with `DIRTY_SCOPE`, and the view offers the SPEC 5.5 action,
//! **Save current draft, then preview restoration**: one checkpoint through
//! the shared operation, then a preview and a final command over the newly
//! captured candidate.

use std::collections::BTreeMap;
use std::ffi::OsString;

use crate::app::{
    self, CheckpointRequest, DocumentContent, Effects, Identities, LogRequest, OperationError,
    OperationOutcome, Operations, RestoreRequest, RestoreResult, ShowOutcome, ShowRequest,
};
use crate::domain::{CommitKind, RESTORE_PLAN_OPERATION, RESTORE_PLAN_VERSION, RestoreIdentity};
use crate::git::{Git, Mode};
use crate::tui::render;
use crate::tui::wrap::{self, ComposedLine, DisplayRow};

/// The request slot of the digest that names one restoration. The real
/// identifier cannot hash itself, so the digest holds this fixed placeholder
/// in that slot; only the digest's stability depends on the choice.
const REQUEST_ID_PLACEHOLDER: &str = "tui";

/// The prefix that marks one request identifier the terminal surface
/// composed, next to the identifiers a caller of the command line chooses.
const REQUEST_ID_PREFIX: &str = "tui:";

/// How many content bytes one preview side carries. The preview states the
/// bound when it truncates; it never invents an ending.
const PREVIEW_BYTES: u64 = 256 * 1024;

/// How many characters one reason or notice occupies.
const NOTICE_CHARS: usize = 200;

/// How many hexadecimal characters one opaque side shows.
const MAX_HEX_CHARS: usize = 96;

/// The typed code of the dirty-scope guard whose refusal the save offer
/// answers (SPEC 5.5, 9.5).
const DIRTY_SCOPE: &str = "DIRTY_SCOPE";

/// How many patch lines the hover excerpt of one revision shows. The
/// excerpt is a reading aid beside the hovered revision, not the
/// restoration preview `Enter` opens; it states the bound when it
/// truncates.
const EXCERPT_LINES: usize = 16;

/// The bounded before/after the hovered revision shows against the
/// current candidate: the literal patch lines of the displayed document
/// between the revision and the candidate, read-only, under the fixed
/// diff contract of SPEC 10.1. The excerpt never feeds a plan; the
/// restoration preview `Enter` opens computes its own proof.
#[derive(Clone, Debug)]
pub struct HoverExcerpt {
    /// The hovered revision the excerpt was computed for.
    commit: String,
    /// The candidate the excerpt was computed against.
    head: String,
    /// The bounded, display-escaped patch lines.
    lines: Vec<String>,
    /// Whether the patch held more lines than the excerpt shows.
    truncated: bool,
}

/// Parses the bounded excerpt lines of one single-file patch. The
/// structural header lines (`diff`, `index`, `old/new mode`, `---`, `+++`)
/// carry no prose: the excerpt keeps the hunk headers and the changed
/// lines themselves, display-escaped, and states its bound when the patch
/// holds more.
fn excerpt_lines(patch: &[u8]) -> (Vec<String>, bool) {
    let text = String::from_utf8_lossy(patch);
    let mut lines = Vec::new();
    let mut truncated = false;
    for line in text.lines() {
        if line.starts_with("diff ")
            || line.starts_with("index ")
            || line.starts_with("old mode ")
            || line.starts_with("new mode ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with("Binary files ")
        {
            continue;
        }
        if lines.len() == EXCERPT_LINES {
            truncated = true;
            break;
        }
        lines.push(crate::cli::display_escape(line));
    }
    (lines, truncated)
}

/// One row of the editorial timeline (SPEC 5.5).
#[derive(Clone, Debug)]
pub struct HistoryRow {
    pub notes: crate::notes::CommitNotes,
    /// The commit object ID. For a collapsed writing pass, its newest
    /// checkpoint.
    pub commit: String,
    /// The validated kind label.
    pub kind: &'static str,
    /// The bounded subject line.
    pub subject: String,
    /// How many draft checkpoints one collapsed entry covers.
    pub checkpoints: u64,
    /// The exact committer timestamp in Unix seconds, when the header
    /// parsed. The order of the timeline is ancestry, never the clock.
    pub committer_time: Option<i64>,
    /// The locally displayed date of the commit, when Git rendered one.
    pub local_date: Option<String>,
}

/// The restoration plan one preview confirms, with the identity it carries
/// (SPEC 7.4).
#[derive(Clone, Debug)]
pub struct PreparedRestore {
    /// The deterministic request identifier of this restoration.
    pub request_id: String,
    /// The exact plan document bytes: the document `margin restore --input`
    /// reads (SPEC 7.4, 16.9).
    pub document: Vec<u8>,
}

/// The whole-document restoration preview of SPEC 5.5: the document at the
/// selected revision, the document at the current candidate C, and the
/// intended result one read-only dry run proved.
#[derive(Clone, Debug)]
pub struct RestorePreview {
    /// The one scoped document the restoration would rewrite.
    path: String,
    /// The revision the content comes from.
    revision_oid: String,
    /// The kind label of the revision's timeline row.
    kind: &'static str,
    /// The subject of the revision's timeline row.
    subject: String,
    /// The locally displayed date of the revision.
    local_date: Option<String>,
    /// The exact committer timestamp of the revision, in Unix seconds.
    committer_time: Option<i64>,
    /// The candidate C the preview ran against; a saved draft replaces it
    /// with the newly captured candidate.
    candidate_oid: String,
    /// The document at the selected revision; `None` when the query failed.
    at_revision: Option<ShowOutcome>,
    /// The document at the current candidate; `None` when the query failed.
    at_candidate: Option<ShowOutcome>,
    /// The plan document and its request identifier, or the typed error
    /// that stopped the composition.
    plan: Result<PreparedRestore, OperationError>,
    /// The read-only proof of the intended result (SPEC 6.4), or its typed
    /// refusal.
    proof: RestoreResult,
}

impl RestorePreview {
    /// The one scoped document the restoration would rewrite.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The revision the restored content comes from.
    pub fn revision_oid(&self) -> &str {
        &self.revision_oid
    }

    /// The candidate the preview ran against.
    pub fn candidate_oid(&self) -> &str {
        &self.candidate_oid
    }

    /// The request identifier this restoration carries, when it could be
    /// computed.
    pub fn request_id(&self) -> Option<&str> {
        self.plan.as_ref().ok().map(|plan| plan.request_id.as_str())
    }

    /// The plan document this preview confirms, when it could be composed:
    /// the exact bytes `margin restore --input` reads (SPEC 7.4, 16.9).
    pub fn plan_document(&self) -> Option<&[u8]> {
        self.plan
            .as_ref()
            .ok()
            .map(|prepared| prepared.document.as_slice())
    }

    /// The read-only proof of the intended result (SPEC 6.4).
    pub fn proof(&self) -> &RestoreResult {
        &self.proof
    }

    /// Whether the preview proved one intended result the second `Enter`
    /// may apply.
    pub fn proved(&self) -> bool {
        self.proof.ok() && self.plan.is_ok()
    }

    /// Whether uncaptured scoped changes stopped the preview, so the SPEC
    /// 5.5 save offer answers it.
    pub fn save_offer(&self) -> bool {
        !self.proof.ok()
            && self
                .proof
                .error
                .as_ref()
                .is_some_and(|error| error.code == DIRTY_SCOPE)
    }

    /// Applies the previewed restoration through the shared operation
    /// (SPEC 9.5). The document applied is the document the preview proved,
    /// so an exact retry replays one identity and one fingerprint. A plan
    /// that could not be composed refuses with the typed error that stopped
    /// it and writes nothing.
    pub fn apply(&self, operations: &Operations<'_>) -> RestoreResult {
        match &self.plan {
            Err(error) => OperationOutcome::failure(error.clone(), Effects::unchanged()),
            Ok(prepared) => operations.restore_document(&RestoreRequest {
                scope: None,
                plan: prepared.document.clone(),
                dry_run: false,
            }),
        }
    }

    /// The lines of the preview page: both sides of the comparison, the
    /// proved intended result, and the action that answers it (SPEC 5.5).
    pub fn lines(&self) -> Vec<ComposedLine> {
        let mut lines = Vec::new();
        lines.push(ComposedLine::heading(format!(
            "Restore preview · {}",
            self.path
        )));
        lines.push(ComposedLine::meta(format!(
            "From {} {} · {}",
            self.kind,
            render::short(&self.revision_oid),
            self.subject,
        )));
        lines.push(ComposedLine::meta(timestamp_line(
            &self.local_date,
            self.committer_time,
        )));
        lines.push(ComposedLine::meta(format!(
            "Against the current candidate {}",
            render::short(&self.candidate_oid),
        )));
        lines.push(ComposedLine::text(String::new()));
        side_lines("At the selected revision", &self.at_revision, &mut lines);
        lines.push(ComposedLine::text(String::new()));
        side_lines("At the current candidate C", &self.at_candidate, &mut lines);
        lines.push(ComposedLine::text(String::new()));
        self.proof_lines(&mut lines);
        lines.push(ComposedLine::text(String::new()));
        if self.save_offer() {
            lines.push(ComposedLine::meta(
                "s or Enter  Save current draft, then preview restoration",
            ));
        } else if self.proved() {
            lines.push(ComposedLine::meta(
                "Enter  Restore this document from the selected revision",
            ));
        }
        lines.push(ComposedLine::meta("Esc  Cancel; nothing is applied"));
        lines
    }

    /// The intended-result half of the page: what the read-only proof
    /// proved, or the typed refusal that stopped it.
    fn proof_lines(&self, lines: &mut Vec<ComposedLine>) {
        if !self.proof.ok() {
            let error = self
                .proof
                .error
                .as_ref()
                .expect("a failed outcome carries its error");
            if self.save_offer() {
                lines.push(ComposedLine::warning(
                    "The preview cannot run while uncaptured changes exist:",
                ));
            } else {
                lines.push(ComposedLine::warning(
                    "The restoration cannot be proved right now:",
                ));
            }
            lines.push(ComposedLine::warning(format!(
                "{}: {}",
                error.code,
                bounded(&error.message)
            )));
            return;
        }
        let data = self
            .proof
            .data
            .as_ref()
            .expect("a successful proof carries data");
        lines.push(ComposedLine::text(
            "Intended result, proved by one read-only preview:",
        ));
        if data.paths.is_empty() {
            lines.push(ComposedLine::meta(
                "Files rewritten: none; the restoration records one receipt commit.",
            ));
        } else {
            lines.push(ComposedLine::meta(format!(
                "Files rewritten: {}",
                data.paths.join(", ")
            )));
        }
        lines.push(ComposedLine::meta(format!(
            "Result tree {}",
            render::short(&data.expected_tree)
        )));
        lines.push(ComposedLine::text(
            "The review baseline stays unchanged; this approves nothing else.",
        ));
        lines.push(ComposedLine::text(
            "The Changes view reviews the restored scope as usual.",
        ));
    }
}

/// The state of the standing preview, as the keys that answer it name it
/// (SPEC 11.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewState {
    /// No preview stands; the timeline answers the keys.
    Closed,
    /// Uncaptured changes stopped the preview; the save offer answers it
    /// (SPEC 5.5).
    SaveOffer,
    /// The preview proved its intended result; the second `Enter` applies.
    Ready,
    /// The preview stands but proves nothing, for the reason it states.
    Refused,
}

/// The History view state: one page of the editorial timeline, the selected
/// revision, and the restoration preview that may stand over it.
pub struct HistoryView {
    /// The timeline rows, newest first in ancestry order.
    rows: Vec<HistoryRow>,
    /// The scope identity the timeline bound; a restoration plan pins it.
    scope_id: String,
    /// Why the page stops short of the whole timeline, when it does.
    truncated: Option<String>,
    /// Whether every draft checkpoint is listed instead of one row per
    /// collapsed writing pass (SPEC 5.5).
    all_checkpoints: bool,
    details: bool,
    path_filter: Vec<String>,
    selection: usize,
    /// The standing restoration preview, when one stands (SPEC 5.5).
    preview: Option<RestorePreview>,
    /// The before/after excerpt of the hovered revision, when one was
    /// computed for the hover and candidate that stand.
    excerpt: Option<HoverExcerpt>,
    // The derived display state: the wrapped rows of the column, the first
    // row of every timeline entry, the width they were wrapped at, and the
    // top visible row.
    display: Vec<DisplayRow>,
    starts: Vec<usize>,
    width: u16,
    dirty: bool,
    offset: usize,
    viewport: usize,
}

impl HistoryView {
    /// Loads the first page of the editorial timeline through the shared
    /// `log` operation (SPEC 5.5, 6.3).
    pub fn load(operations: &Operations<'_>) -> Option<HistoryView> {
        Self::query(operations, false, Vec::new())
    }

    /// Timeline limited to revisions affecting the displayed document.
    pub fn load_document(operations: &Operations<'_>, path: &str) -> Option<HistoryView> {
        Self::query(operations, false, vec![path.to_string()])
    }

    pub fn toggle_details(&mut self) {
        self.details = !self.details;
        self.dirty = true;
    }

    /// Re-queries the timeline with the checkpoint display policy the `a`
    /// key selects, keeping the selection readable.
    fn query(
        operations: &Operations<'_>,
        all_checkpoints: bool,
        path_filter: Vec<String>,
    ) -> Option<HistoryView> {
        let outcome = operations.list_history(&LogRequest {
            scope: None,
            path_filter: path_filter.clone(),
            cursor: None,
            limit: None,
            all_checkpoints,
        });
        if !outcome.ok() {
            return None;
        }
        let data = outcome.data?;
        let dates = local_dates(operations.git(), &data.entries);
        let truncated = (!data.complete)
            .then(|| "this page lists the newest entries; the timeline holds more".to_string());
        let rows = data
            .entries
            .into_iter()
            .map(|entry| {
                let commit = entry.commit.clone();
                HistoryRow {
                    notes: entry.notes,
                    commit,
                    kind: entry.kind.as_str(),
                    subject: entry.subject,
                    checkpoints: entry.checkpoints,
                    committer_time: entry.committer_time,
                    local_date: dates.get(entry.commit.as_str()).cloned(),
                }
            })
            .collect();
        Some(HistoryView {
            rows,
            scope_id: data.scope_id,
            truncated,
            all_checkpoints,
            details: false,
            path_filter,
            selection: 0,
            preview: None,
            excerpt: None,
            display: Vec::new(),
            starts: Vec::new(),
            width: 0,
            dirty: true,
            offset: 0,
            viewport: 1,
        })
    }

    /// Re-queries the timeline with every checkpoint listed instead of one
    /// row per collapsed writing pass, and returns whether the policy
    /// changed (SPEC 5.5 `a`).
    pub fn toggle_checkpoints(&mut self, operations: &Operations<'_>) -> bool {
        let wanted = !self.all_checkpoints;
        let Some(mut fresh) = Self::query(operations, wanted, self.path_filter.clone()) else {
            return false;
        };
        // Keep the reader near the revision they were reading: the newest
        // checkpoint of the selected pass exists in both policies.
        let pinned = self
            .selected()
            .map(|row| row.commit.clone())
            .or_else(|| self.rows.get(self.selection).map(|row| row.commit.clone()));
        if let Some(pinned) = pinned
            && let Some(index) = fresh.rows.iter().position(|row| row.commit == pinned)
        {
            fresh.selection = index;
        }
        *self = fresh;
        true
    }

    /// The timeline rows, newest first in ancestry order.
    pub fn rows(&self) -> &[HistoryRow] {
        &self.rows
    }

    pub fn update_notes(&mut self, notes: &[crate::notes::CommitNotes]) -> bool {
        let mut changed = false;
        for row in &mut self.rows {
            if let Some(fresh) = notes.iter().find(|n| n.commit == row.commit)
                && row.notes != *fresh
            {
                row.notes = fresh.clone();
                self.dirty = true;
                changed = true;
            }
        }
        changed
    }

    /// The selected timeline row, when one exists.
    pub fn selected(&self) -> Option<&HistoryRow> {
        self.rows.get(self.selection)
    }

    /// The selected timeline index.
    pub fn selection(&self) -> usize {
        self.selection
    }

    /// The standing restoration preview, when one stands.
    pub fn preview(&self) -> Option<&RestorePreview> {
        self.preview.as_ref()
    }

    /// The before/after excerpt of the hovered revision, when one stands.
    pub fn excerpt(&self) -> Option<&HoverExcerpt> {
        self.excerpt.as_ref()
    }

    /// Drops the hover excerpt; the next quiet pass recomputes it.
    pub fn clear_excerpt(&mut self) {
        if self.excerpt.take().is_some() {
            self.dirty = true;
        }
    }

    /// Says whether the standing excerpt matches the hovered revision and
    /// the candidate, so one quiet pass can skip recomputing it. A
    /// standing restoration preview and an empty timeline want no excerpt;
    /// a missing candidate wants the excerpt gone.
    pub fn excerpt_settled(&self, head: Option<&str>) -> bool {
        if self.preview.is_some() || self.rows.is_empty() {
            return true;
        }
        match (self.excerpt.as_ref(), head) {
            (None, None) => true,
            (Some(excerpt), Some(head)) => {
                excerpt.head == head
                    && self
                        .rows
                        .get(self.selection)
                        .is_some_and(|row| row.commit == excerpt.commit)
            }
            _ => false,
        }
    }

    /// Computes the hover excerpt of the selected revision for `path`
    /// against `head`, one bounded patch through the fixed diff contract,
    /// only when the hover or the candidate moved since the excerpt that
    /// stands. The excerpt names both sides, so an unchanged excerpt over
    /// a moved hover can never stand.
    pub fn refresh_excerpt(&mut self, operations: &Operations<'_>, head: &str, path: &str) {
        let Some(row) = self.rows.get(self.selection) else {
            self.clear_excerpt();
            return;
        };
        let commit = row.commit.clone();
        if self
            .excerpt
            .as_ref()
            .is_some_and(|excerpt| excerpt.commit == commit && excerpt.head == head)
        {
            return;
        }
        let patch = operations
            .git()
            .diff_tree_patch(&commit, head, &[OsString::from(path)]);
        let excerpt = match patch {
            Ok(patch) => {
                let (lines, truncated) = excerpt_lines(&patch);
                HoverExcerpt {
                    commit,
                    head: head.to_string(),
                    lines,
                    truncated,
                }
            }
            Err(_) => HoverExcerpt {
                commit,
                head: head.to_string(),
                lines: vec!["the before/after could not be read".to_string()],
                truncated: false,
            },
        };
        self.excerpt = Some(excerpt);
        self.dirty = true;
    }

    /// The state of the standing preview, as the keys that answer it name
    /// it (SPEC 11.3).
    pub fn preview_state(&self) -> PreviewState {
        match &self.preview {
            None => PreviewState::Closed,
            Some(preview) if preview.save_offer() => PreviewState::SaveOffer,
            Some(preview) if preview.proved() => PreviewState::Ready,
            Some(_) => PreviewState::Refused,
        }
    }

    /// The wrapped rows of the column.
    pub fn display_rows(&self) -> &[DisplayRow] {
        &self.display
    }

    /// The first visible row of the column.
    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn revision_at_row(&self, row: usize) -> Option<usize> {
        if self.preview.is_some() {
            return None;
        }
        self.starts.iter().rposition(|start| row >= *start)
    }
    pub fn select(&mut self, index: usize) {
        if self.rows.is_empty() {
            return;
        }
        self.selection = index.min(self.rows.len() - 1);
        self.dirty = true;
        self.scroll_to_selection();
    }

    /// Moves the selection one timeline row and scrolls to it; returns
    /// whether the selection moved (SPEC 11.3 `j` and `k`).
    pub fn step_selection(&mut self, direction: i32) -> bool {
        if self.rows.is_empty() {
            return false;
        }
        let next = if direction > 0 {
            (self.selection + 1).min(self.rows.len() - 1)
        } else {
            self.selection.saturating_sub(1)
        };
        if next == self.selection {
            return false;
        }
        self.selection = next;
        self.dirty = true;
        self.scroll_to_selection();
        true
    }

    /// Moves the selection by one viewport of timeline rows, one step at a
    /// time, so paging lands on a row and scrolls to it (SPEC 11.3).
    pub fn page_selection(&mut self, direction: i32, page: usize) -> bool {
        let mut moved = false;
        for _ in 0..page.max(1) {
            moved |= self.step_selection(if direction > 0 { 1 } else { -1 });
        }
        moved
    }

    /// Finds the next revision whose commit, kind, or subject contains
    /// `needle`, folded to one case, and selects it (SPEC 11.3 `/`).
    /// Search moves the selection only. Returns the matched index.
    pub fn search(&mut self, needle: &str) -> Option<usize> {
        let folded = needle.to_lowercase();
        for step in 1..=self.rows.len() {
            let index = (self.selection + step) % self.rows.len();
            let row = &self.rows[index];
            let text = format!("{} {} {}", row.commit, row.kind, row.subject);
            if text.to_lowercase().contains(&folded) {
                self.selection = index;
                self.dirty = true;
                self.scroll_to_selection();
                return Some(index);
            }
        }
        None
    }

    /// Selects the row one revision names, when the page still holds it, so
    /// a reload keeps the selected revision readable (SPEC 5.4).
    pub fn select_commit(&mut self, commit: &str) {
        if let Some(index) = self.rows.iter().position(|row| row.commit == commit) {
            self.selection = index;
            self.dirty = true;
            self.scroll_to_selection();
        }
    }

    /// Scrolls the column so the selected row is visible.
    fn scroll_to_selection(&mut self) {
        if let Some(&start) = self.starts.get(self.selection) {
            self.offset = start;
            self.clamp_offset();
        }
    }

    /// Scrolls the standing preview page by `rows` rows (SPEC 11.3 `j`,
    /// `k`, and paging). The timeline itself moves by selection, never by
    /// a scroll that only follows the top row.
    pub fn scroll_preview(&mut self, rows: i32) {
        if rows >= 0 {
            self.offset = self.offset.saturating_add(rows as usize);
        } else {
            self.offset = self.offset.saturating_sub(rows.unsigned_abs() as usize);
        }
        self.clamp_offset();
    }

    /// Opens the restoration preview of `path` at the selected revision
    /// (SPEC 5.5). `Err` carries one bounded reason no preview could open.
    pub fn open_preview(&mut self, operations: &Operations<'_>, path: &str) -> Result<(), String> {
        let Some(row) = self.rows.get(self.selection) else {
            return Err("no revision is selected to preview".to_string());
        };
        let row = row.clone();
        let preview = RestorePreview::open(operations, path, &self.scope_id, &row)?;
        self.preview = Some(preview);
        self.dirty = true;
        Ok(())
    }

    /// Cancels the standing preview; nothing was applied (SPEC 11.3).
    pub fn cancel_preview(&mut self) {
        self.preview = None;
        self.dirty = true;
    }

    /// Applies the standing preview through the shared operation, when one
    /// stands.
    pub fn apply_preview(&self, operations: &Operations<'_>) -> Option<RestoreResult> {
        let preview = self.preview.as_ref()?;
        Some(preview.apply(operations))
    }

    /// Wraps the column for one frame size; the caller renders afterwards.
    /// The column is recomposed when the width, the selection, the
    /// checkpoint policy, or the preview changed.
    pub fn prepare(&mut self, width: u16, viewport_rows: usize) {
        self.viewport = viewport_rows.max(1);
        if self.dirty || self.display.is_empty() || width != self.width {
            self.compose(width);
            self.width = width;
            self.dirty = false;
            if self.preview.is_none() {
                self.scroll_to_selection();
            }
        }
        self.clamp_offset();
    }

    /// Clamps the offset to what the rows and viewport allow.
    fn clamp_offset(&mut self) {
        let last = self.display.len().saturating_sub(self.viewport.max(1));
        self.offset = self.offset.min(last);
    }

    /// Composes the whole column and wraps it, recording the first row of
    /// every timeline entry. A standing preview replaces the timeline for
    /// as long as it stands (SPEC 5.5).
    fn compose(&mut self, width: u16) {
        if let Some(preview) = &self.preview {
            self.display = wrap::wrap_rows(&preview.lines(), width);
            self.starts = Vec::new();
            return;
        }
        let mut lines = Vec::new();
        lines.push(ComposedLine::heading(format!(
            "  {:16}  {:7}  Version",
            "Date / time", "Commit"
        )));
        lines.push(ComposedLine::meta("Local time · newest first · d details"));
        if self.all_checkpoints {
            lines.push(ComposedLine::meta("Every draft checkpoint is listed."));
        } else {
            lines.push(ComposedLine::meta(
                "Writing passes collapsed · a shows all checkpoints",
            ));
        }
        if let Some(why) = &self.truncated {
            lines.push(ComposedLine::warning(format!(
                "This page is bounded: {why}."
            )));
        }
        lines.push(ComposedLine::text(String::new()));
        let mut starts = Vec::with_capacity(self.rows.len());
        for index in 0..self.rows.len() {
            // The entry lands on the next composed line, one-based.
            starts.push((lines.len() + 1) as u32);
            lines.push(self.row_line(index));
            for note in &self.rows[index].notes.notes {
                lines.push(ComposedLine::meta(format!(
                    "  {}{}",
                    note.label(),
                    if note.group.is_some() {
                        " · change annotation"
                    } else {
                        ""
                    }
                )));
                for line in note.display_text().lines() {
                    lines.push(ComposedLine::text(format!("  {line}")));
                }
            }
            if let Some(reason) = &self.rows[index].notes.unreadable {
                lines.push(ComposedLine::warning(reason));
            }
            // The hover excerpt stands under the revision it belongs to,
            // bounded, beside the action that opens the full preview
            // (SPEC 5.5): the reader sees what the revision holds against
            // the candidate without leaving the timeline.
            if index == self.selection
                && let Some(excerpt) = &self.excerpt
            {
                if excerpt.lines.is_empty() {
                    lines.push(ComposedLine::meta(
                        "  Identical to the current candidate; Enter previews the restoration",
                    ));
                } else {
                    lines.push(ComposedLine::meta(
                        "  Against the current candidate · Enter previews the restoration",
                    ));
                    for line in &excerpt.lines {
                        lines.push(ComposedLine::text(format!("  {line}")));
                    }
                    if excerpt.truncated {
                        lines.push(ComposedLine::meta(
                            "  …the excerpt is bounded; Enter previews the whole comparison",
                        ));
                    }
                }
            }
            if self.details && index == self.selection {
                let row = &self.rows[index];
                lines.push(ComposedLine::meta(format!(
                    "  {} · {}",
                    row.kind, row.commit
                )));
                lines.push(ComposedLine::meta(format!(
                    "  {}",
                    timestamp_line(&row.local_date, row.committer_time)
                )));
            }
        }
        self.display = wrap::wrap_rows(&lines, width);
        self.starts = starts
            .into_iter()
            .map(|line| wrap::row_for_anchor(&self.display, line))
            .collect();
    }

    /// One timeline row: selection, local date, commit, description, and
    /// checkpoint count. Technical metadata is available on demand. The selected
    /// row draws with weight; the marker keeps naming the selection
    /// (SPEC 11.4).
    fn row_line(&self, index: usize) -> ComposedLine {
        let row = &self.rows[index];
        let marker = if index == self.selection {
            "› "
        } else {
            "  "
        };
        let when = row
            .local_date
            .clone()
            .unwrap_or_else(|| "time unavailable".into());
        let passes = if row.checkpoints > 1 {
            format!(" ×{}", row.checkpoints)
        } else {
            String::new()
        };
        let text = format!(
            "{marker}{when}  {}  {}{passes}",
            render::short(&row.commit),
            row.subject,
        );
        if index == self.selection {
            ComposedLine::emphasis(text)
        } else {
            ComposedLine::text(text)
        }
    }
}

impl RestorePreview {
    /// Builds one preview through the shared operations: the document at
    /// the revision, the document at the current candidate C, the plan
    /// document of SPEC 7.4, and the dry run that proves the intended
    /// result. Nothing here writes.
    fn open(
        operations: &Operations<'_>,
        path: &str,
        scope_id: &str,
        row: &HistoryRow,
    ) -> Result<RestorePreview, String> {
        let git = operations.git();
        let candidate_oid = git
            .head_commit()
            .map_err(|error| format!("{}: {}", error.public_code(), bounded(&error.to_string())))?
            .ok_or_else(|| {
                "no candidate exists yet; the branch is unborn, so nothing can be restored"
                    .to_string()
            })?;
        let plan = prepared_restore(git, scope_id, &candidate_oid, path, &row.commit);
        let proof = match &plan {
            Err(error) => OperationOutcome::failure(error.clone(), Effects::unchanged()),
            Ok(prepared) => operations.restore_document(&RestoreRequest {
                scope: None,
                plan: prepared.document.clone(),
                dry_run: true,
            }),
        };
        let at_revision = show_at(operations, path, &row.commit);
        let at_candidate = show_at(operations, path, &candidate_oid);
        Ok(RestorePreview {
            path: path.to_string(),
            revision_oid: row.commit.clone(),
            kind: row.kind,
            subject: row.subject.clone(),
            local_date: row.local_date.clone(),
            committer_time: row.committer_time,
            candidate_oid,
            at_revision,
            at_candidate,
            plan,
            proof,
        })
    }
}

/// Saves the current draft through the shared checkpoint operation (SPEC
/// 5.5: the save offer of a dirty restoration preview). `Ok` carries one
/// notice that names the newly captured candidate; `Err` carries one
/// bounded reason the draft was not saved.
pub fn save_current_draft(operations: &Operations<'_>) -> Result<String, String> {
    let outcome = operations.create_checkpoint(&CheckpointRequest {
        scope: None,
        message: None,
        include_staged: false,
        expect_head: None,
        expect_index: None,
        expect_working: None,
        dry_run: false,
    });
    if !outcome.ok() {
        let error = outcome.error.expect("a failed outcome carries its error");
        return Err(format!("{}: {}", error.code, bounded(&error.message)));
    }
    let data = outcome.data.expect("a successful outcome carries data");
    Ok(format!(
        "draft saved as {}; the preview uses the newly captured candidate",
        render::short(&data.candidate),
    ))
}

/// Reads the document at one immutable revision; `None` when the query
/// failed. The operation resolves the revision once, so a moving branch
/// cannot change the bytes under the preview.
fn show_at(operations: &Operations<'_>, path: &str, revision: &str) -> Option<ShowOutcome> {
    let outcome = operations.show_document(&ShowRequest {
        path: path.to_string(),
        revision: revision.to_string(),
        max_bytes: Some(PREVIEW_BYTES),
        max_lines: None,
        raw: false,
    });
    if outcome.ok() { outcome.data } else { None }
}

/// Composes the restoration plan one preview confirms (SPEC 7.4): the
/// pinned context, the one document, and the source revision.
///
/// The message is omitted, so the plan carries the deterministic default
/// subject of a restore commit; this surface gathers no free text. The
/// request identifier is computed first, because the document cannot name
/// an identity it does not hold yet.
fn prepared_restore(
    git: &Git,
    scope_id: &str,
    candidate_oid: &str,
    path: &str,
    revision: &str,
) -> Result<PreparedRestore, OperationError> {
    let branch_ref = branch_ref_of(git)?;
    let placeholder = RestoreIdentity {
        operation: RESTORE_PLAN_OPERATION.to_string(),
        request_id: REQUEST_ID_PLACEHOLDER.to_string(),
        branch_ref,
        scope_id: scope_id.to_string(),
        candidate_oid: candidate_oid.to_string(),
        path: path.to_string(),
        revision: revision.to_string(),
        message: CommitKind::Restore.default_subject().to_string(),
    };
    let digest = Identities::new(git)
        .and_then(|identities| identities.restore_plan(&placeholder))
        .map_err(OperationError::from)?;
    let request_id = format!("{REQUEST_ID_PREFIX}{}", digest.id());
    let document = serde_json::json!({
        "schema_version": RESTORE_PLAN_VERSION,
        "request_id": request_id,
        "branch_ref": placeholder.branch_ref,
        "scope_id": placeholder.scope_id,
        "candidate_oid": placeholder.candidate_oid,
        "path": placeholder.path,
        "revision": placeholder.revision,
    })
    .to_string()
    .into_bytes();
    Ok(PreparedRestore {
        request_id,
        document,
    })
}

/// The full ref one restoration plan pins (SPEC 7.2, 9.5): where `HEAD` is
/// attached, which is the adopted branch in every ordinary workspace; a
/// detached `HEAD` falls back to the stored adopted branch, and a scope
/// without either keeps the deterministic name `HEAD`.
fn branch_ref_of(git: &Git) -> Result<String, OperationError> {
    match git.head_symbolic_ref() {
        Ok(Some(full)) => Ok(full),
        Ok(None) => Ok(app::read_local_settings(git)
            .ok()
            .and_then(|settings| settings.branch)
            .map(|branch| format!("refs/heads/{branch}"))
            .unwrap_or_else(|| "HEAD".to_string())),
        Err(error) => Err(OperationError::from(error)),
    }
}

/// The locally displayed dates of the page's commits, through the Git engine
/// the surface already reads. The order of the timeline stays ancestry; the
/// date is presentation, so a query that fails leaves the exact Unix
/// timestamps stated alone.
fn local_dates(git: &Git, entries: &[crate::app::LogEntry]) -> BTreeMap<String, String> {
    let mut dates = BTreeMap::new();
    if entries.is_empty() {
        return dates;
    }
    let mut argv: Vec<OsString> = vec![
        "log".into(),
        "--no-walk=unsorted".into(),
        "--date=format-local:%Y-%m-%d %H:%M".into(),
        "--format=%H%x1f%cd".into(),
    ];
    argv.extend(entries.iter().map(|entry| OsString::from(&entry.commit)));
    let Ok(stdout) = git.run_ok(Mode::Query, argv) else {
        return dates;
    };
    for line in String::from_utf8_lossy(&stdout).lines() {
        let Some((commit, date)) = line.split_once('\u{1f}') else {
            continue;
        };
        dates.insert(commit.to_string(), date.to_string());
    }
    dates
}

/// The timestamp line of one preview header: the local display and the
/// exact value together, so the clock never has to be trusted for order
/// (SPEC 5.5).
fn timestamp_line(local: &Option<String>, exact: Option<i64>) -> String {
    match (local, exact) {
        (Some(local), Some(exact)) => format!("Committed {local} · exact {exact} (Unix seconds)"),
        (Some(local), None) => format!("Committed {local}"),
        (None, Some(exact)) => format!("Committed at Unix second {exact}"),
        (None, None) => "Commit timestamp unknown".to_string(),
    }
}

/// The lines of one comparison side: the document's content state, with
/// missing and empty kept distinct (SPEC 5.5).
fn side_lines(label: &str, shown: &Option<ShowOutcome>, lines: &mut Vec<ComposedLine>) {
    lines.push(ComposedLine::meta(format!("{label}:")));
    let Some(outcome) = shown else {
        lines.push(ComposedLine::warning(
            "  the document view failed; no honest preview exists",
        ));
        return;
    };
    match &outcome.content {
        DocumentContent::Missing => lines.push(ComposedLine::warning(
            "  missing: no document exists at this revision; restoring removes the file",
        )),
        DocumentContent::Empty { .. } => lines.push(ComposedLine::meta(
            "  empty: the document exists here and holds zero bytes",
        )),
        DocumentContent::Text(text) => {
            let escaped = crate::cli::display_escape(&text.text);
            for line in escaped.lines() {
                lines.push(ComposedLine::text(format!("  {line}")));
            }
            if text.truncated {
                lines.push(ComposedLine::warning(format!(
                    "  the preview is bounded: {} of {} bytes shown",
                    text.text.len(),
                    text.total_bytes,
                )));
            }
        }
        DocumentContent::Opaque(view) => {
            let mut shown: String = view.hex.chars().take(MAX_HEX_CHARS).collect();
            if view.hex.chars().count() > MAX_HEX_CHARS {
                shown.push('…');
            }
            lines.push(ComposedLine::text(format!(
                "  {shown} ({} bytes, not valid UTF-8)",
                view.total_bytes,
            )));
        }
        DocumentContent::Exact { .. } => lines.push(ComposedLine::text(
            "  the exact bytes of a raw export cannot preview here",
        )),
    }
}

/// Truncates one status line to the notice bound.
fn bounded(text: &str) -> String {
    let mut bounded: String = text.chars().take(NOTICE_CHARS).collect();
    if bounded.chars().count() < text.chars().count() {
        bounded.push('…');
    }
    bounded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{BoundedDocumentText, ShowOutcome};
    use crate::domain::EntryMode;
    use crate::git::ObjectFormat;

    /// One document view over `content`, at a fixed revision.
    fn shown(content: DocumentContent) -> ShowOutcome {
        ShowOutcome {
            revision_oid: "a".repeat(40),
            path: "notes/a.md".to_string(),
            object_format: ObjectFormat::Sha1,
            content,
        }
    }

    /// One bounded text view of `text`.
    fn text_of(text: &str) -> BoundedDocumentText {
        BoundedDocumentText {
            blob_oid: "b".repeat(40),
            mode: EntryMode::Regular,
            text: text.to_string(),
            total_bytes: text.len(),
            returned_bytes: text.len(),
            total_lines: text.lines().count() as u64,
            returned_lines: text.lines().count() as u64,
            truncated: false,
        }
    }

    #[test]
    fn timestamps_pair_the_local_clock_with_the_exact_value() {
        assert_eq!(
            timestamp_line(&Some("2026-01-01 00:00".to_string()), Some(1767225600)),
            "Committed 2026-01-01 00:00 · exact 1767225600 (Unix seconds)",
        );
        assert_eq!(
            timestamp_line(&Some("2026-01-01 00:00".to_string()), None),
            "Committed 2026-01-01 00:00",
        );
        assert_eq!(
            timestamp_line(&None, Some(1767225600)),
            "Committed at Unix second 1767225600",
        );
        assert_eq!(timestamp_line(&None, None), "Commit timestamp unknown",);
    }

    #[test]
    fn one_side_names_absence_and_emptiness_in_distinct_words() {
        // Absence is one answer, emptiness is another, and readable text is
        // a third; the preview never renders one as the others (SPEC 5.5).
        let mut lines = Vec::new();
        side_lines(
            "At the selected revision",
            &Some(shown(DocumentContent::Missing)),
            &mut lines,
        );
        assert_eq!(lines[0].text, "At the selected revision:");
        assert!(
            lines[1]
                .text
                .contains("missing: no document exists at this revision")
        );
        assert!(lines[1].text.contains("restoring removes the file"));

        let mut lines = Vec::new();
        side_lines(
            "At the current candidate C",
            &Some(shown(DocumentContent::Empty {
                blob_oid: "e".repeat(40),
                mode: EntryMode::Regular,
            })),
            &mut lines,
        );
        assert_eq!(lines[0].text, "At the current candidate C:");
        assert_eq!(
            lines[1].text,
            "  empty: the document exists here and holds zero bytes",
        );

        let mut lines = Vec::new();
        side_lines(
            "At the selected revision",
            &Some(shown(DocumentContent::Text(text_of("first\n\nsecond")))),
            &mut lines,
        );
        assert_eq!(lines[1].text, "  first");
        assert!(
            lines[2].text.trim().is_empty(),
            "the blank line stays blank"
        );
        assert_eq!(lines[3].text, "  second");

        let mut lines = Vec::new();
        side_lines("At the selected revision", &None, &mut lines);
        assert_eq!(
            lines[1].text,
            "  the document view failed; no honest preview exists",
        );
    }

    /// One single-file patch with every structural header kind and more
    /// changed lines than the excerpt bound.
    fn long_patch() -> String {
        let mut patch = String::from(
            "diff --git a/notes/a.md b/notes/a.md\nindex 111..222 100644\n--- a/notes/a.md\n+++ b/notes/a.md\n",
        );
        patch.push_str("@@ -1,3 +1,4 @@\n");
        for number in 0..(EXCERPT_LINES + 5) {
            patch.push_str(&format!("{number}\n"));
        }
        patch
    }

    #[test]
    fn the_hover_excerpt_keeps_hunks_and_changed_lines_and_states_its_bound() {
        let (lines, truncated) = excerpt_lines(long_patch().as_bytes());
        assert!(
            lines.iter().all(|line| !line.starts_with("diff ")
                && !line.starts_with("index ")
                && !line.starts_with("--- ")
                && !line.starts_with("+++ ")),
            "no structural header line survives: {lines:?}",
        );
        assert!(lines.contains(&"@@ -1,3 +1,4 @@".to_string()));
        assert_eq!(lines.len(), EXCERPT_LINES);
        assert!(truncated, "the patch held more lines than the bound");

        let (lines, truncated) = excerpt_lines(
            b"diff --git a/x b/x\nindex 1..2\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n",
        );
        assert_eq!(
            lines,
            vec![
                "@@ -1 +1 @@".to_string(),
                "-old".to_string(),
                "+new".to_string()
            ]
        );
        assert!(!truncated);

        let (lines, _) = excerpt_lines(b"");
        assert!(lines.is_empty());
    }

    #[test]
    fn the_hover_excerpt_escapes_terminal_controls() {
        let (lines, _) = excerpt_lines(b"@@ -1 +1 @@\n+safe\x1b[31mred\x1b[0m\n");
        assert_eq!(lines.len(), 2);
        assert!(
            !lines[1].contains('\x1b'),
            "the escape never reaches the frame: {:?}",
            lines[1],
        );
    }
}
