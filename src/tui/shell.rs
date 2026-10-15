//! The terminal shell: views, retained context, and read-only loading.
//!
//! The shell holds every piece of state the surface needs and nothing
//! else. It loads its data through the shared operations of
//! [`crate::app`], so the terminal and the command line read one
//! repository through one contract. Opening the shell before adoption
//! changes nothing (SPEC 5.1), and every key it answers changes at most
//! what the interface shows — the `K` and `r` keys of the Changes view
//! included, which change proposed choices and schedule local intent saving
//! without applying a review (SPEC 5.2).
//!
//! Review and restoration application both hand a composed plan document to
//! the same shared operation the command line exposes (SPEC 5.2, 5.5,
//! 16.9): the confirmed finish of a review — the flow lives in
//! [`crate::tui::finish`], this shell opens its summary and applies it on
//! the second `Enter` — and the confirmed restoration of one document,
//! which the History view previews first and restores on its second
//! `Enter` through [`crate::tui::history`]. Both reload the read-only
//! state afterwards. Quit flushes local progress and never commits choices;
//! save failure requires explicit abandonment of newer in-memory intent.
//!
//! The repository moves around the pinned comparison while the surface
//! stays open, so [`crate::tui::refresh`] observes it: the worker's
//! answers arrive through [`Shell::observe_inspection`], an arrival
//! relabels the review (SPEC 5.3), and `R` rebuilds the read-only state
//! explicitly without losing the pinned context.
//!
//! Three things survive a view switch by design (SPEC 5.4): the selected
//! document, its nearest source anchor, and the reading position derived
//! from that anchor. The Changes view keeps its own retained state beside
//! those: the selected change group and the plan. A resize restores the
//! position the same way, from the anchor plus a best-effort display
//! offset, never from a rendered row number (SPEC 11.2) — and so does one
//! refresh, because the anchor is the position, not a row count.

use crate::tui::session::{self, Anchor, Navigation, Record};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::app::{
    self, BaselineStatus, CheckpointRequest, DocumentContent, InspectRequest, Operations,
    PolicyBlocker, RestoreResult, ReviewDecisionResult, ScopedPathState, ShowOutcome, ShowRequest,
    WorkspaceInspection,
};
use crate::domain::{OperationState, PreservationState, ReviewState, ScopeDescriptor, ScopeInput};
use crate::tui::actions::{Action, MenuEntry};
use crate::tui::changes::{ChangesView, Decision, ToggleOutcome};
use crate::tui::finish::FinishSummary;
use crate::tui::history::{HistoryView, RestorePreview};
use crate::tui::layout::{self, WidthClass};
use crate::tui::refresh::{self, InspectionReply, PinnedObservation};
use crate::tui::render;
use crate::tui::search::{SearchWorker, WorkspaceSearch};
use crate::tui::wrap::{self, DisplayRow};

/// How many content bytes the Read view carries at once. The view states
/// the bound when it truncates; it never invents an ending.
pub(crate) const READ_VIEW_BYTES: u64 = 256 * 1024;

/// How many characters one status notice may occupy.
const NOTICE_CHARS: usize = 200;

/// How many characters one text-entry field carries (SPEC 11.3): the
/// bound keeps a search query or a picker filter inside one context row.
const FIELD_CHARS: usize = 120;

/// How many documents the pre-adoption discovery lists (SPEC 5.1).
const MAX_DISCOVERED_DOCUMENTS: usize = 500;

/// How deep the pre-adoption discovery walks.
const MAX_DISCOVERY_DEPTH: usize = 8;

/// The three views of SPEC 5.4.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum View {
    /// Read C, or an explicitly labeled working preview.
    #[default]
    Read,
    /// Review B to C with proposed restorations.
    Changes,
    /// Browse Git revisions of a document.
    History,
}

impl View {
    /// The label every view shows.
    pub fn label(self) -> &'static str {
        match self {
            View::Read => "Read",
            View::Changes => "Changes",
            View::History => "History",
        }
    }

    /// Parses the `--view` value of the command line.
    pub fn parse(raw: &str) -> Option<View> {
        match raw {
            "read" => Some(View::Read),
            "changes" => Some(View::Changes),
            "history" => Some(View::History),
            _ => None,
        }
    }
}

/// Where keyboard focus rests (SPEC 11.3): the navigation list, the
/// document column, or the standing action menu. The footer names the
/// focused element in words, so focus is visible, never inferred.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    /// The document list.
    Documents,
    /// The reading column.
    Reading,
    /// The action menu, while one stands.
    Actions,
    Reference,
}

impl Focus {
    /// The label the footer prints for the focused element.
    pub fn label(self) -> &'static str {
        match self {
            Focus::Documents => "document list",
            Focus::Reading => "reading",
            Focus::Actions => "action menu",
            Focus::Reference => "reference · read-only",
        }
    }
}

/// One standing overlay of the surface (SPEC 11.3): the action menu, the
/// help list, or one of the two text-entry fields. The picker and the
/// search field hold the text the writer typed; ordinary keys are text
/// there, not commands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Overlay {
    SavedReviews {
        selection: usize,
        inspect: bool,
        confirm: bool,
    },
    /// The action menu over the reading column; the number is the
    /// selected entry.
    Menu {
        /// The selected entry of [`Action::ALL`], as an index into the
        /// menu the shell builds.
        selection: usize,
    },
    /// The contextual key list and the current blockers.
    Help,
    /// The document picker with its filter text.
    Picker {
        /// The filter text the writer typed.
        filter: String,
    },
    /// The search field with its query text.
    Search {
        /// The query text the writer typed.
        query: String,
    },
    /// Search the current working source of all listed documents.
    WorkspaceSearch,
    /// Writer feedback anchored when the field opened, independent of refresh.
    Note {
        commit: String,
        group: String,
        text: String,
    },
}

impl Overlay {
    /// Says whether this overlay is a text-entry field, where ordinary
    /// characters are text (SPEC 11.3).
    fn is_text_entry(&self) -> bool {
        matches!(
            self,
            Overlay::Menu { .. }
                | Overlay::Picker { .. }
                | Overlay::Search { .. }
                | Overlay::WorkspaceSearch
                | Overlay::Note { .. }
        )
    }
}

/// How the surface may decorate its text (SPEC 11.4): color-off mode
/// draws a plain monochrome surface, and every state stays labeled in
/// words and symbols, never hue alone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorMode {
    /// Decorate when the terminal supports it.
    #[default]
    Auto,
    /// Decorate even when redirection hides the terminal's support.
    Always,
    /// Draw no decoration at all; words and symbols carry every state.
    Never,
}

impl ColorMode {
    /// Parses the `--color` value of the command line.
    pub fn parse(raw: &str) -> Option<ColorMode> {
        match raw {
            "auto" => Some(ColorMode::Auto),
            "always" => Some(ColorMode::Always),
            "never" => Some(ColorMode::Never),
            _ => None,
        }
    }
}

/// One requested handoff of the displayed document to the configured
/// editor (SPEC 11.3 `e`, 12.5): the explicit argv from the local
/// settings and the literal path of the document to edit. The shell only
/// requests; the event loop suspends the terminal, runs the child, and
/// reports the return, so the shell itself never owns a terminal.
#[derive(Clone, Debug)]
pub struct EditorHandoff {
    /// The configured editor argv, executable first.
    pub argv: Vec<String>,
    /// The literal path of the document to edit.
    pub path: PathBuf,
}

/// The saved state of one displayed document (SPEC 5.4): uncaptured
/// working content is an unsaved draft, and the label says so in words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SavedState {
    /// The working bytes equal the candidate.
    Saved,
    /// The working bytes differ from the candidate, or no candidate
    /// exists. The view shows the working preview and labels it.
    UnsavedDraft,
    /// The candidate holds the document but the working file is gone.
    WorkingFileGone,
    /// The comparison is not honest here, for the reason the notice or the
    /// content states.
    Unknown,
}

impl SavedState {
    /// The label the interface prints. Words, never hue alone (SPEC 11.4).
    pub fn label(self) -> &'static str {
        match self {
            SavedState::Saved => "Saved",
            SavedState::UnsavedDraft => "Unsaved draft",
            SavedState::WorkingFileGone => "Saved, working file gone",
            SavedState::Unknown => "Save state unknown",
        }
    }
}

/// One displayed document: where its text came from and what state it is
/// in.
#[derive(Clone, Debug)]
pub struct DocumentView {
    /// The repository-relative path.
    pub path: String,
    /// The immutable candidate commit the saved comparison used; `None`
    /// before adoption and when no candidate exists.
    pub candidate: Option<String>,
    /// The saved state of this document.
    pub saved: SavedState,
    /// The bounded, display-escaped text the view shows.
    pub text: String,
    /// Byte length of the complete document, when known.
    pub total_bytes: Option<u64>,
    /// Whether the view dropped content to a bound.
    pub truncated: bool,
}

/// The nearest source anchor of the reading position (SPEC 5.4, 11.2):
/// which document, and which one-based source line the reader rests on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceAnchor {
    /// The repository-relative path.
    pub path: String,
    /// The one-based source line.
    pub line: u32,
}

/// The workspace state one frame summarizes (SPEC 4.2, 5.2): branch,
/// preservation, and review, or the honest note that a query failed. The
/// fields beyond the sentences — the scope identity, the uncaptured
/// paths, the mutation blockers, and the operation state — are what
/// polling compares against the observation the pinned comparison
/// carries (SPEC 5.3, 13.3).
#[derive(Clone, Debug)]
pub struct WorkspaceSnapshot {
    /// Whether a stored writing scope exists.
    pub adopted: bool,
    /// The folder the surface opened on.
    pub folder: String,
    /// The adopted scope roots, such as `notes/`; empty before adoption.
    pub scope_roots: String,
    /// The full scope digest of the adopted scope; empty before adoption.
    pub scope_id: String,
    /// The adopted branch name; `None` while detached or unborn.
    pub branch: Option<String>,
    /// The candidate commit; `None` while unborn.
    pub head: Option<String>,
    /// Whether the observed scoped working content is in the candidate.
    pub preservation: PreservationState,
    /// Whether scoped differences remain against a valid baseline.
    pub review: ReviewState,
    /// Changed scoped files since the baseline; `None` when unknown.
    pub pending_files: Option<usize>,
    /// Uncaptured scoped paths; `None` when unknown.
    pub uncaptured: Option<usize>,
    /// The uncaptured scoped paths, sorted by path bytes; the paths name
    /// what a checkpoint would capture.
    pub uncaptured_paths: Vec<String>,
    /// Scoped documents the resolver listed.
    pub documents: usize,
    /// Repository and storage policy blockers in guard order (SPEC 9.1).
    pub blockers: Vec<PolicyBlocker>,
    /// What a mutation is doing, or what must be inspected before the
    /// next one (SPEC 4.2 operation).
    pub operation: OperationState,
    /// Why the workspace state is incomplete, when a query failed.
    pub note: Option<String>,
}

impl WorkspaceSnapshot {
    /// The snapshot of an unadopted folder.
    fn unadopted(folder: &str, note: Option<String>) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            adopted: false,
            folder: folder.to_string(),
            scope_roots: String::new(),
            scope_id: String::new(),
            branch: None,
            head: None,
            preservation: PreservationState::Uncaptured,
            review: ReviewState::Blocked,
            pending_files: None,
            uncaptured: None,
            uncaptured_paths: Vec::new(),
            documents: 0,
            blockers: Vec::new(),
            operation: OperationState::Idle,
            note,
        }
    }

    /// The sentence that states the preservation state in words.
    pub fn preservation_sentence(&self) -> String {
        match self.preservation {
            PreservationState::Captured => "all drafts saved".to_string(),
            PreservationState::Uncaptured => match self.uncaptured {
                Some(1) => "1 unsaved document".to_string(),
                Some(count) => format!("{count} unsaved documents"),
                None => "unsaved drafts".to_string(),
            },
            PreservationState::Unknown => "save state unknown".to_string(),
        }
    }

    /// The sentence that states the review state in words (SPEC 5.2).
    pub fn review_sentence(&self) -> String {
        match self.review {
            ReviewState::Pending => match self.pending_files {
                Some(1) => "1 document changed since your last review".to_string(),
                Some(count) => format!("{count} documents changed since your last review"),
                None => "changes wait for your review".to_string(),
            },
            ReviewState::Reviewed => "all changes reviewed".to_string(),
            ReviewState::StartingPoint => "starting point saved, nothing reviewed yet".to_string(),
            ReviewState::Blocked => "review unavailable".to_string(),
        }
    }

    /// The shortened review sentence of a reduced-metadata frame
    /// (SPEC 11.2). It states the same state in fewer words.
    pub fn review_sentence_short(&self) -> String {
        match self.review {
            ReviewState::Pending => match self.pending_files {
                Some(count) => format!("{count} changed since last review"),
                None => "changes to review".to_string(),
            },
            ReviewState::Reviewed => "all reviewed".to_string(),
            ReviewState::StartingPoint => "not reviewed yet".to_string(),
            ReviewState::Blocked => "review unavailable".to_string(),
        }
    }

    /// The roots a concise setup action would adopt (SPEC 5.1): the one
    /// directory every discovered document lives under, or the folder
    /// itself when the documents spread.
    pub fn setup_roots(&self, documents: &[String]) -> String {
        let shared = documents.iter().find_map(|path| {
            let (head, _) = path.split_once('/')?;
            Some(head.to_string())
        });
        match shared {
            Some(root)
                if documents
                    .iter()
                    .all(|path| path.starts_with(&format!("{root}/"))) =>
            {
                format!("{root}/")
            }
            _ => ".".to_string(),
        }
    }

    /// The concise setup action of first use (SPEC 5.1): the command line
    /// that adopts this folder. The shell shows it; adopting stays an
    /// explicit command.
    pub fn setup_action(&self, documents: &[String]) -> String {
        format!(
            "margin init --scope {} --auto-checkpoint",
            self.setup_roots(documents)
        )
    }
}

/// The interface-level choices of `margin tui`.
#[derive(Clone, Debug, Default)]
pub struct TuiOptions {
    /// The initial document.
    pub path: Option<String>,
    /// The initial view.
    pub view: View,
    /// Whether --view was explicitly supplied, including --view read.
    pub explicit_view: bool,
    /// How the surface may decorate its text (SPEC 11.4).
    pub color: ColorMode,
    /// Whether the surface redraws only when its frame changed (SPEC
    /// 11.4: the reduced-update mode for terminals and assistive setups
    /// sensitive to redraws).
    pub reduced_updates: bool,
}

/// The finish flow state (SPEC 5.2): the summary of the entire scope, the
/// plan it would apply, and the rows one frame shows of it.
struct FinishPanel {
    summary: FinishSummary,
    rows: Vec<DisplayRow>,
    width: u16,
}

/// The terminal shell.
pub struct Shell<'a> {
    root: PathBuf,
    operations: &'a Operations<'a>,
    view: View,
    focus: Focus,
    layout: WidthClass,
    documents: Vec<String>,
    selection: usize,
    document: Option<DocumentView>,
    source_lines: Vec<String>,
    headings: Vec<wrap::Heading>,
    sidebar_hidden: bool,
    outline_folded: bool,
    navigator_cursor: usize,
    reading_source: bool,
    display: Vec<DisplayRow>,
    display_width: u16,
    offset: usize,
    drift: usize,
    viewport_rows: usize,
    anchor: Option<SourceAnchor>,
    /// The adopted scope descriptor the document list resolved against,
    /// held so the polling watch can name the same input without one Git
    /// query per tick; `None` before adoption.
    descriptor: Option<ScopeDescriptor>,
    workspace: WorkspaceSnapshot,
    changes: Option<ChangesView>,
    history: Option<HistoryView>,
    finish: Option<FinishPanel>,
    /// The outcome of the last confirmed finish, when one ran (SPEC 5.2).
    finish_result: Option<ReviewDecisionResult>,
    /// The outcome of the last confirmed restoration, when one ran
    /// (SPEC 5.5).
    restore_result: Option<RestoreResult>,
    /// The generation of the read-only state (SPEC 13.3): every rebuild
    /// of the shell's state starts one, and every inspection job carries
    /// the generation it was asked for, so a reply that describes a
    /// superseded state is discarded whole.
    generation: u64,
    /// The workspace observation the pinned comparison carries (SPEC
    /// 5.3): what the repository held when the comparison was pinned.
    /// Polling compares against this record; one explicit refresh
    /// re-pins it.
    pinned: Option<PinnedObservation>,
    /// The standing overlay, when one stands (SPEC 11.3).
    overlay: Option<Overlay>,
    workspace_search: WorkspaceSearch,
    search_worker: Option<SearchWorker>,
    palette: super::palette::Palette,
    palette_worker: Option<super::palette::Worker>,
    menu_origin: Focus,
    reduced_updates: bool,
    help_offset: usize,
    saved_offset: usize,
    /// One requested editor handoff the event loop has not run yet (SPEC
    /// 11.3 `e`, 12.5).
    pending_editor: Option<EditorHandoff>,
    /// The configured editor argv of the local settings (SPEC 12.5),
    /// cached at open and at every rebuild so the menu states its reason
    /// without one Git query per frame.
    editor: Option<Vec<String>>,
    /// The configured editor-exit capture policy of the local settings
    /// (SPEC 12.5), cached beside the argv it belongs to: the editor's
    /// return is one capture boundary only while this is on.
    capture_after_edit: bool,
    /// Whether the next frame must be drawn: every state change raises
    /// it, and the reduced-update mode consumes it (SPEC 11.4).
    redraw: bool,
    /// Whether the background slices — the change comparison and the
    /// timeline — still wait for the first drawn frame (SPEC 17.4: the
    /// launch window ends at that frame, so they never delay it).
    background_pending: bool,
    /// How the surface may decorate its text (SPEC 11.4).
    color: ColorMode,
    notice: Option<String>,
    /// Whether one quit warning about unapplied selections stands (SPEC 5.2).
    quit_warning: bool,
    quit: bool,
    session_store: Result<session::Store, String>,
    saved_reviews: Vec<session::Entry>,
    saved_omitted: bool,
    saved_pending: bool,
    session_error: Option<String>,
    writer: Option<session::Writer>,
    draft: Option<Record>,
    decision_pending: Option<(Instant, Instant)>,
    navigation_pending: bool,
    last_save: Instant,
    explicit_path: bool,
    explicit_view: bool,
    repository_identity: String,
    adoption_identity: String,
    anchors: Vec<Anchor>,
    reference: Option<super::reference::Reference>,
    terminal_columns: u16,
}

impl<'a> Shell<'a> {
    pub fn reference(&self) -> Option<&super::reference::Reference> {
        self.reference.as_ref()
    }
    pub fn reference_visible(&self) -> bool {
        self.reference
            .as_ref()
            .is_some_and(|r| r.descriptor.visible)
            && self.finish.is_none()
            && !self.history_preview_stands()
    }
    pub fn reference_only(&self) -> bool {
        self.reference_visible() && self.terminal_columns < 140 && self.focus == Focus::Reference
    }
    pub fn frame_layout(&self, area: ratatui::layout::Rect) -> layout::FrameLayout {
        let presented = self.reference_visible()
            && (area.width >= 140
                || self.focus == Focus::Reference
                || (self.focus == Focus::Actions && self.menu_origin == Focus::Reference));
        let mut frame = layout::frame_layout_with_reference(area, !self.sidebar_hidden, presented);
        let progress = self.progress_rows(area.width);
        if progress > 0 {
            frame.reading.y = frame.reading.y.saturating_add(progress);
            frame.reading.height = frame.reading.height.saturating_sub(progress);
            if let Some(rect) = &mut frame.navigator {
                rect.y = rect.y.saturating_add(progress);
                rect.height = rect.height.saturating_sub(progress);
            }
            if let Some(rect) = &mut frame.reference {
                rect.y = rect.y.saturating_add(progress);
                rect.height = rect.height.saturating_sub(progress);
            }
        }
        frame
    }
    fn back_to_review(&mut self) {
        self.focus = Focus::Reading;
        if self.terminal_columns < 140
            && let Some(r) = &mut self.reference
        {
            r.descriptor.visible = false;
        }
        self.notice = Some("Back to review; reference retained".into());
    }
    fn pin_document(&mut self) {
        let Some(d) = self.document.as_ref() else {
            return;
        };
        if self.view != View::Read || !self.fresh_listed(&d.path) {
            self.notice = Some("Display a safe listed document in Read to pin it".into());
            return;
        }
        let line = self.anchor.as_ref().map_or(1, |a| a.line);
        let result = if d.saved == SavedState::WorkingFileGone {
            super::reference::Reference::historical(
                self.operations,
                &d.path,
                d.candidate.as_deref().unwrap_or("HEAD"),
                line,
            )
        } else {
            super::reference::Reference::snapshot(
                self.operations,
                &d.path,
                d.text.clone(),
                d.saved == SavedState::UnsavedDraft,
                d.truncated,
                line,
                self.reading_source,
            )
        };
        self.replace_reference(result);
    }
    fn pin_version(&mut self) {
        let Some(row) = self.history.as_ref().and_then(|h| h.selected()) else {
            return;
        };
        let Some(d) = &self.document else {
            return;
        };
        if !self.fresh_listed(&d.path) {
            self.notice = Some("Reference is outside the current scope".into());
            return;
        }
        let result =
            super::reference::Reference::historical(self.operations, &d.path, &row.commit, 1);
        self.replace_reference(result);
    }
    fn replace_reference(&mut self, result: Result<super::reference::Reference, String>) {
        match result {
            Ok(r) => {
                let visible = r.descriptor.visible;
                self.reference = Some(r);
                if self.terminal_columns < 140 && visible {
                    self.focus = Focus::Reference;
                } else if !visible && self.focus == Focus::Reference {
                    self.focus = Focus::Reading;
                }
                self.notice = Some("Reference pinned · read-only".into());
            }
            Err(e) => self.notice = Some(e),
        }
    }
    fn refresh_reference(&mut self) {
        let Some(r) = &self.reference else {
            return;
        };
        let d = r.descriptor.clone();
        if !self.fresh_listed(&d.path) {
            self.notice = Some(
                "Reference source unavailable or outside current scope; snapshot retained".into(),
            );
            return;
        }
        let result = if let Some(oid) = &d.revision {
            super::reference::Reference::historical(self.operations, &d.path, oid, d.line)
        } else {
            super::reference::Reference::working(
                self.operations,
                &self.root,
                &d.path,
                d.line,
                self.workspace.uncaptured_paths.contains(&d.path),
            )
        };
        self.replace_reference(result.map(|mut r| {
            r.descriptor.source = d.source;
            r.descriptor.drift = d.drift;
            r.descriptor.visible = d.visible;
            r
        }));
    }

    fn identity(&self) -> Option<session::Identity> {
        let c = self.changes.as_ref()?;
        Some(session::Identity {
            repository: self.repository_identity.clone(),
            branch: c.branch_ref().into(),
            scope: c.scope_id().into(),
            adoption: self.adoption_identity.clone(),
            baseline: c.baseline_oid().into(),
            candidate: c.candidate_oid().into(),
            review: c.review_id().into(),
        })
    }

    fn navigation(&self) -> Navigation {
        let mut anchors = self.anchors.clone();
        if let Some(a) = self.current_anchor() {
            anchors.retain(|p| !(p.path == a.path && p.view == "Read"));
            anchors.push(Anchor {
                path: a.path.clone(),
                view: "Read".into(),
                line: a.line,
                drift: self.drift,
            });
        }
        if anchors.len() > 256 {
            anchors.drain(..anchors.len() - 256);
        }
        Navigation {
            change_position: self.changes.as_ref().and_then(|c| c.source_position()),
            reading_source: self.reading_source,
            view: self.view.label().into(),
            path: self.document.as_ref().map(|d| d.path.clone()),
            selected: self
                .changes
                .as_ref()
                .and_then(|c| c.selected_group())
                .map(|g| g.change_id.clone()),
            source: self.changes.as_ref().is_some_and(|c| c.source_view()),
            anchors,
            history_commit: self
                .history
                .as_ref()
                .and_then(|h| h.selected())
                .map(|r| r.commit.clone()),
        }
    }

    fn list_saved_reviews(&mut self) {
        // A folder without Git has no session directory yet; first use stays quiet.
        if self.session_store.is_err() && !self.workspace.adopted {
            return;
        }
        match self
            .session_store
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|s| s.list())
        {
            Ok((entries, omitted)) => {
                self.saved_reviews = entries;
                self.saved_omitted = omitted;
                // Receipt lookup is a dry run of the exact prepared plan. Never retry application.
                for entry in &mut self.saved_reviews {
                    if let Ok(record) = &mut entry.record
                        && let Some(attempt) = &record.attempt
                    {
                        let proof = self.operations.validate_review(&app::ReviewRequest {
                            scope: None,
                            plan: attempt.plan.clone(),
                            dry_run: true,
                        });
                        if let Some(receipt) =
                            proof.data.as_ref().and_then(|d| d.existing_receipt.clone())
                        {
                            entry.completed = Some(receipt);
                        }
                    }
                }
            }
            Err(error) => self.session_error = Some(error),
        }
    }

    pub fn saved_offset(&self) -> usize {
        self.saved_offset
    }
    pub fn saved_reviews(&self) -> &[session::Entry] {
        &self.saved_reviews
    }
    pub fn saved_reviews_omitted(&self) -> bool {
        self.saved_omitted
    }
    pub fn saved_compatibility(&self, record: &Record) -> String {
        if self.identity().as_ref() != Some(&record.identity)
            || self.workspace.head.as_deref() != Some(record.identity.candidate.as_str())
            || self.workspace.scope_id != record.identity.scope
            || self.workspace.branch.as_deref()
                != record.identity.branch.strip_prefix("refs/heads/")
        {
            "Stale saved review".into()
        } else if let Some(why) = self.changes.as_ref().and_then(|c| c.refusal()) {
            format!("Compatible for inspection · {why}")
        } else {
            "Compatible".into()
        }
    }
    pub fn progress_rows(&self, width: u16) -> u16 {
        self.progress_label().map_or(0, |label| {
            let width = width.max(1) as usize;
            unicode_width::UnicodeWidthStr::width(crate::cli::display_escape(&label).as_str())
                .div_ceil(width)
                .clamp(1, 3) as u16
        })
    }

    pub fn progress_label(&self) -> Option<String> {
        if self.draft.is_some() {
            if let Some(e) = &self.session_error {
                Some(format!("Progress not saved · {e}"))
            } else if self.decision_pending.is_some() || self.navigation_pending {
                Some("Review progress saving locally · not applied".into())
            } else {
                Some("Review progress saved locally · not applied".into())
            }
        } else if let Some(entry) = self.saved_reviews.first() {
            let detail = match &entry.record {
                Ok(r) => format!(
                    " · {} C {} · updated {} UTC ms · {} keep/{} restore · {}",
                    r.identity.branch.trim_start_matches("refs/heads/"),
                    render::short(&r.identity.candidate),
                    r.updated_ms,
                    r.decisions
                        .iter()
                        .filter(|c| c.decision == Decision::Keep)
                        .count(),
                    r.decisions
                        .iter()
                        .filter(|c| c.decision == Decision::Restore)
                        .count(),
                    entry
                        .completed
                        .as_ref()
                        .map_or_else(|| self.saved_compatibility(r), |_| "Completed".into())
                ),
                Err(error) => format!(" · {error}"),
            };
            Some(format!(
                "{} saved review{} · : Resume saved review{detail}{}",
                self.saved_reviews.len(),
                if self.saved_reviews.len() == 1 {
                    ""
                } else {
                    "s"
                },
                if self.saved_omitted {
                    " · more records omitted"
                } else {
                    ""
                }
            ))
        } else if self.saved_omitted {
            Some("Saved review enumeration incomplete · additional records omitted".into())
        } else {
            self.session_error
                .as_ref()
                .map(|e| format!("Saved reviews unavailable · {e}"))
        }
    }

    fn update_progress(&mut self) {
        let Some(changes) = &self.changes else {
            return;
        };
        if self.draft.is_none() && changes.proposed_count() == 0 {
            return;
        }
        let choices = changes.saved_choices();
        let visited = changes.visited_ids();
        let navigation = self.navigation();
        let now = Instant::now();
        let reference = self.reference.as_ref().map(|r| r.descriptor.clone());
        if self.draft.is_none() {
            let Some(identity) = self.identity() else {
                return;
            };
            let owner = self
                .session_store
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|s| s.create());
            let id = owner
                .as_ref()
                .map(|o| o.id.clone())
                .unwrap_or_else(|_| format!("memory-{}", session::now_ms()));
            match owner.and_then(session::Writer::spawn) {
                Ok(writer) => self.writer = Some(writer),
                Err(error) => self.session_error = Some(error),
            }
            self.draft = Some(Record {
                schema_version: 1,
                id,
                updated_ms: session::now_ms(),
                generation: 0,
                identity,
                decisions: vec![],
                visited: vec![],
                navigation: Navigation::default(),
                attempt: None,
                reference: None,
            });
        }
        let draft = self.draft.as_mut().unwrap();
        let changed_choices = draft.decisions != choices || draft.visited != visited;
        let changed_navigation = draft.navigation != navigation || draft.reference != reference;
        if changed_choices || changed_navigation {
            draft.decisions = choices;
            draft.visited = visited;
            draft.navigation = navigation;
            draft.reference = reference;
            draft.generation += 1;
            draft.updated_ms = session::now_ms();
            if changed_choices {
                let first = self.decision_pending.map_or(now, |(first, _)| first);
                self.decision_pending = Some((first, now));
            }
            if changed_navigation {
                self.navigation_pending = true;
            }
        }
    }

    /// Called by the event loop even in reduced-update mode.
    pub fn workspace_work_pending(&self) -> bool {
        self.decision_pending.is_some()
            || self.navigation_pending
            || self.palette.pending.is_some()
            || (!self.reduced_updates && self.palette.edited.is_some())
    }

    pub fn poll_progress(&mut self) {
        let now = Instant::now();
        if let Some(writer) = &self.writer {
            if let Some((generation, result)) = writer.completion()
                && self
                    .draft
                    .as_ref()
                    .is_some_and(|r| r.generation == generation)
            {
                self.decision_pending = None;
                self.navigation_pending = false;
                self.session_error = result.err();
                self.redraw = true;
            }
            let due = self.decision_pending.is_some_and(|(first, last)| {
                now.duration_since(last) >= Duration::from_millis(250)
                    || now.duration_since(first) >= Duration::from_secs(1)
            }) || (self.navigation_pending
                && self.decision_pending.is_none()
                && now.duration_since(self.last_save) >= Duration::from_secs(1));
            if due
                && now.duration_since(self.last_save) >= Duration::from_millis(250)
                && let Some(record) = &self.draft
            {
                writer.submit(record.clone());
                self.last_save = now;
            }
        }
    }

    pub fn flush_progress(&mut self) -> Result<(), String> {
        self.update_progress();
        if self.writer.is_none()
            && let Some(draft) = &mut self.draft
        {
            let result = self
                .session_store
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|s| s.create())
                .and_then(session::Writer::spawn);
            match result {
                Ok(writer) => {
                    draft.id = writer.id().into();
                    self.writer = Some(writer);
                }
                Err(error) => self.session_error = Some(error),
            }
        }
        let result = match (&self.writer, &self.draft) {
            (Some(writer), Some(record)) => writer.flush(record.clone()),
            (None, Some(_)) => Err(self
                .session_error
                .clone()
                .unwrap_or_else(|| "Local review storage unavailable".into())),
            _ => Ok(()),
        };
        match &result {
            Ok(()) => {
                self.last_save = Instant::now();
                self.decision_pending = None;
                self.navigation_pending = false;
                self.session_error = None;
            }
            Err(e) => self.session_error = Some(e.clone()),
        }
        result
    }

    fn retire_draft(&mut self) -> bool {
        if let Err(error) = self.flush_progress() {
            self.notice = Some(format!(
                "Progress not saved: {error}; newer choices retained"
            ));
            return false;
        }
        self.writer.take();
        self.draft = None;
        self.decision_pending = None;
        self.navigation_pending = false;
        self.list_saved_reviews();
        true
    }

    fn discard_saved_review(&mut self, selection: usize) {
        let Some(id) = self
            .saved_reviews
            .get(selection)
            .map(|entry| entry.id.clone())
        else {
            return;
        };
        let own = self.writer.as_ref().is_some_and(|w| w.id() == id);
        let result = if own {
            self.writer.take().unwrap().remove()
        } else {
            self.session_store
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|store| store.acquire(&id))
                .and_then(|owner| owner.remove())
        };
        match result {
            Ok(()) => {
                if own {
                    self.draft = None;
                    self.decision_pending = None;
                    self.navigation_pending = false;
                    self.load_background();
                }
                self.list_saved_reviews();
                self.close_overlay("Saved review discarded; no Git state changed");
            }
            Err(e) => self.notice = Some(e),
        }
    }

    fn resume_review(&mut self, selection: usize) {
        if self.draft.is_some() {
            self.notice =
                Some("Start a separate draft or finish this review before resuming another".into());
            return;
        }
        let result =
            (|| -> Result<(Record, session::Owner, ChangesView, WorkspaceInspection), String> {
                let entry = self
                    .saved_reviews
                    .get(selection)
                    .ok_or("No saved review selected")?;
                if let Some(receipt) = &entry.completed {
                    return Err(format!(
                        "Completed review: matching Git receipt {receipt}; inspect or discard its local record"
                    ));
                }
                entry.record.as_ref().map_err(Clone::clone)?;
                let store = self.session_store.as_ref().map_err(Clone::clone)?;
                let owner = store.acquire(&entry.id)?;
                let record = store.read(&entry.id)?;
                // Load all details and fresh guards, even when a cached comparison matches.
                let changes = ChangesView::load(self.operations).ok_or("Comparison unavailable")?;
                let inspection = self
                    .operations
                    .inspect_workspace(&InspectRequest::default())
                    .data
                    .ok_or("Workspace inspection unavailable")?;
                if inspection.head.as_deref() != Some(changes.candidate_oid())
                    || inspection.scope.digest != changes.scope_id()
                    || inspection.branch.as_deref() != Some(changes.branch_ref())
                {
                    return Err("Stale saved review: the workspace changed while loading".into());
                }
                let adoption = self
                    .operations
                    .review_start_marker(changes.candidate_oid(), changes.scope_id())
                    .map_err(|e| e.message)?;
                let identity = session::Identity {
                    repository: self.repository_identity.clone(),
                    branch: changes.branch_ref().into(),
                    scope: changes.scope_id().into(),
                    adoption,
                    baseline: changes.baseline_oid().into(),
                    candidate: changes.candidate_oid().into(),
                    review: changes.review_id().into(),
                };
                if identity != record.identity {
                    return Err(
                        "Stale saved review; inspect saved choices or start a fresh review with n"
                            .into(),
                    );
                }
                Ok((record, owner, changes, inspection))
            })();
        match result {
            Err(error) => self.notice = Some(error),
            Ok((record, owner, mut changes, inspection)) => {
                if let Err(error) = changes.load_choices(&record) {
                    self.notice = Some(error);
                    return;
                }
                let writer = match session::Writer::spawn(owner) {
                    Ok(w) => w,
                    Err(e) => {
                        self.notice = Some(e);
                        return;
                    }
                };
                let mut navigation_notice = String::new();
                if record
                    .navigation
                    .selected
                    .as_ref()
                    .is_some_and(|id| !changes.groups().iter().any(|g| &g.change_id == id))
                {
                    navigation_notice.push_str(" Selected change missing; first group used.");
                }
                self.absorb_inspection(inspection);
                self.pinned = Some(PinnedObservation::of(&self.workspace));
                self.changes = Some(changes);
                self.finish = None;
                self.writer = Some(writer);
                if !self.explicit_view {
                    self.view = match record.navigation.view.as_str() {
                        "Changes" => View::Changes,
                        "History" => View::History,
                        _ => View::Read,
                    };
                }
                if !self.explicit_path
                    && let Some(path) = &record.navigation.path
                {
                    if let Some(index) = self.documents.iter().position(|p| p == path) {
                        self.selection = index;
                        self.load_document(path);
                    } else {
                        navigation_notice
                            .push_str(" Saved document missing; first listed document retained.");
                    }
                }
                if !self.explicit_view {
                    self.reading_source = record.navigation.reading_source;
                }
                self.anchors = record.navigation.anchors.clone();
                if !self.explicit_path
                    && let Some(a) = self.anchors.iter().find(|a| {
                        a.view == "Read" && self.document.as_ref().is_some_and(|d| d.path == a.path)
                    })
                {
                    let line = a.line.clamp(1, self.source_lines.len().max(1) as u32);
                    if line != a.line {
                        navigation_notice
                            .push_str(" Saved source position moved; nearest valid line used.");
                    }
                    self.anchor = Some(SourceAnchor {
                        path: a.path.clone(),
                        line,
                    });
                    self.drift = a.drift;
                }
                if let Some(position) = &record.navigation.change_position
                    && let Some(c) = &mut self.changes
                {
                    c.prepare(self.display_width.max(1), self.viewport_rows);
                    c.restore_position(position);
                }
                if let Some(commit) = &record.navigation.history_commit
                    && let Some(history) = &mut self.history
                {
                    history.select_commit(commit);
                }
                self.reference = record.reference.clone().map(|d| {
                    let listed = self.fresh_listed(&d.path);
                    super::reference::Reference::resume(self.operations, &self.root, d, listed)
                });
                self.draft = Some(record);
                self.overlay = None;
                self.focus = Focus::Reading;
                self.notice = Some(format!(
                    "Saved choices resumed · not applied; open a fresh finish summary to confirm{navigation_notice}"
                ));
                self.session_error = None;
                self.observe_arrivals();
            }
        }
    }

    /// Loads the shell for the folder at `root` through the shared
    /// operations.
    ///
    /// Loading is read-only and total: a query that fails degrades to a
    /// note, never to a refusal to open, because the surface must open
    /// read-only before adoption and stay readable in every state the
    /// repository can be in. The comparison and the timeline load inside
    /// this call; [`Shell::open_for_launch`] is the interactive path that
    /// defers them.
    pub fn open(root: &Path, operations: &'a Operations<'a>, options: &TuiOptions) -> Shell<'a> {
        Self::open_common(root, operations, options, true)
    }

    /// Loads the shell the interactive launch path uses: the slices the
    /// first useful frame does not show — the change comparison and the
    /// timeline — wait for that frame instead of delaying it (SPEC 17.4:
    /// the launch window ends at the first useful frame).
    pub fn open_for_launch(
        root: &Path,
        operations: &'a Operations<'a>,
        options: &TuiOptions,
    ) -> Shell<'a> {
        Self::open_common(root, operations, options, false)
    }

    /// The shared constructor; `load_background_now` decides whether the
    /// background slices load inside the call or arm the deferred load.
    fn open_common(
        root: &Path,
        operations: &'a Operations<'a>,
        options: &TuiOptions,
        load_background_now: bool,
    ) -> Shell<'a> {
        let folder = folder_name(root);
        let mut shell = Shell {
            root: root.to_path_buf(),
            operations,
            view: options.view,
            focus: Focus::Reading,
            layout: WidthClass::List,
            documents: Vec::new(),
            selection: 0,
            document: None,
            source_lines: Vec::new(),
            headings: Vec::new(),
            sidebar_hidden: false,
            outline_folded: false,
            navigator_cursor: 0,
            reading_source: false,
            display: Vec::new(),
            display_width: 0,
            offset: 0,
            drift: 0,
            viewport_rows: 0,
            anchor: None,
            descriptor: None,
            workspace: WorkspaceSnapshot::unadopted(&folder, None),
            changes: None,
            history: None,
            finish: None,
            finish_result: None,
            restore_result: None,
            generation: 0,
            pinned: None,
            overlay: None,
            workspace_search: WorkspaceSearch::default(),
            search_worker: None,
            palette: super::palette::Palette::default(),
            palette_worker: None,
            menu_origin: Focus::Reading,
            reduced_updates: options.reduced_updates,
            help_offset: 0,
            saved_offset: 0,
            pending_editor: None,
            editor: None,
            capture_after_edit: false,
            redraw: true,
            background_pending: false,
            color: options.color,
            notice: None,
            quit_warning: false,
            quit: false,
            session_store: session::Store::from_git(operations.git()),
            saved_reviews: Vec::new(),
            saved_omitted: false,
            saved_pending: false,
            session_error: None,
            writer: None,
            draft: None,
            decision_pending: None,
            navigation_pending: false,
            last_save: Instant::now() - Duration::from_secs(1),
            explicit_path: options.path.is_some(),
            explicit_view: options.explicit_view || options.view != View::Read,
            repository_identity: operations
                .git()
                .toplevel()
                .unwrap_or_else(|_| root.to_path_buf())
                .canonicalize()
                .unwrap_or_else(|_| root.to_path_buf())
                .to_string_lossy()
                .into_owned(),
            adoption_identity: String::new(),
            anchors: Vec::new(),
            reference: None,
            terminal_columns: 120,
        };
        let settings = app::read_local_settings(shell.operations.git()).ok();
        shell.editor = settings
            .as_ref()
            .and_then(|settings| settings.editor_argv.clone());
        shell.capture_after_edit = settings
            .as_ref()
            .is_some_and(|settings| settings.capture_after_edit);
        match settings.and_then(|settings| settings.scope) {
            Some(descriptor) => {
                shell.load_visible(descriptor);
                // The comparison and the timeline are not on the first
                // useful frame: an opening view that needs them loads
                // them now, and the deferred reading surface waits for
                // that frame to be drawn (SPEC 17.4).
                if load_background_now || shell.view != View::Read {
                    shell.load_background();
                } else {
                    shell.background_pending = true;
                }
            }
            None => shell.load_unadopted(&folder),
        }
        shell.select_initial(options.path.as_deref());
        if load_background_now {
            shell.list_saved_reviews();
        } else {
            shell.saved_pending = true;
        }
        shell
    }

    /// Loads an adopted workspace: status, documents, comparison summary,
    /// and timeline, each through its shared operation. Every rebuild
    /// after a mutation or an explicit refresh goes through here
    /// synchronously; only the launch path splits the two halves so the
    /// first useful frame need not wait for the comparison.
    fn load_adopted(&mut self, descriptor: ScopeDescriptor) {
        self.load_visible(descriptor);
        self.load_background();
    }

    /// Loads the slices the first useful frame shows: the workspace
    /// identity and the document list (SPEC 17.4: the launch window ends
    /// at that frame, so nothing it does not show may delay it).
    fn load_visible(&mut self, descriptor: ScopeDescriptor) {
        self.regenerate();
        self.descriptor = Some(descriptor.clone());
        self.workspace.adopted = true;
        let inspection = self.operations.inspect_workspace(&InspectRequest {
            scope: None,
            path_filter: Vec::new(),
        });
        if inspection.ok() {
            self.absorb_inspection(inspection.data.expect("an ok outcome carries data"));
        } else {
            // The inspection failed; keep the shell readable and say why.
            // The candidate identity still comes from the adapter, so the
            // saved state of a document stays comparable.
            let error = inspection
                .error
                .expect("a failed outcome carries its error");
            self.workspace.note = Some(bounded(&error.message));
            if let Ok(Some(head)) = self.operations.git().head_commit() {
                self.workspace.head = Some(head);
            }
            if let Ok(Some(full)) = self.operations.git().head_symbolic_ref() {
                self.workspace.branch = Some(
                    full.strip_prefix("refs/heads/")
                        .unwrap_or(&full)
                        .to_string(),
                );
            }
        }
        self.load_documents(&descriptor);
    }

    /// Loads the slices the first useful frame does not show: the change
    /// comparison and the timeline page, with the observation that pins
    /// them against the workspace the visible half just read (SPEC 5.3).
    /// The launch path calls this after the first drawn frame; every
    /// rebuild calls it inside [`Shell::load_adopted`].
    fn load_background(&mut self) {
        if self.draft.is_some() && !self.retire_draft() {
            return;
        }
        let previous_notes = self.changes.as_ref().and_then(|changes| {
            self.operations
                .list_notes(Some(changes.candidate_oid()))
                .data
        });
        self.changes = ChangesView::load(self.operations);
        self.adoption_identity.clear();
        if let Some(changes) = &mut self.changes {
            match self
                .operations
                .review_start_marker(changes.candidate_oid(), changes.scope_id())
            {
                Ok(start) => self.adoption_identity = start,
                Err(error) => {
                    changes.observe_arrival(&format!(
                        "Review identity unavailable: {}",
                        error.message
                    ));
                }
            }
        }
        if let (Some(changes), Some(notes)) = (&mut self.changes, previous_notes) {
            changes.retire_notes(notes);
        }
        if self.view == View::Changes {
            // The opening view is the Changes view: the first group the
            // writer sees is visited, and the pinned candidate is checked
            // against the head the load just used.
            if let Some(changes) = &mut self.changes {
                changes.visit_selection();
            }
            self.observe_changes_head();
        }
        self.reload_history();
        // The comparison and the workspace it was pinned over were both
        // read inside this one synchronous load, so every observation a
        // later poll makes is compared against this record (SPEC 5.3).
        self.pinned = Some(PinnedObservation::of(&self.workspace));
    }

    /// Loads the deferred background slices once, after the first drawn
    /// frame. Returns whether anything loaded, so the loop draws again
    /// before it waits for input.
    pub fn load_pending_background(&mut self) -> bool {
        let listed = self.saved_pending;
        if self.saved_pending {
            self.saved_pending = false;
            self.list_saved_reviews();
            self.redraw = true;
        }
        if !self.background_pending {
            return listed;
        }
        self.background_pending = false;
        self.load_background();
        self.redraw = true;
        true
    }

    /// Starts the next generation of the read-only state (SPEC 13.3).
    /// Inspection jobs asked for by an earlier generation answer to a
    /// state the shell no longer holds, so their replies are discarded.
    fn regenerate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.invalidate_workspace_search();
        self.invalidate_palette();
    }

    fn invalidate_palette(&mut self) {
        self.palette.metadata = None;
        if let Some(w) = &self.palette_worker {
            w.cancel();
        }
        self.palette.pending = None;
        if matches!(self.overlay, Some(Overlay::Menu { .. })) {
            self.palette.edited = Some(Instant::now());
        }
    }

    /// Rereads the local settings (SPEC 8.2): the adopted scope the next
    /// rebuild loads, and the configured editor argv and capture policy
    /// the `e` key, the action menu, and the editor's return state.
    fn reload_settings(&mut self) -> Option<ScopeDescriptor> {
        let settings = app::read_local_settings(self.operations.git()).ok();
        self.editor = settings
            .as_ref()
            .and_then(|settings| settings.editor_argv.clone());
        self.capture_after_edit = settings
            .as_ref()
            .is_some_and(|settings| settings.capture_after_edit);
        settings.and_then(|settings| settings.scope)
    }

    /// Reloads the first page of the editorial timeline through the shared
    /// `log` operation, keeping the selected revision readable across the
    /// reload (SPEC 5.4): the same commit stays selected when the page
    /// still holds it.
    fn reload_history(&mut self) {
        let pinned = self
            .history
            .as_ref()
            .and_then(HistoryView::selected)
            .map(|row| row.commit.clone());
        self.history = match &self.document {
            Some(document) => HistoryView::load_document(self.operations, &document.path),
            None => HistoryView::load(self.operations),
        };
        if let (Some(commit), Some(history)) = (pinned, &mut self.history) {
            history.select_commit(&commit);
        }
    }

    /// Absorbs one successful workspace inspection.
    fn absorb_inspection(&mut self, data: WorkspaceInspection) {
        let branch = data
            .branch
            .as_deref()
            .map(|full| full.strip_prefix("refs/heads/").unwrap_or(full).to_string());
        self.workspace = WorkspaceSnapshot {
            adopted: true,
            folder: self.workspace.folder.clone(),
            scope_roots: data.scope.descriptor.include.join(" "),
            scope_id: data.scope.digest,
            branch,
            head: data.head,
            preservation: data.preservation,
            review: review_state_of(&data.baseline, data.review),
            pending_files: data.counts.pending_files,
            uncaptured: data.counts.uncaptured.as_ref().map(|paths| paths.total),
            uncaptured_paths: data
                .counts
                .uncaptured
                .map(|paths| {
                    paths
                        .paths
                        .into_iter()
                        .map(|path| path.to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default(),
            documents: self.workspace.documents,
            blockers: data.policy_blockers,
            operation: data.operation,
            note: None,
        };
    }

    /// Lists the scoped documents through the one scope resolver.
    fn load_documents(&mut self, descriptor: &ScopeDescriptor) {
        let input: ScopeInput = descriptor.to_scope_input();
        let Ok(resolved) = app::resolve_scope(self.operations.git(), &input) else {
            return;
        };
        self.documents = document_paths(&resolved);
        // BTreeMap iteration is already sorted by path bytes.
        self.workspace.documents = self.documents.len();
    }

    /// Loads an unadopted folder: the discovered documents of first use
    /// (SPEC 5.1).
    fn load_unadopted(&mut self, folder: &str) {
        self.regenerate();
        self.descriptor = None;
        self.documents = discover_documents(&self.root);
        let note = self.workspace.note.take();
        self.workspace = WorkspaceSnapshot::unadopted(folder, note);
        self.workspace.documents = self.documents.len();
        self.changes = None;
        self.pinned = None;
    }

    /// Picks the initial document: the requested path, else the first
    /// listed document.
    fn select_initial(&mut self, requested: Option<&str>) {
        let listed = requested.and_then(|path| self.documents.iter().position(|name| name == path));
        match listed {
            Some(index) => {
                self.selection = index;
                let path = self.documents[index].clone();
                self.load_document(&path);
            }
            None => match requested {
                Some(path) => self.load_document(path),
                None => {
                    if let Some(path) = self.documents.first().cloned() {
                        self.load_document(&path);
                    }
                }
            },
        }
    }

    /// Reads one document and its saved state.
    ///
    /// The candidate text comes from the shared show operation at `HEAD`;
    /// the working bytes come from one bounded read of the working file.
    /// The comparison of the two decides the label, so the label is about
    /// the bytes, never a guess.
    fn load_document(&mut self, path: &str) {
        self.offset = 0;
        self.drift = 0;
        let shown = self.show_candidate(path);
        let working = read_working(&self.root, path, READ_VIEW_BYTES);
        let (document, notice) = document_view(path, &working, shown.as_ref());
        if let Some(text) = notice {
            self.notice = Some(text);
        }
        self.absorb_document(document);
        if self.history.is_some() {
            self.reload_history();
        }
    }

    /// Reads the candidate view of one path at `HEAD`; `None` when the
    /// query failed or no candidate exists. The operation resolves `HEAD`
    /// once, so a moving branch cannot change the bytes under the reader.
    fn show_candidate(&self, path: &str) -> Option<ShowOutcome> {
        if !self.workspace.adopted || self.workspace.head.is_none() {
            return None;
        }
        let outcome = self.operations.show_document(&ShowRequest {
            path: path.to_string(),
            revision: "HEAD".to_string(),
            max_bytes: Some(READ_VIEW_BYTES),
            max_lines: None,
            raw: false,
        });
        match outcome.ok() {
            true => outcome.data,
            false => None,
        }
    }

    /// Stores one loaded document and resets the derived display state.
    fn absorb_document(&mut self, document: DocumentView) {
        self.anchors = self.navigation().anchors;
        self.invalidate_palette();
        self.headings = wrap::document_headings(&document.text);
        self.source_lines = document.text.lines().map(str::to_string).collect();
        self.display = Vec::new();
        self.display_width = 0;
        self.anchor = Some(SourceAnchor {
            path: document.path.clone(),
            line: 1,
        });
        self.document = Some(document);
        self.navigator_cursor = self.selection;
    }

    /// Recomputes the wrapped rows for one frame size; the caller renders
    /// afterwards. A changed width restores the reading position from the
    /// anchor plus the best-effort drift (SPEC 11.2).
    pub fn prepare_layout(&mut self, columns: u16, rows: u16) {
        self.terminal_columns = columns;
        self.layout = layout::classify(columns, rows);
        let geometry = self.frame_layout(ratatui::layout::Rect::new(0, 0, columns, rows));
        if self.reference_visible()
            && let Some(reference) = &mut self.reference
        {
            reference.prepare(geometry.reference.unwrap_or(geometry.reading).width);
        }
        if geometry.navigator.is_none() && self.focus == Focus::Documents {
            self.focus = Focus::Reading;
        }
        self.viewport_rows = geometry.reading.height as usize;
        let width = geometry.reading.width;
        match self.view {
            View::Read => self.prepare_reading(width),
            // The Changes column keeps its own viewport, one row smaller
            // than the reading area because the view states the review
            // sentence above it. An open finish summary replaces the
            // column for as long as it stands (SPEC 5.2).
            View::Changes => match (&mut self.finish, &mut self.changes) {
                (Some(panel), _) => {
                    if width != panel.width || panel.rows.is_empty() {
                        panel.rows = wrap::wrap_rows(&panel.summary.lines(), width);
                        panel.width = width;
                    }
                }
                (None, Some(changes)) => {
                    changes.prepare(width, self.viewport_rows.saturating_sub(1));
                }
                (None, None) => {}
            },
            View::History => {
                if let Some(history) = &mut self.history {
                    history.prepare(width, self.viewport_rows.saturating_sub(2));
                }
            }
        }
    }

    /// Rewraps the Read view at `width` and restores its position from the
    /// anchor plus the best-effort drift (SPEC 11.2).
    fn prepare_reading(&mut self, width: u16) {
        if width != self.display_width || self.display.is_empty() {
            let text = self
                .document
                .as_ref()
                .map(|document| document.text.clone())
                .unwrap_or_default();
            let anchor = self.current_anchor().map(|anchor| anchor.line).unwrap_or(1);
            self.display = wrap::reading_document(&text, width, self.reading_source);
            self.display_width = width;
            let anchored = wrap::row_for_anchor(&self.display, anchor);
            let span = wrap::rows_of_source(&self.display, anchored, anchor);
            self.offset = anchored + self.drift.min(span.saturating_sub(1));
        }
        self.clamp_offset();
        self.sync_anchor();
    }

    /// Clamps the display offset to what the rows and viewport allow.
    fn clamp_offset(&mut self) {
        let last = self.display.len().saturating_sub(1);
        self.offset = self.offset.min(last);
    }

    /// Records the anchor and the drift of the current reading position.
    fn sync_anchor(&mut self) {
        if let (Some(document), Some(row)) = (&self.document, self.display.get(self.offset)) {
            let anchored = wrap::row_for_anchor(&self.display, row.source_line);
            self.drift = self.offset.saturating_sub(anchored);
            self.anchor = Some(SourceAnchor {
                path: document.path.clone(),
                line: row.source_line,
            });
        }
    }

    /// The anchor the reading position rests on.
    pub fn current_anchor(&self) -> Option<&SourceAnchor> {
        self.anchor.as_ref()
    }

    /// The last heading at or above the reading position, as the section
    /// part of the breadcrumb (SPEC 11.1).
    pub fn current_section(&self) -> Option<String> {
        let line = self.anchor.as_ref().map(|anchor| anchor.line)?;
        self.headings
            .iter()
            .rev()
            .find(|h| h.line <= line)
            .map(|h| h.title.clone())
    }

    /// Answers one terminal event. Navigation changes only what the
    /// interface shows; the only actions that write are the confirmed
    /// finish of a review (SPEC 5.2) and the confirmed restoration of one
    /// document (SPEC 5.5), both through their shared operations.
    ///
    /// The event also feeds the redraw signal of the reduced-update mode
    /// (SPEC 11.4): one fingerprint of the frame-visible state is taken
    /// before and after, and any difference raises it. A resize raises it
    /// unconditionally, because the rows rewrap when the next frame
    /// prepares them.
    pub fn handle_event(&mut self, event: &Event) {
        let before = self.fingerprint();
        match event {
            // A resize redraws on the next pass at the new size; it resets
            // no plan and mutates no content (SPEC 11.2).
            Event::Resize(_, _) => self.redraw = true,
            // Bracketed paste is literal text in a text field and cannot
            // trigger a sequence of global actions; outside text entry it
            // is ignored with one concise notice (SPEC 11.3).
            Event::Paste(text) => self.handle_paste(text),
            Event::Mouse(mouse)
                if self.overlay.is_none()
                    && self.finish.is_none()
                    && !self.history_preview_stands() =>
            {
                match mouse.kind {
                    crossterm::event::MouseEventKind::ScrollUp => self.step(-3),
                    crossterm::event::MouseEventKind::ScrollDown => self.step(3),
                    _ => {}
                }
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key),
            _ => {}
        }
        self.update_progress();
        if self.fingerprint() != before {
            self.redraw = true;
        }
    }

    /// Absorbs one bracketed paste (SPEC 11.3).
    fn handle_paste(&mut self, pasted_text: &str) {
        let text_entry = self.overlay.as_ref().is_some_and(Overlay::is_text_entry);
        if !text_entry {
            self.notice = Some("pasted text is ignored outside text entry".to_string());
            return;
        }
        if matches!(self.overlay, Some(Overlay::Menu { .. })) {
            let remaining =
                super::palette::QUERY_LIMIT.saturating_sub(self.palette.query.chars().count());
            self.palette
                .query
                .extend(pasted_text.chars().take(remaining));
            self.edit_palette();
            return;
        }
        let pasted: String = pasted_text.chars().take(FIELD_CHARS).collect();
        match self.overlay.as_mut() {
            Some(Overlay::Picker { filter }) => filter.push_str(&pasted),
            Some(Overlay::Search { query }) => query.push_str(&pasted),
            Some(Overlay::WorkspaceSearch) => {
                let remaining =
                    FIELD_CHARS.saturating_sub(self.workspace_search.query.chars().count());
                self.workspace_search
                    .query
                    .extend(pasted.chars().take(remaining));
                self.invalidate_workspace_search();
            }
            Some(Overlay::Note { text, .. }) => {
                let remaining = crate::notes::MAX_TEXT_CHARS.saturating_sub(text.chars().count());
                text.extend(pasted_text.chars().take(remaining));
            }
            _ => {}
        }
    }

    /// Answers one key press.
    fn handle_key(&mut self, key: &KeyEvent) {
        self.notice = None;
        let quit_warning_stood = self.quit_warning;
        self.quit_warning = false;
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.finish.is_some() {
            // The finish summary is one local context (SPEC 11.3): it
            // answers its labeled action, its cancel key, and quit. No
            // other key reaches the plan or the column beneath, and none
            // of the three applies anything on its own.
            match (key.code, control) {
                (KeyCode::Enter, false) => self.apply_finish(),
                (KeyCode::Esc, false) => self.cancel_finish(),
                (KeyCode::Char('c'), true) | (KeyCode::Char('q'), false) => {
                    self.request_quit(quit_warning_stood)
                }
                _ => {
                    self.notice = Some(
                        "Enter keeps the remaining changes and finishes; Esc cancels".to_string(),
                    )
                }
            }
        } else if self.view == View::History && self.history_preview_stands() {
            // The restoration preview is one local context too (SPEC 11.3):
            // it answers its labeled action, its cancel key, its scroll
            // keys, and quit. No key reaches the timeline beneath it, and
            // none of them restores on its own.
            self.handle_history_preview_key(key, control, quit_warning_stood);
        } else if self.overlay.is_some() {
            self.handle_overlay_key(key, control, quit_warning_stood);
        } else {
            self.handle_column_key(key, control, quit_warning_stood);
        }
        if self.view == View::Changes {
            self.observe_changes_head();
        }
    }

    /// Answers one key press of a standing overlay (SPEC 11.3): the
    /// action menu answers its chooser keys, and the two text fields take
    /// precedence over every command key, because in text entry ordinary
    /// characters such as `q`, `r`, `s`, and `:` are text.
    fn handle_overlay_key(&mut self, key: &KeyEvent, control: bool, quit_warning_stood: bool) {
        let Some(overlay) = self.overlay.clone() else {
            return;
        };
        match overlay {
            Overlay::SavedReviews {
                selection,
                inspect,
                confirm,
            } => match (key.code, control) {
                (KeyCode::PageDown, false) => {
                    self.saved_offset = self
                        .saved_offset
                        .saturating_add(self.viewport_rows.saturating_sub(3).max(1))
                        .min(200_000)
                }
                (KeyCode::PageUp, false) => {
                    self.saved_offset = self
                        .saved_offset
                        .saturating_sub(self.viewport_rows.saturating_sub(3).max(1))
                }
                (KeyCode::Char('c'), true) => self.request_quit(quit_warning_stood),
                (KeyCode::Esc, false) => self.close_overlay("Saved reviews left unchanged"),
                (KeyCode::Down, false) => {
                    self.overlay = Some(Overlay::SavedReviews {
                        selection: (selection + 1).min(self.saved_reviews.len().saturating_sub(1)),
                        inspect: false,
                        confirm: false,
                    })
                }
                (KeyCode::Up, false) => {
                    self.overlay = Some(Overlay::SavedReviews {
                        selection: selection.saturating_sub(1),
                        inspect: false,
                        confirm: false,
                    })
                }
                (KeyCode::Char('i'), false) => {
                    self.overlay = Some(Overlay::SavedReviews {
                        selection,
                        inspect: !inspect,
                        confirm: false,
                    })
                }
                (KeyCode::Char('d'), false) => {
                    self.overlay = Some(Overlay::SavedReviews {
                        selection,
                        inspect: true,
                        confirm: true,
                    })
                }
                (KeyCode::Char('n'), false) => {
                    self.refresh();
                    if self.draft.is_none() {
                        self.close_overlay("Fresh review; no saved choices migrated");
                    }
                }
                (KeyCode::Enter, false) if confirm => self.discard_saved_review(selection),
                (KeyCode::Enter, false) => self.resume_review(selection),
                _ => {}
            },
            Overlay::WorkspaceSearch => match (key.code, control) {
                (KeyCode::Esc, false) => {
                    self.invalidate_workspace_search();
                    self.overlay = None;
                }
                (KeyCode::Up, false) => {
                    self.workspace_search.selection =
                        self.workspace_search.selection.saturating_sub(1);
                }
                (KeyCode::Down, false) => {
                    let last = self
                        .workspace_search
                        .results
                        .as_ref()
                        .map_or(0, |r| r.hits.len().saturating_sub(1));
                    self.workspace_search.selection =
                        (self.workspace_search.selection + 1).min(last);
                }
                (KeyCode::PageUp, false) => {
                    self.workspace_search.selection = self
                        .workspace_search
                        .selection
                        .saturating_sub((self.viewport_rows / 2).max(1));
                }
                (KeyCode::PageDown, false) => {
                    let last = self
                        .workspace_search
                        .results
                        .as_ref()
                        .map_or(0, |r| r.hits.len().saturating_sub(1));
                    self.workspace_search.selection = (self.workspace_search.selection
                        + (self.viewport_rows / 2).max(1))
                    .min(last);
                }
                (KeyCode::Enter, false) => self.submit_workspace_search(),
                (KeyCode::Char('f'), true) => self.start_workspace_search(),
                _ => {
                    let mut query = self.workspace_search.query.clone();
                    self.handle_field_key(&mut query, key, control, quit_warning_stood);
                    if query != self.workspace_search.query {
                        self.workspace_search.query = query;
                        self.invalidate_workspace_search();
                    }
                }
            },
            Overlay::Menu { selection } => {
                match (key.code, control) {
                    (KeyCode::Char('c'), true) => self.request_quit(quit_warning_stood),
                    (KeyCode::Esc, false) => {
                        self.close_overlay("Palette closed; nothing ran");
                    }
                    (KeyCode::Up, false) | (KeyCode::PageUp, false) => {
                        let step = if key.code == KeyCode::PageUp {
                            self.viewport_rows.saturating_sub(3).max(1)
                        } else {
                            1
                        };
                        self.overlay = Some(Overlay::Menu {
                            selection: selection.saturating_sub(step),
                        });
                    }
                    (KeyCode::Down, false) | (KeyCode::PageDown, false) => {
                        let step = if key.code == KeyCode::PageDown {
                            self.viewport_rows.saturating_sub(3).max(1)
                        } else {
                            1
                        };
                        self.overlay = Some(Overlay::Menu {
                            selection: (selection + step)
                                .min(self.palette.hits.len().saturating_sub(1)),
                        });
                    }
                    (KeyCode::Enter, false) => self.submit_palette(selection),
                    (KeyCode::Backspace, false) => {
                        self.palette.query.pop();
                        self.edit_palette();
                    }
                    (KeyCode::Char(c), false)
                        if !c.is_control()
                            && self.palette.query.chars().count() < super::palette::QUERY_LIMIT =>
                    {
                        self.palette.query.push(c);
                        self.edit_palette();
                    }
                    // Modal focus is contained; Tab never exposes background decision keys.
                    _ => {}
                }
            }
            Overlay::Help => match (key.code, control) {
                (KeyCode::Down, false) | (KeyCode::Char('j'), false) => {
                    self.help_offset = (self.help_offset + 1).min(80)
                }
                (KeyCode::Up, false) | (KeyCode::Char('k'), false) => {
                    self.help_offset = self.help_offset.saturating_sub(1)
                }
                (KeyCode::PageDown, false) => {
                    self.help_offset =
                        (self.help_offset + self.viewport_rows.saturating_sub(2)).min(80)
                }
                (KeyCode::PageUp, false) => {
                    self.help_offset = self
                        .help_offset
                        .saturating_sub(self.viewport_rows.saturating_sub(2))
                }
                (KeyCode::Char('c'), true) | (KeyCode::Char('q'), false) => {
                    self.overlay = Some(Overlay::Help);
                    self.request_quit(quit_warning_stood);
                }
                (KeyCode::Esc, false) | (KeyCode::Char('?'), false) => {
                    self.close_overlay("help closed");
                }
                _ => {
                    self.overlay = Some(Overlay::Help);
                    self.notice = Some("Esc closes help".to_string());
                }
            },
            Overlay::Picker { mut filter } => {
                if self.handle_field_key(&mut filter, key, control, quit_warning_stood) {
                    self.overlay = Some(Overlay::Picker { filter });
                } else {
                    // Enter and Esc end the field.
                    self.overlay = None;
                    match key.code {
                        KeyCode::Enter => self.submit_picker(&filter),
                        _ => {
                            self.notice = Some("the document picker closed".to_string());
                        }
                    }
                }
            }
            Overlay::Search { mut query } => {
                if self.handle_field_key(&mut query, key, control, quit_warning_stood) {
                    self.overlay = Some(Overlay::Search { query });
                } else {
                    // Enter and Esc end the field.
                    self.overlay = None;
                    match key.code {
                        KeyCode::Enter => self.submit_search(&query),
                        _ => {
                            self.notice = Some("search closed; nothing moved".to_string());
                        }
                    }
                }
            }
            Overlay::Note {
                commit,
                group,
                mut text,
            } => {
                // This field has the envelope's 2000-character bound rather
                // than the shorter search/picker bound.
                let keep = match (key.code, control) {
                    (KeyCode::Char(c), false) if !c.is_control() => {
                        if text.chars().count() < crate::notes::MAX_TEXT_CHARS {
                            text.push(c);
                        }
                        true
                    }
                    _ => self.handle_field_key(&mut text, key, control, quit_warning_stood),
                };
                if keep {
                    self.overlay = Some(Overlay::Note {
                        commit,
                        group,
                        text,
                    });
                } else if key.code == KeyCode::Enter {
                    let result = self.operations.append_note(&crate::notes::AppendRequest {
                        commit: Some(commit.clone()),
                        group: Some(group.clone()),
                        by: crate::notes::Author::Writer,
                        text: text.clone(),
                    });
                    if let Some(error) = result.error {
                        self.notice = Some(bounded(&error.message));
                        self.overlay = Some(Overlay::Note {
                            commit,
                            group,
                            text,
                        });
                    } else {
                        self.notice =
                            Some("Your note saved; review decisions unchanged".to_string());
                        if let Some(changes) = &mut self.changes
                            && let Some(notes) = self
                                .operations
                                .list_notes(Some(changes.candidate_oid()))
                                .data
                        {
                            changes.update_notes(notes);
                        }
                        self.reload_history();
                    }
                } else {
                    self.notice = Some("Note cancelled; nothing saved".to_string());
                }
            }
        }
    }

    /// Answers one key press of a text-entry field (SPEC 11.3). Ordinary
    /// characters join the text, `Enter` submits the field, `Esc` leaves
    /// it, and `Ctrl-C` keeps its explicit clean-quit behavior. Returns
    /// whether the field still stands.
    fn handle_field_key(
        &mut self,
        text: &mut String,
        key: &KeyEvent,
        control: bool,
        quit_warning_stood: bool,
    ) -> bool {
        match (key.code, control) {
            (KeyCode::Char('c'), true) => {
                self.request_quit(quit_warning_stood);
                true
            }
            (KeyCode::Char(c), false) if !c.is_control() => {
                if text.chars().count() < FIELD_CHARS {
                    text.push(c);
                }
                true
            }
            (KeyCode::Backspace, false) => {
                text.pop();
                true
            }
            (KeyCode::Enter, false) | (KeyCode::Esc, false) => false,
            _ => true,
        }
    }

    /// Closes the standing overlay, returns focus to the reading column
    /// when the overlay held it, and states what closed.
    fn close_overlay(&mut self, said: &str) {
        self.overlay = None;
        if self.focus == Focus::Actions {
            self.focus = self.menu_origin;
        }
        if let Some(worker) = &self.palette_worker {
            worker.cancel();
        }
        self.palette.pending = None;
        self.palette.edited = None;
        self.notice = Some(said.to_string());
    }

    /// Opens the action menu (SPEC 11.3 `:`): every action, its binding,
    /// and the reason a disabled action cannot run.
    fn open_menu(&mut self) {
        self.menu_origin = self.focus;
        self.palette.query.clear();
        self.palette.matched_query.clear();
        self.palette.choose_reference = false;
        self.palette.edited = None;
        self.palette.pending = None;
        self.palette.hits = super::palette::matching("", super::palette::actions()).0;
        self.overlay = Some(Overlay::Menu { selection: 0 });
        self.focus = Focus::Actions;
        self.request_palette();
    }

    pub fn help_offset(&self) -> usize {
        self.help_offset
    }

    pub fn palette(&self) -> &super::palette::Palette {
        &self.palette
    }

    fn edit_palette(&mut self) {
        if let Some(worker) = &self.palette_worker {
            worker.cancel();
        }
        self.palette.pending = None;
        self.palette.edited = Some(Instant::now());
        self.overlay = Some(Overlay::Menu { selection: 0 });
    }

    fn request_palette(&mut self) {
        if self.palette_worker.is_none() {
            match super::palette::Worker::spawn() {
                Ok(w) => self.palette_worker = Some(w),
                Err(e) => {
                    self.notice = Some(format!("Palette worker unavailable: {e}"));
                    return;
                }
            }
        }
        self.palette.pending = Some(self.palette_worker.as_ref().unwrap().request(
            self.root.clone(),
            self.documents.clone(),
            self.palette.query.clone(),
            self.palette.metadata.clone(),
            self.palette.choose_reference,
        ));
        self.palette.edited = None;
    }

    pub fn poll_palette(&mut self) {
        if matches!(self.overlay, Some(Overlay::Menu { .. }))
            && !self.reduced_updates
            && self
                .palette
                .edited
                .is_some_and(|t| t.elapsed() >= Duration::from_millis(100))
        {
            self.request_palette();
        }
        if let Some(reply) = self.palette_worker.as_ref().and_then(|w| w.receive())
            && matches!(self.overlay, Some(Overlay::Menu { .. }))
            && self.palette.pending == Some(reply.id)
            && self.palette.query == reply.query
        {
            let selected = if let Some(Overlay::Menu { selection }) = &self.overlay {
                self.palette
                    .hits
                    .get(*selection)
                    .map(|h| h.entry.id.clone())
            } else {
                None
            };
            self.palette.metadata = Some(reply.metadata);
            self.palette.hits = reply.hits;
            self.palette.additional = reply.additional;
            self.palette.matched_query = reply.query;
            self.palette.pending = None;
            self.palette.edited = None;
            let selection = selected
                .and_then(|id| self.palette.hits.iter().position(|h| h.entry.id == id))
                .unwrap_or(0);
            self.overlay = Some(Overlay::Menu { selection });
            self.redraw = true;
        }
    }

    fn fresh_listed(&self, path: &str) -> bool {
        let settings = app::read_local_settings(self.operations.git()).ok();
        match settings.and_then(|s| s.scope) {
            Some(scope) => app::resolve_scope(self.operations.git(), &scope.to_scope_input())
                .is_ok_and(|resolved| document_paths(&resolved).iter().any(|p| p == path)),
            None => discover_documents(&self.root).iter().any(|p| p == path),
        }
    }

    fn submit_palette(&mut self, selection: usize) {
        if !self.palette.ready()
            && !(self.palette.query.is_empty()
                && self.palette.edited.is_none()
                && !self.palette.choose_reference)
        {
            if self.palette.edited.is_some() || self.palette.pending.is_none() {
                self.request_palette();
            }
            self.notice =
                Some("Matching edited query; press Enter again after results arrive".into());
            return;
        }
        let Some(entry) = self.palette.hits.get(selection).map(|h| h.entry.clone()) else {
            return;
        };
        if let Some(action) = entry.action {
            if let Some(data) = self
                .operations
                .inspect_workspace(&InspectRequest::default())
                .data
            {
                self.absorb_inspection(data);
                self.observe_arrivals();
            }
            let Some(menu) = self.menu().into_iter().find(|m| m.action == action) else {
                return;
            };
            if let Some(reason) = menu.disabled {
                self.notice = Some(reason);
                return;
            }
            // The action sees its original pane, including reference restrictions.
            self.overlay = None;
            self.focus = self.menu_origin;
            self.activate(action);
            return;
        }
        if !self.fresh_listed(&entry.path) {
            self.notice = Some(
                "Document unavailable or outside the current scope; current review retained".into(),
            );
            return;
        }
        let current = super::search::read_text(&self.root, &entry.path, READ_VIEW_BYTES as usize);
        if !current
            .as_ref()
            .is_ok_and(|(bytes, _)| super::palette::fingerprint(bytes) == entry.fingerprint)
        {
            self.palette.metadata = None;
            self.request_palette();
            self.notice = Some(
                "Source changed; refreshing metadata. Reselect the passage before opening".into(),
            );
            return;
        }
        let Ok((bytes, truncated)) = current else {
            return;
        };
        let Some(text) = super::search::valid_text(&bytes, truncated) else {
            self.notice =
                Some("Document is no longer readable text; current review retained".into());
            return;
        };
        if self.palette.choose_reference {
            match super::reference::Reference::snapshot(
                self.operations,
                &entry.path,
                prepare_display(text),
                self.workspace.uncaptured_paths.contains(&entry.path),
                truncated,
                entry.line,
                false,
            ) {
                Ok(reference) => {
                    self.reference = Some(reference);
                    self.overlay = None;
                    self.focus = self.menu_origin;
                    if self.terminal_columns < 140 {
                        self.focus = Focus::Reference;
                    }
                    self.notice = Some("Reference pinned · read-only".into());
                }
                Err(error) => self.notice = Some(error),
            }
            return;
        }
        // Open exactly the bounded bytes whose identity was just validated,
        // rather than rereading a potentially replaced file after validation.
        let shown = self.show_candidate(&entry.path);
        let total = bytes.len() as u64;
        let (mut document, notice) =
            document_view(&entry.path, &Ok((bytes, total)), shown.as_ref());
        document.truncated = truncated;
        if truncated {
            document.total_bytes = None;
        }
        if let Some(notice) = notice {
            self.notice = Some(notice);
        }
        self.absorb_document(document);
        self.selection = self
            .documents
            .iter()
            .position(|p| p == &entry.path)
            .unwrap_or(self.selection);
        self.view = View::Read;
        self.focus = Focus::Reading;
        self.anchor = Some(SourceAnchor {
            path: entry.path,
            line: entry.line,
        });
        self.drift = 0;
        self.display_width = 0;
        self.overlay = None;
        self.notice = Some("Opened Read at the selected source anchor".into());
    }

    fn open_note(&mut self) {
        if self.view != View::Changes {
            self.notice = Some("Notes are available on a selected group in Changes".into());
            return;
        }
        if let Some(changes) = &self.changes
            && let Some(group) = changes.selected_group()
        {
            self.overlay = Some(Overlay::Note {
                commit: changes.candidate_oid().to_string(),
                group: group.change_id.clone(),
                text: String::new(),
            });
        } else {
            self.notice = Some("No change group is selected".into());
        }
    }

    /// Submits the document picker (SPEC 11.3 `f`): the filter names the
    /// first listed document whose path contains it, case-blind.
    fn submit_picker(&mut self, filter: &str) {
        self.overlay = None;
        let needle = filter.trim().to_lowercase();
        if needle.is_empty() {
            self.notice = Some("the document picker closed".to_string());
            return;
        }
        match self
            .documents
            .iter()
            .position(|path| path.to_lowercase().contains(&needle))
        {
            Some(index) => {
                self.selection = index;
                let path = self.documents[index].clone();
                self.load_document(&path);
                self.notice = Some(format!("opened {path}"));
            }
            None => {
                self.notice = Some(format!("no listed document matches {filter}"));
            }
        }
    }

    /// Submits the search field (SPEC 11.3 `/`): the query moves the
    /// reading position, the selected group, or the selected revision to
    /// the next match of the current view. Search changes no content.
    fn submit_search(&mut self, query: &str) {
        self.overlay = None;
        let typed = query.trim();
        if typed.is_empty() {
            self.notice = Some("search: type a query; / opens the field again".to_string());
            return;
        }
        let needle = typed.to_lowercase();
        if self.focus == Focus::Reference {
            let found = self.reference.as_mut().is_some_and(|r| r.search(&needle));
            self.notice = Some(if found {
                "Reference search: match".into()
            } else {
                "Reference search: no match".into()
            });
            return;
        }
        let found = match self.view {
            View::Read => self.search_reading(&needle),
            View::Changes => self
                .changes
                .as_mut()
                .and_then(|changes| changes.search(&needle))
                .map(|index| {
                    format!(
                        "group {} of {}",
                        index + 1,
                        self.changes
                            .as_ref()
                            .map_or(0, |changes| changes.groups().len()),
                    )
                }),
            View::History => self
                .history
                .as_mut()
                .and_then(|history| history.search(&needle))
                .map(|index| {
                    format!(
                        "revision {} of {}",
                        index + 1,
                        self.history
                            .as_ref()
                            .map_or(0, |history| history.rows().len()),
                    )
                }),
        };
        self.notice = Some(match found {
            Some(where_) => format!("search: match at {where_}"),
            None => format!("search: no match for {typed}"),
        });
    }

    /// Finds the next wrapped row of the Read view whose text contains
    /// `needle`, case-blind, and moves the reading position to it.
    fn search_reading(&mut self, needle: &str) -> Option<String> {
        if self.display.is_empty() {
            return None;
        }
        let start = (self.offset + 1).min(self.display.len());
        for step in 0..self.display.len() {
            let index = (start + step) % self.display.len();
            if self.display[index].text.to_lowercase().contains(needle) {
                self.offset = index;
                self.clamp_offset();
                self.sync_anchor();
                return Some(format!("line {}", self.display[index].source_line));
            }
        }
        None
    }

    fn invalidate_workspace_search(&mut self) {
        if let Some(worker) = &mut self.search_worker {
            worker.cancel();
        }
        self.workspace_search.request = None;
        self.workspace_search.results = None;
        self.workspace_search.selection = 0;
        self.redraw = true;
    }

    fn open_workspace_search(&mut self) {
        self.overlay = Some(Overlay::WorkspaceSearch);
        self.notice = None;
        if self.workspace_search.results.is_none()
            && !self.workspace_search.searching()
            && !self.workspace_search.query.trim().is_empty()
        {
            self.start_workspace_search();
        }
    }

    fn start_workspace_search(&mut self) {
        self.invalidate_workspace_search();
        let query = self.workspace_search.query.trim().to_string();
        if query.is_empty() {
            return;
        }
        if self.search_worker.is_none() {
            match SearchWorker::spawn() {
                Ok(worker) => self.search_worker = Some(worker),
                Err(error) => {
                    self.notice = Some(bounded(&format!("search could not start: {error}")));
                    return;
                }
            }
        }
        let id = self.search_worker.as_mut().unwrap().request(
            self.root.clone(),
            self.documents.clone(),
            query,
        );
        self.workspace_search.request = Some(id);
    }

    /// Called by the event loop, never by the renderer. Old/cancelled
    /// queries cannot replace the current panel, including after Esc.
    pub fn poll_workspace_search(&mut self) {
        if let Some(worker) = &mut self.search_worker {
            while let Some(reply) = worker.receive() {
                if self.overlay == Some(Overlay::WorkspaceSearch)
                    && self.workspace_search.request == Some(reply.id)
                {
                    self.workspace_search.request = None;
                    self.workspace_search.results = Some(reply.results);
                    self.workspace_search.selection = 0;
                    self.redraw = true;
                }
            }
        }
    }

    fn submit_workspace_search(&mut self) {
        let hit = self
            .workspace_search
            .results
            .as_ref()
            .and_then(|results| results.hits.get(self.workspace_search.selection))
            .cloned();
        let Some(hit) = hit else {
            if !self.workspace_search.searching() {
                self.start_workspace_search();
            }
            return;
        };
        // Revalidate against disk before opening: a result is a location,
        // not an immutable promise about a file that another editor can change.
        let query = self.workspace_search.query.trim().to_lowercase();
        let valid = crate::tui::search::hit_is_current(&self.root, &hit, &query);
        if !valid || !self.documents.contains(&hit.path) {
            self.invalidate_workspace_search();
            self.notice = Some("search result changed on disk; Enter searches again".into());
            return;
        }
        self.selection = self
            .documents
            .iter()
            .position(|path| path == &hit.path)
            .unwrap();
        self.load_document(&hit.path);
        self.view = View::Read;
        self.focus = Focus::Reading;
        // Source mode makes comments and Markdown syntax matches visible,
        // and source-line anchors survive wrapping and resizing.
        self.reading_source = true;
        self.anchor = Some(SourceAnchor {
            path: hit.path.clone(),
            line: hit.line as u32,
        });
        self.drift = 0;
        self.display_width = 0;
        self.overlay = None;
        self.notice = Some(format!(
            "search: {}:{} · source view · Ctrl+F returns to results",
            hit.path, hit.line
        ));
    }

    /// Answers one key press of the reading column, where every key but
    /// the finish action changes only what the interface shows.
    fn handle_column_key(&mut self, key: &KeyEvent, control: bool, quit_warning_stood: bool) {
        if self.focus == Focus::Reference {
            match (key.code, control) {
                (KeyCode::Char('c'), true) | (KeyCode::Char('q'), false) => {
                    self.request_quit(quit_warning_stood)
                }
                (KeyCode::Esc, false) => self.back_to_review(),
                (KeyCode::Tab, _) => self.cycle_focus(true),
                (KeyCode::BackTab, _) => self.cycle_focus(false),
                (KeyCode::Char(':'), false) => self.open_menu(),
                (KeyCode::Char('?'), false) => self.overlay = Some(Overlay::Help),
                (KeyCode::Char('s'), false) => {
                    if let Some(r) = &mut self.reference {
                        r.toggle_source();
                    }
                }
                (KeyCode::Char('/'), false) => {
                    self.overlay = Some(Overlay::Search {
                        query: String::new(),
                    })
                }
                (KeyCode::Char('j'), false) | (KeyCode::Down, false) => self.step(1),
                (KeyCode::Char('k'), false) | (KeyCode::Up, false) => self.step(-1),
                (KeyCode::PageDown, false) => self.page(1),
                (KeyCode::PageUp, false) => self.page(-1),
                (KeyCode::Char('d'), false) => {
                    if let Some(r) = &self.reference {
                        self.notice = Some(format!(
                            "Reference {} · {} · fingerprint {} · captured {} ms UTC",
                            r.descriptor.path,
                            r.descriptor.revision.as_deref().unwrap_or("working"),
                            r.descriptor.fingerprint,
                            r.descriptor.captured_ms
                        ));
                    }
                }
                _ => {
                    self.notice = Some(
                        "Return to review to change decisions, add notes, edit, restore, or finish"
                            .into(),
                    )
                }
            }
            return;
        }
        if key.code == KeyCode::Esc && self.reference_visible() {
            self.back_to_review();
            return;
        }
        match (key.code, control) {
            (KeyCode::Char('f'), true) => self.open_workspace_search(),
            (KeyCode::Char('c'), true) | (KeyCode::Char('q'), false) => {
                self.request_quit(quit_warning_stood)
            }
            (KeyCode::Char('['), false) => self.toggle_sidebar(),
            (KeyCode::Char('d'), false) => self.toggle_details(),
            (KeyCode::Char('u'), false) if self.view == View::Changes => self.undo_decision(),
            (KeyCode::Left, false) if self.focus == Focus::Documents => {
                self.outline_folded = true;
                self.navigator_cursor = self.selection;
                self.redraw = true;
            }
            (KeyCode::Right, false) if self.focus == Focus::Documents => {
                self.outline_folded = false;
                self.redraw = true;
            }
            (KeyCode::Enter, false) if self.focus == Focus::Documents => {
                self.open_navigator_entry();
                self.view = View::Read;
                self.focus = Focus::Reading;
            }
            (KeyCode::Char('s'), false) if self.view == View::Read => self.toggle_current_source(),
            (KeyCode::Char('1'), false) => self.view = View::Read,
            (KeyCode::Char('2'), false) => self.enter_changes(),
            (KeyCode::Char('3'), false) => self.view = View::History,
            (KeyCode::Char('N'), false) => self.open_note(),
            // `R` refreshes the read-only state explicitly (SPEC 5.3);
            // lowercase `r` stays one proposed restoration.
            (KeyCode::Char('R'), false) => self.refresh(),
            // `:` opens every action with its binding and its disabled
            // reason (SPEC 11.3).
            (KeyCode::Char(':'), false) => self.open_menu(),
            // `?` lists the contextual keys and the current blockers
            // (SPEC 11.3).
            (KeyCode::Char('?'), false) => self.overlay = Some(Overlay::Help),
            // `/` searches within the current view; the field takes
            // precedence over every command key while it stands (SPEC
            // 11.3).
            (KeyCode::Char('/'), false) => {
                self.overlay = Some(Overlay::Search {
                    query: String::new(),
                });
            }
            // `f` opens the document picker (SPEC 11.3).
            (KeyCode::Char('f'), false) => {
                if self.documents.is_empty() {
                    self.notice = Some("no documents are listed to find".to_string());
                } else {
                    self.overlay = Some(Overlay::Picker {
                        filter: String::new(),
                    });
                }
            }
            // `e` hands the displayed document to the configured editor
            // (SPEC 11.3, 12.5); the event loop runs the handoff while the
            // terminal is suspended.
            (KeyCode::Char('e'), false) => self.request_editor(),
            (KeyCode::Tab, _) => self.cycle_focus(true),
            (KeyCode::BackTab, _) => self.cycle_focus(false),
            (KeyCode::Char('j'), false) | (KeyCode::Down, false) => self.step(1),
            (KeyCode::Char('k'), false) | (KeyCode::Up, false) => self.step(-1),
            (KeyCode::PageDown, false) => self.page(1),
            (KeyCode::PageUp, false) => self.page(-1),
            (KeyCode::Char('n'), false) if self.view == View::Changes => self.step_group(1),
            (KeyCode::Char('p'), false) if self.view == View::Changes => self.step_group(-1),
            // `K` records a proposed keep; lowercase `k` stays navigation
            // (SPEC 11.3).
            (KeyCode::Char('K'), false) if self.view == View::Changes => {
                self.toggle_decision(Decision::Keep)
            }
            // `r` toggles a proposed restoration, never a disk write (SPEC
            // 5.2, 11.3).
            (KeyCode::Char('r'), false) if self.view == View::Changes => {
                self.toggle_decision(Decision::Restore)
            }
            // `s` shows the exact Markdown changes of every group (SPEC
            // 11.3).
            (KeyCode::Char('s'), false) if self.view == View::Changes => self.toggle_source_view(),
            // `Enter` opens the finish summary of the whole scope (SPEC
            // 5.2, 11.3); only its second press applies.
            (KeyCode::Enter, false) if self.view == View::Changes => self.open_finish(),
            // `Enter` opens the restoration preview of the displayed
            // document at the selected revision (SPEC 5.5, 11.3); only its
            // second press restores.
            (KeyCode::Enter, false) if self.view == View::History => self.open_history_preview(),
            // `a` keeps collapsed checkpoints reachable: the timeline lists
            // every one of them instead of one row per writing pass (SPEC
            // 5.5).
            (KeyCode::Char('a'), false) if self.view == View::History => {
                self.toggle_history_checkpoints()
            }
            _ => {}
        }
    }

    /// The action menu this shell offers right now (SPEC 11.3): every
    /// action of the keyboard contract, each with the reason in words
    /// when the current context cannot run it.
    pub fn menu(&self) -> Vec<MenuEntry> {
        let changes_only = "the Changes view only; press 2 to switch";
        let in_changes = self.view == View::Changes;
        let groups = self
            .changes
            .as_ref()
            .map_or(0, |changes| changes.groups().len());
        Action::ALL
            .iter()
            .map(|&action| {
                let disabled = match action {
                    Action::PinDocument => (self.view != View::Read || self.document.is_none())
                        .then(|| "Display a document in Read to pin it".into()),
                    Action::PinVersion => (self.view != View::History
                        || self.history.as_ref().and_then(|h| h.selected()).is_none())
                    .then(|| "Select a version in History".into()),
                    Action::ToggleReference
                    | Action::FocusReference
                    | Action::ClearReference
                    | Action::RefreshReference
                    | Action::BackToReview => self
                        .reference
                        .is_none()
                        .then(|| "No reference is pinned".into()),
                    Action::ResumeReview => self.saved_reviews.is_empty().then(|| {
                        if self.saved_omitted {
                            "No saved reviews loaded; additional records omitted".into()
                        } else if let Some(error) = &self.session_error {
                            format!("Saved reviews unavailable: {error}")
                        } else {
                            "No saved reviews".into()
                        }
                    }),
                    Action::QuitWithoutSaving => self
                        .session_error
                        .is_none()
                        .then(|| "Progress has no unsaved failure".into()),
                    Action::NextGroup | Action::PreviousGroup | Action::AddNote => {
                        if !in_changes {
                            Some(changes_only.to_string())
                        } else if groups == 0 {
                            Some("no change groups exist in this review".to_string())
                        } else {
                            None
                        }
                    }
                    Action::KeepGroup | Action::RestoreGroup | Action::UndoDecision => {
                        if !in_changes {
                            Some(changes_only.to_string())
                        } else {
                            self.changes.as_ref().and_then(|changes| changes.refusal())
                        }
                    }
                    Action::SourceView => {
                        if self.view == View::Read {
                            None
                        } else if !in_changes {
                            Some("Read or Changes view only".into())
                        } else if self.changes.is_none() {
                            Some("the comparison is unavailable right now".to_string())
                        } else {
                            None
                        }
                    }
                    Action::FinishReview => {
                        if !in_changes {
                            Some(changes_only.to_string())
                        } else if groups == 0 {
                            Some("nothing waits for review".to_string())
                        } else {
                            self.changes.as_ref().and_then(|changes| changes.refusal())
                        }
                    }
                    Action::DocumentPicker | Action::WorkspaceSearch => (self.documents.is_empty())
                        .then(|| "no documents are listed to find".to_string()),
                    Action::EditDocument => {
                        if self.document.is_none() {
                            Some("no document is displayed".to_string())
                        } else if self.editor.is_none() {
                            Some(
                                "no editor is configured; set margin init --editor-argv-json \
                                 [\"nvim\",\"--\"]"
                                    .to_string(),
                            )
                        } else {
                            None
                        }
                    }
                    Action::Cancel => {
                        let stands = self.finish.is_some()
                            || self.history_preview_stands()
                            || self.overlay.is_some();
                        (!stands).then(|| "no overlay or preview stands".to_string())
                    }
                    _ => None,
                };
                let reference_focus = self.focus == Focus::Reference
                    || (self.focus == Focus::Actions && self.menu_origin == Focus::Reference);
                let disabled = if reference_focus
                    && matches!(
                        action,
                        Action::NextGroup
                            | Action::PreviousGroup
                            | Action::KeepGroup
                            | Action::RestoreGroup
                            | Action::UndoDecision
                            | Action::AddNote
                            | Action::EditDocument
                            | Action::FinishReview
                    ) {
                    Some(format!(
                        "Return to review to {}",
                        action.name().to_lowercase()
                    ))
                } else if reference_focus && action == Action::SourceView {
                    None
                } else {
                    disabled
                };
                MenuEntry { action, disabled }
            })
            .collect()
    }

    /// Runs one action, as the menu's `Enter` does (SPEC 11.3). Every
    /// branch is the handler of the action's default binding, so the
    /// menu and the keys cannot disagree.
    fn activate(&mut self, action: Action) {
        match action {
            Action::PinDocument => self.pin_document(),
            Action::PinVersion => self.pin_version(),
            Action::ChooseReference => {
                self.open_menu();
                self.palette.choose_reference = true;
                self.palette.hits.clear();
                self.palette.query.clear();
                self.request_palette();
            }
            Action::ToggleReference => {
                if self.terminal_columns < 140 {
                    if self.reference_only() {
                        self.back_to_review();
                    } else if let Some(r) = &mut self.reference {
                        r.descriptor.visible = true;
                        self.focus = Focus::Reference;
                    }
                } else if let Some(r) = &mut self.reference {
                    r.descriptor.visible = !r.descriptor.visible;
                    self.focus = Focus::Reading;
                }
            }
            Action::FocusReference => {
                if let Some(r) = &mut self.reference {
                    r.descriptor.visible = true;
                    self.focus = Focus::Reference;
                }
            }
            Action::ClearReference => {
                self.reference = None;
                self.focus = Focus::Reading;
            }
            Action::RefreshReference => self.refresh_reference(),
            Action::BackToReview => self.back_to_review(),
            Action::ResumeReview => {
                self.saved_offset = 0;
                self.overlay = Some(Overlay::SavedReviews {
                    selection: 0,
                    inspect: false,
                    confirm: false,
                })
            }
            Action::SeparateReview => {
                if self.retire_draft() {
                    self.load_background();
                    self.notice = Some("Separate review; saved choices left intact".into());
                }
            }
            Action::QuitWithoutSaving => {
                if self.session_error.is_some() {
                    self.quit = true;
                }
            }
            Action::CycleFocus => self.cycle_focus(true),
            Action::ActionMenu => self.open_menu(),
            Action::ReadView => {
                self.view = View::Read;
                self.focus = Focus::Reading;
            }
            Action::ChangesView => {
                self.focus = Focus::Reading;
                self.enter_changes();
            }
            Action::HistoryView => {
                self.view = View::History;
                self.focus = Focus::Reading;
            }
            // `Move` is `j` and `k` together and `PageMove` is `PgUp` and
            // `PgDn` together; the menu entries state the binding rather
            // than running one step. `Cancel` from the menu is the menu's
            // own closing, which already ran.
            Action::Move | Action::PageMove => {}
            Action::Cancel => {
                self.notice = Some("the overlay closed; nothing was applied".to_string())
            }
            Action::NextGroup => self.step_group(1),
            Action::PreviousGroup => self.step_group(-1),
            Action::KeepGroup => self.toggle_decision(Decision::Keep),
            Action::RestoreGroup => self.toggle_decision(Decision::Restore),
            Action::SourceView => self.toggle_current_source(),
            Action::ToggleSidebar => self.toggle_sidebar(),
            Action::Details => {
                if self.focus == Focus::Reference {
                    if let Some(r) = &self.reference {
                        self.notice = Some(format!(
                            "Reference {} · {:?} · fingerprint {}",
                            r.descriptor.path, r.descriptor.revision, r.descriptor.fingerprint
                        ));
                    }
                } else {
                    self.toggle_details();
                }
            }
            Action::UndoDecision => self.undo_decision(),
            Action::AddNote => self.open_note(),
            Action::DocumentPicker => {
                if self.documents.is_empty() {
                    self.notice = Some("no documents are listed to find".to_string());
                } else {
                    self.overlay = Some(Overlay::Picker {
                        filter: String::new(),
                    });
                }
            }
            Action::Search => {
                self.overlay = Some(Overlay::Search {
                    query: String::new(),
                });
            }
            Action::WorkspaceSearch => self.open_workspace_search(),
            Action::EditDocument => self.request_editor(),
            Action::FinishReview => self.open_finish(),
            Action::Help => self.overlay = Some(Overlay::Help),
            Action::Quit => self.request_quit(false),
        }
    }

    /// Requests one handoff of the displayed document to the configured
    /// editor (SPEC 11.3 `e`, 12.5). The request carries the explicit
    /// argv of the local settings and the literal path of the document;
    /// the event loop executes it while the terminal is suspended, so
    /// the shell itself never owns a terminal.
    fn request_editor(&mut self) {
        let Some(document) = self.document.as_ref() else {
            self.notice = Some("no document is displayed to edit".to_string());
            return;
        };
        let Some(argv) = self.editor.clone() else {
            self.notice = Some(
                "no editor is configured; set margin init --editor-argv-json [\"nvim\",\"--\"]"
                    .to_string(),
            );
            return;
        };
        let joined = self.root.join(&document.path);
        let path = std::fs::canonicalize(&joined).unwrap_or(joined);
        self.pending_editor = Some(EditorHandoff { argv, path });
    }

    /// Takes the pending editor handoff, when one stands. The event loop
    /// calls this between frames.
    pub fn take_editor_request(&mut self) -> Option<EditorHandoff> {
        self.pending_editor.take()
    }

    /// Absorbs the return of one editor handoff (SPEC 11.5, 12.5): the
    /// terminal is back and the read-only state of the displayed document
    /// reloads, so the exit is one refresh boundary. It approves nothing.
    /// A completed editing pass captures only while the configured policy
    /// enables it, and that capture is the same shared checkpoint every
    /// other save runs, so it creates one unreviewed draft and moves no
    /// review baseline. An editor that exits nonzero is the abandoned pass
    /// every terminal tool reads it as: it refreshes and reports only.
    pub fn editor_returned(&mut self, result: Result<std::process::ExitStatus, String>) {
        self.redraw = true;
        let status = match result {
            Ok(status) => status,
            Err(why) => {
                // The editor never started, so no boundary was crossed and
                // nothing on any disk changed through it.
                self.notice = Some(bounded(&why));
                return;
            }
        };
        let exited = if status.success() {
            "editor returned".to_string()
        } else {
            format!("editor exited with {status}")
        };
        let reloaded = match self.document.as_ref().map(|document| document.path.clone()) {
            Some(path) => format!("{path} reloaded; "),
            None => "no document stands to reload; ".to_string(),
        };
        if !self.capture_after_edit || !status.success() {
            // Without the policy — or when the editor exited nonzero, the
            // convention every terminal tool reads as an abandoned pass —
            // the exit refreshes and reports only.
            self.reload_document();
            self.notice = Some(if status.success() {
                format!("{exited}; {reloaded}no checkpoint was made")
            } else {
                format!("{exited}; {reloaded}nothing was applied")
            });
            return;
        }
        // The policy makes this completed pass one capture boundary
        // (SPEC 12.5): the same shared checkpoint every other save runs.
        let outcome = self.operations.create_checkpoint(&CheckpointRequest {
            scope: None,
            message: None,
            include_staged: false,
            expect_head: None,
            expect_index: None,
            expect_working: None,
            dry_run: false,
        });
        if !outcome.ok() {
            // The capture refused; the reload still happens and the typed
            // reason stands as the notice.
            self.reload_document();
            let error = outcome.error.expect("a failed outcome carries its error");
            self.notice = Some(bounded(&format!(
                "{exited}; {reloaded}{}: {}",
                error.code, error.message
            )));
            return;
        }
        let data = outcome.data.expect("a successful outcome carries data");
        let captured = match &data.commit {
            Some(commit) => format!(
                "draft saved as {}; nothing was approved",
                render::short(commit)
            ),
            None => "no changes waited to be captured; nothing was approved".to_string(),
        };
        // The capture moved the candidate, so the whole read-only state
        // rebuilds over it the same way one saved draft does.
        self.refresh_after_saved_draft();
        self.notice = Some(format!("{exited}; {reloaded}{captured}"));
    }

    /// The fingerprint of the frame-visible state: the parts of the shell
    /// one frame draws. The reduced-update mode compares it around every
    /// event, so a frame is drawn again only when something it shows
    /// changed (SPEC 11.4).
    fn fingerprint(&self) -> String {
        let overlay = match &self.overlay {
            None => "none".to_string(),
            Some(Overlay::Menu { selection }) => format!(
                "menu:{selection}:{}:{:?}",
                self.palette.query, self.palette.pending
            ),
            Some(Overlay::Help) => format!("help:{}", self.help_offset),
            Some(Overlay::SavedReviews {
                selection,
                inspect,
                confirm,
            }) => format!(
                "saved:{selection}:{inspect}:{confirm}:{}",
                self.saved_offset
            ),
            Some(Overlay::Picker { filter }) => format!("picker:{filter}"),
            Some(Overlay::Search { query }) => format!("search:{query}"),
            Some(Overlay::WorkspaceSearch) => format!(
                "workspace-search:{}:{}:{:?}",
                self.workspace_search.query,
                self.workspace_search.selection,
                self.workspace_search.request
            ),
            Some(Overlay::Note {
                commit,
                group,
                text,
            }) => format!("note:{commit}:{group}:{text}"),
        };
        let document = self
            .document
            .as_ref()
            .map_or("none".to_string(), |document| {
                // The text hashes in, because one edit can keep every
                // countable field and still change every frame row.
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                document.text.hash(&mut hasher);
                format!(
                    "{}|{:?}|{}|{}|{:x}",
                    document.path,
                    document.saved,
                    document.truncated,
                    document.total_bytes.unwrap_or(0),
                    hasher.finish(),
                )
            });
        let mut documents_hasher = std::collections::hash_map::DefaultHasher::new();
        self.documents.hash(&mut documents_hasher);
        let changes = self.changes.as_ref().map_or("none".to_string(), |changes| {
            format!(
                "{:?}|{}|{}|{}|{}",
                changes.selection(),
                changes.source_view(),
                changes.stale().is_some(),
                changes.proposed_count(),
                changes.offset(),
            )
        });
        let history = self.history.as_ref().map_or("none".to_string(), |history| {
            format!("{:?}|{:?}", history.selection(), history.preview_state())
        });
        let workspace = &self.workspace;
        [
            format!("{:?}", self.view),
            format!("{:?}", self.focus),
            format!(
                "{:?}",
                self.reference
                    .as_ref()
                    .map(|r| (&r.descriptor, &r.unavailable, r.changed))
            ),
            overlay,
            format!("{:?}", self.notice),
            format!("{:?}", self.quit),
            format!("{:?}", workspace.adopted),
            format!("{:?}", workspace.branch),
            format!("{:?}", workspace.head),
            document,
            format!("{:x}", documents_hasher.finish()),
            format!("{}", self.selection),
            format!("{}", self.offset),
            format!("{}", self.drift),
            format!("{}", self.display_width),
            format!("{:?}", self.anchor),
            changes,
            history,
            format!("{}", self.finish.is_some()),
            format!("{}", self.finish_result.is_some()),
            format!("{}", self.restore_result.is_some()),
            format!("{:?}", workspace.preservation),
            format!("{:?}", workspace.review),
            format!("{:?}", workspace.note),
            format!("{:?}", self.fingerprint_workspace_counts()),
        ]
        .join("|")
    }

    /// The countable workspace fields of the fingerprint, kept apart so
    /// the format string stays readable.
    fn fingerprint_workspace_counts(&self) -> (usize, Option<usize>, usize, usize) {
        let workspace = &self.workspace;
        (
            workspace.documents,
            workspace.uncaptured,
            workspace.pending_files.unwrap_or(0),
            workspace.blockers.len(),
        )
    }

    /// Takes the redraw signal: whether the next frame must be drawn.
    /// The reduced-update mode calls this instead of drawing every pass
    /// (SPEC 11.4).
    pub fn take_redraw(&mut self) -> bool {
        std::mem::take(&mut self.redraw)
    }

    /// Says whether a redraw is pending, without taking it.
    pub fn needs_redraw(&self) -> bool {
        self.redraw
    }

    /// Says whether one restoration preview stands over the History view
    /// (SPEC 5.5).
    fn history_preview_stands(&self) -> bool {
        self.history
            .as_ref()
            .is_some_and(|history| history.preview().is_some())
    }

    /// Answers one key press of the standing restoration preview, one local
    /// context of the History view (SPEC 5.5, 11.3).
    fn handle_history_preview_key(
        &mut self,
        key: &KeyEvent,
        control: bool,
        quit_warning_stood: bool,
    ) {
        let page = self.viewport_rows.saturating_sub(1).max(1) as i32;
        match (key.code, control) {
            (KeyCode::Esc, false) => self.cancel_history_preview(),
            (KeyCode::Char('c'), true) | (KeyCode::Char('q'), false) => {
                self.request_quit(quit_warning_stood)
            }
            (KeyCode::Enter, false) | (KeyCode::Char('s'), false) => self.answer_history_preview(),
            (KeyCode::Char('j'), false) | (KeyCode::Down, false) => self.scroll_history(1),
            (KeyCode::Char('k'), false) | (KeyCode::Up, false) => self.scroll_history(-1),
            (KeyCode::PageDown, false) => self.scroll_history(page),
            (KeyCode::PageUp, false) => self.scroll_history(-page),
            _ => {
                self.notice = Some(
                    "Enter previews or restores; s saves the draft first; Esc cancels".to_string(),
                );
            }
        }
    }

    /// Scrolls the standing preview, as the History column answers `j`,
    /// `k`, and paging while one stands (SPEC 11.3).
    fn scroll_history(&mut self, rows: i32) {
        if let Some(history) = &mut self.history {
            history.scroll_preview(rows);
        }
    }

    /// Opens the restoration preview of the displayed document at the
    /// selected revision (SPEC 5.5). The preview is read-only: the dry run
    /// of the shared `restore` operation proves the intended result and
    /// writes nothing.
    fn open_history_preview(&mut self) {
        let Some(path) = self.document.as_ref().map(|document| document.path.clone()) else {
            self.notice = Some("no document is displayed to restore".to_string());
            return;
        };
        let Some(history) = &mut self.history else {
            self.notice = Some("the timeline is unavailable right now".to_string());
            return;
        };
        self.notice = history.open_preview(self.operations, &path).err();
    }

    /// Answers the standing preview's labeled action (SPEC 5.5): the save
    /// offer saves the current draft through the shared checkpoint
    /// operation and reopens the preview over the newly captured candidate,
    /// and a proved preview applies through the shared restore operation.
    fn answer_history_preview(&mut self) {
        let offer_save = self
            .history
            .as_ref()
            .is_some_and(|history| history.preview().is_some_and(RestorePreview::save_offer));
        if offer_save {
            self.save_draft_then_preview();
            return;
        }
        let proved = self
            .history
            .as_ref()
            .is_some_and(|history| history.preview().is_some_and(RestorePreview::proved));
        if !proved {
            self.notice =
                Some("the preview proves no restoration right now; Esc cancels it".to_string());
            return;
        }
        self.apply_history_preview();
    }

    /// Saves the current draft, then reopens the preview over the newly
    /// captured candidate (SPEC 5.5). Both the preview and the final
    /// command read the candidate the checkpoint just recorded.
    fn save_draft_then_preview(&mut self) {
        let Some(path) = self.document.as_ref().map(|document| document.path.clone()) else {
            self.notice = Some("no document is displayed to restore".to_string());
            return;
        };
        match crate::tui::history::save_current_draft(self.operations) {
            Ok(saved) => {
                // The reload rebuilds the read-only state over the new
                // candidate, keeping the selected revision; the preview
                // then opens over that candidate.
                self.refresh_after_saved_draft();
                self.notice = Some(match self.history.as_mut() {
                    Some(history) => match history.open_preview(self.operations, &path) {
                        Ok(()) => saved,
                        Err(why) => why,
                    },
                    None => "the timeline is unavailable right now".to_string(),
                });
            }
            Err(why) => self.notice = Some(why),
        }
    }

    /// Applies the standing preview through the shared restore operation
    /// (SPEC 5.5, 9.5, 16.9) and states the outcome in one notice.
    fn apply_history_preview(&mut self) {
        let outcome = match self
            .history
            .as_ref()
            .map(|history| history.apply_preview(self.operations))
        {
            Some(Some(outcome)) => outcome,
            _ => {
                self.notice = Some("no preview stands to restore".to_string());
                return;
            }
        };
        let notice = match outcome.data.as_ref() {
            Some(data) if outcome.ok() => match &data.existing_receipt {
                Some(receipt) => {
                    format!(
                        "restoration already recorded: commit {}",
                        render::short(receipt)
                    )
                }
                None => {
                    let commit = data
                        .commit
                        .as_deref()
                        .map(render::short)
                        .unwrap_or_else(|| "unknown".to_string());
                    format!(
                        "restored {} from {} · commit {commit} · the review baseline is \
                         unchanged",
                        data.path,
                        render::short(&data.revision_oid),
                    )
                }
            },
            _ => match outcome.error.as_ref() {
                Some(error) => format!("{}: {}", error.code, bounded(&error.message)),
                None => "the restoration outcome is incomplete".to_string(),
            },
        };
        let applied = outcome.ok();
        self.notice = Some(notice);
        self.restore_result = Some(outcome);
        if applied {
            self.refresh_after_restore();
        }
    }

    /// Cancels the standing restoration preview; nothing was applied (SPEC
    /// 11.3).
    fn cancel_history_preview(&mut self) {
        if let Some(history) = &mut self.history {
            history.cancel_preview();
        }
        self.notice = Some("preview cancelled; nothing was restored".to_string());
    }

    /// Toggles the checkpoint display policy of the timeline (SPEC 5.5).
    fn toggle_history_checkpoints(&mut self) {
        let Some(history) = &mut self.history else {
            self.notice = Some("the timeline is unavailable right now".to_string());
            return;
        };
        self.notice = Some(if history.toggle_checkpoints(self.operations) {
            "the timeline lists every draft checkpoint".to_string()
        } else {
            "the timeline is unavailable right now".to_string()
        });
    }

    /// Rebuilds the read-only state after one applied restoration (SPEC
    /// 5.5): the workspace, the documents, the comparison, and the
    /// timeline, each through its shared operation. The restored scope is
    /// pending review, and the preview is gone with the candidate it
    /// previewed against.
    fn refresh_after_restore(&mut self) {
        let descriptor = self.reload_settings();
        if let Some(history) = &mut self.history {
            history.cancel_preview();
        }
        if let Some(descriptor) = descriptor {
            self.load_adopted(descriptor);
        }
        self.reload_document();
    }

    /// Rebuilds the read-only state after one saved draft, which changed
    /// the candidate under the preview offer (SPEC 5.5).
    fn refresh_after_saved_draft(&mut self) {
        let descriptor = self.reload_settings();
        if let Some(descriptor) = descriptor {
            self.load_adopted(descriptor);
        }
        self.reload_document();
    }

    /// Refreshes the read-only state explicitly (SPEC 5.3, 13.3): the
    /// workspace, the documents, the comparison, and the timeline rebuild
    /// over the repository as it stands, and the observation of that
    /// repository re-pins with them, so a stale notice clears only this
    /// way. The unapplied selections reset, and one notice states the
    /// reset; selection migration is deferred. The pinned context
    /// survives: the displayed document keeps its source anchor and its
    /// reading position, restored the same way a resize restores them
    /// (SPEC 11.2).
    fn refresh(&mut self) {
        if !self.retire_draft() {
            return;
        }
        let reset = self.changes.as_ref().map_or(0, ChangesView::proposed_count);
        let descriptor = self.reload_settings();
        match descriptor {
            Some(descriptor) => self.load_adopted(descriptor),
            None => {
                let folder = self.workspace.folder.clone();
                self.load_unadopted(&folder);
            }
        }
        self.reload_document();
        // The reload may have met a document it could only describe, not
        // read; that notice stands beside the reset the writer asked for.
        let said = self.refresh_notice(reset);
        self.notice = Some(match self.notice.take() {
            Some(earlier) => bounded(&format!("{said} {earlier}")),
            None => bounded(&said),
        });
    }

    /// The notice one explicit refresh leaves (SPEC 5.3): the candidate
    /// the rebuilt comparison pins, the unapplied selections it reset,
    /// and the deferred selection migration, each said in words.
    fn refresh_notice(&self, reset: usize) -> String {
        let candidate = self
            .changes
            .as_ref()
            .map(|changes| format!("C {}", render::short(changes.candidate_oid())));
        match (candidate, reset) {
            (Some(candidate), 0) => {
                format!("Refreshed over {candidate}; no selection stood to reset.")
            }
            (Some(candidate), 1) => format!(
                "Refreshed over {candidate}; 1 unapplied selection was reset; selections do \
                 not carry over."
            ),
            (Some(candidate), count) => format!(
                "Refreshed over {candidate}; {count} unapplied selections were reset; \
                 selections do not carry over."
            ),
            (None, _) => "Refreshed; the comparison is unavailable right now.".to_string(),
        }
    }

    /// Reloads the displayed document and restores the reading position
    /// from the anchor plus the best-effort drift (SPEC 5.4, 11.2): one
    /// refresh keeps the reader at the source anchor, never at the first
    /// line of the new bytes. The wrapped rows recompute on the next
    /// prepared frame, and the offset restores from the anchor then.
    fn reload_document(&mut self) {
        let Some(path) = self.document.as_ref().map(|document| document.path.clone()) else {
            return;
        };
        let line = self.anchor.as_ref().map(|anchor| anchor.line);
        let drift = self.drift;
        self.load_document(&path);
        if let Some(line) = line {
            let last = self.source_lines.len().clamp(1, u32::MAX as usize) as u32;
            self.anchor = Some(SourceAnchor {
                path,
                line: line.min(last),
            });
            self.drift = drift;
        }
    }

    /// Asks to quit. Quit restores the terminal (SPEC 11.3), and the shell
    /// applied nothing the writer did not confirm, so only unapplied
    /// selections earn a warning: one clear notice precedes their explicit
    /// discard, and quitting never commits them (SPEC 5.2).
    fn request_quit(&mut self, quit_warning_stood: bool) {
        self.update_progress();
        if quit_warning_stood && self.session_error.is_some() {
            self.quit = true;
            return;
        }
        match self.flush_progress() {
            Ok(()) => {
                if self.draft.is_some() {
                    self.notice = Some("Choices remain saved locally and unapplied".into());
                }
                self.quit = true;
            }
            Err(error) => {
                self.quit_warning = true;
                self.notice = Some(format!(
                    "Progress not saved: {error}. Press q again to quit without saving newer choices."
                ));
            }
        }
    }

    /// Opens the finish summary (SPEC 5.2): the entire writing scope and
    /// the intended result one read-only preview proved, beside the
    /// explicit keep-remaining intent. Nothing is applied here.
    fn open_finish(&mut self) {
        let Some(changes) = &self.changes else {
            self.notice = Some("no comparison exists to finish".to_string());
            return;
        };
        if changes.groups().is_empty() {
            self.notice = Some("nothing waits for review".to_string());
            return;
        }
        if let Some(why) = changes.refusal() {
            self.notice = Some(why);
            return;
        }
        let summary = FinishSummary::open(self.operations, changes);
        self.finish = Some(FinishPanel {
            summary,
            rows: Vec::new(),
            width: 0,
        });
    }

    /// Applies the plan the open summary states, through the shared review
    /// operation the command line exposes (SPEC 5.2, 9.4, 16.9), and
    /// states the outcome in one notice.
    fn apply_finish(&mut self) {
        let Some(panel) = self.finish.take() else {
            return;
        };
        if let (Some(id), Some(plan)) = (panel.summary.request_id(), panel.summary.plan_document())
        {
            self.update_progress();
            if let Some(draft) = &mut self.draft {
                draft.attempt = Some(session::Attempt {
                    request_id: id.into(),
                    plan: plan.into(),
                });
                draft.generation += 1;
            }
            // Local save failure never blocks an explicitly confirmed review.
            let _ = self.flush_progress();
        }
        let outcome = panel.summary.apply(self.operations);
        let notice = match outcome.data.as_ref() {
            Some(data) if outcome.ok() => match &data.existing_receipt {
                Some(receipt) => {
                    format!("review already recorded: commit {}", render::short(receipt))
                }
                None => {
                    let commit = data
                        .commit
                        .as_deref()
                        .map(render::short)
                        .unwrap_or_else(|| "unknown".to_string());
                    let kept = data
                        .kept
                        .map(|kept| format!("{kept} kept"))
                        .unwrap_or_else(|| "kept count unknown".to_string());
                    format!(
                        "review recorded: commit {commit} · {} restored · {kept}",
                        data.restored.len(),
                    )
                }
            },
            _ => match outcome.error.as_ref() {
                Some(error) => format!("{}: {}", error.code, bounded(&error.message)),
                None => "the finish outcome is incomplete".to_string(),
            },
        };
        let applied = outcome.ok();
        self.notice = Some(notice);
        self.finish_result = Some(outcome);
        if applied {
            if let Some(writer) = self.writer.take()
                && let Err(error) = writer.remove()
            {
                self.notice = Some(format!(
                    "{} · review succeeded; draft cleanup failed: {error}",
                    self.notice.as_deref().unwrap_or("Review recorded")
                ));
            }
            self.draft = None;
            self.decision_pending = None;
            self.navigation_pending = false;
            self.session_error = None;
            self.list_saved_reviews();
            self.refresh_after_finish();
        }
    }

    /// Cancels the open finish summary; nothing was applied (SPEC 11.3).
    fn cancel_finish(&mut self) {
        self.finish = None;
        self.notice = Some("finish cancelled; no selection was applied".to_string());
    }

    /// Rebuilds the read-only state after one applied review: the
    /// workspace, the documents, the comparison, and the timeline, each
    /// through its shared operation. The applied plan is gone with the
    /// review it decided.
    fn refresh_after_finish(&mut self) {
        let descriptor = self.reload_settings();
        if let Some(descriptor) = descriptor {
            self.load_adopted(descriptor);
        }
        self.reload_document();
    }

    /// Enters the Changes view: the selected group becomes visited. The
    /// head observation that follows every key covers the stale check.
    fn enter_changes(&mut self) {
        self.view = View::Changes;
        if let Some(changes) = &mut self.changes {
            changes.visit_selection();
        }
    }

    /// Observes the current head against the pinned comparison, so a stale
    /// arrival relabels the groups the next frame draws (SPEC 5.3). A query
    /// that fails cannot claim staleness; the refresh slice owns the full
    /// observation.
    fn observe_changes_head(&mut self) {
        if let Some(changes) = &mut self.changes {
            let head = self.operations.git().head_commit().ok().flatten();
            changes.observe_head(head.as_deref());
        }
    }

    /// Absorbs one answered background inspection (SPEC 13.3, 5.3).
    ///
    /// A reply whose generation the shell left behind is discarded whole:
    /// the state it describes is not the state on screen. A failed query
    /// observes nothing, because it cannot claim an arrival — but its
    /// watch parts still absorb, because the working file and the
    /// document list are honest whatever the workspace query said. One
    /// successful answer refreshes the workspace summary the header
    /// states, compares the observed workspace against the observation
    /// the pinned comparison carries, then absorbs the watched document
    /// and list.
    pub fn observe_inspection(&mut self, reply: InspectionReply) {
        let before = self.fingerprint();
        let InspectionReply {
            reference,
            notes,
            generation,
            outcome,
            document,
            documents,
        } = reply;
        if generation != self.generation {
            return;
        }
        if outcome.ok()
            && let Some(data) = outcome.data
        {
            self.absorb_inspection(data);
            self.observe_arrivals();
        }
        if let Some((descriptor, fingerprint)) = reference
            && let Some(r) = &mut self.reference
            && r.descriptor.path == descriptor.path
            && r.descriptor.fingerprint == descriptor.fingerprint
            && r.descriptor.revision.is_none()
            && fingerprint.as_deref() != Some(&r.descriptor.fingerprint)
        {
            r.changed = true;
            self.redraw = true;
        }
        if let Some(watched) = document {
            self.absorb_watched_file(watched);
        }
        if let Some(paths) = documents {
            self.absorb_document_list(paths);
        }
        if let Some(notes) = notes {
            if let Some(changes) = &mut self.changes {
                self.redraw |= changes.update_orphaned_notes(&notes);
            }
            if let Some(changes) = &mut self.changes
                && let Some(fresh) = notes.iter().find(|n| n.commit == changes.candidate_oid())
            {
                self.redraw |= changes.update_notes(fresh.clone());
            }
            if let Some(history) = &mut self.history {
                self.redraw |= history.update_notes(&notes);
            }
        }
        if self.fingerprint() != before {
            self.redraw = true;
        }
    }

    /// What the next poll rereads for this shell (SPEC 13.3): the
    /// displayed document and the document list, against the adopted
    /// scope's stored input or the discovery walk.
    pub fn watch(&self) -> refresh::Watch {
        refresh::Watch {
            reference: self
                .reference
                .as_ref()
                .filter(|r| r.descriptor.revision.is_none())
                .map(|r| r.descriptor.clone()),
            notes_commits: self
                .changes
                .iter()
                .flat_map(|c| c.note_commits())
                .chain(
                    self.history
                        .iter()
                        .flat_map(|h| h.rows().iter().map(|r| r.commit.clone())),
                )
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect(),
            root: self.root.clone(),
            document: self.document.as_ref().map(|document| document.path.clone()),
            list: match &self.descriptor {
                Some(descriptor) => refresh::WatchList::Adopted(descriptor.to_scope_input()),
                None => refresh::WatchList::Unadopted,
            },
        }
    }

    /// Absorbs one watched document's reread (SPEC 5.4, 11.2, 13.3): when
    /// its bytes changed, the view rebuilds through the same builder an
    /// interactive load uses, and the reading position — the anchor plus
    /// its drift — survives the swap. A reread the writer no longer
    /// displays, or one that could not read the file, changes nothing.
    fn absorb_watched_file(&mut self, watched: refresh::WatchedDocument) {
        if matches!(watched.file, refresh::WatchedFile::Unread) {
            return;
        }
        let Some(current) = self.document.as_ref() else {
            return;
        };
        if current.path != watched.path {
            return;
        }
        let working = match &watched.file {
            refresh::WatchedFile::Bytes(bytes, total) => Ok((bytes.clone(), *total)),
            // The deletion restates itself as the same NotFound the
            // interactive load meets, so the label rule stays one rule.
            refresh::WatchedFile::Gone => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            refresh::WatchedFile::Unread => return,
        };
        let (document, notice) = document_view(&watched.path, &working, watched.candidate.as_ref());
        let unchanged = document.text == current.text
            && document.total_bytes == current.total_bytes
            && document.truncated == current.truncated
            && document.saved == current.saved;
        if unchanged {
            return;
        }
        let gone = matches!(watched.file, refresh::WatchedFile::Gone);
        let said = notice.unwrap_or_else(|| {
            bounded(&if gone {
                format!(
                    "{} is gone from disk; the saved candidate still reads",
                    watched.path
                )
            } else {
                format!("{} changed on disk; reloaded", watched.path)
            })
        });
        let line = self.anchor.as_ref().map(|anchor| anchor.line);
        let drift = self.drift;
        self.invalidate_palette();
        self.headings = wrap::document_headings(&document.text);
        self.source_lines = document.text.lines().map(str::to_string).collect();
        self.display = Vec::new();
        self.display_width = 0;
        if let Some(line) = line {
            let last = self.source_lines.len().clamp(1, u32::MAX as usize) as u32;
            self.anchor = Some(SourceAnchor {
                path: document.path.clone(),
                line: line.min(last),
            });
        }
        self.drift = drift;
        self.document = Some(document);
        self.redraw = true;
        // An arrival notice always wins its place: the review cannot
        // apply over it, so it is the more urgent sentence (SPEC 5.3).
        if self.notice.is_none() {
            self.notice = Some(said);
        }
    }

    /// Absorbs one rebuilt document list (SPEC 13.3): the selection stays
    /// on its document by path, and the displayed document stays
    /// untouched even when the list no longer names it.
    fn absorb_document_list(&mut self, paths: Vec<String>) {
        if self.documents != paths {
            self.invalidate_palette();
        }
        if paths == self.documents {
            return;
        }
        self.invalidate_workspace_search();
        let remembered = self.documents.get(self.selection).cloned();
        self.documents = paths;
        self.workspace.documents = self.documents.len();
        let last = self.documents.len().saturating_sub(1);
        self.selection = remembered
            .as_ref()
            .and_then(|path| self.documents.iter().position(|name| name == path))
            .unwrap_or(self.selection.min(last));
        self.redraw = true;
    }

    /// Compares the observed workspace against the pinned observation
    /// (SPEC 5.3): the first arrival marks the pinned comparison stale,
    /// which relabels every group and disables application, while the
    /// text stays readable. One notice states the arrival, because a
    /// writer reading another view cannot see the column relabel.
    fn observe_arrivals(&mut self) {
        let Some(pinned) = self.pinned.clone() else {
            return;
        };
        let Some(why) = refresh::arrival_over(&pinned, &self.workspace) else {
            return;
        };
        let newly = self
            .changes
            .as_mut()
            .is_some_and(|changes| changes.observe_arrival(&why));
        if newly {
            self.notice = Some(bounded(&format!(
                "New edits arrived. Refresh before applying. {why}"
            )));
        }
    }

    /// The generation of the read-only state (SPEC 13.3): the value every
    /// inspection job carries.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Moves the selection between canonical change groups (SPEC 11.3).
    fn step_group(&mut self, direction: i32) {
        let Some(changes) = &mut self.changes else {
            self.notice = Some("no comparison exists to move through".to_string());
            return;
        };
        if changes.groups().is_empty() {
            self.notice = Some("no change groups exist in this review".to_string());
            return;
        }
        if !changes.step_selection(direction) {
            self.notice = Some(if direction > 0 {
                "this is the last passage group".to_string()
            } else {
                "this is the first passage group".to_string()
            });
        }
    }

    /// Toggles one proposed decision on the selected group (SPEC 5.2,
    /// 11.3). The change reaches the in-memory plan only.
    fn toggle_decision(&mut self, decision: Decision) {
        let Some(changes) = &mut self.changes else {
            self.notice = Some("no comparison exists to decide".to_string());
            return;
        };
        let outcome = changes.toggle(decision);
        self.notice = Some(match (&outcome, decision) {
            (ToggleOutcome::Recorded, Decision::Keep) => "proposed: keep this passage".to_string(),
            (ToggleOutcome::Recorded, Decision::Restore) => {
                "proposed: restore the previous passage".to_string()
            }
            (ToggleOutcome::Undone, Decision::Keep) => {
                "keep undone: the passage is undecided again".to_string()
            }
            (ToggleOutcome::Undone, Decision::Restore) => {
                "restoration undone: the passage is undecided again".to_string()
            }
            (ToggleOutcome::Refused(why), _) => why.clone(),
        });
    }

    /// Toggles the source view of the Changes column (SPEC 11.3 `s`).
    fn toggle_source_view(&mut self) {
        let Some(changes) = &mut self.changes else {
            self.notice = Some("no comparison exists to show".to_string());
            return;
        };
        self.notice = Some(if changes.toggle_source() {
            "source view: the exact Markdown changes of each passage".to_string()
        } else {
            "passage view: the changed passages of each group".to_string()
        });
    }

    /// Cycles focus through the visible stops (SPEC 11.3): the document
    /// list, the reading column, and the standing action menu, forward on
    /// `Tab` and backward on `Shift-Tab`. A stop the frame cannot show —
    /// a hidden list, no menu — simply is not a stop, so no key press
    /// loses the focus the surface can actually use.
    fn cycle_focus(&mut self, forward: bool) {
        let menu = matches!(self.overlay, Some(Overlay::Menu { .. }));
        let documents = self.navigator_visible() && !self.documents.is_empty();
        let mut stops: Vec<Focus> = Vec::with_capacity(3);
        if documents {
            stops.push(Focus::Documents);
        }
        stops.push(Focus::Reading);
        if self.reference_visible() && self.terminal_columns >= 140 {
            stops.push(Focus::Reference);
        }
        if menu {
            stops.push(Focus::Actions);
        }
        if stops.len() == 1 {
            self.focus = stops[0];
            return;
        }
        let next = match stops.iter().position(|stop| *stop == self.focus) {
            Some(index) if forward => (index + 1) % stops.len(),
            Some(index) => (index + stops.len() - 1) % stops.len(),
            None if forward => stops
                .iter()
                .position(|stop| *stop == Focus::Reading)
                .unwrap_or(0),
            None => stops.len() - 1,
        };
        self.focus = stops[next];
    }

    /// Moves one step in the focused element. The action menu answers
    /// `j` and `k` itself while it stands, so a step never reaches this
    /// match with that focus.
    fn step(&mut self, direction: i32) {
        match self.focus {
            Focus::Reference => {
                if let Some(r) = &mut self.reference {
                    r.scroll(direction);
                }
            }
            Focus::Documents => self.step_navigator(direction),
            Focus::Reading | Focus::Actions => self.step_reading(direction),
        }
    }

    /// Moves the reading position by rows; in the Changes view this scrolls
    /// the changes column and the selection follows the top visible group,
    /// and in the History view it moves the selected revision.
    fn step_reading(&mut self, direction: i32) {
        match self.view {
            View::Changes => {
                if let Some(changes) = &mut self.changes {
                    changes.scroll_by(direction);
                }
            }
            View::History => {
                // The timeline is one list: a step moves the selected
                // revision and scrolls to it, never a scroll that only
                // follows the top row (SPEC 11.3).
                if let Some(history) = &mut self.history {
                    history.step_selection(direction);
                }
            }
            View::Read => {
                if direction > 0 {
                    self.offset = self.offset.saturating_add(1);
                } else {
                    self.offset = self.offset.saturating_sub(1);
                }
                self.clamp_offset();
                self.sync_anchor();
            }
        }
    }

    /// Moves one viewport, less one held row, in the focused element
    /// (SPEC 11.3 `PgUp` and `PgDn`): the document list pages its
    /// selection, and the reading position pages by view.
    fn page(&mut self, direction: i32) {
        if self.focus == Focus::Reference {
            if let Some(r) = &mut self.reference {
                r.scroll(direction * self.viewport_rows.saturating_sub(2).max(1) as i32);
            }
            return;
        }
        if self.focus == Focus::Documents {
            let page = self.viewport_rows.saturating_sub(1).max(1);
            let last = self.navigator_entries().len().saturating_sub(1);
            self.navigator_cursor = if direction > 0 {
                self.navigator_cursor.saturating_add(page).min(last)
            } else {
                self.navigator_cursor.saturating_sub(page)
            };
            self.open_navigator_entry();
            return;
        }
        match self.view {
            View::Changes => {
                let page = self.viewport_rows.saturating_sub(2).max(1) as i32;
                if let Some(changes) = &mut self.changes {
                    changes.scroll_by(if direction > 0 { page } else { -page });
                }
            }
            View::History => {
                let page = self.viewport_rows.saturating_sub(1).max(1);
                if let Some(history) = &mut self.history {
                    history.page_selection(direction, page);
                }
            }
            View::Read => {
                let page = self.viewport_rows.saturating_sub(1).max(1);
                if direction > 0 {
                    self.offset = self.offset.saturating_add(page);
                } else {
                    self.offset = self.offset.saturating_sub(page);
                }
                self.clamp_offset();
                self.sync_anchor();
            }
        }
    }

    /// The outline contains only the active document's headings.
    pub fn navigator_entries(&self) -> Vec<(usize, Option<usize>)> {
        let mut entries = Vec::new();
        for index in 0..self.documents.len() {
            entries.push((index, None));
            if index == self.selection && !self.outline_folded {
                entries.extend((0..self.headings.len()).map(|h| (index, Some(h))));
            }
        }
        entries
    }

    pub fn headings(&self) -> &[wrap::Heading] {
        &self.headings
    }
    pub fn navigator_visible(&self) -> bool {
        self.layout.shows_navigator()
            && !self.sidebar_hidden
            && !(self.reference_visible()
                && (self.terminal_columns >= 140
                    || self.focus == Focus::Reference
                    || (self.focus == Focus::Actions && self.menu_origin == Focus::Reference)))
    }
    pub fn sidebar_hidden(&self) -> bool {
        self.sidebar_hidden
    }
    pub fn outline_folded(&self) -> bool {
        self.outline_folded
    }
    pub fn reading_source(&self) -> bool {
        self.reading_source
    }
    pub fn navigator_cursor(&self) -> usize {
        self.navigator_cursor
    }
    pub fn active_heading(&self) -> Option<usize> {
        let line = self.anchor.as_ref()?.line;
        self.headings.iter().rposition(|h| h.line <= line)
    }

    fn toggle_sidebar(&mut self) {
        self.sidebar_hidden = !self.sidebar_hidden;
        if self.sidebar_hidden {
            self.focus = Focus::Reading;
        }
        self.redraw = true;
    }

    fn toggle_current_source(&mut self) {
        if self.focus == Focus::Reference {
            if let Some(r) = &mut self.reference {
                r.toggle_source();
            }
            return;
        }
        if self.view == View::Read {
            self.reading_source = !self.reading_source;
            self.display_width = 0;
            self.redraw = true;
        } else if self.view == View::Changes {
            self.toggle_source_view();
        }
    }

    fn toggle_details(&mut self) {
        match self.view {
            View::Changes => {
                if let Some(changes) = &mut self.changes {
                    changes.toggle_details();
                }
            }
            View::History => {
                if let Some(history) = &mut self.history {
                    history.toggle_details();
                }
            }
            View::Read => {
                self.outline_folded = !self.outline_folded;
                self.navigator_cursor = self.selection;
            }
        }
        self.redraw = true;
    }

    fn undo_decision(&mut self) {
        if let Some(changes) = &mut self.changes {
            changes.undo();
        }
        self.notice = Some("Decision undone; the passage is undecided.".into());
        self.redraw = true;
    }

    fn step_navigator(&mut self, direction: i32) {
        let len = self.navigator_entries().len();
        self.navigator_cursor = if direction > 0 {
            (self.navigator_cursor + 1).min(len.saturating_sub(1))
        } else {
            self.navigator_cursor.saturating_sub(1)
        };
        self.open_navigator_entry();
    }

    fn open_navigator_entry(&mut self) {
        let Some((document, heading)) =
            self.navigator_entries().get(self.navigator_cursor).copied()
        else {
            return;
        };
        if let Some(heading) = heading {
            self.view = View::Read;
            self.prepare_reading(self.display_width.max(1));
            self.offset = wrap::row_for_anchor(&self.display, self.headings[heading].line);
            self.sync_anchor();
        } else if document != self.selection {
            self.selection = document;
            let path = self.documents[document].clone();
            self.load_document(&path);
            self.navigator_cursor = self
                .navigator_entries()
                .iter()
                .position(|entry| *entry == (document, None))
                .unwrap_or(0);
        }
        self.redraw = true;
    }

    /// Says whether the event loop should stop.
    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// The active view.
    pub fn view(&self) -> View {
        self.view
    }

    /// The focused element.
    pub fn focus(&self) -> Focus {
        self.focus
    }

    /// The layout class of the last prepared frame.
    pub fn width_class(&self) -> WidthClass {
        self.layout
    }

    /// The listed documents.
    pub fn documents(&self) -> &[String] {
        &self.documents
    }

    /// The selected document index.
    pub fn selection(&self) -> usize {
        self.selection
    }

    /// The displayed document.
    pub fn document(&self) -> Option<&DocumentView> {
        self.document.as_ref()
    }

    /// The wrapped rows of the reading column.
    pub fn display_rows(&self) -> &[DisplayRow] {
        &self.display
    }

    /// The first visible row of the reading column.
    pub fn reading_offset(&self) -> usize {
        self.offset
    }

    /// The workspace snapshot.
    pub fn workspace(&self) -> &WorkspaceSnapshot {
        &self.workspace
    }

    /// The comparison summary, when one was loadable.
    pub fn changes(&self) -> Option<&ChangesView> {
        self.changes.as_ref()
    }

    /// The open finish summary, when one stands (SPEC 5.2).
    pub fn finish(&self) -> Option<&FinishSummary> {
        self.finish.as_ref().map(|panel| &panel.summary)
    }

    /// The wrapped rows of the open finish summary.
    pub fn finish_rows(&self) -> &[DisplayRow] {
        self.finish.as_ref().map_or(&[], |panel| &panel.rows)
    }

    /// The outcome of the last confirmed finish, when one ran.
    pub fn finish_result(&self) -> Option<&ReviewDecisionResult> {
        self.finish_result.as_ref()
    }

    /// The History view, when its timeline was loadable.
    pub fn history(&self) -> Option<&HistoryView> {
        self.history.as_ref()
    }

    /// The outcome of the last confirmed restoration, when one ran.
    pub fn restore_result(&self) -> Option<&RestoreResult> {
        self.restore_result.as_ref()
    }

    /// The standing notice, when one exists.
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// The standing overlay, when one stands (SPEC 11.3).
    pub fn overlay(&self) -> Option<&Overlay> {
        self.overlay.as_ref()
    }

    pub fn workspace_search(&self) -> &WorkspaceSearch {
        &self.workspace_search
    }

    /// The documents the picker filter matches, in list order, with the
    /// query folded to one case.
    pub fn picker_matches(&self, filter: &str) -> Vec<&str> {
        let needle = filter.trim().to_lowercase();
        if needle.is_empty() {
            return self.documents.iter().map(String::as_str).collect();
        }
        self.documents
            .iter()
            .map(String::as_str)
            .filter(|path| path.to_lowercase().contains(&needle))
            .collect()
    }

    /// The color mode of this run (SPEC 11.4).
    pub fn color(&self) -> ColorMode {
        self.color
    }

    /// Says whether this run draws no decoration at all (SPEC 11.4).
    pub fn color_off(&self) -> bool {
        self.color == ColorMode::Never
    }

    /// The current blockers the help list states (SPEC 11.3 `?`): the
    /// policy blockers of the workspace, the refusal of the pinned
    /// comparison, and the workspace note, each in words.
    pub fn blockers(&self) -> Vec<String> {
        let mut blockers: Vec<String> = self
            .workspace
            .blockers
            .iter()
            .map(|blocker| blocker.to_string())
            .collect();
        if let Some(why) = self.changes.as_ref().and_then(|changes| changes.refusal()) {
            blockers.push(why);
        }
        if let Some(note) = &self.workspace.note {
            blockers.push(note.clone());
        }
        blockers
    }
}

/// The review state of an inspection, with the starting point kept
/// separate from approval (SPEC 4.1).
fn review_state_of(baseline: &BaselineStatus, reported: ReviewState) -> ReviewState {
    match baseline {
        BaselineStatus::NoCandidate => ReviewState::Blocked,
        _ => reported,
    }
}

/// Says whether the working bytes equal the candidate content.
///
/// Both sides carry the same byte bound, so equal totals plus an equal
/// bound-length prefix is equality, with no fabricated answer when the
/// candidate view is incomplete and the prefix cannot be compared.
fn working_matches(content: &DocumentContent, bytes: &[u8], total: u64) -> bool {
    match content {
        DocumentContent::Empty { .. } => total == 0,
        DocumentContent::Text(text) => {
            text.total_bytes as u64 == total
                && text.text.len() <= bytes.len()
                && text.text.as_bytes() == &bytes[..text.text.len()]
        }
        _ => false,
    }
}

/// Builds one document view from one working read and one candidate
/// outcome: the label rule, the display text, and the notice one failed
/// read earns. The interactive load and the live reload share this one
/// builder, so the two can never disagree about what the reader sees or
/// the label it reads (SPEC 5.4).
fn document_view(
    path: &str,
    working: &std::io::Result<(Vec<u8>, u64)>,
    shown: Option<&ShowOutcome>,
) -> (DocumentView, Option<String>) {
    let mut document = DocumentView {
        path: path.to_string(),
        candidate: None,
        saved: SavedState::UnsavedDraft,
        text: String::new(),
        total_bytes: None,
        truncated: false,
    };
    let mut notice = None;
    match working {
        Ok((bytes, total)) => {
            document.total_bytes = Some(*total);
            document.truncated = *total > READ_VIEW_BYTES;
            match shown {
                Some(outcome) => {
                    document.candidate = Some(outcome.revision_oid.clone());
                    if working_matches(&outcome.content, bytes, *total) {
                        document.saved = SavedState::Saved;
                        document.text = display_text(&outcome.content);
                    } else if matches!(outcome.content, DocumentContent::Opaque(_)) {
                        // Opaque bytes cannot pair with a working
                        // preview, so the honest label is unknown.
                        document.saved = SavedState::Unknown;
                        document.text = display_text(&outcome.content);
                        document.truncated = content_truncated(&outcome.content);
                    } else {
                        // The working bytes differ, or the candidate
                        // is absent: the view is a labeled preview.
                        document.saved = SavedState::UnsavedDraft;
                        document.text = String::from_utf8_lossy(bytes).into_owned();
                    }
                }
                None => {
                    // No candidate exists to compare with.
                    document.saved = SavedState::UnsavedDraft;
                    document.text = String::from_utf8_lossy(bytes).into_owned();
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => match shown {
            Some(outcome) => {
                document.candidate = Some(outcome.revision_oid.clone());
                document.saved = match outcome.content {
                    DocumentContent::Missing => SavedState::Unknown,
                    _ => SavedState::WorkingFileGone,
                };
                document.text = display_text(&outcome.content);
                document.total_bytes = content_bytes(&outcome.content);
                document.truncated = content_truncated(&outcome.content);
            }
            None => {
                document.saved = SavedState::Unknown;
                notice = Some(bounded(&format!("no document exists at {path}")));
            }
        },
        Err(error) => {
            document.saved = SavedState::Unknown;
            notice = Some(bounded(&format!(
                "the working file cannot be read: {error}"
            )));
            if let Some(outcome) = shown {
                document.candidate = Some(outcome.revision_oid.clone());
                document.text = display_text(&outcome.content);
                document.total_bytes = content_bytes(&outcome.content);
            }
        }
    }
    document.text = prepare_display(&document.text);
    (document, notice)
}

/// Reads the working bytes of one path, bounded by `limit`.
pub(crate) fn read_working(root: &Path, path: &str, limit: u64) -> std::io::Result<(Vec<u8>, u64)> {
    let file = fs::File::open(root.join(path))?;
    let total = file.metadata()?.len();
    let mut bytes = Vec::new();
    let mut taken = file.take(limit + 1);
    taken.read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bytes.truncate(limit as usize);
    }
    Ok((bytes, total))
}

/// The bounded display text of one content state.
fn display_text(content: &DocumentContent) -> String {
    match content {
        DocumentContent::Missing => String::new(),
        DocumentContent::Empty { .. } => String::new(),
        DocumentContent::Text(text) => text.text.clone(),
        DocumentContent::Opaque(view) => {
            let shown: String = view.hex.chars().take(64).collect();
            format!(
                "This document holds {} bytes that are not valid UTF-8.\nLeading bytes: {shown}",
                view.total_bytes
            )
        }
        DocumentContent::Exact { bytes, .. } => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// The byte length a content state reports.
fn content_bytes(content: &DocumentContent) -> Option<u64> {
    match content {
        DocumentContent::Missing => None,
        DocumentContent::Empty { .. } => Some(0),
        DocumentContent::Text(text) => Some(text.total_bytes as u64),
        DocumentContent::Opaque(view) => Some(view.total_bytes as u64),
        DocumentContent::Exact { bytes, .. } => Some(bytes.len() as u64),
    }
}

/// Whether a content state dropped content to a bound.
fn content_truncated(content: &DocumentContent) -> bool {
    match content {
        DocumentContent::Text(text) => text.truncated,
        DocumentContent::Opaque(view) => view.truncated,
        _ => false,
    }
}

/// Prepares raw text for display: tabs become spaces and unsafe control
/// characters become their visible escapes (SPEC 14.2).
pub(crate) fn prepare_display(raw: &str) -> String {
    crate::cli::display_escape(&raw.replace('\t', "    "))
}

/// The folder name a header shows.
fn folder_name(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string())
}

/// Truncates one status line to the notice bound.
fn bounded(text: &str) -> String {
    let mut bounded: String = text.chars().take(NOTICE_CHARS).collect();
    if bounded.chars().count() < text.chars().count() {
        bounded.push('…');
    }
    bounded
}

/// Discovers the Markdown documents of one folder before adoption
/// (SPEC 5.1).
pub(crate) fn discover_documents(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    walk_documents(root, String::new(), 0, &mut found);
    found.sort();
    found.truncate(MAX_DISCOVERED_DOCUMENTS);
    found
}

/// The document paths of one resolved scope: the tracked and untracked
/// entries, already sorted by path bytes (BTreeMap iteration order). The
/// interactive load and the polling watch share this one mapping, so the
/// list they show can never disagree.
pub(crate) fn document_paths(resolved: &app::ResolvedScope) -> Vec<String> {
    resolved
        .paths
        .iter()
        .filter(|(_, state)| matches!(state, ScopedPathState::Tracked | ScopedPathState::Untracked))
        .map(|(path, _)| path.to_string_lossy().into_owned())
        .collect()
}

/// Walks one directory level, collecting Markdown paths relative to the
/// discovery root.
fn walk_documents(root: &Path, prefix: String, depth: usize, found: &mut Vec<String>) {
    if depth > MAX_DISCOVERY_DEPTH || found.len() >= MAX_DISCOVERED_DOCUMENTS {
        return;
    }
    let directory = if prefix.is_empty() {
        root.to_path_buf()
    } else {
        root.join(&prefix)
    };
    let Ok(entries) = fs::read_dir(&directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let relative = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}{name}")
        };
        let Ok(metadata) = fs::metadata(entry.path()) else {
            continue;
        };
        if metadata.is_dir() {
            walk_documents(root, format!("{relative}/"), depth + 1, found);
        } else if metadata.is_file() && name.to_lowercase().ends_with(".md") {
            found.push(relative);
        }
    }
}

impl Drop for Shell<'_> {
    fn drop(&mut self) {
        // Catchable termination gets a best-effort flush. SIGKILL cannot.
        if self.draft.is_some() && self.writer.is_some() {
            let _ =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.flush_progress()));
        }
    }
}
