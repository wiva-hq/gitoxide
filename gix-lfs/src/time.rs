//! Parsing of the few time formats the LFS API uses.
use std::time::{Duration, SystemTime};

/// Parse an RFC 3339 timestamp like `2016-11-10T15:29:07Z`, as used in `expires_at`.
pub(crate) fn parse_rfc3339(input: &str) -> Option<SystemTime> {
    let bytes = input.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || !matches!(bytes[10], b'T' | b't' | b' ') {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let year = number(&bytes[0..4])?;
    let month = number(&bytes[5..7])?;
    let day = number(&bytes[8..10])?;
    let hour = number(&bytes[11..13])?;
    let minute = number(&bytes[14..16])?;
    let second = number(&bytes[17..19])?;
    let mut rest = &bytes[19..];
    if let Some(fraction) = rest.strip_prefix(b".") {
        let digits = fraction.iter().take_while(|b| b.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        rest = &fraction[digits..];
    }
    let offset_seconds = match rest {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] => {
            let offset = number(&[*h1, *h2])? * 3600 + number(&[*m1, *m2])? * 60;
            if *sign == b'+' { offset } else { -offset }
        }
        _ => return None,
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset_seconds;
    from_unix_seconds(seconds)
}

/// The time at which an action or authentication expires, preferring `expires_in` seconds from `now` over `expires_at`.
pub(crate) fn expiry(expires_in: Option<i64>, expires_at: Option<&str>, now: SystemTime) -> Option<SystemTime> {
    match expires_in.filter(|seconds| *seconds != 0) {
        Some(seconds) => match u64::try_from(seconds) {
            Ok(seconds) => now.checked_add(Duration::from_secs(seconds)),
            Err(_) => Some(now),
        },
        None => expires_at.and_then(parse_rfc3339),
    }
}

/// Parse the value of a `Retry-After` header as delay from `now`, given either in seconds or as HTTP date.
pub(crate) fn parse_retry_after(input: &str, now: SystemTime) -> Option<Duration> {
    let input = input.trim();
    if let Ok(seconds) = input.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let at = parse_http_date(input)?;
    Some(at.duration_since(now).unwrap_or_default())
}

/// Parse a date like `Sun, 06 Nov 1994 08:49:37 GMT`, the preferred format of HTTP.
fn parse_http_date(input: &str) -> Option<SystemTime> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut parts = input.split(' ');
    let _weekday = parts.next()?;
    let day = number(parts.next()?.as_bytes())?;
    let month_name = parts.next()?;
    let month = MONTHS.iter().position(|month| *month == month_name)?;
    let year = number(parts.next()?.as_bytes())?;
    let mut time = parts.next()?.split(':');
    let hour = number(time.next()?.as_bytes())?;
    let minute = number(time.next()?.as_bytes())?;
    let second = number(time.next()?.as_bytes())?;
    if parts.next()? != "GMT" || parts.next().is_some() {
        return None;
    }
    let seconds = days_from_civil(year, month as i64 + 1, day) * 86_400 + hour * 3600 + minute * 60 + second;
    from_unix_seconds(seconds)
}

fn number(digits: &[u8]) -> Option<i64> {
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// Times before the epoch are treated as absent, as servers send `0001-01-01T00:00:00Z` to mean "never".
fn from_unix_seconds(seconds: i64) -> Option<SystemTime> {
    let seconds = u64::try_from(seconds).ok()?;
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

/// The number of days since 1970-01-01 of the given proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::{parse_retry_after, parse_rfc3339};

    #[test]
    fn rfc3339() {
        let at = |seconds: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(at(0)), "the epoch");
        assert_eq!(
            parse_rfc3339("2016-11-10T15:29:07Z"),
            Some(at(1_478_791_747)),
            "the example of the batch API documentation"
        );
        assert_eq!(
            parse_rfc3339("2016-11-10T16:29:07.123+01:00"),
            Some(at(1_478_791_747)),
            "offsets are subtracted and fractions ignored"
        );
        assert_eq!(parse_rfc3339("2016-11-10 15:29:07"), None, "a zone is required");
        assert_eq!(parse_rfc3339("2016-13-10T15:29:07Z"), None, "months are validated");
        assert_eq!(
            parse_rfc3339("0001-01-01T00:00:00Z"),
            None,
            "the zero time of Go means no expiry"
        );
    }

    #[test]
    fn retry_after() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(784_111_770);
        assert_eq!(parse_retry_after("120", now), Some(Duration::from_secs(120)), "seconds");
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", now),
            Some(Duration::from_secs(7)),
            "an HTTP date is relative to now"
        );
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:00 GMT", now),
            Some(Duration::ZERO),
            "dates in the past mean no delay"
        );
        assert_eq!(parse_retry_after("soon", now), None, "anything else is ignored");
    }
}
