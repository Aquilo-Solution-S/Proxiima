//! Counts and lengths as the `u64` that reports, frames and hashes carry.

/// Convert a count to `u64`. The `u64::MAX` saturation fallback is unreachable on supported targets.
#[must_use]
pub fn count_u64(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::count_u64;

    #[test]
    fn a_count_keeps_its_value() {
        assert_eq!(count_u64(0), 0);
        assert_eq!(count_u64(42), 42);
        assert_eq!(
            count_u64(usize::MAX),
            u64::try_from(usize::MAX).unwrap_or(u64::MAX)
        );
    }
}
