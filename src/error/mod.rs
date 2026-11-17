use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde::Serialize;
use strum::EnumString;
use utoipa::ToSchema;

use crate::entity;

pub type AppResult<T = ()> = std::result::Result<T, AppError>;

#[derive(Debug, thiserror::Error, ToSchema)]
pub enum AppError {
    #[error("{0} not found")]
    NotFoundError(Resource),
    #[error("{0} not available")]
    NotAvailableError(Resource),
    #[error("{0} already exists")]
    ResourceExistsError(Resource),
    #[error("{0}")]
    PermissionDeniedError(String),
    #[error("{0}")]
    UserNotActiveError(String),
    #[error("{0}")]
    InvalidSessionError(String),
    #[error("{0}")]
    ConflictError(String),
    #[error("{0}")]
    UnauthorizedError(String),
    #[error("bad request {0}")]
    BadRequestError(String),
    #[error("{0}")]
    InvalidPayloadError(String),
    /// A pipeline graph failed validation; each detail is `(path, message)`.
    #[error("{message}")]
    InvalidPipelineError {
        message: String,
        errors: Vec<(String, String)>,
    },
    #[error("{0}")]
    HashError(String),
    #[error("internal server error: {0}")]
    InternalServerError(String),
    #[error("service unavailable: {0}")]
    ServiceUnavailableError(String),
    /// A 503 for something that will be back shortly, with a Retry-After of
    /// this many seconds.
    #[error("service unavailable: {0}")]
    RetryLaterError(String, u64),

    #[error(transparent)]
    #[schema(value_type = String, example = "Validation failed")]
    InvalidInputError(#[from] garde::Report),

    #[error(transparent)]
    #[schema(value_type = String, example = "Database error")]
    DatabaseError(#[from] sea_orm::error::DbErr),

    #[error(transparent)]
    #[schema(value_type = String, example = "WebSocket error")]
    WebSocketError(#[from] tokio_tungstenite::tungstenite::Error),

    #[error(transparent)]
    #[schema(value_type = String, example = "IO error")]
    IoError(#[from] std::io::Error),

    #[error(transparent)]
    #[schema(value_type = String, example = "UUID error")]
    UuidError(#[from] uuid::Error),

    #[error(transparent)]
    #[schema(value_type = String, example = "JWT error")]
    JwtError(#[from] jsonwebtoken::errors::Error),

    #[error(transparent)]
    #[schema(value_type = String, example = "HTTP client error")]
    HttpClientError(#[from] reqwest::Error),

    #[error(transparent)]
    #[schema(value_type = String, example = "Config error")]
    ConfigError(#[from] config::ConfigError),

    #[error(transparent)]
    #[schema(value_type = String, example = "JSON parse error")]
    ParseJsonError(#[from] serde_json::Error),

    #[error(transparent)]
    #[schema(value_type = String, example = "Float parse error")]
    ParseFloatError(#[from] std::num::ParseFloatError),

    #[error(transparent)]
    #[schema(value_type = String, example = "Address parse error")]
    AddrParseError(#[from] std::net::AddrParseError),

    #[error(transparent)]
    #[schema(value_type = String, example = "Task join error")]
    SpawnTaskError(#[from] tokio::task::JoinError),

    #[error(transparent)]
    #[schema(value_type = String, example = "Base64 decode error")]
    Base64Error(#[from] base64::DecodeError),

    #[error(transparent)]
    #[schema(value_type = String, example = "Enum parse error")]
    StrumParseError(#[from] strum::ParseError),

    #[error(transparent)]
    #[schema(value_type = String, example = "System time error")]
    SystemTimeError(#[from] std::time::SystemTimeError),

    #[error(transparent)]
    #[schema(value_type = String, example = "Axum error")]
    AxumError(#[from] axum::Error),

    #[error(transparent)]
    #[schema(value_type = String, example = "Unknown error")]
    UnknownError(#[from] anyhow::Error),

    #[error(transparent)]
    #[schema(value_type = String, example = "Infallible error")]
    Infallible(#[from] std::convert::Infallible),

    #[error(transparent)]
    #[schema(value_type = String, example = "Typed header error")]
    TypeHeaderError(#[from] axum_extra::typed_header::TypedHeaderRejection),
}

impl From<argon2::password_hash::Error> for AppError {
    fn from(value: argon2::password_hash::Error) -> Self {
        AppError::HashError(value.to_string())
    }
}

impl From<bcrypt::BcryptError> for AppError {
    fn from(value: bcrypt::BcryptError) -> Self {
        AppError::HashError(value.to_string())
    }
}

/// SQLSTATE for "string data, right truncated" — MySQL error 1406,
/// `Data too long for column 'X' at row N`.
const SQLSTATE_STRING_DATA_TRUNCATED: &str = "22001";

/// The column named by a "data too long" database error, if that is what this
/// is.
///
/// Exists because roughly forty request fields write to fixed-width columns
/// with no length validation of their own, and every one of them turns an
/// over-long value into a 500 that tells the caller nothing. Per-DTO rules are
/// still better — they reject before the round trip and can say what the limit
/// is — but this catches the ones nobody has got to yet, including columns
/// nobody has enumerated.
///
/// Only the column name is extracted. The driver's message can contain the
/// value and surrounding SQL, which is exactly what the redaction above exists
/// to keep out of a response.
fn value_too_long_column(err: &sea_orm::DbErr) -> Option<String> {
    let (sea_orm::DbErr::Query(sea_orm::RuntimeErr::SqlxError(sqlx_err))
    | sea_orm::DbErr::Exec(sea_orm::RuntimeErr::SqlxError(sqlx_err))) = err
    else {
        return None;
    };
    let db_err = sqlx_err.as_database_error()?;
    if db_err.code().as_deref() != Some(SQLSTATE_STRING_DATA_TRUNCATED) {
        return None;
    }
    parse_too_long_column(db_err.message())
}

/// Pull the column out of `Data too long for column 'Name' at row 1`.
///
/// Returns `None` rather than guessing if the message is not in that shape —
/// a wrong column name in an error is worse than none.
fn parse_too_long_column(message: &str) -> Option<String> {
    let rest = message.split("for column ").nth(1)?;
    let quoted = rest.strip_prefix('\'')?;
    let (column, _) = quoted.split_once('\'')?;
    (!column.is_empty()).then(|| column.to_string())
}

impl AppError {
    pub fn response(self) -> (StatusCode, AppResponseError) {
        use AppError::*;
        let mut message = self.to_string();
        // Raw SeaORM/sqlx error text can leak SQL fragments, table and column
        // names, and connection details. Log the detail server-side but return
        // a generic message to the client.
        // A value too long for its column is bad input, not a server fault, and
        // the driver names the offending column. Classify before the redaction
        // below blanks the text, and surface only the column name — never the
        // statement (GH #55).
        let mut too_long_column: Option<String> = None;
        if let DatabaseError(err) = &self {
            tracing::error!("database error: {err}");
            too_long_column = value_too_long_column(err);
            message = match &too_long_column {
                Some(column) => format!("value too long for '{column}'"),
                None => "A database error occurred".to_string(),
            };
        }
        let (kind, code, details, status_code) = match self {
            InvalidPayloadError(_err) => (
                "INVALID_PAYLOAD_ERROR".to_string(),
                None,
                vec![],
                StatusCode::BAD_REQUEST,
            ),
            InvalidPipelineError { errors, .. } => (
                "INVALID_PIPELINE_ERROR".to_string(),
                None,
                errors,
                StatusCode::BAD_REQUEST,
            ),
            BadRequestError(_err) => (
                "BAD_REQUEST_ERROR".to_string(),
                None,
                vec![],
                StatusCode::BAD_REQUEST,
            ),
            InternalServerError(_err) => (
                "INTERNAL_SERVER_ERROR".to_string(),
                None,
                vec![],
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            ServiceUnavailableError(_err) => (
                "SERVICE_UNAVAILABLE_ERROR".to_string(),
                None,
                vec![],
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            RetryLaterError(_err, secs) => (
                "SERVICE_UNAVAILABLE_ERROR".to_string(),
                None,
                vec![("retry_after_secs".to_string(), secs.to_string())],
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            NotAvailableError(resource) => (
                format!("{resource}_NOT_AVAILABLE_ERROR"),
                None,
                vec![],
                StatusCode::NOT_FOUND,
            ),
            NotFoundError(resource) => (
                format!("{resource}_NOT_FOUND_ERROR"),
                Some(resource.resource_type as i32),
                resource.details.clone(),
                StatusCode::NOT_FOUND,
            ),
            ResourceExistsError(resource) => (
                format!("{resource}_ALREADY_EXISTS_ERROR"),
                Some(resource.resource_type as i32),
                resource.details.clone(),
                StatusCode::CONFLICT,
            ),
            AxumError(_err) => (
                "AXUM_ERROR".to_string(),
                None,
                vec![],
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            ConfigError(_err) => (
                "CONFIG_ERROR".to_string(),
                None,
                vec![],
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            AddrParseError(_err) => (
                "ADDR_PARSE_ERROR".to_string(),
                None,
                vec![],
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            IoError(err) => {
                let (status, kind, code) = match err.kind() {
                    std::io::ErrorKind::NotFound => (
                        StatusCode::NOT_FOUND,
                        format!("{}_NOT_FOUND_ERROR", ResourceType::File),
                        Some(ResourceType::File as i32),
                    ),
                    std::io::ErrorKind::PermissionDenied => {
                        (StatusCode::FORBIDDEN, "FORBIDDEN_ERROR".to_string(), None)
                    }
                    _ => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "IO_ERROR".to_string(),
                        None,
                    ),
                };
                (kind, code, vec![], status)
            }
            WebSocketError(_err) => (
                "WEBSOCKET_ERROR".to_string(),
                None,
                vec![],
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            ParseJsonError(_err) => (
                "PARSE_JSON_ERROR".to_string(),
                None,
                vec![],
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            StrumParseError(_err) => (
                "STRUM_PARSE_ERROR".to_string(),
                None,
                vec![],
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            HttpClientError(_err) => (
                "HTTP_CLIENT_ERROR".to_string(),
                None,
                vec![],
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            SystemTimeError(_err) => (
                "SYSTEM_TIME_ERROR".to_string(),
                None,
                vec![],
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            SpawnTaskError(_err) => (
                "SPAWN_TASK_ERROR".to_string(),
                None,
                vec![],
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            UnknownError(_err) => (
                "UNKNOWN_ERROR".to_string(),
                None,
                vec![],
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            PermissionDeniedError(_err) => (
                "PERMISSION_DENIED_ERROR".to_string(),
                None,
                vec![],
                StatusCode::FORBIDDEN,
            ),
            InvalidSessionError(_err) => (
                "INVALID_SESSION_ERROR".to_string(),
                None,
                vec![],
                // An invalid/expired session is an auth failure — clients must
                // re-authenticate, not treat it as a malformed request.
                StatusCode::UNAUTHORIZED,
            ),
            ConflictError(_err) => (
                "CONFLICT_ERROR".to_string(),
                None,
                vec![],
                // Semantic "already exists / duplicate" (e.g. a live session
                // already running), not a server fault.
                StatusCode::CONFLICT,
            ),
            UserNotActiveError(_err) => (
                "USER_NOT_ACTIVE_ERROR".to_string(),
                None,
                vec![],
                StatusCode::FORBIDDEN,
            ),
            UnauthorizedError(_err) => (
                "UNAUTHORIZED_ERROR".to_string(),
                None,
                vec![],
                StatusCode::UNAUTHORIZED,
            ),
            UuidError(_err) => (
                "UUID_ERROR".to_string(),
                None,
                vec![],
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            JwtError(_err) => (
                "UNAUTHORIZED_ERROR".to_string(),
                None,
                vec![],
                StatusCode::UNAUTHORIZED,
            ),
            HashError(_err) => (
                "HASH_ERROR".to_string(),
                None,
                vec![],
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            ParseFloatError(_err) => (
                "PARSE_FLOAT_ERROR".to_string(),
                None,
                vec![],
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            Base64Error(_err) => (
                "BASE64_ERROR".to_string(),
                None,
                vec![],
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            InvalidInputError(err) => (
                "INVALID_INPUT_ERROR".to_string(),
                None,
                err.iter()
                    .map(|(p, e)| (p.to_string(), e.to_string()))
                    .collect(),
                StatusCode::BAD_REQUEST,
            ),
            DatabaseError(_err) => match too_long_column {
                Some(column) => (
                    "VALUE_TOO_LONG".to_string(),
                    None,
                    vec![(column, "value is longer than the column allows".to_string())],
                    StatusCode::BAD_REQUEST,
                ),
                None => (
                    "DATABASE_ERROR".to_string(),
                    None,
                    vec![],
                    StatusCode::INTERNAL_SERVER_ERROR,
                ),
            },
            Infallible(_err) => (
                "INFALLIBLE".to_string(),
                None,
                vec![],
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            TypeHeaderError(_err) => (
                "TYPE_HEADER_ERROR".to_string(),
                None,
                vec![],
                // A malformed/typed-header rejection (e.g. a bad Authorization
                // header) is a client error, not a server fault.
                StatusCode::BAD_REQUEST,
            ),
        };

        (
            status_code,
            AppResponseError::new(kind, message, code, details),
        )
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let retry_after = match &self {
            AppError::RetryLaterError(_, secs) => Some(*secs),
            _ => None,
        };
        let (status_code, body) = self.response();
        let mut response = (status_code, Json(body)).into_response();
        if let Some(secs) = retry_after {
            response.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from(secs),
            );
        }
        response
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, utoipa::ToSchema)]
pub struct AppResponseError {
    pub kind: String,
    pub error_message: String,
    pub code: Option<i32>,
    pub details: Vec<(String, String)>,
}

impl AppResponseError {
    pub fn new(
        kind: impl Into<String>,
        message: impl Into<String>,
        code: Option<i32>,
        details: Vec<(String, String)>,
    ) -> Self {
        Self {
            kind: kind.into(),
            error_message: message.into(),
            code,
            details,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, ToSchema)]
pub struct Resource {
    pub details: Vec<(String, String)>,
    pub resource_type: ResourceType,
}

impl std::fmt::Display for Resource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // TODO
        self.resource_type.fmt(f)
    }
}

#[derive(
    Debug, EnumString, strum::Display, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ToSchema,
)]
pub enum ResourceType {
    #[strum(serialize = "USER")]
    User,
    #[strum(serialize = "FILE")]
    File,
    #[strum(serialize = "SESSION")]
    Session,
    #[strum(serialize = "MESSAGE")]
    Message,
    #[strum(serialize = "MONITOR")]
    Monitor,
    #[strum(serialize = "CONFIG")]
    Config,
    #[strum(serialize = "EVENT_TAG")]
    EventTag,
    #[strum(serialize = "EVENT_SUMMARY")]
    EventSummary,
    #[strum(serialize = "EVENT")]
    Event,
}

pub fn invalid_input_error(field: &'static str, message: &'static str) -> AppError {
    let mut report = garde::Report::new();
    report.append(garde::Path::new(field), garde::Error::new(message));
    AppError::InvalidInputError(report)
}

#[allow(clippy::result_large_err)]
pub trait ToAppResult {
    type Output: entity::AppEntity;
    fn to_result(self) -> AppResult<Self::Output>;
    fn check_absent(self) -> AppResult;
    fn check_absent_details(self, details: Vec<(String, String)>) -> AppResult;
    fn to_result_details(self, details: Vec<(String, String)>) -> AppResult<Self::Output>;
}

impl<T> ToAppResult for Option<T>
where
    T: entity::AppEntity,
{
    type Output = T;
    fn to_result(self) -> AppResult<Self::Output> {
        self.ok_or_else(|| {
            AppError::NotFoundError(Resource {
                details: vec![],
                resource_type: Self::Output::RESOURCE,
            })
        })
    }

    fn to_result_details(self, details: Vec<(String, String)>) -> AppResult<Self::Output> {
        self.ok_or_else(|| {
            AppError::NotFoundError(Resource {
                details,
                resource_type: Self::Output::RESOURCE,
            })
        })
    }

    fn check_absent(self) -> AppResult {
        if self.is_some() {
            Err(AppError::ResourceExistsError(Resource {
                details: vec![],
                resource_type: Self::Output::RESOURCE,
            }))
        } else {
            Ok(())
        }
    }

    fn check_absent_details(self, details: Vec<(String, String)>) -> AppResult {
        if self.is_some() {
            Err(AppError::ResourceExistsError(Resource {
                details,
                resource_type: Self::Output::RESOURCE,
            }))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // REVIEW_FIXES_PLAN §1.2: these variants previously mapped to misleading
    // status codes (Conflict/InvalidSession → 5xx/400). Lock in the corrected
    // semantics so a future edit can't silently regress them.
    #[test]
    fn conflict_error_maps_to_409() {
        let (status, _) = AppError::ConflictError("dup".into()).response();
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[test]
    fn invalid_session_error_maps_to_401() {
        let (status, _) = AppError::InvalidSessionError("nope".into()).response();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    /// Raw SeaORM/sqlx error text must not reach the client: the response
    /// message is generic even though the underlying error names a column.
    #[test]
    fn database_error_message_is_generic() {
        let err = AppError::DatabaseError(sea_orm::error::DbErr::Custom(
            "column Users.Password does not exist".into(),
        ));
        let (status, body) = err.response();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body.error_message, "A database error occurred");
        assert!(!body.error_message.contains("Password"));
        assert!(!body.error_message.contains("column"));
    }

    /// GH #55: the message parser behind the "data too long" → 400 mapping.
    ///
    /// The driver's text is the only place the column name appears, so this
    /// runs on a string. It must extract the column and nothing else — the
    /// same message can carry the offending value, which is what the
    /// redaction in `response()` exists to keep out of a reply.
    #[test]
    fn the_offending_column_is_extracted() {
        assert_eq!(
            parse_too_long_column("Data too long for column 'Name' at row 1"),
            Some("Name".to_string())
        );
        assert_eq!(
            parse_too_long_column("Data too long for column 'MaxBandwidth' at row 3"),
            Some("MaxBandwidth".to_string())
        );
    }

    #[test]
    fn an_unrecognised_message_yields_nothing_rather_than_a_guess() {
        // A wrong column name in an error is worse than no column name.
        for other in [
            "Duplicate entry 'x' for key 'PRIMARY'",
            "Data too long",
            "Data too long for column Name at row 1", // unquoted
            "Data too long for column '' at row 1",   // empty
            "",
        ] {
            assert_eq!(parse_too_long_column(other), None, "{other:?}");
        }
    }

    #[test]
    fn the_extracted_column_carries_no_value_or_sql() {
        // Guards the leak the redaction is there to prevent: whatever else the
        // driver put in the message, only the column comes out.
        let noisy = "Data too long for column 'Name' at row 1 \
                     (INSERT INTO Reports VALUES ('secret-value'))";
        assert_eq!(parse_too_long_column(noisy), Some("Name".to_string()));
    }
}
