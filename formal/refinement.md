# Refinement map: model actions to Margin operations

This document is the refinement mapping of SPEC 15.7 (task T038). It maps
every action of the formal model `formal/MarginReview.tla` to the
application operation that implements it and to independent real Git tests
that prove the obligation, and it defines the test-only model-action trace
whose sequences the conformance check validates against the allowed
transitions of the model.

Refinement here means: every protocol behavior the implementation performs
over one traced workspace corresponds to a behavior the model permits, and
the identities the trace carries — commit, tree, and request — satisfy the
model's rules at every step. Shared action names alone are not refinement,
which is why the check itself is an independent transcription of the TLA+
module and why negative tests prove it rejects disallowed sequences.

## The co-change rule

Four artifacts move together, in the same change, whenever the mutation
protocol changes:

1. The model: `formal/MarginReview.tla` and its five configurations
   (SPEC 15.8 runs them).
2. This map: `formal/refinement.md`.
3. The executable tests: the real Git tests named below, and the
   trace-conformance suite `tests/refinement_trace.rs` with its checker
   `tests/support/model.rs`.
4. The error documentation: the exit classes of SPEC 7.5 and the public
   error codes the implementation returns, in the reply-code map below.

`tests/refinement_map.rs` enforces the rule mechanically: it re-derives the
action inventory from the TLA+ module and requires this map to cover exactly
it, requires every mapped test function to exist, requires every
implementation-emitted trace record to appear in the source, and requires
every public error code this map names to exist in the implementation.
Renaming a model action, deleting a mapped test, or documenting a code the
binary never returns fails that suite until all four artifacts agree.

## The action map

The model's initial predicate appears first; every action of `Next` follows.
Each row names the application operation in `src/app.rs` and the
independent real Git tests that carry the obligation (SPEC 15.7, 16.3).

| Model action | Application operation | Independent real Git test |
| --- | --- | --- |
| `Init` (initial predicate) | `Operations::initialize_scope`: one verified start marker establishes the anchor and the adopted settings | `tests/initialization.rs::first_use_captures_scoped_content_in_a_valid_start_commit`, `tests/initialization.rs::unborn_initialization_creates_the_root_start_marker`, `tests/commit_metadata.rs::start_markers_validate_on_an_unborn_branch` |
| `ExternalWrite(t)` | No Margin code: the environment's write, observed by the guards and the postcondition inspection | `tests/checkpoint.rs::a_writer_change_after_staging_is_reported_not_saved`, `tests/review_apply.rs::working_bytes_that_change_after_the_apply_refuse_before_the_commit` |
| `ExternalIndex(t)` | No Margin code: the environment's staging, observed by the same guards | `tests/review_apply.rs::a_staged_entry_that_arrives_before_the_commit_refuses_without_writing`, `tests/policy_guards.rs::staged_changes_block_on_born_and_unborn_branches` |
| `RepositoryCondition(ok)` | The repository and storage policy guards of `enforce_mutation_policy` | `tests/policy_guards.rs::an_unfinished_merge_blocks_and_survives_the_guard`, `tests/policy_guards.rs::a_detached_head_blocks`, `tests/policy_guards.rs::the_guards_never_touch_the_repository` |
| `Checkpoint(FALSE)` | `Operations::create_checkpoint`: one ordinary scoped capture from a clean index | `tests/checkpoint.rs::checkpoints_commit_exactly_the_eligible_changes`, `tests/checkpoint.rs::unchanged_content_is_the_documented_no_op`, `tests/checkpoint.rs::out_of_scope_dirt_stays_out_of_the_capture`, `tests/checkpoint.rs::a_draft_after_an_ordinary_commit_keeps_the_prior_baseline` |
| `Checkpoint(TRUE)` | `Operations::create_checkpoint` over `capture_staged_recovery`: the guarded adoption of inspected staged recovery state | `tests/checkpoint.rs::the_guarded_recovery_capture_saves_inspected_staged_content`, `tests/checkpoint.rs::the_recovery_capture_refuses_moved_expected_fingerprints`, `tests/checkpoint.rs::the_recovery_capture_refuses_unrelated_staged_content`, `tests/checkpoint.rs::the_recovery_capture_refuses_staged_bytes_that_differ_from_working_content` |
| `Submit(r, b, c, rs, done, mode)` | Plan validation and receipt lookup of `apply_review` and `resume_review` before every present-day stale check; the receipt branch answers, the conflict branch refuses, and acceptance pins the comparison | `tests/review_apply.rs::stale_plans_refuse_without_writing`, `tests/review_apply.rs::uncaptured_scoped_bytes_refuse_the_application`, `tests/review_apply.rs::staged_entries_and_a_foreign_branch_refuse_the_application`, `tests/receipts.rs::present_day_blockers_never_hide_the_receipt`, `tests/receipts.rs::a_reused_request_id_with_another_plan_conflicts` |
| `Submit` in resume mode | `Operations::resume_review`: the same submission discipline over one retained result | `tests/review_resume.rs::a_candidate_that_moved_refuses_the_resume`, `tests/review_resume.rs::working_bytes_that_differ_from_the_retained_result_refuse`, `tests/review_resume.rs::staged_content_outside_the_restored_passages_refuses` |
| `ApplyGroup(p)` | One complete reverse unit of `apply_selected_review`, applied through the guarded Git index path in the proof's order | `tests/review_apply.rs::a_restoring_plan_commits_the_exact_preview_result`, `tests/review_apply.rs::an_apply_failure_retains_and_reports_the_partial_application`, `tests/failure_phases.rs::a_permission_failure_during_the_apply_is_typed_and_recoverable` |
| `UpdateIndex` | The post-apply verification of `apply_selected_review`: the real index equals the intended tree, the staged delta equals the restored paths, and the scoped working bytes equal the result; index publication is part of `git apply --index`, never a restage | `tests/review_apply.rs::a_restoring_plan_commits_the_exact_preview_result`, `tests/review_apply.rs::an_apply_failure_retains_and_reports_the_partial_application` |
| `Publish` | `commit_marker` under `apply_review` and `resume_review`: the actual parent, tree, and every reserved trailer verified against the prepared map before the baseline advances | `tests/review_apply.rs::keep_all_creates_one_verified_empty_review_commit`, `tests/review_apply.rs::a_rejected_commit_after_the_apply_reports_the_retained_result`, `tests/review_apply.rs::a_hook_that_rewrites_trailers_reports_a_contract_violation`, `tests/commit_metadata.rs::candidate_and_parent_must_agree`, `tests/commit_metadata.rs::hook_that_rewrites_reserved_trailers_fails_verification` |
| `Respond` | The success return of `apply_review` and `resume_review`: one response that names its verified receipt, with the observed post-commit state | `tests/review_apply.rs::an_exact_retry_returns_the_receipt_without_a_second_commit`, `tests/review_resume.rs::the_resume_of_a_completed_request_reports_the_receipt`, `tests/failure_phases.rs::a_process_crash_after_the_commit_loses_the_response_not_the_receipt` |
| `Fail` | The failure paths of `apply_review` and `resume_review` inside the protocol: the retained state is inspected and reported, never called a rollback | `tests/review_apply.rs::an_apply_failure_retains_and_reports_the_partial_application`, `tests/review_apply.rs::a_rejected_commit_after_the_apply_reports_the_retained_result`, `tests/failure_phases.rs::a_timed_out_review_commit_reports_an_unknown_outcome_and_recovers` |
| `Crash` | No Margin code: the process dies; the durable commit and the retained state stay behind | `tests/failure_phases.rs::an_interrupted_application_leaves_the_retained_state_recoverable`, `tests/failure_phases.rs::a_process_crash_after_the_commit_loses_the_response_not_the_receipt` |
| `Restart` | The next Margin process over the same repository | `tests/failure_phases.rs::an_interrupted_application_leaves_the_retained_state_recoverable` |
| `InspectRecovery` | `Operations::inspect_recovery`: the read-only doctor report with its safe recovery argv | `tests/recovery_doctor.rs::doctor_reports_recovery_state_without_writing`, `tests/recovery_doctor.rs::doctor_reports_staged_paths_and_safe_recovery_argv` |
| Resume submission (SPEC 15.7 last row) | `Operations::resume_review`: the original intended tree recomputed, committed only over the exact retained state | `tests/review_resume.rs::the_exact_retained_result_resumes_into_the_verified_commit`, `tests/review_resume.rs::the_resume_of_a_completed_request_reports_the_receipt` |

Two operations hold no model action and stay under the obligations of the
rows above: `restore_document` commits one document from history as an
unreviewed draft with its own receipt and never advances the baseline
(SPEC 15.3 excludes a separate restore action; its real Git tests are
`tests/restore_document.rs::restoration_replaces_one_document_from_adopted_history`
and `tests/restore_document.rs::an_exact_retry_returns_the_original_receipt`),
and every query — status, diff, show, log, and every dry run — writes
nothing, so it refines a stuttering observation that the model's
`[Next]_vars` permits without one record.

## Trace records

A test turns the trace on with the environment variable
`MARGIN_MODEL_TRACE=<file>` for the executable, or by building
`Operations::with_model_trace` in process. Each record is one JSON object
on one line, appended and flushed as it happens, so a killed process leaves
exactly the actions it completed. Tracing never fails an operation and no
operation reads the trace back.

The implementation emits the records of the actions it takes. The test
harness appends the records of the environment — the external writes, the
process deaths, and the restarts — because no Margin code observes itself
dying. One harness record completes the publication of a process killed
between the durable commit and its verification: the model's `Publish` is
atomic, the implementation publishes through Git and then verifies, and the
harness writes the record from the durable state that stock Git and the
shared marker validator observe, which is the same observation the retry
performs.

| Record | Emitter | Identities carried |
| --- | --- | --- |
| `Init` | implementation | the start commit and its result tree |
| `Checkpoint` | implementation | the draft commit, its parent, its result tree, and the `include_staged` flag |
| `Submit` | implementation | the request, the outcome (accepted, receipt, conflict), the mode, the pinned candidate and baseline, the selected groups, and the receipt the reply names |
| `ApplyGroup` | implementation | the request, the group identity, and the still-pinned candidate |
| `UpdateIndex` | implementation | the request, the pinned candidate, and the verified index tree |
| `Publish` | implementation, or the harness for one killed process | the request, the review commit, its parent, and its result tree |
| `Respond` | implementation | the request and the receipt the response names |
| `Fail` | implementation | the request whose protocol failed |
| `InspectRecovery` | implementation | the head the inspection observed |
| `Crash` | harness | none beyond the record itself |
| `Restart` | harness | none beyond the record itself |
| `ExternalWrite` | harness, or the failpoint wrapper that performs the write | the written paths with the blob identities stock Git computes, and whether the value is a third value of the adversarial trace |
| `ExternalIndex` | harness | the same shape as `ExternalWrite` |
| `RepositoryCondition` | harness | whether the repository stays ready |

## The conformance check

`tests/support/model.rs` holds the checker and `tests/refinement_trace.rs`
the scenarios. The checker shares no code with the implementation: it
transcribes the phase machine and the identity rules of the TLA+ module
and replays one trace against them.

What it validates, per record:

- The phase machine: `Checkpoint` and `Submit` need idle, `Fail` needs one
  active phase, `Restart` needs down, and `InspectRecovery` returns
  recovery to idle. One inspection over an idle workspace is one
  stuttering read-only observation, which the model permits.
- The submission protocol: one `ApplyGroup` per remaining group, then
  `UpdateIndex`, then `Publish`, then `Respond`, with the candidate pinned
  at every step (`PinnedWhileApplying`).
- The identity rules: an accepted `Submit` pins the baseline the trace
  reached (`BaselineIsReviewed`), one review receipt per request and no
  re-appended commit (`RequestAtMostOnce`, `HistoryAppendOnly`), one
  `Respond` names exactly its request's receipt (`SuccessHasReceipt`), and
  the published tree is the index result the application verified. Drafts
  never advance the baseline (`DraftDoesNotApprove`).
- The environment boundary: with a cooperative writer, environment records
  during one active phase are violations; without one, an external write
  of a third value inside the apply interval trips the
  `NoUnseenOverwrite` monitor and no `Publish` may follow it, because the
  model's publication guard requires the working tree to equal the
  intended result.

The scenarios of `tests/refinement_trace.rs` cover every emitted record —
`tests/refinement_trace.rs::keep_all_publication_follows_the_model_phases`,
`tests/refinement_trace.rs::restoring_application_applies_each_group_before_publication`,
`tests/refinement_trace.rs::receipt_and_conflict_replies_stay_reply_only`,
`tests/refinement_trace.rs::a_failed_application_recovers_through_inspection_and_a_guarded_capture`,
`tests/refinement_trace.rs::an_interrupted_process_resumes_through_inspection`,
`tests/refinement_trace.rs::a_process_killed_after_publication_loses_only_its_response`,
and
`tests/refinement_trace.rs::an_uncooperative_writer_stops_the_publication`
— and
`tests/refinement_trace.rs::the_checker_rejects_sequences_the_model_forbids`
proves the checker rejects publication without submission, one second
receipt for one request, one checkpoint inside the apply, one stale
baseline acceptance, one response naming another commit, one cooperative
write inside the apply, one index result before the groups complete, one
publication of another tree, one publication over an unseen value, one
restart without one crash, and one failure from an idle workspace.

## The reply-code map

The model's reply values map to the exit classes of SPEC 7.5 and to the
public error codes the implementation returns. Every code below exists in
`src/app.rs`; `tests/refinement_map.rs` keeps it that way.

| Model reply | Public code and exit class | Carrying tests |
| --- | --- | --- |
| `success` | exit 0, no code | `tests/review_apply.rs::keep_all_creates_one_verified_empty_review_commit` |
| `success` over one existing receipt | exit 0 with the receipt named | `tests/receipts.rs::an_exact_retry_returns_the_original_verified_commit` |
| `request-conflict` | `REQUEST_ID_CONFLICT`, exit 3 | `tests/receipts.rs::a_reused_request_id_with_another_plan_conflicts` |
| `incomplete` | `INVALID_PLAN` with exit 2, or `INVALID_ARGUMENT` for malformed input | `tests/review_plans.rs::structural_faults_are_typed_invalid_plans`, `tests/review_plans.rs::a_broken_document_refuses_typed_before_the_application` |
| `precondition` | `UNSUPPORTED_REPOSITORY`, `STAGED_CHANGES`, `BUSY`, `BRANCH_CHANGED`, `NOT_INITIALIZED`, `WRITER_ACTIVE`, or `BASELINE_UNAVAILABLE`, exit 4 | `tests/policy_guards.rs::guards_report_every_blocker_at_once`, `tests/checkpoint.rs::a_held_lock_refuses_the_checkpoint_as_busy` |
| `stale` | `STALE_REVIEW`, exit 3; `DIRTY_SCOPE` keeps exit 4 | `tests/review_apply.rs::stale_plans_refuse_without_writing`, `tests/review_apply.rs::uncaptured_scoped_bytes_refuse_the_application` |
| `failed` | `PARTIAL_MUTATION` or `OUTCOME_UNKNOWN`, exit 5, with `GIT_FAILED`, `HOOK_FAILED`, `IO_FAILED`, and `CONTRACT_VIOLATION` for their causes | `tests/review_apply.rs::an_apply_failure_retains_and_reports_the_partial_application`, `tests/failure_phases.rs::a_timed_out_review_commit_reports_an_unknown_outcome_and_recovers` |
| `model-bound` | No public code: `MaxCommits` bounds exploration only (SPEC 15.2) | none by construction |

## Boundaries the model abstracts

These boundaries carry no trace record; the complementary evidence of
SPEC 15.3 covers each one.

- The scope and the anchor stay constant in one traced scenario. A
  re-anchoring `init --new-start` or one changed scope restarts the
  abstract run at its initial predicate; the real behavior — including
  that A to B to A never revives the old approval — is
  `tests/initialization.rs::new_start_forces_a_fresh_anchor`,
  `tests/initialization.rs::a_changed_scope_creates_its_own_anchor`, and
  `tests/initialization.rs::a_to_b_to_a_uses_the_new_anchor_not_the_old_approval`.
- The model's `Checkpoint` is one atomic action. A failed capture is a
  stutter, so it emits no record; the stage and commit fault injections of
  SPEC 16.6 carry the real behavior in `tests/checkpoint.rs` and
  `tests/initialization.rs`.
- A refused `Submit` — incomplete, precondition, or stale — leaves every
  modeled variable unchanged and its refusal is the operation result, so
  only the receipt and conflict replies, which complete one request's
  lifecycle, emit records.
- The publication window. The implementation publishes through Git and
  then verifies, while the model publishes atomically; the trace records
  `Publish` after verification, and the harness completes the record for a
  process killed inside that window, as described above.
- The model's history holds Margin commits only. An external commit by the
  writer advances the real head between records without one; the checker's
  identity rules never depend on an untracked head, and the baseline and
  receipt rules of
  `tests/baseline_selection.rs::ordinary_commits_and_external_merges_leave_the_prior_baseline`
  and `tests/receipts.rs::rewritten_history_no_longer_promises_deduplication`
  carry the real ancestry obligations.
- A direct retry that skips `doctor` after one restart answers with its
  receipt and mutates nothing, which refines the inspection the model
  requires before new mutations; the traced scenarios drive the modeled
  order, and `tests/receipts.rs::present_day_blockers_never_hide_the_receipt`
  carries the direct path.

## Maintenance

Run `cargo test --test refinement_trace --test refinement_map` after any
protocol change, and rerun the formal gate of SPEC 15.8 through
`formal/run-profiles.sh` after any model change. A conformance failure
names the record and the rule it violated; fix the divergence in whichever
artifact is wrong, never in the checker alone.
