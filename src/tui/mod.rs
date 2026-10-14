//! Terminal views, input mapping, layout, and terminal state (SPEC 5, 11).
//!
//! The terminal interface is one more surface over the shared operations
//! of [`crate::app`]: the command line and this surface read the same
//! repository through the same contract, and the one mutation this surface
//! offers — finishing a review — travels the same operation the command
//! line exposes (SPEC 16.9). The modules split by concern:
//!
//! - [`shell`] holds the state: the views, the retained context, the
//!   read-only loading, the key answers, and the overlays those keys
//!   open.
//! - [`actions`] holds the keyboard contract as one catalog: the actions,
//!   their default bindings, and the menu rows that list them (SPEC
//!   11.3).
//! - [`changes`] holds the Changes view: every change group, the source
//!   view, and the in-memory plan (SPEC 5.2).
//! - [`finish`] holds the finish flow: the whole-scope summary, the
//!   explicit keep-remaining intent, and the one shared application
//!   (SPEC 5.2).
//! - [`history`] holds the History view: the editorial timeline, the
//!   restoration preview, and the one shared restoration (SPEC 5.5).
//! - [`refresh`] holds the refresh slice: the polling worker, the pinned
//!   observation it compares against, and the arrival names (SPEC 5.3,
//!   13.3).
//! - [`layout`] classifies sizes and computes geometry (SPEC 11.2).
//! - [`wrap`] soft-wraps the reading column and keeps source anchors.
//! - [`render`] fills one frame from the shell (SPEC 11.1).
//! - [`run`] owns terminal lifecycle and the event loop (SPEC 11.5).

pub mod actions;
pub mod changes;
pub mod finish;
pub mod history;
pub mod layout;
pub mod refresh;
pub mod render;
pub mod run;
pub mod search;
pub mod shell;
pub mod wrap;

pub use actions::{Action, MenuEntry};
pub use changes::{ChangesView, Decision, GroupState};
pub use finish::FinishSummary;
pub use history::{HistoryView, RestorePreview};
pub use refresh::{
    INSPECTION_INTERVAL, InspectionReply, PinnedObservation, RefreshWorker, Watch, WatchJob,
    WatchList, WatchedDocument, WatchedFile,
};
pub use run::{LaunchError, TerminalGuard, launch};
pub use shell::{ColorMode, EditorHandoff, Focus, Overlay, Shell, TuiOptions, View};
