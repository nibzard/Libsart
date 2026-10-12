//! Raw paired samples (SPEC 17.7.3-17.7.4): every run of every version
//! lands in one JSON Lines file, slow ones included, invalidated blocks
//! preserved rather than deleted.

use std::fs;
use std::io::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The schema version of the raw sample stream.
pub const SAMPLES_SCHEMA_VERSION: u32 = 1;

/// One raw measured run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sample {
    /// Sample schema version.
    pub schema_version: u32,
    /// The metric identifier.
    pub metric: String,
    /// The corpus label.
    pub corpus: String,
    /// Which side of the comparison ran: `baseline`, `candidate`, or
    /// `single` for an absolute single-version measurement.
    pub version: String,
    /// The paired block index, starting at zero.
    pub block: usize,
    /// The run inside its block, starting at zero.
    pub run_in_block: usize,
    /// The global execution order across versions and blocks.
    pub order_index: usize,
    /// Whether this run was one of the warmups, excluded from the
    /// statistics but retained here.
    pub warmup: bool,
    /// Measured wall time in milliseconds.
    ///
    /// The resource metrics store their own quantity here — MiB or a
    /// percentage of one core — and say so in `unit`, because a raw
    /// number without its unit is not evidence.
    pub elapsed_ms: f64,
    /// The unit of `elapsed_ms`: absent means milliseconds; the resource
    /// samplers set `MiB` or `percent-of-one-core`. Absent on every old
    /// latency stream, so they parse unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// The child's exit code, or -1 for a signal.
    pub exit_code: i32,
    /// Complete response size on standard output.
    pub stdout_bytes: u64,
    /// Complete response size on standard error.
    pub stderr_bytes: u64,
    /// `passed` or `failed`.
    pub oracle: String,
    /// What the independent oracle found wanting, when it failed.
    pub oracle_failures: Vec<String>,
    /// The cache method of the run (SPEC 17.3).
    pub cache_method: String,
    /// Whether a documented external incident invalidated this run's
    /// whole block after the fact.
    pub invalidated: bool,
    /// The recorded reason, when the block was invalidated.
    pub invalidation_reason: Option<String>,
    /// When the run happened, RFC 3339 UTC.
    pub at: String,
}

impl Sample {
    /// Whether this sample counts toward the statistics: measured, not a
    /// warmup, and not part of an invalidated block.
    pub fn counted(&self) -> bool {
        !self.warmup && !self.invalidated
    }
}

/// Writes every sample, one JSON object per line.
pub fn write_jsonl(path: &Path, samples: &[Sample]) -> std::io::Result<()> {
    let mut file = fs::File::create(path)?;
    for sample in samples {
        let line = serde_json::to_string(sample).map_err(std::io::Error::other)?;
        writeln!(file, "{line}")?;
    }
    file.flush()
}

/// Reads a raw sample stream back.
pub fn read_jsonl(path: &Path) -> std::io::Result<Vec<Sample>> {
    let body = fs::read_to_string(path)?;
    let mut samples = Vec::new();
    for (number, line) in body.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let sample: Sample = serde_json::from_str(line).map_err(|error| {
            std::io::Error::other(format!("{} line {}: {error}", path.display(), number + 1))
        })?;
        samples.push(sample);
    }
    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(order_index: usize, block: usize) -> Sample {
        Sample {
            schema_version: SAMPLES_SCHEMA_VERSION,
            metric: "P-STATUS".to_string(),
            corpus: "C1".to_string(),
            version: "baseline".to_string(),
            block,
            run_in_block: order_index % 5,
            order_index,
            warmup: order_index < 2,
            elapsed_ms: 120.0 + order_index as f64,
            unit: None,
            exit_code: 0,
            stdout_bytes: 4096,
            stderr_bytes: 0,
            oracle: "passed".to_string(),
            oracle_failures: Vec::new(),
            cache_method: "warm".to_string(),
            invalidated: false,
            invalidation_reason: None,
            at: "2026-10-11T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn a_round_trip_keeps_every_run() {
        let dir = std::env::temp_dir().join(format!(
            "margin-perf-samples-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).expect("the scratch directory exists");
        let path = dir.join("samples.jsonl");
        let written: Vec<Sample> = (0..7).map(|index| sample(index, index / 5)).collect();
        write_jsonl(&path, &written).expect("the stream writes");
        let read = read_jsonl(&path).expect("the stream reads back");
        assert_eq!(read.len(), 7, "slow samples and warmups are retained");
        assert_eq!(read[6].elapsed_ms, 126.0);
        assert!(read[0].warmup && !read[0].counted());
        assert!(read[6].counted());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_latency_stream_without_a_unit_still_parses() {
        // A stream written before the unit field existed carries no
        // `unit` key; it must read back as milliseconds.
        let line = r#"{"schema_version":1,"metric":"P-STATUS","corpus":"C1","version":"single","block":0,"run_in_block":0,"order_index":0,"warmup":false,"elapsed_ms":42.5,"exit_code":0,"stdout_bytes":10,"stderr_bytes":0,"oracle":"passed","oracle_failures":[],"cache_method":"warm","invalidated":false,"invalidation_reason":null,"at":"2026-10-11T00:00:00Z"}"#;
        let sample: Sample = serde_json::from_str(line).expect("the old stream parses");
        assert_eq!(sample.unit, None);
        assert_eq!(sample.elapsed_ms, 42.5);
    }

    #[test]
    fn a_resource_sample_carries_its_unit() {
        let mut own = sample(0, 0);
        own.unit = Some("MiB".to_string());
        let text = serde_json::to_string(&own).expect("the resource sample serializes");
        assert!(text.contains(r#""unit":"MiB""#), "{text}");
        let back: Sample = serde_json::from_str(&text).expect("it reads back");
        assert_eq!(back.unit.as_deref(), Some("MiB"));
    }
}
