//! Bounded literal search of the listed working documents, off the input loop.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};

use super::shell::READ_VIEW_BYTES;

const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_HITS: usize = 500;
const MAX_FILES: usize = 2_000;

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub path: String,
    pub line: usize,
    pub excerpt: String,
}

#[derive(Default, Debug)]
pub struct SearchResults {
    pub hits: Vec<SearchHit>,
    pub scanned: usize,
    pub skipped: usize,
    pub truncated: usize,
    pub limited: bool,
}

#[derive(Default)]
pub struct WorkspaceSearch {
    pub query: String,
    pub results: Option<SearchResults>,
    pub selection: usize,
    pub(crate) request: Option<u64>,
}

impl WorkspaceSearch {
    pub fn searching(&self) -> bool {
        self.request.is_some()
    }
}

impl SearchResults {
    pub fn summary(&self) -> String {
        format!(
            "{} matching lines · {} files searched · {} skipped · {} truncated{}",
            self.hits.len(),
            self.scanned,
            self.skipped,
            self.truncated,
            if self.limited {
                " · search limit reached; narrow the query"
            } else {
                ""
            },
        )
    }
}

struct Job {
    id: u64,
    root: PathBuf,
    paths: Vec<String>,
    query: String,
}

pub(crate) struct SearchReply {
    pub id: u64,
    pub results: SearchResults,
}

/// One running scan and at most two waiting jobs (channel and newest request).
/// A new request cancels older work; the input loop never waits for a scan.
pub(crate) struct SearchWorker {
    jobs: Option<SyncSender<Job>>,
    replies: Receiver<SearchReply>,
    cancellation: Arc<AtomicU64>,
    queued: Option<Job>,
    handle: Option<JoinHandle<()>>,
}

impl SearchWorker {
    pub fn spawn() -> std::io::Result<Self> {
        let (sender, jobs) = mpsc::sync_channel::<Job>(1);
        let (replies, receiver) = mpsc::channel();
        let cancellation = Arc::new(AtomicU64::new(0));
        let token = cancellation.clone();
        let handle = thread::Builder::new()
            .name("margin-search".into())
            .spawn(move || {
                for job in jobs {
                    let results = scan(&job.root, &job.paths, &job.query, || {
                        token.load(Ordering::Relaxed) != job.id
                    });
                    if token.load(Ordering::Relaxed) == job.id
                        && replies
                            .send(SearchReply {
                                id: job.id,
                                results,
                            })
                            .is_err()
                    {
                        break;
                    }
                }
            })?;
        Ok(Self {
            jobs: Some(sender),
            replies: receiver,
            cancellation,
            queued: None,
            handle: Some(handle),
        })
    }

    pub fn cancel(&mut self) {
        self.cancellation.fetch_add(1, Ordering::Relaxed);
        self.queued = None;
    }

    pub fn request(&mut self, root: PathBuf, paths: Vec<String>, query: String) -> u64 {
        self.cancel();
        let id = self.cancellation.load(Ordering::Relaxed);
        self.send(Job {
            id,
            root,
            paths,
            query,
        });
        id
    }

    fn send(&mut self, job: Job) {
        if let Some(sender) = &self.jobs
            && let Err(TrySendError::Full(job)) = sender.try_send(job)
        {
            self.queued = Some(job);
        }
    }

    pub fn receive(&mut self) -> Option<SearchReply> {
        if let Some(job) = self.queued.take() {
            self.send(job);
        }
        self.replies.try_recv().ok()
    }
}

impl Drop for SearchWorker {
    fn drop(&mut self) {
        self.cancel();
        self.jobs.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn scan(root: &Path, paths: &[String], query: &str, cancelled: impl Fn() -> bool) -> SearchResults {
    let needle = query.to_lowercase();
    let mut results = SearchResults::default();
    let mut remaining = MAX_BYTES;
    if needle.is_empty() {
        return results;
    }
    for (index, path) in paths.iter().enumerate() {
        if cancelled() {
            break;
        }
        if index >= MAX_FILES || remaining == 0 {
            results.limited = true;
            break;
        }
        let limit = remaining.min(READ_VIEW_BYTES as usize);
        let Ok((bytes, truncated)) = read_text(root, path, limit) else {
            results.skipped += 1;
            continue;
        };
        remaining = remaining.saturating_sub(bytes.len());
        // A bounded read may end within a UTF-8 character. Only drop that
        // incomplete suffix; other invalid UTF-8 and binary files are skipped.
        let Some(text) = valid_text(&bytes, truncated) else {
            results.skipped += 1;
            continue;
        };
        results.scanned += 1;
        results.truncated += usize::from(truncated);
        for (line, text) in text.lines().enumerate() {
            if cancelled() {
                return results;
            }
            let lower = text.to_lowercase();
            if let Some(offset) = lower.find(&needle) {
                if results.hits.len() == MAX_HITS {
                    results.limited = true;
                    return results;
                }
                // Lowercasing can expand a source character (İ becomes i + ◌̇).
                // Map the match back before choosing its source context.
                let mut remaining = lower[..offset].chars().count();
                let column = text
                    .chars()
                    .take_while(|ch| {
                        let length = ch.to_lowercase().count();
                        if remaining < length {
                            return false;
                        }
                        remaining -= length;
                        true
                    })
                    .count();
                let start = column.saturating_sub(40);
                let excerpt: String = text.chars().skip(start).take(160).collect();
                results.hits.push(SearchHit {
                    path: path.clone(),
                    line: line + 1,
                    excerpt: format!(
                        "{}{}",
                        if start > 0 { "…" } else { "" },
                        crate::cli::display_escape(&excerpt)
                    ),
                });
            }
        }
    }
    results
}

pub(crate) fn valid_text(bytes: &[u8], truncated: bool) -> Option<&str> {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) if truncated && error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).ok()?
        }
        _ => return None,
    };
    (!text.contains('\0')).then_some(text)
}

pub(crate) fn hit_is_current(root: &Path, hit: &SearchHit, query: &str) -> bool {
    let Ok((bytes, truncated)) = read_text(root, &hit.path, READ_VIEW_BYTES as usize) else {
        return false;
    };
    valid_text(&bytes, truncated).is_some_and(|text| {
        text.lines()
            .nth(hit.line - 1)
            .is_some_and(|line| line.to_lowercase().contains(query))
    })
}

/// Only listed, repository-relative regular files; do not follow symlinks.
pub(crate) fn read_text(root: &Path, path: &str, limit: usize) -> std::io::Result<(Vec<u8>, bool)> {
    if crate::domain::parse_document_path(path).is_err() {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let mut full = root.to_path_buf();
    for component in Path::new(path).components() {
        full.push(component);
        if fs::symlink_metadata(&full)?.file_type().is_symlink() {
            return Err(std::io::ErrorKind::InvalidInput.into());
        }
    }
    if !fs::metadata(&full)?.is_file() {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let file = fs::File::open(full)?;
    let total = file.metadata()?.len();
    let mut bytes = Vec::new();
    file.take(limit as u64).read_to_end(&mut bytes)?;
    Ok((bytes, total > limit as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_is_literal_unicode_aware_and_skips_non_text_and_links() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("one.md"), "# ČAJ\nLiteral [a].* ČAJ\n").unwrap();
        fs::write(root.path().join("binary.md"), b"[a].*\0\xff").unwrap();
        std::os::unix::fs::symlink("one.md", root.path().join("link.md")).unwrap();
        let paths = vec![
            "one.md".into(),
            "binary.md".into(),
            "link.md".into(),
            "missing.md".into(),
        ];
        let results = scan(root.path(), &paths, "[a].* čaj", || false);
        assert_eq!(results.hits.len(), 1);
        assert_eq!(results.hits[0].line, 2);
        assert_eq!(results.hits[0].excerpt, "Literal [a].* ČAJ");
        assert_eq!(results.scanned, 1);
        assert_eq!(results.skipped, 3);
        assert!(!results.limited);
        assert!(hit_is_current(root.path(), &results.hits[0], "[a].* čaj"));
    }

    #[test]
    fn excerpt_keeps_the_match_after_expanding_lowercase_characters() {
        let root = tempfile::tempdir().unwrap();
        // U+0130 becomes two characters when converted to lowercase.
        let text = format!("{}TARGET trailing text", "İ".repeat(100));
        fs::write(root.path().join("one.md"), text).unwrap();
        let results = scan(root.path(), &["one.md".into()], "target", || false);
        assert_eq!(results.hits.len(), 1);
        assert_eq!(
            results.hits[0].excerpt,
            format!("…{}TARGET trailing text", "İ".repeat(40))
        );
    }

    #[test]
    fn result_and_file_bounds_are_visible_and_split_utf8_remains_searchable() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("many.md"), "match\n".repeat(MAX_HITS + 1)).unwrap();
        let results = scan(root.path(), &["many.md".into()], "match", || false);
        assert_eq!(results.hits.len(), MAX_HITS);
        assert!(results.limited);
        assert!(results.summary().contains("search limit reached"));
        let mut text = format!("target{}", "x".repeat(READ_VIEW_BYTES as usize - 7));
        text.push_str("🦀\nunsearched ending\n");
        fs::write(root.path().join("long.md"), &text).unwrap();
        let results = scan(root.path(), &["long.md".into()], "target", || false);
        assert_eq!(results.truncated, 1);
        assert_eq!(results.hits.len(), 1);
        assert!(hit_is_current(root.path(), &results.hits[0], "target"));
        let results = scan(root.path(), &["long.md".into()], "unsearched", || false);
        assert!(results.hits.is_empty());
        assert_eq!(results.truncated, 1);
        assert_eq!(
            scan(root.path(), &["many.md".into()], "match", || true).scanned,
            0
        );
    }

    #[test]
    fn rapid_requests_eventually_return_only_the_latest_query() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("one.md"), "old\nlatest\n").unwrap();
        let mut worker = SearchWorker::spawn().unwrap();
        for _ in 0..100 {
            worker.request(root.path().into(), vec!["one.md".into()], "old".into());
        }
        let latest = worker.request(root.path().into(), vec!["one.md".into()], "latest".into());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(reply) = worker.receive()
                && reply.id == latest
            {
                assert_eq!(reply.results.hits.len(), 1);
                assert_eq!(reply.results.hits[0].line, 2);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "latest query was lost"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn aggregate_budget_leaves_an_explicitly_incomplete_search() {
        let root = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        let content = "x".repeat(READ_VIEW_BYTES as usize);
        for index in 0..=MAX_BYTES / READ_VIEW_BYTES as usize {
            let path = format!("{index}.md");
            fs::write(root.path().join(&path), &content).unwrap();
            paths.push(path);
        }
        let results = scan(root.path(), &paths, "absent", || false);
        assert!(results.limited);
        assert!(results.scanned < paths.len());
        assert!(results.hits.is_empty());
        assert!(results.summary().contains("search limit reached"));
    }
}
