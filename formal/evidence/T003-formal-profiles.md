# Formal model profile evidence (T003)

This record satisfies the SPEC 15.8 formal evidence gate and the Appendix A.8
run commands. The gate ran on 2026-10-09/10 (UTC) through
`formal/run-profiles.sh`. That script verifies the pinned tools jar, runs
SANY, and runs the five TLC profiles. It exits nonzero when any profile
deviates from its expected outcome.

## Outcome

SANY parsed and checked `MarginReview.tla` with no errors. The four positive
profiles completed with no errors. The uncooperative profile failed on the
`NoUnseenOverwrite` invariant only. The counterexample matches the adversarial
trace that SPEC 15.6 requires. No timeout occurred.

## Tools

| Item | Value |
| --- | --- |
| Tools artifact | `tla2tools-1.8.0.jar`, 4,501,960 bytes |
| Source | Official release `v1.8.0` of `tlaplus/tlaplus`, published 2026-10-06 |
| Download URL | `https://github.com/tlaplus/tlaplus/releases/download/v1.8.0/tla2tools.jar` |
| Artifact sha256 | `7beec0f04818732a62fa193731711a99aa4f11279499b2360a7d156c519ea78d` |
| Digest authority | GitHub asset digest from the release API (`repos/tlaplus/tlaplus/releases/tags/v1.8.0`). The downloaded jar matched it exactly. `formal/tools/tla2tools-1.8.0.jar.sha256` pins it and the runner re-verifies it before every run. |
| SANY | SANY2 Version 2.2 created 08 July 2020 (bundled in the jar) |
| TLC | TLC2 Version 2026.10.06.014338 (rev 94d0c50) |
| Java | OpenJDK 21.0.12.1, build `21.0.12.1+1-1-24.04.4-Ubuntu`, 64-Bit Server VM |
| Machine | Linux 6.8.0-139-generic x86_64, Ubuntu 24.04, 4 CPUs, 12th Gen Intel Core i9-12900HK |
| TLC settings | `-workers 1 -cleanup -dumpTrace json ...`, breadth-first search, default 2992 MB heap; fingerprint and seed values are in each log |

## Source and config hashes at run time

Recorded in `evidence/logs/environment.txt`. The runner writes this file at
the start of each gate run.

| File | sha256 |
| --- | --- |
| `formal/MarginReview.tla` | `c1a36527e2a1944d2b304ff3fd921b57420361dc46787c1a7a0be52c56129347` |
| `formal/MarginReview.cfg` | `9afd2b0192cc7c0c1ae6919a565b51140a1b3191d9e7e9c9a6fb4d35f30bfe4a` |
| `formal/MarginReviewCooperative.cfg` | `a153e997000b6e79b0073a6893f6198ebf108723703f9604aba6f7601e081e00` |
| `formal/MarginReviewLive.cfg` | `060c7859853c83dc625c7ecf086a36462ba417b31ec5781f0b454c957486306c` |
| `formal/MarginReviewNightly.cfg` | `91b158cf2e4c1adc8da61eeeb92aa71fe274925f750c7c3fe86b001bfdeb0ed2` |
| `formal/MarginReviewUncooperative.cfg` | `5574e936c02eb3315c94615733a28b85f01fa9be9b5e195b504b571fcbb4efb6` |

## Model bounds and properties

All profiles check the eight invariants `TypeOK`, `ApprovalExact`,
`BaselineIsReviewed`, `PinnedWhileApplying`, `RequestAtMostOnce`,
`SuccessHasReceipt`, `NoScopeLeak`, and `NoUnseenOverwrite`. The constant
domains and temporal properties follow each configuration file.

| Profile | Specification | Paths | Scope | Values | RequestIds | MaxCommits | Cooperative | Temporal properties |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| MarginReview | `Spec` | `{paragraph_a, paragraph_b, outside}` | `{paragraph_a, paragraph_b}` | `{old, new}` | `{request_1}` | 3 | TRUE | `HistoryAppendOnly`, `DraftDoesNotApprove`, `RecoveryDoesNotPublish` |
| MarginReviewLive | `LiveSpec` | `{paragraph_a, paragraph_b, outside}` | `{paragraph_a, paragraph_b}` | `{old, new}` | `{request_1}` | 3 | TRUE | `Progress`, `RecoveryDoesNotPublish` |
| MarginReviewNightly | `Spec` | `{paragraph_a, outside}` | `{paragraph_a}` | `{old, new}` | `{request_1, request_2}` | 5 | TRUE | `HistoryAppendOnly`, `DraftDoesNotApprove`, `RecoveryDoesNotPublish` |
| MarginReviewCooperative | `Spec` | `{paragraph_a, outside}` | `{paragraph_a}` | `{old, new, unseen}` | `{request_1}` | 3 | TRUE | `RecoveryDoesNotPublish` |
| MarginReviewUncooperative | `Spec` | `{paragraph_a, outside}` | `{paragraph_a}` | `{old, new, unseen}` | `{request_1}` | 3 | FALSE | `RecoveryDoesNotPublish` |

## Results

| Profile | Expected | Outcome | Exit | States generated | Distinct states | Depth | TLC duration | Wall time |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| MarginReview | pass | pass, no errors | 0 | 61,189,747 | 406,496 | 13 | 06min 05s | 366 s |
| MarginReviewLive | pass | pass, no errors | 0 | 60,115,683 | 389,856 | 12 | 07min 04s | 424 s |
| MarginReviewNightly | pass | pass, no errors | 0 | 236,835,745 | 680,208 | 20 | 30min 22s | 1,823 s |
| MarginReviewCooperative | pass | pass, no errors | 0 | 8,833,531 | 107,130 | 13 | 40s | 41 s |
| MarginReviewUncooperative | fail | violated `NoUnseenOverwrite` | 12 | 94,442 | 5,828 | 6 | 00s | 1 s |

The liveness profile verified `Progress` over the full behavior graph
("Checking temporal properties ... Finished checking temporal properties").
No symmetry reduction was used in any profile.

## Uncooperative counterexample

TLC reported `Error: Invariant NoUnseenOverwrite is violated.` after five
steps at search depth 6. The machine-readable trace is
`evidence/logs/MarginReviewUncooperative.counterexample.json`
(sha256 `740433afbb57beafbed2099cd868fb376b4a17239e6c711fd7c4ce269e55a03c`).
The full human-readable state dump is preserved in
`evidence/logs/tlc-MarginReviewUncooperative.log`.

The trace matches the adversarial sequence required by SPEC 15.6:

| Step | Action | Effect |
| --- | --- | --- |
| 1 | Initial predicate | `work[paragraph_a] = old`, history holds one `initial` commit with old content |
| 2 | `ExternalWrite` | External writer drafts new content: `work[paragraph_a] = new` |
| 3 | `Checkpoint(FALSE)` | Draft commit captures new content; head tree now `new` |
| 4 | `Commands` (`Submit`) | Plan `request_1`, base 1, candidate 2, restore `{paragraph_a}`, complete; phase becomes `apply` |
| 5 | `ExternalWrite` | After validation, external writer replaces scoped content with `unseen` |
| 6 | `ApplyGroup(paragraph_a)` | Apply writes the old target over `unseen`; `unseenOverwrite = TRUE`, invariant fails |

The matched cooperative control run passes with the same domain, so the
failure isolates the disabled cooperation guard and not the third value.

## Model corrections found by execution

The Appendix A sources had never been executed (SPEC 15.8 status before this
task). Execution exposed two defects. Both were corrected in
`formal/MarginReview.tla` and in the SPEC Appendix A.2 block in the same
change, so the drift test `tests/formal_extraction.rs` still passes.

1. Illegal action properties. SANY rejected `HistoryAppendOnly`,
   `DraftDoesNotApprove`, and `RecoveryDoesNotPublish` with three errors:
   `[] followed by action not of form [A]_v`. Each property applied `[]`
   directly to an action formula. The fix wraps each action as `[][A]_vars`.
   A stuttering step leaves `vars` unchanged and satisfies `[A]_vars`
   trivially, so the intended meanings are unchanged.
2. Lost primed binding in `ApplyGroup`. The conjunct
   `unseenOverwrite' = unseenOverwrite \/ (...)` parsed as
   `(unseenOverwrite' = unseenOverwrite) \/ (...)` because `=` binds tighter
   than `\/`. The monitor condition then never defined `unseenOverwrite'`.
   In the two-value profiles the condition is never true, so the defect was
   latent. With three values, TLC stopped with `Successor state is not
   specified by action ApplyGroup ... variable is not defined:
   unseenOverwrite`. The fix parenthesizes the right side of the equality.
   Behavior in the two-value profiles is identical because the monitor needs
   a third value to fire.

## Reproduction

Run from the repository root:

```sh
./formal/run-profiles.sh
```

The script downloads the pinned jar when absent, verifies its sha256, and
rewrites `formal/evidence/logs/`. It prints the summary table and exits
nonzero on any deviation from the expected outcomes. The committed logs are
the raw output of the recorded run; reruns refresh them.

## Evidence files

| File | Content |
| --- | --- |
| `evidence/logs/environment.txt` | Run date, jar URL and hash, Java and machine data, source and config hashes |
| `evidence/logs/sany.log` | SANY parse and semantic check output |
| `evidence/logs/tlc-MarginReview.log` | Default safety profile output |
| `evidence/logs/tlc-MarginReviewLive.log` | Conditional liveness profile output |
| `evidence/logs/tlc-MarginReviewNightly.log` | Nightly safety profile output |
| `evidence/logs/tlc-MarginReviewCooperative.log` | Cooperative control profile output |
| `evidence/logs/tlc-MarginReviewUncooperative.log` | Negative profile output with full counterexample trace |
| `evidence/logs/MarginReviewUncooperative.counterexample.json` | Machine-readable counterexample |
| `evidence/logs/summary.tsv` | Per-profile expectation, exit code, result, state counts, depth, wall time |
| `tools/tla2tools-1.8.0.jar.sha256` | Pinned tools checksum with provenance |
