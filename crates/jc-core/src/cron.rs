//! Five-field cron, `minute hour day month weekday`, read in UTC as Vixie cron reads it: the
//! schedule of a `KnowledgeSource` (T-3052) and of a `wasm` App's jobs (AP-154). One reader, so a
//! schedule the manifest check accepted is the schedule the runner keeps.

/// One field: `*`, `n`, `a-b`, `*/s`, `a-b/s` or a comma list of those, within `min..=max`;
/// `None` for a field that is not one of those.
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

/// The five fields, or `None` for any other count.
fn fields(cron: &str) -> Option<[&str; 5]> {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    fields.try_into().ok()
}

/// Whether `cron` names the minute given by its parts (weekday 0 = Sunday); `None` for an
/// expression that is not five valid fields. Day of month and weekday follow cron's rule: when
/// both are restricted, either one matching is enough. Sunday is 0 or 7.
pub fn matches(
    cron: &str,
    minute: u32,
    hour: u32,
    day: u32,
    month: u32,
    weekday: u32,
) -> Option<bool> {
    let [m, h, d, mo, wd] = fields(cron)?;
    let weekday_ok =
        field_matches(wd, weekday, 0, 7)? || (weekday == 0 && field_matches(wd, 7, 0, 7)?);
    let day_ok = field_matches(d, day, 1, 31)?;
    let day_and_weekday = match (d == "*", wd == "*") {
        (false, false) => day_ok || weekday_ok,
        _ => day_ok && weekday_ok,
    };
    Some(
        field_matches(m, minute, 0, 59)?
            && field_matches(h, hour, 0, 23)?
            && field_matches(mo, month, 1, 12)?
            && day_and_weekday,
    )
}

/// Whether `cron` is five valid fields.
pub fn is_valid(cron: &str) -> bool {
    matches(cron, 0, 0, 1, 1, 0).is_some()
}

/// The shortest time, in minutes, between two runs on one day or from a day's last run to the
/// next day's first: a lower bound of the gap between any two runs, since the day fields only
/// leave days out. `None` for an expression that is not cron, or one that names no minute of a day.
pub fn shortest_gap_minutes(cron: &str) -> Option<u32> {
    let [m, h, ..] = fields(cron)?;
    is_valid(cron).then_some(())?;
    let mut day: Vec<u32> = Vec::new();
    for hour in 0..24 {
        if field_matches(h, hour, 0, 23)? {
            for minute in 0..60 {
                if field_matches(m, minute, 0, 59)? {
                    day.push(hour * 60 + minute);
                }
            }
        }
    }
    let (first, last) = (*day.first()?, *day.last()?);
    let within = day.windows(2).map(|pair| pair[1] - pair[0]).min();
    let across = first + 24 * 60 - last;
    Some(within.map_or(across, |within| within.min(across)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_ranges_and_lists_are_read_as_cron_reads_them() {
        // 14:30 on Tuesday the 6th of October.
        let at = |cron: &str| matches(cron, 30, 14, 6, 10, 2);
        assert_eq!(at("*/15 * * * *"), Some(true));
        assert_eq!(at("*/7 * * * *"), Some(false));
        assert_eq!(at("30 9-17 * * 1-5"), Some(true));
        assert_eq!(at("30 9-17 * * 6,0"), Some(false));
        assert_eq!(at("0,30 14 6 10 *"), Some(true));
        assert_eq!(at("10/20 * * * *"), Some(true)); // 10, 30, 50
    }

    #[test]
    fn day_and_weekday_restricted_together_match_on_either_and_sunday_is_seven_too() {
        assert_eq!(matches("0 0 6 * 5", 0, 0, 6, 10, 2), Some(true));
        assert_eq!(matches("0 0 7 * 5", 0, 0, 9, 10, 5), Some(true));
        assert_eq!(matches("0 0 7 * 5", 0, 0, 8, 10, 4), Some(false));
        assert_eq!(matches("0 0 * * 7", 0, 0, 11, 10, 0), Some(true));
    }

    #[test]
    fn an_expression_that_is_not_cron_is_none_not_a_guess() {
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
            assert!(!is_valid(bad), "{bad}");
            assert_eq!(shortest_gap_minutes(bad), None, "{bad}");
        }
        assert!(is_valid("0 3 * * *"));
    }

    #[test]
    fn the_shortest_gap_is_within_a_day_or_across_midnight() {
        assert_eq!(shortest_gap_minutes("0 * * * *"), Some(60));
        assert_eq!(shortest_gap_minutes("*/5 * * * *"), Some(5));
        assert_eq!(shortest_gap_minutes("* * * * *"), Some(1));
        assert_eq!(shortest_gap_minutes("0,3 9 * * *"), Some(3));
        assert_eq!(shortest_gap_minutes("0 3 * * *"), Some(24 * 60));
        // 23:58 and 00:01 the next day are three minutes apart.
        assert_eq!(shortest_gap_minutes("1,58 0,23 * * *"), Some(3));
    }
}
