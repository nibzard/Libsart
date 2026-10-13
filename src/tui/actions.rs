//! The action catalog: the keyboard contract as one list (SPEC 11.3).
//!
//! Every action of the SPEC 11.3 table lives here once, with its default
//! binding and its name. The action menu draws the list, the help overlay
//! draws the keys, and the shell answers the bindings, so the three can
//! never disagree about what the keys do. An action the current context
//! cannot run carries the reason in words; a disabled action without an
//! explanation is a contract violation, not a style choice (SPEC 11.4).

/// One action of the keyboard contract (SPEC 11.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Cycle focus through navigation, document, and visible actions.
    CycleFocus,
    /// Open the action menu.
    ActionMenu,
    /// Switch to the Read view.
    ReadView,
    /// Switch to the Changes view.
    ChangesView,
    /// Switch to the History view.
    HistoryView,
    /// Move within the focused element.
    Move,
    /// Move one viewport in the focused element.
    PageMove,
    /// Move to the next change group.
    NextGroup,
    /// Move to the previous change group.
    PreviousGroup,
    /// Record a proposed keep for the selected group.
    KeepGroup,
    /// Toggle a proposed restoration for the selected group.
    RestoreGroup,
    /// Show the exact Markdown changes.
    SourceView,
    /// Open the document picker.
    DocumentPicker,
    /// Search within the current view.
    Search,
    /// Hand the displayed document to the configured editor.
    EditDocument,
    /// Open the finish summary of the whole scope.
    FinishReview,
    /// Leave the standing local context.
    Cancel,
    /// Show the contextual key list and current blockers.
    Help,
    /// Restore the terminal and leave.
    Quit,
}

impl Action {
    /// Every action in SPEC 11.3 table order.
    pub const ALL: [Action; 19] = [
        Action::CycleFocus,
        Action::ActionMenu,
        Action::ReadView,
        Action::ChangesView,
        Action::HistoryView,
        Action::Move,
        Action::PageMove,
        Action::NextGroup,
        Action::PreviousGroup,
        Action::KeepGroup,
        Action::RestoreGroup,
        Action::SourceView,
        Action::DocumentPicker,
        Action::Search,
        Action::EditDocument,
        Action::FinishReview,
        Action::Cancel,
        Action::Help,
        Action::Quit,
    ];

    /// The name the menu and the help list print.
    pub fn name(self) -> &'static str {
        match self {
            Action::CycleFocus => "Cycle focus",
            Action::ActionMenu => "Action menu",
            Action::ReadView => "Read view",
            Action::ChangesView => "Changes view",
            Action::HistoryView => "History view",
            Action::Move => "Move",
            Action::PageMove => "Page",
            Action::NextGroup => "Next change group",
            Action::PreviousGroup => "Previous change group",
            Action::KeepGroup => "Keep selected group",
            Action::RestoreGroup => "Restore selected group",
            Action::SourceView => "Source view",
            Action::DocumentPicker => "Document picker",
            Action::Search => "Search within current view",
            Action::EditDocument => "Edit document",
            Action::FinishReview => "Finish review",
            Action::Cancel => "Cancel or go back",
            Action::Help => "Help",
            Action::Quit => "Quit",
        }
    }

    /// The default binding of SPEC 11.3, as the lists print it.
    pub fn binding(self) -> &'static str {
        match self {
            Action::CycleFocus => "Tab / Shift-Tab",
            Action::ActionMenu => ":",
            Action::ReadView => "1",
            Action::ChangesView => "2",
            Action::HistoryView => "3",
            Action::Move => "j / k or arrows",
            Action::PageMove => "PgUp / PgDn",
            Action::NextGroup => "n",
            Action::PreviousGroup => "p",
            Action::KeepGroup => "K",
            Action::RestoreGroup => "r",
            Action::SourceView => "s",
            Action::DocumentPicker => "f",
            Action::Search => "/",
            Action::EditDocument => "e",
            Action::FinishReview => "Enter",
            Action::Cancel => "Esc",
            Action::Help => "?",
            Action::Quit => "q / Ctrl-C",
        }
    }
}

/// One row of the action menu: an action and, when the current context
/// cannot run it, the reason in words (SPEC 11.3: disabled actions have
/// an explanation).
#[derive(Clone, Debug)]
pub struct MenuEntry {
    /// The action this row names.
    pub action: Action,
    /// Why the keys cannot run this action right now; `None` when they
    /// can.
    pub disabled: Option<String>,
}

impl MenuEntry {
    /// The line the menu draws for this entry.
    pub fn line(&self, selected: bool) -> String {
        let marker = if selected { "› " } else { "  " };
        match &self.disabled {
            None => format!(
                "{}{} · {}",
                marker,
                self.action.binding(),
                self.action.name(),
            ),
            Some(reason) => format!(
                "{}{} · {} — unavailable: {}",
                marker,
                self.action.binding(),
                self.action.name(),
                reason,
            ),
        }
    }
}
