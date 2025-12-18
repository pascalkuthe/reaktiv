//! Zero-sized hash builder for performance-critical hash collections.
//!
//! Provides `FastHashBuilder`, a zero-sized `BuildHasher` that uses foldhash
//! with a fixed seed. Suitable for internal data structures where HashDoS
//! resistance is not needed.

use std::hash::BuildHasher;

pub use foldhash::fast::{FixedState, FoldHasher};

/// A zero-sized BuildHasher that uses foldhash with a fixed seed.
///
/// This provides fast hashing with no per-collection memory overhead.
/// The fixed seed means all instances produce identical hash values,
/// which is suitable for internal data structures where HashDoS
/// resistance is not a concern.
///
/// # Properties
/// - Zero-sized (`size_of::<FastHashBuilder>()` == 0)
/// - Fast hashing via foldhash
/// - Deterministic (same input = same hash across all instances)
#[derive(Clone, Copy, Debug, Default)]
pub struct FastHashBuilder;

impl BuildHasher for FastHashBuilder {
    type Hasher = FoldHasher<'static>;

    #[inline]
    fn build_hasher(&self) -> Self::Hasher {
        // Use FixedState with a constant seed for zero-sized, deterministic hashing
        FixedState::with_seed(0x517cc1b727220a95).build_hasher()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_hash_builder_is_zero_sized() {
        assert_eq!(std::mem::size_of::<FastHashBuilder>(), 0);
    }

    #[test]
    fn fast_hash_builder_is_deterministic() {
        let builder1 = FastHashBuilder;
        let builder2 = FastHashBuilder;

        let hash1 = builder1.hash_one(42u64);
        let hash2 = builder2.hash_one(42u64);

        assert_eq!(hash1, hash2);
    }
}
