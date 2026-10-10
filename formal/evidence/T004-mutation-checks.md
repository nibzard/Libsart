# Formal mutation check evidence (T004)

This record satisfies the mutation requirement of SPEC 15.6 and the checks
that Appendix A.9 lists. `formal/run-mutations.sh` produced it on
2026-10-10 (UTC). The script applies each documented model mutation to a
copy of `formal/MarginReview.tla`, parses the mutant with SANY, and model
checks it with TLC. A case counts as evidence only when TLC reports the
designated detector for that defect and the preserved trace ends with the
action that commits the defect.

## Outcome

All ten defect cases failed on their designated property. The hook control
and the cooperative control passed. No case timed out, failed to parse, or
failed on an unrelated property. The whole run took about 90 seconds of TLC
time.

| Case | Deliberate defect (A.9) | Designated detector | Reported by TLC | Trace ends with |
| --- | --- | --- | --- | --- |
| `01-draft-advances-baseline` | Advance baseline on draft capture | `BaselineIsReviewed` | yes | `Checkpoint(FALSE)` |
| `02-admit-incomplete-plan` | Admit an incomplete new plan | `ApprovalExact` | yes | `Publish` |
| `03-drop-stale-bc-validation` | Remove effective stale B/C validation | `PinnedWhileApplying` | yes | `Commands` (`Submit`) |
| `04-bypass-receipt-uniqueness` | Bypass receipt lookup and publication uniqueness guards | `RequestAtMostOnce` | yes | `Publish` |
| `05-existing-id-different-payload-success` | Treat an existing ID with different payload as success | `SuccessHasReceipt` | yes | `Commands` (`Submit`) |
| `06-premature-success` | Return success before a valid receipt exists | `SuccessHasReceipt` | yes | `Commands` (`Submit`) |
| `07-publish-during-recovery` | Publish during crash/recovery inspection | `RecoveryDoesNotPublish` | yes | `InspectRecovery` |
| `08-outside-writes-in-apply-interval` | Enable outside writes inside the apply interval | `NoUnseenOverwrite` | yes | `ApplyGroup(paragraph_a)` |
| `09-hook-corruption-control` | Hook-corruption extension, control | none: must pass | passed | n/a |
| `10-remove-intended-tree-check` | Hook-corruption extension, defect | `ApprovalExact` | yes | `Publish` |

TLC reported cases 01 to 06, 08, and 10 as `Invariant <name> is violated`
and case 07 as `Action property RecoveryDoesNotPublish is violated`, because
`RecoveryDoesNotPublish` is an action property. The runner accepts exactly
those two wordings for the designated property and nothing else.

## Exact model change for each case

Each mutation is a versioned patch in `formal/mutations/`. The table below
states the change in words; the patch file is the authority.

| Case | Change to `MarginReview.tla` |
| --- | --- |
| 01 | `Checkpoint` sets `baseline' = HeadId + 1` instead of leaving `baseline` unchanged, so a draft capture counts as approval |
| 02 | `Submit` drops the `IF ~done THEN "incomplete"` branch, so an incomplete plan becomes pending |
| 03 | `Submit` drops `b # baseline \/ c # HeadId` from the `"stale"` branch, so a plan against an old baseline or an old candidate is accepted |
| 04 | `Submit` drops the `IF oid # 0` receipt branch, and `Publish` drops the `Receipt(pending.req) = 0` guard, so one request ID can publish twice |
| 05 | `Submit` answers `"success"` instead of `"request-conflict"` when the found receipt holds a different payload |
| 06 | `Submit` answers `Reply("success", ..., 0)` in the error branch instead of `Reply(err, ..., 0)`, so success is returned with receipt ID 0 |
| 07 | `Fail` keeps `pending`, and `InspectRecovery` publishes the interrupted review and advances `baseline` instead of only returning to `"idle"` |
| 08 | `ExternalAllowed == TRUE`, so external writes are permitted while a mutation is active |
| 09 | Adds `HookCorruptsIndex(t)` and lists it in `Next`; `Publish` commits `index` instead of `Target`; the intended-tree check `index = Target /\ ScopedEqual(work, Target)` stays |
| 10 | Case 09 plus removal of `index = Target /\ ScopedEqual(work, Target)` from `Publish` |

## Tools

The run used the same pinned artifact as the T003 profile gate:
`tla2tools-1.8.0.jar`, sha256
`7beec0f04818732a62fa193731711a99aa4f11279499b2360a7d156c519ea78d`,
verified from `formal/tools/tla2tools-1.8.0.jar.sha256` before the run.
Java was OpenJDK 21.0.12.1. `evidence/mutations/environment.txt` records
the jar hash, Java version, machine, and the sha256 of the base model, all
five configurations, and all ten patches. The base model hash
(`c1a36527...29347`) equals the T003 record, so both gates checked the same
model.

## Host profile

Every case runs `MarginReviewCooperative.cfg`: `Paths = {paragraph_a,
outside}`, `Scope = {paragraph_a}`, `Values = {old, new, unseen}`,
`RequestIds = {request_1}`, `MaxCommits = 3`, `Cooperative = TRUE`. SPEC
15.6 requires the three-value domain, because two values cannot distinguish
an intervening third value from the candidate and the target. One profile
for all cases also keeps the state counts comparable.

## Results

| Case | Exit | Result | States generated | Distinct states | Depth | Wall |
| --- | --- | --- | --- | --- | --- | --- |
| `cooperative-control` | 0 | pass | 8,833,531 | 107,130 | 13 | 41 s |
| `uncooperative-control` | 12 | `NoUnseenOverwrite` | 94,442 | 5,828 | 6 | 1 s |
| 01 | 12 | `BaselineIsReviewed` | 167 | 83 | 3 | < 1 s |
| 02 | 12 | `ApprovalExact` | 530 | 195 | 3 | 1 s |
| 03 | 12 | `PinnedWhileApplying` | 2,219 | 470 | 4 | 1 s |
| 04 | 12 | `RequestAtMostOnce` | 116,776 | 7,962 | 6 | 1 s |
| 05 | 12 | `SuccessHasReceipt` | 28,394 | 2,926 | 5 | 1 s |
| 06 | 12 | `SuccessHasReceipt` | 22 | 19 | 2 | < 1 s |
| 07 | 13 | `RecoveryDoesNotPublish` | 5,531 | 916 | 4 | 1 s |
| 08 | 12 | `NoUnseenOverwrite` | 62,922 | 5,828 | 6 | 1 s |
| 09 | 0 | pass | 8,835,115 | 107,274 | 13 | 40 s |
| 10 | 12 | `ApprovalExact` | 5,580 | 965 | 4 | 1 s |

## Matched cooperative and uncooperative writer cases

The two control rows re-run the matched pair from SPEC 15.6 beside the
mutations. The base model passes with `Cooperative = TRUE` and fails on
`NoUnseenOverwrite` with `Cooperative = FALSE`. Case 08 mutates the
cooperative model into one that permits outside writes during the apply
interval. Its counterexample is byte-identical to the uncooperative control
counterexample (both sha256
`740433afbb57beafbed2099cd868fb376b4a17239e6c711fd7c4ce269e55a03c`),
because `Cooperative` appears nowhere else in the model. The two traces
follow the adversarial sequence that SPEC 15.6 requires:

1. Initial state: `work[paragraph_a] = old`, one `initial` commit.
2. `ExternalWrite`: the writer drafts `new` content.
3. `Checkpoint(FALSE)`: commit 2 captures the draft.
4. `Commands` (`Submit`): plan `request_1`, base 1, candidate 2, restore
   `{paragraph_a}`; validation passes and the phase becomes `apply`.
5. `ExternalWrite`: after validation, the scoped content becomes `unseen`.
   The base cooperative model forbids this step; the mutant and the
   uncooperative control allow it.
6. `ApplyGroup(paragraph_a)`: apply writes the old target over `unseen`,
   sets `unseenOverwrite = TRUE`, and `NoUnseenOverwrite` fails.

## Counterexample narratives

Each machine-readable trace is `<case>.counterexample.json`. The states
below are the action sequence; the JSON carries the full variable
assignments.

- **01** (3 states): `ExternalWrite` drafts `new`, then the mutated
  `Checkpoint(FALSE)` appends the draft at index 2 and sets `baseline = 2`
  while no review commit exists. `BaselineIsReviewed` fails because the
  latest review-or-anchor index is 1.
- **02** (3 states): `Commands` accepts `request_1` with `complete = FALSE`,
  `Publish` commits it as a `review` with `complete = FALSE`.
  `ApprovalExact` fails on the `complete` conjunct.
- **03** (4 states): `ExternalWrite` then `Checkpoint(FALSE)` make commit 2
  the head. `Commands` submits `request_1` with base 1 and candidate 1. The
  candidate is stale, but the mutated `Submit` no longer rejects it.
  `PinnedWhileApplying` fails because `pending.cand = 1` while the head is
  commit 2.
- **04** (6 states): a keep-all review publishes commit 2 for `request_1`,
  `Respond` returns success, then the same request ID submits and publishes
  commit 3. `RequestAtMostOnce` fails because commits 2 and 3 share one
  request ID.
- **05** (5 states): a keep-all review publishes commit 2 with an empty
  restore set, `Respond` returns success, then the same request ID submits
  an incomplete plan. The mutated `Submit` answers `success` with the
  receipt of commit 2. `SuccessHasReceipt` fails because that receipt holds
  a complete plan, not the submitted payload.
- **06** (2 states): `Commands` submits an incomplete plan; the mutated
  `Submit` answers `success` with receipt ID 0 before any receipt exists.
  `SuccessHasReceipt` fails on `reply.oid = 0`.
- **07** (4 states): a keep-all review reaches the `commit` phase, `Fail`
  moves to `recovery` and keeps the plan, then the mutated
  `InspectRecovery` publishes commit 2 and advances the baseline while the
  phase is still `recovery`. `RecoveryDoesNotPublish` fails on that step.
- **10** (4 states): a keep-all review reaches the `commit` phase,
  `HookCorruptsIndex` rewrites the staged index to `{paragraph_a: old,
  outside: new}`, then `Publish` commits the staged index without the
  intended-tree check. `ApprovalExact` fails because the committed tree
  differs from `Result(1, 1, {})`. That publication would also fail
  `NoScopeLeak`, because the corruption reached the out-of-scope path; TLC
  reports `ApprovalExact` first, since the configuration checks it first.
  A scoped-only corruption is reachable at the same depth and fails
  `ApprovalExact` alone.

## Hook-corruption extension

Appendix A.9 requires the corruption action to exist before the check
removal can demonstrate anything. Case 09 adds `HookCorruptsIndex`, makes
`Publish` commit the staged index, and keeps the intended-tree check. It
passed the full cooperative profile: 8,835,115 states generated, 107,274
distinct, depth 13, all invariants and `RecoveryDoesNotPublish` intact. The
extension grew the reachable state space by 144 distinct states, so the
action is exercised, and no publication of corrupted content occurred. Case
10 then removes only the check and `ApprovalExact` fails within four steps.

## Drift protection

`tests/formal_mutations.rs` keeps the patches honest without running TLC:

- Every patch listed in `formal/mutations/cases.tsv` must apply, in order,
  to the current `formal/MarginReview.tla` and must change it. Editing the
  model without updating the patches fails the test.
- Every case must name an existing configuration, a designated detector
  that the configuration actually checks, and a counterexample action
  unless it is a passing case.

A manual check confirmed the drift test fails when the model changes and
the patches do not.

## Reproduction

Run from the repository root:

```sh
./formal/run-mutations.sh
```

The script verifies the pinned jar, rebuilds every mutant in a temporary
directory, and rewrites `formal/evidence/mutations/`. It exits nonzero when
any case misses its designated detector, when a patch stops applying, when
SANY rejects a mutant, or when a preserved trace does not end with the
expected action. `./formal/run-profiles.sh` still reruns the base-model
gate from T003.

## Evidence files

| File | Content |
| --- | --- |
| `formal/evidence/mutations/summary.tsv` | Per-case expectation, exit code, result, validated trace action, state counts, depth, wall time, mutant sha256 |
| `formal/evidence/mutations/environment.txt` | Run date, jar URL and hash, Java and machine data, hashes of the base model, configurations, and patches |
| `formal/evidence/mutations/tlc-<case>.log` | Full TLC output per case, including the human-readable counterexample |
| `formal/evidence/mutations/sany-<case>.log` | SANY parse of each mutant |
| `formal/evidence/mutations/<case>.counterexample.json` | Machine-readable counterexample per failing case |
| `formal/mutations/cases.tsv` | The case manifest the runner reads |
| `formal/mutations/*.patch` | The exact model change for each case |
| `formal/mutations/README.md` | Per-case description of each defect and its designated detector |
