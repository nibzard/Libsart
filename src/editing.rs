//! Source editing and guarded working-file saves.
use crate::app::{OperationError, Operations};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const MAX_SOURCE_BYTES: usize = 1024 * 1024;
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Version {
    pub path: String,
    pub worktree: String,
    pub scope: String,
    pub branch: Option<String>,
    pub adoption: Option<String>,
    pub digest: String,
    pub text: String,
}

impl Version {
    pub fn same_context(&self, other: &Self) -> bool {
        self.path == other.path
            && self.worktree == other.worktree
            && self.scope == other.scope
            && self.branch == other.branch
            && self.adoption == other.adoption
    }
}

pub(crate) fn io(error: impl std::fmt::Display) -> OperationError {
    OperationError::new("IO_FAILED", error.to_string())
}
pub(crate) fn stale(message: &str) -> OperationError {
    OperationError::new("STALE_STATE", message)
}
pub(crate) fn id() -> String {
    format!(
        "{:x}-{:x}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}
pub(crate) fn read(path: &Path, limit: usize) -> Result<Vec<u8>, OperationError> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|f| f.take(limit as u64 + 1).read_to_end(&mut bytes))
        .map_err(io)?;
    if bytes.len() > limit {
        return Err(OperationError::new(
            "LIMIT_EXCEEDED",
            format!("input exceeds the {limit}-byte limit"),
        ));
    }
    Ok(bytes)
}
pub(crate) fn private_dir(path: &Path) -> Result<(), OperationError> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
            return Err(io("metadata path is not a real directory"));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(path).map_err(io)?;
        }
        Err(e) => return Err(io(e)),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(path).map_err(io)?.permissions().mode() & 0o077 != 0 {
            return Err(io(
                "metadata directory must have private permissions (0700)",
            ));
        }
    }
    Ok(())
}
pub(crate) fn atomic_write(
    path: &Path,
    bytes: &[u8],
    permissions: Option<fs::Permissions>,
) -> Result<(), OperationError> {
    let temp = path.with_file_name(format!(".margin-{}", id()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp).map_err(io)?;
        file.write_all(bytes).map_err(io)?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions).map_err(io)?;
        }
        file.sync_all().map_err(io)?;
        fs::rename(&temp, path).map_err(io)?;
        #[cfg(unix)]
        fs::File::open(path.parent().ok_or_else(|| io("missing parent"))?)
            .and_then(|f| f.sync_all())
            .map_err(io)?;
        Ok(())
    })();
    let _ = fs::remove_file(temp);
    result
}

impl Operations<'_> {
    /// Read complete source after validating the adopted document context.
    pub fn edit_document(&self, path: &str) -> Result<Version, OperationError> {
        let path = crate::domain::parse_document_path(path)
            .map_err(|e| OperationError::new("INVALID_ARGUMENT", e.to_string()))?;
        let (scope, branch, adoption) = self.editing_context(&path)?;
        let absolute = self.editing_path(&path)?;
        let bytes = read(&absolute, MAX_SOURCE_BYTES).map_err(|error| {
            if error.code == "LIMIT_EXCEEDED" { OperationError::new("LIMIT_EXCEEDED", "Built-in editing accepts at most 1 MiB; use E to open the configured external editor") }
            else { error }
        })?;
        let text = String::from_utf8(bytes)
            .map_err(|_| OperationError::new("INVALID_ARGUMENT", "source is not UTF-8"))?;
        Ok(Version {
            path,
            worktree: self
                .git()
                .toplevel()
                .map_err(OperationError::from)?
                .to_string_lossy()
                .into_owned(),
            scope,
            branch,
            adoption,
            digest: self
                .git()
                .hash_object(text.as_bytes())
                .map_err(OperationError::from)?,
            text,
        })
    }
    fn editing_path(&self, path: &str) -> Result<PathBuf, OperationError> {
        let root = self.git().toplevel().map_err(OperationError::from)?;
        let mut absolute = root.clone();
        for part in Path::new(path).components() {
            if !matches!(part, std::path::Component::Normal(_)) {
                return Err(stale("invalid document path"));
            }
            absolute.push(part);
            let metadata = fs::symlink_metadata(&absolute).map_err(io)?;
            if metadata.file_type().is_symlink() {
                return Err(stale("document path contains a symbolic link"));
            }
        }
        let metadata = fs::metadata(&absolute).map_err(io)?;
        if !metadata.is_file() || metadata.permissions().readonly() {
            return Err(stale("document is not a writable regular file"));
        }
        // Opening without truncation checks the caller's actual write access.
        OpenOptions::new().write(true).open(&absolute).map_err(io)?;
        if !fs::canonicalize(&absolute)
            .map_err(io)?
            .starts_with(fs::canonicalize(root).map_err(io)?)
        {
            return Err(stale("document leaves the working tree"));
        }
        Ok(absolute)
    }
    pub fn save_document(&self, base: &Version, text: &str) -> Result<Version, OperationError> {
        let _guard = self.acquire_mutation_lock()?;
        self.save_document_locked(base, text)
    }
    pub(crate) fn save_document_locked(
        &self,
        base: &Version,
        text: &str,
    ) -> Result<Version, OperationError> {
        if text.len() > MAX_SOURCE_BYTES {
            return Err(OperationError::new(
                "LIMIT_EXCEEDED",
                "edited source exceeds 1 MiB",
            ));
        }
        let recovery = self.editing_metadata("recovery")?;
        let record = recovery.join(format!("{}.json", id()));
        if fs::read_dir(&recovery).map_err(io)?.count() >= 32 {
            return Err(OperationError::new(
                "LIMIT_EXCEEDED",
                "32 recovery records retained; inspect and remove old records before saving",
            ));
        }
        self.store_recovery_record(&record, &serde_json::to_vec(&serde_json::json!({"schema_version": 1, "base": base, "edited": text, "observed": null})).map_err(io)?)?;
        let observed = self.edit_document(&base.path)?;
        self.store_recovery_record(&record, &serde_json::to_vec(&serde_json::json!({"schema_version": 1, "base": base, "edited": text, "observed": observed})).map_err(io)?)?;
        if observed != *base {
            return Err(stale(
                "disk or scope changed; buffer retained; compare the recovery record before retrying",
            ));
        }
        let absolute = self.editing_path(&base.path)?;
        let permissions = fs::metadata(&absolute).map_err(io)?.permissions();
        // Recheck after journaling. Unrelated writers do not honor our lock.
        if self.edit_document(&base.path)? != observed {
            return Err(stale("document changed before save; buffer retained"));
        }
        atomic_write(&absolute, text.as_bytes(), Some(permissions)).map_err(|e| {
            OperationError::new(
                "OUTCOME_UNKNOWN",
                format!(
                    "Save did not finish verification: {}; recovery record retained",
                    e.message
                ),
            )
        })?;
        let saved = self.edit_document(&base.path).map_err(|e| {
            OperationError::new(
                "OUTCOME_UNKNOWN",
                format!(
                    "Save could not be verified: {}; recovery record retained",
                    e.message
                ),
            )
        })?;
        if saved.text != text {
            return Err(OperationError::new(
                "OUTCOME_UNKNOWN",
                "file changed during save; recovery record retained",
            ));
        }
        let _ = fs::remove_file(record);
        Ok(saved)
    }
    pub(crate) fn editing_metadata(&self, child: &str) -> Result<PathBuf, OperationError> {
        let root = self
            .git()
            .git_path("margin-editing")
            .map_err(OperationError::from)?;
        private_dir(&root)?;
        let path = root.join(child);
        private_dir(&path)?;
        Ok(path)
    }
}

/// A recovery buffer never changes the working document without explicit saving.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recovery {
    pub schema_version: u32,
    pub base: Version,
    pub edited: String,
    #[serde(default)]
    pub observed: Option<Version>,
}
impl Operations<'_> {
    pub fn retain_edit_buffer(
        &self,
        id: &str,
        base: &Version,
        text: &str,
    ) -> Result<(), OperationError> {
        if !id.starts_with("draft-")
            || id.len() > 110
            || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(stale("invalid recovery ID"));
        }
        if text.len() > MAX_SOURCE_BYTES {
            return Err(OperationError::new(
                "LIMIT_EXCEEDED",
                "recovery buffer exceeds 1 MiB",
            ));
        }
        let _guard = self.acquire_mutation_lock()?;
        let dir = self.editing_metadata("recovery")?;
        let path = dir.join(format!("{id}.json"));
        if !path.exists() && fs::read_dir(&dir).map_err(io)?.count() >= 32 {
            return Err(OperationError::new(
                "LIMIT_EXCEEDED",
                "recovery storage is full; inspect retained records",
            ));
        }
        let record = Recovery {
            schema_version: 1,
            base: base.clone(),
            edited: text.into(),
            observed: None,
        };
        self.store_recovery_record(&path, &serde_json::to_vec(&record).map_err(io)?)
    }
    fn store_recovery_record(&self, path: &Path, bytes: &[u8]) -> Result<(), OperationError> {
        let mut total = bytes.len() as u64;
        let mut count = 0;
        for entry in fs::read_dir(
            path.parent()
                .ok_or_else(|| io("missing recovery directory"))?,
        )
        .map_err(io)?
        .take(33)
        {
            let entry = entry.map_err(io)?;
            count += 1;
            if entry.path() != path {
                total += entry.metadata().map_err(io)?.len();
            }
        }
        if count > 32 || total > 64 * 1024 * 1024 {
            return Err(OperationError::new(
                "LIMIT_EXCEEDED",
                "recovery storage exceeds 32 records or 64 MiB; inspect retained records",
            ));
        }
        atomic_write(path, bytes, None)
    }
    pub fn recover_edit_buffer(&self, path: &str) -> Result<Option<Recovery>, OperationError> {
        let root = self
            .git()
            .git_path("margin-editing")
            .map_err(OperationError::from)?
            .join("recovery");
        if !root.try_exists().map_err(io)? {
            return Ok(None);
        }
        let mut entries: Vec<_> = fs::read_dir(self.editing_metadata("recovery")?)
            .map_err(io)?
            .take(33)
            .collect::<Result<_, _>>()
            .map_err(io)?;
        if entries.len() > 32 {
            return Err(OperationError::new(
                "LIMIT_EXCEEDED",
                "recovery storage exceeds 32 records",
            ));
        }
        entries.sort_by_key(|e| std::cmp::Reverse(e.metadata().and_then(|m| m.modified()).ok()));
        for entry in entries {
            if entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "json")
                || !entry.file_type().map_err(io)?.is_file()
            {
                continue;
            }
            let record: Recovery =
                serde_json::from_slice(&read(&entry.path(), MAX_SOURCE_BYTES * 19)?).map_err(io)?;
            if record.base.text.len() > MAX_SOURCE_BYTES
                || record.edited.len() > MAX_SOURCE_BYTES
                || record
                    .observed
                    .as_ref()
                    .is_some_and(|v| v.text.len() > MAX_SOURCE_BYTES)
            {
                return Err(OperationError::new(
                    "LIMIT_EXCEEDED",
                    "recovery source exceeds 1 MiB",
                ));
            }
            if record.schema_version == 1
                && record.base.path == path
                && record.edited != record.base.text
                && record.edited != self.edit_document(path)?.text
            {
                return Ok(Some(record));
            }
        }
        Ok(None)
    }
    pub fn forget_edit_buffer(&self, id: &str) -> Result<(), OperationError> {
        if !id.starts_with("draft-") || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(stale("invalid recovery ID"));
        }
        let _guard = self.acquire_mutation_lock()?;
        let path = self
            .editing_metadata("recovery")?
            .join(format!("{id}.json"));
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io(e)),
        }
    }
}
