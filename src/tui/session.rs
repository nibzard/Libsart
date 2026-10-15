//! Local intent only. These records confer no approval and never mutate Git.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const MAX_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_RECORDS: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub repository: String,
    pub branch: String,
    pub scope: String,
    pub adoption: String,
    pub baseline: String,
    pub candidate: String,
    pub review: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub id: String,
    pub decision: super::changes::Decision,
    pub path: String,
    pub heading: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Anchor {
    pub path: String,
    pub view: String,
    pub line: u32,
    pub drift: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangePosition {
    pub id: String,
    pub before: bool,
    pub line: u32,
    pub drift: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Navigation {
    pub change_position: Option<ChangePosition>,
    pub view: String,
    pub path: Option<String>,
    pub selected: Option<String>,
    pub source: bool,
    pub reading_source: bool,
    pub anchors: Vec<Anchor>,
    pub history_commit: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attempt {
    pub request_id: String,
    pub plan: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub schema_version: u32,
    pub id: String,
    pub updated_ms: u64,
    pub generation: u64,
    pub identity: Identity,
    pub decisions: Vec<Choice>,
    pub visited: Vec<String>,
    pub navigation: Navigation,
    pub attempt: Option<Attempt>,
    pub reference: Option<super::reference::Descriptor>,
}

impl Record {
    pub fn validate(&mut self) -> Result<(), String> {
        if self.schema_version != 1 {
            return Err("Unsupported saved-review version".into());
        }
        if self.generation > i64::MAX as u64 {
            return Err("Corrupt saved review: invalid save generation".into());
        }
        if !valid_id(&self.id) {
            return Err("Invalid saved-review ID".into());
        }
        if self.decisions.len() > 10_000
            || self.visited.len() > 10_000
            || self.navigation.anchors.len() > 256
        {
            return Err("Saved review exceeds entry limits".into());
        }
        if let Some(attempt) = &self.attempt {
            let plan = crate::domain::ReviewPlan::parse(&attempt.plan)
                .map_err(|_| "Invalid saved finish plan".to_string())?;
            if plan.request_id != attempt.request_id
                || plan.review_id != self.identity.review
                || plan.branch_ref != self.identity.branch
                || plan.scope_id != self.identity.scope
                || plan.baseline_oid != self.identity.baseline
                || plan.candidate_oid != self.identity.candidate
            {
                return Err("Saved finish plan identity mismatch".into());
            }
        }
        let mut unique = BTreeMap::new();
        for choice in self.decisions.drain(..) {
            if let Some(old) = unique.insert(choice.id.clone(), choice.clone())
                && old != choice
            {
                return Err("Conflicting saved decisions".into());
            }
        }
        self.decisions = unique.into_values().collect();
        self.visited = self
            .visited
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub id: String,
    pub record: Result<Record, String>,
    pub completed: Option<String>,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 100 && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

fn regular(path: &Path) -> io::Result<()> {
    if !fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(io::Error::other(
            "saved-review record or lock is not a regular file",
        ));
    }
    Ok(())
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        // O_NOFOLLOW is platform-specific. The checked metadata and inode
        // comparison below additionally reject replacement before open.
    }
    options
}

fn checked_open(path: &Path, write: bool) -> io::Result<File> {
    regular(path)?;
    let before = fs::symlink_metadata(path)?;
    let file = OpenOptions::new().read(true).write(write).open(path)?;
    let after = file.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev()
            || before.ino() != after.ino()
            || fs::symlink_metadata(path)?.file_type().is_symlink()
        {
            return Err(io::Error::other("saved-review file changed while opening"));
        }
    }
    Ok(file)
}

#[derive(Clone, Debug)]
pub struct Store {
    pub directory: PathBuf,
}

impl Store {
    pub fn from_git(git: &crate::git::Git) -> Result<Self, String> {
        git.git_path("margin/tui/reviews")
            .map(|directory| Self { directory })
            .map_err(|e| e.to_string())
    }

    fn check_directory(&self, create: bool) -> io::Result<bool> {
        // Git resolves the parent, including gitfiles. Only our three local
        // directories are created; none may redirect writes via a symlink.
        let mut dirs = vec![self.directory.clone()];
        let mut parent = self.directory.parent();
        for _ in 0..2 {
            if let Some(p) = parent {
                dirs.push(p.to_path_buf());
                parent = p.parent();
            }
        }
        for dir in dirs.iter().rev() {
            match fs::symlink_metadata(dir) {
                Ok(meta) if meta.file_type().is_dir() => {}
                Ok(_) => {
                    return Err(io::Error::other(
                        "saved-review directory is not a real directory",
                    ));
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound && create => {
                    let mut builder = fs::DirBuilder::new();
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::DirBuilderExt;
                        builder.mode(0o700);
                    }
                    match builder.create(dir) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                            if !fs::symlink_metadata(dir)?.is_dir()
                                || fs::symlink_metadata(dir)?.file_type().is_symlink()
                            {
                                return Err(e);
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(e),
            }
            #[cfg(unix)]
            if create {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
            }
        }
        Ok(true)
    }

    pub fn list(&self) -> Result<(Vec<Entry>, bool), String> {
        if !self.check_directory(false).map_err(|e| e.to_string())? {
            return Ok((vec![], false));
        }
        let mut entries = Vec::new();
        let mut omitted = false;
        // Bound reads and directory traversal, including crash-left temporaries.
        for entry in fs::read_dir(&self.directory)
            .map_err(|e| e.to_string())?
            .take(1001)
        {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(id) = name.strip_suffix(".json")
                && valid_id(id)
            {
                if entries.len() == MAX_RECORDS {
                    omitted = true;
                    break;
                }
                entries.push(Entry {
                    id: id.into(),
                    record: self.read(id),
                    completed: None,
                });
            }
        }
        if fs::read_dir(&self.directory)
            .map_err(|e| e.to_string())?
            .take(1001)
            .count()
            > 1000
        {
            omitted = true;
        }
        entries.sort_by(|a, b| {
            b.record
                .as_ref()
                .map_or(0, |r| r.updated_ms)
                .cmp(&a.record.as_ref().map_or(0, |r| r.updated_ms))
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok((entries, omitted))
    }

    pub fn read(&self, id: &str) -> Result<Record, String> {
        if !valid_id(id) {
            return Err("Invalid saved-review ID".into());
        }
        self.check_directory(false).map_err(|e| e.to_string())?;
        let mut file = checked_open(&self.directory.join(format!("{id}.json")), false)
            .map_err(|e| e.to_string())?;
        if file.metadata().map_err(|e| e.to_string())?.len() > MAX_BYTES {
            return Err("Saved review exceeds 4 MiB".into());
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("Saved review exceeds 4 MiB".into());
        }
        let mut record: Record =
            serde_json::from_slice(&bytes).map_err(|e| format!("Corrupt saved review: {e}"))?;
        record.validate()?;
        if record.id != id {
            return Err("Saved-review ID does not match its file".into());
        }
        Ok(record)
    }

    pub fn create(&self) -> Result<Owner, String> {
        self.check_directory(true).map_err(|e| e.to_string())?;
        for _ in 0..16 {
            // The exclusive create is authoritative; timestamp, process ID,
            // and monotonic sequence make collisions exceedingly unlikely.
            static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = format!(
                "{:x}-{:x}-{:x}",
                now_ms(),
                std::process::id(),
                SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
            let path = self.directory.join(format!("{id}.json"));
            let owner = self.acquire(&id)?;
            match private_options().create_new(true).open(path) {
                Ok(_) => return Ok(owner),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
        Err("Could not allocate a separate saved-review ID".into())
    }

    pub fn acquire(&self, id: &str) -> Result<Owner, String> {
        if !valid_id(id) {
            return Err("Invalid saved-review ID".into());
        }
        self.check_directory(true).map_err(|e| e.to_string())?;
        let path = self.directory.join(format!("{id}.lock"));
        let lock = match private_options().create_new(true).open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                checked_open(&path, true).map_err(|e| e.to_string())?
            }
            Err(e) => return Err(e.to_string()),
        };
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                return Err(
                    "Saved review is owned by another viewer; inspect it or start a separate draft"
                        .into(),
                );
            }
            Err(fs::TryLockError::Error(error)) => {
                return Err(format!("Cannot acquire saved-review ownership: {error}"));
            }
        }
        Ok(Owner {
            store: self.clone(),
            id: id.into(),
            _lock: lock,
        })
    }
}

pub struct Owner {
    pub store: Store,
    pub id: String,
    _lock: File,
}
impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self._lock.unlock();
    }
}
impl Owner {
    pub fn save(&self, record: &Record) -> Result<(), String> {
        self.save_with(record, |file, bytes| file.write_all(bytes))
    }
    fn save_with(
        &self,
        record: &Record,
        write: impl FnOnce(&mut File, &[u8]) -> io::Result<()>,
    ) -> Result<(), String> {
        self.save_with_ops(record, write, |from, to| fs::rename(from, to))
    }
    fn save_with_ops(
        &self,
        record: &Record,
        write: impl FnOnce(&mut File, &[u8]) -> io::Result<()>,
        replace: impl FnOnce(&Path, &Path) -> io::Result<()>,
    ) -> Result<(), String> {
        if record.id != self.id {
            return Err("Draft ownership mismatch".into());
        }
        let mut record = record.clone();
        record.validate()?;
        let bytes = serde_json::to_vec(&record).map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("Saved review exceeds 4 MiB".into());
        }
        self.store
            .check_directory(false)
            .map_err(|e| e.to_string())?;
        let path = self.store.directory.join(format!("{}.json", self.id));
        regular(&path).map_err(|e| e.to_string())?;
        static TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temp = self.store.directory.join(format!(
            "{}.{}.{:x}.{stamp:x}.{sequence:x}.tmp",
            self.id,
            record.generation,
            std::process::id()
        ));
        let result = (|| -> io::Result<()> {
            let mut file = private_options().create_new(true).open(&temp)?;
            write(&mut file, &bytes)?;
            file.sync_all()?;
            regular(&path)?;
            replace(&temp, &path)?;
            File::open(&self.store.directory)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result.map_err(|e| e.to_string())
    }
    pub fn remove(&self) -> Result<(), String> {
        self.store
            .check_directory(false)
            .map_err(|e| e.to_string())?;
        let path = self.store.directory.join(format!("{}.json", self.id));
        regular(&path).map_err(|e| e.to_string())?;
        fs::remove_file(path).map_err(|e| e.to_string())?;
        File::open(&self.store.directory)
            .and_then(|d| d.sync_all())
            .map_err(|e| e.to_string())
    }
}

#[derive(Default)]
struct State {
    pending: Option<Record>,
    completed: Option<(u64, Result<(), String>)>,
    busy: bool,
    stop: bool,
}
/// One active write plus one coalesced newest snapshot; serialization and fsync
/// happen here, never on the input loop. Joining precedes any deletion.
pub struct Writer {
    owner: Arc<Owner>,
    state: Arc<(Mutex<State>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}
impl Writer {
    pub fn spawn(owner: Owner) -> Result<Self, String> {
        let owner = Arc::new(owner);
        let copy = owner.clone();
        let state = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let shared = state.clone();
        let thread = thread::Builder::new()
            .name("margin-review-save".into())
            .spawn(move || {
                let (mutex, wake) = &*shared;
                loop {
                    let mut state = mutex.lock().unwrap();
                    while state.pending.is_none() && !state.stop {
                        state = wake.wait(state).unwrap();
                    }
                    let Some(record) = state.pending.take() else {
                        break;
                    };
                    state.busy = true;
                    drop(state);
                    let result = copy.save(&record);
                    let mut state = mutex.lock().unwrap();
                    state.busy = false;
                    state.completed = Some((record.generation, result));
                    wake.notify_all();
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            owner,
            state,
            thread: Some(thread),
        })
    }
    pub fn id(&self) -> &str {
        &self.owner.id
    }
    pub fn submit(&self, record: Record) {
        let (mutex, wake) = &*self.state;
        mutex.lock().unwrap().pending = Some(record);
        wake.notify_all();
    }
    pub fn completion(&self) -> Option<(u64, Result<(), String>)> {
        self.state.0.lock().unwrap().completed.take()
    }
    pub fn flush(&self, record: Record) -> Result<(), String> {
        let generation = record.generation;
        self.submit(record);
        let (mutex, wake) = &*self.state;
        let mut state = mutex.lock().unwrap();
        loop {
            if let Some((done, result)) = &state.completed
                && *done >= generation
                && !state.busy
                && state.pending.is_none()
            {
                return result.clone();
            }
            state = wake.wait(state).unwrap();
        }
    }
    pub fn remove(mut self) -> Result<(), String> {
        self.stop();
        self.owner.remove()
    }
    fn stop(&mut self) {
        {
            let (mutex, wake) = &*self.state;
            let mut state = mutex.lock().unwrap();
            state.stop = true;
            wake.notify_all();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(id: &str, generation: u64) -> Record {
        Record {
            schema_version: 1,
            id: id.into(),
            updated_ms: now_ms(),
            generation,
            identity: Identity {
                repository: "repo".into(),
                branch: "refs/heads/main".into(),
                scope: "scope".into(),
                adoption: "start".into(),
                baseline: "B".into(),
                candidate: "C".into(),
                review: "review".into(),
            },
            decisions: vec![Choice {
                id: "change".into(),
                decision: super::super::changes::Decision::Restore,
                path: "a.md".into(),
                heading: "A".into(),
            }],
            visited: vec!["change".into()],
            navigation: Navigation::default(),
            attempt: None,
            reference: None,
        }
    }
    fn store(root: &Path) -> Store {
        Store {
            directory: root.join("margin/tui/reviews"),
        }
    }
    #[test]
    fn ownership_atomic_failure_and_crash_temps_preserve_valid_intent() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());
        assert!(store.list().unwrap().0.is_empty());
        assert!(!store.directory.exists());
        let owner = store.create().unwrap();
        let id = owner.id.clone();
        owner.save(&record(&id, 1)).unwrap();
        assert!(
            store
                .acquire(&id)
                .err()
                .unwrap()
                .contains("owned by another viewer")
        );
        assert_eq!(
            store.read(&id).unwrap().generation,
            1,
            "inspection is allowed while owned"
        );
        let collision = store.directory.join(format!("{id}.2.tmp"));
        fs::write(&collision, b"crashed").unwrap();
        assert!(
            owner
                .save_with_ops(
                    &record(&id, 2),
                    |file, bytes| file.write_all(bytes),
                    |_, _| Err(io::ErrorKind::PermissionDenied.into())
                )
                .is_err()
        );
        assert_eq!(store.read(&id).unwrap().generation, 1);
        fs::write(store.directory.join("crash.tmp"), b"incomplete").unwrap();
        assert_eq!(store.list().unwrap().0.len(), 1);
        owner.save(&record(&id, 1)).unwrap();
        assert_eq!(store.read(&id).unwrap().generation, 1);
        drop(owner);
        let owner = store.acquire(&id).unwrap();
        owner.save(&record(&id, 3)).unwrap();
        assert_eq!(store.read(&id).unwrap().generation, 3);
        owner.remove().unwrap();
        assert!(store.list().unwrap().0.is_empty());
    }
    #[test]
    fn bounded_corruption_unknown_decisions_and_versions_are_inspectable() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());
        let owner = store.create().unwrap();
        let id = owner.id.clone();
        let path = store.directory.join(format!("{id}.json"));
        for bytes in [
            b"{".to_vec(),
            serde_json::to_vec(&{
                let mut exhausted = record(&id, 1);
                exhausted.generation = u64::MAX;
                fs::write(&path, serde_json::to_vec(&exhausted).unwrap()).unwrap();
                assert!(store.read(&id).unwrap_err().contains("generation"));
                let mut r = record(&id, 1);
                r.schema_version = 2;
                r
            })
            .unwrap(),
            serde_json::to_vec(&record(&id, 1))
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
        ] {
            fs::write(&path, &bytes).unwrap();
            if bytes.len() == 1 {
                assert!(store.read(&id).unwrap_err().contains("Corrupt"));
            } else if serde_json::from_slice::<Record>(&bytes)
                .unwrap()
                .schema_version
                == 2
            {
                assert!(store.read(&id).unwrap_err().contains("Unsupported"));
            } else {
                assert!(store.read(&id).is_ok());
            }
        }
        let mut r = record(&id, 1);
        r.decisions.push(Choice {
            decision: super::super::changes::Decision::Keep,
            ..r.decisions[0].clone()
        });
        fs::write(&path, serde_json::to_vec(&r).unwrap()).unwrap();
        assert!(store.read(&id).unwrap_err().contains("Conflicting"));
        fs::write(&path, vec![b' '; MAX_BYTES as usize + 1]).unwrap();
        assert!(store.read(&id).unwrap_err().contains("4 MiB"));
        assert!(store.list().unwrap().0[0].record.is_err());
        assert!(path.exists());
    }
    #[test]
    fn full_or_unwritable_storage_after_partial_write_preserves_the_previous_record() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());
        let owner = store.create().unwrap();
        let id = owner.id.clone();
        owner.save(&record(&id, 1)).unwrap();
        let result = owner.save_with(&record(&id, 2), |file, bytes| {
            file.write_all(&bytes[..20])?;
            Err(io::Error::from_raw_os_error(28))
        });
        assert!(result.is_err());
        assert_eq!(store.read(&id).unwrap().generation, 1);
        let result = owner.save_with(&record(&id, 3), |_, _| {
            Err(io::ErrorKind::PermissionDenied.into())
        });
        assert!(result.is_err());
        assert_eq!(store.read(&id).unwrap().generation, 1);
        assert!(
            !fs::read_dir(&store.directory).unwrap().any(|p| p
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "tmp"))
        );
    }

    #[test]
    fn worker_coalesces_and_is_drained_before_deletion() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());
        let owner = store.create().unwrap();
        let id = owner.id.clone();
        let writer = Writer::spawn(owner).unwrap();
        for generation in 1..200 {
            writer.submit(record(&id, generation));
        }
        writer.flush(record(&id, 200)).unwrap();
        assert_eq!(store.read(&id).unwrap().generation, 200);
        writer.submit(record(&id, 201));
        writer.remove().unwrap();
        assert!(
            store.list().unwrap().0.is_empty(),
            "no delayed write can recreate a deleted draft"
        );
    }
    #[test]
    fn enumeration_is_bounded_sorted_and_never_evicts_unapplied_records() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());
        for i in 0..105 {
            let owner = store.create().unwrap();
            let mut r = record(&owner.id, 1);
            r.updated_ms = (i % 3) as u64;
            owner.save(&r).unwrap();
        }
        let (entries, omitted) = store.list().unwrap();
        assert_eq!(entries.len(), 100);
        assert!(omitted);
        for pair in entries.windows(2) {
            let a = pair[0].record.as_ref().unwrap();
            let b = pair[1].record.as_ref().unwrap();
            assert!(a.updated_ms > b.updated_ms || (a.updated_ms == b.updated_ms && a.id < b.id));
        }
        assert_eq!(
            fs::read_dir(&store.directory)
                .unwrap()
                .filter(|e| e
                    .as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "json"))
                .count(),
            105
        );
    }
    #[cfg(unix)]
    #[test]
    fn symlink_records_and_directories_never_redirect_reads_or_writes() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let store = store(root.path());
        let owner = store.create().unwrap();
        assert_eq!(
            fs::metadata(&store.directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let path = store.directory.join(format!("{}.json", owner.id));
        fs::remove_file(&path).unwrap();
        let target = outside.path().join("precious");
        fs::write(&target, b"keep").unwrap();
        symlink(&target, &path).unwrap();
        assert!(store.read(&owner.id).is_err());
        assert!(owner.save(&record(&owner.id, 1)).is_err());
        assert!(owner.remove().is_err());
        assert_eq!(fs::read(&target).unwrap(), b"keep");
        drop(owner);
        fs::remove_file(path).unwrap();
        fs::remove_dir_all(root.path().join("margin")).unwrap();
        symlink(outside.path(), root.path().join("margin")).unwrap();
        assert!(store.create().is_err());
        assert!(store.list().is_err());
    }
}
