//! Slack timestamps ("1700000000.000100") as local clock text. The
//! plugin has no libc time; the offset is the user's `tz_offset` from
//! Slack, applied to a civil-from-days conversion.

/// Seconds of a Slack ts.
pub fn secs(ts: &str) -> i64 {
    ts.split('.').next().and_then(|s| s.parse().ok()).unwrap_or(0)
}

/// (days since 1970-01-01, hour, minute) in the zone `tz_offset` seconds
/// east of UTC.
pub fn local(secs: i64, tz_offset: i64) -> (i64, u32, u32) {
    let t = secs + tz_offset;
    let days = t.div_euclid(86_400);
    let rem = t.rem_euclid(86_400);
    (days, (rem / 3600) as u32, ((rem % 3600) / 60) as u32)
}

/// Civil date of a day count (Howard Hinnant's algorithm).
pub fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// "Today", "Yesterday", "Tue Oct 29", or "Oct 29 2025" in another year.
pub fn day_label(days: i64, today_days: i64) -> String {
    if days == today_days {
        return "Today".into();
    }
    if days == today_days - 1 {
        return "Yesterday".into();
    }
    let (y, m, d) = civil(days);
    let (ty, _, _) = civil(today_days);
    let wd = WEEKDAYS[days.rem_euclid(7) as usize];
    if y == ty {
        format!("{wd} {} {d}", MONTHS[(m - 1) as usize])
    } else {
        format!("{} {d} {y}", MONTHS[(m - 1) as usize])
    }
}

pub fn hhmm(h: u32, m: u32) -> String {
    format!("{h:02}:{m:02}")
}

/// Clock text of a ts: "12:34".
pub fn clock(ts: &str, tz_offset: i64) -> String {
    let (_, h, m) = local(secs(ts), tz_offset);
    hhmm(h, m)
}

/// "2026-10-30 14:02" of a ts: the stamp on a copied message.
pub fn stamp(ts: &str, tz_offset: i64) -> String {
    let (days, h, m) = local(secs(ts), tz_offset);
    let (y, mo, d) = civil(days);
    format!("{y:04}-{mo:02}-{d:02} {}", hhmm(h, m))
}

/// "3m", "2h", "5d" since the ts.
pub fn age(ts: &str, now_ms: u64) -> String {
    let now = (now_ms / 1000) as i64;
    let then = secs(ts);
    listkit::text::fmt_age(now.saturating_sub(then).max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(19_660), (2023, 10, 30));
        let (days, h, m) = local(1_698_666_000, -7 * 3600); // 2023-10-30 11:40 UTC
        assert_eq!((h, m), (4, 40));
        assert_eq!(civil(days), (2023, 10, 30));
        assert_eq!(WEEKDAYS[days.rem_euclid(7) as usize], "Mon");
    }

    #[test]
    fn labels() {
        assert_eq!(day_label(100, 100), "Today");
        assert_eq!(day_label(99, 100), "Yesterday");
        assert_eq!(day_label(19_660, 19_665), "Mon Oct 30");
        assert_eq!(day_label(19_660, 20_100), "Oct 30 2023");
        assert_eq!(clock("1698666000.000100", 0), "11:40");
        assert_eq!(stamp("1698666000.000100", -7 * 3600), "2023-10-30 04:40");
    }
}
