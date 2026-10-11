//! The benchmark result record of SPEC 17.8.
//!
//! The format distinguishes `not_measured`, `passed`, `failed`, and
//! `inconclusive`. The first results are `not_measured`, never zero or
//! presumed passing; a measured record adds the tool and config hashes,
//! the warmup and sampling parameters, the raw sample reference, the
//! baseline comparison with its confidence interval, process counts,
//! the memory definition, and any invalidated blocks.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::metric::{self, Budget};
use super::samples::Sample;

/// The result record schema version.
pub const RESULT_SCHEMA_VERSION: u32 = 1;

/// The outcome of one metric on one corpus.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Never measured against a registered environment.
    NotMeasured,
    /// Measured within every gate the record carries.
    Passed,
    /// Measured and over a gate: a failed oracle, a failed run, or an
    /// exceeded budget.
    Failed,
    /// Measured, but the evidence cannot support a verdict: too few
    /// samples or block pairs for the declared policy.
    Inconclusive,
}

impl Status {
    /// The JSON spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Status::NotMeasured => "not_measured",
            Status::Passed => "passed",
            Status::Failed => "failed",
            Status::Inconclusive => "inconclusive",
        }
    }
}

/// Whether the independent oracle ran and what it saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OracleState {
    /// No oracle result exists yet.
    NotRun,
    /// Every verified run was correct.
    Passed,
    /// At least one fast-but-incorrect run failed the benchmark.
    Failed,
}

impl OracleState {
    /// The JSON spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            OracleState::NotRun => "not_run",
            OracleState::Passed => "passed",
            OracleState::Failed => "failed",
        }
    }
}

/// The environment and tool facts a measured record carries (SPEC 17.8
/// "all tool/config hashes").
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct EnvironmentSummary {
    /// The environment record the run was gated on.
    pub path: PathBuf,
    /// Git version of the runner.
    pub git_version: Option<String>,
    /// Rust compiler version of the runner.
    pub rustc_version: Option<String>,
    /// Cargo version of the runner.
    pub cargo_version: Option<String>,
    /// Kernel of the runner.
    pub kernel: Option<String>,
    /// OS of the runner.
    pub os: Option<String>,
    /// SHA-256 of `Cargo.lock` at the measured build.
    pub dependency_lock_sha256: Option<String>,
    /// Build profile of the measured executable.
    pub build_profile: Option<String>,
    /// `RUSTFLAGS` of the measured build.
    pub rustflags: Option<String>,
    /// This harness's revision.
    pub harness_revision: String,
}

/// One executable a record measured.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ExecutableSummary {
    /// The executable path as invoked.
    pub path: PathBuf,
    /// Its size in bytes.
    pub bytes: u64,
    /// Its SHA-256.
    pub sha256: String,
}

/// The corpus facts a measured record carries.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CorpusSummary {
    /// The corpus root holding `repo/` and `manifest.json`.
    pub directory: PathBuf,
    /// The manifest the run consumed.
    pub manifest_path: PathBuf,
    /// SHA-256 of the manifest bytes.
    pub manifest_sha256: String,
    /// The generator revision the manifest records.
    pub generator_revision: String,
    /// The seed the manifest records.
    pub seed: u64,
    /// Whether the corpus ran at test scale (below the SPEC 17.2
    /// numbers); such a run is diagnostic, never a gate.
    pub test_scale: bool,
}

/// The warmup and sampling parameters of a measured record (SPEC 17.7).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Sampling {
    /// Warmup runs per version, excluded from statistics and retained.
    pub warmups: usize,
    /// Independent paired blocks.
    pub blocks: usize,
    /// Fresh-process runs per block per version.
    pub runs_per_block: usize,
    /// The recorded seed of the baseline/candidate order randomization.
    pub order_seed: u64,
    /// How the boundary reads.
    pub boundary: String,
    /// The SPEC 17.3 warm-CLI method sentence.
    pub cache_method: String,
    /// The policy sentence for block randomization.
    pub order_rule: String,
}

impl Sampling {
    /// The default policy of SPEC 17.7.3: five warmups, twenty blocks of
    /// five fresh-process runs per version.
    pub fn policy() -> Sampling {
        Sampling {
            warmups: 5,
            blocks: 20,
            runs_per_block: 5,
            order_seed: 0,
            boundary: "wall time from before process creation through exit and full response delivery".to_string(),
            cache_method: "warm filesystem caches, fresh process per run".to_string(),
            order_rule: "one recorded-seed coin flip per block decides baseline/candidate order".to_string(),
        }
    }

    /// The measured run count one version contributes.
    pub fn measured_runs(&self) -> usize {
        self.blocks * self.runs_per_block
    }
}

/// The statistics block of a measured record.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Statistics {
    /// Counted samples behind the statistics.
    pub sample_count: usize,
    /// Nearest-rank median in milliseconds.
    pub median_ms: f64,
    /// Nearest-rank p95 in milliseconds.
    pub p95_ms: f64,
    /// Smallest counted sample.
    pub min_ms: f64,
    /// Largest counted sample.
    pub max_ms: f64,
    /// The percentile method sentence.
    pub method: String,
}

/// The baseline comparison of a paired record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Comparison {
    /// The baseline executable and hash.
    pub baseline: ExecutableSummary,
    /// Baseline median in milliseconds.
    pub baseline_median_ms: f64,
    /// Baseline p95 in milliseconds.
    pub baseline_p95_ms: f64,
    /// Counted baseline samples.
    pub baseline_sample_count: usize,
    /// Median increase, candidate minus baseline, in milliseconds.
    pub median_increase_ms: f64,
    /// The bootstrap confidence interval.
    pub bootstrap: BootstrapRecord,
    /// Independent block pairs the comparison pooled.
    pub blocks_paired: usize,
    /// Whether the regression gate fired.
    pub regression_blocked: bool,
}

/// The bootstrap interval of the regression gate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct BootstrapRecord {
    /// Resamples drawn.
    pub resamples: usize,
    /// Confidence level in percent.
    pub confidence: u64,
    /// The method sentence.
    pub method: String,
    /// Lower bound of the median increase, in milliseconds.
    pub lower_ms: f64,
    /// Upper bound of the median increase, in milliseconds.
    pub upper_ms: f64,
}

/// What the gates decided.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Gate {
    /// The budget sentence of this metric and corpus.
    pub budget: Option<String>,
    /// Whether the gated p95 exceeded the budget.
    pub budget_exceeded: bool,
    /// Whether one declared confirmation run of both versions is still
    /// required before the failure blocks release (SPEC 17.7).
    pub confirmation_required: bool,
}

/// One block invalidated by a documented external incident (SPEC
/// 17.7.4): preserved, with its reason.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct InvalidatedBlock {
    /// The block index.
    pub block: usize,
    /// The recorded reason.
    pub reason: String,
    /// When the invalidation was recorded.
    pub at: String,
}

/// The extra fields of a measured record (SPEC 17.8).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Details {
    /// The metric kind (`cli-fresh-process`, `pty-interaction`,
    /// `resource`).
    pub metric_kind: String,
    /// The metric definition sentence.
    pub metric_about: String,
    /// When the measurement ran, RFC 3339 UTC.
    pub measured_at: String,
    /// Environment and tool hashes.
    pub environment: EnvironmentSummary,
    /// The gated executable.
    pub executable: ExecutableSummary,
    /// The corpus facts.
    pub corpus: CorpusSummary,
    /// Warmup and sampling parameters.
    pub sampling: Sampling,
    /// The gated side's statistics.
    pub statistics: Option<Statistics>,
    /// The baseline comparison of a paired run.
    pub comparison: Option<Comparison>,
    /// The gate decisions.
    pub gate: Gate,
    /// Git process counts per command, when sampled; `null` until the
    /// process-count sampler exists.
    pub process_counts: Option<serde_json::Value>,
    /// Which memory definition a resource metric used; `null` for CLI
    /// latency metrics.
    pub memory_definition: Option<String>,
    /// Blocks invalidated by documented incidents, preserved.
    pub invalidated_blocks: Vec<InvalidatedBlock>,
    /// Honest limitations of this record.
    pub notes: Vec<String>,
}

/// One benchmark result record. The fifteen base fields are exactly the
/// SPEC 17.8 example; `details` appears only on measured records.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResultRecord {
    /// Record schema version.
    pub schema_version: u32,
    /// The metric identifier.
    pub metric: String,
    /// The corpus label.
    pub corpus: String,
    /// The outcome.
    pub status: Status,
    /// The registered runner, or `None` while unmeasured.
    pub environment_id: Option<String>,
    /// The commit the measured executable was built from.
    pub implementation_commit: Option<String>,
    /// SHA-256 of the measured executable.
    pub executable_hash: Option<String>,
    /// SHA-256 of the corpus manifest.
    pub corpus_manifest_hash: Option<String>,
    /// Counted samples behind the statistics.
    pub sample_count: usize,
    /// Nearest-rank median in milliseconds.
    pub median_ms: Option<f64>,
    /// Nearest-rank p95 in milliseconds.
    pub p95_ms: Option<f64>,
    /// The p95 latency budget of this metric and corpus, when it has
    /// one.
    pub budget_p95_ms: Option<u64>,
    /// The independent correctness oracle.
    pub correctness_oracle: OracleState,
    /// The raw sample stream behind this record.
    pub raw_samples_path: Option<PathBuf>,
    /// The measured extras.
    pub details: Option<Details>,
}

impl ResultRecord {
    /// The initial `not_measured` record of one metric and corpus,
    /// carrying its budget so the gate is visible before any number
    /// exists.
    pub fn not_measured(metric_id: &str, corpus: &str) -> Self {
        let budget = metric::by_id(metric_id)
            .and_then(|definition| definition.budget(corpus))
            .and_then(|budget| budget.p95_ms());
        ResultRecord {
            schema_version: RESULT_SCHEMA_VERSION,
            metric: metric_id.to_string(),
            corpus: corpus.to_string(),
            status: Status::NotMeasured,
            environment_id: None,
            implementation_commit: None,
            executable_hash: None,
            corpus_manifest_hash: None,
            sample_count: 0,
            median_ms: None,
            p95_ms: None,
            budget_p95_ms: budget,
            correctness_oracle: OracleState::NotRun,
            raw_samples_path: None,
            details: None,
        }
    }

    /// Writes the record, pretty-printed, next to its raw samples.
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let body = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, body)
    }

    /// Reads a record back.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let body = std::fs::read_to_string(path)?;
        serde_json::from_str(&body)
            .map_err(|error| std::io::Error::other(format!("{}: {error}", path.display())))
    }

    /// The file name one record occupies inside a results directory.
    pub fn file_name(metric_id: &str, corpus: &str) -> String {
        format!("{metric_id}.{corpus}.json")
    }
}

/// The samples of one version, grouped into their blocks, counted runs
/// only.
pub fn counted_blocks(samples: &[Sample], version: &str) -> Vec<Vec<f64>> {
    let mut grouped: Vec<Vec<f64>> = Vec::new();
    for sample in samples.iter().filter(|sample| sample.version == version && sample.counted()) {
        while grouped.len() <= sample.block {
            grouped.push(Vec::new());
        }
        grouped[sample.block].push(sample.elapsed_ms);
    }
    grouped
}

/// The counted samples of one version, flattened.
pub fn counted_values(samples: &[Sample], version: &str) -> Vec<f64> {
    samples
        .iter()
        .filter(|sample| sample.version == version && sample.counted())
        .map(|sample| sample.elapsed_ms)
        .collect()
}

/// The budget sentence for a record's gate section.
pub fn budget_sentence(metric_id: &str, corpus: &str) -> Option<String> {
    metric::by_id(metric_id)?
        .budget(corpus)
        .map(|budget: Budget| budget.describe())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_initial_record_matches_the_spec_17_8_example() {
        let record = ResultRecord::not_measured("P-DIFF", "C2");
        let value = serde_json::to_value(&record).expect("the record serializes");
        let map = value.as_object().expect("an object");
        // Exactly the fifteen fields of the SPEC example, nothing more;
        // JSON object order carries no meaning, so the set is the claim.
        let mut expected: Vec<&str> = [
            "schema_version", "metric", "corpus", "status", "environment_id",
            "implementation_commit", "executable_hash", "corpus_manifest_hash",
            "sample_count", "median_ms", "p95_ms", "budget_p95_ms",
            "correctness_oracle", "raw_samples_path", "details",
        ]
        .to_vec();
        expected.sort_unstable();
        let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, expected);
        assert_eq!(map["schema_version"], serde_json::json!(1));
        assert_eq!(map["metric"], serde_json::json!("P-DIFF"));
        assert_eq!(map["corpus"], serde_json::json!("C2"));
        assert_eq!(map["status"], serde_json::json!("not_measured"));
        assert_eq!(map["environment_id"], serde_json::Value::Null);
        assert_eq!(map["implementation_commit"], serde_json::Value::Null);
        assert_eq!(map["executable_hash"], serde_json::Value::Null);
        assert_eq!(map["corpus_manifest_hash"], serde_json::Value::Null);
        assert_eq!(map["sample_count"], serde_json::json!(0));
        assert_eq!(map["median_ms"], serde_json::Value::Null);
        assert_eq!(map["p95_ms"], serde_json::Value::Null);
        assert_eq!(map["budget_p95_ms"], serde_json::json!(500));
        assert_eq!(map["correctness_oracle"], serde_json::json!("not_run"));
        assert_eq!(map["raw_samples_path"], serde_json::Value::Null);
    }

    #[test]
    fn a_not_measured_resource_metric_has_no_latency_budget() {
        let record = ResultRecord::not_measured("P-RSS", "C2");
        assert_eq!(record.budget_p95_ms, None);
        assert_eq!(record.status, Status::NotMeasured);
    }

    #[test]
    fn every_status_and_oracle_state_round_trips() {
        for status in [
            Status::NotMeasured,
            Status::Passed,
            Status::Failed,
            Status::Inconclusive,
        ] {
            let text = serde_json::to_string(&status).expect("serializes");
            let back: Status = serde_json::from_str(&text).expect("reads back");
            assert_eq!(back, status);
        }
        for oracle in [OracleState::NotRun, OracleState::Passed, OracleState::Failed] {
            let text = serde_json::to_string(&oracle).expect("serializes");
            let back: OracleState = serde_json::from_str(&text).expect("reads back");
            assert_eq!(back, oracle);
        }
    }
}
