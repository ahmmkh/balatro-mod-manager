//! Minimal UTC date helpers (no chrono): `YYYY-MM-DD` strings and day arithmetic.

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYY-MM-DD` for a unix timestamp (seconds).
pub fn utc_day_of(unix_secs: i64) -> String {
    let (y, m, d) = civil_from_days(unix_secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn utc_day() -> String {
    utc_day_of(crate::db::now_sec())
}

/// The day `n` days before `day` (`YYYY-MM-DD`). An unparsable `day` is returned unchanged.
pub fn days_ago(day: &str, n: i64) -> String {
    let mut it = day.split('-').map(|p| p.parse::<i64>());
    match (it.next(), it.next(), it.next()) {
        (Some(Ok(y)), Some(Ok(m)), Some(Ok(d))) => {
            let (y, m, d) = civil_from_days(days_from_civil(y, m, d) - n);
            format!("{y:04}-{m:02}-{d:02}")
        }
        _ => day.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn known_dates() {
        assert_eq!(utc_day_of(0), "1970-01-01");
        assert_eq!(utc_day_of(1_700_000_000), "2023-11-14");
        assert_eq!(utc_day_of(86_399), "1970-01-01");
        assert_eq!(utc_day_of(86_400), "1970-01-02");
    }
    #[test]
    fn day_arithmetic() {
        assert_eq!(days_ago("2026-03-01", 1), "2026-02-28");
        assert_eq!(days_ago("2024-03-01", 1), "2024-02-29");
        assert_eq!(days_ago("2026-01-01", 6), "2025-12-26");
        assert_eq!(days_ago("2026-11-15", 0), "2026-11-15");
    }
}
