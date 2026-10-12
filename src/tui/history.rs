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
use crate::tui::wrap::{self, DisplayRow};

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

/// One row of the editorial timeline (SPEC 5.5).
#[derive(Clone, Debug)]
pub struct HistoryRow {
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
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(format!("Restore preview · {}", self.path));
        lines.push(format!(
            "From {} {} · {}",
            self.kind,
            render::short(&self.revision_oid),
            self.subject,
        ));
        lines.push(timestamp_line(&self.local_date, self.committer_time));
        lines.push(format!(
            "Against the current candidate {}",
            render::short(&self.candidate_oid),
        ));
        lines.push(String::new());
        side_lines("At the selected revision", &self.at_revision, &mut lines);
        lines.push(String::new());
        side_lines("At the current candidate C", &self.at_candidate, &mut lines);
        lines.push(String::new());
        self.proof_lines(&mut lines);
        lines.push(String::new());
        if self.save_offer() {
            lines.push("s or Enter  Save current draft, then preview restoration".to_string());
        } else if self.proved() {
            lines.push("Enter  Restore this document from the selected revision".to_string());
        }
        lines.push("Esc  Cancel; nothing is applied".to_string());
        lines
    }

    /// The intended-result half of the page: what the read-only proof
    /// proved, or the typed refusal that stopped it.
    fn proof_lines(&self, lines: &mut Vec<String>) {
        if !self.proof.ok() {
            let error = self
                .proof
                .error
                .as_ref()
                .expect("a failed outcome carries its error");
            if self.save_offer() {
                lines.push("The preview cannot run while uncaptured changes exist:".to_string());
            } else {
                lines.push("The restoration cannot be proved right now:".to_string());
            }
            lines.push(format!("{}: {}", error.code, bounded(&error.message)));
            return;
        }
        let data = self
            .proof
            .data
            .as_ref()
            .expect("a successful proof carries data");
        lines.push("Intended result, proved by one read-only preview:".to_string());
        if data.paths.is_empty() {
            lines.push(
                "Files rewritten: none; the restoration records one receipt commit.".to_string(),
            );
        } else {
            lines.push(format!("Files rewritten: {}", data.paths.join(", ")));
        }
        lines.push(format!(
            "Result tree {}",
            render::short(&data.expected_tree)
        ));
        lines.push("The review baseline stays unchanged; this approves nothing else.".to_string());
        lines.push("The Changes view reviews the restored scope as usual.".to_string());
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
    selection: usize,
    /// The standing restoration preview, when one stands (SPEC 5.5).
    preview: Option<RestorePreview>,
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
        Self::query(operations, false)
    }

    /// Re-queries the timeline with the checkpoint display policy the `a`
    /// key selects, keeping the selection readable.
    fn query(operations: &Operations<'_>, all_checkpoints: bool) -> Option<HistoryView> {
        let outcome = operations.list_history(&LogRequest {
            scope: None,
            path_filter: Vec::new(),
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
            selection: 0,
            preview: None,
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
        let Some(mut fresh) = Self::query(operations, wanted) else {
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
            self.display = wrap::wrap_text(&preview.lines().join("\n"), width);
            self.starts = Vec::new();
            return;
        }
        let mut lines = Vec::new();
        lines.push("History · the writing timeline, in ancestry order".to_string());
        lines.push(
            "Times show the local clock; the value in parentheses is the exact Unix timestamp."
                .to_string(),
        );
        if self.all_checkpoints {
            lines.push("Every draft checkpoint is listed.".to_string());
        } else {
            lines.push(
                "A count marks the checkpoints of one collapsed writing pass; a lists them all."
                    .to_string(),
            );
        }
        if let Some(why) = &self.truncated {
            lines.push(format!("This page is bounded: {why}."));
        }
        lines.push(String::new());
        let mut starts = Vec::with_capacity(self.rows.len());
        for index in 0..self.rows.len() {
            // The entry lands on the next composed line, one-based.
            starts.push((lines.len() + 1) as u32);
            lines.push(self.row_line(index));
        }
        self.display = wrap::wrap_text(&lines.join("\n"), width);
        self.starts = starts
            .into_iter()
            .map(|line| wrap::row_for_anchor(&self.display, line))
            .collect();
    }

    /// One timeline row: the selection marker, the kind label, the commit,
    /// the timestamps, the subject, and the checkpoint count.
    fn row_line(&self, index: usize) -> String {
        let row = &self.rows[index];
        let marker = if index == self.selection {
            "› "
        } else {
            "  "
        };
        let when = match (&row.local_date, row.committer_time) {
            (Some(local), Some(exact)) => format!("{local} ({exact})"),
            (Some(local), None) => local.clone(),
            (None, Some(exact)) => format!("unknown local time ({exact})"),
            (None, None) => "no timestamp".to_string(),
        };
        let passes = if row.checkpoints > 1 {
            format!(" ×{}", row.checkpoints)
        } else {
            String::new()
        };
        format!(
            "{marker}{:<8}{}  {when}  {}{passes}",
            row.kind,
            render::short(&row.commit),
            row.subject,
        )
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
fn side_lines(label: &str, shown: &Option<ShowOutcome>, lines: &mut Vec<String>) {
    lines.push(format!("{label}:"));
    let Some(outcome) = shown else {
        lines.push("  the document view failed; no honest preview exists".to_string());
        return;
    };
    match &outcome.content {
        DocumentContent::Missing => lines.push(
            "  missing: no document exists at this revision; restoring removes the file"
                .to_string(),
        ),
        DocumentContent::Empty { .. } => {
            lines.push("  empty: the document exists here and holds zero bytes".to_string())
        }
        DocumentContent::Text(text) => {
            let escaped = crate::cli::display_escape(&text.text);
            for line in escaped.lines() {
                lines.push(format!("  {line}"));
            }
            if text.truncated {
                lines.push(format!(
                    "  the preview is bounded: {} of {} bytes shown",
                    text.text.len(),
                    text.total_bytes,
                ));
            }
        }
        DocumentContent::Opaque(view) => {
            let mut shown: String = view.hex.chars().take(MAX_HEX_CHARS).collect();
            if view.hex.chars().count() > MAX_HEX_CHARS {
                shown.push('…');
            }
            lines.push(format!(
                "  {shown} ({} bytes, not valid UTF-8)",
                view.total_bytes,
            ));
        }
        DocumentContent::Exact { .. } => {
            lines.push("  the exact bytes of a raw export cannot preview here".to_string())
        }
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
        assert_eq!(lines[0], "At the selected revision:");
        assert!(lines[1].contains("missing: no document exists at this revision"));
        assert!(lines[1].contains("restoring removes the file"));

        let mut lines = Vec::new();
        side_lines(
            "At the current candidate C",
            &Some(shown(DocumentContent::Empty {
                blob_oid: "e".repeat(40),
                mode: EntryMode::Regular,
            })),
            &mut lines,
        );
        assert_eq!(lines[0], "At the current candidate C:");
        assert_eq!(
            lines[1],
            "  empty: the document exists here and holds zero bytes",
        );

        let mut lines = Vec::new();
        side_lines(
            "At the selected revision",
            &Some(shown(DocumentContent::Text(text_of("first\n\nsecond")))),
            &mut lines,
        );
        assert_eq!(lines[1], "  first");
        assert!(lines[2].trim().is_empty(), "the blank line stays blank");
        assert_eq!(lines[3], "  second");

        let mut lines = Vec::new();
        side_lines("At the selected revision", &None, &mut lines);
        assert_eq!(
            lines[1],
            "  the document view failed; no honest preview exists",
        );
    }
}
