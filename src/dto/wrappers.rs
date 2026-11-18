use crate::entity::sea_orm_active_enums::Scheme;
use fake::Dummy;
use jiff::civil::DateTime as Civil;
use jiff::Timestamp;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Deserialize, ToSchema, Clone)]
#[schema(value_type = String, format = "date-time", example = "2025-04-24T12:34:56Z")]
pub struct DateTimeWrapper(pub Timestamp);

impl Serialize for DateTimeWrapper {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&crate::util::time::rfc3339_z(self.0))
    }
}

impl Dummy<fake::Faker> for DateTimeWrapper {
    fn dummy_with_rng<R: rand::RngExt + ?Sized>(_: &fake::Faker, rng: &mut R) -> Self {
        // Any second from 1970 to 2100.
        let secs = rng.random_range(0..4_102_444_800_i64);
        DateTimeWrapper(Timestamp::from_second(secs).unwrap_or(Timestamp::UNIX_EPOCH))
    }
}

/// A ZoneMinder `DATETIME` — wall-clock local time on the recording host, with
/// no timezone offset. The value is exactly the digits the column holds.
///
/// Deliberately **not** declared `format: date-time` (GH #32). That format
/// means RFC 3339, which requires an offset, so generators build an
/// offset-aware parser that then rejects every value this API actually sends.
/// A plain string is the honest description; interpret it in the server's
/// timezone, which `GET /api/v3/system/locale` reports.
#[derive(Debug, Deserialize, ToSchema, Clone)]
#[schema(
    value_type = String,
    example = "2025-04-24T12:34:56",
    description = "Local wall-clock time with no offset, as stored by ZoneMinder"
)]
pub struct NaiveDateTimeWrapper(pub Civil);

impl Serialize for NaiveDateTimeWrapper {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&crate::util::time::civil_string(self.0))
    }
}

// Add conversion implementations for NaiveDateTimeWrapper
impl From<NaiveDateTimeWrapper> for Civil {
    fn from(wrapper: NaiveDateTimeWrapper) -> Self {
        wrapper.0
    }
}

impl From<Civil> for NaiveDateTimeWrapper {
    fn from(dt: Civil) -> Self {
        NaiveDateTimeWrapper(dt)
    }
}

// Add reference conversion implementations
impl<'a> From<&'a NaiveDateTimeWrapper> for &'a Civil {
    fn from(wrapper: &'a NaiveDateTimeWrapper) -> Self {
        &wrapper.0
    }
}

// Helper function to convert Option<NaiveDateTimeWrapper> to Option<NaiveDateTime>
pub fn unwrap_naive_datetime_wrapper(opt_wrapper: &Option<NaiveDateTimeWrapper>) -> Option<Civil> {
    opt_wrapper.as_ref().map(|wrapper| wrapper.0)
}

#[derive(Debug, Serialize, Deserialize, ToSchema, Clone)]
#[schema(value_type = String, example = "123.45")]
pub struct DecimalWrapper(pub Decimal);

impl From<Decimal> for DecimalWrapper {
    fn from(d: Decimal) -> Self {
        DecimalWrapper(d)
    }
}

#[derive(Debug, Serialize, Deserialize, ToSchema, Clone)]
#[schema(value_type = String, example = "Deep")]
pub struct SchemeWrapper(pub Scheme);

impl From<Scheme> for SchemeWrapper {
    fn from(s: Scheme) -> Self {
        SchemeWrapper(s)
    }
}

impl From<Timestamp> for DateTimeWrapper {
    fn from(dt: Timestamp) -> Self {
        DateTimeWrapper(dt)
    }
}

/// Serde support for Option<NaiveDateTimeWrapper> using existing optional formatting
pub mod naive_datetime_option {
    use super::NaiveDateTimeWrapper;
    use crate::util::datetime_format::optional as base;
    use serde::{self, Deserializer, Serializer};

    pub fn serialize<S>(
        val: &Option<NaiveDateTimeWrapper>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Explicitly handle Option and serialize the inner datetime
        match val {
            Some(wrapper) => base::serialize(&Some(wrapper.0), serializer),
            None => base::serialize::<S>(&None, serializer), // Specify type for None case
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<NaiveDateTimeWrapper>, D::Error>
    where
        D: Deserializer<'de>,
    {
        base::deserialize(deserializer).map(|opt| opt.map(NaiveDateTimeWrapper))
    }
}
