//! When a `KnowledgeSource` is read again: its five-field cron (`minute hour day month weekday`,
//! jc-core checks the shape), matched against the minute the worker ticks in (T-3052).
//!
//! The worker ticks once a minute and enqueues a source whose cron names that minute; a source
//! never crawled is due at once. A minute the worker was down is not caught up: the next match
//! reads the source, which is what a schedule of "read again" asks for.

/// A source with no schedule is read once a day at 03:00, the example Architecture/22 gives.
pub const DEFAULT: &str = "0 3 * * *";

/// Whether `cron` names the minute `at`; `None` for an expression that is not five valid fields.
/// The reading is jc-core's, the one the manifest check uses (`jc_core::cron`).
pub fn matches(cron: &str, at: time::OffsetDateTime) -> Option<bool> {
    jc_core::cron::matches(
        cron,
        at.minute() as u32,
        at.hour() as u32,
        at.day() as u32,
        at.month() as u32,
        at.weekday().number_days_from_sunday() as u32,
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
