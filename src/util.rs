//! Tiny shared helpers used by more than one module.
//!
//! Keep this module deliberately bare — anything that grows beyond a
//! handful of pure functions should move to its own module instead.

/// Current wall-clock unix timestamp, in seconds since 1970-01-01 UTC.
///
/// Returns 0 on the (impossible-in-practice) case where the system clock
/// is set before the epoch. Callers downstream of this helper interpret
/// a returned `0` as "clock invalid" — for example, `check_replay`'s
/// timestamp-window check rejects all requests when `now == 0`.
pub(crate) fn current_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Current wall-clock unix timestamp, in milliseconds since 1970-01-01 UTC.
///
/// Used for the `received_at_ms` field of the local-pipe `InboundMessage`
/// envelope. Range fits comfortably in i64 until ~year 292278994, so the
/// `as i64` cast is safe for any realistic clock value.
pub(crate) fn current_unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Current wall-clock unix timestamp, in nanoseconds since 1970-01-01 UTC.
///
/// Used as the disambiguator inside `wx-<openid>-<nanos>` correlation
/// ids; the `u128` width is sized to avoid overflow on any reasonable
/// clock setting.
pub(crate) fn current_unix_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_helpers_are_consistent_within_a_call() {
        // Sanity: secs is ms/1000 (with sub-second drift between calls).
        let s = current_unix_secs();
        let ms = current_unix_millis();
        let ns = current_unix_nanos();
        assert!(s > 1_700_000_000, "unix seconds should be after 2023");
        assert!(ms / 1000 >= s, "ms/1000 must be at least seconds");
        // ns must be far larger than ms (3 orders of magnitude).
        assert!(ns > (ms as u128) * 100);
    }

    #[test]
    fn time_helpers_advance_or_stay_equal() {
        let a = current_unix_millis();
        let b = current_unix_millis();
        assert!(b >= a, "wall clock should not go backwards in this test");
    }
}
