//! ONVIF error type and result alias.

/// Errors raised by the ONVIF client subsystem.
#[derive(Debug, thiserror::Error)]
pub enum OnvifError {
    /// Underlying HTTP transport failure (connection, TLS, reqwest-level).
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    /// A SOAP 1.2 Fault returned by the device. `code` is the fault
    /// Code/Value (e.g. `s:Sender`), `reason` is the human-readable
    /// Reason/Text.
    #[error("soap fault: {code}: {reason}")]
    Soap {
        /// SOAP fault code (Code/Value, possibly with a Subcode).
        code: String,
        /// SOAP fault reason text.
        reason: String,
    },

    /// XML/SOAP parsing failure or an unexpected response shape.
    #[error("parse error: {0}")]
    Parse(String),

    /// Authentication failed (missing/invalid WS-Security credentials).
    #[error("authentication failed")]
    Auth,

    /// The request exceeded its timeout.
    #[error("request timed out")]
    Timeout,

    /// WS-Discovery failure (socket bind, multicast send/receive).
    #[error("discovery error: {0}")]
    Discovery(String),

    /// The service address reaches this server or a cloud metadata service,
    /// so no request was sent.
    #[error("refused: {0}")]
    Refused(String),
}

/// Convenience result alias for ONVIF operations.
pub type OnvifResult<T> = Result<T, OnvifError>;
