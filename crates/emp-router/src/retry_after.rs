//! Normalize upstream retry advice before it affects routing or public headers.

use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};

pub(crate) fn parse(value: Option<&str>) -> Option<u64> {
    parse_at(value, OffsetDateTime::now_utc())
}

fn parse_at(value: Option<&str>, now: OffsetDateTime) -> Option<u64> {
    let value = value
        .filter(|value| !value.is_empty() && value.len() <= 128)?
        .trim();
    if value.is_empty() || value.bytes().any(|byte| byte.is_ascii_control()) {
        return None;
    }
    let delay = value.parse::<f64>().ok().or_else(|| {
        let date = OffsetDateTime::parse(value, &time::format_description::well_known::Rfc2822)
            .ok()
            .or_else(|| parse_rfc850(value))?;
        Some(((date.unix_timestamp_nanos() - now.unix_timestamp_nanos()) as f64 / 1e9).max(0.0))
    })?;
    (delay.is_finite() && delay >= 0.0 && delay <= u64::MAX as f64).then(|| delay.ceil() as u64)
}

// Python's email.utils.parsedate_to_datetime also accepts the obsolete
// RFC 850 HTTP-date form. It is still sent by some gateways.
fn parse_rfc850(value: &str) -> Option<OffsetDateTime> {
    let (weekday, rest) = value.split_once(", ")?;
    if !matches!(
        weekday,
        "Monday" | "Tuesday" | "Wednesday" | "Thursday" | "Friday" | "Saturday" | "Sunday"
    ) {
        return None;
    }
    let (date, rest) = rest.split_once(' ')?;
    let (clock, zone) = rest.split_once(' ')?;
    if !matches!(zone, "GMT" | "UTC") {
        return None;
    }
    let mut date = date.split('-');
    let day = date.next()?.parse::<u8>().ok()?;
    let month = match date.next()? {
        "Jan" => Month::January,
        "Feb" => Month::February,
        "Mar" => Month::March,
        "Apr" => Month::April,
        "May" => Month::May,
        "Jun" => Month::June,
        "Jul" => Month::July,
        "Aug" => Month::August,
        "Sep" => Month::September,
        "Oct" => Month::October,
        "Nov" => Month::November,
        "Dec" => Month::December,
        _ => return None,
    };
    let year = date.next()?.parse::<u8>().ok()?;
    if date.next().is_some() {
        return None;
    }
    let year = if year >= 69 { 1900 } else { 2000 } + i32::from(year);
    let mut clock = clock.split(':');
    let hour = clock.next()?.parse::<u8>().ok()?;
    let minute = clock.next()?.parse::<u8>().ok()?;
    let second = clock.next()?.parse::<u8>().ok()?;
    if clock.next().is_some() {
        return None;
    }
    Some(
        PrimitiveDateTime::new(
            Date::from_calendar_date(year, month, day).ok()?,
            Time::from_hms(hour, minute, second).ok()?,
        )
        .assume_utc(),
    )
}

#[cfg(test)]
mod tests {
    use super::parse_at;
    use time::OffsetDateTime;

    #[test]
    fn python_retry_after_contract() {
        let now = OffsetDateTime::from_unix_timestamp(0).unwrap();
        assert_eq!(
            parse_at(Some("Thu, 01 Jan 1970 00:00:30 GMT"), now),
            Some(30)
        );
        assert_eq!(
            parse_at(Some("Thursday, 01-Jan-70 00:00:30 GMT"), now),
            Some(30)
        );
        assert_eq!(
            parse_at(Some("Thu, 01 Jan 1970 00:00:30 +0000"), now),
            Some(30)
        );
        assert_eq!(
            parse_at(Some("Thu, 01 Jan 1969 00:00:30 GMT"), now),
            Some(0)
        );
        assert_eq!(parse_at(Some("2.5"), now), Some(3));
        assert_eq!(parse_at(Some("1e2"), now), Some(100));
        for value in ["", "-1", "NaN", "inf", "secret-value", "3\r\nX-Test: value"] {
            assert_eq!(parse_at(Some(value), now), None, "{value:?}");
        }
    }
}
