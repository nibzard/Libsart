# Supported terminal smoke matrix

This page records one release smoke run of the interactive surface. The
acceptance it serves is in
[SPEC 16.9](../SPEC.md#169-tui-and-interface-parity): buffer and
pseudo-terminal tests cover the review sequences, actual supported
terminals receive a release smoke test, and the command line and the
surface agree on results.

The tests are the authority. This page records what they observed on one
machine. Regenerate the observations with:

```bash
cargo test --test terminal_smoke
```

## The three layers

The surface is verified in three layers. Each layer proves what the one
above it cannot.

| Layer | Suites | What it proves |
| --- | --- | --- |
| Rendered buffers | `tui_shell`, `tui_changes`, `tui_finish`, `tui_history`, `tui_refresh`, `tui_access` | What the surface draws: text, focus, labels, wrapping, layout |
| One pseudo-terminal | `tui_lifecycle` | What the surface does to the terminal: modes, keys, cleanup, editor handoff |
| Supported terminals | `terminal_smoke`, `tmux_desk` | The same behavior across terminal profiles, and composition inside tmux |

A rendered buffer cannot prove physical terminal or assistive-technology
behavior. The smoke layer observes the bytes the process writes and, in
one case, the screen a real emulator shows.

## Environment

| Fact | Value |
| --- | --- |
| Date of the run | 2026-10-11 |
| Kernel | Linux 6.8.0-139-generic |
| tmux | 3.4 |
| Git | 2.43.0 |
| Rust | rustc 1.98.1 (48a229cea 2026-09-01) |
| terminfo | Every profile entry below is present |

## Observed matrix

`tests/terminal_smoke.rs` spawns the installed `margin tui` binary on a
fresh pseudo-terminal per profile. Every profile showed the same
behavior:

- The first frame drew the document.
- The surface entered the alternate screen (`ESC [ ?1049h`), enabled
  bracketed paste (`ESC [ ?2004h`), and turned raw mode on.
- One `q` key ended the process with exit code 0.
- The surface left the alternate screen, disabled bracketed paste,
  showed the cursor again, and restored cooked input.

| Profile | `TERM` | Geometry | Options |
| --- | --- | --- | --- |
| No `TERM` at all | unset | 110x32 | none |
| `xterm-256color` | `xterm-256color` | 110x32 | none |
| `xterm` | `xterm` | 110x32 | none |
| `screen-256color` | `screen-256color` | 110x32 | none |
| `tmux-256color` | `tmux-256color` | 110x32 | none |
| `vt100` | `vt100` | 110x32 | none |
| `linux` | `linux` | 110x32 | none |
| `dumb` | `dumb` | 110x32 | none |
| Classic geometry | `xterm-256color` | 80x24 | none |
| Color off | `xterm-256color` | 110x32 | `--color never` |
| Reduced updates | `xterm-256color` | 110x32 | `--reduced-updates` |

The surface queries no terminfo database. It writes the same mode and
drawing sequences whatever `TERM` says. The profiles above therefore
prove what the process wrote, not what each emulator would render. The
tmux case below is the one observation through a real emulator.

## Decoration and update modes

The default surface decorates with dim (`ESC [ 2m`) and bold
(`ESC [ 1m`) select-graphic-rendition sequences. With `--color never`,
the run observed none of them, and every state stayed labeled in words
and symbols. The buffer suite
(`color_off_renders_every_state_in_words_and_symbols`) proves the
wording behind that observation.

With `--reduced-updates`, the surface redraws only when a frame changed
(`reduced_updates_redraw_only_when_the_frame_changed`), and the
lifecycle suite proves the same mode cleanup as the default surface.

## One real emulator

The run composed the surface inside a private tmux 3.4 server started
with `-f /dev/null`, so no user configuration applied. The pane
environment was the isolated test environment carried through `env -i`.

- `capture-pane -p` showed the document, as the emulator interpreted
  the bytes and not as the test decoded them.
- A resize to 50x10 made the surface draw its minimum-size explanation
  ("Terminal too small").
- A resize back to 110x32 redrew the document at the restored size.
- A `q` delivered by `send-keys` ended the process, and with it the
  session.

## CLI and TUI parity

Three cases feed equivalent plans through both entry points and compare
the results. Commit timestamps may differ, so the comparisons are
semantic: trees, marker meaning, errors, and effects.

| Case | Where | What agrees |
| --- | --- | --- |
| Review plans | `equivalent_cli_and_tui_plans_agree_on_tree_marker_errors_and_effects` in `tui_finish` | Result tree, marker kind, scope, pinned pair, restored and kept groups, all six effect fields, and the `DIRTY_SCOPE` error code |
| Restore plans | `equivalent_cli_and_tui_restore_plans_agree` in `tui_history` | Result tree, restored paths, source revision, marker meaning, and the created commit |
| Through real keys | `a_selection_and_finish_sequence_applies_through_the_keys` in `tui_lifecycle` | The plan the surface composes from key presses, applied by `margin review` on an identical repository, produces the same tree, the same marker, and the same working bytes |

The parity case in `tui_lifecycle` runs the whole sequence over a
pseudo-terminal: select one passage with `r`, open the summary with
`Enter`, apply with a second `Enter`, and compare against stock Git.

## Sequence coverage

The required sequences of SPEC 16.9 and the tests that cover them:

| Sequence | Buffer evidence | Terminal evidence |
| --- | --- | --- |
| Selection | `k_and_r_change_only_the_in_memory_plan`, `every_group_renders_its_context_across_the_detail_pages` in `tui_changes` | `a_selection_and_finish_sequence_applies_through_the_keys` in `tui_lifecycle` |
| Finish | `the_summary_states_the_entire_scope_and_requires_explicit_intent`, `a_second_enter_applies_the_plan_through_the_shared_operation`, `a_finish_without_restorations_records_the_empty_review_commit`, `a_stale_comparison_refuses_to_open_a_finish` in `tui_finish` | The same lifecycle case as selection, including the commit oracle |
| Stale refresh | `polling_detects_new_working_edits_and_disables_application`, `polling_detects_a_moved_candidate_without_a_key_press`, `explicit_refresh_rebuilds_the_review_and_resets_selections` in `tui_refresh`; `stale_arrivals_relabel_every_group_and_refuse_decisions` in `tui_changes` | `a_stale_arrival_reaches_the_frame_without_a_key_press` in `tui_lifecycle` |
| Source and history | `the_source_view_shows_the_exact_markdown_changes`, `opaque_changes_keep_an_exact_source_fallback` in `tui_changes`; the `tui_history` suite; `document_and_source_context_survive_view_switches` in `tui_shell` | The Read view of every smoke profile draws the document |
| Editor return | `edit_requests_the_configured_editor_and_reloads_on_return`, `the_editor_capture_policy_saves_one_draft_and_approves_nothing` in `tui_access`; the `edit_handoff` suite | `the_editor_handoff_suspends_the_terminal_and_takes_it_back`, `a_failing_editor_child_is_stated_and_the_surface_survives` in `tui_lifecycle` |
| Resize | `a_resize_restores_the_reading_position_from_the_anchor`, `the_layout_earns_its_width_class`, `below_the_useful_minimum_the_surface_explains_and_keeps_quit` in `tui_shell` | `a_resize_redraws_and_keeps_quit_working` in `tui_lifecycle`; the resize cells of the tmux case |
| Errors | `an_invalid_initial_document_is_a_typed_refusal`, `the_command_line_refuses_redirected_streams_with_a_typed_error` in `tui_shell`; `unavailable_decisions_when_no_comparison_can_be_built` in `tui_changes`; `a_failed_inspection_claims_no_arrival` in `tui_refresh` | `a_terminal_error_still_restores_the_terminal` and the failing-editor case in `tui_lifecycle` |
| Quit | `quit_warns_once_about_unapplied_selections` in `tui_changes`; `cancel_and_quit_never_commit_a_selection` in `tui_finish`; `cancel_and_quit_never_restore` in `tui_history`; `ctrl_c_keeps_the_clean_quit_inside_text_entry` in `tui_access` | `a_normal_quit_restores_the_terminal`, `ctrl_c_quits_cleanly_and_restores_the_terminal` in `tui_lifecycle`; every smoke profile |
| Hooks | The `integrations` suite: Stop capture and refusal, prompt-submit capture, silent boundaries, duplicate no-op, missing events | The `hook_setup` suite through the installed executable |
| tmux | `missing_tmux_explains_the_two_tab_alternative`, `a_desk_without_a_terminal_refuses_typed` in `tmux_desk` | The composition cases of `tmux_desk` and the emulator case of `terminal_smoke` |

## What this machine cannot observe

- No graphical terminal emulator runs here. tmux is the only emulator
  that interpreted the bytes. GNOME Terminal, Konsole, kitty, Alacritty,
  wezterm, iTerm2, and Windows Terminal were not exercised. Their
  terminfo entries exist on this machine, but an entry is not an
  emulator.
- No assistive technology runs in this suite. Screen-reader and
  braille-display behavior remains unverified. The tested accommodations
  are the color-off labels and the reduced-update mode.
- `Ctrl-C` reaches the process as input byte `0x03` on the
  pseudo-terminal, which is how the keymap reads it in raw mode.
- macOS and Windows hosts are outside this machine. The pinned
  verification tiers record the environments a release claims.

## Source editing and desk requests

The next-release implementation adds `tests/source_editing.rs`. Its buffer
checks cover source selection, Unicode, conflicts, recovery, and request states.
`tests/autosave.rs` covers saved buffers, navigation, conflicts, and agent undo.
Its pseudo-terminal checks cover literal paste, save, resize, exit cleanup,
request submission, and application while the editor stays open.

A two-viewer case uses separate private desk sockets in one repository. A
result waits while its originating viewer has a request prompt open. The other viewer
does not apply that result. Existing tmux composition and external editor tests
continue to run. The external editor binding is now `E`; `e` opens source.

See [editing validation](editing-validation.md) for the current run. These
checks do not claim a live model session or physical terminal rasterization.

## Command palette and request checks

At 80×24 and 120×34, repeat these checks with color off and reduced updates:

1. Open `Ctrl+P` while source has an unsaved selection. Search, then cancel.
   Confirm that text, selection, cursor, and file bytes stay unchanged.
2. Select Unicode source with Shift+arrows, Shift-click, and backward dragging.
   Open `F2`. Check the document, selection preview, and character count.
3. Open **Requests**, select a request, and inspect details. Test pointer and
   keyboard controls. Retry delivery and confirm the owner stays unchanged.
4. Restart with a retained conflict. Open its comparison, then keep writer text.
   Confirm that the proposal remains inspectable and conflict status clears.
5. Preview setup and integration installation. Cancel each and compare files.
6. Open finish review. Check scope, unseen counts, and `d` details before cancel.
7. Restore a historical version. Confirm that the new version needs review.
