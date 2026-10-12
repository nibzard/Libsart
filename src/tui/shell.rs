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

use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

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
use crate::tui::wrap::{self, DisplayRow};

/// How many content bytes the Read view carries at once. The view states
/// the bound when it truncates; it never invents an ending.
const READ_VIEW_BYTES: u64 = 256 * 1024;

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
}

impl Overlay {
    /// Says whether this overlay is a text-entry field, where ordinary
    /// characters are text (SPEC 11.3).
    fn is_text_entry(&self) -> bool {
        matches!(self, Overlay::Picker { .. } | Overlay::Search { .. })
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
    display: Vec<DisplayRow>,
    display_width: u16,
    offset: usize,
    drift: usize,
    viewport_rows: usize,
    anchor: Option<SourceAnchor>,
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
            display: Vec::new(),
            display_width: 0,
            offset: 0,
            drift: 0,
            viewport_rows: 0,
            anchor: None,
            workspace: WorkspaceSnapshot::unadopted(&folder, None),
            changes: None,
            history: None,
            finish: None,
            finish_result: None,
            restore_result: None,
            generation: 0,
            pinned: None,
            overlay: None,
            pending_editor: None,
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
        self.changes = ChangesView::load(self.operations);
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
        self.history = HistoryView::load(self.operations);
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
        self.documents = resolved
            .paths
            .into_iter()
            .filter(|(_, state)| {
                matches!(state, ScopedPathState::Tracked | ScopedPathState::Untracked)
            })
            .map(|(path, _)| path.to_string_lossy().into_owned())
            .collect();
        // BTreeMap iteration is already sorted by path bytes.
        self.workspace.documents = self.documents.len();
    }

    /// Loads an unadopted folder: the discovered documents of first use
    /// (SPEC 5.1).
    fn load_unadopted(&mut self, folder: &str) {
        self.regenerate();
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
        let mut document = DocumentView {
            path: path.to_string(),
            candidate: None,
            saved: SavedState::UnsavedDraft,
            text: String::new(),
            total_bytes: None,
            truncated: false,
        };
        match working {
            Ok((bytes, total)) => {
                document.total_bytes = Some(total);
                document.truncated = total > READ_VIEW_BYTES;
                match &shown {
                    Some(outcome) => {
                        document.candidate = Some(outcome.revision_oid.clone());
                        if working_matches(&outcome.content, &bytes, total) {
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
                            document.text = String::from_utf8_lossy(&bytes).into_owned();
                        }
                    }
                    None => {
                        // No candidate exists to compare with.
                        document.saved = SavedState::UnsavedDraft;
                        document.text = String::from_utf8_lossy(&bytes).into_owned();
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => match &shown {
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
                    self.notice = Some(bounded(&format!("no document exists at {path}")));
                }
            },
            Err(error) => {
                document.saved = SavedState::Unknown;
                self.notice = Some(bounded(&format!(
                    "the working file cannot be read: {error}"
                )));
                if let Some(outcome) = &shown {
                    document.candidate = Some(outcome.revision_oid.clone());
                    document.text = display_text(&outcome.content);
                    document.total_bytes = content_bytes(&outcome.content);
                }
            }
        }
        document.text = prepare_display(&document.text);
        self.absorb_document(document);
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
        self.source_lines = document.text.lines().map(str::to_string).collect();
        self.display = Vec::new();
        self.display_width = 0;
        self.anchor = Some(SourceAnchor {
            path: document.path.clone(),
            line: 1,
        });
        self.document = Some(document);
    }

    /// Recomputes the wrapped rows for one frame size; the caller renders
    /// afterwards. A changed width restores the reading position from the
    /// anchor plus the best-effort drift (SPEC 11.2).
    pub fn prepare_layout(&mut self, columns: u16, rows: u16) {
        self.layout = layout::classify(columns, rows);
        let geometry = layout::frame_layout(ratatui::layout::Rect::new(0, 0, columns, rows));
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
                        let text = panel.summary.lines().join("\n");
                        panel.rows = wrap::wrap_text(&text, width);
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
                    history.prepare(width, self.viewport_rows);
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
            self.display = wrap::wrap_text(&text, width);
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
        let last = self.display.len().saturating_sub(self.viewport_rows.max(1));
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
        self.source_lines
            .iter()
            .take(line as usize)
            .rev()
            .find(|source| source.trim_start().starts_with('#'))
            .map(|heading| heading.trim().to_string())
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
            Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key),
            _ => {}
        }
        if self.fingerprint() != before {
            self.redraw = true;
        }
    }

    /// Absorbs one bracketed paste (SPEC 11.3).
    fn handle_paste(&mut self, text: &str) {
        let text_entry = self.overlay.as_ref().is_some_and(Overlay::is_text_entry);
        if !text_entry {
            self.notice = Some("pasted text is ignored outside text entry".to_string());
            return;
        }
        let pasted: String = text.chars().take(FIELD_CHARS).collect();
        match self.overlay.as_mut() {
            Some(Overlay::Picker { filter }) => filter.push_str(&pasted),
            Some(Overlay::Search { query }) => query.push_str(&pasted),
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
            Overlay::Menu { selection } => {
                let count = self.menu().len().max(1);
                match (key.code, control) {
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
                        self.overlay = Some(Overlay::Help);
                    }
                    (KeyCode::Enter, false) => self.activate_menu(selection),
                    _ => {
                        self.overlay = Some(Overlay::Menu { selection });
                        self.notice =
                            Some("j/k choose · Enter runs · Esc closes the menu".to_string());
                    }
                }
            }
            Overlay::Help => match (key.code, control) {
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
        self.overlay = Some(Overlay::Menu { selection: 0 });
        self.focus = Focus::Actions;
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

    /// Answers one key press of the reading column, where every key but
    /// the finish action changes only what the interface shows.
    fn handle_column_key(&mut self, key: &KeyEvent, control: bool, quit_warning_stood: bool) {
        match (key.code, control) {
            (KeyCode::Char('c'), true) | (KeyCode::Char('q'), false) => {
                self.request_quit(quit_warning_stood)
            }
            (KeyCode::Char('1'), false) => self.view = View::Read,
            (KeyCode::Char('2'), false) => self.enter_changes(),
            (KeyCode::Char('3'), false) => self.view = View::History,
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
                    Action::NextGroup | Action::PreviousGroup => {
                        if !in_changes {
                            Some(changes_only.to_string())
                        } else if groups == 0 {
                            Some("no change groups exist in this review".to_string())
                        } else {
                            None
                        }
                    }
                    Action::KeepGroup | Action::RestoreGroup => {
                        if !in_changes {
                            Some(changes_only.to_string())
                        } else {
                            self.changes.as_ref().and_then(|changes| changes.refusal())
                        }
                    }
                    Action::SourceView => {
                        if !in_changes {
                            Some(changes_only.to_string())
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
                    Action::DocumentPicker => (self.documents.is_empty())
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
                MenuEntry { action, disabled }
            })
            .collect()
    }

    /// Runs the selected entry of the action menu (SPEC 11.3): a disabled
    /// entry states its reason and runs nothing; an enabled entry closes
    /// the menu and runs the same handler its binding reaches.
    fn activate_menu(&mut self, selection: usize) {
        let entry = self.menu().get(selection).cloned();
        match entry {
            Some(entry) => {
                if let Some(reason) = &entry.disabled {
                    self.overlay = Some(Overlay::Menu { selection });
                    self.notice = Some(reason.clone());
                    return;
                }
                let action = entry.action;
                self.overlay = None;
                if self.focus == Focus::Actions {
                    self.focus = Focus::Reading;
                }
                self.activate(action);
            }
            None => self.overlay = Some(Overlay::Menu { selection }),
        }
    }

    /// Runs one action, as the menu's `Enter` does (SPEC 11.3). Every
    /// branch is the handler of the action's default binding, so the
    /// menu and the keys cannot disagree.
    fn activate(&mut self, action: Action) {
        match action {
            Action::CycleFocus => self.cycle_focus(true),
            Action::ActionMenu => self.open_menu(),
            Action::ReadView => self.view = View::Read,
            Action::ChangesView => self.enter_changes(),
            Action::HistoryView => self.view = View::History,
            // `Move` is `j` and `k` together; the menu entry states the
            // binding rather than running one step. `Cancel` from the menu
            // is the menu's own closing, which already ran.
            Action::Move => {}
            Action::Cancel => {
                self.notice = Some("the overlay closed; nothing was applied".to_string())
            }
            Action::NextGroup => self.step_group(1),
            Action::PreviousGroup => self.step_group(-1),
            Action::KeepGroup => self.toggle_decision(Decision::Keep),
            Action::RestoreGroup => self.toggle_decision(Decision::Restore),
            Action::SourceView => self.toggle_source_view(),
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
            Some(Overlay::Menu { selection }) => format!("menu:{selection}"),
            Some(Overlay::Help) => "help".to_string(),
            Some(Overlay::Picker { filter }) => format!("picker:{filter}"),
            Some(Overlay::Search { query }) => format!("search:{query}"),
        };
        let document = self
            .document
            .as_ref()
            .map_or("none".to_string(), |document| {
                format!(
                    "{}|{:?}|{}|{}",
                    document.path,
                    document.saved,
                    document.truncated,
                    document.total_bytes.unwrap_or(0),
                )
            });
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
            overlay,
            format!("{:?}", self.notice),
            format!("{:?}", self.quit),
            format!("{:?}", workspace.adopted),
            format!("{:?}", workspace.branch),
            format!("{:?}", workspace.head),
            document,
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
    /// observes nothing, because it cannot claim an arrival. One
    /// successful answer refreshes the workspace summary the header
    /// states, then compares the observed workspace against the
    /// observation the pinned comparison carries.
    pub fn observe_inspection(&mut self, reply: InspectionReply) {
        let before = self.fingerprint();
        if reply.generation != self.generation {
            return;
        }
        if !reply.outcome.ok() {
            return;
        }
        let Some(data) = reply.outcome.data else {
            return;
        };
        self.absorb_inspection(data);
        self.observe_arrivals();
        if self.fingerprint() != before {
            self.redraw = true;
        }
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
        let documents = self.layout.shows_navigator() && !self.documents.is_empty();
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
            Focus::Documents => self.step_selection(direction),
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

    /// Moves the reading position by one viewport, less one held row.
    fn page(&mut self, direction: i32) {
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

    /// Moves the document selection and loads the document it names.
    fn step_selection(&mut self, direction: i32) {
        if self.documents.is_empty() {
            return;
        }
        let next = if direction > 0 {
            self.selection + 1
        } else {
            self.selection.saturating_sub(1)
        };
        let Some(path) = self.documents.get(next).cloned() else {
            return;
        };
        self.selection = next;
        self.load_document(&path);
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

/// Reads the working bytes of one path, bounded by `limit`.
fn read_working(root: &Path, path: &str, limit: u64) -> std::io::Result<(Vec<u8>, u64)> {
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

/// Discovers the Markdown documents of one folder before adoption
/// (SPEC 5.1).
fn discover_documents(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    walk_documents(root, String::new(), 0, &mut found);
    found.sort();
    found.truncate(MAX_DISCOVERED_DOCUMENTS);
    found
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
