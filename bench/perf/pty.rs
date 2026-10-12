//! The pseudo-terminal driver of the P-OPEN, P-KEY, and P-REFRESH
//! measurements (SPEC 17.3-17.4).
//!
//! The PTY metrics time real terminal interaction, so the harness needs
//! what a real terminal gives: a controlling terminal, a window size, and
//! bytes the surface flushes. The driver calls only the C library the
//! standard library already links — the same POSIX set the terminal
//! lifecycle tests use — so no dependency enters the tree. It is
//! self-contained on purpose: the tests' support module belongs to the
//! product suite, and the harness must stay share-nothing with the tests.
//!
//! Timing follows the SPEC boundaries: the P-OPEN clock starts
//! immediately before the child is spawned and stops when the first frame
//! that shows identity, content, save state, and actions has flushed; the
//! P-KEY clock starts immediately before the key bytes are written and
//! stops when the answering bytes arrive. `poll(2)` wakes the reader the
//! moment bytes land, so the measured latency carries no polling sleep of
//! the harness's own making.
//!
//! Every session runs the surface with `--reduced-updates`: the loop then
//! draws only when frame-visible state changed, so "bytes arrived" means
//! "the surface answered", which is exactly what P-KEY's boundary needs.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

// The C interface, as the two supported Unix platforms define it. The
// constants and the `poll` count type differ per platform; the calls are
// the POSIX set.
#[cfg(target_os = "linux")]
mod ffi {
    pub const O_NOCTTY: i32 = 0o400;
    pub const O_RDWR: i32 = 0o2;
    pub const O_CLOEXEC: i32 = 0o2000000;
    pub const O_NONBLOCK: i32 = 0o4000;
    pub const F_GETFL: i32 = 3;
    pub const F_SETFL: i32 = 4;
    pub const TIOCSWINSZ: u64 = 0x5414;
    pub const TIOCSCTTY: u64 = 0x540E;
    pub const POLLIN: i16 = 0x001;
    /// `nfds_t` on this platform.
    pub type Nfds = u64;
}

#[cfg(target_os = "macos")]
mod ffi {
    pub const O_NOCTTY: i32 = 0x20000;
    pub const O_RDWR: i32 = 0o2;
    pub const O_CLOEXEC: i32 = 0o1000000;
    pub const O_NONBLOCK: i32 = 0o4_0000;
    pub const F_GETFL: i32 = 3;
    pub const F_SETFL: i32 = 4;
    pub const TIOCSWINSZ: u64 = 0x8008_7467;
    pub const TIOCSCTTY: u64 = 0x2000_7461;
    pub const POLLIN: i16 = 0x001;
    /// `nfds_t` on this platform.
    pub type Nfds = u32;
}

#[allow(non_camel_case_types)]
type c_char = i8;

/// One `poll(2)` request.
#[repr(C)]
struct PollFd {
    fd: RawFd,
    events: i16,
    revents: i16,
}

unsafe extern "C" {
    fn posix_openpt(flags: i32) -> RawFd;
    fn grantpt(fd: RawFd) -> i32;
    fn unlockpt(fd: RawFd) -> i32;
    fn ptsname(fd: RawFd) -> *const c_char;
    fn ioctl(fd: RawFd, request: u64, ...) -> i32;
    fn fcntl(fd: RawFd, command: i32, arg: i32) -> i32;
    fn poll(fds: *mut PollFd, nfds: ffi::Nfds, timeout: i32) -> i32;
    fn setsid() -> i32;
    fn open(path: *const c_char, flags: i32, ...) -> RawFd;
    fn close(fd: RawFd) -> i32;
}

/// The window-size form of one ioctl.
unsafe fn ioctl_winsize(fd: RawFd, request: u64, arg: *mut Winsize) -> i32 {
    // SAFETY: the caller owns the descriptor and the structure.
    unsafe { ioctl(fd, request, arg) }
}

/// The integer-argument form of one ioctl.
unsafe fn ioctl_value(fd: RawFd, request: u64, arg: isize) -> i32 {
    // SAFETY: the caller owns the descriptor.
    unsafe { ioctl(fd, request, arg) }
}

/// The window size of a terminal, as `TIOCSWINSZ` carries it.
#[repr(C)]
struct Winsize {
    rows: u16,
    columns: u16,
    xpixel: u16,
    ypixel: u16,
}

/// The window every measured session runs at: wide enough that the footer
/// shows its full text and the document list stands beside the reading
/// column, and tall enough that no layout path sees a zero dimension. A
/// zero-sized window would push the surface into its minimum-size frame,
/// which measures the wrong thing entirely.
pub const SESSION_COLUMNS: u16 = 120;
/// The row count of every measured session.
pub const SESSION_ROWS: u16 = 40;

/// The environment value every session sets, so the surface renders the
/// escape sequences a color terminal renders.
const TERM: &str = "xterm-256color";

/// One measured surface process on its own pseudo-terminal.
pub struct PtySession {
    master: File,
    /// Kept open so the slave never reaches end-of-file from this side.
    slave: File,
    child: Child,
    output: Vec<u8>,
}

/// What one bounded wait decided.
enum Wait {
    /// Bytes arrived; the elapsed time since the caller's start.
    Answered(Duration),
    /// The limit passed with no bytes.
    TimedOut(Duration),
}

impl PtySession {
    /// Spawns `executable` on a fresh pseudo-terminal of the fixed
    /// session size, as a session leader whose controlling terminal is
    /// the slave, with the fixture environment plus `TERM`.
    ///
    /// The child runs with `cwd` as its working directory and the
    /// executable path as `argv[0]`, exactly as a writer's shell would
    /// launch it. The master is non-blocking and drained through
    /// `poll(2)`, so a silent surface never blocks the harness.
    pub fn spawn(
        executable: &Path,
        args: &[&str],
        envs: &BTreeMap<OsString, OsString>,
        cwd: &Path,
    ) -> io::Result<PtySession> {
        // SAFETY: every call below is a plain C call on descriptors this
        // function owns; nothing crosses a boundary that Rust unwinds.
        unsafe {
            let master = posix_openpt(ffi::O_RDWR | ffi::O_NOCTTY | ffi::O_CLOEXEC);
            if master < 0 {
                return Err(io::Error::last_os_error());
            }
            if grantpt(master) != 0 || unlockpt(master) != 0 {
                return Err(io::Error::last_os_error());
            }
            let name = ptsname(master);
            if name.is_null() {
                return Err(io::Error::last_os_error());
            }
            let slave_path = PathBuf::from(OsStr::from_encoded_bytes_unchecked(
                CStr::from_ptr(name).to_bytes(),
            ));
            let slave = open_file(&slave_path, ffi::O_RDWR | ffi::O_NOCTTY | ffi::O_CLOEXEC)?;
            // A zero-sized window is not a window: set a real size before
            // the child can ask for one.
            set_window(master, SESSION_COLUMNS, SESSION_ROWS);
            let flags = fcntl(master, ffi::F_GETFL, 0);
            fcntl(master, ffi::F_SETFL, flags | ffi::O_NONBLOCK);

            let mut command = Command::new(executable);
            command.args(args).current_dir(cwd).env_clear().envs(envs);
            command.env("TERM", TERM);
            command
                .stdin(Stdio::from(slave.try_clone()?))
                .stdout(Stdio::from(slave.try_clone()?))
                .stderr(Stdio::from(slave.try_clone()?));
            let slave_for_exec = slave_path.clone();
            // SAFETY: the closure runs between fork and exec and calls
            // only async-signal-safe functions: setsid, ioctl, open, and
            // close.
            command.pre_exec(move || {
                setsid();
                // Make the slave the controlling terminal, so the child
                // is a foreground program of a real terminal.
                if ioctl_value(0, ffi::TIOCSCTTY, 0) != 0 {
                    let c_path = CString::new(slave_for_exec.as_os_str().as_encoded_bytes())
                        .expect("the device path has no interior zero");
                    let fd = open(c_path.as_ptr(), ffi::O_RDWR);
                    if fd >= 0 {
                        ioctl_value(fd, ffi::TIOCSCTTY, 0);
                        close(fd);
                    }
                }
                Ok(())
            });
            let child = command.spawn()?;
            Ok(PtySession {
                master: File::from_raw_fd(master),
                slave,
                child,
                output: Vec::new(),
            })
        }
    }

    /// The measured child's process id, for the resource samplers.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Writes keys to the terminal, blocking only until the terminal
    /// buffer takes them.
    pub fn send(&mut self, keys: &[u8]) {
        let mut written = 0;
        while written < keys.len() {
            match self.master.write(&keys[written..]) {
                Ok(count) => written += count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("cannot write to the pseudo-terminal: {error}"),
            }
        }
    }

    /// Reads whatever the surface wrote during `slice`.
    pub fn pump(&mut self, slice: Duration) {
        let deadline = Instant::now() + slice;
        loop {
            let mut buffer = [0u8; 8192];
            match self.master.read(&mut buffer) {
                Ok(count) if count > 0 => self.output.extend_from_slice(&buffer[..count]),
                Ok(_) => return,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
            if Instant::now() >= deadline {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Waits until any bytes arrive from the surface, at most `limit`
    /// past `started`, and returns the elapsed time. This is the P-KEY
    /// boundary: injection happened just before `started`, and the first
    /// answering byte stops the clock.
    fn wait_for_bytes(&mut self, started: Instant, limit: Duration) -> Wait {
        let deadline = started + limit;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Wait::TimedOut(started.elapsed());
            }
            // SAFETY: one poll call on the descriptor this object owns,
            // with a local request structure.
            let mut request = PollFd {
                fd: self.master.as_raw_fd(),
                events: ffi::POLLIN,
                revents: 0,
            };
            let remaining = (deadline - now).as_millis().clamp(0, i32::MAX as u128) as i32;
            let ready = unsafe { poll(&mut request, 1, remaining.max(1)) };
            if ready > 0 {
                let mut buffer = [0u8; 8192];
                match self.master.read(&mut buffer) {
                    Ok(count) if count > 0 => {
                        self.output.extend_from_slice(&buffer[..count]);
                        return Wait::Answered(started.elapsed());
                    }
                    _ => continue,
                }
            }
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Wait::TimedOut(started.elapsed());
            }
        }
    }

    /// Waits until the accumulated visible output satisfies `satisfied`,
    /// and reports when the completing bytes arrived. The clock belongs
    /// to the caller: `started` is the instant its boundary opened — the
    /// spawn for P-OPEN, the external edit's write for P-REFRESH — so
    /// the returned milliseconds measure the caller's window. `poll(2)`
    /// wakes the loop the moment bytes land, and the arrival, not the
    /// predicate check, stops the clock: the measured thing is when the
    /// frame's bytes got there, not when the harness noticed.
    pub fn wait_useful<F: Fn(&str) -> bool>(
        &mut self,
        started: Instant,
        limit: Duration,
        satisfied: F,
    ) -> (f64, bool) {
        if satisfied(&self.visible_text()) {
            return (started.elapsed().as_secs_f64() * 1000.0, true);
        }
        let deadline = started + limit;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return (started.elapsed().as_secs_f64() * 1000.0, false);
            }
            // SAFETY: one poll call on the descriptor this object owns,
            // with a local request structure.
            let mut request = PollFd {
                fd: self.master.as_raw_fd(),
                events: ffi::POLLIN,
                revents: 0,
            };
            let remaining = (deadline - now).as_millis().clamp(0, i32::MAX as u128) as i32;
            let ready = unsafe { poll(&mut request, 1, remaining.max(1)) };
            if ready > 0 {
                // The bytes are in the kernel buffer now; that arrival is
                // the event, whatever the read that follows costs.
                let arrival = started.elapsed();
                let mut buffer = [0u8; 8192];
                match self.master.read(&mut buffer) {
                    Ok(count) if count > 0 => {
                        self.output.extend_from_slice(&buffer[..count]);
                        if satisfied(&self.visible_text()) {
                            return (arrival.as_secs_f64() * 1000.0, true);
                        }
                    }
                    _ => continue,
                }
            }
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return (started.elapsed().as_secs_f64() * 1000.0, false);
            }
        }
    }

    /// Pumps until the visible output satisfies `satisfied`, or `limit`
    /// passes. Returns whether the condition held.
    pub fn wait_until<F: Fn(&str) -> bool>(&mut self, limit: Duration, satisfied: F) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            self.pump(Duration::from_millis(20));
            if satisfied(&self.visible_text()) {
                return true;
            }
        }
        satisfied(&self.visible_text())
    }

    /// Pumps until the visible output contains `needle`, ignoring escape
    /// sequences and whitespace runs.
    pub fn wait_for(&mut self, needle: &str, limit: Duration) -> bool {
        let collapsed = collapse(needle);
        self.wait_until(limit, |text| text.contains(&collapsed))
    }

    /// Sends one key and measures the latency to its answering bytes
    /// (SPEC 17.4 P-KEY). Returns the elapsed milliseconds and whether
    /// the surface answered within `limit`.
    pub fn send_key_timed(&mut self, keys: &[u8], limit: Duration) -> (f64, bool) {
        let started = Instant::now();
        self.send(keys);
        match self.wait_for_bytes(started, limit) {
            Wait::Answered(elapsed) => (elapsed.as_secs_f64() * 1000.0, true),
            Wait::TimedOut(elapsed) => (elapsed.as_secs_f64() * 1000.0, false),
        }
    }

    /// Drains the rest of one answer, so no stray byte of this frame can
    /// pose as the next event's response: the surface writes one frame
    /// per change, and `quiet` of silence after it means the frame ended.
    pub fn drain_frame(&mut self, quiet: Duration) {
        let deadline = Instant::now() + Duration::from_millis(200);
        loop {
            let before = self.output.len();
            self.pump(quiet);
            if self.output.len() == before || Instant::now() >= deadline {
                return;
            }
        }
    }

    /// The visible text of everything the surface wrote so far: escape
    /// sequences removed and whitespace runs collapsed, so a partial
    /// repaint still matches the words it drew.
    pub fn visible_text(&self) -> String {
        visible(&self.output)
    }

    /// The raw bytes collected so far.
    pub fn bytes_written(&self) -> u64 {
        self.output.len() as u64
    }

    /// The raw bytes collected so far, for diagnosis and oracles that
    /// must inspect the exact stream.
    pub fn raw_output(&self) -> &[u8] {
        &self.output
    }

    /// Says whether the child has exited, and with which code. A death by
    /// signal reports -1, the stream's spelling of "no clean exit".
    pub fn try_exit(&mut self) -> Option<i32> {
        self.child
            .try_wait()
            .expect("the child can be reaped exactly once")
            .map(|status| status.code().unwrap_or(-1))
    }

    /// Sends the quit key and waits for a clean exit within `limit`. A
    /// child that ignores the quit is killed and reported as `-1`, so a
    /// broken surface can never hang the harness.
    pub fn quit(&mut self, limit: Duration) -> i32 {
        self.send(b"q");
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            self.pump(Duration::from_millis(20));
            if let Some(code) = self.try_exit() {
                self.pump(Duration::from_millis(50));
                return code;
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        -1
    }

    /// Kills the child without ceremony; the caller already failed.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The `File` of one raw open.
unsafe fn open_file(path: &Path, flags: i32) -> io::Result<File> {
    let c_path = CString::new(path.as_os_str().as_encoded_bytes())
        .expect("the device path has no interior zero");
    // SAFETY: the path is a valid C string and the descriptor becomes a
    // File immediately.
    let fd = unsafe { open(c_path.as_ptr(), flags) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: the descriptor is fresh and now owned by the File.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

/// Sets the window size of one terminal.
unsafe fn set_window(fd: RawFd, columns: u16, rows: u16) {
    let mut size = Winsize {
        rows,
        columns,
        xpixel: 0,
        ypixel: 0,
    };
    // SAFETY: the descriptor belongs to the caller; the structure is
    // local.
    let _ = unsafe { ioctl_winsize(fd, ffi::TIOCSWINSZ, &mut size) };
}

/// The visible text of terminal output: escape sequences removed and
/// whitespace runs collapsed, so a wrapped or partially repainted frame
/// still matches the words it drew.
pub fn visible(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\x1b' {
            out.push(character);
            continue;
        }
        match characters.next() {
            // A control sequence: skip to its final byte.
            Some('[') => {
                for next in characters.by_ref() {
                    if next.is_ascii_alphabetic() || next == '~' {
                        break;
                    }
                }
            }
            // An operating-system sequence: skip to its terminator.
            Some(']') => {
                while let Some(next) = characters.next() {
                    if next == '\x07' {
                        break;
                    }
                    if next == '\x1b' {
                        // The string terminator is ESC backslash.
                        characters.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    collapse(&out)
}

/// Removes every whitespace character, so wrapped rows still match.
pub fn collapse(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_sequences_and_whitespace_vanish() {
        let raw = b"\x1b[2mmargin\x1b[0m writing \r\n\x1b]0;title\x07done";
        assert_eq!(visible(raw), "marginwritingdone");
        assert_eq!(
            collapse("New edits  arrived.\nRefresh"),
            "Neweditsarrived.Refresh"
        );
    }

    #[test]
    fn the_session_window_is_the_measured_window() {
        // 120 columns show the full footer text and the document list; 40
        // rows keep every layout path away from a zero dimension.
        assert_eq!(SESSION_COLUMNS, 120);
        assert_eq!(SESSION_ROWS, 40);
    }
}
