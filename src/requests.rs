//! Durable requests. The socket is a notification channel, not the data store.
use crate::{
    app::{OperationError, Operations},
    editing::{self, MAX_SOURCE_BYTES, Version},
};
use serde::{Deserialize, Serialize};
use std::{fs, ops::Range};
const MAX_RECORD: usize = 25 * MAX_SOURCE_BYTES;
const MAX_QUEUE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_REQUESTS: usize = 64;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Queued,
    Claimed,
    ResultReady,
    Applying,
    Applied,
    Undoing,
    Undone,
    Stale,
    Failed,
    Cancelled,
}
impl State {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Queued => "Waiting for agent",
            Self::Claimed => "Agent working",
            Self::ResultReady => "Ready to apply",
            Self::Applying => "Checking an interrupted edit",
            Self::Applied => "Applied",
            Self::Undoing => "Checking an interrupted undo",
            Self::Undone => "Undone",
            Self::Stale => "Needs comparison",
            Self::Failed => "Could not complete",
            Self::Cancelled => "Cancelled; your text is kept",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delivery {
    pub session: String,
    pub version: String,
    pub emitted_at: u64,
}

pub struct Feedback {
    pub text: Option<String>,
    pub entries: Vec<(String, String)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default)]
    pub deliveries: Vec<Delivery>,
    #[serde(default)]
    pub delivery_generation: u64,
    pub schema_version: u32,
    pub id: String,
    pub created_at: u64,
    pub desk: Option<String>,
    pub base: Version,
    pub instruction: String,
    pub start: usize,
    pub end: usize,
    pub state: State,
    pub owner: Option<String>,
    pub result: Option<String>,
    pub explanation: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub note_commit: Option<String>,
    #[serde(default)]
    pub applied_base: Option<Version>,
    #[serde(default)]
    pub applied_version: Option<Version>,
}
impl Request {
    pub fn delivery_version(&self) -> String {
        // The generation distinguishes a released claim from its earlier queue entry.
        format!(
            "{}:{}:{}:{}:{}:{}",
            self.base.digest,
            self.start,
            self.end,
            self.delivery_generation,
            self.state.label(),
            self.desk.as_deref().unwrap_or("")
        )
    }
    pub fn delivery_status(&self) -> &'static str {
        if !matches!(self.state, State::Queued | State::Cancelled | State::Undone) {
            return "not_applicable";
        }
        let version = self.delivery_version();
        if self.deliveries.iter().any(|d| d.version == version) {
            "delivered"
        } else {
            "waiting_for_delivery"
        }
    }
    pub fn display_state(&self) -> &'static str {
        if self.state == State::Queued {
            if self.delivery_status() == "delivered" {
                "Delivered; waiting for agent"
            } else {
                "Waiting for delivery"
            }
        } else {
            self.state.label()
        }
    }
}

fn invalid(message: &str) -> OperationError {
    OperationError::new("INVALID_ARGUMENT", message)
}
fn validate_id(id: &str) -> Result<(), OperationError> {
    if id.is_empty()
        || id.len() > 100
        || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(invalid("invalid request ID"));
    }
    Ok(())
}
impl Operations<'_> {
    pub fn create_edit_request(
        &self,
        base: Version,
        selection: Option<Range<usize>>,
        instruction: &str,
    ) -> Result<Request, OperationError> {
        if instruction.trim().is_empty() || instruction.len() > 8000 {
            return Err(invalid("instruction must contain 1 to 8000 bytes"));
        }
        let _guard = self.acquire_mutation_lock()?;
        if self.edit_document(&base.path)? != base {
            return Err(editing::stale(
                "request source changed; reopen it before submitting",
            ));
        }
        let dir = self.editing_metadata("requests")?;
        if fs::read_dir(dir).map_err(editing::io)?.count() >= MAX_REQUESTS {
            return Err(OperationError::new(
                "LIMIT_EXCEEDED",
                "64 requests retained; inspect and remove completed records before submitting",
            ));
        }
        let span = selection.unwrap_or(0..base.text.len());
        if span.start > span.end
            || !base.text.is_char_boundary(span.start)
            || !base.text.is_char_boundary(span.end)
        {
            return Err(invalid("invalid source span"));
        }
        let request = Request {
            deliveries: vec![],
            delivery_generation: 0,
            schema_version: 1,
            id: editing::id(),
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            desk: std::env::var("MARGIN_DESK_SOCKET").ok(),
            base,
            instruction: instruction.into(),
            start: span.start,
            end: span.end,
            state: State::Queued,
            owner: None,
            result: None,
            explanation: None,
            error: None,
            note_commit: None,
            applied_base: None,
            applied_version: None,
        };
        self.store_edit_request(&request)?;
        Ok(request)
    }
    fn store_edit_request(&self, request: &Request) -> Result<(), OperationError> {
        validate_id(&request.id)?;
        let path = self
            .editing_metadata("requests")?
            .join(format!("{}.json", request.id));
        let bytes = serde_json::to_vec(request).map_err(editing::io)?;
        let mut total = bytes.len() as u64;
        for entry in fs::read_dir(path.parent().unwrap())
            .map_err(editing::io)?
            .take(MAX_REQUESTS + 2)
        {
            let entry = entry.map_err(editing::io)?;
            if entry.path() != path {
                total += entry.metadata().map_err(editing::io)?.len();
            }
        }
        if total > MAX_QUEUE_BYTES {
            return Err(OperationError::new(
                "LIMIT_EXCEEDED",
                "request storage exceeds 32 MiB; remove completed records",
            ));
        }
        editing::atomic_write(&path, &bytes, None)
    }
    pub fn inspect_edit_request(&self, id: &str) -> Result<Request, OperationError> {
        validate_id(id)?;
        let directory = self
            .git()
            .git_path("margin-editing")
            .map_err(OperationError::from)?
            .join("requests");
        if !directory.try_exists().map_err(editing::io)? {
            return Err(invalid("request does not exist"));
        }
        let path = self
            .editing_metadata("requests")?
            .join(format!("{id}.json"));
        if fs::symlink_metadata(&path)
            .map_err(editing::io)?
            .file_type()
            .is_symlink()
        {
            return Err(invalid("request record is a symbolic link"));
        }
        let request: Request =
            serde_json::from_slice(&editing::read(&path, MAX_RECORD)?).map_err(editing::io)?;
        if request.deliveries.len() > 32
            || request
                .deliveries
                .iter()
                .any(|d| d.session.is_empty() || d.session.len() > 200 || d.version.len() > 8192)
            || request
                .applied_base
                .as_ref()
                .is_some_and(|v| v.text.len() > MAX_SOURCE_BYTES)
            || request
                .applied_version
                .as_ref()
                .is_some_and(|v| v.text.len() > MAX_SOURCE_BYTES)
            || request.base.text.len() > MAX_SOURCE_BYTES
            || request.instruction.len() > 8000
            || request
                .result
                .as_ref()
                .is_some_and(|s| s.len() > MAX_SOURCE_BYTES)
            || request.explanation.as_ref().is_some_and(|s| s.len() > 8000)
            || request.start > request.end
            || !request.base.text.is_char_boundary(request.start)
            || !request.base.text.is_char_boundary(request.end)
        {
            return Err(invalid(
                "request fields exceed their limits or contain an invalid source span",
            ));
        }
        if request.schema_version != 1 || request.id != id {
            return Err(invalid("unsupported or mismatched request record"));
        }
        Ok(request)
    }
    pub fn list_edit_requests(&self) -> Result<Vec<Request>, OperationError> {
        let mut requests = Vec::new();
        let path = self
            .git()
            .git_path("margin-editing")
            .map_err(OperationError::from)?
            .join("requests");
        if !path.try_exists().map_err(editing::io)? {
            return Ok(requests);
        }
        // Validate private directories before reading an existing queue.
        let path = self.editing_metadata("requests")?;
        let mut total = 0;
        for entry in fs::read_dir(path)
            .map_err(editing::io)?
            .take(MAX_REQUESTS + 1)
        {
            let entry = entry.map_err(editing::io)?;
            total += entry.metadata().map_err(editing::io)?.len();
            if total > MAX_QUEUE_BYTES {
                return Err(OperationError::new(
                    "LIMIT_EXCEEDED",
                    "request storage exceeds 32 MiB",
                ));
            }
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|s| s.strip_suffix(".json")) else {
                continue;
            };
            requests.push(self.inspect_edit_request(id)?);
        }
        if requests.len() > MAX_REQUESTS {
            return Err(OperationError::new(
                "LIMIT_EXCEEDED",
                "request queue exceeds 64 records",
            ));
        }
        requests.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        Ok(requests)
    }
    pub fn claim_edit_request(&self, id: &str, owner: &str) -> Result<Request, OperationError> {
        if owner.trim().is_empty() || owner.len() > 200 {
            return Err(invalid("owner must contain 1 to 200 bytes"));
        }
        let _guard = self.acquire_mutation_lock()?;
        let mut r = self.inspect_edit_request(id)?;
        if r.state == State::Claimed && r.owner.as_deref() == Some(owner) {
            return Ok(r);
        }
        if r.state != State::Queued {
            return Err(editing::stale(
                "request is not queued; inspect its owner and state",
            ));
        }
        r.state = State::Claimed;
        r.owner = Some(owner.into());
        self.store_edit_request(&r)?;
        Ok(r)
    }
    pub fn submit_edit_result(
        &self,
        id: &str,
        owner: &str,
        replacement: &str,
        explanation: &str,
    ) -> Result<Request, OperationError> {
        if replacement.len() > MAX_SOURCE_BYTES
            || explanation.len() > 8000
            || explanation.trim().is_empty()
        {
            return Err(invalid(
                "result must fit 1 MiB; explanation must contain 1 to 8000 bytes",
            ));
        }
        let _guard = self.acquire_mutation_lock()?;
        let mut r = self.inspect_edit_request(id)?;
        if r.owner.as_deref() != Some(owner) {
            return Err(editing::stale("request belongs to another owner"));
        }
        if matches!(
            r.state,
            State::ResultReady | State::Applying | State::Applied | State::Stale
        ) && r.result.as_deref() == Some(replacement)
            && r.explanation.as_deref() == Some(explanation)
        {
            return Ok(r);
        }
        if r.state != State::Claimed {
            return Err(editing::stale("request is not claimed; inspect its state"));
        }
        if r.start > r.end
            || !r.base.text.is_char_boundary(r.start)
            || !r.base.text.is_char_boundary(r.end)
        {
            return Err(invalid("request span is invalid"));
        }
        if r.base.text.len() - (r.end - r.start) + replacement.len() > MAX_SOURCE_BYTES {
            return Err(OperationError::new(
                "LIMIT_EXCEEDED",
                "resulting source exceeds 1 MiB",
            ));
        }
        r.result = Some(replacement.into());
        r.explanation = Some(explanation.into());
        r.state = State::ResultReady;
        self.store_edit_request(&r)?;
        drop(_guard);
        if let Some(desk) = &r.desk {
            let _ =
                crate::desk::control_at(std::path::Path::new(desk), crate::desk::Control::Notify);
        }
        Ok(r)
    }
    pub fn cancel_edit_request(&self, id: &str) -> Result<Request, OperationError> {
        let _guard = self.acquire_mutation_lock()?;
        let mut r = self.inspect_edit_request(id)?;
        if matches!(
            r.state,
            State::Applied | State::Applying | State::Undoing | State::Undone
        ) {
            return Err(editing::stale(
                "applied or uncertain request cannot be cancelled; inspect the document",
            ));
        }
        r.state = State::Cancelled;
        self.store_edit_request(&r)?;
        Ok(r)
    }
    /// Apply only from the viewer when no buffer or confirmation stands.
    pub fn apply_edit_result(&self, id: &str) -> Result<Request, OperationError> {
        self.apply_edit_result_choice(id, None)
    }
    /// A writer may choose the agent proposal only against the version shown.
    pub fn choose_agent_result(
        &self,
        id: &str,
        compared: &Version,
    ) -> Result<Request, OperationError> {
        self.apply_edit_result_choice(id, Some(compared))
    }
    fn apply_edit_result_choice(
        &self,
        id: &str,
        compared: Option<&Version>,
    ) -> Result<Request, OperationError> {
        let _guard = self.acquire_mutation_lock()?;
        let mut r = self.inspect_edit_request(id)?;
        if r.state == State::Applied {
            return Ok(r);
        }
        if r.state == State::Applying {
            r.error = Some("Interrupted application; inspect recovery records and disk. It will not run again automatically.".into());
            self.store_edit_request(&r)?;
            return Ok(r);
        }
        if r.state != State::ResultReady && !(compared.is_some() && r.state == State::Stale) {
            return Err(editing::stale("request has no pending result"));
        }
        let observed = self.edit_document(&r.base.path)?;
        if compared.is_some_and(|version| *version != observed) {
            return Err(editing::stale(
                "the file changed since comparison; compare again before choosing the agent version",
            ));
        }
        let span = if observed.same_context(&r.base) {
            rebase_span(&r.base.text, &observed.text, r.start..r.end).or_else(|| {
                compared.and_then(|_| choose_span(&r.base.text, &observed.text, r.start..r.end))
            })
        } else {
            None
        };
        let Some(span) = span else {
            r.state = State::Stale;
            r.error = Some("Your edits overlap the requested passage or its context changed. Both versions are retained; compare them before choosing.".into());
            self.store_edit_request(&r)?;
            return Ok(r);
        };
        let mut text = observed.text.clone();
        text.replace_range(
            span,
            r.result
                .as_deref()
                .ok_or_else(|| invalid("missing result"))?,
        );
        if text.len() > MAX_SOURCE_BYTES {
            return Err(OperationError::new(
                "LIMIT_EXCEEDED",
                "combined result exceeds 1 MiB",
            ));
        }
        let mut applied = observed.clone();
        applied.text = text.clone();
        applied.digest = self
            .git()
            .hash_object(text.as_bytes())
            .map_err(OperationError::from)?;
        r.applied_base = Some(observed.clone());
        r.applied_version = Some(applied);
        r.state = State::Applying;
        self.store_edit_request(&r)?;
        match self.save_document_locked(&observed, &text) {
            Ok(_) => {
                r.state = State::Applied;
                r.error = None;
            }
            Err(e) => {
                r.state = if e.code == "OUTCOME_UNKNOWN" {
                    State::Applying
                } else if e.code == "STALE_STATE" {
                    State::Stale
                } else {
                    State::Failed
                };
                r.error = Some(e.message);
            }
        }
        self.store_edit_request(&r)?;
        Ok(r)
    }
    pub fn undo_edit_result(&self, id: &str) -> Result<Request, OperationError> {
        let _guard = self.acquire_mutation_lock()?;
        let mut r = self.inspect_edit_request(id)?;
        if r.state == State::Undone {
            return Ok(r);
        }
        if r.state != State::Applied {
            return Err(editing::stale("only an applied agent edit can be undone"));
        }
        let before = r
            .applied_base
            .clone()
            .ok_or_else(|| invalid("this older request has no undo snapshot"))?;
        let after = r
            .applied_version
            .clone()
            .ok_or_else(|| invalid("missing applied version"))?;
        if self.edit_document(&r.base.path)? != after {
            return Err(editing::stale(
                "the file changed after this agent edit; compare before restoring it",
            ));
        }
        r.state = State::Undoing;
        self.store_edit_request(&r)?;
        self.save_document_locked(&after, &before.text)?;
        r.state = State::Undone;
        r.error = Some("Writer undid this agent edit".into());
        self.store_edit_request(&r)?;
        Ok(r)
    }
    /// Attach explanations only when the checkpoint stores the exact agent result.
    pub(crate) fn attach_edit_request_notes(&self, commit: &str) -> Result<bool, OperationError> {
        let requests = self.list_edit_requests()?;
        let pending: Vec<_> = requests
            .into_iter()
            .filter(|r| r.state == State::Applied && r.note_commit.is_none())
            .collect();
        if pending.is_empty() {
            return Ok(false);
        }
        let tree = self.git().ls_tree(commit).map_err(OperationError::from)?;
        let mut changed = false;
        for mut request in pending {
            let current = match self.edit_document(&request.base.path) {
                Ok(current) => current,
                Err(_) => continue,
            };
            if (
                current.scope,
                current.adoption,
                current.branch,
                current.worktree,
            ) != (
                request.base.scope.clone(),
                request.base.adoption.clone(),
                request.base.branch.clone(),
                request.base.worktree.clone(),
            ) {
                continue;
            }
            let Some(entry) = tree
                .iter()
                .find(|e| e.path == std::ffi::OsStr::new(&request.base.path) && e.kind == "blob")
            else {
                continue;
            };
            let expected = if let Some(applied) = &request.applied_version {
                applied.text.clone()
            } else {
                let mut expected = request.base.text.clone();
                if request.start > request.end
                    || !expected.is_char_boundary(request.start)
                    || !expected.is_char_boundary(request.end)
                {
                    return Err(invalid("invalid request span"));
                }
                expected.replace_range(
                    request.start..request.end,
                    request
                        .result
                        .as_deref()
                        .ok_or_else(|| invalid("missing applied result"))?,
                );
                expected
            };
            if self
                .git()
                .hash_object(expected.as_bytes())
                .map_err(OperationError::from)?
                != entry.oid
            {
                continue;
            }
            let Some(explanation) = &request.explanation else {
                continue;
            };
            let text: String = format!("Request {}: {}", request.id, explanation)
                .chars()
                .take(crate::notes::MAX_TEXT_CHARS)
                .collect();
            let notes = self.list_notes(Some(commit));
            let existing = notes.data.as_ref().is_some_and(|n| {
                n.notes
                    .iter()
                    .any(|n| n.by == crate::notes::Author::Agent && n.text == text)
            });
            if !existing {
                let result = self.append_note(&crate::notes::AppendRequest {
                    commit: Some(commit.into()),
                    group: None,
                    by: crate::notes::Author::Agent,
                    text,
                });
                if let Some(e) = result.error {
                    return Err(e);
                }
                changed = true;
            }
            let _guard = self.acquire_mutation_lock()?;
            request = self.inspect_edit_request(&request.id)?;
            request.note_commit = Some(commit.into());
            self.store_edit_request(&request)?;
        }
        Ok(changed)
    }
    /// Resolve interrupted intent from observed bytes without repeating the write.
    pub fn reconcile_edit_request(&self, id: &str) -> Result<Request, OperationError> {
        let _guard = self.acquire_mutation_lock()?;
        let mut request = self.inspect_edit_request(id)?;
        if request.state == State::Undoing {
            let observed = self.edit_document(&request.base.path)?;
            request.state = if request.applied_base.as_ref() == Some(&observed) {
                State::Undone
            } else if request.applied_version.as_ref() == Some(&observed) {
                State::Applied
            } else {
                State::Stale
            };
            request.error = Some("Interrupted undo reconciled without repeating a write".into());
            self.store_edit_request(&request)?;
            return Ok(request);
        }
        if request.state != State::Applying {
            return Err(editing::stale(
                "only an interrupted application needs reconciliation",
            ));
        }
        let observed = self.edit_document(&request.base.path)?;
        let mut expected = request.base.text.clone();
        if request.start > request.end
            || !expected.is_char_boundary(request.start)
            || !expected.is_char_boundary(request.end)
        {
            return Err(invalid("invalid request span"));
        }
        expected.replace_range(
            request.start..request.end,
            request
                .result
                .as_deref()
                .ok_or_else(|| invalid("missing result"))?,
        );
        let mut expected_version = request
            .applied_version
            .clone()
            .unwrap_or_else(|| request.base.clone());
        if request.applied_version.is_none() {
            expected_version.text = expected;
        }
        expected_version.digest = self
            .git()
            .hash_object(expected_version.text.as_bytes())
            .map_err(OperationError::from)?;
        if observed == expected_version {
            request.state = State::Applied;
            request.error = None;
        } else if observed
            == request
                .applied_base
                .clone()
                .unwrap_or_else(|| request.base.clone())
        {
            request.state = State::Failed;
            request.error = Some(
                "Interrupted application left the captured file unchanged; no write was repeated"
                    .into(),
            );
        } else {
            request.state = State::Stale;
            request.error = Some("Disk differs from both captured and proposed versions; inspect retained recovery data".into());
        }
        self.store_edit_request(&request)?;
        Ok(request)
    }
    pub fn resume_edit_request(&self, id: &str) -> Result<Request, OperationError> {
        let desk = std::env::var("MARGIN_DESK_SOCKET")
            .map_err(|_| invalid("resume requires the new desk environment"))?;
        let _guard = self.acquire_mutation_lock()?;
        let mut r = self.inspect_edit_request(id)?;
        if !matches!(r.state, State::Queued | State::Claimed | State::ResultReady) {
            return Err(editing::stale(
                "only pending requests can move to another desk",
            ));
        }
        r.desk = Some(desk);
        self.store_edit_request(&r)?;
        Ok(r)
    }
    pub fn release_edit_request(&self, id: &str, owner: &str) -> Result<Request, OperationError> {
        let _guard = self.acquire_mutation_lock()?;
        let mut r = self.inspect_edit_request(id)?;
        if r.state != State::Claimed || r.owner.as_deref() != Some(owner) {
            return Err(editing::stale(
                "release requires the current claim owner; inspect before reclaiming interrupted work",
            ));
        }
        r.state = State::Queued;
        r.delivery_generation = r
            .delivery_generation
            .checked_add(1)
            .ok_or_else(|| invalid("delivery generation exhausted"))?;
        r.owner = None;
        self.store_edit_request(&r)?;
        Ok(r)
    }
    pub fn remove_edit_request(&self, id: &str) -> Result<(), OperationError> {
        let _guard = self.acquire_mutation_lock()?;
        let r = self.inspect_edit_request(id)?;
        if !matches!(
            r.state,
            State::Applied | State::Undone | State::Cancelled | State::Failed | State::Stale
        ) {
            return Err(editing::stale("only finished requests can be removed"));
        }
        fs::remove_file(
            self.editing_metadata("requests")?
                .join(format!("{id}.json")),
        )
        .map_err(editing::io)
    }
    pub fn edit_request_feedback(&self) -> Result<Option<String>, OperationError> {
        Ok(self.prepare_request_feedback(None)?.text)
    }

    /// Select unseen versions before applying batch limits, so later requests progress.
    pub fn prepare_request_feedback(
        &self,
        session: Option<&str>,
    ) -> Result<Feedback, OperationError> {
        let requests = self.list_edit_requests()?;
        let desk = std::env::var("MARGIN_DESK_SOCKET").ok();
        let eligible = |r: &&Request| {
            (r.desk.is_none() || r.desk == desk)
                && !session.is_some_and(|session| {
                    r.deliveries
                        .iter()
                        .any(|d| d.session == session && d.version == r.delivery_version())
                })
        };
        let decisions: Vec<_> = requests
            .iter()
            .rev()
            .filter(eligible)
            .filter(|r| matches!(r.state, State::Cancelled | State::Undone))
            .take(2)
            .collect();
        let queued: Vec<_> = requests
            .iter()
            .filter(eligible)
            .filter(|r| r.state == State::Queued)
            .take(4)
            .collect();
        let entries = queued
            .iter()
            .chain(decisions.iter())
            .map(|r| (r.id.clone(), r.delivery_version()))
            .collect();
        if queued.is_empty() && decisions.is_empty() {
            return Ok(Feedback {
                text: None,
                entries,
            });
        }
        let queued_count = requests.iter().filter(|r| r.state == State::Queued).count();
        let queued: Vec<_> = queued.into_iter().map(|r| serde_json::json!({"id":r.id,"path":r.base.path.chars().take(200).collect::<String>(),"instruction":r.instruction.chars().take(80).collect::<String>()})).collect();
        let decisions: Vec<_> = decisions.into_iter().map(|r| serde_json::json!({"id":r.id,"writer_action":r.state,"path":r.base.path.chars().take(60).collect::<String>()})).collect();
        Ok(Feedback {
            text: Some(format!(
                "Margin request update ({queued_count} queued in repository). Inspect with `margin requests inspect ID --json`, claim with `margin requests claim ID --owner SESSION --json`, then submit a UTF-8 replacement file with `margin requests submit ID --owner SESSION --input FILE --explanation TEXT --json`. Replace only the captured source span. Do not write the document directly. Requests: {}. Recent writer decisions (do not repeat cancelled or undone work): {}. Use `margin requests list --json` for the complete queue.",
                serde_json::to_string(&queued).map_err(editing::io)?,
                serde_json::to_string(&decisions).map_err(editing::io)?
            )),
            entries,
        })
    }

    /// Explicit recovery for a lost hook response; claims remain untouched.
    pub fn redeliver_edit_request(
        &self,
        id: &str,
        session: &str,
    ) -> Result<Request, OperationError> {
        if session.is_empty() || session.len() > 200 {
            return Err(invalid("invalid delivery session"));
        }
        let _guard = self.acquire_mutation_lock()?;
        let mut r = self.inspect_edit_request(id)?;
        let previous = r.deliveries.len();
        r.deliveries.retain(|d| d.session != session);
        if previous != r.deliveries.len() {
            self.store_edit_request(&r)?;
        }
        Ok(r)
    }

    /// Record emitted versions without changing request ownership or lifecycle state.
    pub fn record_request_delivery(
        &self,
        session: &str,
        entries: &[(String, String)],
    ) -> Result<(), OperationError> {
        if session.is_empty() || session.len() > 200 {
            return Err(invalid("invalid delivery session"));
        }
        if entries.is_empty() {
            return Ok(());
        }
        let _guard = self.acquire_mutation_lock()?;
        for (id, version) in entries {
            let mut r = self.inspect_edit_request(id)?;
            if r.deliveries
                .iter()
                .any(|d| d.session == session && d.version == *version)
            {
                continue;
            }
            if r.delivery_version() != *version {
                continue;
            }
            r.deliveries.retain(|d| d.session != session);
            if r.deliveries.len() >= 32 {
                r.deliveries.remove(0);
            }
            r.deliveries.push(Delivery {
                session: session.into(),
                version: version.clone(),
                emitted_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            });
            self.store_edit_request(&r)?;
        }
        Ok(())
    }
}

/// Rebase one requested span across one conservative, disjoint writer edit.
/// Multiple separated edits may enlarge the changed interval and refuse safely.
fn rebase_span(
    base: &str,
    current: &str,
    span: std::ops::Range<usize>,
) -> Option<std::ops::Range<usize>> {
    if span.start > span.end
        || !base.is_char_boundary(span.start)
        || !base.is_char_boundary(span.end)
    {
        return None;
    }
    if base == current {
        return Some(span);
    }
    if span == (0..base.len()) {
        return None;
    }
    let mut prefix = 0;
    for (old, new) in base.chars().zip(current.chars()) {
        if old != new {
            break;
        }
        prefix += old.len_utf8();
    }
    let mut suffix = 0;
    for (old, new) in base[prefix..]
        .chars()
        .rev()
        .zip(current[prefix..].chars().rev())
    {
        if old != new {
            break;
        }
        suffix += old.len_utf8();
    }
    let old_end = base.len() - suffix;
    let new_end = current.len() - suffix;
    if old_end <= span.start {
        Some((new_end + span.start - old_end)..(new_end + span.end - old_end))
    } else if prefix >= span.end {
        Some(span)
    } else {
        None
    }
}

fn choose_span(
    base: &str,
    current: &str,
    span: std::ops::Range<usize>,
) -> Option<std::ops::Range<usize>> {
    if span == (0..base.len()) {
        return Some(0..current.len());
    }
    let mut prefix = 0;
    for (a, b) in base.chars().zip(current.chars()) {
        if a != b {
            break;
        }
        prefix += a.len_utf8();
    }
    let mut suffix = 0;
    for (a, b) in base[prefix..]
        .chars()
        .rev()
        .zip(current[prefix..].chars().rev())
    {
        if a != b {
            break;
        }
        suffix += a.len_utf8();
    }
    let old_end = base.len() - suffix;
    let new_end = current.len() - suffix;
    if prefix >= span.start && old_end <= span.end {
        Some(span.start..(new_end + span.end - old_end))
    } else {
        None
    }
}
