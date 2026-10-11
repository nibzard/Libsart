//! Acceptance suite of tasks T053, T074, and T054: the benchmark harness
//! and the environment record of SPEC 17.3-17.8.
//!
//! The suite proves the acceptance criteria against the real pieces:
//!
//! - a registered runner record carries this machine's actual hardware,
//!   software, build, and tool facts, cross-checked independently;
//! - the shipped environment record registers the actual dedicated
//!   runner (T074) with every section probed, and measurement accepts
//!   it, while an `unassigned` record still refuses measurement;
//! - the initial result records are `not_measured` with their budgets
//!   and null numbers;
//! - the timing boundary spans process creation through exit and full
//!   response delivery;
//! - a full run times fresh processes, resets the mutation fixtures
//!   before every iteration, verifies an independent stock-Git oracle
//!   after every run, retains raw paired samples, and reports honest
//!   statuses — a fast but incorrect command fails the benchmark;
//! - the PTY sampler measures launch-to-frame, key-to-bytes, and
//!   edit-to-notice on a real pseudo-terminal (T054, SPEC 17.4), and the
//!   resource sampler reads the kernel's own CPU and memory accounting
//!   of the session's process tree.

#[path = "../bench/perf/mod.rs"]
mod perf;

#[path = "../bench/corpus/gen/mod.rs"]
mod corpus;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use perf::envrec::{EnvironmentRecord, probe_record};
use perf::metric;
use perf::record::{OracleState, ResultRecord, Status};
use perf::runner::{RunPlan, Sides, require_registered};
use perf::samples;

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// One private scratch directory under the run's resource prefix, so
/// generated corpora and records never touch the checkout.
fn scratch(tag: &str) -> PathBuf {
    let prefix = std::env::var("LOOPER_RESOURCE_PREFIX")
        .unwrap_or_else(|_| "looper-margin".to_string());
    let root = std::env::temp_dir().join(format!(
        "{prefix}-t053-{tag}-{}",
        std::process::id()
    ));
    let unique = root.join(format!(
        "{:?}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&unique).expect("the scratch directory exists");
    unique
}

/// Generates one test-scale C1 corpus and returns its root.
fn c1_fixture_dir(tag: &str) -> PathBuf {
    let root = scratch(tag);
    let output = root.join("c1");
    corpus::generate(&corpus::Parameters::c1().test_scale(), &output);
    output
}

/// Registers this machine into `root` and returns the loaded record.
fn register_environment(root: &Path) -> EnvironmentRecord {
    let record = probe_record(
        None,
        root,
        Some(&project_root()),
        Some("acceptance test: quiet run, no competing builds"),
    );
    let path = root.join("environment.json");
    record.write(&path).expect("the record writes");
    EnvironmentRecord::load(&path).expect("the record reads back")
}

/// One measurement plan over the built executable.
fn plan(id: &str, sides: Sides, warmups: usize, blocks: usize, runs: usize) -> RunPlan {
    plan_with(id, sides, warmups, blocks, runs, 0, 0)
}

/// A plan that also pins the P-KEY event count or the P-CPU window.
fn plan_with(
    id: &str,
    sides: Sides,
    warmups: usize,
    blocks: usize,
    runs: usize,
    events_per_session: usize,
    window_seconds: u64,
) -> RunPlan {
    RunPlan {
        metric: metric::by_id(id).expect("the metric exists").id,
        corpus: "C1".to_string(),
        sides,
        warmups,
        blocks,
        runs_per_block: runs,
        order_seed: 20261011,
        events_per_session,
        window_seconds,
        implementation_commit: None,
        notes: Vec::new(),
    }
}

/// One synthetic sample row, so the record arithmetic is tested without
/// running sessions.
fn synthetic(metric: &str, order: usize, block: usize, warmup: bool, value: f64) -> samples::Sample {
    samples::Sample {
        schema_version: samples::SAMPLES_SCHEMA_VERSION,
        metric: metric.to_string(),
        corpus: "C1".to_string(),
        version: "single".to_string(),
        block,
        run_in_block: order % 100,
        order_index: order,
        warmup,
        elapsed_ms: value,
        unit: None,
        exit_code: 0,
        stdout_bytes: 0,
        stderr_bytes: 0,
        oracle: "passed".to_string(),
        oracle_failures: Vec::new(),
        cache_method: "warm".to_string(),
        invalidated: false,
        invalidation_reason: None,
        at: "2026-10-11T00:00:00Z".to_string(),
    }
}

// ---------------------------------------------------------------------------
// The environment record (SPEC 17.6)
// ---------------------------------------------------------------------------

#[test]
fn the_shipped_environment_record_registers_the_dedicated_runner() {
    let path = project_root().join("bench/environment.json");
    let record = EnvironmentRecord::load(&path).expect("the shipped record parses");
    // T074: the operator registered the actual dedicated runner, so the
    // shipped record is live evidence, and measurement accepts it.
    assert_eq!(record.status, "registered", "the dedicated runner is registered");
    assert_eq!(record.schema_version, 1);
    assert!(record.is_registered());
    require_registered(&record).expect("measurement accepts the registered runner");
    assert!(
        record.runner_id.as_deref().is_some_and(|id| !id.is_empty()),
        "the runner has a stable identifier"
    );
    assert_eq!(
        record.registered_at.as_deref().map(|at| at.ends_with('Z')),
        Some(true),
        "the registration time is RFC 3339 UTC"
    );
    assert_eq!(
        record.matches_declared_class,
        Some(true),
        "the registered machine is the declared developer-laptop-class linux x86-64 runner"
    );
    assert!(
        record
            .observed_class
            .as_deref()
            .is_some_and(|class| !class.is_empty()),
        "the observed class names what actually registered"
    );

    // Every section SPEC 17.6 requires is present; a failed probe leaves
    // a null, and no section may be null in the shipped record.
    let hardware = record.hardware.as_ref().expect("hardware is probed");
    assert!(hardware.cpu_cores.expect("cores exist") > 0);
    assert!(hardware.ram_total_bytes.expect("ram exists") > 0);
    assert!(!hardware.cpu_model.as_deref().unwrap_or_default().is_empty());
    let storage = hardware.storage.as_ref().expect("storage is probed");
    assert!(
        !storage.device.is_empty() && !storage.filesystem.is_empty(),
        "the storage and its filesystem are recorded"
    );
    let software = record.software.as_ref().expect("software is probed");
    for fact in [&software.kernel, &software.os, &software.git_version, &software.rustc_version] {
        assert!(
            fact.as_deref().is_some_and(|fact| !fact.is_empty()),
            "a core software fact is missing"
        );
    }
    let build = record.build.as_ref().expect("build is probed");
    assert!(
        build
            .dependency_lock_sha256
            .as_deref()
            .is_some_and(|hash| hash.len() == 64),
        "the dependency lock hash is recorded"
    );
    let tools = record.tools.as_ref().expect("tools are probed");
    assert!(!tools.harness_revision.is_empty());
    assert!(
        record
            .power
            .as_ref()
            .and_then(|power| power.conditions.as_deref())
            .is_some_and(|text| !text.is_empty()),
        "the operator's interference statement is recorded"
    );
    assert!(
        record.terminal.is_some(),
        "the terminal facts are recorded"
    );
    assert!(
        !record.notes.is_empty(),
        "the record states how its fields were obtained"
    );

    // An unassigned record still refuses: the registration gate is the
    // record's state, not this suite's goodwill.
    let mut unregistered = record.clone();
    unregistered.status = "unassigned".to_string();
    let refusal = require_registered(&unregistered).expect_err("measurement refuses");
    assert!(
        refusal.contains("unassigned"),
        "the refusal explains the state: {refusal}"
    );
}

#[test]
fn a_registered_runner_record_carries_actual_details() {
    let root = scratch("env");
    let record = register_environment(&root);
    assert_eq!(record.status, "registered");
    assert!(record.is_registered());
    assert!(
        record
            .runner_id
            .as_deref()
            .is_some_and(|id| id.starts_with("margin-runner-")),
        "a stable identifier is assigned"
    );
    assert_eq!(
        record.registered_at.as_deref().map(|at| at.ends_with('Z')),
        Some(true),
        "the registration time is recorded"
    );

    // Hardware: actual values, cross-checked against the same sources.
    let hardware = record.hardware.as_ref().expect("hardware is probed");
    assert_eq!(hardware.architecture, std::env::consts::ARCH);
    assert!(hardware.cpu_cores.expect("cores exist") > 0);
    assert!(hardware.ram_total_bytes.expect("ram exists") > 0);
    let storage = hardware.storage.as_ref().expect("storage is probed");
    assert!(!storage.mount.is_empty() && !storage.filesystem.is_empty());
    if cfg!(target_os = "linux") {
        let cpuinfo = fs::read_to_string("/proc/cpuinfo").expect("cpuinfo exists");
        if let Some(model) = hardware.cpu_model.as_deref() {
            assert!(
                cpuinfo.contains(model),
                "the CPU model {model} is transcribed, not probed"
            );
        }
        let cores = cpuinfo.lines().filter(|line| line.starts_with("processor")).count();
        assert_eq!(hardware.cpu_cores, Some(cores as u32));
    }

    // Software: the tools this machine really runs.
    let software = record.software.as_ref().expect("software is probed");
    let git = Command::new("git").arg("--version").output().expect("git runs");
    let git = String::from_utf8_lossy(&git.stdout).trim().to_string();
    let git = git.strip_prefix("git version ").unwrap_or(&git).to_string();
    assert_eq!(software.git_version.as_deref(), Some(git.as_str()));
    assert!(
        software
            .rustc_version
            .as_deref()
            .is_some_and(|version| version.starts_with("rustc ")),
        "the Rust version is probed"
    );
    assert!(
        software
            .kernel
            .as_deref()
            .is_some_and(|kernel| !kernel.is_empty())
    );

    // Build: the lock hash is this checkout's lock hash.
    let build = record.build.as_ref().expect("build is probed");
    let lock = fs::read(project_root().join("Cargo.lock")).expect("the lockfile exists");
    assert_eq!(
        build.dependency_lock_sha256.as_deref(),
        Some(perf::hash::sha256_hex(&lock).as_str()),
        "the dependency lock hash matches Cargo.lock"
    );

    // Tools and power: recorded, with absent tools named honestly.
    let tools = record.tools.as_ref().expect("tools are probed");
    assert_eq!(tools.harness_revision, perf::HARNESS_REVISION);
    assert!(
        tools.hyperfine == "absent" || tools.hyperfine.starts_with("hyperfine"),
        "an absent tool is named, not guessed: {}",
        tools.hyperfine
    );
    let power = record.power.as_ref().expect("power is probed");
    assert!(
        power
            .conditions
            .as_deref()
            .is_some_and(|text| !text.is_empty()),
        "the operator's interference statement is recorded"
    );
}

// ---------------------------------------------------------------------------
// The result records (SPEC 17.8)
// ---------------------------------------------------------------------------

#[test]
fn the_initial_result_records_are_not_measured_with_their_budgets() {
    let results = project_root().join("bench/results");
    let mut files = 0;
    for definition in metric::ALL {
        for label in metric::RECORD_CORPORA {
            let path = results.join(ResultRecord::file_name(definition.id, label));
            let Ok(record) = ResultRecord::load(&path) else {
                continue;
            };
            let budget = definition.budget(label);
            if budget.is_none() {
                panic!(
                    "{} on {} has no budget, so no record belongs here",
                    definition.id, label
                );
            }
            files += 1;
            assert_eq!(record.status, Status::NotMeasured, "{}.{}", definition.id, label);
            assert_eq!(record.environment_id, None);
            assert_eq!(record.implementation_commit, None);
            assert_eq!(record.executable_hash, None);
            assert_eq!(record.corpus_manifest_hash, None);
            assert_eq!(record.sample_count, 0);
            assert_eq!(record.median_ms, None, "never zero or presumed passing");
            assert_eq!(record.p95_ms, None, "never zero or presumed passing");
            assert_eq!(record.correctness_oracle, OracleState::NotRun);
            assert_eq!(record.raw_samples_path, None);
            assert_eq!(record.details, None);
            assert_eq!(
                record.budget_p95_ms,
                budget.and_then(|budget| budget.p95_ms()),
                "the budget is visible before any number exists"
            );
        }
    }
    assert_eq!(files, 24, "every primary metric on both budget corpora");

    // The SPEC 17.8 initial example, verbatim.
    let example = ResultRecord::load(&results.join("P-DIFF.C2.json")).expect("the example exists");
    assert_eq!(example.metric, "P-DIFF");
    assert_eq!(example.corpus, "C2");
    assert_eq!(example.status, Status::NotMeasured);
    assert_eq!(example.budget_p95_ms, Some(500));
    assert_eq!(example.correctness_oracle, OracleState::NotRun);

    // A resource metric carries no latency budget.
    let memory = ResultRecord::load(&results.join("P-TREE-MEM.C2.json")).expect("the record exists");
    assert_eq!(memory.budget_p95_ms, None);
}

#[test]
fn a_not_measured_registry_still_distinguishes_every_status() {
    let record = ResultRecord::not_measured("P-KEY", "C1");
    let body = serde_json::to_string(&record).expect("the record serializes");
    for status in ["not_measured", "passed", "failed", "inconclusive"] {
        let spelled = match status {
            "not_measured" => Status::NotMeasured,
            "passed" => Status::Passed,
            "failed" => Status::Failed,
            _ => Status::Inconclusive,
        };
        let text = serde_json::to_string(&spelled).expect("the status serializes");
        assert!(text.contains(status), "{status} is representable");
    }
    assert!(body.contains("\"not_measured\""));
}

// ---------------------------------------------------------------------------
// Measurement boundaries and gating (SPEC 17.3, 17.6)
// ---------------------------------------------------------------------------

#[test]
fn the_boundary_spans_creation_through_exit_and_full_delivery() {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg("head -c 150000 /dev/zero | tr '\\0' m; sleep 0.02; printf tail");
    let timed = perf::timing::time_child(&mut command).expect("the child runs");
    assert_eq!(timed.exit, Some(0));
    assert_eq!(timed.stdout.len(), 150_004, "the whole response is delivered");
    assert!(timed.stdout.ends_with(b"tail"));
    assert!(
        timed.elapsed_ms() >= 20.0,
        "a 20 ms child completed in {} ms; the window excludes its work",
        timed.elapsed_ms()
    );
}

#[test]
fn an_unassigned_environment_blocks_measurement_without_writing_records() {
    let root = scratch("gate");
    let results = root.join("results");
    fs::create_dir_all(&results).expect("the results directory exists");
    let environment = EnvironmentRecord::unassigned();
    let path = root.join("environment.json");
    environment.write(&path).expect("the record writes");

    let corpus = c1_fixture_dir("gate");
    let fixture = perf::fixture::Fixture::prepare(&corpus, &root.join("fixture"))
        .expect("the fixture prepares");
    let run = plan(
        "P-STATUS",
        Sides::Single(PathBuf::from(env!("CARGO_BIN_EXE_margin"))),
        0,
        1,
        1,
    );
    let refusal = perf::runner::run(&run, &fixture, &environment, &path, &results)
        .expect_err("measurement refuses an unassigned record");
    assert!(refusal.contains("unassigned"), "the refusal is explicit: {refusal}");
    assert!(
        results.read_dir().expect("the results directory reads").count() == 0,
        "nothing was written; the record stays not_measured"
    );
}

/// Prepares a registered fixture set for the sampler acceptance tests.
fn sampler_fixture(tag: &str) -> (perf::fixture::Fixture, EnvironmentRecord, PathBuf, PathBuf) {    let root = scratch(tag);
    let results = root.join("results");
    fs::create_dir_all(&results).expect("the results directory exists");
    let environment = register_environment(&root);
    let path = root.join("environment.json");
    let corpus = c1_fixture_dir(tag);
    let fixture = perf::fixture::Fixture::prepare(&corpus, &root.join("fixture"))
        .expect("the fixture prepares");
    (fixture, environment, path, results)
}

#[test]
fn paired_plans_are_refused_for_pty_and_resource_metrics() {
    let (fixture, environment, path, results) = sampler_fixture("paired-refusal");
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_margin"));
    for id in ["P-OPEN", "P-KEY", "P-REFRESH", "P-CPU", "P-RSS", "P-TREE-MEM"] {
        let run = plan(
            id,
            Sides::Paired {
                baseline: executable.clone(),
                candidate: executable.clone(),
            },
            0,
            1,
            1,
        );
        let refusal = perf::runner::run(&run, &fixture, &environment, &path, &results)
            .expect_err("a PTY or resource pairing is refused, not faked");
        assert!(
            refusal.contains("paired"),
            "{id} names the pairing problem: {refusal}"
        );
        assert!(
            !results.join(ResultRecord::file_name(id, "C1")).exists(),
            "{id} was never written"
        );
    }
}

#[test]
fn p_open_measures_launch_to_the_first_useful_frame() {
    let (fixture, environment, path, results) = sampler_fixture("p-open");
    let run = plan(
        "P-OPEN",
        Sides::Single(PathBuf::from(env!("CARGO_BIN_EXE_margin"))),
        0,
        1,
        2,
    );
    let outcome = perf::runner::run(&run, &fixture, &environment, &path, &results)
        .expect("the PTY sampler measures P-OPEN");

    // Two sessions, one sample each: the launch-to-frame window.
    let raw = samples::read_jsonl(&outcome.raw_path).expect("the raw stream reads back");
    assert_eq!(raw.len(), 2);
    for sample in &raw {
        assert!(sample.elapsed_ms > 0.0, "each launch was timed");
        assert_eq!(sample.unit, None, "P-OPEN is a latency in milliseconds");
        assert_eq!(
            sample.oracle,
            "passed",
            "the frame showed identity, content, state, and actions: {:?}",
            sample.oracle_failures
        );
        assert_eq!(sample.exit_code, 0, "the session quit cleanly");
    }
    let record = &outcome.record;
    let details = record.details.as_ref().expect("measured records carry details");
    assert_eq!(details.metric_kind, "pty-interaction");
    assert_eq!(details.memory_definition, None, "a latency record defines no memory");
    assert!(
        details
            .sampling
            .boundary
            .contains("identity, content, save state, and actions"),
        "the boundary names the useful frame: {}",
        details.sampling.boundary
    );
    assert!(
        details
            .sampling
            .cache_method
            .contains("--reduced-updates"),
        "the session shape is recorded"
    );
    // A two-session diagnostic run never passes, and its gate states
    // exactly what the samples show.
    assert!(
        matches!(record.status, Status::Failed | Status::Inconclusive),
        "a below-policy run never passes: {:?}",
        record.status
    );
    let p95 = record.p95_ms.expect("the p95 is measured");
    assert_eq!(
        details.gate.budget_exceeded,
        p95 > record.budget_p95_ms.unwrap_or(u64::MAX) as f64,
        "the gate states exactly what the samples show"
    );
}

#[test]
fn p_key_times_one_sample_per_navigation_event() {
    let (fixture, environment, path, results) = sampler_fixture("p-key");
    // Two sessions of twenty events each, in two blocks: every event is
    // one row, its block is its session's block, and its run index is its
    // place inside the session.
    let run = plan_with(
        "P-KEY",
        Sides::Single(PathBuf::from(env!("CARGO_BIN_EXE_margin"))),
        0,
        2,
        1,
        20,
        0,
    );
    let outcome = perf::runner::run(&run, &fixture, &environment, &path, &results)
        .expect("the PTY sampler measures P-KEY");

    let raw = samples::read_jsonl(&outcome.raw_path).expect("the raw stream reads back");
    assert_eq!(raw.len(), 40, "two sessions of twenty events");
    let order: Vec<usize> = raw.iter().map(|sample| sample.order_index).collect();
    let mut sorted_order = order.clone();
    sorted_order.sort_unstable();
    assert_eq!(order, sorted_order, "the execution order is recorded in order");
    for (index, sample) in raw.iter().enumerate() {
        assert_eq!(sample.block, index / 20, "the block is the session's block");
        assert_eq!(sample.run_in_block, index % 20, "the run index is the event's place");
        assert_eq!(sample.unit, None);
        assert!(
            sample.oracle == "passed" || sample.elapsed_ms > 0.0,
            "a timeout row keeps its timeout as evidence"
        );
    }
    assert!(
        raw.iter().all(|sample| sample.oracle == "passed"),
        "every event drew its changed frame: {:?}",
        raw.iter()
            .filter(|sample| !sample.oracle_failures.is_empty())
            .map(|sample| &sample.oracle_failures[0])
            .take(3)
            .collect::<Vec<_>>()
    );
    // The read-only session left the repository untouched.
    assert!(
        fixture.oracle.status_porcelain().trim().is_empty(),
        "j and k changed no file"
    );

    let record = &outcome.record;
    let details = record.details.as_ref().expect("measured records carry details");
    assert_eq!(details.metric_kind, "pty-interaction");
    assert_eq!(details.sampling.events_per_session, 20);
    assert_eq!(
        details.statistics.as_ref().expect("statistics exist").p99_ms,
        None,
        "a forty-event series does not borrow a p99"
    );
    assert!(
        details
            .notes
            .iter()
            .any(|note| note.contains("10,000")),
        "the event floor is named: {:?}",
        details.notes
    );
    assert!(
        matches!(record.status, Status::Failed | Status::Inconclusive),
        "a below-policy run never passes: {:?}",
        record.status
    );
}

#[test]
fn p_refresh_times_the_external_edit_to_the_arrival_notice() {
    let (fixture, environment, path, results) = sampler_fixture("p-refresh");
    let run = plan(
        "P-REFRESH",
        Sides::Single(PathBuf::from(env!("CARGO_BIN_EXE_margin"))),
        0,
        1,
        1,
    );
    let outcome = perf::runner::run(&run, &fixture, &environment, &path, &results)
        .expect("the PTY sampler measures P-REFRESH");

    let raw = samples::read_jsonl(&outcome.raw_path).expect("the raw stream reads back");
    assert_eq!(raw.len(), 1);
    let sample = &raw[0];
    assert!(sample.elapsed_ms > 0.0, "the discovery window was timed");
    assert_eq!(sample.unit, None);
    assert_eq!(
        sample.oracle,
        "passed",
        "the notice arrived and the oracle held: {:?}",
        sample.oracle_failures
    );
    // The pinned state survived the edit: HEAD never moved, and the
    // working tree holds exactly the appended paragraph.
    assert_eq!(fixture.oracle.head(), fixture.facts.candidate);
    assert_eq!(
        fixture.oracle.status_porcelain().trim(),
        format!("M {}", fixture.facts.selected_path),
        "exactly the selected document is dirty"
    );
    let record = &outcome.record;
    let details = record.details.as_ref().expect("measured records carry details");
    assert_eq!(details.metric_kind, "pty-interaction");
    assert!(
        details.sampling.boundary.contains("arrival notice"),
        "the boundary names the notice: {}",
        details.sampling.boundary
    );
    assert!(
        matches!(record.status, Status::Failed | Status::Inconclusive),
        "a below-policy run never passes: {:?}",
        record.status
    );
}

#[test]
fn the_memory_resources_report_their_quantities_in_mib() {
    let (fixture, environment, path, results) = sampler_fixture("memory");
    for (id, ceiling, marker) in
        [("P-RSS", 64.0, "VmHWM"), ("P-TREE-MEM", 128.0, "concurrent sum")]
    {
        let run = plan(
            id,
            Sides::Single(PathBuf::from(env!("CARGO_BIN_EXE_margin"))),
            0,
            1,
            1,
        );
        let outcome = perf::runner::run(&run, &fixture, &environment, &path, &results)
            .expect("the resource sampler measures the metric");

        let raw = samples::read_jsonl(&outcome.raw_path).expect("the raw stream reads back");
        assert_eq!(raw.len(), 1, "{id} ran one session");
        let sample = &raw[0];
        assert_eq!(sample.unit.as_deref(), Some("MiB"), "{id} names its unit");
        assert!(sample.elapsed_ms > 0.0, "{id} measured a positive quantity");
        assert_eq!(
            sample.oracle,
            "passed",
            "{id} verified: {:?}",
            sample.oracle_failures
        );
        assert_eq!(sample.exit_code, 0, "the session quit cleanly");

        let record = &outcome.record;
        let details = record.details.as_ref().expect("measured records carry details");
        assert_eq!(details.metric_kind, "resource");
        // A resource quantity is not a latency: the base fields stay
        // empty and the numbers live in the statistics block, whose
        // method sentence names the unit.
        assert_eq!(record.median_ms, None, "{id} carries no latency");
        assert_eq!(record.p95_ms, None, "{id} carries no latency");
        let statistics = details.statistics.as_ref().expect("the numbers exist");
        assert!(statistics.method.contains("MiB"), "{id} names its unit");
        assert!(statistics.max_ms > 0.0);
        // The gate is the maximum counted sample against the ceiling,
        // never a percentile.
        assert_eq!(
            details.gate.budget_exceeded,
            statistics.max_ms > ceiling,
            "{id} gates on the maximum"
        );
        let definition = details.memory_definition.as_ref().expect("{id} defines its memory");
        assert!(definition.contains(marker), "{id} says what it summed: {definition}");
        assert!(
            matches!(record.status, Status::Failed | Status::Inconclusive),
            "a below-policy run never passes: {:?}",
            record.status
        );
    }
}

#[test]
fn p_cpu_reports_the_idle_share_of_one_core() {
    let (fixture, environment, path, results) = sampler_fixture("p-cpu");
    // A four-second window is diagnostic scale: it can never stand in
    // for the five-minute definition.
    let run = plan_with(
        "P-CPU",
        Sides::Single(PathBuf::from(env!("CARGO_BIN_EXE_margin"))),
        0,
        1,
        1,
        0,
        4,
    );
    let outcome = perf::runner::run(&run, &fixture, &environment, &path, &results)
        .expect("the resource sampler measures P-CPU");

    let raw = samples::read_jsonl(&outcome.raw_path).expect("the raw stream reads back");
    assert_eq!(raw.len(), 1);
    let sample = &raw[0];
    assert_eq!(
        sample.unit.as_deref(),
        Some("percent-of-one-core"),
        "P-CPU names its unit"
    );
    assert!(sample.elapsed_ms >= 0.0, "a share can round to zero at this scale");
    let record = &outcome.record;
    let details = record.details.as_ref().expect("measured records carry details");
    assert_eq!(details.metric_kind, "resource");
    assert_eq!(record.median_ms, None, "P-CPU carries no latency");
    let definition = details.memory_definition.as_ref().expect("P-CPU defines its accounting");
    assert!(
        definition.contains("CLK_TCK"),
        "the accounting names its ticks: {definition}"
    );
    assert_eq!(details.sampling.window_seconds, 4);
    assert!(
        details
            .notes
            .iter()
            .any(|note| note.contains("five-minute")),
        "the diagnostic window is named: {:?}",
        details.notes
    );
    assert!(
        matches!(record.status, Status::Failed | Status::Inconclusive),
        "a diagnostic window never passes: {:?}",
        record.status
    );
}

#[test]
fn a_resource_ceiling_gates_on_the_maximum_counted_sample() {
    let (fixture, environment, path, results) = sampler_fixture("ceiling");
    // One warmup and four blocks of three runs satisfy the resource
    // policy, so only the gate decides.
    let mut rows = vec![synthetic("P-RSS", 0, 0, true, 9.9)];
    let mut order = 1usize;
    for block in 0..4 {
        for _ in 0..3 {
            let mut row = synthetic("P-RSS", order, block, false, 30.0 + block as f64);
            row.unit = Some("MiB".to_string());
            rows.push(row);
            order += 1;
        }
    }
    let within = plan(
        "P-RSS",
        Sides::Single(PathBuf::from(env!("CARGO_BIN_EXE_margin"))),
        1,
        4,
        3,
    );
    let raw_path = results.join("synthetic-within.jsonl");
    samples::write_jsonl(&raw_path, &rows).expect("the stream writes");
    let outcome =
        perf::runner::finish_record(&within, &fixture, &environment, &path, &rows, &raw_path);
    assert_eq!(outcome.record.status, Status::Passed, "every sample sat inside");
    assert_eq!(outcome.record.median_ms, None, "a resource record carries no latency");
    let details = outcome.record.details.as_ref().expect("the details exist");
    assert!(!details.gate.budget_exceeded);
    assert_eq!(details.statistics.as_ref().expect("statistics").max_ms, 33.0);

    // One wild run over the ceiling fails the benchmark even though the
    // p95 of the series sits far below it: a percentile would hide what a
    // ceiling exists to catch.
    if let Some(last) = rows.last_mut() {
        last.elapsed_ms = 70.0;
    }
    let raw_path = results.join("synthetic-over.jsonl");
    samples::write_jsonl(&raw_path, &rows).expect("the stream writes");
    let outcome =
        perf::runner::finish_record(&within, &fixture, &environment, &path, &rows, &raw_path);
    assert_eq!(outcome.record.status, Status::Failed, "the maximum crossed 64 MiB");
    let details = outcome.record.details.as_ref().expect("the details exist");
    assert!(details.gate.budget_exceeded);
    assert!(
        details
            .notes
            .iter()
            .any(|note| note.contains("maximum counted sample")),
        "the note names the maximum: {:?}",
        details.notes
    );
}

#[test]
fn the_p_key_p99_gate_fires_from_the_large_event_series() {
    let (fixture, environment, path, results) = sampler_fixture("p99");
    // The policy shape: five warmup sessions, then twenty blocks of five
    // sessions of one hundred events — ten thousand counted events.
    let mut rows = Vec::new();
    let mut order = 0usize;
    for _ in 0..5 {
        for _ in 0..100 {
            rows.push(synthetic("P-KEY", order, 0, true, 10.0));
            order += 1;
        }
    }
    let mut counted = 0usize;
    for block in 0..20 {
        for _ in 0..5 {
            for _ in 0..100 {
                // The slowest 150 events sit at 500 ms: the p95 stays
                // inside its 50 ms budget while the p99 crosses its
                // 100 ms budget.
                let value = if counted >= 9_850 { 500.0 } else { 10.0 };
                rows.push(synthetic("P-KEY", order, block, false, value));
                order += 1;
                counted += 1;
            }
        }
    }
    let run = plan(
        "P-KEY",
        Sides::Single(PathBuf::from(env!("CARGO_BIN_EXE_margin"))),
        5,
        20,
        5,
    );
    let raw_path = results.join("synthetic-p99.jsonl");
    samples::write_jsonl(&raw_path, &rows).expect("the stream writes");
    let outcome = perf::runner::finish_record(&run, &fixture, &environment, &path, &rows, &raw_path);
    let record = &outcome.record;
    let details = record.details.as_ref().expect("the details exist");
    let statistics = details.statistics.as_ref().expect("the statistics exist");
    assert_eq!(statistics.sample_count, 10_000);
    assert_eq!(statistics.p95_ms, 10.0, "the p95 stays inside its budget");
    assert_eq!(statistics.p99_ms, Some(500.0), "the p99 exists at policy scale");
    assert_eq!(record.status, Status::Failed, "the p99 crossed its 100 ms budget");
    assert!(details.gate.budget_exceeded);
    assert!(
        details
            .notes
            .iter()
            .any(|note| note.contains("the p99 exceeded")),
        "the note names the tail: {:?}",
        details.notes
    );
}

// ---------------------------------------------------------------------------
// Full runs: timing, reset, oracles, paired samples (SPEC 17.7)
// ---------------------------------------------------------------------------

/// Runs one metric end to end over a fresh fixture and returns the
/// outcome.
fn measure(
    tag: &str,
    id: &str,
    sides: Sides,
    warmups: usize,
    blocks: usize,
    runs: usize,
) -> (perf::runner::RunOutcome, EnvironmentRecord, PathBuf) {
    let root = scratch(tag);
    let results = root.join("results");
    fs::create_dir_all(&results).expect("the results directory exists");
    let environment = register_environment(&root);
    let path = root.join("environment.json");
    let corpus = c1_fixture_dir(tag);
    let fixture = perf::fixture::Fixture::prepare(&corpus, &root.join("fixture"))
        .expect("the fixture prepares");
    let run = plan(id, sides, warmups, blocks, runs);
    let outcome = perf::runner::run(&run, &fixture, &environment, &path, &results)
        .expect("the measurement runs");
    (outcome, environment, results)
}

#[test]
fn a_query_run_records_paired_samples_and_an_honest_verdict() {
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_margin"));
    let (outcome, environment, _results) = measure(
        "query",
        "P-STATUS",
        Sides::Paired {
            baseline: executable.clone(),
            candidate: executable,
        },
        1,
        2,
        2,
    );

    // Raw paired samples: warmups retained, both versions present,
    // global order recorded, every run timed and oracle-verified.
    let raw = samples::read_jsonl(&outcome.raw_path).expect("the raw stream reads back");
    assert_eq!(raw.len(), 1 + 1 + 2 * 2 * 2, "warmups and every measured run");
    assert!(raw.iter().any(|sample| sample.warmup), "warmups are retained");
    let versions: Vec<&str> = {
        let mut seen: Vec<&str> = raw.iter().map(|sample| sample.version.as_str()).collect();
        seen.sort_unstable();
        seen.dedup();
        seen
    };
    assert_eq!(versions, ["baseline", "candidate"], "both sides ran");
    let order: Vec<usize> = raw.iter().map(|sample| sample.order_index).collect();
    let mut sorted_order = order.clone();
    sorted_order.sort_unstable();
    assert_eq!(order, sorted_order, "the execution order is recorded in order");
    for sample in &raw {
        assert!(sample.elapsed_ms > 0.0, "every run is timed");
        assert_eq!(sample.cache_method, "warm", "the cache method is the SPEC 17.3 warm CLI");
        assert_eq!(sample.oracle, "passed", "every run passed its oracle");
    }

    // The record: measured, with the sampling policy and comparison.
    // The verdict of a shortened debug run is never `passed`: it is
    // inconclusive below the policy, or failed when the p95 exceeds the
    // budget, and the gate must agree with its own arithmetic.
    let record = &outcome.record;
    assert_eq!(record.correctness_oracle, OracleState::Passed);
    assert!(
        matches!(record.status, Status::Failed | Status::Inconclusive),
        "a below-policy run never passes: {:?}",
        record.status
    );
    let details = record.details.as_ref().expect("measured records carry details");
    let p95 = record.p95_ms.expect("the p95 is measured");
    assert_eq!(
        details.gate.budget_exceeded,
        p95 > record.budget_p95_ms.unwrap_or(u64::MAX) as f64,
        "the gate states exactly what the samples show"
    );
    assert_eq!(record.sample_count, 4);
    assert!(record.median_ms.unwrap_or(0.0) > 0.0);
    assert!(record.p95_ms.unwrap_or(0.0) >= record.median_ms.unwrap_or(0.0));
    assert_eq!(record.budget_p95_ms, Some(150));
    assert_eq!(record.environment_id, environment.runner_id);
    assert!(record.executable_hash.as_deref().is_some_and(|hash| hash.len() == 64));
    assert!(record.corpus_manifest_hash.as_deref().is_some_and(|hash| hash.len() == 64));
    assert_eq!(details.metric_kind, "cli-fresh-process");
    assert_eq!(details.sampling.warmups, 1);
    assert_eq!(details.sampling.order_seed, 20261011);
    let comparison = details.comparison.as_ref().expect("paired records compare");
    assert_eq!(comparison.bootstrap.resamples, 10_000, "the predeclared bootstrap");
    assert_eq!(comparison.bootstrap.confidence, 99);
    assert_eq!(comparison.baseline_sample_count, 4);
    assert!(!comparison.regression_blocked, "one executable cannot regress on itself");
    assert!(
        details
            .notes
            .iter()
            .any(|note| note.contains("below the SPEC 17.7.3 policy")),
        "a shortened run is marked inconclusive evidence"
    );
}

#[test]
fn a_checkpoint_run_resets_the_dirty_state_before_every_iteration() {
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_margin"));
    let (outcome, _environment, _results) = measure(
        "checkpoint",
        "P-CHECKPOINT",
        Sides::Single(executable),
        1,
        2,
        2,
    );

    // Every iteration began from the intended dirty state and committed
    // it: had the reset failed, the second checkpoint would find nothing
    // eligible and its oracle would fail.
    let raw = samples::read_jsonl(&outcome.raw_path).expect("the raw stream reads back");
    assert_eq!(raw.len(), 5);
    for sample in &raw {
        assert_eq!(
            sample.oracle, "passed",
            "iteration {} verified: {:?}",
            sample.order_index, sample.oracle_failures
        );
    }
    let record = &outcome.record;
    assert_eq!(record.correctness_oracle, OracleState::Passed);
    assert_eq!(record.metric, "P-CHECKPOINT");
    assert!(
        matches!(record.status, Status::Failed | Status::Inconclusive),
        "a below-policy diagnostic run never passes: {:?}",
        record.status
    );
}

#[test]
fn a_selective_review_run_matches_the_independent_plumbing() {
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_margin"));
    let root = scratch("apply");
    let results = root.join("results");
    fs::create_dir_all(&results).expect("the results directory exists");
    let environment = register_environment(&root);
    let path = root.join("environment.json");
    let corpus = c1_fixture_dir("apply");
    let fixture = perf::fixture::Fixture::prepare(&corpus, &root.join("fixture"))
        .expect("the fixture prepares");
    let run = plan("P-APPLY", Sides::Single(executable), 1, 2, 2);
    let outcome = perf::runner::run(&run, &fixture, &environment, &path, &results)
        .expect("the measurement runs");

    let raw = samples::read_jsonl(&outcome.raw_path).expect("the raw stream reads back");
    for sample in &raw {
        assert_eq!(
            sample.oracle, "passed",
            "the selective review verified: {:?}",
            sample.oracle_failures
        );
    }
    assert_eq!(outcome.record.correctness_oracle, OracleState::Passed);
    assert_eq!(outcome.record.budget_p95_ms, Some(750));

    // The fixture's last state is the last verified review: one commit
    // on the candidate, whose tree the oracle derived without Margin.
    let head = fixture.oracle.head();
    assert_ne!(head, fixture.facts.candidate, "the review moved HEAD");
    assert_eq!(
        fixture.oracle.ok(&["rev-parse", "HEAD^"]),
        fixture.facts.candidate
    );
    assert_eq!(
        fixture.oracle.tree_of("HEAD"),
        fixture.review_expected_tree,
        "the result tree matches independent index plumbing"
    );
}

#[test]
fn a_fast_incorrect_command_fails_the_benchmark() {
    // `/bin/false` is as fast as a command can be and always wrong: the
    // oracle must fail the run regardless of its speed (SPEC 17.7.6).
    let (outcome, _environment, _results) = measure(
        "incorrect",
        "P-HELP",
        Sides::Single(PathBuf::from("/bin/false")),
        0,
        1,
        2,
    );
    let raw = samples::read_jsonl(&outcome.raw_path).expect("the raw stream reads back");
    assert_eq!(raw.len(), 2, "failed runs are retained, not discarded");
    for sample in &raw {
        assert_eq!(sample.oracle, "failed");
        assert!(
            sample
                .oracle_failures
                .iter()
                .any(|failure| failure.contains("exit code 1")),
            "the failure names the exit: {:?}",
            sample.oracle_failures
        );
    }
    let record = &outcome.record;
    assert_eq!(record.correctness_oracle, OracleState::Failed);
    assert_eq!(record.status, Status::Failed, "incorrect cannot pass by being fast");
}

#[test]
fn invalidating_a_block_preserves_its_samples_and_recomputes() {
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_margin"));
    let (outcome, _environment, _results) = measure(
        "invalidate",
        "P-HELP",
        Sides::Single(executable),
        0,
        2,
        2,
    );
    let raw_path = outcome.raw_path.clone();
    let record_path = outcome.record_path.clone();
    let before = samples::read_jsonl(&raw_path).expect("the raw stream reads back");
    let counted_before = outcome.record.sample_count;

    perf::runner::invalidate_block(&raw_path, &record_path, 1, "a documented build storm")
        .expect("the invalidation applies");

    // Preserved, marked, excluded.
    let after = samples::read_jsonl(&raw_path).expect("the raw stream still reads");
    assert_eq!(after.len(), before.len(), "no sample is deleted");
    for sample in &after {
        if sample.block == 1 {
            assert!(sample.invalidated);
            assert_eq!(
                sample.invalidation_reason.as_deref(),
                Some("a documented build storm")
            );
        }
    }
    let recomputed = ResultRecord::load(&record_path).expect("the record reads back");
    assert!(recomputed.sample_count < counted_before, "the block is excluded");
    let details = recomputed.details.as_ref().expect("the details survive");
    assert_eq!(details.invalidated_blocks.len(), 1);
    assert_eq!(details.invalidated_blocks[0].block, 1);
    assert!(
        details
            .notes
            .iter()
            .any(|note| note.contains("preserved but excluded")),
        "the rerun rule is stated"
    );
    assert_eq!(recomputed.sample_count, 2, "one block of two runs remains");

    // An unknown block is refused rather than silently ignored.
    let refusal = perf::runner::invalidate_block(&raw_path, &record_path, 99, "nothing")
        .expect_err("no such block");
    assert!(refusal.contains("no sample"));
}
