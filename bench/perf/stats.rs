//! Statistics of SPEC 17.7: nearest-rank median and p95, and the
//! predeclared paired-block bootstrap of the regression gate.
//!
//! Every function retains the sample it was given; nothing here discards
//! a slow run. Percentiles are nearest-rank so two implementations of
//! the same policy cannot disagree about interpolation.

/// Sorted copy of `values`.
fn sorted(values: &[f64]) -> Vec<f64> {
    let mut copy = values.to_vec();
    copy.sort_by(|a, b| a.partial_cmp(b).expect("samples are finite"));
    copy
}

/// The nearest-rank percentile `p` (0 to 100) of `values`: the
/// `ceil(p/100 * n)`-th smallest sample.
pub fn nearest_rank(values: &[f64], percentile: f64) -> f64 {
    assert!(!values.is_empty(), "a percentile needs at least one sample");
    let rank = ((percentile / 100.0) * values.len() as f64).ceil() as usize;
    let rank = rank.clamp(1, values.len());
    sorted(values)[rank - 1]
}

/// The nearest-rank median.
pub fn median(values: &[f64]) -> f64 {
    nearest_rank(values, 50.0)
}

/// The nearest-rank p95.
pub fn p95(values: &[f64]) -> f64 {
    nearest_rank(values, 95.0)
}

/// The smallest and largest sample.
pub fn range(values: &[f64]) -> (f64, f64) {
    let ordered = sorted(values);
    (*ordered.first().expect("samples exist"), *ordered.last().expect("samples exist"))
}

/// The seeded SplitMix64 stream the sampling policy uses for order
/// randomization and bootstrap resampling; the seed is part of every
/// recorded sampling parameter set.
#[derive(Clone, Copy)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    /// A new stream from one recorded seed.
    pub fn seeded(seed: u64) -> Self {
        SplitMix64(seed)
    }

    /// The next raw 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// One unbiased coin flip.
    pub fn coin(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }

    /// One uniform index below `bound`.
    pub fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }
}

/// The result of the paired bootstrap of SPEC 17.7's regression gate.
pub struct Bootstrap {
    /// Resamples drawn.
    pub resamples: usize,
    /// The confidence level in percent.
    pub confidence: u64,
    /// The method sentence recorded with the interval.
    pub method: &'static str,
    /// Lower bound of the confidence interval for the baseline-to-
    /// candidate median increase, in milliseconds.
    pub lower_ms: f64,
    /// Upper bound of the same interval.
    pub upper_ms: f64,
}

impl Bootstrap {
    /// Whether the regression gate fires: the lower bound exceeds both
    /// 10% of the baseline median and an absolute 5 ms.
    pub fn regression_blocked(&self, baseline_median_ms: f64) -> bool {
        self.lower_ms > baseline_median_ms * 0.10 && self.lower_ms > 5.0
    }
}

/// The predeclared bootstrap: 10,000 resamples of complete paired
/// blocks, retaining pairing and order information (SPEC 17.7).
///
/// Each resample draws `blocks.len()` block indices with replacement,
/// pools every run of each side over the drawn blocks, and records the
/// difference of the two pooled medians. The interval is the 0.5 and
/// 99.5 percentile of that distribution.
pub fn paired_bootstrap(
    baseline_blocks: &[Vec<f64>],
    candidate_blocks: &[Vec<f64>],
    resamples: usize,
    seed: u64,
) -> Bootstrap {
    assert_eq!(
        baseline_blocks.len(),
        candidate_blocks.len(),
        "paired blocks must pair"
    );
    assert!(!baseline_blocks.is_empty(), "the bootstrap needs blocks");
    let mut increases = Vec::with_capacity(resamples);
    let mut rng = SplitMix64::seeded(seed ^ 0xB005_7A9_0000_0000);
    for _ in 0..resamples {
        let mut baseline_pool = Vec::new();
        let mut candidate_pool = Vec::new();
        for _ in 0..baseline_blocks.len() {
            let index = rng.below(baseline_blocks.len());
            baseline_pool.extend_from_slice(&baseline_blocks[index]);
            candidate_pool.extend_from_slice(&candidate_blocks[index]);
        }
        increases.push(median(&candidate_pool) - median(&baseline_pool));
    }
    Bootstrap {
        resamples,
        confidence: 99,
        method: "percentile bootstrap of complete paired blocks; median increase; 0.5/99.5 ranks",
        lower_ms: nearest_rank(&increases, 0.5),
        upper_ms: nearest_rank(&increases, 99.5),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_matches_the_policy() {
        let one = [4.0];
        assert_eq!(median(&one), 4.0);
        let ten: Vec<f64> = (1..=10).map(|value| value as f64).collect();
        assert_eq!(median(&ten), 5.0, "rank ceil(0.5*10) = 5");
        assert_eq!(p95(&ten), 10.0, "rank ceil(0.95*10) = 10");
        let twenty: Vec<f64> = (1..=20).map(|value| value as f64).collect();
        assert_eq!(p95(&twenty), 19.0, "rank ceil(0.95*20) = 19");
    }

    #[test]
    fn a_large_increase_blocks_and_a_small_one_does_not() {
        let baseline: Vec<Vec<f64>> = (0..20).map(|_| vec![100.0; 5]).collect();
        let slowed: Vec<Vec<f64>> = (0..20).map(|_| vec![130.0; 5]).collect();
        let bootstrap = paired_bootstrap(&baseline, &slowed, 2_000, 7);
        assert!(bootstrap.lower_ms > 25.0, "lower bound {}", bootstrap.lower_ms);
        assert!(bootstrap.regression_blocked(100.0));

        let steady: Vec<Vec<f64>> = (0..20)
            .map(|_| (0..5).map(|run| 100.0 + run as f64).collect())
            .collect();
        let bootstrap = paired_bootstrap(&baseline, &steady, 2_000, 7);
        assert!(
            !bootstrap.regression_blocked(100.0),
            "a {} ms increase with lower bound {} ms does not block",
            2.0,
            bootstrap.lower_ms
        );
    }

    #[test]
    fn the_same_seed_repeats_the_same_interval() {
        let baseline: Vec<Vec<f64>> = (0..6)
            .map(|block| (0..5).map(|run| 100.0 + block as f64 + run as f64).collect())
            .collect();
        let candidate: Vec<Vec<f64>> = (0..6)
            .map(|block| (0..5).map(|run| 112.0 + block as f64 + run as f64).collect())
            .collect();
        let first = paired_bootstrap(&baseline, &candidate, 500, 99);
        let second = paired_bootstrap(&baseline, &candidate, 500, 99);
        assert_eq!(first.lower_ms, second.lower_ms);
        assert_eq!(first.upper_ms, second.upper_ms);
    }
}
