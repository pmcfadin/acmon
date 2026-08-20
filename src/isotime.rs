//! Reading and writing the ISO 8601 timestamps that Codex records in its session index,
//! and that acmon's own state file records between runs.
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

    // Days per month, not merely 1..=31. `days_from_civil` below shifts the year to start
    // in March, so an impossible day rolls silently into the next month: 31 February
    // becomes 3 March, two days adrift. That is a plausible wrong time rather than an
    // error, and two days is more than enough to cross a recency threshold.
    if !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
        return Err(malformed());
    }
    if hour > 23 || minute > 59 || second > 60 {
        return Err(malformed());
    }

    Ok(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// How many days a month has, in a given year.
fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        // Not a month. The caller checks the range too, so this is unreachable; zero is
        // the safe answer because it makes every day of it invalid.
        _ => 0,
    }
}

/// The Gregorian rule in full: every fourth year, except centuries, except every fourth
/// century. Getting the exceptions wrong shifts a date by a day for a whole century.
fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Seconds since the Unix epoch, for a time that may precede it.
///
/// Shared by `memory.rs` and `state.rs` for their ISO 8601 timestamp serialization.
/// Extracted here to avoid duplication; the conversion logic itself is kept in this module
/// since it pairs with `iso8601_from_unix_seconds`.
pub fn unix_seconds(time: std::time::SystemTime) -> i64 {
    use std::time::UNIX_EPOCH;
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => since.as_secs() as i64,
        // Before the epoch. Should not occur, but negating is the correct reading rather
        // than clamping to zero, which would silently move the time to 1970.
        Err(before) => -(before.duration().as_secs() as i64),
    }
}

/// A time from seconds since the Unix epoch, for a value that may be negative.
///
/// The inverse of [`unix_seconds`]. Shared by `memory.rs` and `state.rs` for deserializing
/// ISO 8601 timestamps back to `SystemTime`.
pub fn time_from_unix_seconds(seconds: i64) -> std::time::SystemTime {
    use std::time::{Duration, UNIX_EPOCH};
    if seconds >= 0 {
        UNIX_EPOCH + Duration::from_secs(seconds as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs())
    }
}

/// A UTC ISO 8601 timestamp from seconds since the Unix epoch.
///
/// The inverse of [`unix_seconds_from_iso8601`], to whole seconds. It exists so that the
/// state acmon carries between runs is written in a form a human can check by eye: the
/// alternative — an epoch integer — is a fact nobody can dispute because nobody can read
/// it, and this tool's whole subject is figures that were believed without being checked.
///
/// Years outside four digits are printed with however many digits they have rather than
/// being truncated to fit. A timestamp is never shortened to look right.
pub fn iso8601_from_unix_seconds(seconds: i64) -> String {
    // Euclidean, not truncating, division: a negative second count belongs to the day
    // *before* the epoch, and `-1 / 86_400 == 0` in Rust would place it on 1 January 1970
    // and give an hour of -1. Pre-epoch times should not occur here, but a wrong one that
    // still parses is worse than one that is obviously absurd.
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);

    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z",
        hour = second_of_day / 3_600,
        minute = (second_of_day % 3_600) / 60,
        second = second_of_day % 60,
    )
}

/// The proleptic Gregorian date a day count since 1970-01-01 names.
///
/// Howard Hinnant's `civil_from_days`, the exact inverse of [`days_from_civil`] below. The
/// two are kept adjacent on purpose: they share the March-first year shift, and a change to
/// one that is not mirrored in the other is a silent one-day error.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365; // [0, 399]
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100); // [0, 365]
    let month_shifted = (5 * day_of_year + 2) / 153; // [0, 11], March being 0
    let day = day_of_year - (153 * month_shifted + 2) / 5 + 1; // [1, 31]
    let month = if month_shifted < 10 {
        month_shifted + 3
    } else {
        month_shifted - 9
    };

    // January and February belong to the following calendar year, because the internal
    // year starts in March so that the leap day lands last.
    (year + i64::from(month <= 2), month, day)
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
    use super::{iso8601_from_unix_seconds, unix_seconds_from_iso8601};

    /// Expected strings come from `date -u -r <seconds> +%Y-%m-%dT%H:%M:%SZ`, not from this
    /// module's own arithmetic — the point is to disagree with the system if we are wrong.
    #[test]
    fn writes_what_the_system_date_command_writes() {
        let cases = [
            (1_786_987_902, "2026-08-17T17:31:42Z"),
            // Leap days, and the century rules that surround them, are where hand-rolled
            // date arithmetic breaks. Each of these is a day the naive rule gets wrong.
            (1_709_208_000, "2024-02-29T12:00:00Z"),
            (951_825_600, "2000-02-29T12:00:00Z"),
            (4_107_542_400, "2100-03-01T00:00:00Z"),
            (0, "1970-01-01T00:00:00Z"),
            // The last second of a day, and the first of the next: an off-by-one in the
            // day/second split shows up here and nowhere else.
            (86_399, "1970-01-01T23:59:59Z"),
            (86_400, "1970-01-02T00:00:00Z"),
        ];

        for (input, expected) in cases {
            assert_eq!(iso8601_from_unix_seconds(input), expected, "{input}");
        }
    }

    /// The state file is written by one and read by the other, so a disagreement between
    /// them would lose every remembered timestamp — quietly, since both halves would still
    /// produce plausible values.
    #[test]
    fn round_trips_through_the_parser() {
        // Every 7-hour-and-change step across a little over a century, so the walk crosses
        // leap days, century boundaries, and every month length without listing them.
        let mut seconds: i64 = 0;
        while seconds < 4_200_000_000 {
            let written = iso8601_from_unix_seconds(seconds);
            assert_eq!(
                unix_seconds_from_iso8601(&written),
                Ok(seconds),
                "{written} came from {seconds}"
            );
            seconds += 25_931;
        }
    }

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
            // 2000 was a leap year despite being a century, because it divides by 400.
            // A rule that only excluded centuries would reject this valid date.
            ("2000-02-29T12:00:00Z", 951_825_600),
            // 2100 is not a leap year, so the day after 28 February is 1 March.
            ("2100-03-01T00:00:00Z", 4_107_542_400),
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
            // Days that do not exist in their month. Each of these would otherwise roll
            // forward into the next month and come back as a plausible time: 31 February
            // 2026 as 3 March, and 29 February 2026 as 1 March, both days adrift.
            "2026-02-31T12:00:00Z",
            "2026-02-29T12:00:00Z", // 2026 is not a leap year
            "2026-04-31T12:00:00Z",
            "2026-06-31T12:00:00Z",
            "2026-09-31T12:00:00Z",
            "2026-11-31T12:00:00Z",
            "1900-02-29T12:00:00Z", // a century that is not a leap year
        ] {
            assert!(
                unix_seconds_from_iso8601(input).is_err(),
                "{input:?} must be an error, not a time"
            );
        }
    }
}
