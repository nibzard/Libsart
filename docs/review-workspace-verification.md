# Review workspace verification

Local verification: 2026-10-15, macOS 26.6.1 arm64, Git 2.56.0,
Rust 1.98.1, declared minimum Rust 1.89.0, tmux 3.7c.

## Behavioral evidence

- `tests/tui_workspace.rs` drives real Git repositories through restart,
  explicit startup options, competing owners, head/adoption/scope-cycle
  mismatches, unknown IDs, dirty/staged blockers, save failures, old save
  completions, receipt recovery, and successful review with cleanup failure.
  Resumed and uninterrupted choices produce identical prepared plans.
  The recorded start marker stays distinct from a later review baseline;
  starting fresh preserves older drafts, and stored reference paths remain
  guarded and display-escaped even below the minimum terminal size. Stock
  Git oracles check that local saving, palette navigation, reference polling,
  and receipt inspection preserve working bytes, index, refs, and approval.
- `src/tui/session.rs` tests partial-write ENOSPC and permission failures,
  atomic replacement failure, corrupt/oversize/new-version records,
  conflicting decisions, bounded enumeration, crash-left temporaries,
  symlinks, advisory ownership, coalescing, and draining before deletion.
  Fault injection occurs at the actual temporary-file write boundary; valid
  prior JSON survives each failed replacement.
- Changes tests cover incomplete detail and unknown visited-ID refusal,
  unavailable eligibility, and source-coordinate retention across wrapping
  and source-mode changes. Session records contain labels and descriptors,
  never document/reference bodies or note text.
- Palette tests cover deterministic ranking, duplicate titles/headings,
  Unicode expansion with original byte spans, literal paste, modal Tab
  containment, disabled reasons, reduced-update submission, stale-source
  reselection, scope exclusions, and metadata/result bounds.
- Reference tests cover working freeze/explicit refresh, changed-source
  resume, immutable historical OIDs, missing versus empty, failed replacement,
  independent source/search/anchors, choosing a heading without moving the
  primary review, and background detection of a changed companion source.
- `tests/tui_lifecycle.rs` has twelve real-PTY cases, including local
  quit/save-failure flow, literal palette input/paste, reference-focused
  decision keys, resize, ordinary finish parity, and terminal restoration.
  The reference flow uses reduced updates and waits for its matched action
  on the current screen rather than assuming a fixed matching delay.
  Existing monochrome, reduced-update, accessibility, history, refresh, and
  shell buffer suites remain part of the test gate.

## Performance smoke

`cargo test --locked --test tui_workspace large_scope_palette_and_save_churn_smoke -- --nocapture`
produced the following debug-build observations on this shared development
machine while other verification work was running:

| Work | Observed |
| --- | --- |
| Metadata scan of a 2,000-document input, stopped at 20,000 entries | 91 ms; 1,429 files read; limit disclosed |
| Five queries over that metadata snapshot | 148,071 / 157,142 / 274,309 / 190,364 / 168,452 µs |
| Submit 100 snapshots of 10,000 decisions and durably flush generation 100 | 147 ms; no temporary files left |

Metadata parsing/matching and session serialization/fsync run on separate
bounded workers. Queries debounce about 100 ms, decision edits 250 ms with a
one-second maximum pending interval, and navigation-only saves at most once
per second. Idle input waiting retains the existing two-second inspection
cadence; only pending work shortens it. These observations do not establish
P-KEY/P-RSS release compliance. No release latency or resource budget is amended. Metadata labels and ancestor
context each have an additional 512-scalar bound to prevent a long heading
from being copied into thousands of entries. A file with such a truncated
field contributes to the visible truncated count; matches beyond those bounds
are not claimed. This is the deliberate metadata-memory bound added to the
proposal.
A dedicated runner campaign, including the prescribed 10,000-event latency
statistics and resource accounting, remains release evidence.

## Verification portability

The required checks exposed existing verification assumptions on macOS. This
branch repairs them so that the new behavior can be exercised meaningfully:

- Correct Darwin PTY flags and variadic `fcntl` ABI; match the Darwin termios
  layout and query the master after session exit invalidates the slave.
- Reconstruct basic cursor/erase output before matching incremental Ratatui
  repaints, including the independent benchmark PTY reader.
- Probe macOS RAM/mount information natively; run Linux `/proc` CPU/RSS
  sampler tests only on Linux. Their parsers and synthetic gate tests still
  run everywhere. Use the platform's actual `false` executable.
- Seed invalid-UTF-8 Git paths through the index, since APFS cannot create
  those filenames. Require configured identities in the missing-identity
  fixture so OS account/email inference cannot satisfy it accidentally.
  Raw terminal export checks use the shared PTY rather than GNU-only `script`
  flags; color-profile checks isolate the inherited `NO_COLOR` setting.
- Correct the TLA+ 1.8.0 artifact checksum to
  `0f56d6a813368c48008dfbecb23a73ab43a4a5e1bf09f026782a674aa9d4b3c9`.
  It matches the downloaded artifact and the digest published by the
  [official GitHub release API](https://api.github.com/repos/tlaplus/tlaplus/releases/tags/v1.8.0).
  Formal runs continue to verify the pin before executing the jar.

## Coverage limits

Filesystem failures are injected deterministically; this does not emulate a
physical power cut or establish durability guarantees beyond the filesystem's
fsync/rename semantics. Reference fingerprints cover the bounded captured
text; truncation is disclosed, and unread tails are not a second full-text
index. Uncatchable termination cannot guarantee a pending flush. The Git
mutation protocols and formal model are unchanged; local intent ownership is
covered by storage and real-viewer tests. No new worktree support or pointer
click/hover contract is advertised. The existing minimum-size, source wrapping,
monochrome, and terminal support limits remain applicable.

## Required checks

| Check | Result |
| --- | --- |
| `ci/run-tier.sh lints pr` | Pass: formatting and clippy for all targets, warnings denied |
| `ci/run-tier.sh msrv pr` | Pass: Rust 1.89.0, all targets, locked dependencies |
| `ci/run-tier.sh formal pr` | Pass: SANY and all four expected profile outcomes (1,332 s) |
| `ci/run-tier.sh tests pr` | Pass: all targets with 256 generated property cases, 16 larger cases, and required tmux (871 s in the final full run) |
| Post-review regression suites | Pass: 177 library cases, 17 workspace cases, 12 PTY lifecycle cases, plus shell/access/refresh/finish/history/changes suites |

The final full-tier run passed 921 tests across 53 test targets, with zero
failures or ignored tests. The final layout geometry and deterministic PTY
matching helper also passed the post-review regression suites above.
Local gate logs, versions, and timings remain under `ci/artifacts/complete/`, `ci/artifacts/pr-ready/`, and
`ci/artifacts/formal-pr/`. Linux CPU/RSS sampler execution is delegated to
Linux CI; it is not claimed by the macOS run.
