//! Schedules: `every <interval>` and five-field cron expressions, with
//! the next-occurrence arithmetic done in pure Rust (Howard Hinnant's
//! civil-date algorithms; no chrono). Times are Unix milliseconds; cron
//! fields are evaluated in local time by way of a UTC offset in minutes
//! that the scheduler re-reads on every wake.

use std::fmt;

/// Smallest interval accepted, so a typo cannot hammer the server.
pub const MIN_EVERY_MS: u64 = 1000;
/// How far ahead a cron expression is searched before it is declared
/// "never fires" (Feb 30, or a dom/dow pair that never coincides).
const MAX_SEARCH_DAYS: i64 = 5 * 366;

const MS_PER_MIN: i64 = 60_000;
const MS_PER_DAY: i64 = 86_400_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    /// A fixed period, milliseconds.
    Every(u64),
    Cron(CronExpr),
}

/// A parsed five-field expression: `minute hour day-of-month month
/// day-of-week`, each a bitmask of the allowed values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    minute: u64,
    hour: u32,
    dom: u32,
    month: u16,
    /// Bit 0 = Sunday .. bit 6 = Saturday.
    dow: u8,
    dom_star: bool,
    dow_star: bool,
    /// The text as given, for display.
    text: String,
}

impl fmt::Display for Schedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Schedule::Every(ms) => write!(f, "every {}", fmt_duration(*ms)),
            Schedule::Cron(c) => f.write_str(&c.text),
        }
    }
}

/// `every 15m`, `every 2s`, or five cron fields.
pub fn parse(s: &str) -> Result<Schedule, String> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("every") {
        if rest.starts_with(char::is_whitespace) || rest.is_empty() {
            return parse_every(rest).map(Schedule::Every);
        }
    }
    parse_cron(s).map(Schedule::Cron)
}

/// "15m" / "2s" / "1h" / "1d" / "500ms" (rejected: under 1 s) -> ms.
pub fn parse_every(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("every needs an interval, e.g. `every 15m`".into());
    }
    let (num, mult) = if let Some(n) = s.strip_suffix("ms") {
        (n, 1)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1000)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60_000)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3_600_000)
    } else if let Some(n) = s.strip_suffix('d') {
        (n, 86_400_000)
    } else {
        (s, 1000)
    };
    let n: u64 = num
        .trim()
        .parse()
        .map_err(|_| format!("bad interval {s:?} (want e.g. 30s, 15m, 2h, 1d)"))?;
    let ms = n
        .checked_mul(mult)
        .ok_or_else(|| format!("interval {s:?} is too large"))?;
    if ms < MIN_EVERY_MS {
        return Err(format!("interval {s:?} is below the 1s floor"));
    }
    Ok(ms)
}

fn parse_cron(s: &str) -> Result<CronExpr, String> {
    let fields: Vec<&str> = s.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!(
            "schedule must be `every <interval>` or 5 cron fields \
             (minute hour day month weekday), got {} field(s)",
            fields.len()
        ));
    }
    let minute = parse_field(fields[0], 0, 59, "minute")?;
    let hour = parse_field(fields[1], 0, 23, "hour")? as u32;
    let dom = parse_field(fields[2], 1, 31, "day-of-month")? as u32;
    let month = parse_field(fields[3], 1, 12, "month")? as u16;
    let mut dow = parse_field(fields[4], 0, 7, "day-of-week")? as u8;
    // 7 is Sunday too.
    if dow & (1 << 7) != 0 {
        dow = (dow & 0x7f) | 1;
    }
    Ok(CronExpr {
        minute,
        hour,
        dom,
        month,
        dow,
        dom_star: fields[2] == "*",
        dow_star: fields[4] == "*",
        text: fields.join(" "),
    })
}

/// One cron field: `*`, `*/n`, `a`, `a-b`, `a-b/n`, and comma lists of
/// those. Numeric only. Returns a bitmask over `min..=max`.
fn parse_field(field: &str, min: u32, max: u32, name: &str) -> Result<u64, String> {
    let bad = |what: &str| {
        format!("bad {name} field {field:?}: {what} (valid values {min}-{max})")
    };
    let mut mask = 0u64;
    for part in field.split(',') {
        if part.is_empty() {
            return Err(bad("empty list item"));
        }
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s.parse().map_err(|_| bad("bad step"))?;
                if step == 0 {
                    return Err(bad("step must be at least 1"));
                }
                (r, step)
            }
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            let a: u32 = a.parse().map_err(|_| bad("not a number"))?;
            let b: u32 = b.parse().map_err(|_| bad("not a number"))?;
            if a > b {
                return Err(bad("range start is after its end"));
            }
            (a, b)
        } else {
            let a: u32 = range.parse().map_err(|_| bad("not a number"))?;
            // `5/10` means "5, 15, 25, ..." in Vixie cron.
            if step > 1 { (a, max) } else { (a, a) }
        };
        if lo < min || hi > max {
            return Err(bad("out of range"));
        }
        let mut v = lo;
        while v <= hi {
            mask |= 1u64 << v;
            v += step;
        }
    }
    if mask == 0 {
        return Err(bad("matches nothing"));
    }
    Ok(mask)
}

// ---------------------------------------------------------------------------
// Civil time (Hinnant). Days are relative to 1970-01-01; weekday 0 = Sunday.
// ---------------------------------------------------------------------------

#[cfg(test)]
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn weekday_from_days(z: i64) -> u32 {
    ((z + 4).rem_euclid(7)) as u32
}

impl CronExpr {
    fn day_matches(&self, month: u32, dom: u32, dow: u32) -> bool {
        if self.month & (1 << month) == 0 {
            return false;
        }
        let dom_ok = self.dom & (1 << dom) != 0;
        let dow_ok = self.dow & (1 << dow) != 0;
        // Vixie rule: with both restricted, either one matching is a
        // match; a `*` on one side defers to the other.
        match (self.dom_star, self.dow_star) {
            (true, true) => true,
            (true, false) => dow_ok,
            (false, true) => dom_ok,
            (false, false) => dom_ok || dow_ok,
        }
    }

    /// First firing strictly after `after_local_ms` (local wall clock as
    /// a Unix-style millisecond count).
    fn next_local(&self, after_local_ms: i64) -> Option<i64> {
        let mut minute_ms = after_local_ms.div_euclid(MS_PER_MIN) * MS_PER_MIN + MS_PER_MIN;
        let mut day = minute_ms.div_euclid(MS_PER_DAY);
        let mut in_day = minute_ms - day * MS_PER_DAY;
        for _ in 0..MAX_SEARCH_DAYS {
            let (_, m, d) = civil_from_days(day);
            if self.day_matches(m, d, weekday_from_days(day)) {
                let mut h = (in_day / 3_600_000) as u32;
                let mut min = ((in_day % 3_600_000) / MS_PER_MIN) as u32;
                while h < 24 {
                    if self.hour & (1 << h) != 0 {
                        while min < 60 {
                            if self.minute & (1u64 << min) != 0 {
                                return Some(
                                    day * MS_PER_DAY
                                        + i64::from(h) * 3_600_000
                                        + i64::from(min) * MS_PER_MIN,
                                );
                            }
                            min += 1;
                        }
                    }
                    h += 1;
                    min = 0;
                }
            }
            day += 1;
            in_day = 0;
            minute_ms = day * MS_PER_DAY;
        }
        let _ = minute_ms;
        None
    }
}

/// The next firing strictly after `now_ms`.
///
/// `Every` keeps a fixed grid anchored at `anchor_ms` (the job's creation
/// time, then its previous `next_run_ms`), so ticks never drift by the
/// run time. `Cron` evaluates in local time via `tz_offset_min`. `None`
/// means the schedule never fires.
pub fn next_after(s: &Schedule, anchor_ms: i64, now_ms: i64, tz_offset_min: i32) -> Option<i64> {
    match s {
        Schedule::Every(p) => {
            let p = *p as i64;
            if anchor_ms > now_ms {
                return Some(anchor_ms);
            }
            let k = (now_ms - anchor_ms) / p + 1;
            Some(anchor_ms + k * p)
        }
        Schedule::Cron(c) => {
            let off = i64::from(tz_offset_min) * MS_PER_MIN;
            c.next_local(now_ms + off).map(|t| t - off)
        }
    }
}

/// The occurrences in `(from_ms, now_ms]` (the missed ones after a stop),
/// oldest first, at most `cap`. `from_ms` itself counts when it is due.
pub fn missed_between(
    s: &Schedule,
    from_ms: i64,
    now_ms: i64,
    tz_offset_min: i32,
    cap: usize,
) -> Vec<i64> {
    let mut out = Vec::new();
    let mut t = from_ms;
    while t <= now_ms && out.len() < cap {
        out.push(t);
        match next_after(s, from_ms, t, tz_offset_min) {
            Some(n) if n > t => t = n,
            _ => break,
        }
    }
    out
}

/// "in 3m", "in 2h05m", "now", "overdue 12s".
pub fn describe_next(next_ms: i64, now_ms: i64) -> String {
    let d = next_ms - now_ms;
    if d.abs() < 1000 {
        return "now".into();
    }
    if d < 0 {
        return format!("overdue {}", fmt_duration((-d) as u64));
    }
    format!("in {}", fmt_duration(d as u64))
}

/// "3s ago", "2h ago".
pub fn describe_ago(then_ms: i64, now_ms: i64) -> String {
    let d = (now_ms - then_ms).max(0) as u64;
    format!("{} ago", fmt_duration(d))
}

/// Compact duration: 45s, 3m, 2h05m, 3d, 340ms.
pub fn fmt_duration(ms: u64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let s = ms / 1000;
    if s < 60 {
        return format!("{s}s");
    }
    let m = s / 60;
    if m < 60 {
        return if s % 60 == 0 { format!("{m}m") } else { format!("{m}m{:02}s", s % 60) };
    }
    let h = m / 60;
    if h < 24 {
        return if m % 60 == 0 { format!("{h}h") } else { format!("{h}h{:02}m", m % 60) };
    }
    let d = h / 24;
    if h % 24 == 0 { format!("{d}d") } else { format!("{d}d{}h", h % 24) }
}

/// Parse a `+0200` / `-0530` offset (tmux's `%z`) into minutes.
pub fn parse_tz_offset(s: &str) -> Option<i32> {
    let s = s.trim();
    let (sign, digits) = match s.chars().next()? {
        '+' => (1, &s[1..]),
        '-' => (-1, &s[1..]),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let h: i32 = digits[..2].parse().ok()?;
    let m: i32 = digits[2..].parse().ok()?;
    Some(sign * (h * 60 + m))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i64, m: u32, d: u32, hh: i64, mm: i64) -> i64 {
        days_from_civil(y, m, d) * MS_PER_DAY + hh * 3_600_000 + mm * MS_PER_MIN
    }

    fn cron(s: &str) -> Schedule {
        parse(s).unwrap()
    }

    #[test]
    fn civil_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(weekday_from_days(0), 4); // Thursday
        for z in [-1000, 0, 19_000, 20_000, 60_000] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
    }

    #[test]
    fn every_parsing() {
        assert_eq!(parse("every 15m").unwrap(), Schedule::Every(900_000));
        assert_eq!(parse("every 2s").unwrap(), Schedule::Every(2000));
        assert_eq!(parse("every 1h").unwrap(), Schedule::Every(3_600_000));
        assert_eq!(parse("every 1d").unwrap(), Schedule::Every(86_400_000));
        assert_eq!(parse("every 30").unwrap(), Schedule::Every(30_000));
        assert!(parse("every 500ms").unwrap_err().contains("floor"));
        assert!(parse("every").is_err());
        assert!(parse("every x").is_err());
        assert_eq!(parse("every 2s").unwrap().to_string(), "every 2s");
        assert_eq!(parse("every 90m").unwrap().to_string(), "every 1h30m");
    }

    #[test]
    fn cron_parsing() {
        let Schedule::Cron(c) = cron("*/15 9-17 * * 1-5") else { panic!() };
        assert_eq!(c.minute, (1 << 0) | (1 << 15) | (1 << 30) | (1 << 45));
        assert_eq!(c.hour, (9..=17).fold(0, |a, h| a | (1 << h)));
        assert_eq!(c.dow, 0b0111110);
        assert!(c.dom_star && !c.dow_star);
        let Schedule::Cron(c) = cron("0 0 1,15 * 7") else { panic!() };
        assert_eq!(c.dow, 1); // 7 folds onto Sunday
        assert_eq!(c.dom, (1 << 1) | (1 << 15));
        assert!(parse("60 * * * *").unwrap_err().contains("0-59"));
        assert!(parse("* * * *").unwrap_err().contains("5 cron fields"));
        assert!(parse("*/0 * * * *").is_err());
        assert!(parse("5-1 * * * *").is_err());
        assert!(parse("mon * * * *").is_err());
        let Schedule::Cron(c) = cron("5/20 * * * *") else { panic!() };
        assert_eq!(c.minute, (1 << 5) | (1 << 25) | (1 << 45));
    }

    #[test]
    fn every_grid_is_anchored() {
        let s = Schedule::Every(2000);
        assert_eq!(next_after(&s, 1000, 1000, 0), Some(3000));
        assert_eq!(next_after(&s, 1000, 1500, 0), Some(3000));
        assert_eq!(next_after(&s, 1000, 3000, 0), Some(5000));
        assert_eq!(next_after(&s, 1000, 9_999, 0), Some(11_000));
        // A future anchor is the next tick itself.
        assert_eq!(next_after(&s, 8000, 1000, 0), Some(8000));
    }

    #[test]
    fn cron_next_quarter_hour() {
        let s = cron("*/15 * * * *");
        assert_eq!(next_after(&s, 0, at(2026, 9, 4, 10, 7), 0), Some(at(2026, 9, 4, 10, 15)));
        // Exactly on a firing: strictly after.
        assert_eq!(next_after(&s, 0, at(2026, 9, 4, 10, 15), 0), Some(at(2026, 9, 4, 10, 30)));
        assert_eq!(next_after(&s, 0, at(2026, 9, 4, 23, 50), 0), Some(at(2026, 9, 5, 0, 0)));
    }

    #[test]
    fn cron_weekdays_skip_the_weekend() {
        let s = cron("0 9 * * 1-5");
        // 2026-09-05 is a Saturday.
        assert_eq!(weekday_from_days(days_from_civil(2026, 9, 5)), 6);
        assert_eq!(next_after(&s, 0, at(2026, 9, 5, 12, 0), 0), Some(at(2026, 9, 7, 9, 0)));
        assert_eq!(next_after(&s, 0, at(2026, 9, 7, 8, 59), 0), Some(at(2026, 9, 7, 9, 0)));
        assert_eq!(next_after(&s, 0, at(2026, 9, 7, 9, 0), 0), Some(at(2026, 9, 8, 9, 0)));
    }

    #[test]
    fn cron_dom_or_dow() {
        // Both restricted: the 13th OR a Friday. 2026-09-11 is a Friday.
        let s = cron("0 0 13 * 5");
        assert_eq!(next_after(&s, 0, at(2026, 9, 9, 0, 0), 0), Some(at(2026, 9, 11, 0, 0)));
        assert_eq!(next_after(&s, 0, at(2026, 9, 11, 0, 0), 0), Some(at(2026, 9, 13, 0, 0)));
        // dom only.
        let s = cron("0 0 13 * *");
        assert_eq!(next_after(&s, 0, at(2026, 9, 9, 0, 0), 0), Some(at(2026, 9, 13, 0, 0)));
    }

    #[test]
    fn cron_feb_29_and_never() {
        let s = cron("0 0 29 2 *");
        assert_eq!(next_after(&s, 0, at(2026, 3, 1, 0, 0), 0), Some(at(2028, 2, 29, 0, 0)));
        assert_eq!(next_after(&cron("0 0 30 2 *"), 0, at(2026, 1, 1, 0, 0), 0), None);
        assert_eq!(next_after(&cron("0 0 31 4 *"), 0, at(2026, 1, 1, 0, 0), 0), None);
    }

    #[test]
    fn tz_offset_shifts_the_utc_answer() {
        let s = cron("0 9 * * *");
        // 09:00 local at +02:00 is 07:00 UTC.
        assert_eq!(next_after(&s, 0, at(2026, 9, 4, 0, 0), 120), Some(at(2026, 9, 4, 7, 0)));
        // At -05:00 it is 14:00 UTC.
        assert_eq!(next_after(&s, 0, at(2026, 9, 4, 0, 0), -300), Some(at(2026, 9, 4, 14, 0)));
        assert_eq!(parse_tz_offset("+0200"), Some(120));
        assert_eq!(parse_tz_offset("-0530"), Some(-330));
        assert_eq!(parse_tz_offset("0200"), None);
        assert_eq!(parse_tz_offset("+02:00"), None);
    }

    #[test]
    fn missed_occurrences() {
        let s = Schedule::Every(2000);
        assert_eq!(missed_between(&s, 10_000, 16_500, 0, 50), vec![10_000, 12_000, 14_000, 16_000]);
        assert_eq!(missed_between(&s, 10_000, 9_000, 0, 50), Vec::<i64>::new());
        assert_eq!(missed_between(&s, 10_000, 100_000, 0, 3), vec![10_000, 12_000, 14_000]);
        let c = cron("0 * * * *");
        let from = at(2026, 9, 4, 10, 0);
        assert_eq!(
            missed_between(&c, from, at(2026, 9, 4, 12, 30), 0, 50),
            vec![from, at(2026, 9, 4, 11, 0), at(2026, 9, 4, 12, 0)]
        );
    }

    #[test]
    fn descriptions() {
        assert_eq!(describe_next(5000, 0), "in 5s");
        assert_eq!(describe_next(0, 500), "now");
        assert_eq!(describe_next(0, 12_000), "overdue 12s");
        assert_eq!(fmt_duration(7_500_000), "2h05m");
        assert_eq!(fmt_duration(340), "340ms");
        assert_eq!(fmt_duration(3 * 86_400_000), "3d");
        assert_eq!(describe_ago(0, 3000), "3s ago");
    }
}
