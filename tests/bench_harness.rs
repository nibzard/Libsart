//! Acceptance suite of task T053: the benchmark harness and the
//! environment record of SPEC 17.3-17.8, including the T074
//! registration of the actual dedicated runner.
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
//!   statuses — a fast but incorrect command fails the benchmark.

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
    RunPlan {
        metric: metric::by_id(id).expect("the metric exists").id,
        corpus: "C1".to_string(),
        sides,
        warmups,
        blocks,
        runs_per_block: runs,
        order_seed: 20261011,
        implementation_commit: None,
        notes: Vec::new(),
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

#[test]
fn pty_and_resource_metrics_stay_not_measured_under_this_harness() {
    let root = scratch("kinds");
    let results = root.join("results");
    let environment = register_environment(&root);
    let path = root.join("environment.json");
    let corpus = c1_fixture_dir("kinds");
    let fixture = perf::fixture::Fixture::prepare(&corpus, &root.join("fixture"))
        .expect("the fixture prepares");
    for id in ["P-OPEN", "P-KEY", "P-REFRESH", "P-CPU", "P-RSS", "P-TREE-MEM"] {
        let run = plan(
            id,
            Sides::Single(PathBuf::from(env!("CARGO_BIN_EXE_margin"))),
            0,
            1,
            1,
        );
        let refusal = perf::runner::run(&run, &fixture, &environment, &path, &results)
            .expect_err("the harness refuses what it cannot time");
        assert!(
            refusal.contains("not_measured"),
            "{id} must stay not_measured: {refusal}"
        );
        assert!(
            !results.join(ResultRecord::file_name(id, "C1")).exists(),
            "{id} was never written"
        );
    }
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
