# Documentation and plan status

Status checked against the local code on 2026-10-15. Implemented means the
code exists locally. It does not mean every change is committed or released.

## Active work

[Remaining gaps plan](remaining-gaps-plan.md) is the active plan. It covers
pointer hover, physical terminal validation, and the decision gate for notes
acknowledgments. No implementation starts from this documentation update.

## Proposed follow-ups

- [Resumable reviews, find anything, and pinned references](review-workspace-spec.md)
- [Ranked and typo-tolerant workspace search](search-implementation-plan.md)

These are proposals; implementation has not started.

## Completed plans and decisions

| Document | Status | Evidence |
| --- | --- | --- |
| [Desk opening](desk-open-plan.md) | Implemented, including the explicit-link follow-up | `tests/desk_open.rs`, `tests/tmux_desk.rs` |
| [Keyboard hover and navigation](tui-hover-navigation.md) | All 11 numbered items implemented | `tests/tui_hover.rs`, the budgets gate |
| [Pointer clicks](tui-pointer-clicks.md) | Clicks and ignored-event filtering implemented | `tests/tui_access.rs`, `src/tui/run.rs` unit tests |
| [Notes channel](notes-channel.md) | All three stages implemented; acknowledgments deferred | `tests/notes_channel.rs`, integration unit tests |
| [Markdown editing and desk communication](editing-and-desk-communication-plan.md) | Implemented locally; uncommitted and unpublished | `tests/source_editing.rs`, `tests/autosave.rs` |
| [Source editor decision](adr-source-editor.md) | Accepted and implemented locally | [Editing validation](editing-validation.md) |

Completed plans remain at their existing paths to preserve links. Their top
status blocks distinguish historical design text from active work. Deliberately
skipped ideas are not an implementation backlog.

## Guides and validation records

- [Editing Markdown and requesting agent edits](source-editing.md)
- [Explicit desk links](desk-links.md)
- [Editing validation](editing-validation.md)
- [Evidence from correcting passages during review](review-correction-evidence.md)
- [Terminal smoke matrix](terminal-smoke-matrix.md)
- [Verification tiers](verification-tiers.md)
