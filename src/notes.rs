//! Durable annotations on Git commits (SPEC 12.7).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::app::{
    DiffRequest, Effects, HistoryWalk, InspectRequest, MarkerError, ObservedEffect, OperationError,
    OperationOutcome, Operations, PolicyBlocker,
};
use crate::domain::{CommitKind, is_fingerprint_id};
use crate::git::{Git, Mode};

pub const NOTES_REF: &str = "refs/notes/margin";
pub const SCHEMA: &str = include_str!("../schemas/notes-v1.json");
// Includes worst-case JSON escaping for 50 entries of 2000 Unicode scalars.
pub const MAX_ENVELOPE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_TEXT_CHARS: usize = 2000;
pub const MAX_ENTRIES: usize = 50;
pub const MAX_CONTEXT_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Author {
    Agent,
    Writer,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Note {
    pub id: String,
    pub by: Author,
    #[serde(
        default,
        deserialize_with = "present_group",
        skip_serializing_if = "Option::is_none"
    )]
    pub group: Option<String>,
    pub text: String,
}

fn present_group<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

impl Note {
    pub fn validate(&self) -> Result<(), OperationError> {
        if self.id.is_empty()
            || self.id.len() > 64
            || !self
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
            || self.text.is_empty()
            || self.text.chars().count() > MAX_TEXT_CHARS
            || self.group.as_ref().is_some_and(|g| !is_fingerprint_id(g))
        {
            return Err(invalid("invalid note ID, group digest, or text length"));
        }
        Ok(())
    }

    pub fn label(&self) -> &'static str {
        match self.by {
            Author::Agent => "Note (agent)",
            Author::Writer => "Your note",
        }
    }

    /// Use the same terminal protection as the document presentation.
    pub fn display_text(&self) -> String {
        crate::cli::display_escape(&self.text.replace('\t', "    "))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub schema_version: u32,
    pub notes: Vec<Note>,
}

impl Envelope {
    pub fn parse(bytes: &[u8]) -> Result<Self, OperationError> {
        if bytes.len() > MAX_ENVELOPE_BYTES {
            return Err(invalid("the notes envelope exceeds the byte bound"));
        }
        let envelope: Self = serde_json::from_slice(bytes)
            .map_err(|_| invalid("the notes envelope has invalid JSON, fields, or authorship"))?;
        if envelope.schema_version != 1
            || envelope.notes.is_empty()
            || envelope.notes.len() > MAX_ENTRIES
        {
            return Err(invalid("unsupported notes version or entry count"));
        }
        let mut ids = BTreeSet::new();
        for note in &envelope.notes {
            note.validate()?;
            if !ids.insert(&note.id) {
                return Err(invalid("note IDs must be unique within each commit"));
            }
        }
        Ok(envelope)
    }
}

fn invalid(reason: &str) -> OperationError {
    OperationError::new("INVALID_NOTES", reason)
}

/// A snapshot of one namespace. Commit lookup is independent of tree fanout.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub head: Option<String>,
    blobs: BTreeMap<String, String>,
}

impl Snapshot {
    pub fn ref_head(git: &Git) -> Result<Option<String>, OperationError> {
        let output = git.run(Mode::Query, ["rev-parse", "--verify", "--quiet", NOTES_REF])?;
        if output.exit_code == Some(1) {
            return Ok(None);
        }
        if !output.success() {
            return Err(OperationError::new(
                "GIT_FAILED",
                "the notes ref cannot be resolved",
            ));
        }
        let head = String::from_utf8(output.stdout)
            .map_err(|_| invalid("invalid notes ref OID"))?
            .trim()
            .to_string();
        if !crate::domain::is_full_oid(&head) {
            return Err(invalid("invalid notes ref OID"));
        }
        Ok(Some(head))
    }

    pub fn load(git: &Git) -> Result<Self, OperationError> {
        let Some(head) = Self::ref_head(git)? else {
            return Ok(Self::default());
        };
        let mut blobs = BTreeMap::new();
        for entry in git.ls_tree(&head)? {
            let commit = entry.path.to_string_lossy().replace('/', "");
            if crate::domain::is_full_oid(&commit)
                && entry.kind == "blob"
                && blobs.insert(commit, entry.oid).is_some()
            {
                return Err(invalid(
                    "the notes tree contains doubled commit annotations",
                ));
            }
        }
        Ok(Self {
            head: Some(head),
            blobs,
        })
    }

    pub fn read(&self, git: &Git, commit: &str) -> CommitNotes {
        let mut result = CommitNotes {
            commit: commit.to_string(),
            notes: Vec::new(),
            unreadable: None,
        };
        if let Some(blob) = self.blobs.get(commit) {
            // Bound raw bytes before parsing, including malformed native notes.
            let bounded = git.clone().with_output_limit(MAX_ENVELOPE_BYTES);
            match bounded
                .read_blob(blob)
                .map_err(OperationError::from)
                .and_then(|b| Envelope::parse(&b))
            {
                Ok(envelope) => result.notes = envelope.notes,
                Err(error) => {
                    result.unreadable = Some(format!(
                        "Note unreadable: {}",
                        crate::cli::bounded_message(&error.message)
                    ))
                }
            }
        }
        result
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CommitNotes {
    pub commit: String,
    pub notes: Vec<Note>,
    pub unreadable: Option<String>,
}

#[derive(Clone, Debug)]
pub struct AppendRequest {
    /// TUI writes to its pinned candidate, even when HEAD has advanced.
    pub commit: Option<String>,
    pub group: Option<String>,
    pub by: Author,
    pub text: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct AppendOutcome {
    pub commit: String,
    pub note: Note,
    pub appended: bool,
    pub notes_ref: String,
}

fn unique_suffix() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{:x}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

impl Operations<'_> {
    pub fn list_notes(&self, commit: Option<&str>) -> OperationOutcome<CommitNotes> {
        let result = (|| {
            let commit = match commit {
                Some(revision) => self.git().resolve_commit(OsStr::new(revision))?,
                None => self
                    .git()
                    .head_commit()?
                    .ok_or_else(|| invalid("no candidate exists"))?,
            };
            Ok(match Snapshot::load(self.git()) {
                Ok(snapshot) => snapshot.read(self.git(), &commit),
                Err(error) => CommitNotes {
                    commit,
                    notes: Vec::new(),
                    unreadable: Some(format!(
                        "Note unreadable: {}",
                        crate::cli::bounded_message(&error.message)
                    )),
                },
            })
        })();
        match result {
            Ok(data) => OperationOutcome::success(data, Effects::unchanged()),
            Err(error) => OperationOutcome::failure(error, Effects::unchanged()),
        }
    }

    pub fn append_note(&self, request: &AppendRequest) -> OperationOutcome<AppendOutcome> {
        let mut effects = Effects::unchanged();
        let result = (|| {
            let note = Note {
                id: format!("note:{}", unique_suffix()),
                by: request.by.clone(),
                group: request.group.clone(),
                text: request.text.clone(),
            };
            note.validate()?;
            let _lock = self.acquire_mutation_lock()?;
            let inspection = self.inspect_workspace(&InspectRequest {
                scope: None,
                path_filter: Vec::new(),
            });
            if let Some(error) = inspection.error {
                return Err(error);
            }
            let data = inspection.data.expect("successful inspection");
            if let Some(blocker) = data.policy_blockers.iter().find(|blocker| {
                matches!(
                    blocker,
                    PolicyBlocker::BareRepository
                        | PolicyBlocker::NotInWorkTree
                        | PolicyBlocker::LinkedWorktree
                        | PolicyBlocker::SparseCheckout
                        | PolicyBlocker::ShallowClone
                        | PolicyBlocker::PartialClone { .. }
                        | PolicyBlocker::NestedRepository { .. }
                        | PolicyBlocker::ActiveMerge
                        | PolicyBlocker::ActiveRebase
                        | PolicyBlocker::ActiveCherryPick
                        | PolicyBlocker::ActiveRevert
                        | PolicyBlocker::ActiveBisect
                        | PolicyBlocker::ActiveSequencer
                )
            }) {
                return Err(OperationError::new(
                    blocker.public_code(),
                    blocker.to_string(),
                ));
            }
            if data
                .branch
                .as_deref()
                .and_then(|b| b.strip_prefix("refs/heads/"))
                != data.settings.branch.as_deref()
                || data.branch.is_none()
            {
                return Err(OperationError::new(
                    "BRANCH_CHANGED",
                    "notes require the adopted branch",
                ));
            }
            let head = data.head.ok_or_else(|| invalid("no candidate exists"))?;
            let commit = request.commit.clone().unwrap_or_else(|| head.clone());
            if !crate::domain::is_full_oid(&commit)
                || !adoption_range(self.git(), &head, &data.scope.digest)?.contains(&commit)
            {
                return Err(invalid(
                    "the note target is outside adopted first-parent history",
                ));
            }
            self.git().author_ident().map_err(|error| match error {
                crate::git::GitError::Exit { .. } => OperationError::new(
                    "MISSING_IDENTITY",
                    "set your Git author identity before writing a note",
                ),
                other => other.into(),
            })?;
            if let Some(group) = &note.group {
                // A pinned TUI group may now be orphaned, but must remain attached
                // to its original candidate. CLI writes validate the current group.
                if request.commit.is_none() {
                    let review = self.build_review(&DiffRequest {
                        scope: None,
                        path_filter: Vec::new(),
                        change: Some(group.clone()),
                        cursor: None,
                        limit: None,
                        working: false,
                    });
                    if let Some(error) = review.error {
                        return Err(error);
                    }
                    if review
                        .data
                        .as_ref()
                        .is_none_or(|r| r.candidate_oid != commit)
                    {
                        return Err(invalid("the comparison candidate changed"));
                    }
                }
            }
            append_atomic(self.git(), &commit, note, &mut effects)
        })();
        match result {
            Ok(data) => OperationOutcome::success(data, effects),
            Err(error) => OperationOutcome::failure(error, effects),
        }
    }

    /// Validated adoption range, reconstructed without TUI state or display filters.
    pub fn feedback_commits(&self) -> Result<Vec<String>, OperationError> {
        let inspection = self.inspect_workspace(&InspectRequest {
            scope: None,
            path_filter: Vec::new(),
        });
        if let Some(error) = inspection.error {
            return Err(error);
        }
        let data = inspection.data.expect("successful inspection");
        if data
            .branch
            .as_deref()
            .and_then(|b| b.strip_prefix("refs/heads/"))
            != data.settings.branch.as_deref()
            || data.branch.is_none()
        {
            return Err(OperationError::new(
                "BRANCH_CHANGED",
                "feedback requires the adopted branch",
            ));
        }
        let head = data.head.ok_or_else(|| invalid("no candidate exists"))?;
        adoption_range(self.git(), &head, &data.scope.digest)
    }
}

fn adoption_range(git: &Git, head: &str, scope: &str) -> Result<Vec<String>, OperationError> {
    let history = git.first_parent_history(head)?;
    let mut walk = HistoryWalk::new(git);
    let mut selected = Vec::new();
    for commit in history {
        selected.push(commit.clone());
        match walk.validate_marker(&commit) {
            Ok(marker) if marker.kind == CommitKind::Start => {
                if marker.scope_digest != scope {
                    return Err(invalid("notes adoption was reset by another scope"));
                }
                return Ok(selected);
            }
            Ok(_) | Err(MarkerError::Invalid(_)) => {}
            Err(error) => {
                return Err(OperationError::new(
                    error.public_code(),
                    format!("notes history is unreadable: {error}"),
                ));
            }
        }
    }
    Err(invalid("notes have no valid starting marker"))
}

struct PrivateRef<'a> {
    git: &'a Git,
    name: String,
}
impl Drop for PrivateRef<'_> {
    fn drop(&mut self) {
        let _ = self
            .git
            .run_ok(Mode::Mutate, ["update-ref", "-d", &self.name]);
    }
}

fn append_atomic(
    git: &Git,
    commit: &str,
    note: Note,
    effects: &mut Effects,
) -> Result<AppendOutcome, OperationError> {
    let private = PrivateRef {
        git,
        name: format!("refs/notes/margin-tmp-{}", unique_suffix()),
    };
    let zero = "0".repeat(commit.len());
    for _ in 0..3 {
        let snapshot = Snapshot::load(git)?;
        let current = snapshot.read(git, commit);
        if let Some(reason) = current.unreadable {
            return Err(invalid(&reason));
        }
        if let Some(existing) = current.notes.iter().find(|n| n.id == note.id) {
            if existing != &note {
                return Err(invalid("note ID already names another entry"));
            }
            return Ok(AppendOutcome {
                commit: commit.to_string(),
                note,
                appended: false,
                notes_ref: snapshot.head.unwrap_or_default(),
            });
        }
        if current.notes.len() >= MAX_ENTRIES {
            return Err(invalid("this commit already holds 50 notes"));
        }
        let mut notes = current.notes;
        notes.push(note.clone());
        if let Some(head) = &snapshot.head {
            git.run_ok(Mode::Mutate, ["update-ref", &private.name, head])?;
        } else {
            git.run_ok(Mode::Mutate, ["update-ref", "-d", &private.name])?;
        }
        let bytes = serde_json::to_vec(&Envelope {
            schema_version: 1,
            notes,
        })
        .expect("notes serialize");
        let args = [
            "notes",
            "--ref",
            private.name.as_str(),
            "add",
            "-f",
            "-F",
            "-",
            commit,
        ];
        let output = git.run_stdin(Mode::Mutate, args, &bytes)?;
        if !output.success() {
            return Err(OperationError::new(
                "GIT_FAILED",
                "the private notes commit could not be written",
            ));
        }
        let new = git.resolve_commit(OsStr::new(&private.name))?;
        let publication = git.run(
            Mode::Mutate,
            [
                "update-ref",
                NOTES_REF,
                &new,
                snapshot.head.as_deref().unwrap_or(&zero),
            ],
        );
        match publication {
            Ok(output) if output.success() => {
                effects.notes_ref = ObservedEffect::Changed;
                return Ok(AppendOutcome {
                    commit: commit.to_string(),
                    note,
                    appended: true,
                    notes_ref: new,
                });
            }
            Ok(_) => {
                // Retry only a verified ref conflict, not an unrelated Git failure.
                if Snapshot::load(git)?.head == snapshot.head {
                    return Err(OperationError::new(
                        "GIT_FAILED",
                        "the notes ref update failed",
                    ));
                }
            }
            Err(error) => {
                effects.notes_ref = ObservedEffect::Unknown;
                return Err(error.into());
            }
        }
    }
    Err(OperationError::new(
        "NOTES_CONFLICT",
        "the notes ref changed during three append attempts; retry",
    )
    .retryable())
}

/// Context is data, labeled by commit and entry; never a normal CLI envelope.
pub fn feedback(operations: &Operations<'_>) -> Result<Option<String>, OperationError> {
    let snapshot = Snapshot::load(operations.git())?;
    if snapshot.head.is_none() {
        return Ok(None);
    }
    let commits = operations.feedback_commits()?;
    let mut context = String::from(
        "Margin writer feedback (annotations; authorship is claimed). Reply with margin note --text or margin note --group <digest> --text.\n",
    );
    let notice = "\n[Feedback omitted or shortened by the 50-entry / 16 KiB limit.]\n";
    let unreadable_notice = "\n[Unreadable note envelopes were skipped; inspect margin notes --revision <commit> for diagnostics.]\n";
    let mut count = 0;
    let mut unreadable = false;
    let mut truncated = false;
    'commits: for commit in commits {
        let notes = snapshot.read(operations.git(), &commit);
        if notes.unreadable.is_some() {
            unreadable = true;
        }
        for note in notes.notes.into_iter().filter(|n| n.by == Author::Writer) {
            let line = format!(
                "Writer note {} on {}{}: {}",
                note.id,
                commit,
                note.group
                    .as_ref()
                    .map(|g| format!(" group {g}"))
                    .unwrap_or_default(),
                crate::cli::display_escape(&note.text)
            );
            let remaining = MAX_CONTEXT_BYTES
                .saturating_sub(context.len() + notice.len() + unreadable_notice.len());
            if count >= MAX_ENTRIES {
                truncated = true;
                break 'commits;
            }
            count += 1;
            if line.len() + 1 > remaining {
                let mut end = remaining;
                while !line.is_char_boundary(end) {
                    end -= 1;
                }
                context.push_str(&line[..end]);
                truncated = true;
                break 'commits;
            }
            context.push_str(&line);
            context.push('\n');
        }
    }
    if count == 0 && !unreadable {
        return Ok(None);
    }
    if truncated {
        context.push_str(notice);
    }
    if unreadable {
        context.push_str(unreadable_notice);
    }
    Ok(Some(context))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_envelopes() {
        let note = Note {
            id: "one".into(),
            by: Author::Agent,
            group: None,
            text: "why".into(),
        };
        let mut envelope = Envelope {
            schema_version: 1,
            notes: vec![note.clone()],
        };
        assert!(Envelope::parse(&serde_json::to_vec(&envelope).unwrap()).is_ok());
        envelope.notes.push(Note {
            text: "different".into(),
            ..note
        });
        assert!(Envelope::parse(&serde_json::to_vec(&envelope).unwrap()).is_err());
        for raw in [
            r#"{"schema_version":1,"notes":[],"extra":true}"#,
            r#"{"schema_version":1,"schema_version":1,"notes":[]}"#,
            r#"{"schema_version":1,"notes":[{"id":"one","by":"agent","text":"a","text":"b"}]}"#,
        ] {
            assert!(Envelope::parse(raw.as_bytes()).is_err());
        }
    }
}
