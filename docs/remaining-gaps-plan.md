# Close the remaining documentation gaps

Status: Planned. Implementation and physical terminal checks have not started.
Created: 2026-10-15. See the [plan index](README.md) for completed work.

## Goal and scope

Close the remaining gaps identified in the completed plans. Keep new pointer
behavior separate from terminal validation and conditional notes work.

| Work | Current state | Completion condition |
| --- | --- | --- |
| Pointer hover | Unimplemented; keyboard hover and hit maps exist | Implement the interaction and pass the checks below |
| Physical terminal links | Automated URL and desk tests pass; physical clicks remain unverified | Record supported-terminal results and limitations |
| Notes acknowledgments | Deliberately deferred; feedback repeats within bounds | Record evidence and decide whether implementation is justified |

## 1. Implement pointer hover

Reuse the last rendered hit map and the existing keyboard selection paths.
Motion over a navigator entry, change group, or revision selects that item.
Motion does not move keyboard focus, approve a change, open editing, or activate
menus, links, tabs, or search results. Motion outside these targets does nothing.

Keep the existing preview delay and pending-target guards. Repeated motion over
the same item must not request another preview or redraw. Source editing,
text entry, confirmation dialogs, and overlays ignore motion. Resizing clears
old targets until the next draw. Hover must not park or replace a source buffer.

1. Extend pointer event filtering to admit only supported motion for hover.
2. Route eligible targets through existing selection functions and context guards.
3. Schedule redraw only when visible state changes. Avoid per-motion Git inspection.
4. Add focused shell and event-loop checks in default and reduced-update modes.
5. Update the pointer contract and terminal checklist with the implemented behavior.

Acceptance checks:

- Navigator, Changes, and History follow eligible motion without changing focus.
- A rapid burst settles only the final preview. Stale results cannot unlock actions.
- Repeated motion over one target causes no extra preview work or frame.
- Editing, dirty buffers, overlays, and confirmations remain unchanged by motion.
- Empty maps, clipped rows, padding, and stale geometry cannot select an item.
- Ignored motion does not delay keyboard input, autosave, refresh, or worker replies.
- Existing click, wheel, navigation, and source-editing tests still pass.

## 2. Validate physical terminal links

Use the documented opt-in kitty adapter. Record the actual operating system,
terminal version, tmux version, command, result, and date. Do not infer a
successful physical click from URL parsing or command-line tests.

1. Generate a link inside a running desk for a path with spaces and Unicode.
2. Click it in kitty and confirm the intended document and source line appear.
3. Repeat through tmux with hyperlink support, then after detach and reattach.
4. Run two desks and confirm each link targets its own viewer without moving focus.
5. Close one desk and confirm its old link cannot open the other viewer.
6. Check agent-rendered output separately; record preserved, suppressed, or reformatted links.
7. Add the results to the terminal smoke matrix and update the desk-link limitations.

Completion requires recorded results for the supplied kitty adapter. Agent output
that strips custom links remains an explicit limitation with `margin open PATH`
as the fallback. Other terminal handlers require separate scope and validation.
If the required physical terminal is unavailable, keep this item open and record
the missing environment. Do not mark automated tests as physical evidence.

## 3. Decide whether notes need acknowledgments

This is an evidence gate, not a commitment to implement tracking. The current
contract delivers reachable writer notes each turn within explicit bounds.

1. Observe repeated delivery during several real multi-turn writing sessions.
2. Record whether repetition obscures new feedback or causes repeated agent work.
3. Keep the current behavior if no material problem appears, and record the decision.
4. If a problem appears, write a separate design before changing delivery semantics.

A follow-up design must define the consumer identity, delivery versus processing,
retries, blocked prompts, new sessions, adoption changes, and crash recovery.
It must preserve feedback until delivery succeeds. Extend the existing notes
and hook tests before changing the contract. Do not silently consume notes.

This item closes with either a documented decision to retain repeated delivery
or an accepted follow-up plan backed by session evidence.

## Completion and evidence

Implement pointer hover first. Physical terminal validation can run independently.
Notes observation can use the same writing sessions. Update this page after each
item with its outcome, test commands, and evidence links. Mark the whole plan
implemented only after pointer hover and physical validation pass, and the notes
decision is recorded. Keep deliberate product exclusions outside this plan.
