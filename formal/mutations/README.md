# Formal mutation checks

These files implement the deliberate defects that SPEC 15.6 and Appendix A.9
require. Each `.patch` file is the exact model change for one defect: it is
applied to a copy of `formal/MarginReview.tla`, parsed by SANY, and model
checked by TLC. `formal/run-mutations.sh` runs the whole set and records the
results in `formal/evidence/mutations/`. `tests/formal_mutations.rs` applies
every patch to the canonical model so the two cannot drift apart silently.

A mutation case is evidence only when TLC reports the designated detector
listed in Appendix A.9 for that defect and the preserved counterexample ends
with the action that commits the defect. A parse failure, a timeout, or an
unrelated property failure is a broken test.

## Why one host profile

Every mutated run uses `MarginReviewCooperative.cfg`. SPEC 15.6 requires the
three-value domain `{old, new, unseen}` to distinguish an intervening third
value from the candidate and the target, so the same profile must host the
writer-cooperation mutation. Running all cases on one profile also keeps the
state counts comparable. The `cooperative-control` and
`uncooperative-control` rows rerun the matched positive and negative pair
from SPEC 15.6 beside the mutations, so one summary table shows the base
model passing where the mutants fail.

## Cases

The table maps each case to its Appendix A.9 row. "Change" names the action
touched; the patch is the authoritative description.

| Case | Deliberate defect (A.9) | Change to `MarginReview.tla` | Designated detector |
| --- | --- | --- | --- |
| `01-draft-advances-baseline` | Advance baseline on draft capture | `Checkpoint` sets `baseline' = HeadId + 1` instead of leaving `baseline` unchanged | `BaselineIsReviewed` |
| `02-admit-incomplete-plan` | Admit an incomplete new plan | `Submit` drops the `IF ~done THEN "incomplete"` branch from `err` | `ApprovalExact` |
| `03-drop-stale-bc-validation` | Remove effective stale B/C validation | `Submit` drops `b # baseline \/ c # HeadId` from the `"stale"` branch of `err` | `PinnedWhileApplying` |
| `04-bypass-receipt-uniqueness` | Bypass receipt lookup and publication uniqueness guards | `Submit` drops the `IF oid # 0` receipt branch; `Publish` drops `Receipt(pending.req) = 0` | `RequestAtMostOnce` |
| `05-existing-id-different-payload-success` | Treat an existing ID with different payload as success | `Submit` answers `"success"` instead of `"request-conflict"` when the receipt payload differs | `SuccessHasReceipt` |
| `06-premature-success` | Return success before a valid receipt exists | `Submit` answers `Reply("success", ..., 0)` in the error branch instead of `Reply(err, ..., 0)` | `SuccessHasReceipt` |
| `07-publish-during-recovery` | Publish during crash/recovery inspection | `Fail` keeps `pending`; `InspectRecovery` publishes the interrupted review and advances `baseline` instead of only returning to `"idle"` | `RecoveryDoesNotPublish` |
| `08-outside-writes-in-apply-interval` | Enable outside writes inside the apply interval | `ExternalAllowed == TRUE`, removing the cooperative exclusion of active phases | `NoUnseenOverwrite` |
| `09-hook-corruption-control` | Hook-corruption extension, control | Adds `HookCorruptsIndex(t)`, adds it to `Next`, and makes `Publish` commit `index` instead of `Target`; the intended-tree check `index = Target /\ ScopedEqual(work, Target)` stays | none: must pass |
| `10-remove-intended-tree-check` | Hook-corruption extension, defect | Case 09 plus removal of `index = Target /\ ScopedEqual(work, Target)` from `Publish` | `ApprovalExact` |

Case 10 is a patch stack: its patch applies on top of the case 09 mutant and
does not apply to the bare model. The runner applies the list in order.

## Scope of each defect

SPEC A.9 notes two boundaries that these mutations respect:

- The hook-corruption extension first adds the excluded action and only then
  removes the intended-tree check. Case 09 proves the extended model with the
  check intact still satisfies every property, so case 10 demonstrates that
  the check, not the absence of the action, is what carries the guarantee.
- A complete submitted plan is assumed authorized. No mutation tries to make
  an identity property fail, because the model does not represent who
  approved a plan.

A redundant guard that survives the removal of another guard is not evidence
that the checks are useless. Each case documents its own logical mutation,
and the preserved counterexample shows the mechanism by which its designated
detector fires.
