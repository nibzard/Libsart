//! The resource samplers of P-CPU, P-RSS, and P-TREE-MEM (SPEC 17.4,
//! 17.5).
//!
//! The resource metrics read the kernel's own accounting through
//! `/proc`, so the numbers are what the operating system charged the
//! processes, never what Margin reports about itself. The process tree is
//! discovered from `PPid` links: the Margin process plus every Git child
//! it spawns, however short-lived.
//!
//! The three statistics are deliberately different, and each definition
//! sentence says which one it is:
//!
//! - P-RSS reads the parent's `VmHWM` — its own high-water resident set
//!   over the whole session, in MiB.
//! - P-TREE-MEM samples every 20 ms and reports the peak of the
//!   concurrent sum of `VmRSS` over the tree (SPEC 17.5: a maximum
//!   individual RSS or a child's lone peak is a different statistic).
//! - P-CPU accumulates `utime+stime` per process over one idle window
//!   and reports the share of one core the whole tree consumed, as a
//!   percentage.
//!
//! Every session drives the same fixed open/review/history sequence the
//! P-RSS metric defines, so all three numbers describe one workload.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::pty::PtySession;

/// How often the P-TREE-MEM sampler wakes to sum the tree's resident
/// memory. Fine enough that a Git child of ordinary lifetime is seen,
/// cheap enough that the sampler itself stays invisible in the number.
const TREE_SAMPLE_INTERVAL: Duration = Duration::from_millis(20);

/// How often the P-CPU accumulator rescans the tree, so a short-lived Git
/// child's ticks are kept even after the child exits.
const CPU_RESCAN_INTERVAL: Duration = Duration::from_secs(1);

/// The quiet gap that ends one frame's bytes.
const FRAME_QUIET: Duration = Duration::from_millis(30);

/// The wait every sequence step and frame check allows. The limit is
/// honesty headroom, not budget: a debug build under load can need ten
/// seconds to reach its first frame.
const STEP_LIMIT: Duration = Duration::from_secs(30);

/// One key press of the fixed sequence and what it does.
struct Step {
    keys: &'static [u8],
    why: &'static str,
}

/// The fixed open/review/history sequence of the resource metrics: the
/// useful Read frame, the Changes view, the History view, back to Read,
/// one `j`/`k` pair, then quit. The sequence is the metric definition, so
/// it lives here once and every run of every resource metric drives it.
///
/// Each step counts as done only when the surface answers with bytes.
/// The session runs in reduced-update mode, which draws only changed
/// frames, so answering bytes mean the frame visibly moved; the reverse
/// does not hold for needles, because a diff frame skips every cell the
/// previous frame already showed, and a needle can straddle a skipped
/// cell. The first frame is a full paint, so it alone is needle-checked.
const SEQUENCE: &[Step] = &[
    Step {
        keys: b"2",
        why: "the Changes view opened",
    },
    Step {
        keys: b"3",
        why: "the History view opened",
    },
    Step {
        keys: b"1",
        why: "the Read view returned",
    },
    Step {
        keys: b"j",
        why: "the reading position stepped down",
    },
    Step {
        keys: b"k",
        why: "the reading position stepped back",
    },
];

/// The sentence a record's notes carry about the fixed sequence, so every
/// resource record names the workload its number describes.
pub fn sequence_sentence() -> &'static str {
    "every session drove the fixed sequence: the useful Read frame, the Changes view, the \
     History view, back to Read, one j/k reading pair, then quit"
}

/// One resource run's outcome: the measured quantity in its own unit, the
/// exit code of the surface, and what the run's checks found wanting.
pub struct ResourceRun {
    /// The measured quantity: MiB for the memory metrics, percent of one
    /// core for P-CPU.
    pub value: f64,
    /// The unit sentence for the sample and the record.
    pub unit: &'static str,
    /// The surface's exit code, or -1 when it had to be killed.
    pub exit_code: i32,
    /// What the run's honesty checks found wanting.
    pub failures: Vec<String>,
    /// The bytes the surface flushed to the terminal.
    pub stdout_bytes: u64,
}

/// The clock ticks per second the kernel charges process time in.
///
/// Read through `sysconf(_SC_CLK_TCK)`; the fallback is the value every
/// mainstream Linux ABI uses, and the actual value is recorded in the
/// definition sentence the record carries.
fn clock_ticks_per_second() -> f64 {
    const SC_CLK_TCK: i32 = 2;
    // SAFETY: one read-only libc query with an integer argument.
    let ticks = unsafe { sysconf(SC_CLK_TCK) };
    if ticks > 0 {
        ticks as f64
    } else {
        100.0
    }
}

// SAFETY: declared for the sysconf call above; the standard C library is
// already linked.
unsafe extern "C" {
    fn sysconf(name: i32) -> i64;
}

/// One process's kernel accounting, read from `/proc/<pid>/stat`.
#[derive(Clone, Copy, Debug, Default)]
struct ProcStat {
    /// The parent's process id.
    ppid: u32,
    /// User time in clock ticks.
    utime: u64,
    /// System time in clock ticks.
    stime: u64,
}

/// Reads one `/proc/<pid>/stat`. The second field (the executable name in
/// parentheses) can contain spaces, so the parser starts after the last
/// closing parenthesis, exactly as proc(5) prescribes.
fn read_stat(pid: u32) -> Option<ProcStat> {
    let body = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = body.rsplit_once(')')?.1;
    let mut fields = after.split_whitespace();
    let _state = fields.next()?;
    let ppid: u32 = fields.next()?.parse().ok()?;
    // Fields 5 through 13 sit between the state and the CPU times.
    for _ in 0..9 {
        fields.next()?;
    }
    let utime: u64 = fields.next()?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    Some(ProcStat { ppid, utime, stime })
}

/// One `kB` field out of `/proc/<pid>/status`, converted to MiB.
fn status_kib_to_mib(pid: u32, field: &str) -> Option<f64> {
    let body = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix(field) {
            let number: f64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
            // The field is kibibytes; the metric is MiB.
            return Some(number / 1024.0);
        }
    }
    None
}

/// The resident set of one process, in MiB, or 0 when it is already gone.
fn resident_mib(pid: u32) -> f64 {
    status_kib_to_mib(pid, "VmRSS:").unwrap_or(0.0)
}

/// Every process id the kernel lists, with its accounting.
fn snapshot() -> BTreeMap<u32, ProcStat> {
    let mut whole = BTreeMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return whole;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(digits) = name.to_str().filter(|text| text.bytes().all(|b| b.is_ascii_digit()))
        else {
            continue;
        };
        let Ok(pid) = digits.parse::<u32>() else {
            continue;
        };
        if let Some(stat) = read_stat(pid) {
            whole.insert(pid, stat);
        }
    }
    whole
}

/// The process tree rooted at `root`: `root` plus every descendant, found
/// by following `PPid` links outward through the snapshot.
fn tree_of(whole: &BTreeMap<u32, ProcStat>, root: u32) -> Vec<u32> {
    let mut tree = vec![root];
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for (&pid, stat) in whole.iter() {
            if stat.ppid == parent && !tree.contains(&pid) {
                tree.push(pid);
                frontier.push(pid);
            }
        }
    }
    tree
}

/// Spawns the surface, waits for its useful first frame, and calls
/// `on_frame` while the session stands at that frame. The fixed sequence
/// then runs; `measure` reads its quantity before the quit key.
fn run_session<F, M>(
    executable: &Path,
    envs: &BTreeMap<OsString, OsString>,
    cwd: &Path,
    on_frame: F,
    measure: M,
) -> Result<ResourceRun, String>
where
    F: FnOnce(&PtySession),
    M: FnOnce(&mut PtySession) -> Result<(&'static str, f64, Vec<String>), String>,
{
    let mut session = PtySession::spawn(executable, &["tui", "--reduced-updates"], envs, cwd)
        .map_err(|error| format!("the surface cannot start on its terminal: {error}"))?;
    let mut failures = Vec::new();
    if !session.wait_for("1 Read 2 Changes 3 History", STEP_LIMIT) {
        failures.push("the session never showed the useful first frame".to_string());
    }
    on_frame(&session);
    session.drain_frame(FRAME_QUIET.max(Duration::from_millis(100)));
    for step in SEQUENCE {
        // Answering bytes are the step's proof: the reduced-update
        // surface draws only when frame-visible state changed.
        let (_, answered) = session.send_key_timed(step.keys, STEP_LIMIT);
        if !answered {
            failures.push(format!("the sequence stalled: {}", step.why));
        }
        session.drain_frame(FRAME_QUIET);
    }
    let (unit, value, mut measured) = measure(&mut session)?;
    failures.append(&mut measured);
    let exit_code = session.quit(STEP_LIMIT);
    if exit_code != 0 {
        failures.push(format!("the surface exited with {exit_code}"));
    }
    Ok(ResourceRun {
        value,
        unit,
        exit_code,
        failures,
        stdout_bytes: session.bytes_written(),
    })
}

/// P-RSS: the surface parent's high-water resident set over the fixed
/// sequence, in MiB.
pub fn run_rss(
    executable: &Path,
    envs: &BTreeMap<OsString, OsString>,
    cwd: &Path,
) -> Result<ResourceRun, String> {
    run_session(
        executable,
        envs,
        cwd,
        |_| {},
        |session| {
            // VmHWM survives until the process exits, so reading it after
            // the sequence and before the quit key captures the whole
            // session's peak; the process is still alive here.
            let peak = status_kib_to_mib(session.pid(), "VmHWM:")
                .ok_or_else(|| "the parent's VmHWM left /proc before it was read".to_string())?;
            let mut failures = Vec::new();
            if !peak.is_finite() || peak <= 0.0 {
                failures.push(format!("the measured peak RSS {peak} is not a positive quantity"));
            }
            Ok(("MiB", peak, failures))
        },
    )
}

/// P-TREE-MEM: the peak of the concurrently sampled process-tree resident
/// sum, in MiB (SPEC 17.5 demands the concurrent sum, not any per-process
/// maximum).
pub fn run_tree_memory(
    executable: &Path,
    envs: &BTreeMap<OsString, OsString>,
    cwd: &Path,
) -> Result<ResourceRun, String> {
    let root = Arc::new(AtomicU32::new(0));
    let stopped = Arc::new(AtomicBool::new(false));
    let sampler_root = Arc::clone(&root);
    let sampler_stopped = Arc::clone(&stopped);
    let sampler = thread::spawn(move || {
        let mut peak = 0.0f64;
        while !sampler_stopped.load(Ordering::Relaxed) {
            let pid = sampler_root.load(Ordering::Relaxed);
            if pid != 0 {
                let whole = snapshot();
                let sum: f64 = tree_of(&whole, pid).iter().map(|&pid| resident_mib(pid)).sum();
                if sum > peak {
                    peak = sum;
                }
            }
            thread::sleep(TREE_SAMPLE_INTERVAL);
        }
        peak
    });
    let outcome = run_session(
        executable,
        envs,
        cwd,
        |session| root.store(session.pid(), Ordering::Relaxed),
        |_| Ok(("MiB", 0.0, Vec::new())),
    );
    // The sampler covers the whole session, launch through quit, because
    // the launch's own Git children belong to the measured workload.
    stopped.store(true, Ordering::Relaxed);
    let peak = sampler.join().map_err(|_| "the tree sampler panicked")?;
    let mut run = outcome?;
    run.value = peak;
    if !peak.is_finite() || peak <= 0.0 {
        run.failures
            .push(format!("the measured tree peak {peak} is not a positive quantity"));
    }
    Ok(run)
}

/// P-CPU: the share of one core the whole tree consumed over one idle
/// window, as a percentage.
///
/// The accumulation keeps a per-process maximum of `utime+stime` seen in
/// one-second rescans of the tree. A process that exits mid-window keeps
/// its last-seen total in the sum, so short-lived Git children's CPU is
/// not lost; kernel pid reuse inside the window can only overcount,
/// never hide. The definition sentence states the method.
pub fn run_cpu(
    executable: &Path,
    envs: &BTreeMap<OsString, OsString>,
    cwd: &Path,
    window: Duration,
) -> Result<ResourceRun, String> {
    let ticks_per_second = clock_ticks_per_second();
    let mut session =
        PtySession::spawn(executable, &["tui", "--reduced-updates"], envs, cwd)
            .map_err(|error| format!("the surface cannot start on its terminal: {error}"))?;
    let mut failures = Vec::new();
    if !session.wait_for("1 Read 2 Changes 3 History", STEP_LIMIT) {
        failures.push("the session never showed the useful first frame".to_string());
    }
    let pid = session.pid();

    // The idle window: no input reaches the surface while it runs.
    let started = Instant::now();
    let start = snapshot();
    let tree_start = tree_of(&start, pid);
    let start_total: u64 = tree_start
        .iter()
        .filter_map(|&p| start.get(&p).map(|stat| stat.utime + stat.stime))
        .sum();
    let mut maxima: BTreeMap<u32, u64> = tree_start
        .into_iter()
        .filter_map(|p| start.get(&p).map(|stat| (p, stat.utime + stat.stime)))
        .collect();
    while started.elapsed() < window {
        let remaining = window.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        thread::sleep(remaining.min(CPU_RESCAN_INTERVAL));
        let whole = snapshot();
        for member in tree_of(&whole, pid) {
            if let Some(stat) = whole.get(&member) {
                let total = stat.utime + stat.stime;
                if total > maxima.get(&member).copied().unwrap_or(0) {
                    maxima.insert(member, total);
                }
            }
        }
        // Drain whatever the idle surface wrote, so the terminal buffer
        // cannot fill and wedge the surface mid-window.
        session.pump(Duration::from_millis(5));
    }
    let end_total: u64 = maxima.values().sum();
    let elapsed = started.elapsed();
    let delta_ticks = end_total.saturating_sub(start_total);
    let percent = 100.0 * (delta_ticks as f64 / ticks_per_second) / elapsed.as_secs_f64();

    let exit_code = session.quit(STEP_LIMIT);
    if exit_code != 0 {
        failures.push(format!("the surface exited with {exit_code}"));
    }
    if !percent.is_finite() || percent <= 0.0 {
        failures.push(format!(
            "the measured idle CPU {percent} is not a positive quantity; the window may be too \
             short for any tick to land"
        ));
    }
    Ok(ResourceRun {
        value: percent,
        unit: "percent-of-one-core",
        exit_code,
        failures,
        stdout_bytes: session.bytes_written(),
    })
}

/// The memory definition sentences the record's `memory_definition`
/// field carries (SPEC 17.8): what was summed, from where, in which unit.
pub fn memory_definition(metric: &str) -> String {
    match metric {
        "P-RSS" => "VmHWM of the surface parent process, read from /proc/<pid>/status after the \
                   fixed open/review/history sequence and before the quit key, in MiB (1 MiB = \
                   1024 KiB); the value is the kernel's high-water mark, not a Margin report"
            .to_string(),
        "P-TREE-MEM" => format!(
            "peak of the concurrent sum of VmRSS over the process tree rooted at the surface \
             parent (Margin plus every Git child, found through PPid links in /proc/*/stat), \
             sampled every {} ms across launch, the fixed sequence, and quit, in MiB; a \
             per-process maximum or a child's lone peak is a different statistic (SPEC 17.5)",
            TREE_SAMPLE_INTERVAL.as_millis()
        ),
        "P-CPU" => format!(
            "sum of utime+stime in clock ticks (CLK_TCK={}, read through sysconf) over the \
             process tree rooted at the surface parent, accumulated as per-process maxima in \
             one-second rescans so short-lived Git children keep their ticks after exit; \
             reported as 100 * (delta ticks / CLK_TCK) / window seconds, the share of one core",
            clock_ticks_per_second()
        ),
        other => format!("no resource definition exists for {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_proc_stat_parser_skips_the_command_field() {
        // A comm with spaces and parentheses must not shift the fields.
        // The parser reads /proc only, so the same splitting is proved
        // here on synthetic bytes.
        let text = "42 (git (status)) S 41 42 42 0 -1 4194560 100 0 0 5 7 1 2 0 20 0";
        let after = text.rsplit_once(')').unwrap().1;
        let mut fields = after.split_whitespace();
        let _state = fields.next().unwrap();
        let ppid: u32 = fields.next().unwrap().parse().unwrap();
        for _ in 0..9 {
            fields.next();
        }
        let utime: u64 = fields.next().unwrap().parse().unwrap();
        let stime: u64 = fields.next().unwrap().parse().unwrap();
        assert_eq!(ppid, 41, "the parent link survives the command field");
        assert_eq!(utime, 7);
        assert_eq!(stime, 1, "the pair is fields 14 and 15, nine fields past ppid");
    }

    #[test]
    fn the_tree_of_a_snapshot_contains_every_descendant() {
        let mut whole = BTreeMap::new();
        whole.insert(1, ProcStat { ppid: 0, utime: 0, stime: 0 });
        whole.insert(10, ProcStat { ppid: 1, utime: 0, stime: 0 });
        whole.insert(11, ProcStat { ppid: 10, utime: 0, stime: 0 });
        whole.insert(99, ProcStat { ppid: 1, utime: 0, stime: 0 });
        let mut tree = tree_of(&whole, 10);
        tree.sort_unstable();
        assert_eq!(tree, vec![10, 11], "an unrelated process stays out");
    }

    #[test]
    fn the_definitions_name_their_statistic() {
        let tree = memory_definition("P-TREE-MEM");
        assert!(tree.contains("concurrent sum"), "{tree}");
        assert!(tree.contains("different statistic"), "{tree}");
        let cpu = memory_definition("P-CPU");
        assert!(cpu.contains("one-second rescans"), "{cpu}");
        let rss = memory_definition("P-RSS");
        assert!(rss.contains("VmHWM"), "{rss}");
    }

    #[test]
    fn the_fixed_sequence_visits_every_view() {
        assert_eq!(SEQUENCE.len(), 5, "changes, history, read, j, k");
        let sentence = sequence_sentence();
        assert!(sentence.contains("Changes") && sentence.contains("History"), "{sentence}");
    }

    #[test]
    fn the_window_loop_ends_at_the_window() {
        // A window shorter than one rescan slice must still terminate and
        // read the tree at least once.
        let started = Instant::now();
        let window = Duration::from_millis(50);
        while started.elapsed() < window {
            let remaining = window.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                break;
            }
            thread::sleep(remaining.min(CPU_RESCAN_INTERVAL));
        }
        assert!(started.elapsed() >= window);
    }
}
