# Search implementation plan

Status: proposed; implementation has not started.

## Goal

Make cross-document search fast, ranked, typo-tolerant, and useful for finding
Markdown passages. Preserve a predictable literal search mode. Add optional
semantic retrieval only after measuring whether it improves real searches.

Search remains read-only with respect to documents, Git state, and review
decisions. Its domain is the TUI's listed working documents, including
pre-adoption discovery; it does not search arbitrary repository files or history.

## Current implementation and constraints

- `src/tui/search.rs` scans source lines using case-insensitive substring
  matching, with a background worker, bounded pending jobs, and cancellation.
- `src/tui/shell.rs` owns query editing, submission, selection, cancellation,
  and validation against disk before opening a source-line anchor.
- `src/tui/render.rs` renders paths, lines, excerpts, and scan coverage.
- `src/tui/run.rs` polls background work and controls redraws.
- SPEC 11.3 and README currently promise literal, Enter-submitted search.
  Update these contracts with the corresponding implementation milestones.
- Current limits are 256 KiB per file, 16 MiB per scan, 2,000 files, and 500
  matching lines. Invalid UTF-8, binary, unreadable, non-regular, and symlinked
  files are skipped. Preserve these limits for Exact mode initially.
- The current navigation check requires a literal substring. It cannot be
  reused unchanged for fuzzy or semantic hits.
- Respect the declared Rust 1.89 minimum and the repository's existing
  performance, accessibility, and verification requirements.

## Target interaction

`Ctrl+F` opens a search panel with an explicit Smart/Exact mode indicator.
Smart becomes the default after the ranked-search milestone passes its gates.
Exact retains the current case-insensitive substring semantics, including
literal punctuation. `/` continues searching the current view.

```text
Search all documents · working source · Smart
> garden maintenence

› gardening.md · Seasonal care · line 42
  …regular garden maintenance prevents weeds…
  Typo match: maintenance

  notes/spring.md · Weekend tasks · line 18
  …maintenance of the garden beds starts in March…

Preview: selected passage with surrounding source

↑↓ select · Enter open · Ctrl+F refresh · Ctrl+T mode · Esc close
```

- Debounce edits by approximately 100 ms; cancel obsolete work immediately.
  Enter flushes a pending query and opens only a result for that query once
  available. It must never open an old result after an edit.
- Preserve query, mode, and selection identity when returning from a result.
- Show matching spans and explanatory labels without relying on color.
- Show a bounded preview on wide/tall terminals; omit it on small terminals
  while retaining path, location, excerpt, and coverage status.
- Group adjacent hits into passages and initially cap displayed passages per
  document, with an explicit action to reveal additional matches.
- Ordinary text-entry characters remain text. Verify proposed `Ctrl+T` against
  the key map before implementation; expose mode switching in contextual help.
- Distinguish searching, indexing, stale results, no matches in complete scope,
  and no matches in the indexed/searched portion.
- Keep assistive/reduced-update mode usable: support Enter-submitted searches
  there and avoid announcing every intermediate result set.

## Query and ranking contract

| Input | Smart behavior |
| --- | --- |
| `garden maintenance` | Prefer passages containing both words; reward proximity |
| `"garden maintenance"` | Require the phrase in that order |
| `maintenence` | Permit bounded typo expansion; prefer exact matches |
| `gard` | Prefix-match the final unquoted term while typing |
| `grdn` | Permit abbreviation matching in filenames/paths only |
| `path:notes/ winter` | Restrict retrieval to listed paths with that prefix |
| `how do I keep weeds down?` | Use keyword retrieval initially; optional semantic mode later |

Implement a small application-owned parser, not an exposed engine query
language. Support only quoted phrases and `path:` initially. Treat other
punctuation as ordinary input; show actionable feedback for malformed quotes
or an empty filter. Exact mode bypasses this parser entirely. Path filters
intersect the listed scope and apply before candidate retrieval.

Use BM25 as the lexical baseline, with boosts for title and heading fields,
phrase matches, and proximity. Prefer all-term matches; if relaxed partial
matches are needed, show them after full matches with a clear label. Quoted
phrases remain required even in relaxed retrieval. Use deterministic path and
source-position tie-breakers.

Start typo expansion conservatively: none for very short tokens, edit distance
one for ordinary tokens, and distance two only for longer tokens when exact
retrieval is insufficient. Cap term expansions and candidate counts. Tune
thresholds with relevance fixtures rather than assuming a universal formula.
Do not fuzzy-expand quoted phrases or path filters. Abbreviation matching is
a separate filename/path scorer and does not run across body prose.

## Architecture

Prefer Tantivy for ranked local retrieval, subject to an initial dependency
and performance spike. Its fuzzy term query support is a good fit for the
Rust application. SQLite FTS5 is an alternative if the spike fails; its BM25
and trigram support do not by themselves provide equivalent typo correction.

Evolve the current worker facade into a search service. Split implementation
into focused modules if needed, keeping engine details outside TUI rendering:

```text
Query input + listed scope + generations
                  |
          cancellable search service
            /                 \
     Exact bounded scan    Smart retrieval
                                 |
                       rank + group + snippets
            \                 /
         typed results + coverage + source identity
                  |
          TUI selection / preview / open
```

### Passage records

Parse Markdown using the existing `pulldown-cmark` dependency and source
offsets. Split at headings and paragraph boundaries, with a bounded fallback
for long blocks. Preserve searchable code, links, and comments; rendered-text
extraction must not silently remove source that users could previously find.
Index raw source alongside structural fields where needed.

Each record contains document identity/path, title, heading hierarchy,
passage identity, source byte range, source line range, searchable text,
source fingerprint, and index schema/tokenizer version. Keep a mapping from
normalized text spans back to source bytes; Unicode case conversion and
normalization can change string length. Do not use normalized byte offsets
directly to highlight original text.

Results carry match kind, matched spans, navigation anchor, source fingerprint,
and query/scope/index generations. Coverage reports searched/indexed files,
skipped files, truncated files, pending updates, and exhausted limits.

### Index lifecycle and freshness

- Begin with an in-memory index built lazily off the input loop. Publish
  coherent snapshots; never block TUI input waiting for a commit or rebuild.
- Use current source-read limits for the first indexed release and report
  incomplete coverage. Any higher limits require benchmark evidence and a
  documented budget change, including compatibility with the source viewer.
- Reconcile using the existing document-list/live-reload flow. Reindex added
  or changed files, remove deleted/out-of-scope files, and cancel work on scope
  changes. Do not introduce an independent watcher until necessary.
- Use metadata for cheap change detection and content fingerprints to validate
  indexed data. Explicit refresh must reconcile content, including changes
  missed by metadata heuristics.
- Reject replies for obsolete query or scope generations. Track index snapshot
  identity so late results cannot overwrite a newer published snapshot.
- Before preview/open, validate the source fingerprint and scope membership.
  If changed, refresh that document and rerun/reanchor; never trust an old line
  number or require the original query to occur literally.
- Cache corruption, an unavailable index, or dependency initialization failure
  must yield a visible explanation and a usable Exact-mode fallback.
- Retain bounded queues, cancellation, read safeguards, and worker shutdown.
  Engine calls that cannot be interrupted must be bounded; discard their stale
  results and verify they do not create unbounded shutdown latency.

Persistent caching is a later optimization, gated on cold-start measurements.
If justified, use the platform cache directory, keyed by canonical worktree
identity and schema version, outside versioned project content. Reconcile
cached paths against the current scope before exposing any results. Make
cache replacement atomic, handle concurrent processes, support invalidation,
and fall back to memory if the cache is unwritable. Never treat cached source
as authoritative or retain removed documents indefinitely.

## Implementation milestones

### 1. Establish contracts and validate the engine

1. Add representative query/expected-passage fixtures: exact phrases, typos,
   partial words, path abbreviations, Markdown syntax, Unicode, and irrelevant
   near-matches. Include the existing literal-search cases.
2. Spike Tantivy indexing and retrieval on representative benchmark corpora;
   verify compatible crate version, Rust minimum, license, binary-size impact,
   memory, cold indexing time, and cancellation behavior.
3. Record the dependency decision and proposed resource budgets. Select and
   pin a compatible release only after this check.
4. Define typed queries, hits, coverage, and generation identities. Document
   final keyboard semantics and the SPEC changes required by later phases.

Exit: engine choice and measurable budgets are recorded; the current search
continues to work unchanged.

### 2. Ship live literal search and previews

1. Add debounce scheduling through `run.rs`/`shell.rs`, using the existing
   background worker and cancellation mechanism.
2. Implement Enter flush/open behavior, selection preservation, bounded
   previews, highlights, and explicit pending/coverage status.
3. Update contextual help, README, SPEC, and TUI access tests with the new
   interaction. Keep Exact as the only/default mode at this milestone.

Exit: fast typing and navigation never open stale hits; small terminals,
bracketed paste, Esc, Ctrl-C, and reduced-update behavior remain usable.

### 3. Ship indexed Smart search

1. Build passage extraction with source mappings and an in-memory index.
2. Add the limited query parser, path filtering, BM25 field boosts, phrase and
   prefix queries, deterministic ranking, and passage grouping.
3. Integrate scope reconciliation and incremental updates with existing reload.
4. Replace literal-only hit validation with source/version-aware navigation.
5. Expose Smart/Exact switching and make Smart default only after acceptance.

Exit: expected passages rank above unrelated text, literal mode retains its
contract, source locations survive wrapping, and changed/out-of-scope content
cannot be opened through stale results.

### 4. Add controlled fuzzy matching

1. Add bounded typo expansion and separate filename abbreviation retrieval.
2. Merge candidates with exact-match preference and deduplicate passage hits.
3. Label typo/partial matches and tune thresholds against relevance fixtures.
4. Add adversarial short-term/common-term queries to verify expansion budgets.

Exit: typo queries improve top-five retrieval without degrading exact queries
or overwhelming results with weak matches. Warm retrieval meets its budget.

### 5. Harden and measure

1. Measure p50/p95 warm retrieval, query-to-paint time, initial indexing,
   incremental updates, memory, index size, startup, and binary size.
2. Target p95 warm lexical retrieval below 100 ms on a recorded machine and
   corpus, excluding the separately measured debounce. Record cold behavior
   separately; do not describe initial indexing as instant.
3. Add persistent caching only if measured startup/indexing costs justify it.
4. Run relevant PR-tier checks from `docs/verification-tiers.md`, terminal smoke
   checks, and the declared Rust-minimum build after dependency changes.

Exit: benchmark evidence, coverage limitations, fallback behavior, and user
documentation agree. No unexplained regression in existing SPEC budgets.

### 6. Evaluate optional semantic retrieval

This phase depends on a stable lexical release and a judged query set with
conceptual searches that lexical retrieval misses.

1. Evaluate local embedding models on relevance, supported languages, license,
   download size, runtime support, memory, and latency. Record the model choice;
   do not assume a model or download one during ordinary search startup.
2. Add explicit semantic opt-in and model-download controls. Keep ordinary
   search offline; remote inference requires a separate explicit user choice.
3. Embed bounded passages and cache by content hash plus model/tokenizer
   version. Invalidate embeddings when source or model changes.
4. Start with exact vector similarity over the bounded corpus; add approximate
   nearest-neighbor indexing only when measurements justify its complexity.
5. Apply scope/path constraints to both retrieval paths, merge lexical/vector
   candidates using reciprocal rank fusion, deduplicate, and mark semantic-only
   matches as related passages rather than fabricated word matches.
6. Evaluate an optional reranker over a bounded candidate set only if it adds
   measurable relevance within the latency budget. Preserve query generations
   across progressive results; avoid moving the user's active selection.
7. Keep keyword search available during model failure, download, or indexing;
   show semantic coverage separately from lexical coverage.

Exit: the judged conceptual-query set improves over lexical-only search, exact
queries do not regress materially, and published resource costs justify the
feature. If not, defer semantic search and retain the lexical release.

## Verification and acceptance

Use meaningful behavioral tests and measured relevance, not assertions that
merely reproduce the scoring implementation.

| Area | Required evidence |
| --- | --- |
| Relevance | Expected passage in top five; reciprocal rank/nDCG on graded fixtures; exact-query regression comparison |
| Query semantics | Quotes, punctuation, filters, prefix matching, typo limits, partial-match labels, Exact compatibility |
| Source fidelity | Unicode normalization, combining marks, CRLF, code/comments, long lines, wrapping-independent anchors |
| Freshness | Edit/delete/rename, scope removal, explicit refresh, stale preview/open, reordered async replies |
| Resource bounds | Large files, many documents, broad queries, expansion limits, cancellation, shutdown |
| Input and access | Rapid typing, Enter during debounce, paste, Esc/reopen, mode switch, narrow layout, no-color/reduced-update |
| Recovery | Index failure/corruption, rebuild, unwritable cache, concurrent cache users if persistence ships |
| Semantics | Model unavailable, partial embeddings, changed model/source, semantic-only location validation |

Extend `search.rs` unit tests for retrieval contracts, `tests/tui_access.rs`
and appropriate shell/lifecycle suites for integration, and the existing
benchmark harness for performance. Use deterministic fake time/workers for
debounce and reply-order tests rather than timing-sensitive sleeps.

For each milestone, update README, SPEC 11.3/11.4 where behavior changes,
contextual help, and applicable benchmark/verification documentation. Follow
the repository's backlog workflow if converting this plan into tasks; this
document itself does not modify `to-do.json`.

## References

- [Tantivy fuzzy-search example](https://github.com/quickwit-oss/tantivy/blob/main/examples/fuzzy_search.rs)
- [SQLite FTS5: ranking, snippets, and trigram indexing](https://www.sqlite.org/fts5.html)
- [Hybrid retrieval and ranking](https://www.elastic.co/docs/solutions/search/ranking)
- [Reciprocal rank fusion](https://www.elastic.co/docs/reference/elasticsearch/rest-apis/reciprocal-rank-fusion)

These explain candidate techniques, not a commitment to Elasticsearch or a
hosted service. Margin's proposed first release is local lexical search.
