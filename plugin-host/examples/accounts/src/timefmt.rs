//! Time text for the panel: a countdown to a reset, the reset as a clock
//! in the server's local time, and the ISO 8601 stamps the usage API
//! sends. No chrono: the guest has no clock or zone of its own, so the
//! host's epoch milliseconds and a `date +%z` offset read at init are
//! the whole of it.

/// `2026-10-09T04:49:59.656212+00:00`, `...Z`, or `...+05:30` to epoch
/// milliseconds. None for anything else.
pub fn parse_iso(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |a: usize, n: usize| -> Option<i64> { s.get(a..a + n)?.parse::<i64>().ok() };
    let year = num(0, 4)?;
    let month = num(5, 2)?;
    let day = num(8, 2)?;
    let hour = num(11, 2)?;
    let min = num(14, 2)?;
    let sec = num(17, 2)?;
    let mut i = 19;
    let mut millis = 0i64;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let frac = &s[start..i];
        let mut f = 0i64;
        for (k, c) in frac.bytes().take(3).enumerate() {
            f += i64::from(c - b'0') * 10i64.pow(2 - k as u32);
        }
        millis = f;
    }
    let mut off_min = 0i64;
    match b.get(i) {
        Some(b'Z') | None => {}
        Some(&c) if c == b'+' || c == b'-' => {
            let sign = if c == b'+' { 1 } else { -1 };
            let h = num(i + 1, 2)?;
            let m = if b.get(i + 3) == Some(&b':') { num(i + 4, 2)? } else { num(i + 3, 2).unwrap_or(0) };
            off_min = sign * (h * 60 + m);
        }
        _ => return None,
    }
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + min * 60 + sec - off_min * 60;
    Some(secs * 1000 + millis)
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// (year, month, day) for days since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

const MONTHS: [&str; 12] =
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// Local civil time: (year, month, day, hour, minute).
fn local(ms: i64, tz_min: i32) -> (i64, i64, i64, i64, i64) {
    let secs = ms.div_euclid(1000) + i64::from(tz_min) * 60;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    (y, m, d, rem / 3600, (rem % 3600) / 60)
}

/// `4h 32m`, `1d 2h`, `12m`, or `now`.
pub fn countdown(until_ms: i64, now_ms: i64) -> String {
    let secs = (until_ms - now_ms) / 1000;
    if secs <= 0 {
        return "now".into();
    }
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3600;
    let m = (secs % 3600) / 60;
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m}m")
    }
}

/// `21:49` when the stamp falls on the local calendar day of `now_ms`,
/// else `Oct 9 05:59`.
pub fn clock(ms: i64, now_ms: i64, tz_min: i32) -> String {
    let (y, mo, d, h, mi) = local(ms, tz_min);
    let (ny, nmo, nd, _, _) = local(now_ms, tz_min);
    if (y, mo, d) == (ny, nmo, nd) {
        format!("{h:02}:{mi:02}")
    } else {
        format!("{} {d} {h:02}:{mi:02}", MONTHS[(mo - 1).clamp(0, 11) as usize])
    }
}

/// `3m ago`, `2h ago`, `just now`.
pub fn ago(then_ms: i64, now_ms: i64) -> String {
    let secs = ((now_ms - then_ms) / 1000).max(0);
    if secs < 45 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", (secs + 30) / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// `+0530` / `-0700` from `date +%z` to minutes east of UTC.
pub fn parse_tz(s: &str) -> i32 {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 5 {
        return 0;
    }
    let sign = if b[0] == b'-' { -1 } else { 1 };
    let h: i32 = s[1..3].parse().unwrap_or(0);
    let m: i32 = s[3..5].parse().unwrap_or(0);
    sign * (h * 60 + m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_round_trip() {
        let ms = parse_iso("2026-10-09T04:49:59.656212+00:00").unwrap();
        assert_eq!(ms, 1_791_521_399_656);
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("2026-10-09T04:49:59-07:00").unwrap(), parse_iso("2026-10-09T11:49:59Z").unwrap());
    }

    #[test]
    fn civil() {
        assert_eq!(civil_from_days(days_from_civil(2026, 10, 9)), (2026, 10, 9));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }
}
