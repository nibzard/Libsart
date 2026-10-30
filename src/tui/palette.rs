//! Deterministic metadata matching. Body search remains a separate facility.
use super::{actions::Action, search, wrap};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;

pub const QUERY_LIMIT: usize = 256;
const ENTRY_LIMIT: usize = 20_000;
const RESULT_LIMIT: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Action,
    Document,
    Heading,
    Request,
}
impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Action => "Action",
            Self::Document => "Document",
            Self::Heading => "Heading",
            Self::Request => "Request",
        }
    }
}
#[derive(Clone, Debug)]
pub struct Entry {
    pub id: String,
    pub kind: Kind,
    pub action: Option<Action>,
    pub label: String,
    pub path: String,
    pub context: String,
    pub line: u32,
    pub fingerprint: u64,
}
#[derive(Clone, Debug)]
pub struct Hit {
    pub entry: Entry,
    pub spans: Vec<Range<usize>>,
    rank: u8,
}
#[derive(Default, Debug)]
pub struct Metadata {
    pub entries: Vec<Entry>,
    pub scanned: usize,
    pub skipped: usize,
    pub truncated: usize,
    pub limited: bool,
}
impl Metadata {
    pub fn summary(&self) -> String {
        format!(
            "{} files indexed · {} skipped · {} truncated{}",
            self.scanned,
            self.skipped,
            self.truncated,
            if self.limited {
                " · metadata limits reached; additional content omitted"
            } else {
                ""
            }
        )
    }
}
#[derive(Default)]
pub struct Palette {
    pub query: String,
    pub matched_query: String,
    pub hits: Vec<Hit>,
    pub additional: usize,
    pub metadata: Option<Arc<Metadata>>,
    pub pending: Option<u64>,
    pub edited: Option<Instant>,
    pub choose_reference: bool,
    pub requests: Vec<Entry>,
    pub suggestions: Vec<Action>,
}
impl Palette {
    pub fn ready(&self) -> bool {
        self.pending.is_none() && self.edited.is_none() && self.query == self.matched_query
    }
}

pub fn actions() -> Vec<Entry> {
    Action::ALL
        .into_iter()
        .enumerate()
        .map(|(i, action)| Entry {
            id: format!("action:{i:03}"),
            kind: Kind::Action,
            action: Some(action),
            label: action.name().into(),
            path: String::new(),
            context: action.aliases().into(),
            line: 0,
            fingerprint: 0,
        })
        .collect()
}

/// Each lowercased scalar retains the byte range of its original scalar;
/// Unicode expansion (e.g. İ) cannot produce invalid highlighting boundaries.
fn folded(text: &str) -> (Vec<char>, Vec<Range<usize>>) {
    let mut chars = Vec::new();
    let mut spans = Vec::new();
    for (i, c) in text.char_indices() {
        for lower in c.to_lowercase() {
            chars.push(lower);
            spans.push(i..i + c.len_utf8());
        }
    }
    (chars, spans)
}
fn score(text: &str, needle: &[char]) -> Option<(u8, Vec<Range<usize>>)> {
    let (chars, spans) = folded(text);
    if chars == needle {
        return Some((0, spans));
    }
    if chars.starts_with(needle) {
        return Some((1, spans[..needle.len()].to_vec()));
    }
    let tokens: Vec<_> = needle
        .split(|c| c.is_whitespace())
        .filter(|s| !s.is_empty())
        .collect();
    if tokens.is_empty() {
        return None;
    }
    let matches: Option<Vec<usize>> = tokens
        .iter()
        .map(|token| chars.windows(token.len()).position(|w| w == *token))
        .collect();
    if let Some(matches) = matches {
        let ranges = matches
            .into_iter()
            .zip(tokens)
            .flat_map(|(i, t)| spans[i..i + t.len()].to_vec())
            .collect();
        return Some((2, ranges));
    }
    // Bound subsequence work by the bounded labels/context and query. Limit
    // gaps to 32 scalars so arbitrary dispersed letters do not swamp results.
    let mut found = Vec::new();
    let mut at = 0;
    for c in needle.iter().filter(|c| !c.is_whitespace()) {
        let i = chars.iter().skip(at).take(33).position(|x| x == c)? + at;
        found.push(spans[i].clone());
        at = i + 1;
    }
    (!found.is_empty()).then_some((3, found))
}

pub fn matching(query: &str, entries: impl IntoIterator<Item = Entry>) -> (Vec<Hit>, usize) {
    let needle = folded(query.trim()).0;
    let mut hits = Vec::new();
    for entry in entries {
        if needle.is_empty() {
            if entry.kind == Kind::Action {
                hits.push(Hit {
                    entry,
                    rank: 0,
                    spans: vec![],
                });
            }
            continue;
        }
        let label = score(&entry.label, &needle);
        let context = score(
            &format!("{} {} {}", entry.label, entry.path, entry.context),
            &needle,
        )
        .map(|(r, _)| (r + 4, vec![]));
        if let Some((rank, spans)) = label.or(context) {
            hits.push(Hit { entry, rank, spans });
        }
    }
    hits.sort_by(|a, b| {
        a.rank
            .cmp(&b.rank)
            .then_with(|| a.entry.kind.cmp(&b.entry.kind))
            .then_with(|| a.entry.path.cmp(&b.entry.path))
            .then_with(|| a.entry.line.cmp(&b.entry.line))
            .then_with(|| a.entry.id.cmp(&b.entry.id))
    });
    let additional = hits.len().saturating_sub(RESULT_LIMIT);
    hits.truncate(RESULT_LIMIT);
    (hits, additional)
}

pub fn fingerprint(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hash);
    hash.finish()
}
fn bounded(s: &str) -> String {
    s.chars().take(512).collect()
}
pub fn metadata(root: &Path, paths: &[String], cancelled: impl Fn() -> bool) -> Metadata {
    let mut result = Metadata::default();
    let mut remaining = 16 * 1024 * 1024;
    for (index, path) in paths.iter().enumerate() {
        if cancelled() {
            break;
        }
        if index >= 2000 || remaining == 0 || result.entries.len() >= ENTRY_LIMIT {
            result.limited = true;
            break;
        }
        let Ok((bytes, truncated)) = search::read_text(root, path, remaining.min(256 * 1024))
        else {
            result.skipped += 1;
            continue;
        };
        remaining = remaining.saturating_sub(bytes.len());
        let Some(text) = search::valid_text(&bytes, truncated) else {
            result.skipped += 1;
            continue;
        };
        let fingerprint = fingerprint(&bytes);
        let headings = wrap::document_headings(text);
        let title = bounded(headings.first().map_or(path.as_str(), |h| &h.title));
        let mut metadata_truncated = headings.iter().any(|h| h.title.chars().count() > 512);
        result.entries.push(Entry {
            id: format!("doc:{path}"),
            kind: Kind::Document,
            action: None,
            label: title.clone(),
            path: path.clone(),
            context: String::new(),
            line: 1,
            fingerprint,
        });
        let mut ancestors: Vec<(usize, String)> = vec![];
        for h in headings {
            if result.entries.len() == ENTRY_LIMIT {
                result.limited = true;
                break;
            }
            while ancestors.last().is_some_and(|(level, _)| *level >= h.level) {
                ancestors.pop();
            }
            let context = format!(
                "{title} · {}",
                ancestors
                    .iter()
                    .map(|(_, s)| s.as_str())
                    .collect::<Vec<_>>()
                    .join(" / ")
            );
            metadata_truncated |= context.chars().count() > 512;
            result.entries.push(Entry {
                id: format!("heading:{path}:{}", h.line),
                kind: Kind::Heading,
                action: None,
                label: bounded(&h.title),
                path: path.clone(),
                context: bounded(&format!(
                    "{title} · {}",
                    ancestors
                        .iter()
                        .map(|(_, s)| s.as_str())
                        .collect::<Vec<_>>()
                        .join(" / ")
                )),
                line: h.line,
                fingerprint,
            });
            ancestors.push((h.level, bounded(&h.title)));
        }
        result.scanned += 1;
        result.truncated += usize::from(truncated || metadata_truncated);
    }
    result
}

pub struct Reply {
    pub id: u64,
    pub query: String,
    pub metadata: Arc<Metadata>,
    pub hits: Vec<Hit>,
    pub additional: usize,
}
struct Job {
    id: u64,
    root: PathBuf,
    paths: Vec<String>,
    query: String,
    metadata: Option<Arc<Metadata>>,
    choose: bool,
}
#[derive(Default)]
struct State {
    pending: Option<Job>,
    reply: Option<Reply>,
    stop: bool,
}
pub struct Worker {
    shared: Arc<(Mutex<State>, Condvar)>,
    generation: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}
impl Worker {
    pub fn spawn() -> std::io::Result<Self> {
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let copy = shared.clone();
        let generation = Arc::new(AtomicU64::new(0));
        let token = generation.clone();
        let thread = thread::Builder::new()
            .name("margin-palette".into())
            .spawn(move || {
                let (mutex, wake) = &*copy;
                loop {
                    let mut state = mutex.lock().unwrap();
                    while state.pending.is_none() && !state.stop {
                        state = wake.wait(state).unwrap();
                    }
                    let Some(job) = state.pending.take() else {
                        break;
                    };
                    drop(state);
                    let metadata = job.metadata.unwrap_or_else(|| {
                        Arc::new(metadata(&job.root, &job.paths, || {
                            token.load(Ordering::Relaxed) != job.id
                        }))
                    });
                    if token.load(Ordering::Relaxed) != job.id {
                        continue;
                    }
                    let mut entries = if job.choose { vec![] } else { actions() };
                    entries.extend(metadata.entries.iter().cloned());
                    let (hits, additional) = if job.choose && job.query.trim().is_empty() {
                        let additional = entries.len().saturating_sub(RESULT_LIMIT);
                        (
                            entries
                                .into_iter()
                                .take(RESULT_LIMIT)
                                .map(|entry| Hit {
                                    entry,
                                    spans: vec![],
                                    rank: 0,
                                })
                                .collect(),
                            additional,
                        )
                    } else {
                        matching(&job.query, entries)
                    };
                    let mut state = mutex.lock().unwrap();
                    if token.load(Ordering::Relaxed) == job.id {
                        state.reply = Some(Reply {
                            id: job.id,
                            query: job.query,
                            metadata,
                            hits,
                            additional,
                        });
                    }
                }
            })?;
        Ok(Self {
            shared,
            generation,
            thread: Some(thread),
        })
    }
    pub fn request(
        &self,
        root: PathBuf,
        paths: Vec<String>,
        query: String,
        metadata: Option<Arc<Metadata>>,
        choose: bool,
    ) -> u64 {
        let id = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let (mutex, wake) = &*self.shared;
        mutex.lock().unwrap().pending = Some(Job {
            id,
            root,
            paths,
            query,
            metadata,
            choose,
        });
        wake.notify_all();
        id
    }
    pub fn receive(&self) -> Option<Reply> {
        self.shared.0.lock().unwrap().reply.take()
    }
    pub fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        let mut state = self.shared.0.lock().unwrap();
        state.pending = None;
        state.reply = None;
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel();
        {
            let (mut state, wake) = (self.shared.0.lock().unwrap(), &self.shared.1);
            state.stop = true;
            wake.notify_all();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(id: &str, label: &str, path: &str, kind: Kind, line: u32) -> Entry {
        Entry {
            id: id.into(),
            label: label.into(),
            path: path.into(),
            kind,
            line,
            action: None,
            context: "Ancestor Context".into(),
            fingerprint: 0,
        }
    }
    #[test]
    fn rankings_ties_unicode_and_original_spans_are_deterministic() {
        let entries = vec![
            entry("path", "Other", "target.md", Kind::Document, 1),
            entry("sub", "t a r g e t", "z.md", Kind::Heading, 5),
            entry("token", "A target passage", "a.md", Kind::Heading, 2),
            entry("prefix", "Target passage", "a.md", Kind::Heading, 1),
            entry("exact", "TARGET", "b.md", Kind::Document, 1),
            entry("exact-heading", "target", "a.md", Kind::Heading, 7),
        ];
        let (hits, _) = matching("target", entries.clone());
        assert_eq!(
            hits.iter().map(|h| h.entry.id.as_str()).collect::<Vec<_>>(),
            vec!["exact", "exact-heading", "prefix", "token", "sub", "path"]
        );
        assert_eq!(
            matching("target", entries)
                .0
                .iter()
                .map(|h| h.entry.id.clone())
                .collect::<Vec<_>>(),
            hits.iter().map(|h| h.entry.id.clone()).collect::<Vec<_>>()
        );
        let (hits, _) = matching(
            "i\u{307}",
            vec![entry("unicode", "İSTANBUL", "a.md", Kind::Heading, 1)],
        );
        assert_eq!(hits.len(), 1);
        for range in &hits[0].spans {
            assert_eq!(&hits[0].entry.label[range.clone()], "İ");
        }
        assert_eq!(
            matching(
                "context a.md",
                vec![entry("mixed", "Label", "a.md", Kind::Heading, 2)]
            )
            .0
            .len(),
            1
        );
    }
    #[test]
    fn bounds_and_unsafe_content_are_reported_without_hiding_actions() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.md"), "# Duplicate\n\n## Target\n").unwrap();
        std::fs::write(root.path().join("b.md"), "# Duplicate\n\n## Target\n").unwrap();
        std::fs::write(root.path().join("binary.md"), b"\0").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("a.md", root.path().join("link.md")).unwrap();
        let metadata = metadata(
            root.path(),
            &[
                "b.md".into(),
                "a.md".into(),
                "binary.md".into(),
                "link.md".into(),
            ],
            || false,
        );
        assert_eq!(metadata.scanned, 2);
        assert_eq!(metadata.skipped, 2);
        let hits = matching("target", metadata.entries).0;
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].entry.path, "a.md");
        let entries = (0..150)
            .map(|i| entry(&i.to_string(), "Target", "a.md", Kind::Heading, i))
            .collect::<Vec<_>>();
        let (hits, additional) = matching("target", entries);
        assert_eq!(hits.len(), 100);
        assert_eq!(additional, 50);
        assert_eq!(matching("", actions()).0.len(), Action::ALL.len());
        std::fs::write(
            root.path().join("long.md"),
            format!("# Long\n{}", "x".repeat(256 * 1024)),
        )
        .unwrap();
        assert_eq!(
            super::metadata(root.path(), &["long.md".into()], || false).truncated,
            1
        );
    }
}
