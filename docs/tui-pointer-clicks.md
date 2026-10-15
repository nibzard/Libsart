# TUI pointer clicks plan

Status: implemented. Pointer hover remains deferred until the hover core lands.

This document records the click interaction design. It extends the
mouse-wheel work (task T101) and shares its foundation with the hover plan
of `tui-hover-navigation.md`. The skip entry for mouse support in that
document said mouse could be added later without a redesign; the wheel
work kept that promise, and this plan is the later addition. Its premise
changed: mouse capture is now always on, so the surface already pays the
capture cost — clicks answer input the surface already receives and drops.

## 1. Principle

Every click runs an action the keyboard already runs (SPEC 11.3, 11.4).
No click is the only path to anything, and no click changes review state
in a way its key twin does not. Clicking a pane also moves the visible
focus there, as `Tab` does, subject to the context precedence in section 3.

## 2. The pointer hit map

Today only `viewport_rows` survives a draw. Clicks need to know which
item each screen row shows, and that knowledge exists only inside the
render functions.

1. **The renderer records what it draws.** Each `draw_*` function in
   `render.rs` fills one `PointerTargets` map on the Shell while it draws:
   for each clickable region, the screen rows and what they show. The map
   is cleared at the start of every draw and rebuilt from the visible,
   clipped rows of that frame.
2. **Regions resolve to typed targets.** Each target has a screen region
   derived from the existing layout and rendered rows. One target enum:
   `NavigatorEntry(index)`, `ViewTab(View)`, `ChangeGroup(index)`,
   `Revision(index)`, `MenuEntry(index)`, `SearchHit(index)`, `Pane(Focus)`.
   A click asks
   the map for the target at its column and row; the shell routes the
   target to the same functions the keys use. Item targets take precedence
   over their containing pane. Pane targets cover only visible focus stops:
   Documents when the navigator is available, Reading, and Actions while
   the menu stands. Headers, footers, context rows, and hidden panes have
   no pane target; tabs retain their explicit item targets.
3. **A click before the first draw hits nothing.** An empty map is a
   no-op, like any other mouse event today.
4. **Layout stays pure.** `frame_layout` already computes the rectangles;
   the map records only what a rectangle's rows display, not new geometry.
   A resize empties the frame until the next draw redraws it.
5. **Wrapped rows keep their owner.** All visible continuation rows of a
   menu entry, change group, or revision resolve to that same item; both
   the path and excerpt rows of a search hit resolve to that hit. Apply
   the actual scroll offset and clipping before recording regions. Titles,
   separators, status rows, and blank padding are not item targets. A
   partially visible item owns only its visible cells; offscreen content
   has no target.

## 3. Click actions

| # | Item | Action (key twin) | Effort | SPEC anchor |
| --- | --- | --- | --- | --- |
| 1 | Navigator row | Set the cursor to that entry and open it, as `j`/`k` plus selection do today; focus moves to Documents | S | 11.3, 11.4 |
| 2 | View tab (`1 Read` …) | Switch view, as `1`/`2`/`3` | S | 11.3 |
| 3 | Change-group row | Select that group and scroll it into view, as `n`/`p` to it | S | 6.3, 11.3 |
| 4 | Revision row | Select that revision, as `j`/`k` to it | S | 5.5, 11.3 |
| 5 | Action-menu entry | Move the selection to it and run it, as `j`/`k` plus `Enter` | S | 11.3 |
| 6 | Workspace-search hit | Select it and open it, as arrows plus `Enter` | S | 11.3 |
| 7 | Visible pane without an item target | Move focus to the clicked pane, as `Tab`, when the current context permits it | S | 11.4 |
| 8 | Right click in a drawn frame | Open or close the action menu, as `:`, only in the contexts below | S | 11.3 |

Route button presses through this precedence before resolving or running
any target. The same rules apply even to visible navigator rows and tabs
outside an overlay's rectangle:

1. **No valid drawn map:** all clicks, including right clicks, do nothing.
2. **Finish summary or restore preview:** all clicks do nothing.
3. **Search, Picker, or Note text entry:** all clicks do nothing.
4. **Help:** all clicks do nothing; its keyboard close action remains.
5. **Action menu:** left clicks may activate only `MenuEntry` targets or
   focus its `Pane(Actions)`. Right click closes the menu through its
   existing close action. Navigator, tab, and underlying content targets
   are blocked, regardless of keyboard focus.
6. **WorkspaceSearch:** left clicks may activate only `SearchHit` targets
   through the existing submit/revalidation path. Other clicks, including
   right clicks and pane-focus clicks, do nothing; the query is unchanged.
7. **No overlay:** left clicks resolve item targets first, then pane focus.
   Group and revision clicks also focus Reading; navigator clicks focus
   Documents. Right click opens the action menu. Unmapped left clicks do
   nothing.

Only left and right button-down events have actions. Releases, middle
clicks, and drags do nothing. Menu activation uses the existing disabled
action checks. No click reaches content underneath a standing context.

## 4. Hover: shared foundation, later phase

The hit map is exactly what pointer hover needs. The hover plan defines
hover as keyboard-driven state (`tui-hover-navigation.md` items 1–4);
pointer motion then becomes one more source for the same state once that
core lands:

- `MouseEventKind::Moved` over a navigator row, group row, or revision
  row sets the hover the arrow keys set. Nothing else changes.
- Motion is frequent, so hover state must stay cheap to recompute and
  must join the reduced-updates fingerprint (the wheel work set this
  pattern for the preview offset).
- The hover plan's preview-lock (its item 5) bounds rapid hover churn the
  same way it bounds rapid `j`/`k`; clicks inherit that protection for
  free once they route through the same actions.

Pointer hover is not scheduled by this plan. It waits for the hover core.

## 5. Tests

Buffer tests drive `handle_event` with `Event::Mouse` of
`MouseEventKind::Down(MouseButton::Left)` after one `draw`, so the hit
map exists; they assert through the same accessors the wheel tests use.

- Clicking a navigator row opens that document and moves focus; clicking
  a tab switches view; clicking a group or revision selects it; clicking
  a menu entry runs it; clicking a search hit opens it.
- Clicks over the finish summary, the preview, and text entry change
  nothing, including right clicks and clicks on visible tabs or navigator
  rows. A click with an empty map changes nothing, including right click.
- Test each overlay's routing: Help ignores all clicks; Menu accepts its
  entries and closes on right click but blocks navigator and tab clicks;
  WorkspaceSearch opens only its own hits through revalidation and ignores
  right clicks without changing its query. Disabled menu actions retain
  their existing refusal behavior.
- Clicking pane padding moves focus only to a visible focus stop in an
  allowed context. Item targets win over pane targets. Header, footer,
  context-row, and hidden-pane clicks do nothing outside explicit tabs.
- At narrow widths, click each visible continuation row of a wrapped menu
  entry and assert the same action. Test both rows of a search hit, wrapped
  group/revision content, and partially clipped items. Separators and
  status rows must never activate an adjacent item.
- Scroll the navigator, menu, changes, timeline, and search results, draw,
  then click known visible items and verify their identities rather than
  their screen-row indices. Include navigator heading entries. After a
  resize, clicks before the next draw do nothing; after that draw, only
  the new geometry responds. A too-small frame exposes no click targets.
- The no-repository-bytes list in `tui_shell.rs` gains click events.
- One PTY case sends SGR press bytes (`ESC [ < 0 ; COL ; ROW M`) and
  asserts the clicked document opened; release bytes (`… m`) assert as
  no-ops.

## 6. Fix unused pointer-motion rendering (P2)

Complete this alongside the click foundation. `EnableMouseCapture` enables
any-motion tracking (`1003`), so moving the pointer currently wakes the
event loop even though motion has no action. Skipping the Git watch request
avoids worker traffic, but `Shell::handle_event` still computes two frame
fingerprints, and the default (non-reduced) loop still renders a full frame
for every ignored event.

- Filter mouse events with no implemented action before shell
  fingerprinting and rendering. Preserve wheel handling and the click
  events introduced by this plan; releases and unsupported drags remain
  no-ops. Alternatively, configure capture without motion reporting until
  pointer hover needs it.
- Ignored events must not postpone periodic inspection, worker replies,
  search completion, resize handling, or keyboard input. When hover lands,
  enable motion deliberately and redraw only when its visible state changes.
- Verify default and reduced-update modes: a burst of ignored motion and
  release events causes no extra render or per-event Git watch request;
  wheel and supported clicks still update the frame, and keyboard input
  remains responsive during the burst. Test at the event-loop boundary,
  since the existing shell redraw-flag test cannot detect default-mode
  renders.

## 7. Deliberately skipped

- **Drag and drag-select.** Text selection stays the terminal's own job
  (Shift-drag bypasses capture). In-surface dragging earns nothing over
  the wheel.
- **Double-click.** No action needs it; the menu runs on one click.
- **Reading-column row clicks.** The reading column has no per-row action
  today; position-following belongs to hover.
- **Scrollbar interaction.** The surface draws no scrollbar; one is a
  design change, not a pointer change.
- **Positional menus or tooltips.** Menus stay centered lists; positional
  UI is a redesign the skip list of the hover plan already rejects.

Suggested order: the hit map (section 2) together with the navigator and
tab clicks (table rows 1–2), then rows 3–6, then rows 7–8 and the PTY
case. The whole plan is one task-sized change set beyond the map; the map
is the only structural work.
