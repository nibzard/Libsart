# Review workspace: resume, find anything, and pinned references

Status: implemented in the review-workspace branch. The three milestones below
preserve the existing CLI review and restoration protocols. Verification and
coverage limits are recorded in [review-workspace-verification.md](review-workspace-verification.md).

## Goal

Let a writer leave a long review, return to their decisions and reading
position, find an action or passage by name, and check a reference beside the
changes. Saving review progress preserves intent; only the existing finish
confirmation records approval.

The ideas come from GrimoireTUI's [resume record][resume], [command palette][palette],
and [read-only companion scene][beside]. Adapt the interactions to Margin's
Git-backed review model rather than adopting Grimoire's project format.

[resume]: https://github.com/kelsierbot/GrimoireTUI/blob/main/crates/grimoire-core/src/resume.rs
[palette]: https://github.com/kelsierbot/GrimoireTUI/blob/main/src/palette.rs
[beside]: https://github.com/kelsierbot/GrimoireTUI/blob/main/src/app.rs

## Existing contracts and intended amendments

- `src/tui/changes.rs` owns the pinned comparison, proposed decisions, visited
  groups, and selection. `finish.rs` composes the ordinary review plan and its
  deterministic request ID. Keep that application path authoritative.
- SPEC 5.2 currently discards selections on quit; SPEC 8.2 permits only
  in-memory selections and rebuildable caches. Milestone 1 explicitly adds local
  saved review drafts and replaces the discard-on-quit rule. These files carry
  user intent but never establish a baseline, receipt, or approval.
- SPEC 5.3 still applies: a changed comparison invalidates application of the
  old plan. This proposal does not migrate decisions to new changes.
- `actions.rs` supplies action names, bindings, and disabled reasons. Extend
  that catalog rather than creating a second command dispatcher.
- SPEC 11.1–11.4 require readable measure, source anchors, visible focus,
  keyboard access, and labels independent of color. Preserve those properties.
- [The hover plan](tui-hover-navigation.md) rejects three columns. Milestone 3
  makes a deliberate exception for an explicitly pinned reference: hide the
  navigator while displaying two prose columns, and provide a single-column
  fallback. It does not introduce a third reading column.
- Coordinate with [the search plan](search-implementation-plan.md) and
  [pointer plan](tui-pointer-clicks.md). Neither is a prerequisite. Palette
  metadata matching is separate from body search; pointer actions, if present,
  must use the same validation and modal precedence as keyboard actions.

Update those contracts and README as their corresponding milestones ship.
This document alone does not change the advertised behavior.

## 1. Resumable reviews

### Writer interaction

On the first proposed decision, begin saving a local review draft. Show
`Review progress saved locally · not applied` after a successful write. Saving
progress must never invoke checkpoint, review, restore, or a notes write.

At startup, offer a compact saved-review entry showing branch, candidate,
timestamp, proposed keep/restore counts, and compatibility with the current
comparison. Do not replace the initial view or steal focus automatically.
Expose the same entry through `Resume saved review` in the action catalog.
When multiple drafts exist, list them newest first with stable ID tie-breaks.

The entry offers:

| Action | Result |
| --- | --- |
| Resume | Validate and load a compatible draft into this viewer |
| Inspect | Show saved identity, decisions, and compatibility/blocker reasons |
| Discard saved review | Explicitly confirm deletion of that draft only |
| Esc | Leave the offer without loading or deleting anything |

Resuming never reopens an actionable finish confirmation. The writer must
open a fresh summary and explicitly confirm the existing finish action.
Explicit command-line path/view options take precedence over saved navigation;
resuming choices does not override those options. Otherwise restore the last
view, document, selected change, and source position where they remain valid.

Quit flushes pending progress. If successful, say that choices remain saved
and unapplied, without a discard warning. On save failure, retain the in-memory
plan, show the error, and require an explicit quit-without-saving action before
discarding newer choices. Ctrl-C uses this same flow; cleanup on unavoidable
termination remains best effort. Never claim that an uncatchable kill flushed
pending work.

### Storage and ownership

Store versioned JSON under the repository's resolved Git directory:
`margin/tui/reviews/<draft-id>.json`. Resolve the Git directory through the Git
adapter; do not assume `.git` is a directory. Do not add worktree support beyond
Margin's current support envelope or put these files in the writing scope.

Each viewer creates its own collision-resistant draft ID with exclusive file
creation. Resuming acquires a per-draft advisory ownership lock for the viewer's
lifetime. Another viewer may inspect it, but cannot resume, overwrite, or
discard it while owned. Lock contention is a visible condition, not an excuse
to overwrite another session. Crashes release ownership through OS lock
semantics; the JSON survives. A viewer can explicitly start a separate draft.

Write complete replacements through same-directory temporary files and atomic
rename, following the project's durability/error conventions. Never replace a
valid record with a partially serialized record. Ignore incomplete temp files
on load. Reject symlinks and non-regular records and keep directory/file access
restricted to the current user where supported. Do not resolve stored paths
outside the existing document-access guards.

The record contains:

- Schema version, draft ID, update timestamp, and monotonic save generation.
- Repository identity, branch ref, scope ID, adoption/start marker identity,
  baseline OID, candidate OID, and complete `review_id`.
- Proposed decisions keyed by canonical change ID, plus visited change IDs.
- Last view/path, selected change ID, source-mode choice, and a bounded map of
  per-document/per-view source anchors. Persist source lines and optional
  within-source-line display offsets, never rendered row numbers alone.
- Pinned reference descriptor and its position once milestone 3 exists.
- If finish has been attempted, its existing request ID and exact prepared
  review plan, so receipt inspection can identify an already completed attempt.

Do not store document bodies, note text, terminal output, or command-line
secrets. Store enough change labels (path and heading) to inspect old choices
without pretending they belong to today's groups. An unreadable old revision
does not erase the saved choice.

Bound each record to 4 MiB, at most 10,000 decision/visited entries each, and
256 navigation anchors. Deduplicate IDs and reject conflicting/unknown decision
values. Bound startup enumeration to 100 records and report omissions rather
than claiming the list is complete. Never automatically evict a draft with
unapplied choices. Oversize records, unsupported versions, and corrupt files
produce an inspectable error and leave the original intact; normal reading
and review remain available.

### Save scheduling and compatibility

- Debounce decision changes by 250 ms, with a one-second maximum pending
  interval. Save navigation at a lower frequency, at most once per second.
  Flush on normal quit and before a finish attempt. Serialize owned snapshots
  off the input loop; bounded pending work coalesces to the newest generation.
- A completion for an old generation must not label newer choices as saved.
  A delayed write must not recreate a record after successful completion or
  explicit discard; drain/cancel writes before removing it.
- Resume reloads the full comparison through the existing diff operations.
  Require matching repository, branch, scope, adoption, B, C, and review ID,
  complete detail loading, and known decision/visited IDs. Never attach a
  decision using a filename, excerpt similarity, or list index.
- A matching comparison with a current mutation blocker may resume for
  inspection, retaining the existing disabled reasons. Dirty content or
  changed eligibility must not become actionable because it was resumed.
- A mismatching identity is `Stale saved review`. Inspect its saved choices;
  offer a fresh review with no migrated choices. Keep the old record until
  explicit discard. Refreshing an active stale review follows the same rule.
- Missing navigation targets fall back to the nearest valid source position
  or first listed document, with a notice. This does not relax plan identity
  checks. Source-mode changes and resize preserve source coordinates.
- After verified successful review, remove the completed draft. If cleanup
  fails, show success plus the cleanup error; never report the review as failed.
  On restart, inspect the existing request's Git receipt before offering it as
  unfinished. A matching receipt means completed; missing/uncertain evidence
  leaves it inspectable. Startup never invokes a mutating retry automatically.

Saving is optional for continued use: an unavailable directory leaves the
review in memory with a persistent `Progress not saved` label. A failed local
save does not prevent an explicitly confirmed review through the existing
operation; successful application is still determined by its receipt.

## 2. Find anything palette

### Interaction and result model

Extend `:` outside text entry into a searchable action palette. With an empty
query, show the complete action catalog in its existing order, including
disabled actions and their reasons. Keep `f` as the document-only picker,
`/` as current-view search, and `Ctrl+F` as body search across listed documents.

Nonempty queries search these typed entries:

| Kind | Search fields | Enter |
| --- | --- | --- |
| Action | Display name and curated aliases | Run the existing action handler |
| Document | Markdown title and repository-relative path | Open in Read |
| Heading | Heading text, document title/path, ancestor headings | Open Read at source anchor |

Show kind, label, disambiguating path/context, shortcut when present, and any
disabled reason. Names and labels are escaped using existing display rules.
No raw document text or path is interpreted as a command. Notes and historical
full-text retrieval are deferred; they are not needed for this milestone.

Arrows select, PgUp/PgDn page, Enter activates, and Esc closes without action.
Printable `j`, `k`, `q`, `r`, `s`, `:`, and `?` are query text while the palette
is open. Backspace edits and bracketed paste inserts bounded literal text.
Ctrl-C retains clean quit. Tab/Shift-Tab remain inside the palette while it
stands; they must not expose the background reading column to decision keys.
Document these contextual bindings in help.

An action result uses the same current-context enablement check at activation,
including preview-pending, stale, and mutation blockers. Preserve the originating
pane/view as its target context while the palette owns input focus; opening
the palette must not unlock an action disabled in the reference pane. Finish opens the
ordinary summary; it never applies from the palette. Disabled entries remain
selectable for explanation; Enter on one leaves the palette open. Opening an
entry closes the palette only after successful dispatch. Failed navigation
keeps the query and shows the error. Opening the palette from an input field
or confirmation is not allowed; a typed colon stays text in an input field.

### Matching, bounds, and freshness

Use a deterministic metadata matcher with case-insensitive Unicode text
handling and original-text span mappings. Rank exact label matches, label
prefixes, all-token matches, then bounded subsequence matches in that order.
Path-only matches rank below equivalent label matches. Tie-break by kind
(action, document, heading), canonical path, source line, and stable entry ID.
Do not add a full-text engine or semantic dependency for this feature.

Reuse listed-document discovery and Markdown parsing/source mappings. Before
adoption, use the existing pre-adoption discovery policy. Read at most 256 KiB
per file, 16 MiB total, and 2,000 files, with existing unsafe-file exclusions.
Cap the metadata set at 20,000 entries, result display at 100, and queries at
256 Unicode scalar values. Keep all actions available in the empty-query menu;
metadata limits must never hide an action. Report skipped/truncated content,
entry limits, and additional matching results in words.

Metadata loading runs off the input loop, reuses existing refresh signals,
and publishes coherent snapshots. Query/list generations reject obsolete
results; use stable entry IDs to preserve selection across updates. Debounce
query edits by approximately 100 ms. In reduced-update mode, support
Enter-submitted matching: the first Enter computes edited input, and a later
Enter activates a displayed result. Never activate a result from an old query.

Before navigation, revalidate scope, file safety, and source identity. If a
heading changed, refresh and require reselection instead of jumping to an old
line. A missing document leaves the current review untouched. Palette
navigation does not change visited-group counts or proposed choices.

## 3. Pinned reference

### Pinning and identity

Support one explicitly selected, read-only reference at a time:

- `Pin current document as reference`: capture the currently displayed Read
  source and anchor as a snapshot, whether saved or an unsaved working preview.
- `Pin selected version as reference`: from History, read that document at the
  selected immutable revision, without entering restoration preview.
- `Choose reference…`: reuse the document/heading picker to choose a listed
  working document or passage, without changing the primary document first.
- `Show/hide reference`, `Focus reference`, and `Clear reference` are catalog
  actions. No new global printable shortcuts are required for the first version.

Pinning replaces the prior reference only after the new one loads successfully.
The reference has its own path, source anchor/range, rendered/source mode,
scroll offset, and origin: full commit OID for historical content, or content
fingerprint and capture time for working content. Display a short revision or
`Working snapshot`/`Unsaved working snapshot` label plus exact identity on demand.
Keep missing-at-revision distinct from an empty file. Bound reads to 256 KiB
per reference and disclose truncation; reuse current scope, file safety, and
historical-read guards. No external file browser is introduced.

Working references are frozen snapshots, not a second live editor. Existing
polling marks them `Source changed since pinning` when appropriate and offers
`Refresh reference` explicitly. Historical references remain pinned to the
resolved OID even if HEAD moves. Pinning or refreshing never re-pins B/C or
changes review decisions. Deleting or changing the source does not silently
replace the already loaded snapshot.

Persist only the descriptor/fingerprint and source anchor in the local review
draft, not the snapshot text. On resume, historical content reloads by OID.
A working reference reloads only if its fingerprint still matches. Otherwise
show `Reference unavailable: working source changed` and offer explicit refresh
or replacement. Missing objects/paths remain visible as unavailable references
and do not prevent resuming valid review decisions.

### Responsive layout and focus

At 140 or more columns of the Margin terminal, show primary content and
reference side by side, hide the navigator, and divide available width evenly
with one blank separator cell. Cap each prose column at the existing 76-cell
measure. This threshold refers to Margin's actual pane width, not the total
tmux desk width. Read, Changes, and the History list may show the companion;
finish summaries and restoration previews temporarily hide it.

At 60–139 columns, `Show reference` switches the single content region to the
reference with an explicit `Reference · read-only` header and `Back to review`
action. Esc returns to the unchanged primary view. Hiding either presentation
retains the pin and both source positions. Below the existing minimum size,
retain the current minimum-size message and quit/resize behavior.

Reference is an additional visible focus stop. Tab/Shift-Tab, paging, wheel,
and supported clicks target the visibly focused pane. `s` toggles source for
that pane; `/` searches its frozen displayed content. Reference navigation
never marks review groups visited. While reference has focus, keep/restore,
undo-decision, add-note, edit, history-restore, and finish are disabled with
`Return to review to …`; they must never act on the hidden primary selection.
Esc closes a local search first, then returns focus to the primary view.

Resize preserves both source anchors and the plan. Moving from two columns to
one retains the focused pane as the visible content; returning to two columns
restores both. Opening a modal suspends reference focus and restores a valid
visible focus on close. Status distinguishes the working candidate's state
from the reference's origin; a reference must never inherit a `Reviewed` label
from the primary document accidentally.

## Implementation milestones

1. **Save and resume review progress.** Add a small persistence module such as
   `src/tui/session.rs`, expose typed snapshots from `changes.rs` and shell
   navigation, and schedule IO through `run.rs`. Reuse Git receipt and identity
   checks through application operations. Complete storage/failure/concurrency
   tests before changing quit behavior or advertising resume.
2. **Search the action catalog and document structure.** Add a pure matcher and
   typed entries in `src/tui/palette.rs`; extend `actions.rs`, `shell.rs`,
   `render.rs`, and existing refresh workers. Ship action matching first, then
   bounded document/heading metadata in the same milestone. Body search remains
   independently usable throughout.
3. **Pin and display reference content.** Add a reference state/loader module;
   extend `layout.rs`, focus/event routing, rendering, refresh integration,
   and session descriptors. Reuse source wrapping and historical show operations
   rather than the mutating restore-preview controller.

All milestones preserve the single-package architecture, Rust minimum version,
terminal lifecycle, bounded worker queues, stale-result rejection, and existing
dependency policy. No async runtime or new external service is required.

## Acceptance and verification

Extend meaningful existing integration, buffer, and PTY tests. Do not treat
serialization round trips or screenshots alone as proof of safe behavior.

| Area | Required evidence |
| --- | --- |
| Resume | Quit/restart retains choices, visited IDs, selection, and source position; resize and explicit startup options obey precedence |
| Compatibility | New HEAD, scope A → B → A, adoption change, dirty source, missing group IDs, incomplete diff, and changed eligibility cannot revive actionable old choices |
| Storage | Atomic replacement failure, full/unwritable storage, corrupt/oversize/new-version records, concurrent viewers, stale save completion, and crash-left temp files preserve valid drafts and report actual save state |
| Finish | Resumed choices produce the same plan/result as uninterrupted review; a crash after commit before cleanup is recognized through the existing receipt; startup never applies |
| Palette | Deterministic ranking, duplicate titles/headings, Unicode spans, literal paste, disabled reasons, no stale-query activation, scope exclusions, and bounded/incomplete metadata |
| Reference | Working snapshot freeze/refresh, immutable revision pinning, missing versus empty, failed replacement, changed-source resume, and independent anchors/search/source mode |
| Focus/layout | Wide/narrow/too-small layouts, reference-focused decision keys, modal precedence, resize, hidden navigator restoration, monochrome, and reduced updates |
| Isolation | Resume, palette, and reference reads leave document bytes, index, refs, receipts, notes, and baseline unchanged; only the documented local session files may change |

Run targeted suites during each milestone, then
`cargo test --all-targets --locked` and the repository's required verification
tier before integration.
Include PTY checks for quit/save-failure flow, palette text precedence, and
reference focus. Measure large-scope palette startup/query latency and session
save churn against existing TUI performance budgets; record coverage limits and
any deliberate budget amendments. Do not add startup writes for a viewer that
only reads and never starts or resumes a review draft.

Ship with updated README examples, SPEC 5.2–5.4/8.2/11.1–11.4/13.3, contextual
help, and applicable navigation/pointer plan amendments. Update backlog status
only for work actually implemented and verified.

## Navigation, search, and pointer plan coordination

The referenced hover/search/pointer proposals are not committed at this branch's
base. This implementation establishes the following integration amendments:

- Two reading columns are permitted only for an explicitly pinned reference at
  140+ columns of Margin itself. The navigator is hidden; there is no third
  prose column. Collapsing/hiding the reference restores the navigator policy.
- Palette metadata and body search remain independent workers and result types.
  `f`, `/`, and Ctrl+F retain their contextual document/local/body functions.
- Future pointer hit targets must use the same pane restrictions and modal
  precedence as keyboard dispatch. Wheel currently targets the focused pane.
  No pointer-click/hover behavior from an unmerged proposal is advertised here.
- Restoring a draft restores source coordinates and canonical selected IDs;
  screen row indices are derived after wrapping and are never persisted.

### Metadata field bound amendment

Labels and ancestor context are each capped at 512 Unicode scalars so a single
very long heading cannot be copied into thousands of entries without a byte
bound. A file with truncated metadata contributes to the displayed truncated
count. Content beyond those bounds is not claimed searchable. The 256 KiB/file,
16 MiB total, 2,000-file, 20,000-entry, 100-result, and release latency/resource
budgets remain unchanged.
