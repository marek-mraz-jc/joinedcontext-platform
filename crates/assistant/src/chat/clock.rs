//! The moment of a question as every model call reads it (T-4339): the UTC minute, the weekday
//! and the deployment's local time. The request takes the server's clock once and passes it in;
//! nothing here reads the clock, so a test fixes the moment.

use time::{Date, Month, OffsetDateTime, UtcOffset, Weekday};

/// The local time zones of the deployments, by the first answer language that names one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    Bratislava,
    Prague,
    Helsinki,
}

impl Zone {
    /// The zone of the first language in `languages` that is one city's: `sk`, `cs` or `fi`;
    /// `None` (UTC only) when none is.
    pub fn of(languages: &[String]) -> Option<Zone> {
        languages.iter().find_map(|code| match code.as_str() {
            "sk" => Some(Zone::Bratislava),
            "cs" => Some(Zone::Prague),
            "fi" => Some(Zone::Helsinki),
            _ => None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Zone::Bratislava => "Europe/Bratislava",
            Zone::Prague => "Europe/Prague",
            Zone::Helsinki => "Europe/Helsinki",
        }
    }

    /// The offset at `utc`: standard time, one hour more in EU summer time, which all three
    /// keep (from the last Sunday of March to the last Sunday of October, 01:00 UTC both).
    fn offset(self, utc: OffsetDateTime) -> UtcOffset {
        let standard = match self {
            Zone::Bratislava | Zone::Prague => 1,
            Zone::Helsinki => 2,
        };
        let summer = |month| {
            last_sunday(utc.year(), month)
                .with_hms(1, 0, 0)
                .map(|at| at.assume_utc())
        };
        let hours = match (summer(Month::March), summer(Month::October)) {
            (Ok(from), Ok(until)) if utc >= from && utc < until => standard + 1,
            _ => standard,
        };
        UtcOffset::from_hms(hours, 0, 0).unwrap_or(UtcOffset::UTC)
    }
}

/// The last Sunday of a 31-day month (March, October).
fn last_sunday(year: i32, month: Month) -> Date {
    let last = Date::from_calendar_date(year, month, 31).unwrap_or(Date::MIN);
    last - time::Duration::days(i64::from(last.weekday().number_days_from_sunday()))
}

fn weekday(day: Weekday) -> &'static str {
    match day {
        Weekday::Monday => "Monday",
        Weekday::Tuesday => "Tuesday",
        Weekday::Wednesday => "Wednesday",
        Weekday::Thursday => "Thursday",
        Weekday::Friday => "Friday",
        Weekday::Saturday => "Saturday",
        Weekday::Sunday => "Sunday",
    }
}

/// `utc` in the zone's local time; UTC itself without one.
pub fn local(utc: OffsetDateTime, zone: Option<Zone>) -> OffsetDateTime {
    let utc = utc.to_offset(UtcOffset::UTC);
    zone.map_or(utc, |zone| utc.to_offset(zone.offset(utc)))
}

/// The line every model call reads beside the question, like
/// `Now: 2026-10-16T12:05Z (Friday); local time 2026-10-16 14:05 (Friday) Europe/Bratislava, UTC+02:00.`
pub fn now_line(utc: OffsetDateTime, zone: Option<Zone>) -> String {
    let utc = utc.to_offset(UtcOffset::UTC);
    let mut line = format!(
        "Now: {}T{:02}:{:02}Z ({})",
        utc.date(),
        utc.hour(),
        utc.minute(),
        weekday(utc.weekday())
    );
    if let Some(zone) = zone {
        let offset = zone.offset(utc);
        let local = utc.to_offset(offset);
        line.push_str(&format!(
            "; local time {} {:02}:{:02} ({}) {}, UTC+{:02}:00",
            local.date(),
            local.hour(),
            local.minute(),
            weekday(local.weekday()),
            zone.name(),
            offset.whole_hours()
        ));
    }
    line.push('.');
    line
}

/// The clock readings of `line` an answer may quote, `HH:MM` in UTC and in local time: an answer
/// that holds one told the time and is never kept for the next asker.
pub fn readings(utc: OffsetDateTime, zone: Option<Zone>) -> Vec<String> {
    let utc = utc.to_offset(UtcOffset::UTC);
    let mut at = vec![format!("{:02}:{:02}", utc.hour(), utc.minute())];
    if let Some(zone) = zone {
        let local = utc.to_offset(zone.offset(utc));
        at.push(format!("{}:{:02}", local.hour(), local.minute()));
        at.push(format!("{:02}:{:02}", local.hour(), local.minute()));
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn the_line_names_the_utc_minute_the_weekday_and_the_local_time() {
        assert_eq!(
            now_line(datetime!(2026-10-16 12:05:59 UTC), Some(Zone::Bratislava)),
            "Now: 2026-10-16T12:05Z (Friday); local time 2026-10-16 14:05 (Friday) Europe/Bratislava, UTC+02:00."
        );
        assert_eq!(
            now_line(datetime!(2026-10-16 12:05 UTC), None),
            "Now: 2026-10-16T12:05Z (Friday)."
        );
        // Past midnight in Helsinki, still the day before in UTC: the local day is the one "today" means.
        assert_eq!(
            now_line(datetime!(2026-12-31 22:30 UTC), Some(Zone::Helsinki)),
            "Now: 2026-12-31T22:30Z (Thursday); local time 2027-01-01 00:30 (Friday) Europe/Helsinki, UTC+02:00."
        );
        // An offset moment reads as the same instant in UTC.
        assert!(
            now_line(datetime!(2026-10-16 14:05 +2), None).starts_with("Now: 2026-10-16T12:05Z")
        );
    }

    #[test]
    fn summer_time_starts_and_ends_on_the_last_sundays_at_one_utc() {
        let hours = |at| Zone::Prague.offset(at).whole_hours();
        // 2026: summer time from 29 March to 25 October, 01:00 UTC.
        assert_eq!(hours(datetime!(2026-03-29 00:59 UTC)), 1);
        assert_eq!(hours(datetime!(2026-03-29 01:00 UTC)), 2);
        assert_eq!(hours(datetime!(2026-10-25 00:59 UTC)), 2);
        assert_eq!(hours(datetime!(2026-10-25 01:00 UTC)), 1);
        assert_eq!(
            Zone::Helsinki
                .offset(datetime!(2026-07-01 12:00 UTC))
                .whole_hours(),
            3
        );
        assert_eq!(
            Zone::Bratislava
                .offset(datetime!(2026-01-15 12:00 UTC))
                .whole_hours(),
            1
        );
    }

    #[test]
    fn the_zone_is_the_first_city_language_and_none_names_utc_only() {
        let langs = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(Zone::of(&langs(&["en", "fi", "sv"])), Some(Zone::Helsinki));
        assert_eq!(
            Zone::of(&langs(&["sk", "cs", "en"])),
            Some(Zone::Bratislava)
        );
        assert_eq!(Zone::of(&langs(&["cs", "en"])), Some(Zone::Prague));
        assert_eq!(Zone::of(&langs(&["en", "de"])), None);
        assert_eq!(Zone::of(&[]), None);
    }

    #[test]
    fn the_readings_are_the_utc_and_local_clock() {
        assert_eq!(
            readings(datetime!(2026-10-16 07:05 UTC), Some(Zone::Bratislava)),
            vec!["07:05", "9:05", "09:05"]
        );
        assert_eq!(
            readings(datetime!(2026-10-16 07:05 UTC), None),
            vec!["07:05"]
        );
    }
}
