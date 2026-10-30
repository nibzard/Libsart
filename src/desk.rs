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
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
const LIMIT: u64 = 8192;
/// How long the server thread waits for the viewer's own loop to answer
/// one forwarded request. The viewer's socket exists before its event
/// loop drains requests, so the wait must bridge viewer startup on slow
/// shared machines; the client's own five-second cap bounds it in turn.
const WAIT: Duration = Duration::from_secs(4);
/// The reply when the viewer's own loop does not answer within `WAIT`.
const UNANSWERED: &str = "viewer busy or unresponsive; retry after closing the editor or dialog";
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Navigation only: no repository mutation or terminal selection.
pub struct Request {
    pub path: String,
    pub line: Option<u32>,
}
/// Versioned control operations. Queue content stays in repository metadata.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Control {
    Open { path: String, line: Option<u32> },
    Status,
    Context,
    Notify,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub schema_version: u32,
    pub request: Control,
}
#[derive(Debug, Serialize, Deserialize)]
/// A successful reply follows a completed terminal draw.
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub displayed: bool,
    pub error: Option<String>,
}
fn packet<T: serde::de::DeserializeOwned>(
    stream: &mut UnixStream,
    timeout: Duration,
) -> io::Result<T> {
    let deadline = Instant::now() + timeout;
    stream.set_nonblocking(true)?;
    let mut bytes = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "desk message timed out",
            ));
        }
        let mut buffer = [0; 1024];
        let count = match stream.read(&mut buffer) {
            Ok(count) => count,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(e) => return Err(e),
        };
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.len() as u64 > LIMIT {
            return Err(io::Error::other("desk message too large"));
        }
    }
    stream.set_nonblocking(false)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}
/// Reserve a fresh short Unix-socket directory, private to this user.
pub fn directory() -> io::Result<PathBuf> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    for n in 0..1000 {
        let dir =
            PathBuf::from("/tmp").join(format!("margin-{}-{nonce:x}-{n}", std::process::id()));
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("cannot allocate a private desk directory"))
}
use std::os::unix::fs::DirBuilderExt;
/// Send one request with startup retries; success requires the drawn-frame reply.
pub fn open_at(
    path: &std::path::Path,
    request: Request,
    timeout: Duration,
) -> Result<(), OperationError> {
    let fail = |message: String| OperationError::new("OPERATION_UNAVAILABLE", message);
    let deadline = Instant::now() + timeout.min(Duration::from_secs(5));
    let mut stream = loop {
        match UnixStream::connect(path) {
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
    let reply: Reply = packet(&mut stream, remaining)
        .map_err(|e| fail(format!("desk viewer did not acknowledge opening: {e}")))?;
    match (reply.displayed, reply.error) {
        (true, None) => Ok(()),
        (_, Some(e)) => Err(OperationError::new("OPERATION_UNAVAILABLE", e)),
        _ => Err(fail(
            "desk viewer did not confirm that the document was displayed".into(),
        )),
    }
}
/// A queued request is valid only until its deadline.
pub struct Pending {
    pub request: Request,
    pub reply: mpsc::Sender<Reply>,
    pub deadline: Instant,
}
/// The viewer owns the listener, worker, and socket cleanup.
pub struct Server {
    pub requests: mpsc::Receiver<Pending>,
    context: Arc<Mutex<serde_json::Value>>,
    busy: Arc<AtomicBool>,
    notified: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    path: PathBuf,
}
impl Server {
    pub fn set_context(&self, value: serde_json::Value) {
        let bytes = serde_json::to_vec(&value).unwrap();
        *self.context.lock().unwrap() = if bytes.len() as u64 <= LIMIT {
            value
        } else {
            serde_json::json!({"error":"viewer context exceeds the 8 KiB bound"})
        };
    }

    pub fn take_notification(&self) -> bool {
        self.notified.swap(false, Ordering::AcqRel)
    }
    pub fn set_busy(&self, busy: bool) {
        self.busy.store(busy, Ordering::Release);
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
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::symlink_metadata(parent)?;
        let probe = directory()?;
        let uid = fs::metadata(&probe)?.uid();
        let _ = fs::remove_dir(probe);
        if !metadata.is_dir() || metadata.uid() != uid || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(io::Error::other("desk socket directory must be private"));
        }
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        let (tx, requests) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let busy = Arc::new(AtomicBool::new(false));
        let b = busy.clone();
        let notified = Arc::new(AtomicBool::new(false));
        let notification = notified.clone();
        let context = Arc::new(Mutex::new(serde_json::Value::Null));
        let published_context = context.clone();
        let thread = thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(20));
                    continue;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
                let decoded = packet::<serde_json::Value>(&mut stream, Duration::from_millis(500));
                let decoded = decoded.and_then(|value| {
                    if value.get("schema_version").is_none() { return serde_json::from_value::<Request>(value).map(Some).map_err(io::Error::other); }
                    let envelope: Envelope = serde_json::from_value(value).map_err(io::Error::other)?;
                    if envelope.schema_version != 1 { return Err(io::Error::other("unsupported desk schema version")); }
                    match envelope.request {
                        Control::Open { path, line } => Ok(Some(Request { path, line })),
                        Control::Context => {
                            let value = published_context.lock().unwrap().clone();
                            stream.write_all(&serde_json::to_vec(&value).unwrap())?;
                            Ok(None)
                        }
                        Control::Status | Control::Notify => {
                            let notify = matches!(envelope.request, Control::Notify);
                            if notify { notification.store(true, Ordering::Release); }
                            let response = serde_json::json!({"schema_version": 1, "busy": b.load(Ordering::Acquire), "notified": notify});
                            stream.write_all(&serde_json::to_vec(&response).unwrap())?;
                            Ok(None)
                        }
                    }
                });
                let reply = match decoded {
                    Ok(None) => continue,
                    Err(error) => Reply {
                        displayed: false,
                        error: Some(format!("invalid or oversized desk request: {error}")),
                    },
                    Ok(_) if b.load(Ordering::Acquire) => Reply {
                        displayed: false,
                        error: Some("viewer busy; finish the editor or dialog and retry".into()),
                    },
                    Ok(Some(request)) => {
                        let (reply, rx) = mpsc::channel();
                        let pending = Pending {
                            request,
                            reply,
                            deadline: Instant::now() + WAIT,
                        };
                        if tx.try_send(pending).is_err() {
                            Reply {
                                displayed: false,
                                error: Some("viewer busy".into()),
                            }
                        } else {
                            rx.recv_timeout(WAIT).unwrap_or(Reply {
                                displayed: false,
                                error: Some(UNANSWERED.into()),
                            })
                        }
                    }
                };
                let _ = stream.write_all(&serde_json::to_vec(&reply).unwrap());
            }
        });
        Ok(Self {
            context,
            requests,
            busy,
            notified,
            stop,
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

/// An explicit desk identity travels with a link, independently of the
/// terminal handler's environment. Hex keeps paths lossless and shell inert.
pub fn link(socket: &std::path::Path, request: &Request) -> Result<String, OperationError> {
    use std::os::unix::ffi::OsStrExt;
    crate::domain::parse_document_path(&request.path)
        .map_err(|e| OperationError::new("INVALID_ARGUMENT", e.to_string()))?;
    if request.line == Some(0) {
        return Err(OperationError::new(
            "INVALID_ARGUMENT",
            "line must be positive",
        ));
    }
    if serde_json::to_vec(request)
        .map_err(|e| OperationError::new("INVALID_ARGUMENT", e.to_string()))?
        .len() as u64
        > LIMIT
    {
        return Err(OperationError::new(
            "INVALID_ARGUMENT",
            "open request is too large",
        ));
    }
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    Ok(format!(
        "margin://open/{}/{}/{}",
        hex(socket.as_os_str().as_bytes()),
        hex(request.path.as_bytes()),
        request.line.unwrap_or(0)
    ))
}
pub fn parse_link(url: &str) -> Result<(PathBuf, Request), OperationError> {
    use std::os::unix::ffi::OsStringExt;
    let fail = || OperationError::new("INVALID_ARGUMENT", "invalid Margin desk link");
    if url.len() > LIMIT as usize * 2 {
        return Err(fail());
    }
    let mut parts = url
        .strip_prefix("margin://open/")
        .ok_or_else(fail)?
        .split('/');
    let decode = |text: &str| -> Result<Vec<u8>, OperationError> {
        if !text.len().is_multiple_of(2) || !text.is_ascii() {
            return Err(fail());
        }
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(|_| fail()))
            .collect()
    };
    let socket = PathBuf::from(std::ffi::OsString::from_vec(decode(
        parts.next().ok_or_else(fail)?,
    )?));
    let path = String::from_utf8(decode(parts.next().ok_or_else(fail)?)?).map_err(|_| fail())?;
    let line: u32 = parts.next().ok_or_else(fail)?.parse().map_err(|_| fail())?;
    if parts.next().is_some() || !socket.is_absolute() {
        return Err(fail());
    }
    crate::domain::parse_document_path(&path).map_err(|_| fail())?;
    Ok((
        socket,
        Request {
            path,
            line: (line != 0).then_some(line),
        },
    ))
}

/// Send a bounded versioned status or notification operation.
pub fn control_at(
    path: &std::path::Path,
    request: Control,
) -> Result<serde_json::Value, OperationError> {
    let fail = |e: io::Error| OperationError::new("OPERATION_UNAVAILABLE", e.to_string());
    let mut stream = UnixStream::connect(path).map_err(fail)?;
    stream
        .set_write_timeout(Some(Duration::from_millis(200)))
        .map_err(fail)?;
    stream
        .write_all(
            &serde_json::to_vec(&Envelope {
                schema_version: 1,
                request,
            })
            .unwrap(),
        )
        .map_err(fail)?;
    stream.shutdown(std::net::Shutdown::Write).map_err(fail)?;
    packet(&mut stream, Duration::from_millis(250)).map_err(fail)
}
