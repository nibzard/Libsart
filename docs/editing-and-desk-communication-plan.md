# Markdown editing and desk communication

Status: Implemented locally; uncommitted and unpublished. This is a completed
design record, not an active plan. See [the user guide](source-editing.md),
[validation](editing-validation.md), and the [plan index](README.md).

| Milestone | Implementation |
| --- | --- |
| Contracts and editor choice | `SPEC.md` Section 12.8 and [the decision record](adr-source-editor.md) |
| Source buffer and edit mode | `src/tui/editor.rs`, shell input, and source rendering |
| Guarded saves and recovery | `src/editing.rs` and shared application guards |
| Durable request protocol | `src/requests.rs`, `margin requests`, and versioned desk control |
| Passage requests and adapter | Saved source selections, request status, and Claude prompt context |
| Validation and documentation | Real-Git tests, terminal tests, gates, and the user guide |

Make each eligible Markdown document editable inside Margin. Let a writer request an agent edit from a selected passage. Show the resulting changes in the existing review workflow.

## Implemented workflow

The initial manual-save plan is superseded by the agreed autosave workflow.
The [source editing guide](source-editing.md) defines the current interactions.

- Save after 750 milliseconds without input on a background worker.
- Retain cursor, selection, and undo across document navigation.
- Save the latest selection automatically before an agent request.
- Keep writing while the agent works; preserve disjoint edits on application.
- Retain conflicts for explicit use or keep choices.
- Preserve request and result boundaries automatically without approval.
- Undo a fresh agent result and communicate that decision through writer feedback.

## Acceptance coverage

`tests/autosave.rs` verifies debounce, retained buffers, background lock
contention, paused conflicts, quit flushing, staged-index preservation, passage
clicking, request boundaries, disjoint application, explicit choices, and undo.
`tests/source_editing.rs` verifies source fidelity, recovery, request ownership,
terminal input, and independent desks. The existing UI and CLI suites verify
integration. See [validation](editing-validation.md) for completed gates.

Linked worktrees remain inspection-only. Source is bounded at 1 MiB. The cache
retains 16 inactive buffers plus the active buffer. Queue and recovery bounds
remain explicit. Source saves preserve staging; history capture can wait for it.
