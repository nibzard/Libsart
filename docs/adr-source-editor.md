# Source editor and desk request storage

Status: Accepted and implemented locally. This is a decision record, not an
active plan. See the [plan index](README.md) for delivery status.

## Decision

Use a small source buffer over the existing Crossterm input and Ratatui output.
Keep file saves and request state transitions in the shared operations layer.
Use the existing Git adapter for source identities and metadata paths.

Use `unicode-segmentation` 1.13.3 directly for cursor and selection boundaries.
The lockfile already includes this exact crate through Ratatui. This change
adds no package or version to the resolved graph. Unicode grapheme boundaries
need more than character boundaries, especially for combining marks and emoji.
The existing `unicode-width` crate measures terminal cell widths.

A full editor framework would add behavior and dependency cost beyond this
release's bounded source editing scope. The buffer supports selection,
undo, redo, and literal paste. It bounds source, history, and rendered cells.
The external editor remains available for larger documents and other tools.

## Save and delivery contracts

File saves compare the complete base version under Margin's mutation lock.
They journal before replacement and retain uncertain outcomes. This does not
provide exclusion against writers that ignore the lock.

Store requests durably in private Git-resolved metadata. Keep the socket as a
bounded navigation and notification channel. Agents submit replacement text
for a captured source span. The viewer applies it after validating the base.
Persist `applying` before the write and `applied` after verification. Never
replay an interrupted application automatically.

Do not lease claims by elapsed time. An explicit owner releases interrupted
work after inspection. Retries reuse the request ID and identical payload.
A new desk explicitly resumes a pending request from the previous desk.

Autosave uses one background worker after a 750-millisecond pause. New input
continues while a captured version saves. Its reply updates the base without
replacing newer text. Retain up to 16 inactive buffers and evict only clean ones.
Source writes preserve the index, so unrelated staging does not block saving.
Automatic history captures request and result boundaries instead of each pause.
History capture retains its existing staging guard and retries when blocked.

Permit conservative span rebasing for one disjoint writer edit. Retain conflicts
for an explicit choice against the shown version. Store applied before/after
versions for guarded undo and interrupted-operation reconciliation.

## Cost and evidence

The earlier release executable in this workspace measured 5,231,920 bytes.
The [implementation validation record](editing-validation.md) records the new
executable measurement and the source rendering diagnostic.
The repository's `budgets` gate enforces the stripped executable budget.
The focused editing suite measures behavior on complete source, including
Unicode, CRLF, concurrency, conflicts, and terminal lifecycle.

Linked worktrees remain inspection-only. Git resolves metadata paths correctly,
but the existing mutation guards continue to refuse unsupported worktrees.

The first long-line diagnostic spent about 72 ms moving to the end and
rendering 1 MiB of ASCII source. Restricting allocations to visible cells and
using direct byte spans for plain ASCII reduced a later local measurement
to about 4 ms. Unicode source still uses grapheme boundaries. These are local
observations, not a registered benchmark verdict.

## Requirements and reconsideration

This decision implements SPEC 2.5, 11.3, and 12.8. It preserves the shared
operations boundary and separates saves from approvals. Reconsider the editor
component when richer editing commands or measured Unicode
rendering costs exceed this buffer's scope. Reconsider queue storage when the
single-worktree, local-writer contract changes.
