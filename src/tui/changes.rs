//! The Changes view: every group's source context and the in-memory plan
//! (SPEC 5.2, 5.4, 11.3; task T042).
//!
//! The view loads every group of the pinned comparison by following the
//! detail pages the shared `diff` operation issues, so no group is reduced
//! to a path name without its context. The groups then form one reading
//! column: each block states its heading, the exact passages of both
//! sides, the keys that decide it, and the label that names its state in
//! words (SPEC 11.4).
//!
//! `K` and `r` change this plan only. The plan holds proposed decisions
//! and the groups the writer visited; nothing here reaches the repository.
//! The finish flow of [`crate::tui::finish`] hands the same decisions to
//! the shared review operation, and only that flow writes. A stale
//! arrival (SPEC 5.3) relabels every group and refuses new decisions
//! while the text stays readable.
//!
//! The source view (`s`) is the exact fallback of SPEC 5.2 and 11.3: the
//! `@@` coordinates of every range and the exact excerpt bytes of both
//! sides, with every bound stated, never hidden.

use std::collections::{BTreeMap, BTreeSet};

use crate::app::{
    BoundedExcerpt, ChangeDetail, DiffRequest, Operations, ReviewOutcome, ScopeTotals,
};
use crate::tui::render;
use crate::tui::wrap::{self, DisplayRow};

/// How many detail pages one load follows. The page contract bounds each
/// page, not the whole comparison, so this bound keeps one load finite;
/// beyond it the view states an incomplete comparison instead of looping.
/// At the default page size the bound covers several times the acceptance
/// envelope of one thousand scoped documents (SPEC 3.3), so it trips only
/// outside the supported envelope, and it says so when it does.
const MAX_DETAIL_PAGES: usize = 64;

/// How many changed documents the column header lists before a remaining
/// count takes over. Every group block names its own document, so the
/// header list is a summary, not the coverage.
const MAX_LISTED_DOCUMENTS: usize = 8;

/// How many word hints one group block shows before a remaining count.
const MAX_SHOWN_WORD_HINTS: usize = 8;

/// How many hexadecimal characters one opaque side shows.
const MAX_HEX_CHARS: usize = 96;

/// How many characters one reason or notice occupies.
const NOTICE_CHARS: usize = 200;

/// One proposed decision of the in-memory plan (SPEC 5.2, 11.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Keep the passage as the candidate holds it.
    Keep,
    /// Restore the previous passage (SPEC 5.2).
    Restore,
}

impl Decision {
    /// The label the interface prints (SPEC 11.4).
    pub fn label(self) -> &'static str {
        match self {
            Decision::Keep => "Kept",
            Decision::Restore => "Restored",
        }
    }
}

/// The state of one change group, as its label names it in words (SPEC
/// 11.4): kept, restored, undecided, unavailable, or stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupState {
    /// No decision exists for this group yet.
    Undecided,
    /// The plan holds one proposed decision.
    Decided(Decision),
    /// No decision can exist in this state: the comparison is not
    /// actionable or its detail pages did not complete.
    Unavailable,
    /// The pinned comparison is no longer the current one (SPEC 5.3).
    Stale,
}

impl GroupState {
    /// The label the interface prints. Words, never hue alone (SPEC 11.4).
    pub fn label(self) -> &'static str {
        match self {
            GroupState::Undecided => "Undecided",
            GroupState::Decided(Decision::Keep) => "Kept",
            GroupState::Decided(Decision::Restore) => "Restored",
            GroupState::Unavailable => "Unavailable",
            GroupState::Stale => "Stale",
        }
    }
}

/// What one toggle of `K` or `r` did (SPEC 11.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToggleOutcome {
    /// The plan recorded the decision.
    Recorded,
    /// The plan removed the decision it held.
    Undone,
    /// No decision changed, for the reason given.
    Refused(String),
}

/// The in-memory plan (SPEC 5.2): the proposed decisions and the groups
/// the writer visited. Nothing here reaches the repository.
#[derive(Clone, Debug, Default)]
pub struct PlanDraft {
    decisions: BTreeMap<String, Decision>,
    visited: BTreeSet<String>,
}

impl PlanDraft {
    /// The decision recorded for one change, when one exists.
    pub fn choice(&self, change_id: &str) -> Option<Decision> {
        self.decisions.get(change_id).copied()
    }

    /// Records one decision, replacing whatever the group held.
    fn record(&mut self, change_id: &str, decision: Decision) {
        self.decisions.insert(change_id.to_string(), decision);
    }

    /// Removes the decision one group holds; returns whether one existed.
    fn clear(&mut self, change_id: &str) -> bool {
        self.decisions.remove(change_id).is_some()
    }

    /// How many proposed decisions the plan holds.
    pub fn proposed_count(&self) -> usize {
        self.decisions.len()
    }

    /// Marks one change group as visited.
    fn visit(&mut self, change_id: &str) {
        self.visited.insert(change_id.to_string());
    }

    /// Says whether the writer visited one change group.
    pub fn is_visited(&self, change_id: &str) -> bool {
        self.visited.contains(change_id)
    }
}

/// The counts one summary states (SPEC 5.2): the decisions the plan holds
/// and the scope they still have to cover.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanCounts {
    /// Change groups of the comparison.
    pub groups: usize,
    /// Documents those groups touch.
    pub documents: usize,
    /// Groups the plan keeps.
    pub kept: usize,
    /// Groups the plan restores.
    pub restored: usize,
    /// Groups with no decision yet.
    pub undecided: usize,
    /// Groups the writer never visited.
    pub unvisited: usize,
    /// Documents no group of which the writer visited.
    pub unvisited_documents: usize,
}

/// The Changes view state: the pinned comparison, every group it holds,
/// and the plan the keys change.
pub struct ChangesView {
    review_id: String,
    branch_ref: String,
    scope_id: String,
    baseline_oid: String,
    candidate_oid: String,
    actionable: bool,
    totals: ScopeTotals,
    groups: Vec<ChangeDetail>,
    /// Why the detail pages stopped before every group, when they did.
    incomplete: Option<String>,
    /// Why the pinned comparison is stale, when it is (SPEC 5.3).
    stale: Option<String>,
    plan: PlanDraft,
    selection: usize,
    /// Whether the source view replaces the passage view (SPEC 11.3 `s`).
    source: bool,
    // The derived display state: the wrapped rows of the column, the first
    // row of every group block, the width they were wrapped at, and the
    // top visible row.
    rows: Vec<DisplayRow>,
    starts: Vec<usize>,
    width: u16,
    dirty: bool,
    offset: usize,
    viewport: usize,
}

impl ChangesView {
    /// Loads every group of the current comparison through the shared
    /// `diff` operation (SPEC 6.3).
    ///
    /// The first page failure means no comparison exists to show, so the
    /// caller states that; a later page failure leaves the groups already
    /// loaded and records why the rest are missing, because a comparison
    /// the interface cannot see completely is not one it may decide.
    pub fn load(operations: &Operations<'_>) -> Option<ChangesView> {
        let mut cursor: Option<String> = None;
        let mut groups: Vec<ChangeDetail> = Vec::new();
        let mut identity: Option<ReviewOutcome> = None;
        let mut incomplete: Option<String> = None;
        for _ in 0..MAX_DETAIL_PAGES {
            let outcome = operations.build_review(&DiffRequest {
                scope: None,
                path_filter: Vec::new(),
                change: None,
                cursor: cursor.clone(),
                limit: None,
                working: false,
            });
            if !outcome.ok() {
                if groups.is_empty() {
                    return None;
                }
                let error = outcome.error.expect("a failed outcome carries its error");
                incomplete = Some(bounded(&error.message));
                break;
            }
            let data = outcome.data?;
            if identity.is_none() {
                identity = Some(data.clone());
            }
            groups.extend(data.changes);
            match data.next_cursor {
                Some(next) => cursor = Some(next),
                None => {
                    cursor = None;
                    break;
                }
            }
        }
        if cursor.is_some() && incomplete.is_none() {
            incomplete = Some(format!(
                "the detail pages stopped after {MAX_DETAIL_PAGES} pages"
            ));
        }
        let first = identity?;
        let totals = first.scope_totals;
        if incomplete.is_none() && groups.len() != totals.groups {
            // A complete page set that misses groups the totals count is
            // not complete; the plan refuses to decide what it never saw.
            incomplete = Some(format!(
                "the detail pages returned {} of {} groups",
                groups.len(),
                totals.groups,
            ));
        }
        Some(ChangesView {
            review_id: first.review_id,
            branch_ref: first.branch_ref,
            scope_id: first.scope_id,
            baseline_oid: first.baseline_oid,
            candidate_oid: first.candidate_oid,
            actionable: first.actionable,
            totals,
            groups,
            incomplete,
            stale: None,
            plan: PlanDraft::default(),
            selection: 0,
            source: false,
            rows: Vec::new(),
            starts: Vec::new(),
            width: 0,
            dirty: true,
            offset: 0,
            viewport: 1,
        })
    }

    /// The change groups this view holds, in manifest order.
    pub fn groups(&self) -> &[ChangeDetail] {
        &self.groups
    }

    /// The whole-scope totals of the pinned comparison.
    pub fn totals(&self) -> ScopeTotals {
        self.totals
    }

    /// The pinned candidate C.
    pub fn candidate_oid(&self) -> &str {
        &self.candidate_oid
    }

    /// The baseline B of the pinned comparison.
    pub fn baseline_oid(&self) -> &str {
        &self.baseline_oid
    }

    /// The identity of the pinned comparison (SPEC 7.2).
    pub fn review_id(&self) -> &str {
        &self.review_id
    }

    /// The full adopted branch ref the comparison ran on.
    pub fn branch_ref(&self) -> &str {
        &self.branch_ref
    }

    /// The full scope digest of the comparison (SPEC 7.2).
    pub fn scope_id(&self) -> &str {
        &self.scope_id
    }

    /// Why the detail pages did not complete, when they did not.
    pub fn incomplete(&self) -> Option<&str> {
        self.incomplete.as_deref()
    }

    /// Why the pinned comparison is stale, when it is.
    pub fn stale(&self) -> Option<&str> {
        self.stale.as_deref()
    }

    /// The wrapped rows of the reading column.
    pub fn rows(&self) -> &[DisplayRow] {
        &self.rows
    }

    /// The first visible row of the column.
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// The selected change group (SPEC 5.4).
    pub fn selection(&self) -> usize {
        self.selection
    }

    /// The selected group's detail, when one exists.
    pub fn selected_group(&self) -> Option<&ChangeDetail> {
        self.groups.get(self.selection)
    }

    /// Says whether the source view is on.
    pub fn source_view(&self) -> bool {
        self.source
    }

    /// Toggles the source view and returns whether it is now on.
    pub fn toggle_source(&mut self) -> bool {
        self.source = !self.source;
        self.dirty = true;
        self.source
    }

    /// The state of one group, as its label names it (SPEC 11.4). A stale
    /// comparison outranks every decision, and a comparison that cannot be
    /// decided outranks the decisions it would hold.
    pub fn state_of(&self, index: usize) -> GroupState {
        if self.stale.is_some() {
            return GroupState::Stale;
        }
        if !self.decidable() {
            return GroupState::Unavailable;
        }
        match self.groups.get(index) {
            Some(group) => match self.plan.choice(&group.change_id) {
                Some(decision) => GroupState::Decided(decision),
                None => GroupState::Undecided,
            },
            None => GroupState::Unavailable,
        }
    }

    /// Says whether the keys may record decisions right now.
    fn decidable(&self) -> bool {
        self.stale.is_none() && self.actionable && self.incomplete.is_none()
    }

    /// Why the keys may not record decisions, when they may not.
    pub fn refusal(&self) -> Option<String> {
        if let Some(why) = &self.stale {
            return Some(format!("New edits arrived. Refresh before applying. {why}"));
        }
        if !self.actionable {
            return Some("decisions are unavailable: the comparison is not actionable".to_string());
        }
        if let Some(why) = &self.incomplete {
            return Some(format!("decisions are unavailable: {why}"));
        }
        None
    }

    /// Toggles one decision on the selected group (SPEC 11.3: `K` records
    /// a keep, `r` toggles a proposed restoration). One decision replaces
    /// the other; the same key again undoes the decision it holds.
    pub fn toggle(&mut self, decision: Decision) -> ToggleOutcome {
        let Some(group) = self.groups.get(self.selection) else {
            return ToggleOutcome::Refused("no change groups exist in this review".to_string());
        };
        if let Some(why) = self.refusal() {
            return ToggleOutcome::Refused(why);
        }
        let change_id = group.change_id.clone();
        if self.plan.choice(&change_id) == Some(decision) {
            self.plan.clear(&change_id);
            self.dirty = true;
            return ToggleOutcome::Undone;
        }
        self.plan.record(&change_id, decision);
        self.dirty = true;
        ToggleOutcome::Recorded
    }

    /// The number of proposed decisions the plan holds.
    pub fn proposed_count(&self) -> usize {
        self.plan.proposed_count()
    }

    /// The change identities the plan proposes to restore, sorted: the
    /// `restore` array of the plan document one finish confirms (SPEC
    /// 7.3).
    pub fn restore_selection(&self) -> Vec<String> {
        let mut restore: Vec<String> = self
            .groups
            .iter()
            .filter(|group| self.plan.choice(&group.change_id) == Some(Decision::Restore))
            .map(|group| group.change_id.clone())
            .collect();
        restore.sort();
        restore
    }

    /// Observes the candidate the repository holds now. A head that no
    /// longer equals the pinned C is one arrival of SPEC 5.3, so every
    /// label turns stale; the text stays readable and the decisions stay
    /// in memory.
    pub fn observe_head(&mut self, head: Option<&str>) {
        if let Some(head) = head
            && head != self.candidate_oid
        {
            self.observe_arrival(&format!(
                "new content: the candidate moved to {} after this review was pinned",
                render::short(head),
            ));
        }
    }

    /// Observes one arrival the polling or the head check found (SPEC
    /// 5.3): the pinned comparison turns stale, every group relabels, and
    /// no decision or application may proceed until one explicit refresh
    /// rebuilds the review. The first arrival stands; a later one cannot
    /// claim more staleness than exists. Returns whether this call newly
    /// marked the comparison stale.
    pub fn observe_arrival(&mut self, why: &str) -> bool {
        if self.stale.is_some() {
            return false;
        }
        self.stale = Some(why.to_string());
        self.dirty = true;
        true
    }

    /// Marks the selected group visited, as entering the view does.
    pub fn visit_selection(&mut self) {
        if let Some(group) = self.groups.get(self.selection) {
            let change_id = group.change_id.clone();
            self.plan.visit(&change_id);
            self.dirty = true;
        }
    }

    /// Finds the next group whose own text contains `needle`, folded to
    /// one case, and selects it (SPEC 11.3 `/`). Search moves the
    /// selection only: it changes no content and no decision. Returns
    /// the matched index.
    pub fn search(&mut self, needle: &str) -> Option<usize> {
        let folded = needle.to_lowercase();
        for step in 1..=self.groups.len() {
            let index = (self.selection + step) % self.groups.len();
            if self.group_text(index).to_lowercase().contains(&folded) {
                self.select(index);
                return Some(index);
            }
        }
        None
    }

    /// The searchable text of one group: its path, its kind, its reason,
    /// and the exact passages of both sides.
    fn group_text(&self, index: usize) -> String {
        let group = &self.groups[index];
        let excerpt = |excerpt: &BoundedExcerpt| {
            excerpt
                .text
                .clone()
                .unwrap_or_else(|| excerpt.hex.clone().unwrap_or_default())
        };
        format!(
            "{} {} {} {} {}",
            group.path,
            group.kind,
            group.reason,
            excerpt(&group.presentation.before),
            excerpt(&group.presentation.after),
        )
    }

    /// Moves the selection one group and scrolls to it; returns whether
    /// the selection moved (SPEC 11.3 `n` and `p`).
    pub fn step_selection(&mut self, direction: i32) -> bool {
        if self.groups.is_empty() {
            return false;
        }
        let next = if direction > 0 {
            (self.selection + 1).min(self.groups.len() - 1)
        } else {
            self.selection.saturating_sub(1)
        };
        if next == self.selection {
            return false;
        }
        self.select(next);
        true
    }

    /// Selects one group and scrolls to its first row.
    fn select(&mut self, index: usize) {
        self.set_selection(index);
        if let Some(&start) = self.starts.get(self.selection) {
            self.offset = start;
            self.clamp_offset();
        }
    }

    /// Records the selection without moving the viewport.
    fn set_selection(&mut self, index: usize) {
        let index = index.min(self.groups.len().saturating_sub(1));
        if index == self.selection {
            return;
        }
        self.selection = index;
        self.visit_selection();
    }

    /// Scrolls the column by `rows` rows; the selection follows the group
    /// the top visible row shows (SPEC 11.3 `j`, `k`, and paging).
    pub fn scroll_by(&mut self, rows: i32) {
        if rows >= 0 {
            self.offset = self.offset.saturating_add(rows as usize);
        } else {
            self.offset = self.offset.saturating_sub(rows.unsigned_abs() as usize);
        }
        self.clamp_offset();
        let mut current = 0;
        for (index, &start) in self.starts.iter().enumerate() {
            if start <= self.offset {
                current = index;
            } else {
                break;
            }
        }
        self.set_selection(current);
    }

    /// Wraps the column for one frame size; the caller renders
    /// afterwards. The column is recomposed when the width, the plan, the
    /// selection, or the comparison state changed.
    pub fn prepare(&mut self, width: u16, viewport_rows: usize) {
        self.viewport = viewport_rows.max(1);
        if self.dirty || self.rows.is_empty() || width != self.width {
            self.compose(width);
            self.width = width;
            self.dirty = false;
        }
        self.clamp_offset();
    }

    /// Clamps the offset to what the rows and viewport allow.
    fn clamp_offset(&mut self) {
        let last = self.rows.len().saturating_sub(self.viewport.max(1));
        self.offset = self.offset.min(last);
    }

    /// The counts one summary states (SPEC 5.2): decisions, and the scope
    /// they still have to cover, unvisited documents included.
    pub fn counts(&self) -> PlanCounts {
        let mut kept = 0;
        let mut restored = 0;
        let mut visited = 0;
        let mut documents: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        for group in &self.groups {
            let entry = documents.entry(group.path.as_str()).or_insert((0, 0));
            entry.0 += 1;
            match self.plan.choice(&group.change_id) {
                Some(Decision::Keep) => kept += 1,
                Some(Decision::Restore) => restored += 1,
                None => {}
            }
            if self.plan.is_visited(&group.change_id) {
                visited += 1;
                entry.1 += 1;
            }
        }
        let groups = self.groups.len();
        PlanCounts {
            groups,
            documents: documents.len(),
            kept,
            restored,
            undecided: groups - kept - restored,
            unvisited: groups - visited,
            unvisited_documents: documents.values().filter(|(_, seen)| *seen == 0).count(),
        }
    }

    /// The plan counts as one context-row sentence.
    pub fn counts_sentence(&self) -> String {
        let counts = self.counts();
        format!(
            "{} kept · {} restored · {} undecided · {} unvisited",
            counts.kept, counts.restored, counts.undecided, counts.unvisited,
        )
    }

    /// The unvisited count alone, for the reduced-metadata row.
    pub fn unvisited_sentence(&self) -> String {
        format!("{} unvisited", self.counts().unvisited)
    }

    /// Composes the whole column and wraps it, recording the first row of
    /// every group block.
    fn compose(&mut self, width: u16) {
        let counts = self.counts();
        let mut lines = Vec::new();
        lines.push(format!(
            "B {} → C {} · review {}",
            render::short(&self.baseline_oid),
            render::short(&self.candidate_oid),
            render::short(&self.review_id),
        ));
        lines.push(format!(
            "{} changed document{} · {} passage group{}",
            self.totals.files,
            plural(self.totals.files),
            self.totals.groups,
            plural(self.totals.groups),
        ));
        lines.push(format!(
            "Plan in memory: {} kept · {} restored · {} undecided · {} unvisited",
            counts.kept, counts.restored, counts.undecided, counts.unvisited,
        ));
        if counts.unvisited_documents > 0 {
            lines.push(format!(
                "{} document{} hold{} no visited passage yet",
                counts.unvisited_documents,
                plural(counts.unvisited_documents),
                if counts.unvisited_documents == 1 {
                    "s"
                } else {
                    ""
                },
            ));
        }
        if !self.actionable {
            lines.push("Decisions unavailable: the comparison is not actionable.".to_string());
        }
        if let Some(why) = &self.incomplete {
            lines.push(format!("Decisions unavailable: {why}"));
        }
        if let Some(why) = &self.stale {
            lines.push(format!("Stale: {why}"));
            lines.push("New edits arrived. Refresh before applying.".to_string());
        }
        lines.push(String::new());
        lines.push("Changed documents:".to_string());
        let documents = self.changed_documents();
        let listed = documents.len().min(MAX_LISTED_DOCUMENTS);
        for (path, count) in documents.iter().take(listed) {
            lines.push(format!("  {path} · {count} group{}", plural(*count)));
        }
        if documents.len() > listed {
            lines.push(format!(
                "  and {} more changed documents",
                documents.len() - listed,
            ));
        }
        lines.push(String::new());
        if self.groups.is_empty() {
            lines.push("No pending changes.".to_string());
        }
        let mut starts = Vec::with_capacity(self.groups.len());
        for index in 0..self.groups.len() {
            // The header lands on the next composed line, one-based.
            starts.push((lines.len() + 1) as u32);
            lines.extend(self.group_block(index));
            lines.push(String::new());
        }
        self.rows = wrap::wrap_text(&lines.join("\n"), width);
        self.starts = starts
            .into_iter()
            .map(|line| wrap::row_for_anchor(&self.rows, line))
            .collect();
    }

    /// The changed documents with their group counts, in path order.
    fn changed_documents(&self) -> Vec<(&str, usize)> {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for group in &self.groups {
            *counts.entry(group.path.as_str()).or_insert(0) += 1;
        }
        counts.into_iter().collect()
    }

    /// Composes one group's block: the header with its state label, the
    /// source coordinates, the deciding keys, and the passage view or the
    /// source view (SPEC 5.2, 10.4, 11.3-11.4).
    fn group_block(&self, index: usize) -> Vec<String> {
        let group = &self.groups[index];
        let state = self.state_of(index);
        let marker = if index == self.selection {
            "› "
        } else {
            "  "
        };
        let mut lines = Vec::new();
        lines.push(format!(
            "{marker}Group {} of {} · {}",
            index + 1,
            self.groups.len(),
            state.label(),
        ));
        lines.push(format!(
            "{} · {}{}{}",
            group.path,
            group.kind,
            if group.whole_file {
                " · whole file"
            } else {
                ""
            },
            if group.opaque { " · opaque bytes" } else { "" },
        ));
        lines.push(format!("Why one decision: {}", group.reason));
        lines.push("r Restore previous passage · K Keep passage · s source view".to_string());
        if self.source {
            self.source_block(group, &mut lines);
        } else {
            self.passage_block(group, &mut lines);
        }
        lines
    }

    /// The passage view of one group: the exact excerpts of both sides,
    /// placed by line range, with the word hints the pairing supports
    /// (SPEC 5.2, 10.4).
    fn passage_block(&self, group: &ChangeDetail, lines: &mut Vec<String>) {
        let presentation = &group.presentation;
        lines.push(format!("Before · {}:", line_span(&presentation.before)));
        excerpt_lines(&presentation.before, None, lines);
        bounded_notice("before", &presentation.before, lines);
        lines.push(format!("After · {}:", line_span(&presentation.after)));
        excerpt_lines(&presentation.after, None, lines);
        bounded_notice("after", &presentation.after, lines);
        let shown = presentation
            .word_hints
            .iter()
            .take(MAX_SHOWN_WORD_HINTS)
            .count();
        if shown > 0 {
            lines.push("Changed words:".to_string());
            for hint in presentation.word_hints.iter().take(MAX_SHOWN_WORD_HINTS) {
                lines.push(format!(
                    "  line {} to {}: {}[{} → {}]{}",
                    hint.before_line,
                    hint.after_line,
                    hint.prefix,
                    hint.removed,
                    hint.added,
                    hint.suffix,
                ));
            }
            let more = presentation.word_hints.len() - shown + presentation.word_hints_omitted;
            if more > 0 {
                lines.push(format!("  and {more} more word hints"));
            }
        } else if presentation.word_hints_omitted > 0 {
            lines.push(format!(
                "{} word hints are omitted by the detail bound",
                presentation.word_hints_omitted,
            ));
        }
    }

    /// The source view of one group (SPEC 11.3 `s`): the `@@` coordinates
    /// of every range and the exact bytes of both sides, so the reader
    /// always holds the exact Markdown changes even where the passage view
    /// was bounded.
    fn source_block(&self, group: &ChangeDetail, lines: &mut Vec<String>) {
        for range in &group.ranges {
            lines.push(format!(
                "@@ -{},{} +{},{} @@",
                range.old_start, range.old_count, range.new_start, range.new_count,
            ));
        }
        if group.ranges_omitted > 0 {
            lines.push(format!(
                "{} more ranges are omitted by the detail bound",
                group.ranges_omitted,
            ));
        }
        if group.ranges.is_empty() && group.ranges_omitted == 0 {
            // An opaque change emits no readable hunks, so no line range
            // exists to state; the exact bytes of both sides are the
            // source, and the view says so instead of implying a loss.
            lines.push(
                "no line ranges apply; the exact bytes of both sides are the source".to_string(),
            );
        }
        lines.push(format!(
            "Before · {}:",
            line_span(&group.presentation.before)
        ));
        excerpt_lines(&group.presentation.before, Some('-'), lines);
        bounded_notice("before", &group.presentation.before, lines);
        lines.push(format!("After · {}:", line_span(&group.presentation.after)));
        excerpt_lines(&group.presentation.after, Some('+'), lines);
        bounded_notice("after", &group.presentation.after, lines);
    }
}

/// The placed line range of one excerpt side, in words.
fn line_span(excerpt: &BoundedExcerpt) -> String {
    match (excerpt.first_line, excerpt.last_line) {
        (0, _) | (_, 0) => String::from("no lines"),
        (first, last) if first == last => format!("line {first}"),
        (first, last) => format!("lines {first} to {last}"),
    }
}

/// Renders the exact lines of one excerpt side, indented for the passage
/// view or prefixed for the source view. Opaque bytes render as one
/// bounded hexadecimal view with the true byte length (SPEC 6.3, 7.2).
fn excerpt_lines(excerpt: &BoundedExcerpt, prefix: Option<char>, lines: &mut Vec<String>) {
    let lead = |text: &str| match prefix {
        Some(sign) => format!("{sign}{text}"),
        None => format!("  {text}"),
    };
    match (&excerpt.text, &excerpt.hex) {
        (Some(text), _) => {
            for line in text.lines() {
                lines.push(lead(line));
            }
        }
        (None, Some(hex)) => {
            let mut shown: String = hex.chars().take(MAX_HEX_CHARS).collect();
            if hex.chars().count() > MAX_HEX_CHARS {
                shown.push('…');
            }
            lines.push(lead(&format!(
                "{shown} ({} bytes, not valid UTF-8)",
                excerpt.total_bytes,
            )));
        }
        _ => lines.push(lead("no readable lines")),
    }
}

/// States what a bounded excerpt hid, when it hid something. The source
/// coordinates stay exact, so the notice names the bytes, never the
/// position (SPEC 7.2).
fn bounded_notice(side: &str, excerpt: &BoundedExcerpt, lines: &mut Vec<String>) {
    if excerpt.truncated && excerpt.text.is_some() {
        let shown = excerpt.text.as_ref().map_or(0, |text| text.len());
        lines.push(format!(
            "the {side} excerpt is bounded: {shown} of {} bytes shown; \
             the source coordinates stay exact",
            excerpt.total_bytes,
        ));
    }
}

/// The plural marker one count uses.
fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// Truncates one status line to the notice bound.
fn bounded(text: &str) -> String {
    let mut bounded: String = text.chars().take(NOTICE_CHARS).collect();
    if bounded.chars().count() < text.chars().count() {
        bounded.push('…');
    }
    bounded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{BoundedExcerpt, BoundedPresentation, ChangeRange};

    /// One hand-built group: the smallest detail the view renders.
    fn detail(change_id: &str, path: &str, before: &str, after: &str) -> ChangeDetail {
        ChangeDetail {
            change_id: change_id.to_string(),
            path: path.to_string(),
            kind: "modified",
            mode_before: Some("100644"),
            mode_after: Some("100644"),
            oid_before: Some("0123456789abcdef".to_string()),
            oid_after: Some("fedcba9876543210".to_string()),
            opaque: false,
            whole_file: false,
            reason: "one independent passage".to_string(),
            ranges: vec![ChangeRange {
                old_start: 3,
                old_count: 1,
                new_start: 3,
                new_count: 1,
            }],
            ranges_omitted: 0,
            presentation: presentation(before, after),
        }
    }

    /// The bounded presentation of one passage.
    fn presentation(before: &str, after: &str) -> BoundedPresentation {
        BoundedPresentation {
            heading: "Passage".to_string(),
            before: excerpt(before, 3),
            after: excerpt(after, 3),
            word_hints: Vec::new(),
            word_hints_omitted: 0,
            whitespace_only: false,
            link_change: false,
            invisible_change: false,
        }
    }

    /// One exact, unbounded excerpt of the lines of `text`.
    fn excerpt(text: &str, first_line: u64) -> BoundedExcerpt {
        BoundedExcerpt {
            first_line,
            last_line: first_line,
            total_bytes: text.len(),
            truncated: false,
            text: Some(text.to_string()),
            hex: None,
        }
    }

    /// One view over hand-built groups; the display state composes on
    /// demand exactly as the loaded one does.
    fn view(groups: Vec<ChangeDetail>) -> ChangesView {
        let totals = ScopeTotals {
            files: groups
                .iter()
                .map(|group| group.path.clone())
                .collect::<BTreeSet<_>>()
                .len(),
            groups: groups.len(),
        };
        ChangesView {
            review_id: "review:1".to_string(),
            branch_ref: "refs/heads/main".to_string(),
            scope_id: "sha1:1".to_string(),
            baseline_oid: "b".repeat(40),
            candidate_oid: "c".repeat(40),
            actionable: true,
            totals,
            groups,
            incomplete: None,
            stale: None,
            plan: PlanDraft::default(),
            selection: 0,
            source: false,
            rows: Vec::new(),
            starts: Vec::new(),
            width: 0,
            dirty: true,
            offset: 0,
            viewport: 1,
        }
    }

    /// The composed column as one string, at a reading width.
    fn column(view: &mut ChangesView) -> String {
        view.prepare(76, 40);
        view.rows
            .iter()
            .map(|row| row.text.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn states_label_themselves_in_words() {
        assert_eq!(GroupState::Undecided.label(), "Undecided");
        assert_eq!(GroupState::Decided(Decision::Keep).label(), "Kept");
        assert_eq!(GroupState::Decided(Decision::Restore).label(), "Restored",);
        assert_eq!(GroupState::Unavailable.label(), "Unavailable");
        assert_eq!(GroupState::Stale.label(), "Stale");
    }

    #[test]
    fn decisions_toggle_undo_and_stay_in_memory() {
        let mut view = view(vec![
            detail("a", "notes/one.md", "old one", "new one"),
            detail("b", "notes/two.md", "old two", "new two"),
        ]);
        view.visit_selection();
        assert_eq!(view.state_of(0), GroupState::Undecided);

        // K records a keep; K again undoes it.
        assert_eq!(view.toggle(Decision::Keep), ToggleOutcome::Recorded);
        assert_eq!(view.state_of(0), GroupState::Decided(Decision::Keep));
        assert_eq!(view.toggle(Decision::Keep), ToggleOutcome::Undone);
        assert_eq!(view.state_of(0), GroupState::Undecided);

        // r records a restoration and replaces a keep; r again undoes it.
        assert_eq!(view.toggle(Decision::Keep), ToggleOutcome::Recorded);
        assert_eq!(view.toggle(Decision::Restore), ToggleOutcome::Recorded);
        assert_eq!(view.state_of(0), GroupState::Decided(Decision::Restore));
        assert_eq!(view.toggle(Decision::Restore), ToggleOutcome::Undone);
        assert_eq!(view.state_of(0), GroupState::Undecided);
        assert_eq!(view.proposed_count(), 0);

        // A decision on one group leaves the others undecided.
        assert_eq!(view.toggle(Decision::Keep), ToggleOutcome::Recorded);
        assert_eq!(view.state_of(1), GroupState::Undecided);
        assert_eq!(view.proposed_count(), 1);
    }

    #[test]
    fn stale_comparisons_relabel_every_group_and_refuse_decisions() {
        let mut view = view(vec![detail("a", "notes/one.md", "old", "new")]);
        let pinned = "c".repeat(40);
        view.observe_head(Some(pinned.as_str()));
        assert!(view.stale().is_none(), "the pinned head is not stale");
        let moved = "d".repeat(40);
        view.observe_head(Some(moved.as_str()));
        assert_eq!(view.state_of(0), GroupState::Stale);
        match view.toggle(Decision::Keep) {
            ToggleOutcome::Refused(why) => assert!(
                why.starts_with("New edits arrived. Refresh before applying."),
                "the refusal names the stale arrival: {why}",
            ),
            other => panic!("a stale comparison refuses decisions, got {other:?}"),
        }
        assert_eq!(view.state_of(0), GroupState::Stale);
        // The column relabels the group and states the SPEC 5.3 sentence,
        // while the passage itself stays readable.
        let stale = column(&mut view);
        assert!(stale.contains("Group 1 of 1 · Stale"));
        assert!(stale.contains("New edits arrived. Refresh before applying."));
        assert!(stale.contains("new"), "the passage still reads");
        // A missing answer cannot claim more staleness than exists.
        view.observe_head(None);
        assert!(view.stale().is_some());
    }

    #[test]
    fn incomplete_or_unactionable_comparisons_label_groups_unavailable() {
        let mut groups = vec![detail("a", "notes/one.md", "old", "new")];
        let mut unavailable = view(std::mem::take(&mut groups));
        unavailable.incomplete = Some("the detail pages returned 1 of 2 groups".to_string());
        assert_eq!(unavailable.state_of(0), GroupState::Unavailable);
        assert!(
            matches!(
                unavailable.toggle(Decision::Restore),
                ToggleOutcome::Refused(_)
            ),
            "no decision may exist for a group the view never saw complete",
        );
        let refused = column(&mut unavailable);
        assert!(
            refused.contains("Group 1 of 1 · Unavailable"),
            "the label names the state in words",
        );
        assert!(
            refused.contains("Decisions unavailable: the detail pages returned 1 of 2 groups"),
            "the reason stays stated",
        );

        let mut unactionable = view(vec![detail("a", "notes/one.md", "old", "new")]);
        unactionable.actionable = false;
        assert_eq!(unactionable.state_of(0), GroupState::Unavailable);
    }

    #[test]
    fn counts_cover_decisions_and_unvisited_scope() {
        let mut view = view(vec![
            detail("a", "notes/one.md", "old one", "new one"),
            detail("b", "notes/one.md", "old two", "new two"),
            detail("c", "notes/two.md", "old three", "new three"),
        ]);
        // Nothing visited: the whole scope is unvisited.
        let counts = view.counts();
        assert_eq!(counts.groups, 3);
        assert_eq!(counts.documents, 2);
        assert_eq!(counts.undecided, 3);
        assert_eq!(counts.unvisited, 3);
        assert_eq!(counts.unvisited_documents, 2);

        // Visiting the first group covers one document.
        view.visit_selection();
        assert_eq!(view.counts().unvisited, 2);
        assert_eq!(view.counts().unvisited_documents, 1);

        // Deciding the visited group leaves the counts honest; the other
        // key replaces the decision instead of adding one.
        assert_eq!(view.toggle(Decision::Keep), ToggleOutcome::Recorded);
        assert_eq!(view.toggle(Decision::Restore), ToggleOutcome::Recorded);
        let counts = view.counts();
        assert_eq!(counts.kept, 0);
        assert_eq!(counts.restored, 1);
        assert_eq!(counts.undecided, 2);
        assert_eq!(counts.unvisited, 2);
        assert_eq!(counts.unvisited_documents, 1);
    }

    #[test]
    fn the_column_renders_every_group_with_its_context() {
        let groups: Vec<ChangeDetail> = (0..10)
            .map(|index| {
                detail(
                    &format!("change{index}"),
                    &format!("notes/{index}.md"),
                    &format!("old passage {index}"),
                    &format!("new passage {index}"),
                )
            })
            .collect();
        let mut view = view(groups);
        let column = column(&mut view);
        for index in 0..10 {
            assert!(
                column.contains(&format!("Group {} of 10 · Undecided", index + 1)),
                "group {index} carries its header",
            );
            assert!(
                column.contains(&format!("old passage {index}")),
                "group {index} carries its before passage",
            );
            assert!(
                column.contains(&format!("new passage {index}")),
                "group {index} carries its after passage",
            );
            assert!(
                column.contains(&format!("notes/{index}.md")),
                "group {index} names its document",
            );
        }
        assert!(
            column.contains("r Restore previous passage"),
            "the action wording"
        );
        assert!(column.contains("10 passage groups"), "the scope totals");
    }

    #[test]
    fn the_source_view_states_exact_coordinates_and_bytes() {
        let mut view = view(vec![detail(
            "a",
            "notes/one.md",
            "old passage",
            "new passage",
        )]);
        view.toggle_source();
        let source = column(&mut view);
        assert!(source.contains("@@ -3,1 +3,1 @@"), "the exact coordinates");
        assert!(source.contains("-old passage"), "the removed line");
        assert!(source.contains("+new passage"), "the added line");
        view.toggle_source();
        let passage = column(&mut view);
        assert!(
            !passage.contains("@@ -3,1 +3,1 @@"),
            "the passage view returns",
        );
        assert!(passage.contains("old passage"), "the passage still reads");
    }

    #[test]
    fn bounded_excerpts_state_what_they_hid() {
        let mut view = view(vec![detail("a", "notes/one.md", "old", "new")]);
        let group = view.groups.get_mut(0).expect("the group");
        group.presentation.before.truncated = true;
        group.presentation.before.total_bytes = 200_000;
        group.presentation.word_hints_omitted = 3;
        let column = column(&mut view);
        assert!(
            column.contains("the before excerpt is bounded"),
            "the bound is stated",
        );
        assert!(
            column.contains("3 word hints are omitted by the detail bound"),
            "the omitted hints are stated",
        );
    }

    #[test]
    fn selection_moves_between_groups_and_follows_scrolling() {
        let mut view = view(vec![
            detail("a", "notes/one.md", "old one", "new one"),
            detail("b", "notes/two.md", "old two", "new two"),
            detail("c", "notes/three.md", "old three", "new three"),
        ]);
        view.prepare(76, 40);
        assert_eq!(view.selection(), 0);
        assert!(view.step_selection(1));
        assert_eq!(view.selection(), 1);
        assert!(view.step_selection(-1));
        assert_eq!(view.selection(), 0);

        // Scrolling selects the group the top visible row shows.
        view.step_selection(1);
        view.step_selection(1);
        assert_eq!(view.selection(), 2);
        view.scroll_by(-(view.offset() as i32));
        assert_eq!(view.selection(), 0, "the top row selects its group");

        // The ends hold.
        for _ in 0..5 {
            view.step_selection(1);
        }
        assert_eq!(view.selection(), 2);
        assert!(!view.step_selection(1), "the last group holds");
    }

    #[test]
    fn opaque_sides_render_as_bounded_hexadecimal() {
        let mut view = view(vec![detail("a", "notes/data.md", "old", "new")]);
        let group = view.groups.get_mut(0).expect("the group");
        group.opaque = true;
        group.presentation.after = BoundedExcerpt {
            first_line: 1,
            last_line: 1,
            total_bytes: 4,
            truncated: false,
            text: None,
            hex: Some("fffe0001".to_string()),
        };
        let column = column(&mut view);
        assert!(
            column.contains("fffe0001 (4 bytes, not valid UTF-8)"),
            "the exact leading bytes with the true length",
        );
        assert!(column.contains("opaque bytes"), "the state is labeled");
    }
}
