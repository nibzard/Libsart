//! The measurement boundary of SPEC 17.3.
//!
//! CLI latency starts before process creation and ends after exit and
//! full response delivery: the clock reads immediately before `spawn`
//! and stops only after both pipes have drained to end of file and the
//! child has been reaped. Everything the process does — Git children,
//! parsing, validation, filesystem work, serialization, required commit
//! verification — happens inside that window because it is all triggered
//! by the one timed child.

use std::io;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// One timed child process: the elapsed wall time plus its complete
/// response.
pub struct TimedProcess {
    /// Wall time from immediately before process creation through exit
    /// and full response delivery.
    pub elapsed: Duration,
    /// The exit code, or `None` when the child died from a signal.
    pub exit: Option<i32>,
    /// Complete standard output.
    pub stdout: Vec<u8>,
    /// Complete standard error.
    pub stderr: Vec<u8>,
}

impl TimedProcess {
    /// The elapsed wall time in milliseconds, microsecond resolution.
    pub fn elapsed_ms(&self) -> f64 {
        self.elapsed.as_secs_f64() * 1000.0
    }

    /// The failure a nonzero exit or a signal produces, or `None`.
    pub fn exit_problem(&self) -> Option<String> {
        match self.exit {
            Some(0) => None,
            Some(code) => Some(format!("exit code {code}")),
            None => Some("the process died from a signal".to_string()),
        }
    }
}

/// Runs one prepared command under the SPEC 17.3 boundary. The command
/// must carry its own working directory and environment; this function
/// only owns the pipes and the clock, never any fixture work.
pub fn time_child(command: &mut Command) -> io::Result<TimedProcess> {
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let started = Instant::now();
    let child = command.spawn()?;
    let output = child.wait_with_output()?;
    Ok(TimedProcess {
        elapsed: started.elapsed(),
        exit: output.status.code(),
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic child whose output size and sleep are known, so
    /// the test can prove the window covers creation and delivery. The
    /// payload comes from `head` so the argv stays small.
    const SLEEPER_SCRIPT: &str =
        "head -c 200000 /dev/zero | tr '\\0' m; sleep 0.02; printf tail";

    #[test]
    fn the_window_spans_creation_sleep_and_full_delivery() {
        let mut command = Command::new("sh");
        command.arg("-c").arg(SLEEPER_SCRIPT);
        let timed = time_child(&mut command).expect("the child runs");
        assert_eq!(timed.exit, Some(0));
        assert_eq!(timed.stdout.len(), 200_004, "the whole response arrived");
        assert!(timed.stdout.ends_with(b"tail"));
        assert!(
            timed.elapsed_ms() >= 20.0,
            "a 20 ms sleep cannot fit in {} ms",
            timed.elapsed_ms()
        );
        assert!(
            timed.elapsed_ms() < 2000.0,
            "the window did not wait for something unrelated"
        );
    }

    #[test]
    fn a_nonzero_exit_is_reported_not_hidden() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("printf 'partial'; exit 3");
        let timed = time_child(&mut command).expect("the child runs");
        assert_eq!(timed.exit_problem().as_deref(), Some("exit code 3"));
        assert_eq!(timed.stdout, b"partial");
    }
}
