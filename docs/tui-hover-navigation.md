# TUI hover and navigation plan

Status: Implemented for all 11 numbered items. This is a completed design
record, not an active plan. Here, hover means keyboard selection. Pointer
hover remains separate; see the [remaining gaps plan](remaining-gaps-plan.md).

This document records the hover interaction design and the navigation improvements taken from Yazi (`sxyazi/yazi`). Each item states its value and cost. Items that do not earn their complexity are listed as deliberately skipped.

Source of the ideas: a review of Yazi's folder model (`yazi-core/src/tab/folder.rs`), external-change reconciler (`yazi-core/src/reconciler.rs`), directory scanner (`yazi-fs/src/scanner/scanner.rs`), and preview locking (`yazi-core/src/tab/preview_lock.rs`), mapped onto the Margin SPEC.

## 1. Hover, adapted to Margin

Yazi keeps one selection state, called hover. All other surfaces follow it. Margin uses the same rule: the hover lives in list-like surfaces, and the main column follows.

1. **One hover per surface, no duplicates.** A hovered document exists in the navigator and the document picker. A hovered change group exists in Changes. A hovered revision exists in History. Each surface owns at most one hover. `Tab` moves the input focus between surfaces. No second "active document" concept exists anywhere. The navigator, the reading column, and Changes read the same hovered-document state. This makes the view-switch state retention of SPEC 5.4 automatic.
2. **The reading column follows document hover.** `j` / `k` in the navigator moves the hover. The reading column shows the hovered document at its saved anchor. The focus does not move.
3. **History hover shows an instant before/after.** When the user hovers a revision, the main column shows the diff of that document against C. The data is read-only and needs one `git diff` for one file. No plan state changes (SPEC INV-12).
4. **Changes hover drives the excerpt focus.** `n` / `p` moves between change groups. The hover decides what the details view (`d`) acts on, so the excerpt and the details never disagree.

## 2. Navigation improvements taken from Yazi

| # | Item | Effort | SPEC anchor | Note |
| --- | --- | --- | --- | --- |
| 5 | Preview-lock on fast navigation | S–M | 11.4, 11.5 | While `n` / `p` or navigator `j` / `k` repeats quickly, keep the last settled content in the main column. Recompute after a short quiet period. Label the pending target and disable content-dependent actions until the displayed content matches its identity and context. This removes flicker and makes items 2 and 3 feel good. |
| 6 | Ticket-tag every background result | S | 13.3 | Tag each worker job with its generation: search, diff detail, history pages. Drop stale results at one choke point. The live-reload work already does this for the open surface; make it the rule for all async surfaces. |
| 7 | Preserve hover identity on reload | S | 11.2 | Preserve the hovered item by stable identity: document path, change ID within its pinned review, or revision OID. Keep the navigator's existing path-based preservation. Only when the item disappears, fall back to its old index clamped to the new list; clear hover when the list is empty. Insertions and deletions in the same reload must not move hover off a surviving item. |
| 8 | Visible incompleteness | S | 6.3, 11.3 | The search panel and paginated lists show "still scanning" or "N more omitted" as a first-class row. The data (`complete`, `omitted_detail_count`) already exists; show it at full visibility. |
| 9 | Per-document anchor map | S–M | 11.2, 5.4 | Keep a small map of `path → source anchor (+ best-effort offset)` per view across view switches, with a size cap. |
| 10 | Documented total order for the navigator | S | 11.2 | Define one sorter with explicit tie-breakers. Test that the order is stable across reloads. This makes item 7's fallback predictable; surviving items are retained by identity regardless of index changes. |
| 11 | Startup and binary-size CI check | S | 17 | Measure cold start and release binary size in CI. This makes the SPEC 17 budget enforceable. |

Suggested order: 5, then 2, then 3 (hover core); then 7 and 10 together, because they are coupled; then the rest. Items 6–8 extend the live-reload and search code, so they cost least while that code is fresh.

Acceptance checks for items 5 and 7:

- While a preview is pending, `K`, `r`, `u`, `N`, `d`, `e`, and restoration/finish actions are disabled with a visible reason, including through the action menu. A rejected action is not queued or replayed when the preview settles. Navigation and cancellation remain available. A rapid `n` → `r` must leave the plan unchanged; a fresh `r` after the matching preview appears targets the displayed group. Late results for an earlier target cannot unlock actions.
- A reload from `[a,b,c,d]` to `[b,bb,c]` keeps `c` hovered. Also check insertion-only and deletion-only reloads, removal of the hovered item, and an empty result. When the hovered item disappears, the fallback selection and its displayed content must obey the same pending-preview action rule.

## 3. Implementation status

The implementing change (`tui-hover-navigation`) recorded what it built and what it found already present:

- **Item 1** — the single hover already existed as the shell's selection state; the change closed the one divergence (a reload whose fallback moved the selection while the column kept the old document now obeys the pending rule) and repositions the navigator cursor with the kept hover.
- **Item 2** — the reading column already followed the navigator; the change deferred that load to the quiet pass (item 5) and restored the saved anchor (item 9).
- **Item 3** — new: the `HoverExcerpt` of the History view, one bounded single-file patch through the fixed diff contract, recomputed only when the hover or the candidate moved.
- **Item 4** — already true: `d` and every decision key act on the selected group. No change.
- **Item 5** — new: the pending navigation target, its label in the context row, and the gate on `d`, `u`, `N`, `e`, `K`, `r`, and both `Enter` actions, in the keys and in the action menu.
- **Item 6** — already true at both choke points (inspection generations, search request IDs); no change was needed.
- **Item 7** — the document list already kept its hover by path; the change added the identity-kept change group across one reload of the same pinned review, the acceptance matrix, and the pending fallback.
- **Item 8** — the search panel already stated its states; the change added the discovery bound row and the excerpt bound notice.
- **Item 9** — new: the saved-position map, capped, restored on load.
- **Item 10** — the order already was ascending UTF-8 path bytes in both builders; the change documented the rule in one place and pinned it with a test.
- **Item 11** — new: the `budgets` CI gate (stripped release size and a `--help` startup smoke ceiling at ten times the P-HELP budget).

## 4. Deliberately skipped

These items were considered and rejected. The reason is recorded so they are not re-proposed without new evidence.

- **Three-column layout.** Margin has one reading measure (SPEC 11.1). A second live column competes with it. Hover gives the behavior without the geometry.
- **Tabs, marks, yank/paste verbs, relative motion counts (`5j`).** This is file-manager machinery. A writing desk does not review five documents at once.
- **Mouse support (historical decision, superseded).** Wheel and click support
  are now implemented. See the [pointer design](tui-pointer-clicks.md). Pointer
  hover remains separate.
- **Kitty keyboard protocol.** The keymap is single-key simple. The default crossterm parsing is sufficient.
- **Crate split, async runtime, plugin system.** Rejected by SPEC 13.1, 13.4, and 13.6. Yazi's scale justifies them; Margin's does not.
