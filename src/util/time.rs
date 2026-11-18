//! Clock helpers, and the one place zm-api's jiff types meet SeaORM's.
//!
//! zm-api's own code uses jiff. SeaORM and sqlx still decode date columns into
//! chrono types, so the entities and anything bound to a query carry
//! `sea_orm::prelude::DateTime` (a chrono `NaiveDateTime`) and `DateTimeUtc`
//! (a chrono `DateTime<Utc>`). The four converters below are the only code that
//! touches chrono's API. Convert as a value leaves an entity or enters a query,
//! and use jiff everywhere else.

use jiff::civil::DateTime as Civil;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp, Zoned};
use sea_orm::prelude::{DateTime as DbDateTime, DateTimeUtc as DbDateTimeUtc};

/// Seconds since the Unix epoch, for token claims and expiry checks.
pub fn unix_now() -> i64 {
    Timestamp::now().as_second()
}

/// The wall-clock time in UTC, with no offset attached.
pub fn utc_now() -> Civil {
    utc_civil(Timestamp::now())
}

/// The wall-clock time in the server's zone, as ZoneMinder's `DATETIME`
/// columns store it.
pub fn local_now() -> Civil {
    Zoned::now().datetime()
}

/// The current UTC wall-clock as a value to bind to a `DATETIME` column that
/// zm-api keeps in UTC (its own tables, not ZoneMinder's local-time ones).
pub fn db_utc_now() -> DbDateTime {
    db_datetime(utc_now())
}

/// The UTC wall-clock digits of an instant.
pub fn utc_civil(t: Timestamp) -> Civil {
    t.to_zoned(TimeZone::UTC).datetime()
}

/// The instant whose UTC wall-clock digits are `c`.
pub fn civil_utc(c: Civil) -> Timestamp {
    match c.to_zoned(TimeZone::UTC) {
        Ok(z) => z.timestamp(),
        // Only the last hours of year 9999 or the first of -9999 fail.
        Err(_) if c.year() < 0 => Timestamp::MIN,
        Err(_) => Timestamp::MAX,
    }
}

const EPOCH: Civil = Civil::constant(1970, 1, 1, 0, 0, 0, 0);

/// A `DATETIME` column value as a jiff civil datetime. The digits are unchanged.
///
/// This counts from the epoch in civil arithmetic and never goes through
/// [`Timestamp`], whose range stops short of the last day MySQL can store
/// (`9999-12-31`).
pub fn civil(column: DbDateTime) -> Civil {
    let utc = column.and_utc();
    let since_epoch = SignedDuration::new(utc.timestamp(), utc.timestamp_subsec_nanos() as i32);
    match EPOCH.checked_add(since_epoch) {
        Ok(c) => c,
        // chrono's range is wider than jiff's (years -9999 to 9999).
        Err(_) if since_epoch.is_negative() => Civil::MIN,
        Err(_) => Civil::MAX,
    }
}

/// The instant of a `DATETIME` column that holds UTC digits, as zm-api's own
/// tables do. ZoneMinder's columns hold local time: use
/// [`crate::util::datetime_to_utc`] for those.
pub fn utc_instant(column: DbDateTime) -> Timestamp {
    civil_utc(civil(column))
}

/// A jiff civil datetime as a value to bind to a `DATETIME` column.
pub fn db_datetime(c: Civil) -> DbDateTime {
    let since_epoch = c.duration_since(EPOCH);
    let nanos = since_epoch.as_nanos();
    let secs = nanos.div_euclid(1_000_000_000) as i64;
    let sub = nanos.rem_euclid(1_000_000_000) as u32;
    // jiff's range sits inside chrono's, so this always succeeds.
    DbDateTimeUtc::from_timestamp(secs, sub)
        .unwrap_or_default()
        .naive_utc()
}

/// A chrono UTC instant as a jiff timestamp. No zone arithmetic: see
/// [`crate::util::timestamp_to_utc`] for reading a `TIMESTAMP` column.
pub fn instant(t: DbDateTimeUtc) -> Timestamp {
    let secs = t.timestamp();
    match Timestamp::new(secs, t.timestamp_subsec_nanos() as i32) {
        Ok(ts) => ts,
        // chrono's range is wider than jiff's (years -9999 to 9999).
        Err(_) if secs < 0 => Timestamp::MIN,
        Err(_) => Timestamp::MAX,
    }
}

/// A jiff timestamp as a chrono UTC instant.
pub fn db_instant(t: Timestamp) -> DbDateTimeUtc {
    let nanos = t.as_nanosecond();
    let secs = nanos.div_euclid(1_000_000_000) as i64;
    let sub = nanos.rem_euclid(1_000_000_000) as u32;
    // jiff's range sits inside chrono's, so this always succeeds.
    DbDateTimeUtc::from_timestamp(secs, sub).unwrap_or_default()
}

/// The fraction chrono printed: none for whole seconds, otherwise 3, 6 or 9
/// digits. zm-api's JSON kept this shape when it moved to jiff, whose own
/// output trims trailing zeros instead.
fn fraction(nanos: i32) -> String {
    let nanos = nanos.unsigned_abs();
    if nanos == 0 {
        String::new()
    } else if nanos.is_multiple_of(1_000_000) {
        format!(".{:03}", nanos / 1_000_000)
    } else if nanos.is_multiple_of(1_000) {
        format!(".{:06}", nanos / 1_000)
    } else {
        format!(".{nanos:09}")
    }
}

/// `2026-10-23T21:30:15`, the digits of a civil datetime.
pub fn civil_string(c: Civil) -> String {
    format!(
        "{}{}",
        c.strftime("%Y-%m-%dT%H:%M:%S"),
        fraction(c.subsec_nanosecond())
    )
}

/// `2026-10-23T21:30:15Z`, an instant in RFC 3339 with the `Z` suffix.
pub fn rfc3339_z(t: Timestamp) -> String {
    format!("{}Z", civil_string(utc_civil(t)))
}

/// `2026-10-23T21:30:15+00:00`, an instant in RFC 3339 with a numeric offset.
pub fn rfc3339_offset(t: Timestamp) -> String {
    format!("{}+00:00", civil_string(utc_civil(t)))
}

/// `2026-10-23 21:30:15 UTC`, the spelling `GET /frames` has always used for
/// `time_stamp` (chrono's `Display`). Kept so existing clients still parse it.
pub fn utc_display(t: Timestamp) -> String {
    let c = utc_civil(t);
    format!(
        "{}{} UTC",
        c.strftime("%Y-%m-%d %H:%M:%S"),
        fraction(c.subsec_nanosecond())
    )
}

/// `serialize_with` for a field that must keep [`rfc3339_z`]'s spelling.
pub fn serialize_rfc3339_z<S: serde::Serializer>(t: &Timestamp, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&rfc3339_z(*t))
}

/// `serialize_with` for an optional instant, in [`rfc3339_z`]'s spelling.
pub fn serialize_option_rfc3339_z<S: serde::Serializer>(
    t: &Option<Timestamp>,
    s: S,
) -> Result<S::Ok, S::Error> {
    match t {
        Some(t) => s.serialize_str(&rfc3339_z(*t)),
        None => s.serialize_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_datetime_column_keeps_its_digits_both_ways() {
        let c: Civil = "2026-10-23T21:30:15.25".parse().unwrap();
        let column = db_datetime(c);
        assert_eq!(column.to_string(), "2026-10-23 21:30:15.250");
        assert_eq!(civil(column), c);
    }

    /// MySQL's whole `DATETIME` range, including the day jiff's `Timestamp`
    /// cannot reach, and a value before the epoch.
    #[test]
    fn a_datetime_column_keeps_its_digits_at_the_edges() {
        for (text, chrono_text) in [
            ("9999-12-31T12:00:00", "9999-12-31 12:00:00"),
            ("9999-12-31T23:59:59.999999", "9999-12-31 23:59:59.999999"),
            ("1000-01-01T00:00:00", "1000-01-01 00:00:00"),
            ("1969-12-31T23:59:59.5", "1969-12-31 23:59:59.500"),
        ] {
            let c: Civil = text.parse().unwrap();
            let column = db_datetime(c);
            assert_eq!(column.to_string(), chrono_text);
            assert_eq!(civil(column), c);
        }
    }

    #[test]
    fn an_instant_survives_the_round_trip() {
        let t: Timestamp = "2026-10-23T21:30:15.000000001Z".parse().unwrap();
        assert_eq!(instant(db_instant(t)), t);
    }

    #[test]
    fn an_instant_before_the_epoch_survives_the_round_trip() {
        let t: Timestamp = "1969-12-31T23:59:59.5Z".parse().unwrap();
        let column = db_instant(t);
        assert_eq!(column.timestamp(), -1);
        assert_eq!(column.timestamp_subsec_nanos(), 500_000_000);
        assert_eq!(instant(column), t);
    }

    #[test]
    fn civil_and_instant_agree_on_utc_digits() {
        let t: Timestamp = "2026-01-02T03:04:05Z".parse().unwrap();
        assert_eq!(utc_civil(t).to_string(), "2026-01-02T03:04:05");
        assert_eq!(civil_utc(utc_civil(t)), t);
    }

    #[test]
    fn strings_keep_the_fraction_widths_clients_already_see() {
        let whole: Timestamp = "2026-10-23T21:30:15Z".parse().unwrap();
        assert_eq!(rfc3339_z(whole), "2026-10-23T21:30:15Z");
        assert_eq!(rfc3339_offset(whole), "2026-10-23T21:30:15+00:00");
        let millis: Timestamp = "2026-10-23T21:30:15.5Z".parse().unwrap();
        assert_eq!(rfc3339_z(millis), "2026-10-23T21:30:15.500Z");
        let micros: Timestamp = "2026-10-23T21:30:15.000250Z".parse().unwrap();
        assert_eq!(rfc3339_z(micros), "2026-10-23T21:30:15.000250Z");
        let nanos: Timestamp = "2026-10-23T21:30:15.000000001Z".parse().unwrap();
        assert_eq!(rfc3339_offset(nanos), "2026-10-23T21:30:15.000000001+00:00");
        assert_eq!(utc_display(whole), "2026-10-23 21:30:15 UTC");
        assert_eq!(utc_display(millis), "2026-10-23 21:30:15.500 UTC");
        let c: Civil = "2025-04-24T12:34:56.120".parse().unwrap();
        assert_eq!(civil_string(c), "2025-04-24T12:34:56.120");
    }
}
