pub mod assertion;
pub mod authz;
pub mod claim;
pub mod dir;
pub mod file;
pub mod hash;
pub mod key;
pub mod media_url;
pub mod middleware;
pub mod password;
pub mod path;
pub mod random;
pub mod rate_limit;
pub mod regex;
pub mod result;
pub mod retry;
pub mod revocation;
pub mod task;
pub mod ws;

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
pub fn naive_local_to_utc(ndt: chrono::NaiveDateTime) -> chrono::DateTime<chrono::Utc> {
    use chrono::{Local, LocalResult, TimeZone, Utc};
    match Local.from_local_datetime(&ndt) {
        LocalResult::Single(dt) | LocalResult::Ambiguous(dt, _) => dt.with_timezone(&Utc),
        LocalResult::None => chrono::DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc),
    }
}

/// The server-local wall-clock time of a UTC instant: the inverse of
/// [`naive_local_to_utc`], for values compared against or written to
/// ZoneMinder's local-time columns (`Events.StartDateTime` and the like).
/// UTC to local is never ambiguous, so this needs no DST handling (#173).
pub fn utc_to_naive_local(dt: chrono::DateTime<chrono::Utc>) -> chrono::NaiveDateTime {
    dt.with_timezone(&chrono::Local).naive_local()
}

/// The true instant of a ZoneMinder `TIMESTAMP` column (`Monitor_Status.UpdatedOn`,
/// `Frames.TimeStamp`, `Tags.CreateDate` and the like).
///
/// The session runs in the server's zone (`timezone=SYSTEM`, #98), so MariaDB
/// returns a TIMESTAMP as local wall-clock. sqlx only decodes TIMESTAMP into
/// `DateTime<Utc>` and labels those digits UTC, which put every value off by
/// the UTC offset (zm-web showed every monitor Offline: its status row looked
/// hours old, #216). This reads the digits as the local time they are.
pub fn timestamp_to_utc(column: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::Utc> {
    naive_local_to_utc(column.naive_utc())
}

/// The value to bind to a ZoneMinder `TIMESTAMP` column for a true instant:
/// the inverse of [`timestamp_to_utc`]. MariaDB reads bound values in the
/// session zone, so it must carry the local wall-clock.
pub fn utc_to_timestamp(dt: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::Utc> {
    utc_to_naive_local(dt).and_utc()
}

#[cfg(test)]
mod timestamp_column_tests {
    use super::{timestamp_to_utc, utc_to_timestamp};

    #[test]
    fn a_written_instant_reads_back_unchanged() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-23T21:30:15Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(timestamp_to_utc(utc_to_timestamp(now)), now);
    }

    /// The column carries local digits: what `NOW()` stores in a SYSTEM session.
    #[test]
    fn the_column_holds_local_wall_clock() {
        let now = chrono::Utc::now();
        assert_eq!(
            utc_to_timestamp(now).naive_utc(),
            now.with_timezone(&chrono::Local).naive_local()
        );
    }
}

#[cfg(test)]
mod naive_local_to_utc_tests {
    use super::{naive_local_to_utc, utc_to_naive_local};

    /// A client's UTC instant reaches the column as the wall-clock the column
    /// stores, whatever the server's zone.
    #[test]
    fn utc_to_local_inverts_local_to_utc() {
        let ndt = chrono::NaiveDate::from_ymd_opt(2026, 10, 18)
            .unwrap()
            .and_hms_opt(11, 21, 23)
            .unwrap();
        assert_eq!(utc_to_naive_local(naive_local_to_utc(ndt)), ndt);
    }
    use chrono::{Local, NaiveDate, TimeZone};

    /// TZ-independent: interpreting the result back in the local zone must yield
    /// the original naive wall-clock. (Regression for the old code that stamped
    /// server-local time as UTC, shifting it by the offset.)
    #[test]
    fn round_trips_through_local() {
        let ndt = NaiveDate::from_ymd_opt(2026, 8, 21)
            .unwrap()
            .and_hms_opt(0, 40, 11)
            .unwrap();
        let utc = naive_local_to_utc(ndt);
        // Convert back to local and compare the naive wall-clock.
        assert_eq!(utc.with_timezone(&Local).naive_local(), ndt);
        // And the absolute instant matches what Local says that wall-clock is
        // (single, unambiguous for this date in any fixed zone).
        if let chrono::LocalResult::Single(expected) = Local.from_local_datetime(&ndt) {
            assert_eq!(utc, expected.with_timezone(&chrono::Utc));
        }
    }
}

pub mod datetime_format {
    use chrono::NaiveDateTime;
    use serde::{self, Deserialize, Deserializer, Serializer};

    // Format for serializing/deserializing NaiveDateTime
    const FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.f";

    // For regular NaiveDateTime fields
    pub fn serialize<S>(date: &NaiveDateTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let s = format!("{}", date.format(FORMAT));
        serializer.serialize_str(&s)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<NaiveDateTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        NaiveDateTime::parse_from_str(&s, FORMAT).map_err(serde::de::Error::custom)
    }

    // For Option<NaiveDateTime> fields
    pub mod optional {
        use super::*;

        pub fn serialize<S>(date: &Option<NaiveDateTime>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            match date {
                Some(date) => super::serialize(date, serializer),
                None => serializer.serialize_none(),
            }
        }

        pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<NaiveDateTime>, D::Error>
        where
            D: Deserializer<'de>,
        {
            Option::<String>::deserialize(deserializer)?
                .map(|s| {
                    NaiveDateTime::parse_from_str(&s, FORMAT).map_err(serde::de::Error::custom)
                })
                .transpose()
        }
    }
}
