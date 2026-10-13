//! The finish flow of one review (SPEC 5.2, 11.3; task T043).
//!
//! The finish action is the one key of this surface that writes, and it
//! writes only through the shared review operation the command line
//! exposes. The plan the writer confirms is the same document `margin
//! review --input` reads (SPEC 7.3), byte for byte, so two equivalent
//! plans cannot diverge in result tree, marker meaning, errors, or effects
//! (SPEC 16.9).
//!
//! The flow holds two explicit steps (SPEC 5.2). `Enter` opens a summary
//! of the entire writing scope — documents affected, passages restored,
//! changes kept, unvisited documents included — beside the intended result
//! one read-only dry run proved (SPEC 6.4). Only a second `Enter` on the
//! labeled action, Keep remaining changes and finish, applies it, because
//! no undecided passage and no unvisited document may be approved by
//! navigating away. `Esc` leaves the summary, and quit never applies: the
//! unapplied selections are discarded only after the standing notice of
//! SPEC 5.2.
//!
//! The request identifier is deterministic over the pinned comparison and
//! the proposed restorations, so a retry after a lost response reuses one
//! identity and matches the receipt the first attempt recorded, while
//! different decisions over one comparison take a different identity and
//! never conflict (SPEC 8.7).

use crate::app::{
    Effects, Identities, OperationError, OperationOutcome, Operations, ReviewDecisionResult,
    ReviewRequest,
};
use crate::domain::{
    CommitKind, DecisionMode, PlanIdentity, REVIEW_PLAN_OPERATION, REVIEW_PLAN_VERSION,
};
use crate::git::Git;
use crate::tui::changes::{ChangesView, PlanCounts};
use crate::tui::render;
use crate::tui::wrap::ComposedLine;

/// The request slot of the digest that names one finish. The real
/// identifier cannot hash itself, so the digest holds this fixed
/// placeholder in that slot; only the digest's stability depends on the
/// choice.
const REQUEST_ID_PLACEHOLDER: &str = "tui";

/// The prefix that marks one request identifier the terminal surface
/// composed, next to the identifiers a caller of the command line chooses.
const REQUEST_ID_PREFIX: &str = "tui:";

/// How many rewritten paths the summary lists before a remaining count
/// takes over. Every count stays stated, so the bound hides no decision.
const MAX_LISTED_PATHS: usize = 8;

/// How many characters one error line of the summary occupies.
const NOTICE_CHARS: usize = 200;

/// The plan document one finish confirms, with the identity it carries.
#[derive(Clone, Debug)]
pub struct PreparedPlan {
    /// The deterministic request identifier of this finish (SPEC 7.3).
    pub request_id: String,
    /// The exact plan document bytes: the document `margin review --input`
    /// reads, composed with the same field order and the same decision
    /// mode (SPEC 7.3, 16.9).
    pub document: Vec<u8>,
}

/// The whole-scope summary one finish states, the plan it would apply, and
/// the intended result one read-only preview proved (SPEC 5.2).
#[derive(Clone, Debug)]
pub struct FinishSummary {
    /// The pinned review identity (SPEC 7.3).
    review_id: String,
    /// The baseline B of the pinned comparison.
    baseline_oid: String,
    /// The candidate C of the pinned comparison.
    candidate_oid: String,
    /// The change identities the plan restores, sorted.
    restore: Vec<String>,
    /// The counts of the entire scope the decision covers.
    counts: PlanCounts,
    /// The plan document and its request identifier, or the typed error
    /// that stopped the composition.
    plan: Result<PreparedPlan, OperationError>,
    /// The read-only proof of the intended result (SPEC 6.4): the same
    /// decision data the dry run reports, or its typed refusal.
    preview: ReviewDecisionResult,
}

impl FinishSummary {
    /// Builds the summary of one pinned comparison and the in-memory plan
    /// it holds, and proves the intended result through one read-only dry
    /// run of the shared operation.
    ///
    /// Nothing here writes: the preview is the dry run of SPEC 6.4, which
    /// reserves neither the branch nor the working files.
    pub fn open(operations: &Operations<'_>, changes: &ChangesView) -> FinishSummary {
        let restore = changes.restore_selection();
        let counts = changes.counts();
        let plan = prepared_plan(operations.git(), changes, &restore);
        let preview = match &plan {
            Err(error) => OperationOutcome::failure(error.clone(), Effects::unchanged()),
            Ok(prepared) => operations.validate_review(&ReviewRequest {
                scope: None,
                plan: prepared.document.clone(),
                dry_run: true,
            }),
        };
        FinishSummary {
            review_id: changes.review_id().to_string(),
            baseline_oid: changes.baseline_oid().to_string(),
            candidate_oid: changes.candidate_oid().to_string(),
            restore,
            counts,
            plan,
            preview,
        }
    }

    /// The counts of the entire scope this decision covers.
    pub fn counts(&self) -> PlanCounts {
        self.counts
    }

    /// The change identities the plan restores, sorted.
    pub fn restore(&self) -> &[String] {
        &self.restore
    }

    /// The request identifier this finish carries, when it could be
    /// computed.
    pub fn request_id(&self) -> Option<&str> {
        self.plan.as_ref().ok().map(|plan| plan.request_id.as_str())
    }

    /// The plan document this finish confirms, when it could be composed:
    /// the exact bytes `margin review --input` reads (SPEC 7.3, 16.9).
    pub fn plan_document(&self) -> Option<&[u8]> {
        self.plan
            .as_ref()
            .ok()
            .map(|prepared| prepared.document.as_slice())
    }

    /// The read-only preview of the intended result (SPEC 6.4).
    pub fn preview(&self) -> &ReviewDecisionResult {
        &self.preview
    }

    /// Confirms the finish through the shared review operation (SPEC 9.4).
    ///
    /// The document applied is the document the preview proved, so an
    /// exact retry replays one identity and one fingerprint. A plan that
    /// could not be composed refuses with the typed error that stopped it
    /// and writes nothing.
    pub fn apply(&self, operations: &Operations<'_>) -> ReviewDecisionResult {
        match &self.plan {
            Err(error) => OperationOutcome::failure(error.clone(), Effects::unchanged()),
            Ok(prepared) => operations.apply_review(&ReviewRequest {
                scope: None,
                plan: prepared.document.clone(),
                dry_run: false,
            }),
        }
    }

    /// The lines of the summary: the entire scope, the intended result,
    /// and the explicit keep-remaining intent (SPEC 5.2, 11.3).
    pub fn lines(&self) -> Vec<ComposedLine> {
        let counts = self.counts;
        let mut lines = Vec::new();
        lines.push(ComposedLine::heading("Finish review"));
        lines.push(ComposedLine::text(
            "The entire writing scope, unvisited documents included:",
        ));
        lines.push(ComposedLine::text(format!(
            "{} document{} affected · {} passage group{}",
            counts.documents,
            plural(counts.documents),
            counts.groups,
            plural(counts.groups),
        )));
        lines.push(ComposedLine::text(format!(
            "{} passage{} restored · {} kept · {} undecided",
            counts.restored,
            plural(counts.restored),
            counts.kept,
            counts.undecided,
        )));
        if counts.unvisited > 0 || counts.unvisited_documents > 0 {
            lines.push(ComposedLine::warning(format!(
                "{} passage group{} and {} document{} never opened",
                counts.unvisited,
                plural(counts.unvisited),
                counts.unvisited_documents,
                plural(counts.unvisited_documents),
            )));
        }
        lines.push(ComposedLine::meta(
            "Finishing keeps every passage you did not restore, opened or not.",
        ));
        lines.push(ComposedLine::meta(format!(
            "B {} → C {} · review {}",
            render::short(&self.baseline_oid),
            render::short(&self.candidate_oid),
            render::short(&self.review_id),
        )));
        lines.push(ComposedLine::text(String::new()));
        self.result_lines(&mut lines);
        lines.push(ComposedLine::text(String::new()));
        lines.push(ComposedLine::meta(
            "Enter  Keep remaining changes and finish",
        ));
        lines.push(ComposedLine::meta("Esc    Cancel; nothing is applied"));
        lines
    }

    /// The intended-result half of the summary: what the read-only preview
    /// proved, or the typed error that stopped it.
    fn result_lines(&self, lines: &mut Vec<ComposedLine>) {
        let Err(error) = &self.plan else {
            return self.preview_lines(lines);
        };
        lines.push(ComposedLine::warning(
            "The plan cannot be prepared right now:",
        ));
        lines.push(ComposedLine::warning(format!(
            "{}: {}",
            error.code,
            bounded(&error.message)
        )));
    }

    /// The lines of one proved preview: the decisions it records, the
    /// files it rewrites, and the result tree it intends.
    fn preview_lines(&self, lines: &mut Vec<ComposedLine>) {
        let Some(data) = self.preview.data.as_ref() else {
            let error = self
                .preview
                .error
                .as_ref()
                .expect("a failed outcome carries its error");
            lines.push(ComposedLine::warning(
                "The intended result cannot be proved right now:",
            ));
            lines.push(ComposedLine::warning(format!(
                "{}: {}",
                error.code,
                bounded(&error.message)
            )));
            return;
        };
        lines.push(ComposedLine::text(
            "Intended result, proved by one read-only preview:",
        ));
        let kept = match data.kept {
            Some(kept) => format!("{kept} of {} passage groups kept", self.counts.groups),
            None => "passage groups kept: count unknown".to_string(),
        };
        lines.push(ComposedLine::meta(format!(
            "{kept} · {} restored",
            data.restored.len(),
        )));
        if data.paths.is_empty() {
            lines.push(ComposedLine::meta(
                "Files rewritten: none; the review commit records the kept tree.",
            ));
        } else {
            lines.push(ComposedLine::meta(format!(
                "Files rewritten: {}{}",
                data.paths
                    .iter()
                    .take(MAX_LISTED_PATHS)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
                remaining(&data.paths, MAX_LISTED_PATHS),
            )));
        }
        lines.push(ComposedLine::meta(format!(
            "Result tree {}",
            render::short(&data.expected_tree)
        )));
    }
}

/// Composes the plan one finish confirms (SPEC 7.3): the pinned context
/// the comparison issued, the proposed restorations, and the one decision
/// mode the labeled action states.
///
/// The message is omitted, so the plan carries the deterministic default
/// subject of a review commit; this surface gathers no free text. The
/// request identifier is computed first, because the document cannot name
/// an identity it does not hold yet.
fn prepared_plan(
    git: &Git,
    changes: &ChangesView,
    restore: &[String],
) -> Result<PreparedPlan, OperationError> {
    let placeholder = PlanIdentity {
        operation: REVIEW_PLAN_OPERATION.to_string(),
        request_id: REQUEST_ID_PLACEHOLDER.to_string(),
        review_id: changes.review_id().to_string(),
        branch_ref: changes.branch_ref().to_string(),
        scope_id: changes.scope_id().to_string(),
        baseline_oid: changes.baseline_oid().to_string(),
        candidate_oid: changes.candidate_oid().to_string(),
        restore: restore.to_vec(),
        decision_mode: DecisionMode::KeepRemaining,
        message: CommitKind::Review.default_subject().to_string(),
    };
    let digest = Identities::new(git)
        .and_then(|identities| identities.plan(&placeholder))
        .map_err(OperationError::from)?;
    let request_id = format!("{REQUEST_ID_PREFIX}{}", digest.id());
    let document = serde_json::json!({
        "schema_version": REVIEW_PLAN_VERSION,
        "request_id": request_id,
        "review_id": changes.review_id(),
        "branch_ref": changes.branch_ref(),
        "scope_id": changes.scope_id(),
        "baseline_oid": changes.baseline_oid(),
        "candidate_oid": changes.candidate_oid(),
        "restore": restore,
        "keep_remaining": true,
    })
    .to_string()
    .into_bytes();
    Ok(PreparedPlan {
        request_id,
        document,
    })
}

/// The remaining-count suffix of one bounded path list.
fn remaining(paths: &[String], listed: usize) -> String {
    match paths.len().saturating_sub(listed) {
        0 => String::new(),
        more => format!(", and {more} more"),
    }
}

/// The plural marker one count uses.
fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// Truncates one error line to the notice bound.
fn bounded(text: &str) -> String {
    let mut bounded: String = text.chars().take(NOTICE_CHARS).collect();
    if bounded.chars().count() < text.chars().count() {
        bounded.push('…');
    }
    bounded
}
