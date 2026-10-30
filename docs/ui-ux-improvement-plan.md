# Interface and interaction improvement plan

Status: software implementation complete, 2026-10-29. Final verification is recorded below.

## Outcome

The terminal user interface (TUI) should support the complete writing workflow.
A writer can open a project, edit a passage, ask an agent for help, and review
the result without memorizing shortcuts or understanding Git internals.
At every step, the interface identifies the active document, affected scope,
current state, and next useful action.

The existing Read, Changes, and History views remain the primary navigation.
Requests become a persistent drawer available from all three views.
The existing command palette becomes the main action discovery surface.

## Evidence before implementation

The October 29 evaluation used live tmux sessions, real Claude requests,
terminal captures, and automated tests. Evidence is retained locally in
`/home/agent/margin-ui-artifacts/QA-report.txt` and the adjacent images.
These paths are local evidence; they are not portable repository links.

- Selection and conflict details disappeared during unchanged background refreshes.
  Fixes and regression tests now cover both cases.
- The title can still report an agent conflict after the writer resolves it.
- Queued and working messages can disagree across interface regions.
- The request composer does not make selection scope sufficiently visible.
- Initialization requires leaving the interface after an editing error.
- Review confirmation exposes internal identifiers before the main consequence.
- Long messages lose useful content at narrow widths.
- `:` already opens a palette with actions, documents, and headings.
  It supports typing, paste, asynchronous matching, and disabled-action reasons.
  Extend this implementation rather than introducing another dispatcher.
- Mouse clicks and Shift+click exist. Drag selection is not implemented.

## Design rules

1. Describe effects using writing terms. Put technical identifiers in Details.
2. Derive visible state from current data. Expire notices when their subject changes.
3. Keep ordinary typing literal while editing, including `:` and view-number keys.
4. Preserve selection, focus, scroll, and unsaved text across observation and overlays.
5. Preview scope and consequences before approval or historical restoration.
6. Provide keyboard and pointer access to primary actions through the same handlers.
7. Keep source previews, search work, and background updates bounded.

## Information architecture

| Surface | Purpose | Primary actions |
| --- | --- | --- |
| Read | Read and edit the current document | Edit, Ask agent, Save checkpoint, Pin reference |
| Changes | Inspect and decide passage changes | Keep, Restore earlier text, Next undecided, Finish review |
| History | Inspect earlier document versions | Preview, Compare, Restore this version |
| Requests drawer | Track work and resolve requests | Inspect, Compare, Cancel, Retry delivery, Open document |
| Command palette | Find actions and navigate | Search actions, documents, headings, and request identifiers |
| Details | Inspect technical evidence | Revisions, request identifiers, scope, errors, recovery commands |

The document list and outline remain distinct concepts. Show the current
document clearly and make its outline collapsible. Preserve a usable prose
width before allocating space to navigation or reference content.

## Delivery sequence

### 1. Reliable state and immediate fixes

Priority: high. Dependencies: none.

- [x] Finalize the selection and conflict-refresh fixes already in the working tree.
- [x] Clear or replace stale activity labels after application, undo, cancellation,
      conflict resolution, and failures.
- [x] Define shared projections for document, review, and request status.
      Use them in titles, drawers, footers, and notices.
- [x] Replace ambiguous save labels with separate states: `Saved to file`,
      `Not checkpointed`, and `Needs review`.
- [x] Make persistent errors separate from transient success notices.
      Give notices a subject and a replacement rule.
- [x] Wrap recovery messages and put the next action before long identifiers.

Acceptance:

- Selection and conflict details survive repeated refreshes and autosave completion.
- Keeping writer text leaves no active-conflict label in any interface region.
- A claimed request cannot simultaneously be described as waiting for a prompt.
- A file saved to disk but not checkpointed is never called unsaved without qualification.
- No history, index, or approval changes occur merely because status refreshes.

Implementation: `src/tui/editor.rs`, `shell.rs`, `render.rs`, and `src/requests.rs`.
Add transition tests and terminal captures for each corrected state.

### 2. Discoverable command palette

Priority: high. Depends on milestone 1 status vocabulary.

- [x] Rename the visible Action menu to Command palette.
- [x] Add a persistent, clickable `Commands` entry with its shortcut.
- [x] Prefer `Ctrl+P` as the global binding, after checking existing bindings.
      Retain `:` outside editing. Provide a visible entry if a terminal intercepts a key.
- [x] Open the palette from Read, the source editor, Changes, History, and Requests.
      Return to the exact invoking focus, selection, and scroll position on cancel.
- [x] Route palette input before editor input while the palette is open.
      Opening, searching, or cancelling must not save or alter the editor buffer.
- [x] Group results as Actions, Documents, Headings, and Requests.
      Keep actions available immediately while metadata loads.
- [x] Rank current-context actions first. Add task aliases such as `ask`, `agent`,
      `save draft`, `checkpoint`, `conflict`, and `finish review`.
- [x] Show each action's name, shortcut, scope, and disabled reason when applicable.
- [x] Provide contextual suggestions for an empty query. Keep their order stable
      while the palette remains open.
- [x] Revalidate the chosen action against live state before execution.
      Confirmation actions open their existing previews rather than bypassing them.
- [x] Make loading, no matches, truncated results, and reduced-update behavior explicit.
      Do not execute a result from an older query. Do not replay Enter automatically
      after delayed matching changes the result list.

Example suggestions while editing a selected passage:

```text
Commands                                      Ctrl+P
> ask

Actions
  Ask agent about selected passage            F2
  Show agent requests                         B
  Undo latest agent edit                      Ctrl+U

Selection: 18 characters in content/article.md
```

Acceptance:

- A new user can find Ask agent, Requests, checkpointing, and conflict comparison by name.
- `:` still inserts a colon in source. Palette queries never enter the document.
- Escape restores the selection and unsaved text exactly.
- Keyboard and mouse selection invoke the same validated action.
- Disabled actions explain how to become available.
- The palette works at 80 and 120 columns with no clipped action names or reasons.
- Normal and reduced-update modes clearly distinguish matching from execution.

Implementation: extend `src/tui/palette.rs`, `actions.rs`, `shell.rs`, `render.rs`,
and pointer targets. Reuse the action catalog and existing metadata worker.
Coordinate ranking with `search-implementation-plan.md`; avoid a second search engine.

### 3. Explicit selection and request scope

Priority: high. Depends on milestones 1 and 2.

- [x] Show a selection summary and an Ask agent action beside editor status.
- [x] Keep a bounded selected-text preview visible in the request composer.
- [x] Label scope explicitly: `Selected passage` or `Whole document`.
      Never silently convert a stale selection into a whole-document request.
- [x] Explain when source must save before the request can be queued.
- [x] Add left-button drag selection, including backward and multiline selection.
- [x] Support Unicode graphemes, tabs, horizontal scrolling, and carriage-return/line-feed (CRLF) endings.
- [x] Add bounded edge scrolling during a drag. Stop on release, cancel, focus loss,
      document switch, or a change that invalidates the source mapping.
- [x] Keep Shift+click and keyboard selection as alternatives. Document terminal
      selection bypass and tmux mouse requirements.

Acceptance:

- The composer identifies the exact document and scope before submission.
- A changed document or selection cannot redirect a request to different content.
- Pointer movement without an active drag does not select text or trigger extra work.
- Dragging beyond the editor never activates another view or action.
- F2 captures exactly the selected source span after keyboard or mouse selection.

Implementation: editor, pointer targets, shell event routing, and request composer.
Coordinate with `tui-pointer-clicks.md` and the terminal smoke matrix.

### 4. Requests drawer and conflict resolution

Priority: high. Depends on milestones 1 through 3.

- [x] Add a persistent `Requests` entry with pending and attention counts.
- [x] Group requests by Needs your choice, Waiting, Working, and Finished.
- [x] Show document, short instruction, current state, and next action first.
      Put full identifiers and delivery receipts in Details.
- [x] Explain prompt-based delivery as `Waiting for your next Claude prompt` when known.
      Use a neutral message when the agent or delivery route is unknown.
- [x] Provide Compare versions, Keep my text, and Use agent proposal actions.
      Keep both versions visible in a scrollable comparison.
- [x] Provide explicit cancellation and delivery retry. Neither action may steal a claim.
- [x] On application, identify the changed passage and offer the existing guarded undo.
- [x] Show requests for the current desk by default and label requests from other desks.
- [x] Reconstruct unresolved conflicts from durable requests after viewer restart.

Acceptance:

- Every pending request has a visible state and useful next action.
- F3, F4, the drawer, and palette commands share the same conflict checks.
- A restart preserves access to both versions and the correct request owner.
- Long instructions and proposals remain inspectable at narrow widths.
- No status described as Delivered claims that Claude read or accepted the request.

### 5. Clear review and restoration decisions

Priority: high. Depends on milestones 1 and 2.

- [x] Lead finish confirmation with affected documents, keep/restore counts,
      and the number of undecided and unvisited passages.
- [x] Explicitly label the action `Keep remaining changes and finish` when applicable.
- [x] Offer Review remaining passages as a prominent alternative.
- [x] Move hashes, result trees, and protocol identifiers into Details.
- [x] Clarify current-document versus entire-writing-scope operations.
- [x] Give history entries readable summaries; put exact timestamps and hashes in Details.
- [x] Explain that historical restoration creates a new saved version needing review.
- [x] Use precise verbs: undo an agent edit, restore earlier passage text,
      or restore a historical document version.

Acceptance:

- Users can identify the complete affected scope before confirmation.
- Finishing with unseen changes requires an explicit, accurately labelled choice.
- The confirmation cannot apply after its underlying comparison becomes stale.
- Cancelling a preview changes no content or approval.
- Existing review receipts, retry behavior, and restoration checks remain authoritative.

### 6. Guided setup and responsive layout

Priority: medium. Depends on milestone 2.

- [x] Replace the uninitialized editing dead end with a setup action.
- [x] Preview the proposed writing scope and included Markdown files before initialization.
- [x] Explain repositories that cannot be edited and provide the relevant next step.
- [x] Keep integration installation an explicit action with a preview of managed files.
- [x] Reduce repeated paths and headings. Give the active document and focus clear emphasis.
- [x] Collapse navigation at narrow widths before reducing the useful editing area.
- [x] Preserve keyboard access to hidden navigation and references through the palette.
- [x] Distinguish selection, focus, current item, and warning without relying on color alone.

Acceptance:

- A new writer can initialize a supported repository and begin editing without leaving the TUI.
- The setup preview makes the writing scope clear and respects existing settings.
- All primary workflows remain usable at 80×24 and 120×34.
- Long filenames, large text, Unicode, and no-color mode remain legible.

### 7. End-to-end validation and release

Depends on the completed behavior changes above.

- [x] Test first use, selection, request creation, delivery, application, undo,
      conflict resolution, checkpoint review, and historical restoration.
- [x] Exercise mouse, keyboard, palette, and direct shortcuts against the same actions.
- [x] Check pending saves, disconnected agents, stale comparisons, and viewer restart.
- [x] Run the full automated suite and required verification tiers.
      Retest affected suites after corrections and report the final coverage accurately.
- [x] Capture screenshots at both terminal sizes after asynchronous work settles.
      Verify state through files and command output as well as screenshots.
- [ ] Observe a writer unfamiliar with the shortcuts performing the core tasks.
      Record completion, errors, and requests for help; do not substitute expert judgment for this check.
- [x] Update the README, contextual help, palette labels, guides, and specifications together.

Release gate: no known selection loss, hidden unresolved conflict, contradictory
persistent status, or ambiguous approval scope in the tested workflows.
Any remaining issue must have reproducible evidence and a stated limitation.

Keep new QA images, raw terminal captures, and reports in
`/home/agent/margin-ui-artifacts`. Preserve failed-attempt evidence with clear labels.
Use a timestamped run directory to avoid overwriting earlier captures.

## Change boundaries

Keep the existing operation layer authoritative for saves, claims, application,
review, and restoration. Interface changes must call those operations.
A visible action does not imply automatic permission to apply or approve content.
The palette must not become an arbitrary shell-command runner.

Active delivery through Claude channels is a separate integration project.
A useful request drawer and clear waiting state do not depend on that transport.
Broader typography or color changes follow interaction and state fixes.

## Delivery notes

The software changes cover milestones 1 through 6. Validation includes real
Claude application and undo, restart comparison, keyboard and pointer tests,
and captures at both target sizes. The unfamiliar-writer observation remains
a human release check. It is not replaced by agent testing.

Implementation choices:

- Drag edge scrolling advances with pointer events. It stops on release,
  focus loss, keyboard input, resize, and document changes.
- Delivery retry resets the last recorded session receipt. It preserves the
  request owner and state. It does not restart claimed work.
- Other desks require the drawer's `o` toggle. Full identifiers, delivery
  counts, and retained errors are available through `d`.
- Setup adopts Markdown files in the shared discovered root. Existing settings
  disable initialization. Unsupported repositories retain operation errors.
- Notices wrap to three rows. Autosave and history errors take priority over
  transient success messages. A displayed notice does not follow another document.
- A human still confirms setup, installation, review, and restoration in the
  interface. The command palette opens these previews.

The implementation and verification are complete. Human usability observation
remains a release follow-up.

QA artifacts: `/home/agent/margin-ui-artifacts/ui-improvements-20261029-200959/`.
The captures reconstruct real terminal buffers. They are not desktop screenshots.
Raw captures, failed attempts, and test logs remain available beside the images.


Verification: the final PR tier passes all 1,050 tests across 59 targets.
It includes 256 generated restoration cases and required tmux coverage.
Formatting, strict Clippy, and Rust 1.89 compatibility checks pass.
The release binary passes the size and startup smoke budgets.
The QA report records raw logs, earlier failed attempts, and the pending
human observation.
The formal PR gate passes all four profiles, including the expected negative
control. The release binary is available at `target/release/margin`.
