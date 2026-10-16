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
        let (hours, _) = self.at(utc);
        UtcOffset::from_hms(hours, 0, 0).unwrap_or(UtcOffset::UTC)
    }

    /// The zone's abbreviation at `utc`, as the IANA database names it: CET or CEST, EET or EEST.
    fn abbreviation(self, utc: OffsetDateTime) -> &'static str {
        match (self, self.at(utc).1) {
            (Zone::Bratislava | Zone::Prague, false) => "CET",
            (Zone::Bratislava | Zone::Prague, true) => "CEST",
            (Zone::Helsinki, false) => "EET",
            (Zone::Helsinki, true) => "EEST",
        }
    }

    /// The hours ahead of UTC at `utc`, and whether summer time is on.
    fn at(self, utc: OffsetDateTime) -> (i8, bool) {
        let standard = match self {
            Zone::Bratislava | Zone::Prague => 1,
            Zone::Helsinki => 2,
        };
        let summer = |month| {
            last_sunday(utc.year(), month)
                .with_hms(1, 0, 0)
                .map(|at| at.assume_utc())
        };
        match (summer(Month::March), summer(Month::October)) {
            (Ok(from), Ok(until)) if utc >= from && utc < until => (standard + 1, true),
            _ => (standard, false),
        }
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

/// The line every model call reads beside the question, local first as a person says it, then
/// UTC: `Now: Friday 2026-10-16 14:17 CEST (Europe/Bratislava, UTC+02:00); 12:17 UTC (2026-10-16T12:17Z).`
pub fn now_line(utc: OffsetDateTime, zone: Option<Zone>) -> String {
    let utc = utc.to_offset(UtcOffset::UTC);
    let iso = format!("{}T{:02}:{:02}Z", utc.date(), utc.hour(), utc.minute());
    let Some(zone) = zone else {
        return format!(
            "Now: {} {} {:02}:{:02} UTC ({iso}).",
            weekday(utc.weekday()),
            utc.date(),
            utc.hour(),
            utc.minute()
        );
    };
    let offset = zone.offset(utc);
    let local = utc.to_offset(offset);
    format!(
        "Now: {} {} {:02}:{:02} {} ({}, UTC+{:02}:00); {:02}:{:02} UTC ({iso}).",
        weekday(local.weekday()),
        local.date(),
        local.hour(),
        local.minute(),
        zone.abbreviation(utc),
        zone.name(),
        offset.whole_hours(),
        utc.hour(),
        utc.minute()
    )
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
    fn the_line_is_local_first_with_its_zone_then_utc() {
        // The owner's reference (T-4339): "Je piatok 16. októbra 2026, 14:17 (CEST)."
        assert_eq!(
            now_line(datetime!(2026-10-16 12:17:59 UTC), Some(Zone::Bratislava)),
            "Now: Friday 2026-10-16 14:17 CEST (Europe/Bratislava, UTC+02:00); 12:17 UTC (2026-10-16T12:17Z)."
        );
        assert_eq!(
            now_line(datetime!(2026-10-16 12:17 UTC), None),
            "Now: Friday 2026-10-16 12:17 UTC (2026-10-16T12:17Z)."
        );
        // Past midnight in Helsinki, still the day before in UTC: the local day is the one "today" means.
        assert_eq!(
            now_line(datetime!(2026-12-31 22:30 UTC), Some(Zone::Helsinki)),
            "Now: Friday 2027-01-01 00:30 EET (Europe/Helsinki, UTC+02:00); 22:30 UTC (2026-12-31T22:30Z)."
        );
        // An offset moment reads as the same instant.
        assert!(now_line(datetime!(2026-10-16 14:17 +2), None).ends_with("(2026-10-16T12:17Z)."));
    }

    /// 2026-10-25, 01:00 UTC: CEST becomes CET, and 02:30 local comes twice, an hour apart.
    #[test]
    fn the_night_summer_time_ends_names_each_02_30_by_its_zone() {
        assert_eq!(
            now_line(datetime!(2026-10-25 00:30 UTC), Some(Zone::Bratislava)),
            "Now: Sunday 2026-10-25 02:30 CEST (Europe/Bratislava, UTC+02:00); 00:30 UTC (2026-10-25T00:30Z)."
        );
        assert_eq!(
            now_line(datetime!(2026-10-25 01:30 UTC), Some(Zone::Bratislava)),
            "Now: Sunday 2026-10-25 02:30 CET (Europe/Bratislava, UTC+01:00); 01:30 UTC (2026-10-25T01:30Z)."
        );
        assert_eq!(
            now_line(datetime!(2026-10-25 01:30 UTC), Some(Zone::Prague)),
            "Now: Sunday 2026-10-25 02:30 CET (Europe/Prague, UTC+01:00); 01:30 UTC (2026-10-25T01:30Z)."
        );
        assert_eq!(
            Zone::Helsinki.abbreviation(datetime!(2026-10-25 00:30 UTC)),
            "EEST"
        );
        assert_eq!(
            Zone::Helsinki.abbreviation(datetime!(2026-10-25 01:30 UTC)),
            "EET"
        );
        // And back: 2027-03-28 01:00 UTC, CET becomes CEST.
        assert_eq!(
            Zone::Prague.abbreviation(datetime!(2027-03-28 00:59 UTC)),
            "CET"
        );
        assert_eq!(
            Zone::Prague.abbreviation(datetime!(2027-03-28 01:00 UTC)),
            "CEST"
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
