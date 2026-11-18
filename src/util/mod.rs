pub mod assertion;
pub mod authz;
pub mod claim;
pub mod dir;
pub mod file;
pub mod hash;
pub mod key;
pub mod media_url;
pub mod middleware;
pub mod net_guard;
pub mod password;
pub mod path;
pub mod random;
pub mod rate_limit;
pub mod regex;
pub mod result;
pub mod retry;
pub mod revocation;
pub mod task;
pub mod time;
pub mod ws;

use jiff::civil::DateTime as Civil;
use jiff::Timestamp;

/// Convert a ZoneMinder `DATETIME` value to true UTC.
///
/// ZoneMinder stores `DATETIME` columns in the database in the **server's local
/// time** (naive, no zone). Emitting them with a `Z` suffix as if they were UTC
/// (the old behaviour) put every timestamp off by the server's UTC offset. This
/// interprets the naive value in the process's local zone — which on a normal
/// single-box deployment is the same zone the MySQL server used to write it —
/// and converts to real UTC.
///
/// DST edge cases: an ambiguous local time (the "fall back" hour) takes the
/// earlier instant; a non-existent local time (the "spring forward" gap) falls
/// back to treating the value as already-UTC rather than failing.
pub fn naive_local_to_utc(local: Civil) -> Timestamp {
    use jiff::tz::{AmbiguousOffset, TimeZone};
    let ambiguous = TimeZone::system().into_ambiguous_zoned(local);
    match ambiguous.offset() {
        AmbiguousOffset::Gap { .. } => time::civil_utc(local),
        _ => match ambiguous.earlier() {
            Ok(zoned) => zoned.timestamp(),
            Err(_) => time::civil_utc(local),
        },
    }
}

/// The server-local wall-clock time of a UTC instant: the inverse of
/// [`naive_local_to_utc`], for values compared against or written to
/// ZoneMinder's local-time columns (`Events.StartDateTime` and the like).
/// UTC to local is never ambiguous, so this needs no DST handling (#173).
pub fn utc_to_naive_local(t: Timestamp) -> Civil {
    t.to_zoned(jiff::tz::TimeZone::system()).datetime()
}

/// The true instant of a ZoneMinder `DATETIME` column, read straight off an
/// entity: [`naive_local_to_utc`] for a value that is still SeaORM's type.
pub fn datetime_to_utc(column: sea_orm::prelude::DateTime) -> Timestamp {
    naive_local_to_utc(time::civil(column))
}

/// The value to bind to a ZoneMinder `DATETIME` column for a true instant: the
/// inverse of [`datetime_to_utc`].
pub fn utc_to_datetime(t: Timestamp) -> sea_orm::prelude::DateTime {
    time::db_datetime(utc_to_naive_local(t))
}

/// The true instant of a ZoneMinder `TIMESTAMP` column (`Monitor_Status.UpdatedOn`,
/// `Frames.TimeStamp`, `Tags.CreateDate` and the like).
///
/// The session runs in the server's zone (`timezone=SYSTEM`, #98), so MariaDB
/// returns a TIMESTAMP as local wall-clock. sqlx only decodes TIMESTAMP into
/// `DateTime<Utc>` and labels those digits UTC, which put every value off by
/// the UTC offset (zm-web showed every monitor Offline: its status row looked
/// hours old, #216). This reads the digits as the local time they are.
pub fn timestamp_to_utc(column: sea_orm::prelude::DateTimeUtc) -> Timestamp {
    naive_local_to_utc(time::utc_civil(time::instant(column)))
}

/// The value to bind to a ZoneMinder `TIMESTAMP` column for a true instant:
/// the inverse of [`timestamp_to_utc`]. MariaDB reads bound values in the
/// session zone, so it must carry the local wall-clock.
pub fn utc_to_timestamp(t: Timestamp) -> sea_orm::prelude::DateTimeUtc {
    time::db_instant(time::civil_utc(utc_to_naive_local(t)))
}

#[cfg(test)]
mod timestamp_column_tests {
    use super::{time, timestamp_to_utc, utc_to_timestamp};
    use jiff::Timestamp;

    #[test]
    fn a_written_instant_reads_back_unchanged() {
        let now: Timestamp = "2026-10-23T21:30:15Z".parse().unwrap();
        assert_eq!(timestamp_to_utc(utc_to_timestamp(now)), now);
    }

    /// The column carries local digits: what `NOW()` stores in a SYSTEM session.
    #[test]
    fn the_column_holds_local_wall_clock() {
        let now = Timestamp::now();
        assert_eq!(
            time::utc_civil(time::instant(utc_to_timestamp(now))),
            now.to_zoned(jiff::tz::TimeZone::system()).datetime()
        );
    }
}

#[cfg(test)]
mod naive_local_to_utc_tests {
    use super::{naive_local_to_utc, utc_to_naive_local};
    use jiff::civil::date;
    use jiff::tz::TimeZone;

    /// A client's UTC instant reaches the column as the wall-clock the column
    /// stores, whatever the server's zone.
    #[test]
    fn utc_to_local_inverts_local_to_utc() {
        let local = date(2026, 10, 18).at(11, 21, 23, 0);
        assert_eq!(utc_to_naive_local(naive_local_to_utc(local)), local);
    }

    /// TZ-independent: interpreting the result back in the local zone must yield
    /// the original naive wall-clock. (Regression for the old code that stamped
    /// server-local time as UTC, shifting it by the offset.)
    #[test]
    fn round_trips_through_local() {
        let local = date(2026, 8, 21).at(0, 40, 11, 0);
        let utc = naive_local_to_utc(local);
        // Convert back to local and compare the naive wall-clock.
        assert_eq!(utc.to_zoned(TimeZone::system()).datetime(), local);
        // And the absolute instant matches what the zone says that wall-clock
        // is (single, unambiguous for this date in any fixed zone).
        let ambiguous = TimeZone::system().into_ambiguous_zoned(local);
        if !ambiguous.is_ambiguous() {
            assert_eq!(utc, ambiguous.compatible().unwrap().timestamp());
        }
    }

    /// A wall-clock that the zone skips is read as UTC digits, and one that
    /// occurs twice takes the earlier instant.
    #[test]
    fn dst_edges_resolve_the_documented_way() {
        use jiff::tz::AmbiguousOffset;
        let tz = TimeZone::get("America/New_York").unwrap();
        let gap = date(2026, 3, 8).at(2, 30, 0, 0);
        assert!(matches!(
            tz.to_ambiguous_zoned(gap).offset(),
            AmbiguousOffset::Gap { .. }
        ));
        let fold = date(2026, 11, 1).at(1, 30, 0, 0);
        let earlier = tz.to_ambiguous_zoned(fold).earlier().unwrap();
        assert_eq!(earlier.offset().seconds(), -4 * 3600);
    }
}

pub mod datetime_format {
    use jiff::civil::DateTime as Civil;
    use serde::{self, Deserialize, Deserializer, Serializer};

    // Format for serializing/deserializing a civil datetime
    const FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.f";

    fn parse<E: serde::de::Error>(s: &str) -> Result<Civil, E> {
        Civil::strptime(FORMAT, s).map_err(serde::de::Error::custom)
    }

    // For regular civil datetime fields
    pub fn serialize<S>(date: &Civil, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&super::time::civil_string(*date))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Civil, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        parse(&s)
    }

    // For Option<Civil> fields
    pub mod optional {
        use super::*;

        pub fn serialize<S>(date: &Option<Civil>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            match date {
                Some(date) => super::serialize(date, serializer),
                None => serializer.serialize_none(),
            }
        }

        pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Civil>, D::Error>
        where
            D: Deserializer<'de>,
        {
            Option::<String>::deserialize(deserializer)?
                .map(|s| parse(&s))
                .transpose()
        }
    }
}
