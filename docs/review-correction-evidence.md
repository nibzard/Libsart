# Correcting part of a passage during review

Date: 2026-10-15. Original source revision:
`bb0ccc45b80f0e5d6a40d87074ef4d75ec84f972`. The correction below is implemented
in the local changes following that investigation.

Checkpointing a source correction now preserves decisions for unchanged patches.
The initial investigation showed that correcting one unwanted change required
repeating unrelated decisions. The updated workflow checks verify that these
decisions survive and that changed groups return to undecided.

## Task and method

The original paragraph says:

> The session starts at 10:00. The introduction is rather long and wordy.

The candidate says:

> The session starts at 11:00. The introduction is concise.

The intended result keeps the concise wording and restores `10:00`.
Both changes belong to one review group. Other passages contain independent
changes that the writer chooses to restore.

[The workflow checks](../tests/review_correction.rs) drive the real terminal
shell and shared operations against temporary Git repositories. They use the
built-in editor to correct the source, then checkpoint and finish review.
Assertions check saved bytes, commit changes, blocked actions, decision counts,
and the exact final document.

The editor receives the complete corrected source through select-all and paste.
This isolates review transitions from text entry. It does not measure typing,
discovery, human review time, physical terminal behavior, or perceived effort.

## Original observations

| Workflow | Prior restore decisions | Decisions entered again | Exact final source |
| --- | ---: | ---: | --- |
| Correct before other decisions | 0 | 0 | Yes |
| Correct during review | 1 | 1 | Yes |
| Correct during review | 5 | 5 | Yes |
| Finish first, then correct | 5 | 0 | Yes |

Editing opens directly from Changes with `e`. `Esc` saves and returns to Read.
Saving preserves the current commit and does not approve the correction.

With no prior decisions, `C` checkpoints the correction and opens Changes.
Two Enter presses preview and confirm the review.

Before the correction, `C` refused with prior decisions present:

> Finish or clear your review decisions before checkpointing edits

The tested route used `R` to refresh and clear those decisions, then `C` to
checkpoint. The writer had to enter the unrelated restore decisions again.
In the five-decision fixture, this added five navigation keys and five restore
keys after rebuilding the review. Refresh and the refused checkpoint added
further actions. These counts describe this route, not a measured minimum.

Finishing first avoids that repetition. However, the first review approves
`11:00`, the known unwanted time. A second review approves the correction.
The test verifies the intermediate text as well as the final text.

## Implemented correction

The `C` action now checkpoints with prior decisions present. It rebuilds the
comparison, matches complete patches, and records matching decisions against
the new change identifiers. The notice states retained and reset counts.

| Correction workflow | Prior restore decisions | Decisions entered again |
| --- | ---: | ---: |
| Correct before other decisions | 0 | 0 |
| Correct during review | 1 | 0 |
| Correct during review | 5 | 0 |

All three workflows produce the exact intended source. There is no need to
finish a review containing the unwanted time before correcting it.

Matching requires the same baseline, branch, scope, comparison engine, and
Git version. Keys contain complete patch bytes, source coordinates, modes,
and grouping reasons. Only independent text groups exclude the file-wide
`index` header, because another passage can change the candidate blob.
Whole-file and opaque groups retain their complete original patches.

Only unique matches retain decisions. Changed, removed, shifted, or
unverifiable groups reset. The old plan never applies to the new candidate.
Explicit refresh with `R` continues to clear decisions.

Additional checks cover mixed keep and restore decisions, changed groups,
removed groups, repeated text, shifted coordinates, changes beyond displayed
excerpts, external corrections, unchanged checkpoints, blocked checkpoints,
new starting points, and changed scope. A failed checkpoint preserves the
old plan and the unrelated index state.

The unchanged-checkpoint case also rewrites identical file bytes before review.
This exposed stale Git stat metadata that could block `apply --index`.
Review application now refreshes that metadata without staging content.
The checks retain an unrelated working edit and verify it stays out of commits.

Retention rebuilds complete manifests before and after checkpointing when
decisions exist. This adds work to that action. No latency improvement or
human review-time improvement is claimed by these functional checks.

## Reproduce

```bash
env -u NO_COLOR cargo test --locked --test review_correction -- --nocapture
```

The initial four checks passed before the correction. The updated suite contains
13 checks. They use synthetic documents and do not consume the frozen benchmark
holdout set.

## Validation

The full pull-request test gate passes 982 tests across 57 targets, with zero
failures or ignored tests. The run takes 368 seconds locally. The lint gate
and Rust 1.89.0 compatibility gate also pass.

```bash
env -u NO_COLOR ci/run-tier.sh tests pr
env -u NO_COLOR ci/run-tier.sh lints pr
env -u NO_COLOR ci/run-tier.sh msrv pr
```

Local gate records are under `ci/artifacts/tests-pr`, `ci/artifacts/lints-pr`,
and `ci/artifacts/msrv-pr`.
