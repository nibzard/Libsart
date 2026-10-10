# C0 correctness corpus

The C0 corpus is the hand-reviewed Markdown fixture set of SPEC 16.7 and
17.2: at least 60 small source pairs with recorded expectations for source
comparison, grouping, formatting, Unicode handling, and adversarial
content. `tests/c0_corpus.rs` validates the corpus itself against stock
Git; the grouping slice (T024) must meet these expectations, and the
restoration slice (T025) reuses them for exact keep/restore checks.

## Layout

- `mod.rs` — the fixture types, the module registry, and the class
  vocabulary (`REQUIRED_CLASSES` from SPEC 16.7, `EXTRA_CLASSES` for the
  whole-file decision units of SPEC 10.3 and 10.4).
- `punctuation.rs`, `whitespace.rs`, `structure.rs`, `links.rs`,
  `adversarial.rs`, `transitions.rs` — the fixtures, grouped by content
  class family.
- `tests/c0_corpus.rs` — the validation suite (outside this directory).

## What each fixture records

| Field | Meaning |
| --- | --- |
| `id`, `title` | Stable identity and a one-line description. |
| `classes` | Content classes covered, drawn from the vocabulary. |
| `path` | Repository-relative document path. Paths stay single-segment so expected result trees stay flat. |
| `before`, `after` | The exact B and C bytes: valid UTF-8 as `Content::text`, non-UTF-8 as `Content::opaque`; a side may be `Side::ABSENT`. |
| `changes` | The exact source changes under the fixed contract of SPEC 10.1: file-level kind, binary-patch flag, and every hunk range. |
| `expectation` | Either independent `Group`s whose hunks partition the patch, or `WholeFile` with the fallback reason. Each group carries the exact bytes of restoring only that group. |
| `reader` | What a reader must still be able to perceive. A rendering that hides the change fails. |

Expected results follow from the bytes: keep-all yields C, restore-all
yields B, and restoring one group yields that group's `restore` content
while every other change stays. The validation suite proves each of these
through stock Git — `git diff-tree` under the SPEC 10.1 flags for the
recorded changes, `git apply --reverse --index` for every restoration, and
`git mktree` plus `git write-tree` for exact result-tree equality. It also
proves that restoring every group in descending hunk order recomposes B
exactly, which is the composition property the restoration slice extends
to every subset.

## Class vocabulary

`REQUIRED_CLASSES` are the 23 classes of SPEC 16.7 (slugs; `moved-paragraphs`
covers "duplicate/moved paragraphs"). `EXTRA_CLASSES` adds `line-endings`,
`file-presence`, and `file-mode`, the whole-file decision units that are
not content classes. A fixture may carry several classes when one pair
exercises two of them, for example tabs inside a fenced code block.

## Review protocol

The pairs, group assignments, reasons, and reader notes were authored and
reviewed by hand. The hunk layouts and per-group restoration bytes were
produced once with stock Git 2.43.0 under the fixed contract and then
frozen into these modules; the validation suite re-derives them on every
run, so the data cannot silently drift from what Git produces. Editing a
fixture means editing the bytes and the recorded expectations together and
re-running `cargo test --test c0_corpus`.

Keep these rules when extending the corpus:

1. Author synthetic text only, never private drafts (SPEC 17.2).
2. Give independent groups at least ten unchanged lines of separation so
   the fixed contract emits one hunk per region.
3. Record a whole-file fallback whenever independence cannot be
   established from both versions; reference links, footnotes, opaque
   bytes, presence transitions, and mode changes are always whole-file.
4. State the reader-visible difference for every fixture; word-highlight
   quality is assessed separately from source coverage.
5. Keep fixture bytes exact: these modules are marked `-text` in
   `.gitattributes` so Git never normalizes them.
