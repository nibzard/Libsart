//! The refresh slice: polling that observes arrivals and rereads the
//! open surface, and the worker that keeps both off the input loop
//! (SPEC 5.3, 13.3; task T045).
//!
//! The displayed B/C pair is immutable, so the only honest way to treat
//! the repository that keeps moving around it is to observe it and say
//! what arrived. One pinned observation records what the workspace held
//! when the comparison was pinned: the candidate, the scope, the
//! uncaptured scoped paths, and the mutation eligibility. Every later
//! observation is compared against that record, and the first difference
//! names one arrival of SPEC 5.3: a new head, a dirty scoped file, a
//! scope change, or changed eligibility. An arrival relabels every group
//! and disables application; the text stays readable, and one explicit
//! refresh re-pins over the rebuilt review. Beside that observation, each
//! job rereads the displayed document and rebuilds the document list, so
//! the surface shows current disk state while the pinned comparison
//! stands.
//!
//! The observation runs on one worker thread, because a blocking Git
//! read must not stall a key press (SPEC 13.3, 17.4 P-KEY). The worker
//! owns one clone of the process adapter and answers one job at a time
//! through standard-library channels. Every job carries the generation
//! of the shell state that asked for it, so a reply the shell superseded
//! is discarded whole. At most one job runs while one waits: requests
//! that arrive while both slots hold replace the waiting job, so
//! redundant polls coalesce to the newest generation instead of queueing
//! behind each other. Polling discovers state; it saves nothing (SPEC
//! 13.3).

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::app::{InspectRequest, InspectResult, Operations, PolicyBlocker, ShowOutcome, ShowRequest};
use crate::domain::OperationState;
use crate::domain::ScopeInput;
use crate::git::Git;
use crate::tui::render;
use crate::tui::shell::{self, WorkspaceSnapshot};

/// The default inspection interval of one open surface (SPEC 13.3).
pub const INSPECTION_INTERVAL: Duration = Duration::from_secs(2);

/// How many characters one arrival sentence occupies.
const NOTICE_CHARS: usize = 200;

/// What one job asks the worker to reread beside the workspace (SPEC
/// 13.3): the displayed document and the document list, so the surface
/// shows current disk state without one blocking read on the input loop.
#[derive(Clone, Debug)]
pub struct Watch {
    /// The repository root the paths resolve against.
    pub root: PathBuf,
    /// The displayed document's repository-relative path; `None` when no
    /// document stands.
    pub document: Option<String>,
    /// How the document list rebuilds: the adopted scope's input, or the
    /// discovery walk of one unadopted folder.
    pub list: WatchList,
}

impl Watch {
    /// One watch of nothing but the root: no document, one unadopted
    /// list.
    pub fn new(root: PathBuf) -> Watch {
        Watch {
            root,
            document: None,
            list: WatchList::Unadopted,
        }
    }
}

/// How the document list of one watch rebuilds.
#[derive(Clone, Debug)]
pub enum WatchList {
    /// The adopted scope: the worker resolves the stored input.
    Adopted(ScopeInput),
    /// The unadopted folder: the worker repeats the discovery walk.
    Unadopted,
}

/// One job for the worker: the generation that asked, and what to reread
/// (SPEC 13.3).
#[derive(Clone, Debug)]
pub struct WatchJob {
    /// The generation of the requesting shell state.
    pub generation: u64,
    /// What the job rereads beside the workspace.
    pub watch: Watch,
}

/// The working file of one watched document, as the worker read it.
#[derive(Clone, Debug)]
pub enum WatchedFile {
    /// The bounded working bytes and the complete byte length.
    Bytes(Vec<u8>, u64),
    /// The working file no longer exists.
    Gone,
    /// The working file could not be read; no claim is honest about it.
    Unread,
}

/// One watched document's reread: its path, its working file, and the
/// candidate content at `HEAD` when one exists. The shell rebuilds its
/// view from these pieces through the same rule an interactive load uses,
/// so the two can never disagree about the label.
#[derive(Clone, Debug)]
pub struct WatchedDocument {
    /// The repository-relative path the job watched.
    pub path: String,
    /// The working file as the worker read it.
    pub file: WatchedFile,
    /// The candidate content at `HEAD`; `None` before adoption and when
    /// no candidate exists.
    pub candidate: Option<ShowOutcome>,
}

/// One answered inspection: the generation of the shell state that asked
/// for it, the outcome of the one shared `inspect_workspace` operation,
/// and what the watched document and list held (SPEC 13.2, 13.3).
pub struct InspectionReply {
    /// The generation of the requesting shell state.
    pub generation: u64,
    /// The inspection outcome; a failure observes and claims nothing.
    pub outcome: InspectResult,
    /// The watched document's reread, when the job watched one.
    pub document: Option<WatchedDocument>,
    /// The rebuilt document list, when the job could build one.
    pub documents: Option<Vec<String>>,
}

/// The one worker of the terminal surface (SPEC 13.3): one thread that
/// runs the blocking Git reads of inspection polling off the input loop.
pub struct RefreshWorker {
    /// Where jobs go; dropping it ends the thread.
    jobs: Sender<WatchJob>,
    /// Where answers come back.
    replies: Receiver<InspectionReply>,
    /// The thread, joined on shutdown.
    handle: JoinHandle<()>,
    /// Whether one job is running.
    running: bool,
    /// The one waiting job: the newest job asked for while one ran. One
    /// slot is the whole queue, so redundant polls coalesce.
    queued: Option<WatchJob>,
}

impl RefreshWorker {
    /// Starts the worker over one adapter clone. The worker builds its
    /// operations once and reuses their shared history walk for every
    /// poll, so an idle surface pays for what changed since the last
    /// tick, not for the whole history again every two seconds
    /// (SPEC 13.3, 17.4). `None` when the thread cannot start: the
    /// surface then runs without polling, and its keys keep working
    /// exactly as they do.
    pub fn spawn(git: Git) -> Option<RefreshWorker> {
        let (job_sender, jobs) = mpsc::channel::<WatchJob>();
        let (reply_sender, replies) = mpsc::channel::<InspectionReply>();
        let handle = thread::Builder::new()
            .name("margin-inspection".to_string())
            .spawn(move || {
                // One loop over the job channel: a job is one inspection
                // through the shared operation plus the watch's rereads,
                // and the reply channel closing means the surface left.
                let operations = Operations::new(&git);
                for job in jobs {
                    let outcome = operations.inspect_workspace(&InspectRequest {
                        scope: None,
                        path_filter: Vec::new(),
                    });
                    let document = job.watch.document.map(|path| {
                        let file =
                            match shell::read_working(&job.watch.root, &path, shell::READ_VIEW_BYTES)
                            {
                                Ok((bytes, total)) => WatchedFile::Bytes(bytes, total),
                                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                    WatchedFile::Gone
                                }
                                Err(_) => WatchedFile::Unread,
                            };
                        let candidate = candidate_at_head(&operations, &outcome, &path);
                        WatchedDocument {
                            path,
                            file,
                            candidate,
                        }
                    });
                    let documents = match &job.watch.list {
                        WatchList::Adopted(input) => {
                            crate::app::resolve_scope(operations.git(), input)
                                .ok()
                                .map(|resolved| shell::document_paths(&resolved))
                        }
                        WatchList::Unadopted => Some(shell::discover_documents(&job.watch.root)),
                    };
                    if reply_sender
                        .send(InspectionReply {
                            generation: job.generation,
                            outcome,
                            document,
                            documents,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            })
            .ok()?;
        Some(RefreshWorker {
            jobs: job_sender,
            replies,
            handle,
            running: false,
            queued: None,
        })
    }

    /// Requests one inspection of the state the job names, rereading what
    /// its watch holds (SPEC 13.3).
    ///
    /// The request never blocks: it either sends one job or replaces the
    /// one waiting job, so a burst of requests costs one running plus one
    /// waiting inspection, whatever the burst held.
    pub fn request(&mut self, job: WatchJob) {
        if self.running {
            self.queued = Some(job);
        } else {
            self.running = self.jobs.send(job).is_ok();
        }
    }

    /// Says whether a job runs or waits. The event loop shortens its
    /// input wait while one does, so an answer reaches the frame well
    /// inside the P-REFRESH budget (SPEC 17.4).
    pub fn is_busy(&self) -> bool {
        self.running || self.queued.is_some()
    }

    /// Takes one answered inspection, when one arrived without waiting.
    pub fn receive(&mut self) -> Option<InspectionReply> {
        let reply = match self.replies.try_recv() {
            Ok(reply) => reply,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => {
                self.running = false;
                self.queued = None;
                return None;
            }
        };
        self.settled();
        Some(reply)
    }

    /// Waits up to `limit` for one answered inspection. The buffer tests
    /// use it to prove the request and reply cycle without a terminal.
    pub fn wait_for(&mut self, limit: Duration) -> Option<InspectionReply> {
        match self.replies.recv_timeout(limit) {
            Ok(reply) => {
                self.settled();
                Some(reply)
            }
            Err(_) => None,
        }
    }

    /// Frees the running slot and sends the waiting job, when one stands.
    fn settled(&mut self) {
        self.running = false;
        if let Some(job) = self.queued.take() {
            self.running = self.jobs.send(job).is_ok();
        }
    }

    /// Stops the worker: the job channel closes, a running inspection
    /// finishes, and the thread joins. The surface quits without it.
    pub fn shutdown(self) {
        drop(self.jobs);
        let _ = self.handle.join();
    }
}

/// Reads the candidate content of `path` at `HEAD` through the shared
/// show operation, under the same guard the shell's interactive load
/// uses: only one inspection that succeeded — an adopted repository —
/// with a standing head has a candidate (SPEC 5.4). The read runs on the
/// worker, so the input loop never blocks on it.
fn candidate_at_head(
    operations: &Operations<'_>,
    outcome: &InspectResult,
    path: &str,
) -> Option<ShowOutcome> {
    let data = outcome.data.as_ref()?;
    data.head.as_ref()?;
    let shown = operations.show_document(&ShowRequest {
        path: path.to_string(),
        revision: "HEAD".to_string(),
        max_bytes: Some(shell::READ_VIEW_BYTES),
        max_lines: None,
        raw: false,
    });
    if shown.ok() {
        shown.data
    } else {
        None
    }
}

/// The workspace observation one pinned comparison carries (SPEC 5.3):
/// what the repository held when the comparison was pinned. Polling
/// compares against this record, so an arrival is a change from the
/// pinned context, never an absolute condition: a review pinned over a
/// dirty scope stays readable until its scope changes again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedObservation {
    /// The candidate commit at pin time; `None` while the branch was
    /// unborn.
    pub head: Option<String>,
    /// The scope digest at pin time.
    pub scope: String,
    /// The uncaptured scoped paths at pin time, sorted by path bytes.
    pub uncaptured: Vec<String>,
    /// The mutation policy blockers at pin time.
    pub blockers: Vec<PolicyBlocker>,
    /// The operation state at pin time.
    pub operation: OperationState,
}

impl PinnedObservation {
    /// The observation of one workspace snapshot.
    pub fn of(workspace: &WorkspaceSnapshot) -> PinnedObservation {
        PinnedObservation {
            head: workspace.head.clone(),
            scope: workspace.scope_id.clone(),
            uncaptured: workspace.uncaptured_paths.clone(),
            blockers: workspace.blockers.clone(),
            operation: workspace.operation,
        }
    }
}

/// Names the arrival the observed workspace holds over the pinned
/// observation (SPEC 5.3), in the order the specification lists: a new
/// head, a dirty scoped file, a scope change, then changed eligibility.
/// `None` means the pinned comparison still describes the repository.
pub fn arrival_over(pinned: &PinnedObservation, workspace: &WorkspaceSnapshot) -> Option<String> {
    if workspace.head.as_deref() != pinned.head.as_deref() {
        return Some(match &workspace.head {
            Some(head) => bounded(&format!(
                "new content: the candidate moved to {} after this review was pinned",
                render::short(head)
            )),
            None => "new content: the candidate is gone; the branch holds no commit".to_string(),
        });
    }
    if workspace.uncaptured_paths != pinned.uncaptured {
        let count = workspace.uncaptured_paths.len();
        return Some(bounded(&format!(
            "new content: {count} scoped path{} hold{} working edits this review never saw",
            if count == 1 { "" } else { "s" },
            if count == 1 { "s" } else { "" },
        )));
    }
    if workspace.scope_id != pinned.scope {
        return Some(bounded(&format!(
            "the writing scope changed from {} to {}",
            render::short(&pinned.scope),
            render::short(&workspace.scope_id)
        )));
    }
    if workspace.blockers != pinned.blockers {
        return Some(bounded(&eligibility_sentence(
            &pinned.blockers,
            &workspace.blockers,
        )));
    }
    if is_active_writing(workspace.operation) && workspace.operation != pinned.operation {
        return Some(bounded(&format!(
            "active writing: a Margin mutation is {}",
            workspace.operation
        )));
    }
    None
}

/// Says whether one operation state is a mutation in progress. The
/// recovery-required state derives from the blockers, so the blocker
/// comparison above already names it; only these states add news.
fn is_active_writing(operation: OperationState) -> bool {
    matches!(
        operation,
        OperationState::Validating
            | OperationState::Applying
            | OperationState::Committing
            | OperationState::OutcomeUnknown
    )
}

/// The SPEC 5.3 category of one mutation blocker: staged changes stand
/// alone, because they ask for one inspection rather than one adoption;
/// every other blocker is unsupported repository state.
fn blocker_category(blocker: &PolicyBlocker) -> &'static str {
    match blocker {
        PolicyBlocker::StagedChanges { .. } => "staged changes",
        _ => "unsupported repository state",
    }
}

/// The sentence that names the eligibility change between two blocker
/// lists: the first blocker that arrived states its category, and a
/// blocker that cleared states the recovery honestly.
fn eligibility_sentence(pinned: &[PolicyBlocker], current: &[PolicyBlocker]) -> String {
    if let Some(arrived) = current.iter().find(|blocker| !pinned.contains(blocker)) {
        return format!("{}: {arrived}", blocker_category(arrived));
    }
    if let Some(cleared) = pinned.iter().find(|blocker| !current.contains(blocker)) {
        return format!("the repository state changed: {cleared} cleared");
    }
    "the repository state changed".to_string()
}

/// Truncates one arrival sentence to the notice bound.
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

    /// One observation over hand-chosen facts.
    fn observation(head: Option<&str>) -> PinnedObservation {
        PinnedObservation {
            head: head.map(str::to_string),
            scope: "sha1:scope".to_string(),
            uncaptured: Vec::new(),
            blockers: Vec::new(),
            operation: OperationState::Idle,
        }
    }

    /// One adopted snapshot over hand-chosen facts.
    fn snapshot(head: Option<&str>) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            adopted: true,
            folder: "notes".to_string(),
            scope_roots: "notes/".to_string(),
            scope_id: "sha1:scope".to_string(),
            branch: Some("main".to_string()),
            head: head.map(str::to_string),
            preservation: crate::domain::PreservationState::Captured,
            review: crate::domain::ReviewState::Pending,
            pending_files: Some(1),
            uncaptured: Some(0),
            uncaptured_paths: Vec::new(),
            documents: 1,
            blockers: Vec::new(),
            operation: OperationState::Idle,
            note: None,
        }
    }

    #[test]
    fn a_matching_workspace_reports_no_arrival() {
        let pinned = observation(Some("a".repeat(40)).as_deref());
        assert!(arrival_over(&pinned, &snapshot(Some(&"a".repeat(40)))).is_none());
    }

    #[test]
    fn a_moved_head_names_new_content() {
        let pinned = observation(Some(&"a".repeat(40)));
        let why = arrival_over(&pinned, &snapshot(Some(&"b".repeat(40)))).expect("an arrival");
        assert!(why.contains("new content"), "the category: {why}");
        assert!(why.contains("the candidate moved to"), "the fact: {why}");
        // A branch that lost its candidate is an arrival too, not a
        // crash or a silent pass.
        let why = arrival_over(&pinned, &snapshot(None)).expect("an arrival");
        assert!(why.contains("the candidate is gone"), "the fact: {why}");
    }

    #[test]
    fn dirty_scoped_files_name_new_content() {
        let mut workspace = snapshot(Some(&"a".repeat(40)));
        workspace.uncaptured_paths = vec!["notes/a.md".to_string(), "notes/b.md".to_string()];
        let pinned = observation(Some(&"a".repeat(40)));
        let why = arrival_over(&pinned, &workspace).expect("an arrival");
        assert!(why.contains("new content"), "the category: {why}");
        assert!(why.contains("2 scoped paths hold"), "the count: {why}");
    }

    #[test]
    fn a_changed_scope_and_changed_eligibility_name_their_categories() {
        let pinned = observation(Some(&"a".repeat(40)));
        let mut scope = snapshot(Some(&"a".repeat(40)));
        scope.scope_id = "sha1:other".to_string();
        assert!(
            arrival_over(&pinned, &scope)
                .expect("an arrival")
                .contains("the writing scope changed")
        );

        let mut staged = snapshot(Some(&"a".repeat(40)));
        staged.blockers = vec![PolicyBlocker::StagedChanges {
            count: 1,
            paths: Vec::new(),
        }];
        let why = arrival_over(&pinned, &staged).expect("an arrival");
        assert!(why.starts_with("staged changes: "), "the category: {why}");

        let mut unsupported = snapshot(Some(&"a".repeat(40)));
        unsupported.blockers = vec![PolicyBlocker::ActiveMerge];
        let why = arrival_over(&pinned, &unsupported).expect("an arrival");
        assert!(
            why.starts_with("unsupported repository state: "),
            "the category: {why}"
        );

        let mut writing = snapshot(Some(&"a".repeat(40)));
        writing.operation = OperationState::Applying;
        let why = arrival_over(&pinned, &writing).expect("an arrival");
        assert!(why.starts_with("active writing: "), "the category: {why}");

        // A recovery-required state names its blocker, not a category of
        // its own: the blocker comparison comes first.
        let mut recovering = snapshot(Some(&"a".repeat(40)));
        recovering.operation = OperationState::RecoveryRequired;
        assert!(
            arrival_over(&pinned, &recovering).is_none(),
            "the blockers name the recovery"
        );
    }

    #[test]
    fn a_cleared_blocker_states_the_recovery() {
        let mut pinned = observation(Some(&"a".repeat(40)));
        pinned.blockers = vec![PolicyBlocker::ActiveMerge];
        let why = arrival_over(&pinned, &snapshot(Some(&"a".repeat(40)))).expect("an arrival");
        assert!(why.contains("cleared"), "the change: {why}");
    }
}
