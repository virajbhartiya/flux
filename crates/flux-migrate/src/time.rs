//! Timestamps: GitHub's RFC 3339 and HTTP dates ↔ unix milliseconds.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let (m, d) = (i64::from(m), i64::from(d));
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn ymd_hms_ms(y: i64, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> Option<i64> {
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86_400_000 + i64::from(h * 3600 + mi * 60 + s) * 1000)
}

/// `2024-01-02T03:04:05Z`, with optional fractional seconds and a numeric
/// offset, to unix milliseconds.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, rest) = s.split_once(['T', 't', ' '])?;
    let mut dp = date.splitn(3, '-');
    let y: i64 = dp.next()?.parse().ok()?;
    let mo: u32 = dp.next()?.parse().ok()?;
    let d: u32 = dp.next()?.parse().ok()?;
    let (time, offset_min) = match rest.strip_suffix(['Z', 'z']) {
        Some(t) => (t, 0i64),
        None => {
            let i = rest.rfind(['+', '-'])?;
            let (t, off) = rest.split_at(i);
            let sign = if off.starts_with('-') { -1 } else { 1 };
            let (oh, om) = off[1..].split_once(':')?;
            (t, sign * (oh.parse::<i64>().ok()? * 60 + om.parse::<i64>().ok()?))
        }
    };
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut tp = hms.splitn(3, ':');
    let h: u32 = tp.next()?.parse().ok()?;
    let mi: u32 = tp.next()?.parse().ok()?;
    let sec: u32 = tp.next()?.parse().ok()?;
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let ms: i64 = format!("{frac:0<3}")[..3].parse().ok()?;
    Some(ymd_hms_ms(y, mo, d, h, mi, sec)? + ms - offset_min * 60_000)
}

/// Unix milliseconds to `YYYY-MM-DDTHH:MM:SSZ` (what GitHub's `since=` takes).
pub fn format_rfc3339(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", t / 3600, t / 60 % 60, t % 60)
}

/// An HTTP `Date` header (`Sun, 06 Nov 1994 08:49:37 GMT`) to unix ms.
pub fn parse_http_date(s: &str) -> Option<i64> {
    let mut it = s.split_whitespace().skip(1);
    let d: u32 = it.next()?.parse().ok()?;
    let mon = it.next()?;
    let mo = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]
        .iter()
        .position(|m| m.eq_ignore_ascii_case(mon))? as u32
        + 1;
    let y: i64 = it.next()?.parse().ok()?;
    let mut tp = it.next()?.splitn(3, ':');
    let h: u32 = tp.next()?.parse().ok()?;
    let mi: u32 = tp.next()?.parse().ok()?;
    let sec: u32 = tp.next()?.parse().ok()?;
    ymd_hms_ms(y, mo, d, h, mi, sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_to_unix_ms() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2011-04-10T20:09:31Z"), Some(1_302_466_171_000));
        assert_eq!(parse_rfc3339("2011-04-10T20:09:31.25Z"), Some(1_302_466_171_250));
        assert_eq!(parse_rfc3339("2011-04-10T22:09:31+02:00"), Some(1_302_466_171_000));
        assert_eq!(parse_rfc3339("2011-04-10T13:09:31-07:00"), Some(1_302_466_171_000));
        assert_eq!(parse_rfc3339("2024-02-29T12:00:00Z"), Some(1_709_208_000_000));
        assert_eq!(parse_rfc3339("1969-12-31T23:59:59Z"), Some(-1000));
        assert_eq!(parse_rfc3339("2024-13-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339("yesterday"), None);
        assert_eq!(parse_rfc3339(""), None);
    }

    #[test]
    fn format_round_trips() {
        for ms in [0, 1_302_466_171_000, 1_709_208_000_000, 4_102_444_800_000, -86_400_000] {
            assert_eq!(parse_rfc3339(&format_rfc3339(ms)), Some(ms), "{}", format_rfc3339(ms));
        }
        assert_eq!(format_rfc3339(1_302_466_171_999), "2011-04-10T20:09:31Z");
    }

    #[test]
    fn http_dates() {
        assert_eq!(parse_http_date("Sun, 10 Apr 2011 20:09:31 GMT"), Some(1_302_466_171_000));
        assert_eq!(parse_http_date("garbage"), None);
    }
}
