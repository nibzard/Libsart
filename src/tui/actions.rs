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
    Refresh,
    HistoryCheckpoints,
    PreviewRestore,
    NextUndecided,
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
    /// Collapse or show the document outline.
    ToggleSidebar,
    /// Show grouping rationale or revision metadata.
    Details,
    /// Clear the selected review decision.
    UndoDecision,
    /// Add writer feedback to the selected group without applying decisions.
    AddNote,
    /// Open the document picker.
    DocumentPicker,
    /// Search within the current view.
    Search,
    /// Search working source across the listed documents.
    WorkspaceSearch,
    /// Edit the complete current source in the document pane.
    EditDocument,
    /// Hand the current file to the configured external editor.
    ExternalEditor,
    /// Queue an instruction for saved source.
    AskAgent,
    /// Inspect durable request states and explanations.
    AgentRequests,
    /// Capture the complete writing scope and inspect its changes.
    CheckpointDraft,
    UndoAgentEdit,
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
    pub const ALL: [Action; 33] = [
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
        Action::WorkspaceSearch,
        Action::EditDocument,
        Action::ExternalEditor,
        Action::AskAgent,
        Action::AgentRequests,
        Action::CheckpointDraft,
        Action::UndoAgentEdit,
        Action::FinishReview,
        Action::Cancel,
        Action::Help,
        Action::Quit,
        Action::ToggleSidebar,
        Action::Details,
        Action::UndoDecision,
        Action::AddNote,
        Action::Refresh,
        Action::HistoryCheckpoints,
        Action::PreviewRestore,
        Action::NextUndecided,
    ];

    /// The name the menu and the help list print.
    pub fn name(self) -> &'static str {
        match self {
            Action::Refresh => "Refresh",
            Action::HistoryCheckpoints => "Expand draft checkpoints",
            Action::PreviewRestore => "Preview document restoration",
            Action::NextUndecided => "Next undecided group",
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
            Action::ToggleSidebar => "Toggle sidebar",
            Action::Details => "Details / fold outline",
            Action::UndoDecision => "Clear selected review decision",
            Action::AddNote => "Note on selected group",
            Action::DocumentPicker => "Document picker",
            Action::Search => "Search within current view",
            Action::WorkspaceSearch => "Search all documents",
            Action::EditDocument => "Edit current file",
            Action::ExternalEditor => "Open external editor",
            Action::AskAgent => "Ask agent",
            Action::AgentRequests => "Agent requests",
            Action::CheckpointDraft => "Checkpoint scope and review",
            Action::UndoAgentEdit => "Undo latest agent edit",
            Action::FinishReview => "Finish review",
            Action::Cancel => "Cancel or go back",
            Action::Help => "Help",
            Action::Quit => "Quit",
        }
    }

    /// Typed bindings are shared by dispatch and shortcut labels.
    pub fn primary_key(self) -> Option<crossterm::event::KeyEvent> {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let code = match self {
            Self::ReadView => KeyCode::Char('1'),
            Self::ChangesView => KeyCode::Char('2'),
            Self::HistoryView => KeyCode::Char('3'),
            Self::Refresh => KeyCode::Char('R'),
            Self::HistoryCheckpoints => KeyCode::Char('a'),
            Self::NextUndecided => KeyCode::Char(']'),
            Self::ToggleSidebar => KeyCode::Char('['),
            Self::Details => KeyCode::Char('d'),
            Self::UndoDecision => KeyCode::Char('u'),
            Self::SourceView => KeyCode::Char('s'),
            Self::AddNote => KeyCode::Char('N'),
            Self::ActionMenu => KeyCode::Char(':'),
            Self::Help => KeyCode::Char('?'),
            Self::Search => KeyCode::Char('/'),
            Self::DocumentPicker => KeyCode::Char('f'),
            Self::EditDocument => KeyCode::Char('e'),
            Self::ExternalEditor => KeyCode::Char('E'),
            Self::AskAgent => KeyCode::Char('A'),
            Self::AgentRequests => KeyCode::Char('B'),
            Self::CheckpointDraft => KeyCode::Char('C'),
            Self::UndoAgentEdit => KeyCode::Char('U'),
            Self::NextGroup => KeyCode::Char('n'),
            Self::PreviousGroup => KeyCode::Char('p'),
            Self::KeepGroup => KeyCode::Char('K'),
            Self::RestoreGroup => KeyCode::Char('r'),
            Self::WorkspaceSearch => {
                return Some(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
            }
            Self::FinishReview | Self::PreviewRestore => KeyCode::Enter,
            _ => return None,
        };
        Some(KeyEvent::new(code, KeyModifiers::NONE))
    }

    pub fn from_key(key: &crossterm::event::KeyEvent) -> Option<Self> {
        use crossterm::event::{KeyCode, KeyModifiers};
        if key.code == KeyCode::Enter
            || key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return None;
        }
        Self::ALL.into_iter().find(|action| {
            action.primary_key().is_some_and(|binding| {
                binding.code == key.code
                    && binding.modifiers == key.modifiers.difference(KeyModifiers::SHIFT)
            })
        })
    }

    pub fn group(self) -> (u8, &'static str) {
        match self {
            Self::EditDocument | Self::ExternalEditor | Self::CheckpointDraft => (1, "Write"),
            Self::NextGroup
            | Self::PreviousGroup
            | Self::NextUndecided
            | Self::KeepGroup
            | Self::RestoreGroup
            | Self::UndoDecision
            | Self::AddNote
            | Self::FinishReview
            | Self::HistoryCheckpoints
            | Self::PreviewRestore => (2, "Review"),
            Self::AskAgent | Self::AgentRequests | Self::UndoAgentEdit => (3, "Agent"),
            Self::Help | Self::Cancel | Self::Quit | Self::ActionMenu => (4, "Interface"),
            _ => (0, "Navigate"),
        }
    }

    pub fn executable(self) -> bool {
        !matches!(self, Self::Move | Self::PageMove)
    }

    /// The default binding of SPEC 11.3, as the lists print it.
    pub fn binding(self) -> String {
        use crossterm::event::{KeyCode, KeyModifiers};
        if let Some(key) = self.primary_key() {
            return match key.code {
                KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    format!("Ctrl+{}", c.to_ascii_uppercase())
                }
                KeyCode::Char(c) => c.to_string(),
                KeyCode::Enter => "Enter".into(),
                _ => unreachable!("catalog keys have printable labels"),
            };
        }
        match self {
            Self::CycleFocus => "Tab / Shift-Tab",
            Self::Move => "j / k or arrows",
            Self::PageMove => "PgUp / PgDn",
            Self::Cancel => "Esc",
            Self::Quit => "q / Ctrl-C",
            _ => "",
        }
        .into()
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

/// Commands available while the source editor owns input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorAction {
    Ask,
    UseProposal,
    KeepText,
    SaveChoices,
    Details,
    Help,
    UndoAgent,
    Save,
    Recover,
    AcceptDisk,
    CompareDisk,
    Read,
    Quit,
}
impl EditorAction {
    pub const ALL: [Self; 13] = [
        Self::Ask,
        Self::UseProposal,
        Self::KeepText,
        Self::SaveChoices,
        Self::Details,
        Self::Help,
        Self::UndoAgent,
        Self::Save,
        Self::Recover,
        Self::AcceptDisk,
        Self::CompareDisk,
        Self::Read,
        Self::Quit,
    ];
    pub fn key(self) -> crossterm::event::KeyEvent {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        match self {
            Self::Ask => KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE),
            Self::UseProposal => KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE),
            Self::KeepText => KeyEvent::new(KeyCode::F(4), KeyModifiers::NONE),
            Self::SaveChoices => KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE),
            Self::Details => KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE),
            Self::Help => KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE),
            Self::UndoAgent => KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            Self::Save => KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            Self::Recover => KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
            Self::AcceptDisk => KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            Self::CompareDisk => KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            Self::Read => KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            Self::Quit => KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Ask => "Save and ask agent about selection",
            Self::UseProposal => "Use agent proposal",
            Self::KeepText => "Keep my text",
            Self::SaveChoices => "Save / discard / stay",
            Self::Details => "Inspect conflict details",
            Self::Help => "Editor help",
            Self::UndoAgent => "Undo latest agent edit",
            Self::Save => "Save working file",
            Self::Recover => "Load retained recovery buffer",
            Self::AcceptDisk => "Keep my text against compared disk version",
            Self::CompareDisk => "Compare disk with my text",
            Self::Read => "Save and return to Read",
            Self::Quit => "Save buffers and quit",
        }
    }
    pub fn binding(self) -> String {
        use crossterm::event::KeyCode;
        match self.key().code {
            KeyCode::F(n) => format!("F{n}"),
            KeyCode::Char(c) => format!("Ctrl+{}", c.to_ascii_uppercase()),
            KeyCode::Esc => "Esc / Ctrl+C".into(),
            _ => unreachable!(),
        }
    }
    pub fn from_key(key: &crossterm::event::KeyEvent) -> Option<Self> {
        use crossterm::event::{KeyCode, KeyModifiers};
        if key.modifiers.contains(KeyModifiers::ALT) {
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if key.code == KeyCode::Char('c') {
                return Some(Self::Read);
            }
            if key.modifiers.contains(KeyModifiers::SHIFT)
                && matches!(key.code, KeyCode::Char('a' | 'A'))
            {
                return Some(Self::Ask);
            }
        }
        Self::ALL.into_iter().find(|action| {
            let binding = action.key();
            binding.code == key.code && binding.modifiers == key.modifiers
        })
    }
}
