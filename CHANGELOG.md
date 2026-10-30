# Changelog

## 0.3.0

- Integrate Claude sessions with writing context, request delivery, and review feedback.
- Improve source editing, pointer selection, and navigation in the writing workspace.
- Preserve Unicode search matches and grapheme boundaries when refreshing edited text.
- Reuse manifest data during review application and expand benchmark verification.
- Add fuzz targets for Markdown analysis and text wrapping, with seed inputs and replay instructions.

## 0.2.0

- We are rocking.

- Edit Markdown source with autosave, selection, undo/redo, conflict handling,
  and recovery. Request agent edits to a selection while continuing to write.
- Resume local review drafts with their decisions and navigation. Saved choices
  remain unapplied until the ordinary finish confirmation.
- Find actions, documents, and headings through the searchable palette.
- Pin working snapshots or historical revisions as read-only references with
  independent navigation and search.
- Retain verified unchanged review decisions when checkpointing corrections.
- Allow mutations in the main checkout while additional worktrees exist;
  linked checkouts remain inspection-only.
- Place Margin on the left of a wide tmux desk with 70% of the columns and
  keep the agent on the right with initial focus.
- Improve macOS terminal and benchmark verification. Keep Git installer output
  suitable for PATH capture in the nightly support matrix.

Source editing remains limited to 1 MiB per document. Physical-terminal link
validation and pointer hover remain open work. See docs/remaining-gaps-plan.md.
