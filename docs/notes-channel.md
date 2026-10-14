# The notes channel

Status: implemented; acceptance coverage lives in `tests/notes_channel.rs`
and the notes/instruction parser unit tests. The contract of record is
[SPEC 12.7](../SPEC.md) and [the notes schema](../schemas/notes-v1.json);
this document holds the reasoning and the staged build order.

## Why

A diff states what changed; it cannot state why. Agents that rewrite a
passage leave the writer guessing at intent, and the writer's reaction —
"too terse", "restore the example" — lives in chat scrollback where
neither the repository nor the next agent turn can read it. Notes give
both directions a durable home with no new infrastructure: one Git notes
namespace, `refs/notes/margin`, holding one note blob per commit in the
notes-v1 envelope.

The design borrows two proven shapes: inline annotations beside each
edited passage (hunk's review stream) and annotations that flow back to
the agent (plannotator's feedback loop). Margin's difference from both
stays the same as the rest of the product: the writer sees prose and
decisions, never Git vocabulary; the agent sees native commands.

## The mechanism

- One note blob per commit on `refs/notes/margin`; `margin note` does
  a guarded read-modify-write so callers only ever append one entry.
- Each entry carries `by` (`agent` or `writer`), an optional `group`
  digest anchoring it to one change group of the comparison the commit
  pins, and bounded `text` (2000 characters, 50 entries per commit).
- The CLI is the writing contract: `margin note --text "…"` for a
  commit-wide remark, `margin note --group <digest> --text "…"` for a
  group annotation, `margin notes` to list the candidate's entries, and
  `margin notes --revision <commit>` to read an earlier version. Agents discover it the way
  they discover every Margin command: `--help`. The raw envelope stays
  documented for native-Git writers (`git notes --ref=margin add`).
- Guards: a note never touches the working tree, never enters the diff
  or a capture, never moves `HEAD`, never invalidates a pinned
  comparison. Writing needs the Git identity Margin already requires.
  The JSON result reports a changed or unknown notes ref separately from
  the unchanged document, index, and adopted-branch history effects.

### Validation and atomic append

The JSON Schema validates the envelope's shape and bounds. The semantic
validator additionally requires unique `id` values within each commit's
`notes` array; `uniqueItems` only rejects identical complete entries. Both
CLI and native-Git envelopes pass through these same two validation layers
when read. An invalid envelope produces a diagnostic and is never silently
replaced by an append.
Readers cap raw envelope input at 2 MiB before parsing, including malformed
native-Git notes and whitespace. Presentation escapes terminal controls.

An append holds Margin's shared mutation lock, snapshots the notes ref
OID (including its absence), and reads the envelope from that snapshot.
It builds the replacement notes commit on a private temporary ref, then
publishes it with `git update-ref refs/notes/margin <new> <expected-old>`
(the all-zero OID means the ref was absent). It must never force-replace
the public notes ref through `git notes`. Native-Git writers do not hold
Margin's lock, so the expected-old check is required even while locked.
On a ref conflict, reread, revalidate, and rebuild against the new snapshot;
after three publication attempts, refuse with a retryable conflict. Remove
the private ref on success or failure. A concurrent entry, including one
on a different commit, must survive a successful append.

Generate the new entry's ID once per invocation and preserve it across
publication retries. If that ID already exists with the identical entry,
return it without appending; a different entry with that ID is a conflict.
Recheck the 50-entry limit after every reread. Native-Git writers that
replace an existing envelope must also guard their public ref update;
an unconditional native force-write cannot promise preservation.

### The instruction block

Discovery needs a standing layer beside `--help` and the hook reminder:
one bounded block in the agent instruction files that tells the agent
the channel exists. Installing it rides the same explicit surface that
installs the hook entries, with the same discipline (SPEC 12.4): it
never runs as a side effect of another command, it rereads before
writing and refuses when the file changed underneath it, and `--dry-run`
prints the exact lines. `CLAUDE.md` and the tool-agnostic `AGENTS.md`
each carry the block; a missing file is created only by the explicit
install.

The block is fenced by Margin's own markers. Markers locate a candidate
block; they do not establish ownership of its contents. Update or removal
requires exactly one complete marker pair whose entire block matches a
recognized shipped template for the declared version, including the
markers. Match using the file's existing LF or CRLF convention and preserve
every byte outside the block. An unchanged current template is a no-op on
install; a recognized older template can be updated. Edited contents,
unknown versions, duplicate blocks, nested markers, and unmatched markers
are conflicts: preserve the file and report the conflict without appending
another block. With no markers, install may append a block and removal is
a no-op. Validate all instruction files and the hook configuration before
the first intended write, and retain the reread-and-refuse checks before
publishing each file.
Version 1 is the first shipped template; no older template is recognized
yet. Future template versions must retain recognized earlier bytes to
support the update rule above.

```markdown
<!-- margin:notes v1 -->
After you checkpoint a draft, explain each meaningful change with Margin:
`margin note --group <digest> --text "<why>"` — group digests come from
`margin diff --json`. Writers read these notes beside your edits and
answer with their own; `margin notes --json` lists theirs. Native Git
equivalent: `git notes --ref=margin`; `margin notes --schema` prints
the envelope schema. IDs must also be unique within each commit's notes.
<!-- /margin:notes -->
```

The wording points the agent at the CLI, not the schema: the CLI is the
writing contract, and the envelope is the documented fallback.
`margin notes --schema` ships in stage 1, prints the raw notes-v1 JSON
Schema to stdout without the normal result envelope, and requires no
repository or adoption. It performs no writes and is an alternative to
listing notes, so agents can obtain the installed version's schema from
any project without assuming Margin's source files are present.

### Feedback selection and delivery

The hook reconstructs feedback from Git, independently of any running TUI.
At `UserPromptSubmit`, before attempting the configured capture, snapshot
the adopted branch's HEAD and the notes ref. Select `writer` entries on
the first-parent ancestry from that HEAD back through the current scope's
starting marker, inclusive. Use the same starting-marker discovery as
adoption; do not use the latest review marker as the lower bound. A later
checkpoint or review therefore does not hide feedback on an earlier
candidate. A new start resets this range; unreachable commits and commits
before that start are excluded. If adoption cannot be validated, report
that feedback is unavailable instead of guessing a range.

Read envelopes from the snapshotted notes ref. Order commits newest first
and entries within each commit in envelope order; include each entry's
commit, ID, and optional group digest with its text. Deliver at most 50
entries and 16 KiB of UTF-8 context, including labels and a truncation
notice. Truncate only at character boundaries and explicitly disclose
omitted or shortened feedback. Invalid envelopes are disclosed and skipped,
not interpreted as agent instructions or allowed to suppress valid peers.

Then run the existing capture policy. When the prompt is allowed, emit the
selected feedback as supported `UserPromptSubmit` hook context even if
capture advanced HEAD, or capture is manual. When capture blocks the
prompt, preserve the existing blocking response; no feedback is consumed,
and the next allowed prompt selects it again. Notes arriving after the
snapshot appear on the next turn. Delivery remains independent of TUI
re-pinning, process lifetime, and history presentation filters.

## Stages

Each stage ships alone; no stage migrates earlier data.

1. **Commit notes.** Git-adapter notes commands (`show`, `list`, and the
   guarded atomic append behind `margin note`); the `note` and `notes` CLI commands;
   History renders one note row under its commit; `margin log` and its
   JSON envelope carry notes; the inspection worker watches the notes
   ref head so an open TUI shows fresh notes within one tick. Stage 1
   also ships the agent instruction block above, so discovery of the
   channel begins with the first install.
2. **Group-anchored notes.** The Changes view renders each note beside
   its group. Notes anchor to the pinned candidate; after a re-pin they
   stay with their commit (visible in History), and a note whose group
   left the comparison states itself once as an orphan.
3. **The writer's answer.** `N` opens a text field on the selected
   group (the existing field machinery: bracketed paste safe, Enter
   submits, Esc cancels) and stores a `writer` entry. A
   `UserPromptSubmit` hook entry — installed by the same `margin hook
   install claude` surface — emits the pending writer notes as hook
   context, bounded per turn. The hook context names `margin note` so
   the loop teaches the agent its own half.

## Settled decisions

- **Envelope from day one.** Structured entries cost nothing in stage 1
  and stage 2 never migrates data. Agents never hand-write JSON: the
  CLI writes the bytes.
- **No acknowledgment bookkeeping.** The hook surfaces all writer notes
  in the current adoption's reachable first-parent range each turn,
  bounded as specified above. A consumed marker is
  deferred until repetition proves noisy.
- **Naming.** CLI: `margin note` (append), `margin notes` (list). TUI
  word: "Note". Authorship prints in words — "Note (agent)", "Your
  note" — never hue.
- **Surfaces.** Changes and History carry notes; the Read view stays
  clean. A note is an annotation on a change, not document content.
- **Claimed authorship.** `by` is a claim, like Git's committer field;
  the interface states it in words and Margin does not authenticate it.
  A signing contract would be a different, explicit decision.
- **One group identity.** The change-group digest is the only identifier
  `diff --json`, review plans, and notes ever name (SPEC 10.3); the
  notes schema leans on that stability, so the digest contract is now
  specified rather than implied.
- **Instruction files are the user's voice.** Margin writes there only
  through the explicit install, only inside its marked block, and only
  with the exact patch shown first by `--dry-run` (SPEC 12.4).

## Test shape

Agent-side writes through both paths (CLI and native `git notes`) must
render identically; a note must leave `status`, `diff`, and the working
tree byte-identical (the same read-only oracle the TUI suite uses); a
re-pin must orphan a group note without losing it; the hook round trip
needs the same fingerprint-refusal tests as the existing hook entries.
Validation fixtures must reject repeated IDs even when author or text
differs, accept distinct IDs with identical text, and preserve invalid
native envelopes on refused append. Concurrent-write tests must interleave
a native ref update between snapshot and publication, on both the same
commit and another commit; cover absent-ref creation, retry exhaustion,
and a concurrent append reaching the entry limit.

Hook fixtures must deliver an older candidate's feedback after a checkpoint
and after a review, including a checkpoint created by that same prompt
hook. Cover manual capture, a blocked prompt followed by successful retry,
TUI exit/re-pin, a new start, unreachable history, invalid envelopes, and
deterministic truncation with multibyte text. These are stage acceptance
tests exercised alongside the notes commands and hook delivery.

Instruction-file fixtures must cover missing files, unchanged repeated
install, recognized older-template update, and removal, preserving bytes
outside the block with both LF and CRLF files. Edited contents, unknown
versions, duplicate blocks, nested markers, and missing opening or closing
markers must refuse without changing either instruction file or the hook
configuration during preflight. Cover concurrent edits after preflight with
the same fingerprint-refusal checks as hook configuration. From a directory
outside any repository, `margin notes --schema` must emit the shipped schema
as parseable JSON without creating files.
