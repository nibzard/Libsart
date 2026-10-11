//! The measurement loop of SPEC 17.7: warmups, independent paired
//! blocks of fresh-process runs, recorded order randomization, per-run
//! oracle verification, and the honest status decision.
//!
//! Policy defaults are the SPEC numbers: five warmups and twenty blocks
//! of five runs per version. Below-policy runs are reported as
//! `inconclusive`, never silently promoted.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::envrec::{EnvironmentRecord, now_utc};
use super::fixture::Fixture;
use super::metric::{self, Kind};
use super::oracle;
use super::record::{
    self, Comparison, CorpusSummary, Details, EnvironmentSummary, ExecutableSummary, Gate,
    InvalidatedBlock, OracleState, ResultRecord, Sampling, Statistics, Status,
};
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
    /// Fresh-process runs per block per version.
    pub runs_per_block: usize,
    /// The seed of the order randomization, recorded with the samples.
    pub order_seed: u64,
    /// The commit the executable was built from, when known.
    pub implementation_commit: Option<String>,
    /// Operator notes recorded verbatim.
    pub notes: Vec<String>,
}

impl RunPlan {
    /// The sampling parameters this plan implies.
    fn sampling(&self) -> Sampling {
        Sampling {
            warmups: self.warmups,
            blocks: self.blocks,
            runs_per_block: self.runs_per_block,
            order_seed: self.order_seed,
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
    let failures = verify_run(plan, fixture, &timed.stdout, &timed.stderr, request_id.as_deref());
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
        exit_code,
        stdout_bytes: timed.stdout.len() as u64,
        stderr_bytes: timed.stderr.len() as u64,
        oracle: if failures.is_empty() { "passed".to_string() } else { "failed".to_string() },
        oracle_failures: failures.to_vec(),
        cache_method: "warm".to_string(),
        invalidated: false,
        invalidation_reason: None,
        at: now_utc(),
    }
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
    let definition = metric::by_id(plan.metric)
        .ok_or_else(|| format!("unknown metric {}", plan.metric))?;
    if definition.kind != Kind::Cli {
        return Err(format!(
            "{} is a {} metric; this harness times fresh CLI processes only, \
             and the metric stays not_measured",
            plan.metric,
            definition.kind.as_str()
        ));
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
            samples.push(one_run(
                plan,
                &executable,
                version,
                0,
                warmup,
                order_index,
                true,
                order_index,
                fixture,
            )?);
            order_index += 1;
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
                samples.push(one_run(
                    plan,
                    &executable,
                    version,
                    block,
                    run_in_block,
                    order_index,
                    false,
                    iteration,
                    fixture,
                )?);
                order_index += 1;
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
    let outcome = finish_record(plan, fixture, environment, environment_path, &samples, &raw_path);
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
    details
        .invalidated_blocks
        .push(InvalidatedBlock {
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
    let counted: Vec<f64> = samples
        .iter()
        .filter(|sample| sample.counted() && sample.version == gated_version)
        .map(|sample| sample.elapsed_ms)
        .collect();
    if counted.is_empty() {
        record.status = Status::Inconclusive;
        record.sample_count = 0;
        record.median_ms = None;
        record.p95_ms = None;
        details.statistics = None;
    } else {
        let (min_ms, max_ms) = stats::range(&counted);
        let statistics = Statistics {
            sample_count: counted.len(),
            median_ms: stats::median(&counted),
            p95_ms: stats::p95(&counted),
            min_ms,
            max_ms,
            method: "nearest-rank; all samples retained including slow ones".to_string(),
        };
        record.sample_count = statistics.sample_count;
        record.median_ms = Some(statistics.median_ms);
        record.p95_ms = Some(statistics.p95_ms);
        if let Some(limit) = record.budget_p95_ms {
            if statistics.p95_ms > limit as f64 {
                record.status = Status::Failed;
            }
        }
        details.statistics = Some(statistics);
    }

    // A paired record's comparison must follow the same exclusion: the
    // baseline statistics and the bootstrap are recomputed over the
    // retained blocks with the recorded seed, never left describing the
    // samples that were just excluded.
    if samples
        .iter()
        .any(|sample| sample.version == "baseline" && sample.counted())
        && samples
            .iter()
            .any(|sample| sample.version == "candidate" && sample.counted())
    {
        let baseline_path = details
            .comparison
            .as_ref()
            .map(|comparison| comparison.baseline.path.clone())
            .unwrap_or_else(|| details.executable.path.clone());
        let plan = RunPlan {
            metric: metric::by_id(&record.metric)
                .map(|definition| definition.id)
                .unwrap_or("P-HELP"),
            corpus: record.corpus.clone(),
            sides: Sides::Paired {
                baseline: baseline_path,
                candidate: details.executable.path.clone(),
            },
            warmups: details.sampling.warmups,
            blocks: details.sampling.blocks,
            runs_per_block: details.sampling.runs_per_block,
            order_seed: details.sampling.order_seed,
            implementation_commit: record.implementation_commit.clone(),
            notes: Vec::new(),
        };
        details.comparison = comparison_of(&plan, &samples);
    }
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
    let statistics = (!measured.is_empty()).then(|| {
        let (min_ms, max_ms) = stats::range(&measured);
        Statistics {
            sample_count: measured.len(),
            median_ms: stats::median(&measured),
            p95_ms: stats::p95(&measured),
            min_ms,
            max_ms,
            method: "nearest-rank; all samples retained including slow ones".to_string(),
        }
    });

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
    let mut budget_exceeded = false;
    if let (Some(limit), Some(stats_block)) = (budget_p95, &statistics) {
        budget_exceeded = stats_block.p95_ms > limit as f64;
    }

    let comparison = comparison_of(plan, samples);
    let regression_blocked = comparison
        .as_ref()
        .map(|comparison| comparison.regression_blocked)
        .unwrap_or(false);

    let invalidated_blocks: Vec<InvalidatedBlock> = samples
        .iter()
        .filter(|sample| sample.invalidated)
        .filter_map(|sample| {
            sample.invalidation_reason.as_ref().map(|reason| InvalidatedBlock {
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
    // diagnostic and its verdict is inconclusive, never a pass.
    let below_policy =
        plan.warmups < 5 || plan.blocks < 20 || plan.runs_per_block < 5;

    let status = if oracle_state == OracleState::Failed || exits_broken {
        Status::Failed
    } else if budget_exceeded || regression_blocked {
        Status::Failed
    } else if below_policy {
        Status::Inconclusive
    } else if statistics.is_some() {
        Status::Passed
    } else {
        Status::NotMeasured
    };

    let mut notes = plan.notes.clone();
    if below_policy {
        notes.push(format!(
            "{} counted samples over {} blocks sit below the SPEC 17.7.3 policy \
             (at least 20 independent block pairs); the comparison evidence is inconclusive",
            measured.len(),
            plan.blocks
        ));
    }
    if budget_exceeded {
        notes.push(
            "the p95 exceeded its budget: SPEC 17.7 requires one declared confirmation run \
             of both versions, preserved, before the failure blocks release"
                .to_string(),
        );
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
        memory_definition: None,
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
        sample_count: statistics.as_ref().map_or(0, |stats_block| stats_block.sample_count),
        median_ms: statistics.as_ref().map(|stats_block| stats_block.median_ms),
        p95_ms: statistics.as_ref().map(|stats_block| stats_block.p95_ms),
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
    let baseline_values = record::counted_values(samples, "baseline");
    let candidate_values = record::counted_values(samples, "candidate");
    if baseline_values.is_empty() || candidate_values.is_empty() {
        return None;
    }
    let baseline_blocks = record::counted_blocks(samples, "baseline");
    let candidate_blocks = record::counted_blocks(samples, "candidate");
    let count = baseline_blocks.len().min(candidate_blocks.len());
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
        git_version: environment.software.as_ref().and_then(|software| software.git_version.clone()),
        rustc_version: environment
            .software
            .as_ref()
            .and_then(|software| software.rustc_version.clone()),
        cargo_version: environment
            .software
            .as_ref()
            .and_then(|software| software.cargo_version.clone()),
        kernel: environment.software.as_ref().and_then(|software| software.kernel.clone()),
        os: environment.software.as_ref().and_then(|software| software.os.clone()),
        dependency_lock_sha256: environment
            .build
            .as_ref()
            .and_then(|build| build.dependency_lock_sha256.clone()),
        build_profile: None,
        rustflags: environment.build.as_ref().map(|build| build.rustflags.clone()),
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
        assert!(problem.contains("unassigned"), "the refusal names the state");
        let mut registered = record;
        registered.status = "registered".to_string();
        assert!(require_registered(&registered).is_ok());
    }
}
