//! Reading the ISO 8601 timestamps that Codex records in its session index.
//!
//! Deliberately not a general date library. It parses exactly the shape Codex writes —
//! `2026-08-17T17:31:42.384761Z` — and refuses anything else rather than guessing. A
//! timestamp misread as older than it is would hide a live session; misread as newer, it
//! would make the tool open transcripts it has no reason to touch.

/// Seconds since the Unix epoch, from a UTC ISO 8601 timestamp.
///
/// Fractional seconds are accepted and discarded; the recency decisions this feeds are
/// measured in hours. A trailing `Z` is required, because a local-time string would be
/// wrong by the offset and there would be no way to tell.
pub fn unix_seconds_from_iso8601(text: &str) -> Result<i64, String> {
    let malformed = || format!("{text:?} is not a UTC ISO 8601 timestamp");

    let text = text.strip_suffix('Z').ok_or_else(malformed)?;
    let (date, time) = text.split_once('T').ok_or_else(malformed)?;
    // Fractional seconds carry no information this needs.
    let time = time.split('.').next().ok_or_else(malformed)?;

    let date: Vec<&str> = date.split('-').collect();
    let time: Vec<&str> = time.split(':').collect();
    let ([year, month, day], [hour, minute, second]) = (
        <[&str; 3]>::try_from(date).map_err(|_| malformed())?,
        <[&str; 3]>::try_from(time).map_err(|_| malformed())?,
    );

    let year: i64 = year.parse().map_err(|_| malformed())?;
    let month: i64 = month.parse().map_err(|_| malformed())?;
    let day: i64 = day.parse().map_err(|_| malformed())?;
    let hour: i64 = hour.parse().map_err(|_| malformed())?;
    let minute: i64 = minute.parse().map_err(|_| malformed())?;
    let second: i64 = second.parse().map_err(|_| malformed())?;

    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(malformed());
    }
    if hour > 23 || minute > 59 || second > 60 {
        return Err(malformed());
    }

    Ok(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
///
/// Howard Hinnant's `days_from_civil`, which handles the leap-year rules without a
/// lookup table by shifting the year to start in March, so the leap day lands last.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::unix_seconds_from_iso8601;

    /// Expected values come from `date -u -j -f "%Y-%m-%dT%H:%M:%S" ... +%s`, not from
    /// this module's own arithmetic. The first is a real timestamp from the Codex index
    /// on this machine.
    #[test]
    fn matches_what_the_system_date_command_says() {
        let cases = [
            ("2026-08-17T17:31:42.384761Z", 1_786_987_902),
            ("2026-08-17T17:31:42Z", 1_786_987_902),
            // A leap day, which is where hand-rolled date arithmetic usually breaks.
            ("2024-02-29T12:00:00Z", 1_709_208_000),
            ("1970-01-01T00:00:00Z", 0),
        ];

        for (input, expected) in cases {
            assert_eq!(unix_seconds_from_iso8601(input), Ok(expected), "{input}");
        }
    }

    #[test]
    fn refuses_anything_it_cannot_read_exactly() {
        // Never a zero, which would read as 1970 and make every session look ancient —
        // and therefore make the tool skip reading any of them.
        for input in [
            "",
            "2026-08-17",
            "2026-08-17T17:31:42",       // no zone: could be off by hours
            "2026-08-17T17:31:42+02:00", // not UTC
            "2026-13-01T00:00:00Z",      // month 13
            "2026-08-32T00:00:00Z",      // day 32
            "2026-08-17T25:00:00Z",      // hour 25
            "not-a-date",
        ] {
            assert!(
                unix_seconds_from_iso8601(input).is_err(),
                "{input:?} must be an error, not a time"
            );
        }
    }
}
