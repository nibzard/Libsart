//! The terminal shell: views, retained context, and read-only loading.
//!
//! The shell holds every piece of state the surface needs and nothing
//! else. It loads its data through the shared operations of
//! [`crate::app`], so the terminal and the command line read one
//! repository through one contract. Opening the shell before adoption
//! changes nothing (SPEC 5.1), and every key it answers changes at most
//! what the interface shows — the `K` and `r` keys of the Changes view
//! included, which change one in-memory plan and nothing on any disk
//! (SPEC 5.2).
//!
//! Exactly two actions write, and both hand a composed plan document to
//! the same shared operation the command line exposes (SPEC 5.2, 5.5,
//! 16.9): the confirmed finish of a review — the flow lives in
//! [`crate::tui::finish`], this shell opens its summary and applies it on
//! the second `Enter` — and the confirmed restoration of one document,
//! which the History view previews first and restores on its second
//! `Enter` through [`crate::tui::history`]. Both reload the read-only
//! state afterwards. Quit never commits: unapplied selections are
//! discarded only after one clear notice.
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

use std::cell::RefCell;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};

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

/// How many rows one wheel notch moves a free-scrolling column. Lists
/// move one entry per notch instead, the distance of `j` and `k`.
const WHEEL_ROWS: usize = 3;

/// How many documents the pre-adoption discovery lists (SPEC 5.1).
pub(crate) const MAX_DISCOVERED_DOCUMENTS: usize = 500;

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
}

impl Focus {
    /// The label the footer prints for the focused element.
    pub fn label(self) -> &'static str {
        match self {
            Focus::Documents => "document list",
            Focus::Reading => "reading",
            Focus::Actions => "action menu",
        }
    }
}

/// One standing overlay of the surface (SPEC 11.3): the action menu, the
/// help list, or one of the two text-entry fields. The picker and the
/// search field hold the text the writer typed; ordinary keys are text
/// there, not commands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Overlay {
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
            Overlay::Picker { .. }
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
/// editor (SPEC 11.3 `E`, 12.5): the explicit argv from the local
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

/// One saved reading position of a document the writer left (SPEC 5.4):
/// the source anchor is the position; the drift is the best-effort display
/// offset inside the anchor's source line (SPEC 11.2).
#[derive(Clone)]
struct SavedPosition {
    path: String,
    line: u32,
    drift: usize,
}

/// How many documents keep their reading position (SPEC 5.4). The map is
/// disposable interface state: losing one position costs one scroll, never
/// one decision or one draft.
const MAX_SAVED_POSITIONS: usize = 64;

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
    viewport_columns: u16,
    anchor: Option<SourceAnchor>,
    /// The reading positions of documents the writer left, most recently
    /// left first (SPEC 5.4): one view switch or one document return
    /// restores the position the anchor names, never a rendered row.
    saved_positions: Vec<SavedPosition>,
    /// The document navigation is moving to, when movement outran the
    /// load: the target the next quiet pass loads. While it stands, the
    /// reading column keeps the last settled content and every
    /// content-dependent action states its reason instead of running — a
    /// decision must never land on the passage the column does not show.
    pending_document: Option<String>,
    /// Whether the unadopted discovery stopped at its document bound, so
    /// the navigator says the list is bounded instead of showing a quietly
    /// short list (SPEC 11.3).
    discovery_truncated: bool,
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
    pub(crate) help_scroll: usize,
    pub(crate) help_query: String,
    pub(crate) help_filtering: bool,
    pub(crate) help_all: bool,
    help_return: Option<Overlay>,
    pub(crate) menu_query: String,
    pub(crate) menu_filtering: bool,
    pub(crate) picker_selected: Option<String>,
    pub(crate) finish_scroll: usize,
    pub(crate) request_scroll: usize,
    pub(crate) agent_status: String,
    pointer: RefCell<super::pointer::PointerTargets>,
    workspace_search: WorkspaceSearch,
    search_worker: Option<SearchWorker>,
    /// One requested editor handoff the event loop has not run yet (SPEC
    /// 11.3 `E`, 12.5).
    pending_editor: Option<EditorHandoff>,
    source_editor: Option<super::editor::Editor>,
    retained_editors: Vec<super::editor::Editor>,
    autosave_worker: Option<super::editor::SaveWorker>,
    history_pending: bool,
    history_error: Option<String>,
    agent_prompt: Option<(
        crate::editing::Version,
        Option<std::ops::Range<usize>>,
        String,
    )>,
    request_poll: std::time::Instant,
    recovery_poll: std::time::Instant,
    request_panel: Option<String>,
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
}

impl<'a> Shell<'a> {
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
        // Document operands are repository-relative even when desk is
        // launched from an agent's current subdirectory.
        let repository_root = operations.git().toplevel().ok();
        let root = repository_root.as_deref().unwrap_or(root);
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
            viewport_columns: 76,
            anchor: None,
            saved_positions: Vec::new(),
            pending_document: None,
            discovery_truncated: false,
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
            help_scroll: 0,
            help_query: String::new(),
            help_filtering: false,
            help_all: false,
            help_return: None,
            menu_query: String::new(),
            menu_filtering: false,
            picker_selected: None,
            finish_scroll: 0,
            request_scroll: 0,
            agent_status: String::new(),
            pointer: RefCell::default(),
            workspace_search: WorkspaceSearch::default(),
            search_worker: None,
            pending_editor: None,
            source_editor: None,
            retained_editors: Vec::new(),
            autosave_worker: None,
            history_pending: false,
            history_error: None,
            agent_prompt: None,
            request_poll: std::time::Instant::now(),
            recovery_poll: std::time::Instant::now(),
            request_panel: None,
            editor: None,
            capture_after_edit: false,
            redraw: true,
            background_pending: false,
            color: options.color,
            notice: None,
            quit_warning: false,
            quit: false,
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
        // One rebuild recomputes the whole read-only state, so a pending
        // navigation target from the rebuilt state is void: the reload
        // below carries the displayed document, and the writer navigates
        // again over what the rebuild now shows.
        self.pending_document = None;
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
        let previous_notes = self.changes.as_ref().and_then(|changes| {
            self.operations
                .list_notes(Some(changes.candidate_oid()))
                .data
        });
        // One reload of the same pinned review keeps the hovered group by
        // its change ID (SPEC 5.4); a new review pins new IDs, so its
        // hover starts at the first group.
        let hovered = self.changes.as_ref().map(|changes| {
            (
                changes.review_id().to_string(),
                changes
                    .selected_group()
                    .map(|group| group.change_id.clone()),
            )
        });
        self.changes = ChangesView::load(self.operations);
        if let (Some((review, change)), Some(changes)) = (hovered, &mut self.changes)
            && changes.review_id() == review
            && let Some(change) = change
        {
            changes.select_change(&change);
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
        if !self.background_pending {
            return false;
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
        let (documents, truncated) = discover_documents(&self.root);
        self.documents = documents;
        self.discovery_truncated = truncated;
        self.pending_document = None;
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
        self.save_position();
        self.offset = 0;
        self.drift = 0;
        let shown = self.show_candidate(path);
        let working = read_working(&self.root, path, READ_VIEW_BYTES);
        let (document, notice) = document_view(path, &working, shown.as_ref());
        if let Some(text) = notice {
            self.notice = Some(text);
        }
        self.absorb_document(document);
        self.restore_position(path);
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
        self.layout = layout::classify(columns, rows);
        let geometry = layout::frame_layout_with_navigator(
            ratatui::layout::Rect::new(0, 0, columns, rows),
            !self.sidebar_hidden,
        );
        if geometry.navigator.is_none() && self.focus == Focus::Documents {
            self.focus = Focus::Reading;
        }
        self.viewport_rows = geometry.reading.height as usize;
        let width = geometry.reading.width;
        self.viewport_columns = width;
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
        if self.overlay == Some(Overlay::Help) {
            self.help_scroll = self.help_scroll.min(
                wrap::dialog_rows(&self.help_text(), width)
                    .len()
                    .saturating_sub(1),
            );
        }
        self.finish_scroll = self
            .finish_scroll
            .min(self.finish_rows().len().saturating_sub(1));
        if let Some(text) = &self.request_panel {
            self.request_scroll = self
                .request_scroll
                .min(wrap::dialog_rows(text, width).len().saturating_sub(1));
        }
        if let Some(editor) = &mut self.source_editor
            && let Some(super::editor::EditorDialog::Comparison { text, offset, .. }) =
                &mut editor.dialog
        {
            *offset = (*offset).min(wrap::dialog_rows(text, width).len().saturating_sub(1));
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

    /// Answers one terminal event. Source saves, checkpoints, requests,
    /// reviews, and restorations use the shared application operations.
    /// Navigation changes only the displayed context.
    ///
    /// The event also feeds the redraw signal of the reduced-update mode
    /// (SPEC 11.4): one fingerprint of the frame-visible state is taken
    /// before and after, and any difference raises it. A resize raises it
    /// unconditionally, because the rows rewrap when the next frame
    /// prepares them.
    pub fn handle_event(&mut self, event: &Event) {
        if super::pointer::ignored(event) {
            return;
        }
        if self.overlay == Some(Overlay::Help) {
            let before = self.fingerprint();
            if self.view == View::Changes && matches!(event, Event::Key(_)) {
                self.observe_changes_head();
            }
            match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_help_key(key),
                Event::Mouse(mouse)
                    if matches!(
                        mouse.kind,
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                    ) =>
                {
                    let key = KeyEvent::new(
                        if mouse.kind == MouseEventKind::ScrollUp {
                            KeyCode::Up
                        } else {
                            KeyCode::Down
                        },
                        KeyModifiers::NONE,
                    );
                    self.handle_help_key(&key);
                }
                Event::Paste(text) if self.help_filtering => {
                    self.help_query.extend(
                        text.chars()
                            .filter(|c| !c.is_control())
                            .take(FIELD_CHARS.saturating_sub(self.help_query.chars().count())),
                    );
                    self.help_scroll = 0;
                }
                _ => {}
            }
            if self.fingerprint() != before || matches!(event, Event::Resize(_, _)) {
                self.redraw = true;
                self.clear_pointer_targets();
            }
            return;
        }
        if self.request_panel.is_some() {
            if let Event::Key(key) = event {
                if scroll_key(
                    key,
                    &mut self.request_scroll,
                    self.viewport_rows,
                    wrap::dialog_rows(
                        self.request_panel.as_deref().unwrap_or(""),
                        self.viewport_columns.max(1),
                    )
                    .len(),
                ) {
                } else if key.code == KeyCode::Esc {
                    self.request_panel = None;
                } else if key.code == KeyCode::Char('q')
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL))
                {
                    self.request_panel = None;
                    self.request_quit(false);
                }
            }
            self.redraw = true;
            return;
        }
        if self.agent_prompt.is_some() {
            self.handle_agent_event(event);
            self.redraw = true;
            self.clear_pointer_targets();
            return;
        }
        if self.source_editor.is_some() {
            self.source_editor.as_mut().unwrap().rejected = false;
            match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_source_key(key),
                Event::Mouse(mouse)
                    if matches!(
                        mouse.kind,
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                    ) && matches!(
                        self.source_editor.as_ref().unwrap().dialog,
                        Some(super::editor::EditorDialog::Comparison { .. })
                    ) =>
                {
                    if let Some(super::editor::EditorDialog::Comparison { text, offset, .. }) =
                        self.source_editor.as_mut().unwrap().dialog.as_mut()
                    {
                        let rows = wrap::dialog_rows(text, self.viewport_columns).len();
                        *offset = offset
                            .saturating_add_signed(if mouse.kind == MouseEventKind::ScrollUp {
                                -3
                            } else {
                                3
                            })
                            .min(rows.saturating_sub(1));
                    }
                }
                _ if self
                    .source_editor
                    .as_ref()
                    .is_some_and(|e| e.dialog.is_some()) => {}
                Event::Paste(text)
                    if !self
                        .source_editor
                        .as_ref()
                        .is_some_and(|e| e.dialog.is_some()) =>
                {
                    self.source_editor.as_mut().unwrap().insert(text);
                }
                Event::Mouse(mouse)
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right)) =>
                {
                    if self.park_source_editor() {
                        self.handle_click(MouseButton::Right, mouse.column, mouse.row);
                    }
                }
                Event::Mouse(mouse)
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) =>
                {
                    let target = self.pointer.borrow().at(mouse.column, mouse.row);
                    if let Some(super::pointer::Target::SourceOffset(offset)) = target {
                        if !self.source_editor.as_ref().unwrap().dialog.is_some() {
                            self.source_editor.as_mut().unwrap().place_cursor(
                                offset,
                                mouse.modifiers.contains(KeyModifiers::SHIFT),
                            );
                        }
                    } else if matches!(
                        target,
                        Some(
                            super::pointer::Target::NavigatorEntry(_)
                                | super::pointer::Target::ViewTab(_)
                        )
                    ) && self.park_source_editor()
                    {
                        self.handle_click(MouseButton::Left, mouse.column, mouse.row);
                        self.settle_preview();
                        if matches!(target, Some(super::pointer::Target::NavigatorEntry(_))) {
                            self.open_source_editor();
                        }
                    }
                }
                Event::Mouse(mouse)
                    if !self.source_editor.as_ref().unwrap().dialog.is_some()
                        && matches!(
                            mouse.kind,
                            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                        ) =>
                {
                    self.source_editor.as_mut().unwrap().scroll(
                        if mouse.kind == MouseEventKind::ScrollUp {
                            -1
                        } else {
                            1
                        },
                    );
                }
                _ => {}
            }
            if self.source_editor.as_ref().is_some_and(|e| e.rejected) {
                self.notice =
                    Some("Source exceeds the 1 MiB editing limit; input was not inserted".into());
            }
            self.redraw = true;
            self.clear_pointer_targets();
            return;
        }
        let before = self.fingerprint();
        match event {
            // A resize redraws on the next pass at the new size; it resets
            // no plan and mutates no content (SPEC 11.2).
            Event::Resize(_, _) => {
                self.clear_pointer_targets();
                self.redraw = true;
            }
            // Bracketed paste is literal text in a text field and cannot
            // trigger a sequence of global actions; outside text entry it
            // is ignored with one concise notice (SPEC 11.3).
            Event::Paste(text) => self.handle_paste(text),
            Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key),
            // Pointer actions reuse keyboard navigation and confirmation rules.
            // Unsupported pointer events were filtered before fingerprinting.
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::Down(button) => self.handle_click(button, mouse.column, mouse.row),
                MouseEventKind::ScrollUp => self.handle_wheel(-1),
                MouseEventKind::ScrollDown => self.handle_wheel(1),
                _ => {}
            },
            _ => {}
        }
        if self.fingerprint() != before
            || self.agent_prompt.is_some()
            || self.source_editor.is_some()
            || self.request_panel.is_some()
        {
            self.clear_pointer_targets();
            self.redraw = true;
        }
    }

    pub fn clear_pointer_targets(&self) {
        self.pointer.borrow_mut().clear();
    }
    pub fn begin_pointer_targets(&self, area: ratatui::layout::Rect) {
        self.pointer.borrow_mut().begin(area);
    }
    pub fn record_pointer_target(
        &self,
        area: ratatui::layout::Rect,
        target: super::pointer::Target,
    ) {
        self.pointer.borrow_mut().record(area, target);
    }
    fn handle_click(&mut self, button: MouseButton, column: u16, row: u16) {
        use super::pointer::Target;
        if self.finish.is_some()
            || self.history_preview_stands()
            || self.overlay.as_ref().is_some_and(|o| {
                o.is_text_entry() && !matches!(o, Overlay::WorkspaceSearch | Overlay::Picker { .. })
            })
        {
            return;
        }
        if !self.pointer.borrow().contains(column, row) {
            return;
        }
        if button == MouseButton::Right {
            match self.overlay {
                Some(Overlay::Menu { .. }) => {
                    self.close_overlay("the action menu closed; nothing ran")
                }
                None => {
                    self.notice = None;
                    self.quit_warning = false;
                    self.open_menu();
                }
                _ => {}
            }
            return;
        }
        if button != MouseButton::Left {
            return;
        }
        let target = self.pointer.borrow().at(column, row);
        let Some(target) = target else {
            return;
        };
        let allowed = matches!(
            (&self.overlay, target),
            (None, _)
                | (
                    Some(Overlay::Menu { .. }),
                    Target::MenuEntry(_) | Target::Pane(Focus::Actions)
                )
                | (Some(Overlay::WorkspaceSearch), Target::SearchHit(_))
                | (Some(Overlay::Picker { .. }), Target::PickerEntry(_))
        );
        if !allowed {
            return;
        }
        self.notice = None;
        self.quit_warning = false;
        match (&self.overlay, target) {
            (Some(Overlay::Menu { .. }), Target::MenuEntry(index)) => {
                self.overlay = Some(Overlay::Menu { selection: index });
                self.activate_menu(index);
            }
            (Some(Overlay::Picker { filter }), Target::PickerEntry(index)) => {
                let filter = filter.clone();
                self.picker_selected = self
                    .picker_matches(&filter)
                    .get(index)
                    .map(|p| p.to_string());
                self.submit_picker(&filter);
            }
            (Some(Overlay::WorkspaceSearch), Target::SearchHit(index)) => {
                self.workspace_search.selection = index;
                self.submit_workspace_search();
            }
            (Some(Overlay::Menu { .. }), Target::Pane(Focus::Actions)) => {
                self.focus = Focus::Actions;
            }
            (Some(_), _) => {}
            (None, Target::NavigatorEntry(index)) => {
                self.focus = Focus::Documents;
                self.navigator_cursor = index;
                self.open_navigator_entry();
            }
            (None, Target::ViewTab(view)) => {
                self.handle_key(&KeyEvent::new(
                    KeyCode::Char(match view {
                        View::Read => '1',
                        View::Changes => '2',
                        View::History => '3',
                    }),
                    KeyModifiers::NONE,
                ));
            }
            (None, Target::ChangeGroup(index)) => {
                self.focus = Focus::Reading;
                if let Some(changes) = &mut self.changes {
                    changes.select(index);
                }
                self.observe_changes_head();
            }
            (None, Target::Revision(index)) => {
                self.focus = Focus::Reading;
                if let Some(history) = &mut self.history {
                    history.select(index);
                }
            }
            (None, Target::SourceLine(line)) if self.view == View::Read => {
                if let Some(document) = &self.document {
                    self.anchor = Some(SourceAnchor {
                        path: document.path.clone(),
                        line,
                    });
                }
                self.open_source_editor();
            }
            (None, Target::Pane(focus)) => {
                self.focus = focus;
            }
            _ => {}
        }
    }

    /// Remote navigation refuses unfinished local interactions and never
    /// changes the pinned comparison or the writer's review decisions.
    pub fn remote_busy(&self) -> bool {
        self.source_editor
            .as_ref()
            .is_some_and(|editor| editor.dialog.is_some())
            || self.agent_prompt.is_some()
            || self.request_panel.is_some()
            || self.overlay.is_some()
            || self.finish.is_some()
            || self.history_preview_stands()
            || self.pending_editor.is_some()
            || self.quit_warning
    }

    pub fn open_remote(&mut self, request: &crate::desk::Request) -> Result<(), String> {
        self.open_remote_until(request, None)
    }

    /// Validate completely before applying a still-live transport request.
    pub fn open_remote_until(
        &mut self,
        request: &crate::desk::Request,
        deadline: Option<std::time::Instant>,
    ) -> Result<(), String> {
        if self.remote_busy() {
            return Err("viewer busy; finish the input or confirmation and retry".into());
        }
        let request = crate::desk::Request {
            path: crate::domain::parse_document_path(&request.path).map_err(|e| e.to_string())?,
            line: request.line,
        };
        if request.line == Some(0) {
            return Err("line must be greater than zero".into());
        }
        let eligible = if let Some(descriptor) = &self.descriptor {
            let git = self.operations.git().clone().at(&self.root);
            let resolved = app::resolve_scope(&git, &descriptor.to_scope_input())
                .map_err(|e| e.to_string())?;
            document_paths(&resolved).contains(&request.path)
        } else {
            discover_documents(&self.root).0.contains(&request.path)
        };
        if !eligible {
            return Err("document is missing, unsupported, or outside the viewer scope".into());
        }
        // Refuse symbolic links at every component, including before adoption.
        let mut checked = self.root.clone();
        for part in Path::new(&request.path).components() {
            checked.push(part);
            let meta = fs::symlink_metadata(&checked).map_err(|e| e.to_string())?;
            if meta.file_type().is_symlink() {
                return Err("symbolic links cannot be opened".into());
            }
        }
        if !fs::metadata(&checked).map_err(|e| e.to_string())?.is_file() {
            return Err("document is not a regular file".into());
        }
        let canonical_root = fs::canonicalize(&self.root).map_err(|e| e.to_string())?;
        if !fs::canonicalize(&checked)
            .map_err(|e| e.to_string())?
            .starts_with(canonical_root)
        {
            return Err("document escapes the viewer root".into());
        }
        // Read before touching any visible state: failures preserve the current document.
        let working =
            read_working(&self.root, &request.path, READ_VIEW_BYTES).map_err(|e| e.to_string())?;
        let shown = self.show_candidate(&request.path);
        let (document, notice) = document_view(&request.path, &Ok(working), shown.as_ref());
        if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            return Err("opening request expired before navigation; retry".into());
        }
        if !self.park_source_editor() {
            return Err(
                "buffer retention limit reached; resolve an unsaved buffer before navigating"
                    .into(),
            );
        }
        self.save_position();
        self.pending_document = None;
        self.clear_pointer_targets();
        self.absorb_document(document);
        if self.history.is_some() {
            self.reload_history();
        }
        if let Some(index) = self.documents.iter().position(|p| p == &request.path) {
            self.selection = index;
        } else {
            self.documents.push(request.path.clone());
            self.documents.sort();
            self.selection = self
                .documents
                .iter()
                .position(|p| p == &request.path)
                .unwrap();
        }
        self.view = View::Read;
        self.focus = Focus::Reading;
        self.reading_source = request.line.is_some();
        self.reposition_navigator_cursor();
        self.anchor = Some(SourceAnchor {
            path: request.path.clone(),
            line: request
                .line
                .unwrap_or(1)
                .min(self.source_lines.len().max(1) as u32),
        });
        self.offset = 0;
        self.drift = 0;
        self.display_width = 0;
        self.notice = notice;
        self.regenerate();
        self.redraw = true;
        Ok(())
    }

    /// Absorbs one bracketed paste (SPEC 11.3).
    fn handle_paste(&mut self, pasted_text: &str) {
        if matches!(self.overlay, Some(Overlay::Menu { .. })) && self.menu_filtering {
            self.menu_query.extend(
                pasted_text
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(FIELD_CHARS.saturating_sub(self.menu_query.chars().count())),
            );
            self.overlay = Some(Overlay::Menu { selection: 0 });
            return;
        }
        let text_entry = self.overlay.as_ref().is_some_and(Overlay::is_text_entry);
        if !text_entry {
            self.notice = Some("pasted text is ignored outside text entry".to_string());
            return;
        }
        let pasted: String = pasted_text.chars().take(FIELD_CHARS).collect();
        match self.overlay.as_mut() {
            Some(Overlay::Picker { filter }) => {
                filter.extend(
                    pasted
                        .chars()
                        .take(FIELD_CHARS.saturating_sub(filter.chars().count())),
                );
            }
            Some(Overlay::Search { query }) => {
                query.extend(
                    pasted
                        .chars()
                        .take(FIELD_CHARS.saturating_sub(query.chars().count())),
                );
            }
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
        if let Some(Overlay::Picker { filter }) = &self.overlay {
            let paths: Vec<String> = self
                .picker_matches(filter)
                .into_iter()
                .map(str::to_owned)
                .collect();
            if !paths
                .iter()
                .any(|p| Some(p) == self.picker_selected.as_ref())
            {
                self.picker_selected = paths.first().cloned();
            }
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
                (KeyCode::Char(']'), false) => {
                    self.cancel_finish();
                    self.next_undecided();
                }
                _ if scroll_key(
                    key,
                    &mut self.finish_scroll,
                    self.viewport_rows,
                    self.finish.as_ref().map_or(0, |p| p.rows.len()),
                ) => {}
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

    /// Answers one wheel notch with the same movement the focused element
    /// makes for `j` and `k` (SPEC 11.4): the wheel is a redundant path
    /// and adds no action the keyboard cannot reach. One notch moves a
    /// free-scrolling column `WHEEL_ROWS` rows and one list entry.
    fn handle_wheel(&mut self, direction: i32) {
        self.notice = None;
        self.quit_warning = false;
        if self.finish.is_some() {
            self.finish_scroll = self
                .finish_scroll
                .saturating_add_signed(direction as isize * 3)
                .min(self.finish_rows().len().saturating_sub(1));
            return;
        }
        if self.view == View::History && self.history_preview_stands() {
            self.scroll_history(direction * WHEEL_ROWS as i32);
        } else if let Some(overlay) = self.overlay.clone() {
            match overlay {
                Overlay::Menu { selection } => {
                    let count = self.visible_menu().len().max(1);
                    let selection = if direction > 0 {
                        (selection + 1).min(count - 1)
                    } else {
                        selection.saturating_sub(1)
                    };
                    self.overlay = Some(Overlay::Menu { selection });
                }
                Overlay::WorkspaceSearch => {
                    let last = self
                        .workspace_search
                        .results
                        .as_ref()
                        .map_or(0, |results| results.hits.len().saturating_sub(1));
                    self.workspace_search.selection = if direction > 0 {
                        (self.workspace_search.selection + 1).min(last)
                    } else {
                        self.workspace_search.selection.saturating_sub(1)
                    };
                }
                Overlay::Picker { filter } => {
                    let paths: Vec<String> = self
                        .picker_matches(&filter)
                        .into_iter()
                        .map(str::to_owned)
                        .collect();
                    let index = self
                        .picker_index(&paths)
                        .saturating_add_signed(direction as isize)
                        .min(paths.len().saturating_sub(1));
                    self.picker_selected = paths.get(index).cloned();
                }
                // Help handles its own scroll events; ordinary fields ignore the wheel.
                _ => {}
            }
        } else {
            match self.focus {
                Focus::Documents => self.step_navigator(direction),
                Focus::Reading | Focus::Actions => match self.view {
                    View::Changes => {
                        if let Some(changes) = &mut self.changes {
                            changes.scroll_by(direction * WHEEL_ROWS as i32);
                        }
                    }
                    View::History => {
                        // The timeline is one list: the wheel moves the
                        // selected revision, as `j` and `k` do (SPEC 11.3).
                        if let Some(history) = &mut self.history {
                            history.step_selection(direction);
                        }
                    }
                    View::Read => {
                        if direction > 0 {
                            self.offset = self.offset.saturating_add(WHEEL_ROWS);
                        } else {
                            self.offset = self.offset.saturating_sub(WHEEL_ROWS);
                        }
                        self.clamp_offset();
                        self.sync_anchor();
                    }
                },
            }
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
                let entries = self.visible_menu();
                let count = entries.len().max(1);
                let labels: Vec<String> = entries.iter().map(|entry| entry.line(false)).collect();
                if let Some(index) = choice_move(
                    key,
                    selection,
                    &labels,
                    self.viewport_columns,
                    self.viewport_rows.saturating_sub(4),
                ) {
                    self.overlay = Some(Overlay::Menu { selection: index });
                    return;
                }
                if self.menu_filtering {
                    if control && key.code == KeyCode::Char('c') {
                        self.request_quit(quit_warning_stood);
                        return;
                    }
                    match key.code {
                        KeyCode::Esc => self.menu_filtering = false,
                        KeyCode::Enter => self.activate_menu(selection),
                        KeyCode::Backspace => {
                            self.menu_query.pop();
                        }
                        KeyCode::Char(c)
                            if !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                                && self.menu_query.chars().count() < FIELD_CHARS =>
                        {
                            self.menu_query.push(c)
                        }
                        _ => {}
                    }
                    if self.overlay.is_some() {
                        let action = entries.get(selection).map(|e| e.action);
                        let selection = self
                            .visible_menu()
                            .iter()
                            .position(|e| Some(e.action) == action)
                            .unwrap_or(0);
                        self.overlay = Some(Overlay::Menu { selection });
                    }
                    return;
                }
                match (key.code, control) {
                    (KeyCode::Char('/'), false) => self.menu_filtering = true,
                    (KeyCode::Char('c'), true) | (KeyCode::Char('q'), false) => {
                        self.overlay = Some(Overlay::Menu { selection });
                        self.request_quit(quit_warning_stood);
                    }
                    (KeyCode::Esc, false) | (KeyCode::Char(':'), false) => {
                        self.close_overlay("the action menu closed; nothing ran");
                    }
                    (KeyCode::Tab, _) => {
                        self.overlay = Some(Overlay::Menu { selection });
                        self.cycle_focus(true);
                    }
                    (KeyCode::BackTab, _) => {
                        self.overlay = Some(Overlay::Menu { selection });
                        self.cycle_focus(false);
                    }
                    (KeyCode::Char('j'), false) | (KeyCode::Down, false) => {
                        let selection = (selection + 1).min(count - 1);
                        self.overlay = Some(Overlay::Menu { selection });
                    }
                    (KeyCode::Char('k'), false) | (KeyCode::Up, false) => {
                        let selection = selection.saturating_sub(1);
                        self.overlay = Some(Overlay::Menu { selection });
                    }
                    (KeyCode::Char('?'), false) => {
                        self.open_help();
                    }
                    (KeyCode::Enter, false) => self.activate_menu(selection),
                    _ => {
                        self.overlay = Some(Overlay::Menu { selection });
                        self.notice =
                            Some("j/k choose · Enter runs · Esc closes the menu".to_string());
                    }
                }
            }
            Overlay::Help => self.handle_help_key(key),
            Overlay::Picker { mut filter } => {
                let paths: Vec<String> = self
                    .picker_matches(&filter)
                    .into_iter()
                    .map(str::to_owned)
                    .collect();
                let labels: Vec<String> = paths
                    .iter()
                    .map(|p| format!("  {}", crate::cli::display_escape(p)))
                    .collect();
                if let Some(index) = choice_move(
                    key,
                    self.picker_index(&paths),
                    &labels,
                    self.viewport_columns,
                    self.viewport_rows.saturating_sub(4),
                ) {
                    self.picker_selected = paths.get(index).cloned();
                } else if key.code == KeyCode::Enter {
                    self.submit_picker(&filter);
                } else if key.code == KeyCode::Esc {
                    self.close_overlay("the document picker closed");
                } else {
                    self.handle_field_key(&mut filter, key, control, quit_warning_stood);
                    let paths: Vec<String> = self
                        .picker_matches(&filter)
                        .into_iter()
                        .map(str::to_owned)
                        .collect();
                    if !paths
                        .iter()
                        .any(|p| Some(p) == self.picker_selected.as_ref())
                    {
                        self.picker_selected = paths.first().cloned();
                    }
                    self.overlay = Some(Overlay::Picker { filter });
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
            self.focus = Focus::Reading;
        }
        self.notice = Some(said.to_string());
    }

    /// Opens the action menu (SPEC 11.3 `:`): every action, its binding,
    /// and the reason a disabled action cannot run.
    fn open_menu(&mut self) {
        self.menu_query.clear();
        self.menu_filtering = false;
        self.overlay = Some(Overlay::Menu { selection: 0 });
        self.focus = Focus::Actions;
    }

    pub(crate) fn picker_index(&self, paths: &[String]) -> usize {
        paths
            .iter()
            .position(|p| Some(p) == self.picker_selected.as_ref())
            .unwrap_or(0)
    }

    pub(crate) fn visible_menu(&self) -> Vec<MenuEntry> {
        let query = self.menu_query.to_lowercase();
        self.menu()
            .into_iter()
            .filter(|entry| {
                entry.action.executable()
                    && format!(
                        "{} {} {}",
                        entry.action.group().1,
                        entry.action.name(),
                        entry.action.binding()
                    )
                    .to_lowercase()
                    .contains(&query)
            })
            .collect()
    }

    fn open_help(&mut self) {
        self.help_return = self.overlay.take();
        self.help_scroll = 0;
        self.help_query.clear();
        self.help_filtering = false;
        self.help_all = false;
        self.overlay = Some(Overlay::Help);
    }

    pub(crate) fn help_text(&self) -> String {
        let mut text = format!(
            "Margin keys · {:?}{}\n",
            self.view,
            if self.source_editor.is_some() {
                " · Editing"
            } else {
                ""
            }
        );
        text.push_str("/ search · a all shortcuts · arrows/PgUp/PgDn scroll · Esc closes help\n\nCurrent blockers\n");
        let blockers = self.blockers();
        if blockers.is_empty() {
            text.push_str("  none\n");
        }
        for blocker in blockers {
            text.push_str(&format!("  {blocker}\n"));
        }
        if let Some(editor) = &self.source_editor
            && let Some(error) = &editor.autosave_error
        {
            text.push_str(&format!("  Autosave paused: {error}\n"));
        }
        text.push('\n');
        let query = self.help_query.to_lowercase();
        if self.source_editor.is_some() || self.help_all {
            for line in super::editor::help_text().lines() {
                if line.to_lowercase().contains(&query) {
                    text.push_str(line);
                    text.push('\n');
                }
            }
        }
        if self.source_editor.is_none() || self.help_all {
            for entry in self.action_entries() {
                if !self.help_all
                    && matches!(
                        entry.action,
                        Action::HistoryCheckpoints | Action::PreviewRestore
                    )
                    && self.view != View::History
                {
                    continue;
                }
                if !self.help_all
                    && matches!(
                        entry.action,
                        Action::NextGroup
                            | Action::PreviousGroup
                            | Action::NextUndecided
                            | Action::KeepGroup
                            | Action::RestoreGroup
                            | Action::UndoDecision
                            | Action::AddNote
                            | Action::FinishReview
                    )
                    && self.view != View::Changes
                {
                    continue;
                }
                let line = entry.line(false);
                if line.to_lowercase().contains(&query) {
                    text.push_str(&line);
                    text.push('\n');
                }
            }
            text.push_str("Left/right · Fold/expand outline when documents have focus\n");
        }
        text
    }

    fn handle_help_key(&mut self, key: &KeyEvent) {
        let quit_warning_stood = self.quit_warning;
        self.quit_warning = false;
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.source_editor.is_some() {
                self.overlay = self.help_return.take();
            } else {
                self.request_quit(quit_warning_stood);
            }
            return;
        }
        if self.help_filtering {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.help_filtering = false,
                KeyCode::Backspace => {
                    self.help_query.pop();
                    self.help_scroll = 0;
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && self.help_query.chars().count() < FIELD_CHARS =>
                {
                    self.help_query.push(c);
                    self.help_scroll = 0;
                }
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::F(1) => {
                    self.overlay = self.help_return.take();
                }
                KeyCode::Char('/') => self.help_filtering = true,
                KeyCode::Char('a') => {
                    self.help_all = !self.help_all;
                    self.help_scroll = 0;
                }
                KeyCode::Char('q') if self.source_editor.is_none() => {
                    self.request_quit(quit_warning_stood)
                }
                _ => {
                    let total = wrap::dialog_rows(&self.help_text(), self.viewport_columns).len();
                    scroll_key(key, &mut self.help_scroll, self.viewport_rows, total);
                }
            }
        }
    }

    fn next_undecided(&mut self) {
        if let Some(changes) = &mut self.changes {
            let count = changes.groups().len();
            let next = (1..=count)
                .map(|n| (changes.selection() + n) % count)
                .find(|i| changes.state_of(*i) == super::changes::GroupState::Undecided);
            if let Some(index) = next {
                changes.select(index);
            } else {
                self.notice = Some("No undecided groups remain in the loaded comparison".into());
            }
        }
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
        let paths: Vec<String> = self
            .picker_matches(filter)
            .into_iter()
            .map(str::to_owned)
            .collect();
        let path = paths.get(self.picker_index(&paths));
        if let Some(index) = path.and_then(|path| self.documents.iter().position(|p| p == path)) {
            self.navigate_document(index);
            self.notice = Some(format!("opening {}", self.documents[index]));
        } else {
            self.notice = Some(format!("no listed document matches {filter}"));
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
        // While one navigation target is still opening, the actions that
        // depend on the displayed content state their reason and run
        // nothing: a rejected action is not queued and never replays when
        // the target settles. Navigation, search, and cancellation stay
        // available.
        if let Some(action) = Action::from_key(key) {
            self.execute_action(action);
            return;
        }
        match (key.code, control) {
            (KeyCode::Char('c'), true) | (KeyCode::Char('q'), false) => {
                self.request_quit(quit_warning_stood)
            }
            (KeyCode::Left, false) if self.focus == Focus::Documents => {
                self.outline_folded = true;
                self.navigator_cursor = self.selection;
            }
            (KeyCode::Right, false) if self.focus == Focus::Documents => {
                self.outline_folded = false
            }
            (KeyCode::Enter, false) if self.focus == Focus::Documents => {
                self.open_navigator_entry();
                self.view = View::Read;
                self.focus = Focus::Reading;
            }
            (KeyCode::Enter, false) if self.view == View::Changes => {
                self.execute_action(Action::FinishReview)
            }
            (KeyCode::Enter, false) if self.view == View::History => {
                self.execute_action(Action::PreviewRestore)
            }
            (KeyCode::Tab, _) => self.cycle_focus(true),
            (KeyCode::BackTab, _) => self.cycle_focus(false),
            (KeyCode::Char('j'), false) | (KeyCode::Down, false) => self.step(1),
            (KeyCode::Char('k'), false) | (KeyCode::Up, false) => self.step(-1),
            (KeyCode::PageDown, false) => self.page(1),
            (KeyCode::PageUp, false) => self.page(-1),
            _ => {}
        }
    }

    fn execute_action(&mut self, action: Action) {
        if let Some(entry) = self.menu().into_iter().find(|entry| entry.action == action)
            && let Some(reason) = entry.disabled
        {
            self.notice = Some(reason);
            return;
        }
        self.activate(action);
    }

    /// The action menu this shell offers right now (SPEC 11.3): every
    /// action of the keyboard contract, each with the reason in words
    /// when the current context cannot run it.
    pub fn menu(&self) -> Vec<MenuEntry> {
        let mut entries: Vec<_> = self
            .action_entries()
            .into_iter()
            .filter(|e| e.action.executable())
            .collect();
        entries.sort_by_key(|e| e.action.group().0);
        entries
    }

    fn action_entries(&self) -> Vec<MenuEntry> {
        let changes_only = "the Changes view only; press 2 to switch";
        let in_changes = self.view == View::Changes;
        let groups = self
            .changes
            .as_ref()
            .map_or(0, |changes| changes.groups().len());
        let opening = self.pending_preview();
        Action::ALL
            .iter()
            .map(|&action| {
                let disabled = match action {
                    Action::PreviewRestore
                    | Action::UndoAgentEdit
                    | Action::KeepGroup
                    | Action::RestoreGroup
                    | Action::UndoDecision
                    | Action::AddNote
                    | Action::Details
                    | Action::FinishReview
                    | Action::EditDocument
                    | Action::ExternalEditor
                    | Action::AskAgent
                    | Action::CheckpointDraft
                        if opening.is_some() =>
                    {
                        opening.clone()
                    }
                    Action::NextGroup
                    | Action::PreviousGroup
                    | Action::NextUndecided
                    | Action::AddNote => {
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
                            self.changes.as_ref().map_or_else(
                                || Some("no comparison exists to decide".into()),
                                |changes| changes.refusal(),
                            )
                        }
                    }
                    Action::HistoryCheckpoints | Action::PreviewRestore
                        if self.view != View::History =>
                    {
                        Some("History view only; press 3 to switch".into())
                    }
                    Action::SourceView => {
                        if self.view == View::Read {
                            None
                        } else if !in_changes {
                            Some("Read or Changes view only".into())
                        } else if self.changes.is_none() {
                            Some("no comparison exists; source changes are unavailable".to_string())
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
                            self.changes.as_ref().map_or_else(
                                || Some("no comparison exists to decide".into()),
                                |changes| changes.refusal(),
                            )
                        }
                    }
                    Action::DocumentPicker | Action::WorkspaceSearch => (self.documents.is_empty())
                        .then(|| "no documents are listed to find".to_string()),
                    Action::EditDocument | Action::AskAgent => self
                        .document
                        .is_none()
                        .then(|| "no document is displayed".into()),
                    Action::ExternalEditor => {
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
                MenuEntry { action, disabled }
            })
            .collect()
    }

    /// Runs the selected entry of the action menu (SPEC 11.3): a disabled
    /// entry states its reason and runs nothing; an enabled entry closes
    /// the menu and runs the same handler its binding reaches.
    fn activate_menu(&mut self, selection: usize) {
        let entry = self.visible_menu().get(selection).cloned();
        match entry {
            Some(entry) => {
                if let Some(reason) = &entry.disabled {
                    self.overlay = Some(Overlay::Menu { selection });
                    self.notice = Some(reason.clone());
                    return;
                }
                let action = entry.action;
                if action == Action::Help {
                    self.open_help();
                    return;
                }
                self.overlay = None;
                if self.focus == Focus::Actions {
                    self.focus = Focus::Reading;
                }
                self.execute_action(action);
            }
            None => self.overlay = Some(Overlay::Menu { selection }),
        }
    }

    /// Runs one action, as the menu's `Enter` does (SPEC 11.3). Every
    /// branch is the handler of the action's default binding, so the
    /// menu and the keys cannot disagree.
    fn activate(&mut self, action: Action) {
        match action {
            Action::Refresh => self.refresh(),
            Action::HistoryCheckpoints => self.toggle_history_checkpoints(),
            Action::PreviewRestore => self.open_history_preview(),
            Action::NextUndecided => self.next_undecided(),
            Action::CycleFocus => self.cycle_focus(true),
            Action::ActionMenu => self.open_menu(),
            Action::ReadView => self.view = View::Read,
            Action::ChangesView => self.enter_changes(),
            Action::HistoryView => self.view = View::History,
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
            Action::Details => self.toggle_details(),
            Action::UndoDecision => self.undo_decision(),
            Action::AddNote => self.open_note(),
            Action::DocumentPicker => {
                self.picker_selected = self.document.as_ref().map(|d| d.path.clone());
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
            Action::EditDocument => self.open_source_editor(),
            Action::ExternalEditor => self.request_editor(),
            Action::AskAgent => self.open_agent_request(),
            Action::AgentRequests => self.open_request_panel(),
            Action::CheckpointDraft => self.checkpoint_edited_draft(),
            Action::UndoAgentEdit => self.undo_agent_edit(),
            Action::FinishReview => self.open_finish(),
            Action::Help => self.open_help(),
            Action::Quit => self.request_quit(false),
        }
    }

    fn checkpoint_edited_draft(&mut self) {
        if self
            .changes
            .as_ref()
            .is_some_and(|c| c.proposed_count() > 0)
        {
            self.notice =
                Some("Finish or clear your review decisions before checkpointing edits".into());
            return;
        }
        let outcome = self.operations.create_checkpoint(&CheckpointRequest {
            scope: None,
            message: None,
            include_staged: false,
            expect_head: None,
            expect_index: None,
            expect_working: None,
            dry_run: false,
        });
        if let Some(error) = outcome.error {
            self.notice = Some(format!("{}: {}", error.code, error.message));
            return;
        }
        if let Some(warning) = outcome.warnings.first() {
            self.notice = Some(warning.message.clone());
        }
        let notice = self.notice.take();
        self.refresh();
        self.enter_changes();
        self.notice = notice.or_else(|| Some("Checkpoint saved; changes await your review".into()));
    }
    fn checkpoint_boundary(&mut self) -> bool {
        let result = self.operations.create_checkpoint(&CheckpointRequest {
            scope: None,
            message: Some("Preserve an agent edit boundary".into()),
            include_staged: false,
            expect_head: None,
            expect_index: None,
            expect_working: None,
            dry_run: false,
        });
        if let Some(error) = result.error {
            self.history_pending = true;
            self.history_error = Some(format!(
                "File saved; version history paused: {}",
                error.message
            ));
            if error.code != "BUSY" {
                self.notice = self.history_error.clone();
            }
            return false;
        }
        self.history_pending = false;
        self.history_error = None;
        if self
            .notice
            .as_ref()
            .is_some_and(|notice| notice.starts_with("File saved; version history paused:"))
        {
            self.notice = None;
        }
        if let Some(warning) = result.warnings.first() {
            self.notice = Some(warning.message.clone());
        }
        if !self
            .changes
            .as_ref()
            .is_some_and(|changes| changes.proposed_count() > 0)
        {
            let notice = self.notice.take();
            self.refresh();
            self.notice = notice;
        }
        true
    }
    fn sync_editors(&mut self, path: &str) {
        let Ok(base) = self.operations.edit_document(path) else {
            return;
        };
        for editor in self
            .source_editor
            .iter_mut()
            .chain(self.retained_editors.iter_mut())
            .filter(|e| e.base.path == path && !e.dirty())
        {
            editor.accept_disk(base.clone());
        }
        if self.source_editor.is_none() && self.document.as_ref().is_some_and(|d| d.path == path) {
            self.reload_document();
        }
    }
    fn undo_agent_edit(&mut self) {
        if self.source_editor.as_ref().is_some_and(|e| e.dirty())
            && !self.save_source_editor_with_policy(false)
        {
            return;
        }
        let Some(path) = self
            .source_editor
            .as_ref()
            .map(|e| e.base.path.clone())
            .or_else(|| self.document.as_ref().map(|d| d.path.clone()))
        else {
            return;
        };
        let request = self
            .operations
            .list_edit_requests()
            .ok()
            .and_then(|requests| {
                requests
                    .into_iter()
                    .rev()
                    .find(|r| r.base.path == path && r.state == crate::requests::State::Applied)
            });
        let Some(request) = request else {
            self.notice = Some("No applied agent edit exists for this document".into());
            return;
        };
        match self.operations.undo_edit_result(&request.id) {
            Ok(_) => {
                self.sync_editors(&path);
                if self.checkpoint_boundary() {
                    let result = self.operations.append_note(&crate::notes::AppendRequest {
                        commit: None,
                        group: None,
                        by: crate::notes::Author::Writer,
                        text: format!(
                            "Undid agent request {}. Keep the preceding wording.",
                            request.id
                        ),
                    });
                    self.notice = Some(if let Some(error) = result.error {
                        format!(
                            "Agent edit undone; feedback remains in the request: {}",
                            error.message
                        )
                    } else {
                        "Agent edit undone; your preceding text is restored".into()
                    });
                }
            }
            Err(error) => self.notice = Some(error.message),
        }
        self.redraw = true;
    }
    fn resolve_agent_conflict(&mut self, use_agent: bool) {
        let Some(editor) = &self.source_editor else {
            return;
        };
        let Some(id) = editor.conflicting_request.clone() else {
            self.notice = Some("No agent conflict is selected".into());
            return;
        };
        if use_agent && editor.dirty() {
            self.notice = Some("Compare your latest text before choosing the agent version".into());
            return;
        }
        let result = if use_agent {
            self.operations.choose_agent_result(&id, &editor.base)
        } else {
            self.operations.cancel_edit_request(&id)
        };
        match result {
            Ok(result) => {
                if use_agent && result.state != crate::requests::State::Applied {
                    self.notice = Some("The changed passage cannot be isolated safely. Both versions remain available for manual comparison.".into());
                    return;
                }
                self.sync_editors(&result.base.path);
                if let Some(editor) = &mut self.source_editor {
                    editor.conflicting_request = None;
                    editor.conflict = None;
                    editor.dialog = None;
                }
                if use_agent {
                    self.checkpoint_boundary();
                }
                self.notice = Some(
                    if use_agent {
                        "Agent version applied; Ctrl+U undoes it"
                    } else {
                        "Your text kept; the agent proposal remains in the cancelled request"
                    }
                    .into(),
                );
            }
            Err(error) => self.notice = Some(error.message),
        }
    }
    pub fn request_panel(&self) -> Option<&str> {
        self.request_panel.as_deref()
    }
    fn open_request_panel(&mut self) {
        self.request_scroll = 0;
        self.request_panel = Some(match self.operations.list_edit_requests() {
            Ok(requests) => {
                let mut text = "Agent requests · Esc closes\nUse margin requests inspect ID --json for full context.\n".to_string();
                for r in requests.into_iter().rev() {
                    let passage: String = r.base.text[r.start..r.end].chars().take(160).collect();
                    text.push_str(&format!(
                        "\n{} · {} · {}\nPassage: {}\nYou: {}\nAgent: {}\n{}\n",
                        r.base.path,
                        r.state.label(),
                        r.id,
                        passage,
                        r.instruction,
                        r.explanation.unwrap_or_default(),
                        r.error.unwrap_or_default()
                    ));
                }
                text
            }
            Err(e) => e.message,
        });
    }
    pub fn agent_prompt(&self) -> Option<(&str, &str)> {
        self.agent_prompt
            .as_ref()
            .map(|(base, _, text)| (base.path.as_str(), text.as_str()))
    }
    fn open_agent_request(&mut self) {
        self.finish_autosave();
        if let Some(reason) = self.pending_preview() {
            self.notice = Some(reason);
            return;
        }
        if self.source_editor.as_ref().is_some_and(|e| e.dirty())
            && !self.save_source_editor_with_policy(false)
        {
            return;
        }
        let (base, span) = if let Some(editor) = &self.source_editor {
            let range = editor.selection();
            (editor.base.clone(), (!range.is_empty()).then_some(range))
        } else {
            let Some(path) = self.document.as_ref().map(|d| d.path.clone()) else {
                return;
            };
            if let Some(editor) = self
                .retained_editors
                .iter_mut()
                .find(|e| e.base.path == path && e.dirty())
            {
                match self.operations.save_document(&editor.base, &editor.text) {
                    Ok(base) => {
                        editor.saved(base);
                        let _ = self.operations.forget_edit_buffer(&editor.recovery_id);
                    }
                    Err(error) => {
                        editor.autosave_error = Some(error.message.clone());
                        self.notice = Some(error.message);
                        return;
                    }
                }
            }
            match self.operations.edit_document(&path) {
                Ok(base) => (base, None),
                Err(e) => {
                    self.notice = Some(e.message);
                    return;
                }
            }
        };
        self.agent_prompt = Some((base, span, String::new()));
        self.overlay = None;
        self.notice = None;
    }
    fn handle_agent_event(&mut self, event: &Event) {
        match event {
            Event::Paste(text) => {
                let prompt = &mut self.agent_prompt.as_mut().unwrap().2;
                for c in text.chars() {
                    if prompt.len() + c.len_utf8() > 8000 {
                        break;
                    }
                    prompt.push(c);
                }
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc => {
                    self.agent_prompt = None;
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.agent_prompt = None;
                }
                KeyCode::Enter => {
                    self.checkpoint_boundary();
                    let (base, span, text) = self.agent_prompt.as_ref().unwrap();
                    match self
                        .operations
                        .create_edit_request(base.clone(), span.clone(), text)
                    {
                        Ok(request) => {
                            self.agent_prompt = None;
                            self.notice = Some(format!(
                                "Request {} queued; Claude receives it on the next prompt; other agents use margin requests list --json",
                                request.id
                            ));
                        }
                        Err(e) => self.notice = Some(e.message),
                    }
                }
                KeyCode::Backspace => {
                    self.agent_prompt.as_mut().unwrap().2.pop();
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && !c.is_control() =>
                {
                    let prompt = &mut self.agent_prompt.as_mut().unwrap().2;
                    if prompt.len() + c.len_utf8() <= 8000 {
                        prompt.push(c);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    pub fn notify_edit_requests(&mut self) {
        self.request_poll = std::time::Instant::now() - std::time::Duration::from_secs(2);
    }
    pub fn poll_edit_requests(&mut self) {
        self.poll_autosave_at(std::time::Instant::now());
        if self.recovery_poll.elapsed() >= std::time::Duration::from_secs(2) {
            self.recovery_poll = std::time::Instant::now();
            if let Some(editor) = &self.source_editor
                && editor.dirty()
                && let Err(e) = self.operations.retain_edit_buffer(
                    &editor.recovery_id,
                    &editor.base,
                    &editor.text,
                )
            {
                self.notice = Some(format!("Recovery buffer could not be saved: {}", e.message));
                self.redraw = true;
            }
        }
        if self.request_poll.elapsed() < std::time::Duration::from_secs(2) {
            return;
        }
        self.request_poll = std::time::Instant::now();
        let Ok(requests) = self.operations.list_edit_requests() else {
            return;
        };
        let desk = std::env::var("MARGIN_DESK_SOCKET").ok();
        let owned = requests
            .iter()
            .filter(|r| r.desk == desk || r.desk.is_none());
        let pending = owned
            .clone()
            .filter(|r| {
                matches!(
                    r.state,
                    crate::requests::State::Queued
                        | crate::requests::State::Claimed
                        | crate::requests::State::ResultReady
                )
            })
            .count();
        let attention = owned
            .filter(|r| {
                matches!(
                    r.state,
                    crate::requests::State::Stale
                        | crate::requests::State::Failed
                        | crate::requests::State::Applying
                        | crate::requests::State::Undoing
                )
            })
            .count();
        let status = if pending + attention > 0 {
            format!("Agent: {pending} pending · {attention} need attention")
        } else {
            String::new()
        };
        self.redraw |= status != self.agent_status;
        self.agent_status = status;
        if self.remote_busy()
            || self.pending_document.is_some()
            || self
                .changes
                .as_ref()
                .is_some_and(|c| c.proposed_count() > 0)
        {
            return;
        }
        if self.history_pending && !self.autosave_pending() {
            self.checkpoint_boundary();
        }
        for editor in self
            .source_editor
            .iter_mut()
            .chain(self.retained_editors.iter_mut())
        {
            let latest = requests
                .iter()
                .rev()
                .find(|r| r.base.path == editor.base.path && (r.desk == desk || r.desk.is_none()));
            let previous_activity = editor.activity.clone();
            editor.activity = latest.and_then(|r| match r.state {
                crate::requests::State::Queued => {
                    Some("Request queued · delivered on the next agent turn".into())
                }
                crate::requests::State::Claimed => Some("Agent working on your request".into()),
                crate::requests::State::ResultReady => {
                    Some("Agent result waiting for your typing pause".into())
                }
                crate::requests::State::Applied => Some("Agent edit applied · Ctrl+U undo".into()),
                crate::requests::State::Stale => Some("Agent result needs comparison".into()),
                _ => None,
            });
            self.redraw |= editor.activity != previous_activity;
            if let Some(request) = latest.filter(|r| {
                r.state == crate::requests::State::Stale
                    && editor.conflicting_request.as_deref() != Some(r.id.as_str())
            }) {
                editor.conflicting_request = Some(request.id.clone());
                editor.conflict = Some(format!(
                    "Agent proposal overlaps your changes. Your text is retained above.\nF3: use agent version · F4: keep mine\nInstruction: {}\nAgent proposal:\n{}",
                    request.instruction,
                    request.result.as_deref().unwrap_or_default()
                ));
            }
        }
        if let Some(request) = requests.into_iter().find(|r| {
            r.state == crate::requests::State::ResultReady
                && (r.desk == desk || r.desk.is_none())
                && !self
                    .source_editor
                    .iter()
                    .chain(self.retained_editors.iter())
                    .any(|e| e.base.path == r.base.path && e.dirty())
        }) {
            match self.operations.apply_edit_result(&request.id) {
                Ok(result) => {
                    self.sync_editors(&result.base.path);
                    if result.state == crate::requests::State::Applied {
                        let preserved = self.checkpoint_boundary();
                        if preserved {
                            self.notice = Some(format!(
                                "Agent edit applied to {}; version preserved · U / Ctrl+U undo · {}",
                                result.base.path,
                                result.explanation.unwrap_or_default()
                            ));
                        }
                    } else {
                        self.notice = Some(format!(
                            "Agent result retained for comparison: {}",
                            result.error.unwrap_or_default()
                        ));
                    }
                    self.redraw = true;
                    self.notify_edit_requests();
                }
                Err(error) if error.code == "BUSY" => {
                    self.request_poll =
                        std::time::Instant::now() - std::time::Duration::from_millis(1800);
                }
                Err(error) => {
                    self.notice = Some(error.message);
                    self.redraw = true;
                }
            }
        }
    }

    /// Keep buffers across reading, document switches, and external refreshes.
    fn park_source_editor(&mut self) -> bool {
        if self.source_editor.is_none() {
            return true;
        }
        if self.retained_editors.len() >= 16 {
            if let Some(index) = self.retained_editors.iter().position(|e| !e.dirty()) {
                self.retained_editors.remove(index);
            } else {
                self.notice = Some(
                    "16 unsaved buffers are retained; resolve a paused save before opening another"
                        .into(),
                );
                return false;
            }
        }
        let editor = self.source_editor.take().unwrap();
        self.anchor = Some(SourceAnchor {
            path: editor.base.path.clone(),
            line: editor.position().0 as u32 + 1,
        });
        self.retained_editors.push(editor);
        true
    }
    pub fn history_notice(&self) -> Option<&str> {
        self.history_error.as_deref()
    }
    pub fn autosave_notice(&self) -> Option<String> {
        self.source_editor
            .iter()
            .chain(self.retained_editors.iter())
            .find_map(|e| {
                e.autosave_error.as_ref().map(|error| {
                    format!(
                        "Autosave paused: {} · {} · reopen to compare or retry",
                        e.base.path, error
                    )
                })
            })
    }
    pub fn autosave_wait(&self, now: std::time::Instant) -> Option<std::time::Duration> {
        if self.autosave_worker.is_some() {
            return Some(std::time::Duration::from_millis(20));
        }
        self.source_editor
            .iter()
            .chain(self.retained_editors.iter())
            .filter(|e| e.dirty() && e.autosave_error.is_none())
            .map(|e| (e.last_edit + super::editor::AUTOSAVE_DELAY).saturating_duration_since(now))
            .min()
    }
    pub fn autosave_pending(&self) -> bool {
        self.autosave_worker.is_some()
            || self
                .source_editor
                .iter()
                .chain(self.retained_editors.iter())
                .any(|e| e.dirty() && e.autosave_error.is_none())
    }
    fn absorb_autosave(&mut self, reply: super::editor::SaveReply) {
        let Some(editor) = self
            .source_editor
            .iter_mut()
            .chain(self.retained_editors.iter_mut())
            .find(|e| e.recovery_id == reply.recovery_id && e.base == reply.base)
        else {
            return;
        };
        let path = editor.base.path.clone();
        match reply.result {
            Ok(base) => {
                editor.saved(base);
                if !editor.dirty() {
                    let _ = self.operations.forget_edit_buffer(&editor.recovery_id);
                }
            }
            Err(error) if error.code == "BUSY" => {
                editor.last_edit =
                    std::time::Instant::now() - std::time::Duration::from_millis(550);
            }
            Err(error) => {
                editor.autosave_error = Some(error.message.clone());
                editor.conflict = Some(error.message);
            }
        }
        if self.source_editor.is_none() && self.document.as_ref().is_some_and(|d| d.path == path) {
            self.reload_document();
        }
        self.redraw = true;
    }
    fn finish_autosave(&mut self) {
        if let Some(worker) = self.autosave_worker.take()
            && let Some(reply) = worker.finish()
        {
            self.absorb_autosave(reply);
        }
    }
    /// Debounce uses an explicit clock; the file operation runs off the input thread.
    pub fn poll_autosave_at(&mut self, now: std::time::Instant) {
        let reply = self
            .autosave_worker
            .as_ref()
            .and_then(|worker| worker.receive());
        if let Some(reply) = reply {
            self.autosave_worker = None;
            self.absorb_autosave(reply);
        }
        if self.autosave_worker.is_some() {
            return;
        }
        if let Some(editor) = self
            .source_editor
            .iter()
            .chain(self.retained_editors.iter())
            .find(|e| e.autosave_due(now))
        {
            self.autosave_worker = Some(super::editor::SaveWorker::start(
                self.operations.git().clone(),
                editor,
            ));
            self.redraw = true;
        }
    }
    fn flush_buffers(&mut self) -> bool {
        self.finish_autosave();
        let mut ok = true;
        if self.source_editor.as_ref().is_some_and(|e| e.dirty()) {
            ok &= self.save_source_editor_with_policy(false);
        }
        for editor in &mut self.retained_editors {
            if !editor.dirty() {
                continue;
            }
            match self.operations.save_document(&editor.base, &editor.text) {
                Ok(base) => {
                    editor.saved(base);
                    let _ = self.operations.forget_edit_buffer(&editor.recovery_id);
                }
                Err(error) => {
                    editor.autosave_error = Some(error.message.clone());
                    editor.conflict = Some(error.message);
                    ok = false;
                }
            }
        }
        ok
    }
    pub fn source_editor(&self) -> Option<&super::editor::Editor> {
        self.source_editor.as_ref()
    }

    fn open_source_editor(&mut self) {
        if let Some(reason) = self.pending_preview() {
            self.notice = Some(reason);
            return;
        }
        let Some(path) = self.document.as_ref().map(|d| d.path.clone()) else {
            return;
        };
        if let Some(index) = self
            .retained_editors
            .iter()
            .position(|e| e.base.path == path)
        {
            let mut editor = self.retained_editors.remove(index);
            if !editor.dirty()
                && let Ok(base) = self.operations.edit_document(&path)
            {
                editor.accept_disk(base);
            }
            self.source_editor = Some(editor);
            self.pending_document = None;
            self.overlay = None;
            self.focus = Focus::Reading;
            self.view = View::Read;
            self.redraw = true;
            return;
        }
        match self.operations.edit_document(&path) {
            Ok(base) => {
                self.pending_document = None;
                let recovery = self.operations.recover_edit_buffer(&base.path);
                let recovery_notice = match recovery {
                    Ok(Some(_)) => Some("A recovery buffer exists. Ctrl+R loads it explicitly; your file stays unchanged.".to_string()),
                    Ok(None) => None,
                    Err(error) => Some(format!("Recovery could not be inspected: {}", error.message)),
                };
                let mut editor = super::editor::Editor::new(base);
                if let Some(anchor) = &self.anchor
                    && anchor.path == path
                    && anchor.line > 1
                {
                    editor.cursor = editor
                        .text
                        .match_indices('\n')
                        .nth(anchor.line as usize - 2)
                        .map_or(editor.text.len(), |(i, _)| i + 1);
                }
                editor.conflict = recovery_notice;
                self.source_editor = Some(editor);
                self.overlay = None;
                self.focus = Focus::Reading;
                self.view = View::Read;
                self.notice = Some(
                    "Edit source · Autosave on · Esc read · Shift+arrows select · F2 ask agent"
                        .into(),
                );
            }
            Err(e) => self.notice = Some(format!("{}: {}", e.code, e.message)),
        }
    }
    fn save_source_editor(&mut self) -> bool {
        self.save_source_editor_with_policy(true)
    }
    fn save_source_editor_with_policy(&mut self, capture: bool) -> bool {
        self.finish_autosave();
        let Some(editor) = self.source_editor.as_ref() else {
            return false;
        };
        match self.operations.save_document(&editor.base, &editor.text) {
            Ok(base) => {
                let editor = self.source_editor.as_mut().unwrap();
                editor.saved(base);
                let _ = self.operations.forget_edit_buffer(&editor.recovery_id);
                self.notice = Some("Saved; changes remain unreviewed".into());
                if capture && self.capture_after_edit {
                    let outcome = self.operations.create_checkpoint(&CheckpointRequest {
                        scope: None,
                        message: None,
                        include_staged: false,
                        expect_head: None,
                        expect_index: None,
                        expect_working: None,
                        dry_run: false,
                    });
                    if let Some(e) = outcome.error {
                        self.notice = Some(format!("Saved; checkpoint failed: {}", e.message));
                    }
                }
                true
            }
            Err(e) => {
                let editor = self.source_editor.as_mut().unwrap();
                editor.conflict = Some(e.message.clone());
                editor.autosave_error = Some(e.message.clone());
                self.notice = Some(format!(
                    "{}: {} · Ctrl+D compares disk; Esc keeps your buffer",
                    e.code, e.message
                ));
                false
            }
        }
    }
    fn handle_source_key(&mut self, key: &KeyEvent) {
        let quit_warning_stood = self.quit_warning;
        self.quit_warning = false;
        self.notice = None;
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(super::editor::EditorDialog::Comparison { kind, text, offset }) =
            self.source_editor.as_mut().unwrap().dialog.as_mut()
        {
            let total = wrap::dialog_rows(text, self.viewport_columns).len();
            if scroll_key(key, offset, self.viewport_rows, total) {
                return;
            }
            match (key.code, control) {
                (KeyCode::Esc, _) | (KeyCode::Char('c'), true) => {
                    self.source_editor.as_mut().unwrap().dialog = None;
                    return;
                }
                (KeyCode::Char('b'), true) if *kind == super::editor::ComparisonKind::Disk => {}
                (KeyCode::F(3) | KeyCode::F(4), _)
                    if *kind == super::editor::ComparisonKind::Agent => {}
                (KeyCode::Char('d'), true) | (KeyCode::F(1) | KeyCode::F(5), _) => {}
                _ => return,
            }
        }
        if matches!(
            self.source_editor.as_ref().unwrap().dialog,
            Some(super::editor::EditorDialog::SaveChoices)
        ) {
            match key.code {
                KeyCode::Char('s') => {
                    if self.save_source_editor() {
                        let notice = self.notice.take();
                        self.source_editor = None;
                        self.refresh();
                        self.notice = notice;
                    }
                }
                KeyCode::Char('d') => {
                    let id = self.source_editor.as_ref().unwrap().recovery_id.clone();
                    let _ = self.operations.forget_edit_buffer(&id);
                    self.source_editor = None;
                    self.refresh();
                    self.notice = Some("Buffer discarded".into());
                }
                KeyCode::Esc | KeyCode::Char('c') => {
                    self.source_editor.as_mut().unwrap().dialog = None;
                }
                _ => {}
            }
            return;
        }
        use super::actions::EditorAction;
        match EditorAction::from_key(key) {
            Some(EditorAction::Ask) => self.open_agent_request(),
            Some(EditorAction::UseProposal) => self.resolve_agent_conflict(true),
            Some(EditorAction::KeepText) => self.resolve_agent_conflict(false),
            Some(EditorAction::Details) => {
                self.finish_autosave();
                let editor = self.source_editor.as_mut().unwrap();
                if let Some(text) = &editor.conflict {
                    let kind = if editor.conflicting_request.is_some() {
                        super::editor::ComparisonKind::Agent
                    } else {
                        super::editor::ComparisonKind::Information
                    };
                    editor.dialog = Some(super::editor::EditorDialog::Comparison {
                        kind,
                        text: format!("Your current text\n{}\n\nDetails\n{text}", editor.text),
                        offset: 0,
                    });
                } else {
                    self.notice = Some("No conflict details to inspect".into());
                }
            }
            Some(EditorAction::SaveChoices) => {
                self.finish_autosave();
                self.source_editor.as_mut().unwrap().dialog =
                    Some(super::editor::EditorDialog::SaveChoices);
            }
            Some(EditorAction::UndoAgent) => self.undo_agent_edit(),
            Some(EditorAction::Help) => self.open_help(),
            Some(EditorAction::Save) => {
                self.save_source_editor();
            }
            Some(EditorAction::Recover) => {
                let editor = self.source_editor.as_mut().unwrap();
                if editor.dirty() {
                    self.notice =
                        Some("Save or discard your current buffer before loading recovery".into());
                    return;
                }
                match self.operations.recover_edit_buffer(&editor.base.path) {
                    Ok(Some(record)) => {
                        *editor = super::editor::Editor::new(record.base);
                        editor.text = record.edited;
                        editor.autosave_error = Some(
                            "Compare recovered text with disk before accepting its base".into(),
                        );
                        editor.conflict =
                            Some("Recovery loaded. Compare disk with Ctrl+D before saving.".into());
                    }
                    Ok(None) => {
                        self.notice = Some("No recovery buffer exists for this document".into())
                    }
                    Err(e) => self.notice = Some(e.message),
                }
            }
            Some(EditorAction::AcceptDisk) => {
                let editor = self.source_editor.as_mut().unwrap();
                let Some(disk) = editor.disk_comparison.clone() else {
                    self.notice =
                        Some("Compare disk with Ctrl+D before accepting it as the base".into());
                    return;
                };
                match self.operations.edit_document(&editor.base.path) {
                    Ok(current) if current == disk => {
                        editor.base = disk;
                        editor.dialog = None;
                        editor.disk_comparison = None;
                        editor.autosave_error = None;
                        editor.last_edit = std::time::Instant::now();
                        editor.conflict = Some("Disk base accepted; your buffer is retained. Autosave resumes after this comparison.".into());
                    }
                    _ => self.notice = Some(
                        "Disk changed again; compare it with Ctrl+D before accepting a new base"
                            .into(),
                    ),
                }
            }
            Some(EditorAction::CompareDisk) => {
                self.finish_autosave();
                let editor = self.source_editor.as_mut().unwrap();
                match self.operations.edit_document(&editor.base.path) {
                    Ok(disk) => {
                        let text = super::editor::comparison(&editor.text, &disk.text);
                        editor.disk_comparison = Some(disk);
                        editor.dialog = Some(super::editor::EditorDialog::Comparison {
                            kind: super::editor::ComparisonKind::Disk,
                            text,
                            offset: 0,
                        });
                    }
                    Err(e) => self.notice = Some(e.message),
                }
            }
            Some(EditorAction::Read) => {
                if self
                    .source_editor
                    .as_ref()
                    .is_some_and(|e| e.dirty() && e.autosave_error.is_none())
                {
                    self.save_source_editor_with_policy(false);
                }
                if self.park_source_editor() {
                    self.reload_document();
                }
            }
            Some(EditorAction::Quit) => self.request_quit(quit_warning_stood),
            _ => self.source_editor.as_mut().unwrap().key(key),
        }
    }

    /// Requests one handoff of the displayed document to the configured
    /// editor (SPEC 11.3 `E`, 12.5). The request carries the explicit
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
                "menu:{selection}:{}:{}",
                self.menu_query, self.menu_filtering
            ),
            Some(Overlay::Help) => format!(
                "help:{}:{}:{}:{}",
                self.help_scroll, self.help_query, self.help_filtering, self.help_all
            ),
            Some(Overlay::Picker { filter }) => {
                format!("picker:{filter}:{:?}", self.picker_selected)
            }
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
        let overlay = format!("{overlay}:finish-scroll:{}", self.finish_scroll);
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
            format!(
                "{:?}|{:?}|{}",
                history.selection(),
                history.preview_state(),
                history.offset()
            )
        });
        let workspace = &self.workspace;
        [
            format!("{:?}", self.view),
            format!("{:?}", self.focus),
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
            format!("{:?}", self.pending_document),
            format!("{}", self.discovery_truncated),
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
        if !self.flush_buffers() {
            let mut recoverable = true;
            for editor in self
                .source_editor
                .iter()
                .chain(self.retained_editors.iter())
                .filter(|e| e.dirty())
            {
                recoverable &= self
                    .operations
                    .retain_edit_buffer(&editor.recovery_id, &editor.base, &editor.text)
                    .is_ok();
            }
            if !quit_warning_stood || !recoverable {
                self.quit_warning = true;
                self.notice = Some(if recoverable {
                    "Some files could not be saved. Recovery copies are retained. Press q again to quit, or reopen the paused file."
                } else {
                    "Files and recovery copies could not be saved. Keep Margin open and fix storage, or copy your text before using F5 to discard a buffer."
                }.into());
                return;
            }
        }
        let proposed = self.changes.as_ref().map_or(0, ChangesView::proposed_count);
        if proposed > 0 && !quit_warning_stood {
            self.quit_warning = true;
            self.notice = Some(format!(
                "{} unapplied selection{}. Press q again to discard them and quit.",
                proposed,
                if proposed == 1 { "" } else { "s" },
            ));
        } else {
            self.quit = true;
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
        self.finish_scroll = 0;
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
        if self.agent_prompt.is_some()
            || self
                .source_editor
                .as_ref()
                .is_some_and(|e| e.dialog.is_some())
        {
            return;
        }
        if reply.generation != self.generation {
            return;
        }
        if let Some(editor) = &self.source_editor {
            if !editor.dirty() {
                let path = editor.base.path.clone();
                self.sync_editors(&path);
                self.redraw = true;
            }
            return;
        }
        let before = self.fingerprint();
        let InspectionReply {
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

    /// Absorbs one rebuilt document list (SPEC 13.3). The hover stays on
    /// its document by path, so insertions and deletions in one reload
    /// never move it off a surviving document; only when the hovered
    /// document disappeared does the selection fall back to its old index
    /// clamped to the new list, and the fallback obeys the pending-preview
    /// rule: its content loads on the next quiet pass, and the
    /// content-dependent actions state their reason until then. An empty
    /// list clears the hover; the displayed document stays untouched even
    /// when the list no longer names it.
    fn absorb_document_list(&mut self, paths: Vec<String>) {
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
        self.reposition_navigator_cursor();
        self.redraw = true;
        if self.documents.is_empty() {
            return;
        }
        // The fallback selection names a document the column may not show
        // yet: gate on it exactly as one explicit navigation does, so no
        // content-dependent action lands beside content it did not load.
        let shown = self.document.as_ref().map(|document| document.path.clone());
        if shown.is_none_or(|shown| shown != self.documents[self.selection]) {
            self.pending_document = Some(self.documents[self.selection].clone());
        }
    }

    /// Points the navigator cursor at the selected document's row, so one
    /// absorbed reload keeps the outline marker on the hover it kept.
    fn reposition_navigator_cursor(&mut self) {
        self.navigator_cursor = self
            .navigator_entries()
            .iter()
            .position(|entry| *entry == (self.selection, None))
            .unwrap_or(self.selection);
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

    /// Loads the pending navigation target, when one stands, and refreshes
    /// the History view's hover excerpt. The event loop calls this only on
    /// a pass no input waits on, so one rapid run of navigation steps
    /// loads exactly the document it ends on and computes exactly the
    /// excerpt of the revision it rests on.
    pub fn settle_preview(&mut self) {
        if let Some(path) = self.pending_document.take() {
            if self
                .document
                .as_ref()
                .is_none_or(|document| document.path != path)
            {
                self.load_document(&path);
            }
            self.redraw = true;
        }
        self.refresh_history_excerpt();
    }

    /// Whether one navigation target waits for its quiet pass.
    pub fn has_pending_preview(&self) -> bool {
        self.pending_document.is_some()
    }

    /// Whether one quiet pass owes work: a pending navigation target, or
    /// a hover excerpt that no longer matches the hover it belongs to.
    pub fn has_deferred_work(&self) -> bool {
        self.pending_document.is_some()
            || self
                .history
                .as_ref()
                .is_some_and(|history| !history.excerpt_settled(self.workspace.head.as_deref()))
    }

    /// The sentence that labels the pending target (SPEC 11.4): while the
    /// target stands, it is the visible reason every content-dependent
    /// action gives for not running.
    pub fn pending_preview(&self) -> Option<String> {
        self.pending_document
            .as_ref()
            .map(|path| bounded(&format!("{path} is still opening; wait for it to settle")))
    }

    /// Refreshes the History view's hover excerpt (the hover of the
    /// timeline): the bounded before/after of the displayed document
    /// between the hovered revision and the current candidate, read-only,
    /// recomputed only when the hover or the candidate moved.
    fn refresh_history_excerpt(&mut self) {
        let Some(history) = &mut self.history else {
            return;
        };
        if history.preview().is_some() {
            history.clear_excerpt();
            return;
        }
        let adopted = self.workspace.adopted;
        let head = self.workspace.head.clone();
        let path = self.document.as_ref().map(|document| document.path.clone());
        match (adopted, head, path) {
            (true, Some(head), Some(path)) => {
                history.refresh_excerpt(self.operations, &head, &path);
            }
            _ => history.clear_excerpt(),
        }
    }

    /// Saves the reading position of the displayed document (SPEC 5.4):
    /// the anchor plus its drift, most recently left first, capped. The
    /// positions are disposable interface state — losing one costs one
    /// scroll, never one decision.
    fn save_position(&mut self) {
        let Some(document) = &self.document else {
            return;
        };
        let Some(anchor) = &self.anchor else {
            return;
        };
        let position = SavedPosition {
            path: document.path.clone(),
            line: anchor.line,
            drift: self.drift,
        };
        self.saved_positions
            .retain(|saved| saved.path != position.path);
        self.saved_positions.insert(0, position);
        self.saved_positions.truncate(MAX_SAVED_POSITIONS);
    }

    /// Restores the saved reading position of `path` after one load; a
    /// document without one starts at its first line.
    fn restore_position(&mut self, path: &str) {
        let Some(saved) = self
            .saved_positions
            .iter()
            .find(|saved| saved.path == path)
            .cloned()
        else {
            return;
        };
        let last = self.source_lines.len().clamp(1, u32::MAX as usize) as u32;
        self.anchor = Some(SourceAnchor {
            path: path.to_string(),
            line: saved.line.min(last),
        });
        self.drift = saved.drift;
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
        if menu {
            stops.push(Focus::Actions);
        }
        if stops.len() == 1 {
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
        self.layout.shows_navigator() && !self.sidebar_hidden
    }
    /// Whether the unadopted discovery stopped at its document bound, so
    /// the navigator must say the list is bounded (SPEC 11.3).
    pub fn discovery_truncated(&self) -> bool {
        self.discovery_truncated
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
        } else {
            self.navigate_document(document);
        }
        self.redraw = true;
    }

    /// Moves the document hover to `index` and defers the load: the
    /// reading column follows the hover (SPEC 5.4), but one rapid run of
    /// navigation steps loads only the document the run ends on — the
    /// last settled content stays readable while the target stands, and
    /// one quiet pass loads it.
    fn navigate_document(&mut self, index: usize) {
        let path = match self.documents.get(index) {
            Some(path) => path.clone(),
            None => return,
        };
        self.selection = index;
        self.reposition_navigator_cursor();
        if self
            .document
            .as_ref()
            .is_some_and(|document| document.path == path)
        {
            return;
        }
        self.pending_document = Some(path);
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
        if let Some(why) = self.changes.as_ref().map_or_else(
            || Some("no comparison exists to decide".into()),
            |changes| changes.refusal(),
        ) {
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
fn prepare_display(raw: &str) -> String {
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

/// The one total order of the document list (SPEC 11.2): ascending UTF-8
/// path bytes. Paths are unique, so the order has no ties to break, and
/// both list builders below sort by it — the adopted resolution through
/// BTreeMap iteration, the discovery walk through an explicit sort — so
/// every reload lists the same set in the same order and a preserved
/// hover never moves.
pub(crate) fn discover_documents(root: &Path) -> (Vec<String>, bool) {
    let mut found = Vec::new();
    walk_documents(root, String::new(), 0, &mut found);
    found.sort();
    let truncated = found.len() > MAX_DISCOVERED_DOCUMENTS;
    found.truncate(MAX_DISCOVERED_DOCUMENTS);
    (found, truncated)
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

/// Shared bounded movement for selectable lists. Text characters stay text.
fn list_move(key: &KeyEvent, selected: usize, count: usize, page: usize) -> Option<usize> {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    let last = count.saturating_sub(1);
    Some(
        match key.code {
            KeyCode::Up => selected.saturating_sub(1),
            KeyCode::Down => selected.saturating_add(1).min(last),
            KeyCode::PageUp => selected.saturating_sub(page.max(1)),
            KeyCode::PageDown => selected.saturating_add(page.max(1)).min(last),
            KeyCode::Home => 0,
            KeyCode::End => last,
            _ => return None,
        }
        .min(last),
    )
}

fn scroll_key(key: &KeyEvent, offset: &mut usize, page: usize, rows: usize) -> bool {
    let code = match key.code {
        KeyCode::Char('j') => KeyCode::Down,
        KeyCode::Char('k') => KeyCode::Up,
        code => code,
    };
    if let Some(next) = list_move(
        &KeyEvent::new(code, key.modifiers),
        *offset,
        rows,
        page.saturating_sub(1),
    ) {
        *offset = next;
        true
    } else {
        false
    }
}

/// Page through wrapped choices using their rendered row counts.
fn choice_move(
    key: &KeyEvent,
    selected: usize,
    labels: &[String],
    width: u16,
    page: usize,
) -> Option<usize> {
    if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
        && key.modifiers.is_empty()
        && !labels.is_empty()
    {
        let mut offset = 0usize;
        let starts: Vec<usize> = labels
            .iter()
            .map(|label| {
                let start = offset;
                offset += wrap::wrap_text(label, width.max(1)).len().max(1);
                start
            })
            .collect();
        let selected = selected.min(starts.len() - 1);
        let target = if key.code == KeyCode::PageDown {
            starts[selected].saturating_add(page.max(1))
        } else {
            starts[selected].saturating_sub(page.max(1))
        };
        let index = starts
            .iter()
            .rposition(|start| *start <= target)
            .unwrap_or(0);
        return Some(if key.code == KeyCode::PageDown && index == selected {
            (selected + 1).min(starts.len() - 1)
        } else {
            index
        });
    }
    list_move(key, selected, labels.len(), page)
}
