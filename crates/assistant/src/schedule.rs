//! When a `KnowledgeSource` is read again: its five-field cron (`minute hour day month weekday`,
//! jc-core checks the shape), matched against the minute the worker ticks in (T-3052).
//!
//! The worker ticks once a minute and enqueues a source whose cron names that minute; a source
//! never crawled is due at once. A minute the worker was down is not caught up: the next match
//! reads the source, which is what a schedule of "read again" asks for.

/// A source with no schedule is read once a day at 03:00, the example Architecture/22 gives.
pub const DEFAULT: &str = "0 3 * * *";

/// One field: `*`, `n`, `a-b`, `*/s`, `a-b/s` or a comma list of those, within `min..=max`.
fn field_matches(field: &str, value: u32, min: u32, max: u32) -> Option<bool> {
    let mut any = false;
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => (range, step.parse::<u32>().ok().filter(|s| *s > 0)?),
            None => (part, 1),
        };
        let (low, high) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (a.parse().ok()?, b.parse().ok()?)
        } else {
            let n: u32 = range.parse().ok()?;
            // `n/s` starts at n and runs to the field's end, as Vixie cron reads it.
            (n, if part.contains('/') { max } else { n })
        };
        if low < min || high > max || low > high {
            return None;
        }
        any |= value >= low && value <= high && (value - low).is_multiple_of(step);
    }
    Some(any)
}

/// Whether `cron` names the minute `at`; `None` for an expression that is not five valid fields.
/// Day of month and weekday follow cron's rule: when both are restricted, either one matching is
/// enough. Sunday is 0 or 7.
pub fn matches(cron: &str, at: time::OffsetDateTime) -> Option<bool> {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    let [minute, hour, day, month, weekday] = fields.as_slice() else {
        return None;
    };
    let weekday_now = at.weekday().number_days_from_sunday() as u32;
    let weekday_ok = field_matches(weekday, weekday_now, 0, 7)?
        || (weekday_now == 0 && field_matches(weekday, 7, 0, 7)?);
    let day_ok = field_matches(day, at.day() as u32, 1, 31)?;
    let day_and_weekday = match (*day == "*", *weekday == "*") {
        (false, false) => day_ok || weekday_ok,
        _ => day_ok && weekday_ok,
    };
    Some(
        field_matches(minute, at.minute() as u32, 0, 59)?
            && field_matches(hour, at.hour() as u32, 0, 23)?
            && field_matches(month, at.month() as u32, 1, 12)?
            && day_and_weekday,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn the_default_reads_once_a_day_at_three() {
        assert_eq!(
            matches(DEFAULT, datetime!(2026-10-06 03:00 UTC)),
            Some(true)
        );
        assert_eq!(
            matches(DEFAULT, datetime!(2026-10-06 03:01 UTC)),
            Some(false)
        );
        assert_eq!(
            matches(DEFAULT, datetime!(2026-10-06 04:00 UTC)),
            Some(false)
        );
    }

    #[test]
    fn steps_ranges_and_lists_are_read_as_cron_reads_them() {
        let at = datetime!(2026-10-06 14:30 UTC); // a Tuesday
        assert_eq!(matches("*/15 * * * *", at), Some(true));
        assert_eq!(matches("*/7 * * * *", at), Some(false));
        assert_eq!(matches("30 9-17 * * 1-5", at), Some(true));
        assert_eq!(matches("30 9-17 * * 6,0", at), Some(false));
        assert_eq!(matches("0,30 14 6 10 *", at), Some(true));
        assert_eq!(matches("30 14 * * 2", at), Some(true));
        assert_eq!(matches("10/20 * * * *", at), Some(true)); // 10, 30, 50
    }

    #[test]
    fn day_and_weekday_restricted_together_match_on_either() {
        // The 6th, or any Friday: the 6th of October 2026 is a Tuesday.
        assert_eq!(
            matches("0 0 6 * 5", datetime!(2026-10-06 00:00 UTC)),
            Some(true)
        );
        assert_eq!(
            matches("0 0 7 * 5", datetime!(2026-10-09 00:00 UTC)),
            Some(true)
        );
        assert_eq!(
            matches("0 0 7 * 5", datetime!(2026-10-08 00:00 UTC)),
            Some(false)
        );
        // Sunday as 7.
        assert_eq!(
            matches("0 0 * * 7", datetime!(2026-10-11 00:00 UTC)),
            Some(true)
        );
    }

    #[test]
    fn an_expression_that_is_not_cron_is_none_not_a_guess() {
        let at = datetime!(2026-10-06 03:00 UTC);
        for bad in [
            "",
            "0 3 * *",
            "0 3 * * * *",
            "61 * * * *",
            "* 24 * * *",
            "*/0 * * * *",
            "5-1 * * * *",
            "a * * * *",
        ] {
            assert_eq!(matches(bad, at), None, "{bad}");
        }
    }
}
