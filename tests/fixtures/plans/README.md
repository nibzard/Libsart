# Review plan fixtures

The parser fixtures of the review plan input contract (SPEC 7.3). Every
document in `valid/` must parse through `margin::domain::ReviewPlan::parse`
without a refusal; every document in `invalid/` must refuse with the
problem `tests/review_plans.rs` maps to its file name. The shipped JSON
Schema of the same contract lives at `schemas/review-plan-v1.json` and is
generated from the field list the parser reads, so the two cannot drift.

The context values are well-formed identities, not identifiers of a real
repository. A fixture proves the document contract only; the coverage,
staleness, and conflict rules need a real pinned review and are covered by
the real-Git cases of `tests/review_plans.rs`.

## What each invalid fixture breaks

| Fixture | Broken rule |
| --- | --- |
| `bad-*.json` | One context field or decision identity is not in its documented form. |
| `duplicate-decision.json`, `overlapping-decision.json` | One identity is decided twice, or both restored and kept. |
| `duplicate-key.json` | One JSON key is stated twice. |
| `false-without-keep.json`, `no-decision.json`, `two-decisions.json` | No decision mode, or two at once. |
| `missing-*.json` | One required field is absent. |
| `not-an-object.json`, `truncated.json` | The document is not one JSON object. |
| `null-message.json`, `wrong-type-*.json`, `non-string-in-keep.json` | One field carries the wrong JSON shape. |
| `oversized-message.json`, `trailer-injection.json` | The message breaks the shared free-text rule. |
| `unknown-field.json` | A field the contract does not define. |
| `unsupported-version.json` | A `schema_version` this build does not read. |
