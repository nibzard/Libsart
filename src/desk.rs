//! Bounded, private per-desk viewer control. No terminal input is synthesized.
use crate::app::OperationError;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Read, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
const LIMIT: u64 = 8192;
const WAIT: Duration = Duration::from_secs(2);
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub path: String,
    pub line: Option<u32>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Reply {
    pub error: Option<String>,
}
fn packet<T: serde::de::DeserializeOwned>(stream: &mut UnixStream) -> io::Result<T> {
    let mut bytes = Vec::new();
    let deadline = Instant::now() + WAIT;
    loop {
        if Instant::now() >= deadline {
            return Err(io::Error::other("desk message timed out"));
        }
        let mut chunk = [0; 1024];
        let count = stream.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.len() as u64 > LIMIT {
            return Err(io::Error::other("desk message too large"));
        }
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}
pub fn directory() -> io::Result<PathBuf> {
    for n in 0..1000 {
        let dir = PathBuf::from("/tmp").join(format!("margin-{}-{n}", std::process::id()));
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("cannot allocate a private desk directory"))
}
use std::os::unix::fs::DirBuilderExt;
pub fn open(request: Request, timeout: Duration) -> Result<(), OperationError> {
    let fail = |message: String| OperationError::new("OPERATION_UNAVAILABLE", message);
    let path = std::env::var_os("MARGIN_DESK_SOCKET").ok_or_else(|| {
        fail("no desk viewer; run this command from the agent pane of margin desk".into())
    })?;
    let deadline = Instant::now() + timeout.min(Duration::from_secs(5));
    let mut stream = loop {
        match UnixStream::connect(&path) {
            Ok(s) => break s,
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Err(_) => {
                return Err(fail(
                    "desk viewer is closed or unavailable; start a new margin desk".into(),
                ));
            }
        }
    };
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(1));
    stream
        .set_read_timeout(Some(remaining))
        .map_err(|e| fail(e.to_string()))?;
    stream
        .set_write_timeout(Some(remaining))
        .map_err(|e| fail(e.to_string()))?;
    let bytes = serde_json::to_vec(&request).unwrap();
    if bytes.len() as u64 > LIMIT {
        return Err(OperationError::new(
            "INVALID_ARGUMENT",
            "open request is too large",
        ));
    }
    stream
        .write_all(&bytes)
        .and_then(|_| stream.shutdown(std::net::Shutdown::Write))
        .map_err(|e| fail(e.to_string()))?;
    let reply: Reply = packet(&mut stream)
        .map_err(|e| fail(format!("desk viewer did not acknowledge opening: {e}")))?;
    match reply.error {
        Some(e) => Err(OperationError::new("BUSY", e)),
        None => Ok(()),
    }
}
pub struct Pending {
    pub request: Request,
    pub reply: mpsc::Sender<Reply>,
    pub deadline: Instant,
}
pub struct Server {
    pub requests: mpsc::Receiver<Pending>,
    stop: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    path: PathBuf,
}
impl Server {
    pub fn set_busy(&self, busy: bool) {
        self.busy.store(busy, Ordering::Relaxed);
    }
    pub fn start() -> io::Result<Option<Self>> {
        let Some(path) = std::env::var_os("MARGIN_DESK_SOCKET").map(PathBuf::from) else {
            return Ok(None);
        };
        Self::bind(path).map(Some)
    }
    pub fn bind(path: PathBuf) -> io::Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("invalid desk socket"))?;
        if fs::metadata(parent)?.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::other("desk socket directory must be private"));
        }
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        let (tx, requests) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let busy = Arc::new(AtomicBool::new(false));
        let is_busy = busy.clone();
        let thread = thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(20));
                    continue;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
                let reply = match packet::<Request>(&mut stream) {
                    Err(_) => Reply {
                        error: Some("invalid or oversized desk request".into()),
                    },
                    Ok(_) if is_busy.load(Ordering::Relaxed) => Reply {
                        error: Some("viewer busy in external editor; retry after it closes".into()),
                    },
                    Ok(request) => {
                        let (reply, rx) = mpsc::channel();
                        let pending = Pending {
                            request,
                            reply,
                            deadline: Instant::now() + WAIT,
                        };
                        if tx.try_send(pending).is_err() {
                            Reply {
                                error: Some("viewer busy".into()),
                            }
                        } else {
                            rx.recv_timeout(WAIT).unwrap_or(Reply { error: Some("viewer busy or unresponsive; retry after closing the editor or dialog".into()) })
                        }
                    }
                };
                let _ = stream.write_all(&serde_json::to_vec(&reply).unwrap());
            }
        });
        Ok(Self {
            requests,
            stop,
            busy,
            thread: Some(thread),
            path,
        })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = fs::remove_file(&self.path);
        if let Some(parent) = self.path.parent() {
            let _ = fs::remove_dir(parent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_packets_and_unicode_paths() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        a.write_all(br#"{"path":"notes/a b.md","line":42}"#)
            .unwrap();
        a.shutdown(std::net::Shutdown::Write).unwrap();
        let request: Request = packet(&mut b).unwrap();
        assert_eq!(request.path, "notes/a b.md");
        assert_eq!(request.line, Some(42));
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let writer = thread::spawn(move || {
            let _ = a.write_all(&vec![b'x'; LIMIT as usize + 1]);
            let _ = a.shutdown(std::net::Shutdown::Write);
        });
        assert!(packet::<Request>(&mut b).is_err());
        drop(b);
        writer.join().unwrap();
        let (mut a, mut b) = UnixStream::pair().unwrap();
        a.write_all("{\"path\":\"résumé 文.md\",\"line\":null}".as_bytes())
            .unwrap();
        a.shutdown(std::net::Shutdown::Write).unwrap();
        assert_eq!(packet::<Request>(&mut b).unwrap().path, "résumé 文.md");
    }
    #[test]
    fn private_directories_are_distinct() {
        let a = directory().unwrap();
        let b = directory().unwrap();
        assert_ne!(a, b);
        assert_eq!(
            fs::metadata(&a).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::remove_dir(a).unwrap();
        fs::remove_dir(b).unwrap();
    }
}
