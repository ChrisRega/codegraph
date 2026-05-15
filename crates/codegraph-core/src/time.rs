//! Tiny ISO-8601 timestamp helper. Single source of truth for the
//! `created_at` / `updated_at` / `last_run_at` strings produced across
//! the toolchain — kept here to avoid the chronic risk that two local
//! copies drift in precision or formatting.
//!
//! No external dep: implements the Howard Hinnant civil-date conversion
//! by hand.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current wall-clock as RFC 3339 / ISO 8601 with microsecond precision,
/// e.g. `2026-05-15T18:55:00.123456Z`.
pub fn now_iso() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    iso_from_unix_micros(dur.as_secs() as i64, dur.subsec_nanos())
}

/// Format `(unix_seconds, subsec_nanos)` as ISO 8601 with microsecond
/// precision. Exposed for tests + any future deterministic-time caller.
pub fn iso_from_unix_micros(secs: i64, nanos: u32) -> String {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    let h = (secs_of_day / 3600) as u32;
    let mi = ((secs_of_day % 3600) / 60) as u32;
    let s = (secs_of_day % 60) as u32;
    format!(
        "{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{:06}Z",
        nanos / 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_iso_is_iso_shaped() {
        let s = now_iso();
        assert!(s.ends_with('Z'), "got {s}");
        assert_eq!(s.as_bytes()[4], b'-');
        assert_eq!(s.as_bytes()[7], b'-');
        assert_eq!(s.as_bytes()[10], b'T');
        // Sanity: includes the microsecond fractional.
        assert!(s.contains('.'), "got {s}");
    }

    #[test]
    fn iso_from_unix_micros_matches_known_epoch() {
        // 2000-01-01T00:00:00 UTC = 946 684 800 unix seconds (well-known).
        assert_eq!(
            iso_from_unix_micros(946_684_800, 123_456_000),
            "2000-01-01T00:00:00.123456Z"
        );
    }

    #[test]
    fn iso_from_unix_micros_handles_epoch_zero() {
        assert_eq!(iso_from_unix_micros(0, 0), "1970-01-01T00:00:00.000000Z");
    }
}
