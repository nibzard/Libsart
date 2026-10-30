# Edit Markdown and ask an agent

Open an adopted Markdown document with `margin PATH` or `margin tui`.
Press `e` or click a passage to edit its current source in the document pane.
Margin saves after 750 milliseconds without input. **Saving**, **Saved to file**, and
**Autosave paused** show the current state.

Editing requires an eligible, writable UTF-8 file in the adopted writing scope.
The existing repository guards apply, including the adopted branch. Source saves leave the Git index untouched,
so unrelated staging does not block them. Checkpoints still require a clean index. Files larger than 1 MiB stay readable through the bounded preview.
Use `E` to open the configured external editor for these files.
Linked worktrees remain inspection-only under the existing repository policy.

## Edit and save

| Key | Action |
| --- | --- |
| `e` in reading | Edit the current file |
| Arrows, Home, End, Page Up, Page Down | Move through source |
| Shift with movement | Select source |
| `Ctrl+Left` / `Ctrl+Right` | Move by Unicode word |
| `Ctrl+Home` / `Ctrl+End` | Move to the document boundaries |
| `Ctrl+P` | Find commands without changing text or selection |
| Left-button drag | Select source; move beyond the edge to scroll |
| Click / Shift-click | Place the source cursor / extend selection |
| Wheel | Scroll source without moving the cursor |
| `Ctrl+A` | Select all source |
| `Ctrl+Z` / `Ctrl+Y` | Undo / redo |
| `Ctrl+S` | Save the working file |
| `Esc` | Save and return to reading |
| `Ctrl+D` | Compare the disk version without replacing your buffer |
| `Ctrl+B` | Accept the compared disk version as the base; retain your text |
| `Ctrl+R` | Load a retained recovery buffer explicitly |
| `F2` | Save the latest text and ask an agent about the selection |
| `F1` | Open scrollable, searchable editor help |
| `F6` | Inspect conflict details in a scrollable panel |
| `F3` / `F4` | Use the agent proposal / keep your text after a conflict |
| `Ctrl+U` | Undo the latest agent edit if the file still matches it |
| `F5` | Open explicit Save, Discard, or Stay choices |
| `Ctrl+Q` | Save all buffers and quit |
| `E` in reading | Open the configured external editor |
| `C` in reading | Checkpoint the entire writing scope and open Changes |

Text entry owns ordinary keys, including `q`, `r`, and `:`. Bracketed paste
inserts text. Cursor movement treats Unicode grapheme clusters and CRLF as
units. Saving preserves existing line endings, the final newline, and file
permissions. It does not format the source. Undo retains at most 512 snapshots
and approximately 8 MiB of source text.

Navigation retains your cursor, selection, and undo history. Margin keeps up to
16 inactive buffers plus the active buffer. It evicts only clean buffers.
Autosave continues after you switch documents. Remote navigation also retains
buffers. Actual dialogs can defer navigation and agent results.

Historical views offer **Edit current file**. This opens the working file;
it does not modify the historical version.

Saving changes the working file. Checkpointing preserves the scope in Git.
Reviewing records your decisions. Saving and checkpointing never approve edits.
Autosave runs in the background and does not create a commit after every pause.
Margin creates checkpoints at request and result boundaries. These checkpoints
preserve meaningful versions without approving them. If staging blocks history,
the file stays saved and history capture retries later. `C` remains available
for an explicit checkpoint. `--capture-after-edit` also applies to manual saves.

To correct part of a passage during review, press `e`, edit the source, and
press `Esc` to save. Press `C` to checkpoint and return to Changes. Margin
retains keep and restore decisions for unchanged patches with the same source
coordinates and context. It resets changed, removed, or uncertain decisions.
The notice reports both counts. Review undecided groups before finishing.
Explicit refresh with `R` still clears all decisions.

## Conflicts and recovery

Each buffer captures source bytes, a content digest, the working tree, the
writing scope, the adopted branch, and the nearest start marker. Margin checks
these values before saving. If disk content changes, it retains your buffer
and refuses the save. Use `Ctrl+D` to inspect disk, then reconcile the source.
The comparison shows the changed line span from each version. Common opening
and closing lines are counted and omitted. Use arrows or Page Up/Down to read
the comparison; Esc returns to editing.
Press `Ctrl+B` to accept that exact disk version as the new base while keeping
your text. Autosave resumes after you accept the compared version. If disk changes again, compare again.
You can also use `F5`, **Discard**, and `e` to reload the current file.

Cooperating Margin operations share the existing mutation lock. Saves use a
temporary file in the document directory, preserve permissions, sync the file,
replace the destination, and sync its directory on Unix. An unrelated writer
can still race the checks. The lock does not control arbitrary editors or Git.
An uncertain save retains a recovery record and requires inspection.

Margin retains a dirty buffer every two seconds. A crash can lose the input
since the latest retention pass. On reopening the editor, a notice identifies
an available buffer. `Ctrl+R` loads it without saving it. Disk changes still
cause a conflict when you try to save recovered text.

Find private recovery records with:

```bash
git rev-parse --git-path margin-editing/recovery
```

Draft records contain `base` and `edited` text. A failed save also retains an
observation of disk. Successful verified saves remove their temporary save
records. Saving or discarding removes the current buffer's retained draft.
Older records remain available until you inspect and remove them. The limit
is 32 records and 64 MiB. Remove an old record only after you recover or discard its text.

## Request an agent edit

1. Press `e` and select source with Shift and movement.
2. Press `F2`, type your instruction, and press Enter.
3. Keep writing or switch documents while the agent works.
4. Read the status beside the source, or press `B` in reading for request details.
5. Review the applied changes. Saving and automatic checkpoints do not approve them.

`F2` saves your latest text before capturing the request. You do not need a
separate save command. Margin preserves a checkpoint at the request boundary
when repository state permits it.

Use `A` in reading for a whole-document request. In the editor, an empty
selection also requests the whole document. The stored selection uses UTF-8
byte offsets in the complete captured source. It does not use rendered rows.

Margin waits for a dirty buffer to save before applying its result. You can
continue editing other passages. A conservative comparison preserves a single
disjoint writer edit before or after the requested passage. Overlapping or
ambiguous changes pause application and retain both versions. `F3` uses the
shown agent proposal; `F4` keeps your text. Use `F6` to inspect long proposals.
The panel scrolls and retains both choices. Esc returns to editing. A whole-document request conflicts
with any intervening source change. Branch, scope, and adoption changes also
refuse application. Review decisions and actual dialogs defer application.

Press `Ctrl+U` in editing, or `U` in reading, to undo the latest agent edit.
Undo refuses if later file changes would be overwritten. Margin records the
undo as writer feedback. The Claude adapter includes recent cancelled and
undone requests in the next prompt context.

When a checkpoint stores the exact applied result, Margin attaches its
explanation as an agent note. The explanation also remains in the request.
If the writer changes the result before checkpointing, Margin keeps its
explanation in the request instead of attaching it to different content.

## Generic agent commands

Agents can use the command-line interface without a special integration:

```bash
margin requests list --json
margin requests inspect REQUEST_ID --json
margin requests claim REQUEST_ID --owner SESSION_ID --json
margin requests submit REQUEST_ID --owner SESSION_ID \
  --input /tmp/margin-replacement.txt --explanation "Keep the concrete example" --json
```

Write the replacement to a temporary UTF-8 file outside the writing scope.
The replacement file contains only the replacement for the captured source
span. Inspect `base.text`, `start`, and `end` first. For a whole-document
request, replace the complete captured source. Do not write the requested
file directly when using this protocol.

A successful submission means `result_ready`, not applied. The viewer applies
it after checking the captured context. Direct agent writes are ordinary
external edits and receive no submission guarantee.

Claims have explicit owners. An identical claim or submission can be retried.
A different owner cannot submit a result. After an interrupted agent, inspect
its work before explicitly releasing the claim:

```bash
margin requests release REQUEST_ID --owner SESSION_ID --json
margin requests cancel REQUEST_ID --json
margin requests remove REQUEST_ID --json
```

Cancellation does not reverse an applied edit. Removal accepts only finished
requests. An interrupted application remains `applying`; an interrupted undo remains
`undoing`. Both require manual
inspection of the document and recovery records. Margin does not repeat it.
Run `margin requests reconcile REQUEST_ID --json` after inspection. This
compares disk with the captured and proposed versions, then records applied,
failed, or stale state. It never writes the document.

Requests survive a viewer restart. A new desk has a new socket identity.
From the new desk's agent pane, explicitly reassign a pending request:

```bash
margin requests resume REQUEST_ID --json
```

This command does not steal a claim or apply its result. It changes the viewer
that can receive the pending result. Separate desks do not apply each other's
requests. Requests made outside a desk can be received by a standalone viewer.

## Claude Code delivery

Install the existing adapter explicitly with `margin hook install claude`.
The installer also adds request guidance to its owned instruction block.
It installs `/margin` at `.claude/skills/margin/SKILL.md`.
Dry runs include this file. Removal deletes the unchanged skill file.
An edited skill causes installation or removal to stop before writing.
It upgrades an unchanged older block and preserves user-edited blocks by
refusing to replace them. Starting a desk does not install instructions.

The adapter announces queued requests on `SessionStart` and the next
`UserPromptSubmit` event. Startup context includes existing owned requests
and commands for inspecting writing state. This also applies when Claude
resumes a session or starts after compaction.
When Claude supplies `session_id`, context gives the concrete claim owner
`claude-SESSION_ID`. Use this owner for claims and submissions.
Existing claims remain explicit; startup never releases or transfers them.
It uses the documented `hookSpecificOutput.additionalContext` field. It does
not interrupt an active turn or keep Claude running after Stop.
See the [Claude Code hook reference](https://code.claude.com/docs/en/hooks#userpromptsubmit-decision-control).
Other agents use the generic commands above.

### Track delivery

Margin records request context emitted by hooks for each Claude `session_id`.
A delivery receipt identifies the request version, session, and emission time.
It does not confirm that Claude read the context or claimed the request.
Claims, application, and review remain separate.

For each session, the adapter sends up to four unseen queued requests and two
unseen cancellation or undo decisions per hook. Later prompts advance through
remaining updates. Releasing a claim creates a new delivery generation.
Moving a request to another desk also makes it eligible for delivery there.
Desk hooks announce requests for their desk and requests without a desk.
Hooks without a session identifier retain repeated delivery and write no receipts.
Writer notes retain their existing feedback behavior.

`margin requests list --json` and `margin context --json` include delivery status
and receipts. The request panel shows **Waiting for delivery** or
**Delivered; waiting for agent** for queued requests. The reading status includes
waiting and delivered counts. Delivered means at least one retained session
received an offer for the current version. Inspect receipts for a specific session.

If hook output is lost, clear that session's receipt and retry on the next prompt:

```bash
margin requests redeliver REQUEST_ID --session CLAUDE_SESSION_ID --json
```

Use the raw session identifier from a delivery receipt. This command preserves
the owner, request state, source, and result. Repeating it is harmless.
A blocked prompt records no delivery. Receipt writes use the shared repository lock
and skip versions that changed after feedback selection. A tracking failure leaves
context available and reports a diagnostic; later hooks can repeat the update.
Receipt writes and hook output are separate operations. Interrupted output can
require explicit redelivery. Concurrent hooks can emit the same update.

Each request retains at most 32 session receipts. A new session replaces the oldest
receipt at this limit. A session whose receipt was removed can receive the update again.
Older request records remain readable with empty delivery history.

## Read writing context

Run `margin context --json` from the agent pane to inspect writing context.
The response includes repository scope, review state, and pending requests.
Pending requests include identifiers, owners, states, captured digests, and byte spans.

Inside a desk, `viewer` contains the last snapshot published after a terminal draw:

- `captured_ms`: snapshot time in milliseconds since the Unix epoch.
- `active_document`: path, content digest, source, unsaved state, and preview.
- `selection`: start and end offsets in UTF-8 bytes, plus a preview.
- `reference`: the pinned reference descriptor and preview.

The active source is the editor buffer when editing, or the working file otherwise.
The working file can differ from the saved comparison shown in Changes or History.
The editor digest covers unsaved text. `disk_digest` identifies its captured base.
Reference fingerprints identify the pinned content rather than the current file.
Each preview contains at most 256 bytes and reports truncation and complete byte length.
Selection offsets refer to the complete active source, including omitted preview text.

Check the snapshot time and content digest before acting on a passage.
Use edit requests to obtain complete source and apply results with conflict checks.
The context query does not save text, create requests, or approve changes.
Without a desk, `viewer` is null. A missing or unavailable desk socket returns an error.
A viewer from another repository is refused. Repository inspection and viewer capture
have separate observation times; the response is not an atomic workspace snapshot.

## Storage and protocol limits

Requests live under `git rev-parse --git-path margin-editing/requests`.
Directories use private permissions. Records use atomic replacement and the
shared Margin lock. The writing files and Git index do not store the queue.
The queue accepts at most 64 records and 32 MiB of serialized data. Instructions
and explanations accept at most 8,000 bytes each. Source and replacements
accept at most 1 MiB each. Remove finished requests when storage is full.

The legacy desk open request remains supported. Versioned control uses:

```json
{"schema_version":1,"request":{"operation":"status"}}
```

Operations are `status`, `notify`, `context`, and `open`. An `open` request includes
`path` and optional `line` inside `request`. A status response reports the
schema version and viewer busy state. A notification wakes queue polling;
it does not acknowledge application. Socket messages retain the 8 KiB bound
and bounded deadlines. Queue data travels through repository metadata.

## Help and input contexts

Help, disk comparison, conflict details, and Save/Discard/Stay each own their
input. Ordinary text and paste cannot edit the document behind these dialogs.
Closing help restores the cursor, selection, and conflict details.
The editor title shows **EDITING**, followed by the save state and path.
The footer keeps the primary controls visible at supported terminal widths.
The request panel (`B` in reading) supports arrows, Page Up/Down, and Home/End.
The Read context row reports pending requests and requests that need attention.

## Find commands and track requests

Click **Commands** or press `Ctrl+P` from any primary view or the editor.
Type an action, document, heading, or request identifier. `Esc` returns to
where you started. A colon still inserts `:` while editing.
The palette shows shortcuts, disabled reasons, and the selected scope.

The request composer labels **Selected passage** or **Whole document**.
It shows the character count and a bounded source preview. `F2` saves the
selected source before opening this composer. `Enter` queues the instruction.

Click **Requests** or find **Agent requests** in the palette. Requests show
waiting, working, attention, and finished states. `Tab` selects the next
request; `Shift+Tab` selects the previous request. `Enter` opens its document
and comparison when needed. Use `c` to cancel, `r` to retry the last delivery,
and `d` for details. Retry does not change ownership or restart a claim.
Use `o` to include other desks. A delivery receipt confirms emission only.

Drag selection works inside the source editor. Release the mouse to stop.
Shift-click and Shift+arrows remain available. To copy terminal text instead,
use your terminal's mouse-capture bypass, commonly Shift-drag. In tmux, enable
mouse forwarding with `set -g mouse on`. Terminal shortcuts can vary.

## Set up a project

In an existing Git repository, press `e` before initialization to preview the
Markdown writing scope. `Enter` initializes it; `Esc` leaves it unchanged.
The command palette also offers **Set up writing scope**.
**Install Claude integration** separately previews managed settings and
instruction files before `Enter` installs them.

**Saved to file** describes disk content. **Not checkpointed** describes
history capture. **Needs review** describes undecided changes. These are
separate states. Finish review covers the entire writing scope. Its preview
shows counts and unseen passages. Use `d` for technical details.
Restoring a historical document creates a saved version that needs review.
