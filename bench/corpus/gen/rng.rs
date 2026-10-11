//! The seeded pseudo-random stream of the benchmark corpora (SPEC 17.2).
//!
//! The generators must reproduce byte-identical corpora from a recorded
//! seed, so they cannot use a thread-local or entropy-seeded source. This
//! module implements SplitMix64: four arithmetic steps on wrapping `u64`
//! values that every supported platform evaluates identically. The
//! algorithm is fixed by this module; changing it changes every generated
//! corpus and requires a generator-revision bump.

/// One deterministic SplitMix64 stream.
#[derive(Clone, Debug)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// Starts the stream at `seed`.
    pub fn seeded(seed: u64) -> Self {
        Rng { state: seed }
    }

    /// Advances the stream and returns the next 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Returns a value below `bound`. The bound must not be zero.
    pub fn below(&mut self, bound: usize) -> usize {
        assert!(bound > 0, "a random bound must be positive");
        (self.next_u64() % bound as u64) as usize
    }

    /// Returns a value in `low..high` (the high bound is exclusive). The
    /// range must be nonempty.
    pub fn range(&mut self, low: usize, high: usize) -> usize {
        assert!(high > low, "a random range must be nonempty");
        low + self.below(high - low)
    }

    /// Returns true with probability `percent` out of 100.
    pub fn chance(&mut self, percent: u64) -> bool {
        self.next_u64() % 100 < percent
    }
}

#[cfg(test)]
mod tests {
    use super::Rng;

    /// The published first outputs of SplitMix64 at seed 0, so a toolchain
    /// or refactor change that silently alters the stream fails here
    /// instead of re-basing every corpus.
    #[test]
    fn splitmix64_reference_sequence() {
        let mut rng = Rng::seeded(0);
        assert_eq!(rng.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(rng.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(rng.next_u64(), 0x06C4_5D18_8009_454F);
    }

    #[test]
    fn same_seed_same_sequence() {
        let mut left = Rng::seeded(0x6D61_7267);
        let mut right = Rng::seeded(0x6D61_7267);
        for _ in 0..64 {
            assert_eq!(left.next_u64(), right.next_u64());
        }
    }
}
