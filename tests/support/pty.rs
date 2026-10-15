//! A minimal pseudo-terminal harness (SPEC 11.5, 16.3).
//!
//! The terminal-lifecycle tests need a real terminal: they spawn the
//! built binary on a pseudo-terminal, feed it keys, resize it, and read
//! the bytes it writes, so the tests can prove that raw mode, the
//! alternate screen, bracketed paste, and the cursor come back on every
//! exit path. The harness calls only the C library the standard library
//! already links: no new dependency enters the tree.
//!
//! The child runs as a session leader with the pseudo-terminal as its
//! controlling terminal, which is what makes a window-size change arrive
//! as the resize signal a real terminal sends.

use std::ffi::{CStr, CString, OsStr};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

// The C interface, as the two supported Unix platforms define it. The
// constants differ per platform; the calls are the POSIX set.
#[cfg(target_os = "linux")]
mod ffi {
    pub const O_NOCTTY: i32 = 0o400;
    pub const O_RDWR: i32 = 0o2;
    pub const O_NONBLOCK: i32 = 0o4000;
    pub const O_CLOEXEC: i32 = 0o2000000;
    pub const F_GETFL: i32 = 3;
    pub const F_SETFL: i32 = 4;
    pub const TIOCSWINSZ: u64 = 0x5414;
    pub const TIOCSCTTY: u64 = 0x540E;
    pub const ICANON: u32 = 0x0000_0002;
    pub const ECHO: u32 = 0x0000_0008;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Termios {
        pub c_iflag: u32,
        pub c_oflag: u32,
        pub c_cflag: u32,
        pub c_lflag: u32,
        pub c_line: u8,
        pub c_cc: [u8; 32],
        pub c_ispeed: u32,
        pub c_ospeed: u32,
    }
}

#[cfg(target_os = "macos")]
mod ffi {
    pub const O_NOCTTY: i32 = 0x20000;
    pub const O_RDWR: i32 = 0o2;
    // Darwin fcntl.h: O_NONBLOCK is 0x4, not the Linux flag value.
    pub const O_NONBLOCK: i32 = 0x0000_0004;
    pub const O_CLOEXEC: i32 = 0x1000000;
    pub const F_GETFL: i32 = 3;
    pub const F_SETFL: i32 = 4;
    pub const TIOCSWINSZ: u64 = 0x8008_7467;
    pub const TIOCSCTTY: u64 = 0x2000_7461;
    pub const ICANON: u64 = 0x0000_0100;
    pub const ECHO: u64 = 0x0000_0008;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Termios {
        pub c_iflag: u64,
        pub c_oflag: u64,
        pub c_cflag: u64,
        pub c_lflag: u64,
        pub c_cc: [u8; 20],
        pub c_ispeed: u64,
        pub c_ospeed: u64,
    }
}

#[allow(non_camel_case_types)]
type c_char = i8;

unsafe extern "C" {
    fn posix_openpt(flags: i32) -> RawFd;
    fn grantpt(fd: RawFd) -> i32;
    fn unlockpt(fd: RawFd) -> i32;
    fn ptsname(fd: RawFd) -> *const c_char;
    fn ioctl(fd: RawFd, request: u64, ...) -> i32;
    fn fcntl(fd: RawFd, command: i32, ...) -> i32;
    fn tcgetattr(fd: RawFd, termios: *mut ffi::Termios) -> i32;
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
#[derive(Clone, Copy)]
struct Winsize {
    rows: u16,
    columns: u16,
    xpixel: u16,
    ypixel: u16,
}

/// One process on a pseudo-terminal, with the bytes it wrote.
pub struct Pty {
    master: File,
    /// Kept open so the slave never reaches end-of-file from this side;
    /// the termios checks read it.
    slave: File,
    child: Child,
    output: Vec<u8>,
}

/// A second pseudo-terminal used only for output: closing its master
/// makes every write of the process fail while its input terminal stays
/// healthy, which is the mid-run write error of the handled-error path.
pub struct OutputPty {
    master: File,
    /// Kept open so the descriptors the child holds stay valid.
    slave: File,
    output: Vec<u8>,
}

impl OutputPty {
    /// Creates one output terminal of `columns` by `rows`.
    pub fn new(columns: u16, rows: u16) -> io::Result<OutputPty> {
        // SAFETY: plain C calls on descriptors this constructor owns.
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
            set_window(master, columns, rows);
            let flags = fcntl(master, ffi::F_GETFL, 0);
            if flags < 0 || fcntl(master, ffi::F_SETFL, flags | ffi::O_NONBLOCK) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(OutputPty {
                master: File::from_raw_fd(master),
                slave,
                output: Vec::new(),
            })
        }
    }

    /// One more descriptor of the slave, for the child's streams.
    pub fn slave(&self) -> io::Result<File> {
        self.slave.try_clone()
    }

    /// Reads whatever the process wrote during `slice`.
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
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Pumps until the visible output contains `needle`, or `limit`
    /// passes.
    pub fn wait_for(&mut self, needle: &str, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            self.pump(Duration::from_millis(120));
            if visible(&self.output).contains(&collapse(needle)) {
                return true;
            }
        }
        visible(&self.output).contains(&collapse(needle))
    }

    /// Closes the master: every later write of the process fails.
    pub fn kill(&mut self) {
        self.master = File::create("/dev/null").expect("/dev/null exists");
    }
}

impl Pty {
    /// Spawns `binary` on a fresh pseudo-terminal of `columns` by `rows`,
    /// as a session leader whose controlling terminal is the slave.
    pub fn spawn(
        binary: &Path,
        args: &[&str],
        envs: &[(String, String)],
        cwd: &Path,
        columns: u16,
        rows: u16,
    ) -> io::Result<Pty> {
        Self::spawn_with_output(binary, args, envs, cwd, columns, rows, None)
    }

    /// Spawns `binary` reading from a fresh pseudo-terminal, and writing
    /// to `output` when it is given, else to the same terminal it reads.
    pub fn spawn_with_output(
        binary: &Path,
        args: &[&str],
        envs: &[(String, String)],
        cwd: &Path,
        columns: u16,
        rows: u16,
        output: Option<File>,
    ) -> io::Result<Pty> {
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
            set_window(master, columns, rows);
            // The reads never block the test: output drains when it is
            // there, and a poll loop decides when to look again.
            let flags = fcntl(master, ffi::F_GETFL, 0);
            if flags < 0 || fcntl(master, ffi::F_SETFL, flags | ffi::O_NONBLOCK) < 0 {
                return Err(io::Error::last_os_error());
            }

            let mut command = Command::new(binary);
            command
                .args(args)
                .current_dir(cwd)
                .env_clear()
                .envs(envs.iter().cloned());
            command.stdin(Stdio::from(slave.try_clone()?));
            let (stdout, stderr) = match output {
                Some(file) => (file.try_clone()?, file),
                None => (slave.try_clone()?, slave.try_clone()?),
            };
            command.stdout(Stdio::from(stdout));
            command.stderr(Stdio::from(stderr));
            let slave_for_exec = slave_path.clone();
            // SAFETY: the closure runs between fork and exec and calls
            // only async-signal-safe functions: setsid, ioctl, open, and
            // close.
            command.pre_exec(move || {
                setsid();
                // Make the slave the controlling terminal, so resizes
                // reach the process as they reach a real foreground
                // program. Standard input is the slave once the standard
                // library wired the streams; the reopened path covers the
                // other order.
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
            Ok(Pty {
                master: File::from_raw_fd(master),
                slave,
                child,
                output: Vec::new(),
            })
        }
    }

    /// Writes keys to the terminal.
    pub fn send(&mut self, keys: &[u8]) {
        let mut written = 0;
        while written < keys.len() {
            match self.master.write(&keys[written..]) {
                Ok(count) => written += count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("cannot write to the pseudo-terminal: {error}"),
            }
        }
    }

    /// Sends keys and gives the surface a moment to answer.
    pub fn send_key(&mut self, keys: &[u8]) {
        self.send(keys);
        thread::sleep(Duration::from_millis(60));
        self.pump(Duration::from_millis(60));
    }

    /// Changes the terminal size, exactly as a window resize does.
    pub fn resize(&mut self, columns: u16, rows: u16) {
        // SAFETY: one ioctl on a descriptor this object owns.
        unsafe {
            set_window(self.master.as_raw_fd(), columns, rows);
        }
        thread::sleep(Duration::from_millis(100));
        self.pump(Duration::from_millis(150));
    }

    /// Reads whatever the process wrote during `slice`.
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
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Pumps until the visible output contains `needle`, or `limit`
    /// passes. Returns whether the needle appeared.
    pub fn wait_for(&mut self, needle: &str, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            self.pump(Duration::from_millis(120));
            if self.shows(needle) {
                return true;
            }
        }
        self.shows(needle)
    }

    /// Says whether the visible output so far contains `needle`,
    /// ignoring escape sequences and whitespace runs.
    pub fn shows(&self, needle: &str) -> bool {
        visible(&self.output).contains(&collapse(needle))
    }

    /// Says whether the raw output so far contains `needle`.
    pub fn shows_raw(&self, needle: &str) -> bool {
        self.count_raw(needle) > 0
    }

    /// Counts the occurrences of `needle` in the raw output.
    pub fn count_raw(&self, needle: &str) -> usize {
        if needle.is_empty() {
            return 0;
        }
        let mut count = 0;
        let mut start = 0;
        let bytes = needle.as_bytes();
        while let Some(found) = self.output[start..]
            .windows(bytes.len())
            .position(|window| window == bytes)
        {
            count += 1;
            start += found + bytes.len();
        }
        count
    }

    /// Waits for the process to exit within `limit`, collecting output
    /// while it runs.
    pub fn wait_exit(&mut self, limit: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            self.pump(Duration::from_millis(60));
            if let Some(status) = self
                .child
                .try_wait()
                .expect("the child can be reaped exactly once")
            {
                self.pump(Duration::from_millis(100));
                return Some(status);
            }
        }
        None
    }

    /// The raw bytes collected so far.
    pub fn raw_output(&self) -> &[u8] {
        &self.output
    }

    /// Closes the master side, so the process's terminal reads and
    /// writes fail: the error path every handled failure takes.
    pub fn close_master(&mut self) {
        self.master = File::create("/dev/null").expect("/dev/null exists");
    }

    /// Says whether the terminal is back in cooked mode: canonical input
    /// and echo restored, which raw mode had turned off.
    pub fn terminal_is_cooked(&self) -> bool {
        // SAFETY: one tcgetattr into a zeroed structure of the size the
        // platform defines.
        unsafe {
            let mut termios = std::mem::zeroed::<ffi::Termios>();
            if tcgetattr(self.slave.as_raw_fd(), &mut termios) != 0 {
                return false;
            }
            (termios.c_lflag & ffi::ICANON) != 0 && (termios.c_lflag & ffi::ECHO) != 0
        }
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
/// whitespace runs collapsed, so a partial repaint still matches the
/// words it drew.
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
fn collapse(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}
