//! The measurement loop of SPEC 17.7: warmups, independent paired
//! blocks of fresh-process runs, recorded order randomization, per-run
//! oracle verification, and the honest status decision.
//!
//! Three sampler families meet here. The CLI metrics time one fresh
//! process per run (SPEC 17.3). The PTY metrics drive a real
//! pseudo-terminal session per run — launch-to-frame, key-to-bytes,
//! edit-to-indicator (SPEC 17.4) — and the resource metrics read the
//! kernel's accounting of the session's process tree. Paired comparison
//! stays a CLI-only shape: the SPEC 17.7 regression gate is defined over
//! paired fresh-process blocks, and a PTY or resource pairing would need
//! its own design, so the harness refuses one instead of faking it.
//!
//! Policy defaults are the SPEC numbers per metric kind. Below-policy
//! runs are reported as `inconclusive`, never silently promoted.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use super::envrec::{EnvironmentRecord, now_utc};
use super::fixture::Fixture;
use super::metric::{self, Kind};
use super::oracle;
use super::pty::{self, PtySession};
use super::record::{
    self, Comparison, CorpusSummary, Details, EnvironmentSummary, ExecutableSummary, Gate,
    InvalidatedBlock, OracleState, ResultRecord, Sampling, Statistics, Status,
};
use super::resource;
use super::samples::{SAMPLES_SCHEMA_VERSION, Sample};
use super::stats;
use super::timing::time_child;

/// Which executable configuration one run measures.
#[derive(Clone, Debug)]
pub enum Sides {
    /// One absolute measurement against the budget.
    Single(PathBuf),
    /// A paired baseline-to-candidate comparison (SPEC 17.7.1-17.7.3).
    Paired {
        /// The baseline executable.
        baseline: PathBuf,
        /// The candidate executable.
        candidate: PathBuf,
    },
}

impl Sides {
    /// The executable the gates apply to.
    fn gated(&self) -> &Path {
        match self {
            Sides::Single(path) => path,
            Sides::Paired { candidate, .. } => candidate,
        }
    }

    /// The version labels the samples carry, in execution-planning
    /// order.
    fn versions(&self) -> Vec<&'static str> {
        match self {
            Sides::Single(_) => vec!["single"],
            Sides::Paired { .. } => vec!["baseline", "candidate"],
        }
    }
}

/// One planned measurement.
#[derive(Clone, Debug)]
pub struct RunPlan {
    /// The metric identifier.
    pub metric: &'static str,
    /// The corpus label.
    pub corpus: String,
    /// The executable configuration.
    pub sides: Sides,
    /// Warmups per version.
    pub warmups: usize,
    /// Independent blocks.
    pub blocks: usize,
    /// Fresh-process runs per block per version. For P-KEY one run is
    /// one whole session of events; for P-CPU one run is one window.
    pub runs_per_block: usize,
    /// The seed of the order randomization, recorded with the samples.
    pub order_seed: u64,
    /// Counted navigation events per P-KEY session (SPEC 17.4: at least
    /// 10,000 over the recorded sessions). Zero for every other metric.
    pub events_per_session: usize,
    /// The length of one P-CPU idle window in seconds (SPEC 17.4: five
    /// minutes). Zero for every other metric.
    pub window_seconds: u64,
    /// The commit the executable was built from, when known.
    pub implementation_commit: Option<String>,
    /// Operator notes recorded verbatim.
    pub notes: Vec<String>,
}

impl RunPlan {
    /// The sampling parameters this plan implies. The boundary and
    /// session sentences follow the metric's kind, so a PTY record never
    /// claims the CLI process boundary and a resource record never
    /// claims a latency at all.
    fn sampling(&self) -> Sampling {
        let kind = metric::by_id(self.metric)
            .map(|definition| definition.kind)
            .unwrap_or(Kind::Cli);
        let (boundary, cache_method) = match kind {
            Kind::Cli => (
                "wall time from before process creation through exit and full response \
                 delivery (SPEC 17.3)"
                    .to_string(),
                "warm filesystem caches, fresh process per run".to_string(),
            ),
            Kind::Pty => (
                match self.metric {
                    "P-OPEN" => {
                        "wall time from immediately before the surface spawns until the first \
                         frame that shows identity, content, save state, and actions has flushed"
                            .to_string()
                    }
                    "P-KEY" => {
                        "wall time from immediately before one key's bytes are written until the \
                         first answering bytes arrive from the surface"
                            .to_string()
                    }
                    _ => "wall time from an external edit's write returning until the arrival \
                         notice flushes"
                        .to_string(),
                },
                format!(
                    "one fresh surface session per run on a {}-column, {}-row xterm-256color \
                     pseudo-terminal, driven with --reduced-updates so bytes mean a frame change",
                    pty::SESSION_COLUMNS,
                    pty::SESSION_ROWS
                ),
            ),
            Kind::Resource => (
                "a kernel-accounted quantity of the session's process tree, not a wall \
                 latency: VmHWM, the concurrent tree sum, or utime+stime"
                    .to_string(),
                format!(
                    "one fresh surface session per run on a {}-column, {}-row xterm-256color \
                     pseudo-terminal, driven with --reduced-updates through the fixed \
                     open/review/history sequence",
                    pty::SESSION_COLUMNS,
                    pty::SESSION_ROWS
                ),
            ),
        };
        Sampling {
            warmups: self.warmups,
            blocks: self.blocks,
            runs_per_block: self.runs_per_block,
            order_seed: self.order_seed,
            boundary,
            cache_method,
            events_per_session: self.events_per_session,
            window_seconds: self.window_seconds,
            ..Sampling::policy()
        }
    }
}

/// One executed measurement: the record and where its raw samples live.
#[derive(Debug)]
pub struct RunOutcome {
    /// The finished result record.
    pub record: ResultRecord,
    /// Every sample, warmups included.
    pub samples: Vec<Sample>,
    /// The raw sample stream that was written.
    pub raw_path: PathBuf,
    /// The record file that was written.
    pub record_path: PathBuf,
}

/// Refuses to measure without a registered environment (SPEC 17.6):
/// until a runner registers, results stay `not_measured`.
pub fn require_registered(environment: &EnvironmentRecord) -> Result<(), String> {
    match environment.is_registered() {
        true => Ok(()),
        false => Err(format!(
            "the environment record is `{}`; register a runner before measuring \
             (SPEC 17.6 keeps results not_measured until then)",
            environment.status
        )),
    }
}

/// The executable summary of one path.
fn executable_summary(path: &Path) -> Result<ExecutableSummary, String> {
    let bytes = std::fs::metadata(path)
        .map_err(|error| format!("{} cannot be stat'ed: {error}", path.display()))?
        .len();
    let sha256 = super::hash::sha256_file(path)
        .map_err(|error| format!("{} cannot be hashed: {error}", path.display()))?;
    Ok(ExecutableSummary {
        path: path.to_path_buf(),
        bytes,
        sha256,
    })
}

/// Builds the timed child command for one metric.
fn timed_command(
    plan: &RunPlan,
    executable: &Path,
    fixture: &Fixture,
    prepared: &super::fixture::Prepared,
) -> Command {
    let mut command = Command::new(executable);
    match plan.metric {
        "P-HELP" => {
            command.args(["--help", "--json"]);
        }
        "P-STATUS" => {
            command.args(["status", "--json"]);
        }
        "P-DIFF" => {
            command.args(["diff", "--path", &fixture.facts.selected_path, "--json"]);
        }
        "P-HISTORY" => {
            command.args(["log", "--limit", "50", "--json"]);
        }
        "P-CHECKPOINT" => {
            command.args(["checkpoint", "--message", "bench checkpoint pass", "--json"]);
        }
        "P-APPLY" => {
            let super::fixture::Prepared::Review { plan, .. } = prepared else {
                unreachable!("the review reset prepared a plan")
            };
            let input = plan.display().to_string();
            command.args(["review", "--input", &input, "--json"]);
        }
        other => unreachable!("the plan was validated against the metric table: {other}"),
    }
    command.current_dir(&fixture.repo);
    for (key, _) in std::env::vars_os() {
        let scrubs = key.as_encoded_bytes().starts_with(b"GIT_") || key == "EMAIL";
        if scrubs {
            command.env_remove(&key);
        }
    }
    command.envs(&fixture.vars);
    command
}

/// One run against one executable: reset, time, verify. Reset and
/// verification sit outside the timed window by construction — the
/// clock only wraps the child.
// The parameter list mirrors the sample coordinates of SPEC 17.7.3 (block,
// position, order index, warmup flag, iteration), so the record stays
// reconstructible from the call site.
#[allow(clippy::too_many_arguments)]
fn one_run(
    plan: &RunPlan,
    executable: &Path,
    version: &str,
    block: usize,
    run_in_block: usize,
    order_index: usize,
    warmup: bool,
    iteration: usize,
    fixture: &Fixture,
) -> Result<Sample, String> {
    let prepared = fixture.reset_for(plan.metric, iteration)?;
    let request_id = match &prepared {
        super::fixture::Prepared::Review { request_id, .. } => Some(request_id.clone()),
        super::fixture::Prepared::Repository => None,
    };
    let mut command = timed_command(plan, executable, fixture, &prepared);
    let timed = time_child(&mut command)
        .map_err(|error| format!("cannot run the measured command: {error}"))?;
    let failures = verify_run(
        plan,
        fixture,
        &timed.stdout,
        &timed.stderr,
        request_id.as_deref(),
    );
    let exit_code = timed.exit.unwrap_or(-1);
    let mut all = failures;
    if let Some(problem) = timed.exit_problem() {
        // A failed run is still a sample: SPEC 17.7.4 retains everything.
        all.insert(0, format!("the command ended with {problem}"));
    }
    Ok(sample_of(
        plan,
        version,
        block,
        run_in_block,
        order_index,
        warmup,
        timed.elapsed_ms(),
        exit_code,
        &timed,
        &all,
    ))
}

/// The oracle check of one finished run.
fn verify_run(
    plan: &RunPlan,
    fixture: &Fixture,
    stdout: &[u8],
    stderr: &[u8],
    request_id: Option<&str>,
) -> Vec<String> {
    let facts = &fixture.facts;
    let mut failures = Vec::new();
    match plan.metric {
        "P-HELP" => oracle::check(&mut failures, "help", oracle::verify_help(stdout, stderr)),
        "P-STATUS" => oracle::check(
            &mut failures,
            "status",
            oracle::verify_status(&fixture.oracle, facts, stdout, stderr),
        ),
        "P-DIFF" => oracle::check(
            &mut failures,
            "diff",
            oracle::verify_diff(&fixture.oracle, facts, stdout, stderr),
        ),
        "P-HISTORY" => oracle::check(
            &mut failures,
            "history",
            oracle::verify_history(&fixture.oracle, stdout, stderr, 50),
        ),
        "P-CHECKPOINT" => oracle::check(
            &mut failures,
            "checkpoint",
            oracle::verify_checkpoint(
                &fixture.oracle,
                facts,
                &fixture.checkpoint_expected_tree,
                &fixture.checkpoint_dirty,
                stdout,
                stderr,
            ),
        ),
        "P-APPLY" => oracle::check(
            &mut failures,
            "review",
            oracle::verify_review(
                &fixture.oracle,
                facts,
                &fixture.review_expected_tree,
                &fixture.review_restored,
                &fixture.review_kept,
                request_id.unwrap_or("bench-p-apply-unknown"),
                stdout,
                stderr,
            ),
        ),
        other => unreachable!("unmeasured metric {other} reached the oracle"),
    }
    failures
}

/// Assembles one raw sample.
#[allow(clippy::too_many_arguments)]
fn sample_of(
    plan: &RunPlan,
    version: &str,
    block: usize,
    run_in_block: usize,
    order_index: usize,
    warmup: bool,
    elapsed_ms: f64,
    exit_code: i32,
    timed: &super::timing::TimedProcess,
    failures: &[String],
) -> Sample {
    Sample {
        schema_version: SAMPLES_SCHEMA_VERSION,
        metric: plan.metric.to_string(),
        corpus: plan.corpus.to_string(),
        version: version.to_string(),
        block,
        run_in_block,
        order_index,
        warmup,
        elapsed_ms,
        unit: None,
        exit_code,
        stdout_bytes: timed.stdout.len() as u64,
        stderr_bytes: timed.stderr.len() as u64,
        oracle: if failures.is_empty() {
            "passed".to_string()
        } else {
            "failed".to_string()
        },
        oracle_failures: failures.to_vec(),
        cache_method: "warm".to_string(),
        invalidated: false,
        invalidation_reason: None,
        at: now_utc(),
    }
}

// ---------------------------------------------------------------------------
// The PTY and resource samplers (SPEC 17.4)
// ---------------------------------------------------------------------------

/// How long one PTY session may take to reach its first useful frame or
/// its arrival notice. A timeout is a failed sample with its elapsed
/// recorded, never a hang. The limit is honesty headroom, not budget: a
/// debug build under load can need ten seconds to reach its first frame,
/// and the budget gate reads the measured p95, not this ceiling.
const FRAME_LIMIT: Duration = Duration::from_secs(30);

/// How long one P-KEY event may take to answer. SPEC 17.4 fails missing
/// responses and event timeouts; the sample keeps the timeout as its
/// elapsed and an oracle failure.
const KEY_LIMIT: Duration = Duration::from_secs(2);

/// How long a session gets to exit after its quit key before the harness
/// kills it and records the run as failed.
const QUIT_LIMIT: Duration = Duration::from_secs(10);

/// The quiet gap that ends one frame's bytes, so no stray byte of one
/// answer can pose as the next event's response.
const FRAME_QUIET: Duration = Duration::from_millis(30);

/// After how many consecutive unanswered events a P-KEY session counts
/// as dead: the event script provably changes the frame every time, so a
/// silent surface is a broken session, and the harness stops feeding it.
const DEAD_AFTER_TIMEOUTS: usize = 3;

/// The paragraph the P-REFRESH edit appends to the selected document's
/// working file. The bytes are read untimed; the write itself is the
/// trigger the clock starts from.
const REFRESH_PARAGRAPH: &[u8] =
    b"\n\nThe bench refresh edit appends this paragraph to the working file. The next inspection \
      must discover it and state the arrival.\n";

/// One point of evidence one measured unit produced. For the latency
/// metrics the value is milliseconds; a resource metric stores its own
/// quantity and names the unit.
struct MeasuredPoint {
    /// The measured quantity.
    value: f64,
    /// The unit sentence, `None` for milliseconds.
    unit: Option<&'static str>,
    /// What this point's checks found wanting.
    failures: Vec<String>,
}

/// One executed PTY session or resource run, before it becomes raw
/// samples. A P-KEY session contributes one point per event; every other
/// unit contributes exactly one.
struct MeasuredUnit {
    /// The measured points, in execution order.
    points: Vec<MeasuredPoint>,
    /// The child's exit code, or -1 when it had to be killed.
    exit_code: i32,
    /// What the unit's own checks found wanting.
    failures: Vec<String>,
    /// The bytes the child flushed to its terminal.
    stdout_bytes: u64,
}

/// Runs one planned unit — one CLI run, one PTY session, or one resource
/// run — and appends its raw samples. `order_index` advances by exactly
/// the number of samples the unit produced.
#[allow(clippy::too_many_arguments)]
fn unit_samples(
    plan: &RunPlan,
    executable: &Path,
    version: &str,
    block: usize,
    run_in_block: usize,
    warmup: bool,
    iteration: usize,
    fixture: &Fixture,
    order_index: &mut usize,
) -> Result<Vec<Sample>, String> {
    let kind = metric::by_id(plan.metric).expect("validated").kind;
    let unit = match kind {
        Kind::Cli => {
            let sample = one_run(
                plan,
                executable,
                version,
                block,
                run_in_block,
                *order_index,
                warmup,
                iteration,
                fixture,
            )?;
            *order_index += 1;
            return Ok(vec![sample]);
        }
        Kind::Pty => pty_unit(plan, executable, fixture)?,
        Kind::Resource => resource_unit(plan, executable, fixture)?,
    };
    let start = *order_index;
    let rows = unit
        .points
        .iter()
        .enumerate()
        .map(|(event, point)| {
            let mut failures = unit.failures.clone();
            failures.extend(point.failures.iter().cloned());
            if unit.exit_code != 0 {
                failures.insert(
                    0,
                    format!("the session ended with exit code {}", unit.exit_code),
                );
            }
            Sample {
                schema_version: SAMPLES_SCHEMA_VERSION,
                metric: plan.metric.to_string(),
                corpus: plan.corpus.to_string(),
                version: version.to_string(),
                // SPEC 17.4 P-KEY: every event is one sample; the block is
                // its session's block and the run index is the event's
                // place inside the session.
                block,
                run_in_block: event,
                order_index: start + event,
                warmup,
                elapsed_ms: point.value,
                unit: point.unit.map(str::to_string),
                exit_code: unit.exit_code,
                stdout_bytes: unit.stdout_bytes,
                stderr_bytes: 0,
                oracle: if failures.is_empty() {
                    "passed".to_string()
                } else {
                    "failed".to_string()
                },
                oracle_failures: failures,
                cache_method: "warm".to_string(),
                invalidated: false,
                invalidation_reason: None,
                at: now_utc(),
            }
        })
        .collect::<Vec<_>>();
    *order_index += rows.len();
    Ok(rows)
}

/// The arguments every measured surface session runs with: the fixed
/// 120x40 terminal comes from the driver, the selected document pins the
/// first frame to the corpus fact, and `--reduced-updates` makes the
/// surface draw only changed frames — so bytes arriving at the master
/// mean the surface answered, which is the P-KEY boundary.
fn session_args(fixture: &Fixture) -> Vec<String> {
    vec![
        "tui".to_string(),
        "--reduced-updates".to_string(),
        "--path".to_string(),
        fixture.facts.selected_path.clone(),
    ]
}

/// The needles the first useful frame must show (SPEC 17.4): the header
/// fragment that names the branch and the review head, the document's
/// own opening heading, one saved-state label, the active tab, and the
/// contextual footer. A spinner or a blank frame shows none of them.
///
/// Every needle is written for one terminal row. The surface draws its
/// panes side by side, so the flattened stream interleaves the panes row
/// by row: a needle longer than one row would span foreign text. The
/// first frame is a full paint of an empty screen, so its rows arrive
/// complete; later frames are diffs that skip unchanged cells, which is
/// why only the first frame is needle-verified.
fn useful_frame_needles(fixture: &Fixture) -> Result<(String, String, Vec<String>), String> {
    let facts = &fixture.facts;
    let short_head: String = facts.candidate.chars().take(7).collect();
    let identity = pty::collapse(&format!("· main @ {short_head}"));
    let working = std::fs::read(fixture.repo.join(&facts.selected_path))
        .map_err(|error| format!("the selected document cannot be read: {error}"))?;
    let text = String::from_utf8_lossy(&working);
    let first_line = text.lines().next().unwrap_or_default();
    let source = if first_line.chars().count() >= 4 {
        first_line
    } else {
        &text
    };
    let content: String = pty::collapse(source).chars().take(40).collect();
    let saved = [
        "Checkpointed",
        "Not checkpointed",
        "Checkpointed, working file gone",
    ]
    .iter()
    .map(|label| pty::collapse(label))
    .collect();
    Ok((identity, content, saved))
}

/// Whether one frame's visible text is the useful first frame of SPEC
/// 17.4: identity, meaningful content, freshness state, and actions.
fn frame_is_useful(text: &str, identity: &str, content: &str, saved: &[String]) -> bool {
    text.contains(identity)
        && text.contains(content)
        && text.contains(&pty::collapse("[1 Read]"))
        && text.contains(&pty::collapse("2 Changes"))
        && text.contains(&pty::collapse("3 History"))
        && text.contains(&pty::collapse("focus reading"))
        && saved.iter().any(|label| text.contains(label))
}

/// Runs one PTY metric session: P-OPEN, P-KEY, or P-REFRESH.
fn pty_unit(plan: &RunPlan, executable: &Path, fixture: &Fixture) -> Result<MeasuredUnit, String> {
    // Every iteration begins from the intended state (SPEC 17.7.2); a
    // P-REFRESH run leaves its edit behind, so the reset matters between
    // runs, not only before the first.
    fixture.reset_to_candidate()?;
    fixture.assert_reviewable()?;
    match plan.metric {
        "P-OPEN" => open_unit(executable, fixture),
        "P-KEY" => key_unit(plan, executable, fixture),
        "P-REFRESH" => refresh_unit(executable, fixture),
        other => Err(format!("the PTY sampler does not measure {other}")),
    }
}

/// P-OPEN: launch to the first useful document frame (SPEC 17.4). The
/// clock starts immediately before the child is spawned and stops when
/// the bytes that complete the useful frame arrive at the master.
fn open_unit(executable: &Path, fixture: &Fixture) -> Result<MeasuredUnit, String> {
    let (identity, content, saved) = useful_frame_needles(fixture)?;
    let args = session_args(fixture);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let started = Instant::now();
    let mut session = PtySession::spawn(executable, &arg_refs, &fixture.vars, &fixture.repo)
        .map_err(|error| format!("the surface cannot start on its terminal: {error}"))?;
    let (elapsed_ms, _reached) = session.wait_useful(started, FRAME_LIMIT, |text| {
        frame_is_useful(text, &identity, &content, &saved)
    });
    let exit_code = session.quit(QUIT_LIMIT);
    let mut failures = Vec::new();
    // The oracle repeats the frame check outside timing, on everything
    // the session ever flushed: a fast run whose frame never showed
    // identity, content, state, and actions fails the benchmark.
    if !frame_is_useful(&session.visible_text(), &identity, &content, &saved) {
        failures.push(
            "the flushed frames never showed identity, content, a saved-state label, and the \
             footer actions together"
                .to_string(),
        );
    }
    Ok(MeasuredUnit {
        points: vec![MeasuredPoint {
            value: elapsed_ms,
            unit: None,
            failures,
        }],
        exit_code,
        failures: Vec::new(),
        stdout_bytes: session.bytes_written(),
    })
}

/// P-KEY: input injection to answering bytes, one sample per event
/// (SPEC 17.4).
///
/// The event script is deterministic: alternating `j`/`k` reading steps
/// from the top of the selected document. Every event provably changes
/// the frame, because the reading offset starts at the top row and each
/// step moves it by one row, so a step that drew nothing is a real
/// missing response. Timing starts immediately before the key bytes are
/// written — queued input, polling, decoding, and rendering are all
/// inside (SPEC 17.4) — and stops at the first answering byte.
fn key_unit(plan: &RunPlan, executable: &Path, fixture: &Fixture) -> Result<MeasuredUnit, String> {
    let clean_before = fixture.oracle.status_porcelain();
    let args = session_args(fixture);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut session = PtySession::spawn(executable, &arg_refs, &fixture.vars, &fixture.repo)
        .map_err(|error| format!("the surface cannot start on its terminal: {error}"))?;
    let (identity, content, saved) = useful_frame_needles(fixture)?;
    let mut failures = Vec::new();
    if !session.wait_until(FRAME_LIMIT, |text| {
        frame_is_useful(text, &identity, &content, &saved)
    }) {
        failures.push("the session never showed its useful first frame".to_string());
    }
    session.drain_frame(FRAME_QUIET.max(Duration::from_millis(100)));

    let mut points = Vec::with_capacity(plan.events_per_session);
    let mut consecutive_timeouts = 0usize;
    let mut dead = false;
    for event in 0..plan.events_per_session {
        if dead {
            // The session stopped answering; the remaining events never
            // ran, and their rows say so instead of borrowing a number.
            points.push(MeasuredPoint {
                value: 0.0,
                unit: None,
                failures: vec![format!(
                    "the session stopped answering before event {event} ran; the event was \
                     never injected"
                )],
            });
            continue;
        }
        let key: &[u8] = if event % 2 == 0 { b"j" } else { b"k" };
        let (elapsed_ms, answered) = session.send_key_timed(key, KEY_LIMIT);
        let mut event_failures = Vec::new();
        if answered {
            consecutive_timeouts = 0;
        } else {
            event_failures.push(format!(
                "event {event} ({}) drew no bytes within its {} ms timeout",
                if event % 2 == 0 { 'j' } else { 'k' },
                KEY_LIMIT.as_millis()
            ));
            consecutive_timeouts += 1;
            if consecutive_timeouts >= DEAD_AFTER_TIMEOUTS {
                dead = true;
            }
        }
        session.drain_frame(FRAME_QUIET);
        points.push(MeasuredPoint {
            value: elapsed_ms,
            unit: None,
            failures: event_failures,
        });
    }
    let exit_code = session.quit(QUIT_LIMIT);

    // The read-only session touched nothing: stock Git confirms it
    // outside timing (SPEC 17.7.6).
    if !clean_before.is_empty() {
        failures.push(format!(
            "the repository was dirty before the session: {clean_before}"
        ));
    }
    let after = fixture.oracle.status_porcelain();
    if !after.is_empty() {
        failures.push(format!("the session left changes behind: {after}"));
    }
    Ok(MeasuredUnit {
        points,
        exit_code,
        failures,
        stdout_bytes: session.bytes_written(),
    })
}

/// P-REFRESH: a selected-file external edit to the explicit arrival
/// notice (SPEC 17.4, 13.3).
///
/// The session launches untimed and shows its useful frame first. The
/// paragraph bytes are prepared untimed; the clock starts the moment the
/// write returns, and stops when the collapsed output first carries the
/// arrival sentence. Discovery and display are measured, never durable
/// saving — the old candidate stays pinned.
fn refresh_unit(executable: &Path, fixture: &Fixture) -> Result<MeasuredUnit, String> {
    let facts = &fixture.facts;
    let target = fixture.repo.join(&facts.selected_path);
    let mut dirty = std::fs::read(&target)
        .map_err(|error| format!("the selected document cannot be read: {error}"))?;
    dirty.extend_from_slice(REFRESH_PARAGRAPH);

    let args = session_args(fixture);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut session = PtySession::spawn(executable, &arg_refs, &fixture.vars, &fixture.repo)
        .map_err(|error| format!("the surface cannot start on its terminal: {error}"))?;
    let (identity, content, saved) = useful_frame_needles(fixture)?;
    let mut failures = Vec::new();
    if !session.wait_until(FRAME_LIMIT, |text| {
        frame_is_useful(text, &identity, &content, &saved)
    }) {
        failures.push("the session never showed its useful first frame".to_string());
    }
    session.drain_frame(FRAME_QUIET.max(Duration::from_millis(100)));

    std::fs::write(&target, &dirty)
        .map_err(|error| format!("the external edit cannot land: {error}"))?;
    let started = Instant::now();
    // The needle is the notice's own tail, not its opening words. The
    // notice replaces the short status line at the foot of the reading
    // pane, and a diff frame skips every cell the status line already
    // held — an early needle can straddle a skipped cell and never match
    // the flattened stream even though the frame shows the notice. The
    // status text ends near the row's start, while the notice's tail sits
    // past column 90 of a 120-column row, so its cells are always new.
    // The prior review status can already contain "review" in those cells.
    // Match only the final words beyond that status, which the diff emits.
    let notice = pty::collapse("never saw");
    let (elapsed_ms, appeared) =
        session.wait_useful(started, FRAME_LIMIT, |text| text.contains(&notice));
    let exit_code = session.quit(QUIT_LIMIT);
    if !appeared {
        failures.push(
            "the arrival notice never reached the frame within its limit; the 2 s polling window \
             and the draw belong inside this metric"
                .to_string(),
        );
    }
    // The pinned state survived: exactly the appended edit is dirty and
    // HEAD never moved (SPEC 17.4: the candidate stays pinned until a
    // refresh). The porcelain line's leading space is the index column,
    // and the oracle's `ok` trims the whole output, so the expected line
    // carries no leading space either.
    let dirty_state = fixture.oracle.status_porcelain();
    let expected_line = format!("M {}", facts.selected_path);
    if dirty_state.trim_end() != expected_line {
        failures.push(format!(
            "the working tree was expected to hold exactly the appended edit ({expected_line}), \
             but holds: {dirty_state}"
        ));
    }
    let head = fixture.oracle.head();
    if head != facts.candidate {
        failures.push(format!(
            "HEAD moved to {head} during a refresh measurement; the old candidate must stay pinned"
        ));
    }
    Ok(MeasuredUnit {
        points: vec![MeasuredPoint {
            value: elapsed_ms,
            unit: None,
            failures,
        }],
        exit_code,
        failures: Vec::new(),
        stdout_bytes: session.bytes_written(),
    })
}

/// Runs one resource metric session: P-CPU, P-RSS, or P-TREE-MEM.
fn resource_unit(
    plan: &RunPlan,
    executable: &Path,
    fixture: &Fixture,
) -> Result<MeasuredUnit, String> {
    fixture.reset_to_candidate()?;
    fixture.assert_reviewable()?;
    let run = match plan.metric {
        "P-RSS" => resource::run_rss(executable, &fixture.vars, &fixture.repo)?,
        "P-TREE-MEM" => resource::run_tree_memory(executable, &fixture.vars, &fixture.repo)?,
        "P-CPU" => resource::run_cpu(
            executable,
            &fixture.vars,
            &fixture.repo,
            Duration::from_secs(plan.window_seconds.max(1)),
        )?,
        other => return Err(format!("the resource sampler does not measure {other}")),
    };
    Ok(MeasuredUnit {
        points: vec![MeasuredPoint {
            value: run.value,
            unit: Some(run.unit),
            failures: run.failures,
        }],
        exit_code: run.exit_code,
        failures: Vec::new(),
        stdout_bytes: run.stdout_bytes,
    })
}

/// Runs one whole measurement and writes the raw samples and record.
pub fn run(
    plan: &RunPlan,
    fixture: &Fixture,
    environment: &EnvironmentRecord,
    environment_path: &Path,
    results_dir: &Path,
) -> Result<RunOutcome, String> {
    require_registered(environment)?;
    let definition =
        metric::by_id(plan.metric).ok_or_else(|| format!("unknown metric {}", plan.metric))?;
    match definition.kind {
        Kind::Cli => {}
        Kind::Pty | Kind::Resource => {
            if !matches!(plan.sides, Sides::Single(_)) {
                return Err(format!(
                    "{} is a {} metric; the SPEC 17.7 paired comparison is defined for primary \
                     CLI metrics, and a PTY or resource pairing would need its own paired \
                     design; measure one side with --executable or leave the metric \
                     not_measured",
                    plan.metric,
                    definition.kind.as_str()
                ));
            }
        }
    }
    // Return the fixture to C before anything derives from it: a
    // previous measurement may have left its last mutation behind
    // (SPEC 17.7.2). The review context is pinned once, outside timing,
    // and only the metric that reads a plan needs it.
    fixture.reset_to_candidate()?;
    fixture.assert_reviewable()?;
    if plan.metric == "P-APPLY" {
        fixture.derive_review_context(plan.sides.gated())?;
    }

    let mut samples: Vec<Sample> = Vec::new();
    let mut order_index = 0usize;
    let mut rng = stats::SplitMix64::seeded(plan.order_seed);

    // Warmups: same reset and oracle treatment, excluded from the
    // statistics, retained in the raw stream.
    for version in plan.sides.versions() {
        let executable = executable_of(&plan.sides, version)?;
        for warmup in 0..plan.warmups {
            samples.extend(unit_samples(
                plan,
                &executable,
                version,
                0,
                warmup,
                true,
                order_index,
                fixture,
                &mut order_index,
            )?);
        }
    }

    // Measured blocks; a paired block randomizes its side order under
    // the recorded seed (SPEC 17.7.3).
    let mut iteration = order_index;
    for block in 0..plan.blocks {
        let mut versions = plan.sides.versions();
        if versions.len() == 2 && rng.coin() {
            versions.swap(0, 1);
        }
        for version in versions {
            let executable = executable_of(&plan.sides, version)?;
            for run_in_block in 0..plan.runs_per_block {
                samples.extend(unit_samples(
                    plan,
                    &executable,
                    version,
                    block,
                    run_in_block,
                    false,
                    iteration,
                    fixture,
                    &mut order_index,
                )?);
                iteration += 1;
            }
        }
    }

    let raw_dir = results_dir
        .join("raw")
        .join(format!("{}.{}", plan.metric, plan.corpus))
        .join(now_utc().replace(':', ""));
    std::fs::create_dir_all(&raw_dir)
        .map_err(|error| format!("cannot create {}: {error}", raw_dir.display()))?;
    let raw_path = raw_dir.join("samples.jsonl");
    super::samples::write_jsonl(&raw_path, &samples)
        .map_err(|error| format!("cannot write {}: {error}", raw_path.display()))?;

    let record_path = results_dir.join(ResultRecord::file_name(plan.metric, &plan.corpus));
    let outcome = finish_record(
        plan,
        fixture,
        environment,
        environment_path,
        &samples,
        &raw_path,
    );
    outcome
        .record
        .write(&record_path)
        .map_err(|error| format!("cannot write {}: {error}", record_path.display()))?;
    Ok(RunOutcome {
        record: outcome.record,
        samples,
        raw_path,
        record_path,
    })
}

/// Marks one block invalidated by a documented external incident and
/// recomputes the record from the retained samples (SPEC 17.7.4: the
/// block is preserved with its reason, never deleted).
pub fn invalidate_block(
    samples_path: &Path,
    record_path: &Path,
    block: usize,
    reason: &str,
) -> Result<(), String> {
    let mut samples = super::samples::read_jsonl(samples_path)
        .map_err(|error| format!("cannot read {}: {error}", samples_path.display()))?;
    let mut marked = 0;
    for sample in &mut samples {
        if sample.block == block {
            sample.invalidated = true;
            sample.invalidation_reason = Some(reason.to_string());
            marked += 1;
        }
    }
    if marked == 0 {
        return Err(format!(
            "no sample of {} belongs to block {block}",
            samples_path.display()
        ));
    }
    super::samples::write_jsonl(samples_path, &samples)
        .map_err(|error| format!("cannot rewrite {}: {error}", samples_path.display()))?;

    let mut record = ResultRecord::load(record_path)
        .map_err(|error| format!("cannot read {}: {error}", record_path.display()))?;
    let details = record
        .details
        .as_mut()
        .ok_or_else(|| "the record was never measured; nothing to recompute".to_string())?;
    details.invalidated_blocks.push(InvalidatedBlock {
        block,
        reason: reason.to_string(),
        at: now_utc(),
    });
    details.notes.push(format!(
        "block {block} was invalidated after a documented incident ({reason}); its samples are \
         preserved but excluded, and both sides must be rerun once under the predeclared rule"
    ));

    // Recompute the statistics from the retained samples with the same
    // arithmetic a fresh run uses.
    let gated_version = if samples.iter().any(|sample| sample.version == "candidate") {
        "candidate"
    } else {
        "single"
    };
    let definition = metric::by_id(&record.metric)
        .ok_or_else(|| format!("unknown recorded metric {}", record.metric))?;
    let paired = samples.iter().any(|sample| sample.version == "baseline");
    let plan = RunPlan {
        metric: definition.id,
        corpus: record.corpus.clone(),
        sides: if paired {
            Sides::Paired {
                baseline: details
                    .comparison
                    .as_ref()
                    .map(|comparison| comparison.baseline.path.clone())
                    .unwrap_or_else(|| details.executable.path.clone()),
                candidate: details.executable.path.clone(),
            }
        } else {
            Sides::Single(details.executable.path.clone())
        },
        warmups: details.sampling.warmups,
        blocks: details.sampling.blocks,
        runs_per_block: details.sampling.runs_per_block,
        order_seed: details.sampling.order_seed,
        events_per_session: details.sampling.events_per_session,
        window_seconds: details.sampling.window_seconds,
        implementation_commit: record.implementation_commit.clone(),
        notes: Vec::new(),
    };
    let counted = record::counted_values(&samples, gated_version);
    let statistics = statistics_of(&record.metric, &counted);
    details.comparison = comparison_of(&plan, &samples);
    let regression_blocked = details
        .comparison
        .as_ref()
        .is_some_and(|comparison| comparison.regression_blocked);
    let budget_exceeded = statistics.as_ref().is_some_and(|statistics| {
        record
            .budget_p95_ms
            .is_some_and(|limit| statistics.p95_ms > limit as f64)
            || record::budget_p99_ms(&record.metric, &record.corpus)
                .is_some_and(|limit| statistics.p99_ms.is_some_and(|p99| p99 > limit as f64))
            || record::resource_ceiling(&record.metric, &record.corpus)
                .is_some_and(|limit| statistics.max_ms > limit)
    });
    let short_of_policy = sampling_below_policy(&plan, &samples);
    record.sample_count = counted.len();
    if definition.kind == Kind::Resource {
        record.median_ms = None;
        record.p95_ms = None;
    } else {
        record.median_ms = statistics.as_ref().map(|statistics| statistics.median_ms);
        record.p95_ms = statistics.as_ref().map(|statistics| statistics.p95_ms);
    }
    details.gate.budget_exceeded = budget_exceeded;
    details.gate.confirmation_required = budget_exceeded || regression_blocked;
    let exits_broken = samples
        .iter()
        .any(|sample| sample.exit_code != 0 && !sample.warmup);
    record.status = if record.correctness_oracle == OracleState::Failed
        || exits_broken
        || budget_exceeded
        || regression_blocked
    {
        Status::Failed
    } else if short_of_policy {
        details.notes.push(below_policy_note(&plan, counted.len()));
        Status::Inconclusive
    } else if statistics.is_some() {
        Status::Passed
    } else {
        Status::NotMeasured
    };
    details.statistics = statistics;
    record
        .write(record_path)
        .map_err(|error| format!("cannot write {}: {error}", record_path.display()))?;
    println!(
        "benchrun: invalidated block {block} of {} ({} samples marked); record {} recomputed",
        samples_path.display(),
        marked,
        record_path.display()
    );
    Ok(())
}

/// The executable of one version label.
fn executable_of(sides: &Sides, version: &str) -> Result<PathBuf, String> {
    match (sides, version) {
        (Sides::Single(path), "single") => Ok(path.clone()),
        (Sides::Paired { baseline, .. }, "baseline") => Ok(baseline.clone()),
        (Sides::Paired { candidate, .. }, "candidate") => Ok(candidate.clone()),
        _ => Err(format!("no executable for version {version}")),
    }
}

/// The statistics block of one counted series. Shared by the run and the
/// invalidate paths, so a recomputed record can never disagree with a
/// fresh one. The method sentence names the unit the numbers carry,
/// because a resource quantity in unlabeled fields is not evidence.
pub fn statistics_of(metric_id: &str, measured: &[f64]) -> Option<Statistics> {
    if measured.is_empty() {
        return None;
    }
    let (min_ms, max_ms) = stats::range(measured);
    Some(Statistics {
        sample_count: measured.len(),
        median_ms: stats::median(measured),
        p95_ms: stats::p95(measured),
        // A p99 is evidence only in the large event series P-KEY's
        // policy produces (SPEC 17.7.5); a smaller series would borrow a
        // rank it cannot support, so it stays absent.
        p99_ms: if metric_id == "P-KEY" && measured.len() >= 10_000 {
            Some(stats::nearest_rank(measured, 99.0))
        } else {
            None
        },
        min_ms,
        max_ms,
        method: match metric::by_id(metric_id).map(|definition| definition.kind) {
            Some(Kind::Resource) if metric_id == "P-CPU" => {
                "nearest-rank over percent-of-one-core values; every counted run retained; the \
                 gate is the maximum sample against the ceiling"
                    .to_string()
            }
            Some(Kind::Resource) => {
                "nearest-rank over MiB values; every counted run retained; the gate is the \
                 maximum sample against the ceiling"
                    .to_string()
            }
            _ => "nearest-rank; all samples retained including slow ones".to_string(),
        },
    })
}

/// The unit sentence of one metric's samples, for notes that must quote a
/// number next to its unit.
fn unit_of(metric_id: &str) -> &'static str {
    match metric_id {
        "P-CPU" => "percent-of-one-core",
        "P-RSS" | "P-TREE-MEM" => "MiB",
        _ => "ms",
    }
}

/// Whether this plan met the sampling policy its metric kind declares
/// (SPEC 17.7.3, 17.4). The floors differ per kind: the event series for
/// P-KEY, the five-minute window for P-CPU, the lighter repeat count for
/// the resource quantities. A run below its floor is diagnostic and its
/// verdict is `inconclusive`, never a pass.
fn below_policy(plan: &RunPlan, counted_samples: usize) -> bool {
    match plan.metric {
        "P-KEY" => {
            plan.warmups < 5
                || plan.blocks < 20
                || plan.runs_per_block < 5
                || counted_samples < 10_000
        }
        "P-CPU" => plan.blocks < 2 || plan.window_seconds < 300 || counted_samples < 2,
        "P-RSS" | "P-TREE-MEM" => {
            plan.warmups < 1 || plan.blocks < 4 || plan.runs_per_block < 3 || counted_samples < 12
        }
        _ => {
            plan.warmups < 5 || plan.blocks < 20 || plan.runs_per_block < 5 || counted_samples < 100
        }
    }
}

/// Checks the retained complete blocks for every measured version.
fn sampling_below_policy(plan: &RunPlan, samples: &[Sample]) -> bool {
    let minimum_blocks = match plan.metric {
        "P-CPU" => 2,
        "P-RSS" | "P-TREE-MEM" => 4,
        _ => 20,
    };
    let rows_per_run = if plan.metric == "P-KEY" {
        plan.events_per_session
    } else {
        1
    };
    let rows_per_block = plan.runs_per_block.saturating_mul(rows_per_run);
    plan.sides.versions().iter().any(|version| {
        let values = record::counted_values(samples, version);
        let complete_blocks = record::counted_blocks(samples, version)
            .iter()
            .filter(|block| !block.is_empty() && block.len() >= rows_per_block)
            .count();
        below_policy(plan, values.len()) || complete_blocks < minimum_blocks
    }) || matches!(plan.sides, Sides::Paired { .. }) && {
        let baseline = record::counted_blocks(samples, "baseline");
        let candidate = record::counted_blocks(samples, "candidate");
        baseline
            .iter()
            .zip(&candidate)
            .filter(|(baseline, candidate)| {
                !baseline.is_empty()
                    && !candidate.is_empty()
                    && baseline.len() >= rows_per_block
                    && candidate.len() >= rows_per_block
            })
            .count()
            < minimum_blocks
    }
}

/// The note that names which policy floor a run fell short of, so an
/// `inconclusive` verdict always carries its reason.
fn below_policy_note(plan: &RunPlan, counted_samples: usize) -> String {
    match plan.metric {
        "P-KEY" => format!(
            "{} counted events sit below the SPEC 17.4 floor of 10,000 over the policy session \
             count (5 warmup sessions, then 20 blocks of 5 sessions of 100 events); the verdict \
             is inconclusive",
            counted_samples
        ),
        "P-CPU" => format!(
            "{} idle window{} of {} s sit below the five-minute definition of SPEC 17.4; the run \
             is diagnostic and its verdict is inconclusive",
            plan.blocks,
            if plan.blocks == 1 { "" } else { "s" },
            plan.window_seconds
        ),
        "P-RSS" | "P-TREE-MEM" => format!(
            "the resource policy asks at least 4 blocks of 3 runs after 1 warmup; this plan ran \
             {} warmup{} and {} block{} of {} run{}, so the verdict is inconclusive",
            plan.warmups,
            if plan.warmups == 1 { "" } else { "s" },
            plan.blocks,
            if plan.blocks == 1 { "" } else { "s" },
            plan.runs_per_block,
            if plan.runs_per_block == 1 { "" } else { "s" },
        ),
        _ => format!(
            "{} counted samples over {} blocks sit below the SPEC 17.7.3 policy \
             (at least 20 independent block pairs); the comparison evidence is inconclusive",
            counted_samples, plan.blocks
        ),
    }
}

/// Computes statistics, gates, and the final status from the retained
/// samples. Shared by the run and the invalidate paths, so marking a
/// block invalidated and recomputing can never disagree with a fresh
/// run's arithmetic.
pub fn finish_record(
    plan: &RunPlan,
    fixture: &Fixture,
    environment: &EnvironmentRecord,
    environment_path: &Path,
    samples: &[Sample],
    raw_path: &Path,
) -> RunOutcome {
    let definition = metric::by_id(plan.metric).expect("validated");
    let gated_version = match plan.sides {
        Sides::Single(_) => "single",
        Sides::Paired { .. } => "candidate",
    };
    let measured = record::counted_values(samples, gated_version);
    let statistics = statistics_of(plan.metric, &measured);

    // Correctness first: one incorrect run fails the benchmark
    // regardless of speed (SPEC 17.7.6).
    let oracle_state = if samples.iter().any(|sample| sample.oracle == "failed") {
        OracleState::Failed
    } else if samples.iter().any(|sample| sample.oracle == "passed") {
        OracleState::Passed
    } else {
        OracleState::NotRun
    };
    let exits_broken = samples
        .iter()
        .any(|sample| sample.exit_code != 0 && !sample.warmup);

    let budget = definition.budget(&plan.corpus);
    let budget_p95 = budget.as_ref().and_then(|budget| budget.p95_ms());
    let budget_p99 = record::budget_p99_ms(plan.metric, &plan.corpus);
    let ceiling = record::resource_ceiling(plan.metric, &plan.corpus);
    // The gate shape follows the metric kind. A latency budget compares
    // the nearest-rank p95, and P-KEY adds its p99 once the event series
    // is large enough to carry one. A resource ceiling compares the
    // maximum counted sample, because one wild run is exactly what a
    // ceiling exists to catch, and a percentile would hide it.
    let p95_exceeded = match (&statistics, budget_p95) {
        (Some(stats_block), Some(limit)) => stats_block.p95_ms > limit as f64,
        _ => false,
    };
    let p99_exceeded = match (&statistics, budget_p99) {
        (Some(stats_block), Some(limit)) => {
            stats_block.p99_ms.is_some_and(|p99| p99 > limit as f64)
        }
        _ => false,
    };
    let ceiling_exceeded = match (&statistics, ceiling) {
        (Some(stats_block), Some(limit)) => stats_block.max_ms > limit,
        _ => false,
    };
    let budget_exceeded = p95_exceeded || p99_exceeded || ceiling_exceeded;

    let comparison = comparison_of(plan, samples);
    let regression_blocked = comparison
        .as_ref()
        .map(|comparison| comparison.regression_blocked)
        .unwrap_or(false);

    let invalidated_blocks: Vec<InvalidatedBlock> = samples
        .iter()
        .filter(|sample| sample.invalidated)
        .filter_map(|sample| {
            sample
                .invalidation_reason
                .as_ref()
                .map(|reason| InvalidatedBlock {
                    block: sample.block,
                    reason: reason.clone(),
                    at: now_utc(),
                })
        })
        .fold(Vec::new(), |mut acc: Vec<InvalidatedBlock>, block| {
            if !acc.iter().any(|recorded| recorded.block == block.block) {
                acc.push(block);
            }
            acc
        });

    let policy = plan.sampling();
    // The SPEC 17.7.3 minimums decide, not the knobs: a shortened run is
    // diagnostic and its verdict is inconclusive, never a pass. Each kind
    // carries its own floor.
    let short_of_policy = sampling_below_policy(plan, samples);

    // Correctness failures and gate failures share one verdict: a fast but
    // wrong run and an over-budget run both fail, never pass by speed.
    let status = if oracle_state == OracleState::Failed
        || exits_broken
        || budget_exceeded
        || regression_blocked
    {
        Status::Failed
    } else if short_of_policy {
        Status::Inconclusive
    } else if statistics.is_some() {
        Status::Passed
    } else {
        Status::NotMeasured
    };

    let mut notes = plan.notes.clone();
    if short_of_policy {
        notes.push(below_policy_note(plan, measured.len()));
    }
    match definition.kind {
        Kind::Pty => {
            // The session shape is part of the metric's meaning: record
            // it, so a number is never read without its terminal.
            notes.push(format!(
                "each session ran the surface on a {}-column, {}-row xterm-256color \
                 pseudo-terminal with --reduced-updates",
                pty::SESSION_COLUMNS,
                pty::SESSION_ROWS
            ));
            if plan.metric == "P-KEY" {
                notes.push(format!(
                    "the event script alternated j/k reading steps, so every one of the {} \
                     events per session provably changed the frame",
                    plan.events_per_session
                ));
            }
        }
        Kind::Resource => notes.push(resource::sequence_sentence().to_string()),
        Kind::Cli => {}
    }
    if p95_exceeded {
        notes.push(
            "the p95 exceeded its budget: SPEC 17.7 requires one declared confirmation run \
             of both versions, preserved, before the failure blocks release"
                .to_string(),
        );
    }
    if p99_exceeded {
        notes.push(
            "the p99 exceeded its budget: the key-to-bytes tail is what the P-KEY p99 exists \
             to catch, and one declared confirmation run must precede any release"
                .to_string(),
        );
    }
    if ceiling_exceeded {
        let (value, unit) = statistics
            .as_ref()
            .map(|stats_block| (stats_block.max_ms, unit_of(plan.metric)))
            .expect("an exceeded ceiling implies statistics");
        notes.push(format!(
            "the maximum counted sample ({value:.1} {unit}) exceeded its ceiling: a resource \
             gate reads the maximum, never a percentile"
        ));
    }
    if regression_blocked {
        notes.push(
            "the regression gate fired: the 99% bootstrap lower bound for the median increase \
             exceeds both 10% of the baseline median and 5 ms"
                .to_string(),
        );
    }

    let mut environment_summary = environment_summary_of(environment, environment_path);
    let executable = executable_summary(plan.sides.gated()).ok();
    environment_summary.build_profile = Some(observed_profile(plan.sides.gated()));
    let details = Details {
        metric_kind: definition.kind.as_str().to_string(),
        metric_about: definition.about.to_string(),
        measured_at: now_utc(),
        environment: environment_summary,
        executable: executable.clone().unwrap_or(ExecutableSummary {
            path: plan.sides.gated().to_path_buf(),
            bytes: 0,
            sha256: String::new(),
        }),
        corpus: CorpusSummary {
            directory: fixture.root.clone(),
            manifest_path: fixture.root.join("manifest.json"),
            manifest_sha256: fixture.manifest_sha256.clone(),
            generator_revision: fixture.generator_revision.clone(),
            seed: fixture.seed,
            test_scale: fixture.test_scale(),
        },
        sampling: policy,
        statistics: statistics.clone(),
        comparison: comparison.clone(),
        gate: Gate {
            budget: budget.as_ref().map(|budget| budget.describe()),
            budget_exceeded,
            confirmation_required: budget_exceeded || regression_blocked,
        },
        process_counts: None,
        // Every resource record states what its numbers summed and where
        // the kernel kept them (SPEC 17.8); a latency record carries none.
        memory_definition: if definition.kind == Kind::Resource {
            Some(resource::memory_definition(plan.metric))
        } else {
            None
        },
        invalidated_blocks,
        notes,
    };

    let record = ResultRecord {
        schema_version: record::RESULT_SCHEMA_VERSION,
        metric: plan.metric.to_string(),
        corpus: plan.corpus.to_string(),
        status,
        environment_id: environment.runner_id.clone(),
        implementation_commit: plan.implementation_commit.clone(),
        executable_hash: executable.as_ref().map(|summary| summary.sha256.clone()),
        corpus_manifest_hash: Some(fixture.manifest_sha256.clone()),
        sample_count: statistics
            .as_ref()
            .map_or(0, |stats_block| stats_block.sample_count),
        // A resource quantity is not a latency: the base fields stay
        // empty and the numbers live in `details.statistics`, whose
        // method sentence names their unit (SPEC 17.8).
        median_ms: if definition.kind == Kind::Resource {
            None
        } else {
            statistics.as_ref().map(|stats_block| stats_block.median_ms)
        },
        p95_ms: if definition.kind == Kind::Resource {
            None
        } else {
            statistics.as_ref().map(|stats_block| stats_block.p95_ms)
        },
        budget_p95_ms: budget_p95,
        correctness_oracle: oracle_state,
        raw_samples_path: Some(raw_path.to_path_buf()),
        details: Some(details),
    };
    RunOutcome {
        record,
        samples: samples.to_vec(),
        raw_path: raw_path.to_path_buf(),
        record_path: PathBuf::new(),
    }
}

/// The paired comparison block, when both sides ran.
fn comparison_of(plan: &RunPlan, samples: &[Sample]) -> Option<Comparison> {
    let Sides::Paired { baseline, .. } = &plan.sides else {
        return None;
    };
    let all_baseline_blocks = record::counted_blocks(samples, "baseline");
    let all_candidate_blocks = record::counted_blocks(samples, "candidate");
    // Slots retain their original block IDs. Pair those IDs before removing
    // gaps, so a missing block never shifts one version against the other.
    let (baseline_blocks, candidate_blocks): (Vec<_>, Vec<_>) = all_baseline_blocks
        .into_iter()
        .zip(all_candidate_blocks)
        .filter(|(baseline, candidate)| {
            !baseline.is_empty()
                && !candidate.is_empty()
                && baseline.len() >= plan.runs_per_block
                && candidate.len() >= plan.runs_per_block
        })
        .unzip();
    let count = baseline_blocks.len();
    if count == 0 {
        return None;
    }
    let baseline_values: Vec<f64> = baseline_blocks.iter().flatten().copied().collect();
    let candidate_values: Vec<f64> = candidate_blocks.iter().flatten().copied().collect();
    let bootstrap = stats::paired_bootstrap(
        &baseline_blocks[..count],
        &candidate_blocks[..count],
        10_000,
        plan.order_seed,
    );
    let baseline_median = stats::median(&baseline_values);
    Some(Comparison {
        baseline: executable_summary(baseline).ok()?,
        baseline_median_ms: baseline_median,
        baseline_p95_ms: stats::p95(&baseline_values),
        baseline_sample_count: baseline_values.len(),
        median_increase_ms: stats::median(&candidate_values) - baseline_median,
        bootstrap: super::record::BootstrapRecord {
            resamples: bootstrap.resamples,
            confidence: bootstrap.confidence,
            method: bootstrap.method.to_string(),
            lower_ms: bootstrap.lower_ms,
            upper_ms: bootstrap.upper_ms,
        },
        blocks_paired: count,
        regression_blocked: bootstrap.regression_blocked(baseline_median),
    })
}

/// The build profile an executable path observably sits in. The
/// registration declares the release policy; the record states what this
/// run actually measured, because a debug build against a release budget
/// must be visible, not blended in.
fn observed_profile(path: &Path) -> String {
    let text = path.to_string_lossy();
    if text.contains("/target/debug/") {
        "debug (diagnostic run; the release budgets still apply)".to_string()
    } else if text.contains("/target/release/") {
        "release".to_string()
    } else {
        "unidentified build; verify the flags before gating".to_string()
    }
}

/// The environment summary a record carries.
fn environment_summary_of(
    environment: &EnvironmentRecord,
    environment_path: &Path,
) -> EnvironmentSummary {
    EnvironmentSummary {
        path: environment_path.to_path_buf(),
        git_version: environment
            .software
            .as_ref()
            .and_then(|software| software.git_version.clone()),
        rustc_version: environment
            .software
            .as_ref()
            .and_then(|software| software.rustc_version.clone()),
        cargo_version: environment
            .software
            .as_ref()
            .and_then(|software| software.cargo_version.clone()),
        kernel: environment
            .software
            .as_ref()
            .and_then(|software| software.kernel.clone()),
        os: environment
            .software
            .as_ref()
            .and_then(|software| software.os.clone()),
        dependency_lock_sha256: environment
            .build
            .as_ref()
            .and_then(|build| build.dependency_lock_sha256.clone()),
        build_profile: None,
        rustflags: environment
            .build
            .as_ref()
            .map(|build| build.rustflags.clone()),
        harness_revision: super::HARNESS_REVISION.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unassigned_environment_refuses_to_measure() {
        let record = EnvironmentRecord::unassigned();
        let problem = require_registered(&record).unwrap_err();
        assert!(
            problem.contains("unassigned"),
            "the refusal names the state"
        );
        let mut registered = record;
        registered.status = "registered".to_string();
        assert!(require_registered(&registered).is_ok());
    }
}
