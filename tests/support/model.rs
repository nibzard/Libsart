//! The independent trace-conformance checker of the refinement map (SPEC
//! 15.7, task T038).
//!
//! The implementation appends one JSON record per model action it takes to
//! a trace file (see `MODEL_TRACE_VARIABLE` in `src/app.rs`); the test
//! harness appends the records of the environment actions the model blames
//! on nobody but the writer — external writes, crashes, restarts. This
//! module replays such a trace against the allowed transitions of
//! `formal/MarginReview.tla` and refuses every sequence the model forbids.
//!
//! The checker shares no code with the implementation. Every rule below is
//! transcribed from the TLA+ module, and the negative tests in
//! `tests/refinement_trace.rs` prove it rejects disallowed sequences, so
//! shared action names alone cannot pass it (SPEC 15.7).
//!
//! What the checker validates, per record:
//!
//! - The phase machine: `Checkpoint` and `Submit` need `idle`, `Fail` needs
//!   an active phase, `Restart` needs `down`, `InspectRecovery` returns
//!   `recovery` to `idle` (a run over an idle workspace is one stuttering
//!   read-only observation, which `[Next]_vars` permits).
//! - The submission protocol: one `ApplyGroup` per remaining group, then
//!   `UpdateIndex`, then `Publish`, then `Respond`, with the candidate
//!   pinned at every step (`PinnedWhileApplying`).
//! - The identity rules the records carry: an accepted `Submit` pins the
//!   baseline the trace reached (`BaselineIsReviewed`), one review receipt
//!   per request and no re-appended commit (`RequestAtMostOnce`,
//!   `HistoryAppendOnly`), a `Respond` names exactly its request's receipt
//!   (`SuccessHasReceipt`), and the published tree is the index result the
//!   application verified.
//! - The environment boundary: with a cooperative writer, external actions
//!   during an active phase are violations; without one, an external write
//!   of a third value during the apply interval trips the
//!   `NoUnseenOverwrite` monitor, and no `Publish` may follow it, because
//!   the model's publication guard requires the working tree to equal the
//!   intended result.
//!
//! The checker cannot see the working tree or the index between records;
//! the real Git tests cited in `formal/refinement.md` carry those
//! obligations (SPEC 15.3).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

/// The phases of `formal/MarginReview.tla`: the four active protocol phases
/// plus `idle`, `down`, and `recovery`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Apply,
    Index,
    Commit,
    Respond,
    Idle,
    Down,
    Recovery,
}

impl Phase {
    /// Whether the phase is one of the four active protocol phases.
    fn active(self) -> bool {
        matches!(
            self,
            Phase::Apply | Phase::Index | Phase::Commit | Phase::Respond
        )
    }
}

/// The pending submission the trace is inside.
struct Pending {
    request: String,
    /// The candidate the submission pinned; every active-phase record must
    /// still name it (`PinnedWhileApplying`).
    head: String,
    /// The groups the application has not applied yet.
    remaining: BTreeSet<String>,
    /// The tree `UpdateIndex` verified, which `Publish` must commit.
    index_tree: Option<String>,
}

/// The abstract state the checker derives from a trace.
struct Machine {
    phase: Phase,
    repo_ready: bool,
    cooperative: bool,
    /// The review baseline: the initial anchor, then the latest review
    /// commit (`BaselineIsReviewed`, `DraftDoesNotApprove`).
    baseline: Option<String>,
    pending: Option<Pending>,
    /// One review receipt per request (`RequestAtMostOnce`).
    receipts: BTreeMap<String, String>,
    /// Every commit any record ever appended (`HistoryAppendOnly`).
    appended: BTreeSet<String>,
    /// The verification monitor, not an implementation journal.
    unseen_overwrite: bool,
}

impl Machine {
    fn new(cooperative: bool) -> Self {
        Machine {
            phase: Phase::Idle,
            repo_ready: true,
            cooperative,
            baseline: None,
            pending: None,
            receipts: BTreeMap::new(),
            appended: BTreeSet::new(),
            unseen_overwrite: false,
        }
    }

    /// Requires the phase the model requires, or names the violation.
    fn require_phase(&self, action: &str, phase: Phase, needed: &str) -> Result<(), String> {
        if self.phase == phase {
            Ok(())
        } else {
            Err(format!(
                "{action} requires the model phase {needed}, but the trace is in {:?}",
                self.phase
            ))
        }
    }

    fn external_allowed(&self) -> bool {
        !self.cooperative || !self.phase.active()
    }

    fn step(&mut self, record: &Value) -> Result<(), String> {
        let action = field(record, "action")?;
        match action {
            "Init" => {
                let head = field(record, "head")?;
                let _ = field(record, "tree")?;
                // The initial predicate establishes one anchor; a traced
                // scenario holds the scope constant, so a re-anchoring init
                // restarts the abstract run (SPEC 15.3 scope boundary).
                self.phase = Phase::Idle;
                self.repo_ready = true;
                self.baseline = Some(head.to_string());
                self.pending = None;
                self.receipts.clear();
                self.appended.clear();
                self.appended.insert(head.to_string());
                Ok(())
            }
            "Checkpoint" => {
                self.require_phase("Checkpoint", Phase::Idle, "idle")?;
                if !self.repo_ready {
                    return Err("Checkpoint requires a ready repository".to_string());
                }
                let head = field(record, "head")?;
                let _ = field(record, "parent")?;
                let _ = field(record, "tree")?;
                if self.appended.contains(head) {
                    return Err(format!(
                        "Checkpoint re-appends the commit {head}, which the trace already \
                         published (HistoryAppendOnly)"
                    ));
                }
                // A draft never advances the baseline (DraftDoesNotApprove);
                // the parent and the tree stay validated identities.
                self.appended.insert(head.to_string());
                Ok(())
            }
            "Submit" => {
                self.require_phase("Submit", Phase::Idle, "idle")?;
                let request = field(record, "request")?;
                let outcome = field(record, "outcome")?;
                match outcome {
                    "receipt" => {
                        let receipt = field(record, "receipt")?;
                        match self.receipts.get(request) {
                            Some(known) if known == receipt => Ok(()),
                            Some(known) => Err(format!(
                                "Submit answers the request {request} with the commit \
                                 {receipt}, but its receipt is {known} (SuccessHasReceipt)"
                            )),
                            None => Err(format!(
                                "Submit reports one receipt for the unknown request \
                                 {request}"
                            )),
                        }
                    }
                    "conflict" => {
                        if self.receipts.contains_key(request) {
                            Ok(())
                        } else {
                            Err(format!(
                                "Submit refuses the request {request} as one conflict, \
                                 but the trace holds no receipt for it"
                            ))
                        }
                    }
                    "accepted" => {
                        if self.receipts.contains_key(request) {
                            return Err(format!(
                                "Submit accepts the request {request} that already holds \
                                 one receipt (RequestAtMostOnce)"
                            ));
                        }
                        if !self.repo_ready {
                            return Err(
                                "Submit accepts one plan over an unready repository, which \
                                 the model answers with one precondition refusal"
                                    .to_string(),
                            );
                        }
                        let head = field(record, "head")?;
                        let baseline = field(record, "baseline")?;
                        if Some(baseline) != self.baseline.as_deref() {
                            return Err(format!(
                                "Submit accepts one plan pinned to the baseline {baseline}, \
                                 but the reviewed baseline is {:?}; the model answers one \
                                 stale submission, never an acceptance (BaselineIsReviewed)",
                                self.baseline
                            ));
                        }
                        let mode = field(record, "mode")?;
                        let restore = restore_set(record)?;
                        let remaining = if mode == "resume" {
                            // The retained result already passed the
                            // application, so the resume restarts at the
                            // commit phase.
                            BTreeSet::new()
                        } else {
                            restore
                        };
                        self.phase = if remaining.is_empty() {
                            Phase::Commit
                        } else {
                            Phase::Apply
                        };
                        self.pending = Some(Pending {
                            request: request.to_string(),
                            head: head.to_string(),
                            remaining,
                            index_tree: None,
                        });
                        Ok(())
                    }
                    other => Err(format!("Submit carries the unknown outcome {other:?}")),
                }
            }
            "ApplyGroup" => {
                self.require_phase("ApplyGroup", Phase::Apply, "apply")?;
                let pending = self
                    .pending
                    .as_mut()
                    .ok_or_else(|| "ApplyGroup runs with no pending submission".to_string())?;
                let request = field(record, "request")?;
                let group = field(record, "group")?;
                let head = field(record, "head")?;
                if request != pending.request {
                    return Err(format!(
                        "ApplyGroup applies the request {request}, but the pinned submission \
                         is {} (PinnedWhileApplying)",
                        pending.request
                    ));
                }
                if head != pending.head {
                    return Err(pinned_violation("ApplyGroup", &pending.head, head));
                }
                if !pending.remaining.remove(group) {
                    return Err(format!(
                        "ApplyGroup applies the group {group}, which the submission did not \
                         select or already applied"
                    ));
                }
                if pending.remaining.is_empty() {
                    self.phase = Phase::Index;
                }
                Ok(())
            }
            "UpdateIndex" => {
                self.require_phase("UpdateIndex", Phase::Index, "index")?;
                let pending = self
                    .pending
                    .as_mut()
                    .ok_or_else(|| "UpdateIndex runs with no pending submission".to_string())?;
                let request = field(record, "request")?;
                let head = field(record, "head")?;
                let tree = field(record, "tree")?;
                if request != pending.request {
                    return Err(format!(
                        "UpdateIndex publishes the index of the request {request}, but the \
                         pinned submission is {} (PinnedWhileApplying)",
                        pending.request
                    ));
                }
                if head != pending.head {
                    return Err(pinned_violation("UpdateIndex", &pending.head, head));
                }
                pending.index_tree = Some(tree.to_string());
                self.phase = Phase::Commit;
                Ok(())
            }
            "Publish" => {
                self.require_phase("Publish", Phase::Commit, "commit")?;
                let pending = self
                    .pending
                    .as_ref()
                    .ok_or_else(|| "Publish runs with no pending submission".to_string())?;
                let request = field(record, "request")?;
                if request != pending.request {
                    return Err(format!(
                        "Publish commits the request {request}, but the pinned submission is \
                         {} (PinnedWhileApplying)",
                        pending.request
                    ));
                }
                if !self.repo_ready {
                    return Err(
                        "Publish commits over an unready repository, which the model \
                         forbids"
                            .to_string(),
                    );
                }
                let head = field(record, "head")?;
                let parent = field(record, "parent")?;
                let tree = field(record, "tree")?;
                if parent != pending.head {
                    return Err(pinned_violation("Publish", &pending.head, parent));
                }
                if let Some(expected) = pending.index_tree.as_deref()
                    && expected != tree
                {
                    return Err(format!(
                        "Publish commits the tree {tree}, but the application verified \
                         the index result {expected}"
                    ));
                }
                if let Some(existing) = self.receipts.get(request) {
                    return Err(format!(
                        "Publish creates one second receipt {head} for the request \
                         {request}, which already holds {existing} (RequestAtMostOnce)"
                    ));
                }
                if self.appended.contains(head) {
                    return Err(format!(
                        "Publish re-appends the commit {head}, which the trace already \
                         published (HistoryAppendOnly)"
                    ));
                }
                if self.unseen_overwrite {
                    return Err(
                        "Publish follows one external write of a third value during the \
                         apply interval, which the model's publication guard refuses \
                         (NoUnseenOverwrite)"
                            .to_string(),
                    );
                }
                self.receipts.insert(request.to_string(), head.to_string());
                self.appended.insert(head.to_string());
                self.baseline = Some(head.to_string());
                self.phase = Phase::Respond;
                Ok(())
            }
            "Respond" => {
                self.require_phase("Respond", Phase::Respond, "respond")?;
                let pending = self
                    .pending
                    .as_ref()
                    .ok_or_else(|| "Respond runs with no pending submission".to_string())?;
                let request = field(record, "request")?;
                let receipt = field(record, "receipt")?;
                if request != pending.request {
                    return Err(format!(
                        "Respond answers the request {request}, but the pinned submission \
                         is {} (PinnedWhileApplying)",
                        pending.request
                    ));
                }
                match self.receipts.get(request) {
                    Some(known) if known == receipt => {}
                    Some(known) => {
                        return Err(format!(
                            "Respond names the commit {receipt}, but the receipt of \
                             {request} is {known} (SuccessHasReceipt)"
                        ));
                    }
                    None => {
                        return Err(format!(
                            "Respond names the commit {receipt} as one receipt the trace \
                             never committed for {request}"
                        ));
                    }
                }
                self.phase = Phase::Idle;
                self.pending = None;
                Ok(())
            }
            "Fail" => {
                if !self.phase.active() || self.phase == Phase::Respond {
                    return Err(format!(
                        "Fail requires one of the model phases apply, index, or commit, but \
                         the trace is in {:?}",
                        self.phase
                    ));
                }
                let pending = self
                    .pending
                    .as_ref()
                    .ok_or_else(|| "Fail runs with no pending submission".to_string())?;
                let request = field(record, "request")?;
                if request != pending.request {
                    return Err(format!(
                        "Fail reports the request {request}, but the pinned submission is \
                         {} (PinnedWhileApplying)",
                        pending.request
                    ));
                }
                self.phase = Phase::Recovery;
                self.pending = None;
                Ok(())
            }
            "Crash" => {
                if self.phase == Phase::Down {
                    return Err(
                        "Crash repeats over a process the model already holds down".to_string()
                    );
                }
                self.phase = Phase::Down;
                self.pending = None;
                Ok(())
            }
            "Restart" => {
                self.require_phase("Restart", Phase::Down, "down")?;
                self.phase = Phase::Recovery;
                Ok(())
            }
            "InspectRecovery" => {
                match self.phase {
                    Phase::Recovery => self.phase = Phase::Idle,
                    // One inspection over a workspace whose last modeled
                    // transition succeeded changes nothing: a stuttering
                    // step `[Next]_vars` permits.
                    Phase::Idle => {}
                    other => {
                        return Err(format!(
                            "InspectRecovery requires the model phase recovery or one \
                             read-only stutter over idle, but the trace is in {other:?}"
                        ));
                    }
                }
                let _ = record.get("head");
                Ok(())
            }
            "ExternalWrite" | "ExternalIndex" => {
                if !self.external_allowed() {
                    return Err(format!(
                        "{action} happens while the trace is in {:?}, but a cooperative \
                         writer stays outside the active protocol phases",
                        self.phase
                    ));
                }
                if action == "ExternalWrite"
                    && self.phase.active()
                    && record.get("unseen").and_then(Value::as_bool) == Some(true)
                {
                    // The verification monitor: one third value neither the
                    // candidate nor the intended result holds.
                    self.unseen_overwrite = true;
                }
                Ok(())
            }
            "RepositoryCondition" => {
                if !self.external_allowed() {
                    return Err(format!(
                        "{action} happens while the trace is in {:?}, but a cooperative \
                         environment stays outside the active protocol phases",
                        self.phase
                    ));
                }
                self.repo_ready = record.get("ready").and_then(Value::as_bool) == Some(true);
                Ok(())
            }
            other => Err(format!("the trace names the unknown action {other:?}")),
        }
    }
}

/// The verdict of one conforming trace.
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    /// Whether an external write replaced an intervening third value during
    /// one apply interval; a verification monitor, never a claim about the
    /// implementation.
    pub unseen_overwrite: bool,
}

/// Reads one non-empty string field, or names the missing identity.
fn field<'a>(record: &'a Value, name: &str) -> Result<&'a str, String> {
    let value = record
        .get(name)
        .ok_or_else(|| format!("the {} record carries no {name}", action_of(record)))?;
    value
        .as_str()
        .filter(|text| !text.is_empty())
        .ok_or_else(|| format!("the {name} identity of the record is not one non-empty string"))
}

fn action_of(record: &Value) -> String {
    record
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("<unnamed>")
        .to_string()
}

/// Reads the restored-group identities one accepted Submit selected.
fn restore_set(record: &Value) -> Result<BTreeSet<String>, String> {
    let groups = record
        .get("restore")
        .and_then(Value::as_array)
        .ok_or_else(|| "the accepted Submit carries no restore identities".to_string())?;
    let mut set = BTreeSet::new();
    for group in groups {
        let group = group
            .as_str()
            .filter(|text| !text.is_empty())
            .ok_or_else(|| "one restored-group identity is not one non-empty string".to_string())?;
        if !set.insert(group.to_string()) {
            return Err(format!(
                "the accepted Submit selects the group {group} more than once"
            ));
        }
    }
    Ok(set)
}

/// Reads the request identity one active-phase record carries.
fn pinned_violation(action: &str, pinned: &str, observed: &str) -> String {
    format!(
        "{action} runs over the head {observed} while the submission pins {pinned} \
         (PinnedWhileApplying)"
    )
}

/// Replays one trace, whose lines are JSON records, against the allowed
/// transitions of `formal/MarginReview.tla`. `cooperative` mirrors the
/// model constant: a cooperative environment never acts during an active
/// protocol phase.
///
/// Returns the outcome of one conforming trace, or the first violation with
/// its 1-based record number.
pub fn check(trace: &str, cooperative: bool) -> Result<Outcome, String> {
    let mut machine = Machine::new(cooperative);
    for (number, line) in trace.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let record: Value = serde_json::from_str(line)
            .map_err(|error| format!("record {} is not one JSON object: {error}", number + 1))?;
        if !record.is_object() {
            return Err(format!("record {} is not one JSON object", number + 1));
        }
        machine
            .step(&record)
            .map_err(|message| format!("record {}: {message}", number + 1))?;
    }
    Ok(Outcome {
        unseen_overwrite: machine.unseen_overwrite,
    })
}
