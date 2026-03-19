//! Errors encountered by the proxy.

use jrpxy_backend::error::BackendError;
use jrpxy_body::error::BodyError;
use jrpxy_frontend::error::FrontendError;
use jrpxy_http_message::header::HeaderError;

use crate::ConnectionTokenParserError;

/// Errors encountered by the proxy.
#[derive(thiserror::Error, Debug)]
pub enum ProxyError {
    /// The request framing is bad.
    #[error("Invalid request framing: {0}")]
    InvalidRequestFraming(HeaderError),
    /// An error occurred while copying the body.
    #[error("Body copy error: {0}")]
    CopyError(#[from] ProxyCopyError),
}

/// An error occurred while interacting with the frontend.
#[derive(thiserror::Error, Debug)]
pub enum ProxyFrontendError {
    /// The frontend attempted to CONNECT.
    #[error("CONNECT method is not supported")]
    ConnectNotSupported,
    /// The frontend attempted to TRACE.
    #[error("TRACE method is not supported")]
    TraceNotSupported,
    /// A frontend error occurred.
    #[error("Frontend error: {0}")]
    FrontendError(#[from] FrontendError),
    /// The frontend connection header was invalid.
    #[error("Invalid connection header")]
    InvalidConnectionHeader(#[from] ConnectionTokenParserError),
    /// The request-target is not in a form the proxy can forward (RFC 9112
    /// section 3.2).
    #[error("Malformed request-target")]
    MalformedRequestTarget,
    /// The request used asterisk-form (`*`) with a method other than OPTIONS
    /// (RFC 9112 section 3.2.4).
    #[error("asterisk-form request-target is only valid for OPTIONS")]
    AsteriskFormNotAllowed,
    /// The request used authority-form with a method other than CONNECT (RFC
    /// 9112 section 3.2.3).
    #[error("authority-form request-target is only valid for CONNECT")]
    AuthorityFormNotAllowed,
    /// An HTTP/1.1 request was missing the required Host header (RFC 9112
    /// section 3.2.2).
    #[error("HTTP/1.1 request missing Host header")]
    MissingHost,
    /// The request had more than one Host header (RFC 9110 section 7.2).
    #[error("request has multiple Host headers")]
    MultipleHosts,
    /// The request is from an http/1.0 client, and contained a
    /// transfer-encoding header (RFC 9112 section 6.1)
    #[error("http/1.0 client sent a transfer-encoding header")]
    TransferEncodingOnHttp10Client,
}

/// An error occurred while interacting with the backend.
#[derive(thiserror::Error, Debug)]
pub enum ProxyBackendError {
    /// A backend error occurred.
    #[error("Backend error: {0}")]
    BackendError(#[from] BackendError),
    /// The backend connection header was invalid.
    #[error("Invalid connection header")]
    InvalidConnectionHeader(#[from] ConnectionTokenParserError),
}

/// An error occurred while copying bodies between frontend and backend.
#[derive(thiserror::Error, Debug)]
pub enum ProxyCopyError {
    /// An error was encountered while interacting with the frontend.
    #[error("Frontend Error: {0}")]
    FrontendError(#[from] ProxyFrontendError),
    /// An error was encountered while interacting with the backend.
    #[error("Backend Error: {0}")]
    BackendError(#[from] ProxyBackendError),
    /// A forward operation was attempted after an error occurred.
    #[error("Body forwarder is latched following a prior error")]
    ForwardAfterError,
    /// Failed to fully copy the body from the frontend to the backend.
    #[error("Failed to completely copy the frontend body to the backend")]
    FrontendCopyIncomplete,
    /// Failed to fully copy the body from the backend to the frontend.
    #[error("Failed to completely copy the backend body to the frontend")]
    BackendCopyIncomplete,
}

impl From<FrontendError> for ProxyCopyError {
    fn from(e: FrontendError) -> Self {
        Self::FrontendError(e.into())
    }
}

impl From<BodyError> for ProxyFrontendError {
    fn from(e: BodyError) -> Self {
        Self::FrontendError(e.into())
    }
}

/// Symmetric to the [`ProxyFrontendError`] impl above, for backend
/// writes.
impl From<BodyError> for ProxyBackendError {
    fn from(e: BodyError) -> Self {
        Self::BackendError(e.into())
    }
}

impl From<BackendError> for ProxyCopyError {
    fn from(e: BackendError) -> Self {
        Self::BackendError(e.into())
    }
}

impl From<ProxyFrontendError> for ProxyError {
    fn from(e: ProxyFrontendError) -> Self {
        Self::CopyError(e.into())
    }
}

impl From<ProxyBackendError> for ProxyError {
    fn from(e: ProxyBackendError) -> Self {
        Self::CopyError(e.into())
    }
}

/// An error encountered while configuring [`crate::ProxyOptions`].
#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum ProxyOptionsError {
    /// `received_by` was empty; a Via pseudonym is `1*tchar` (RFC 9110 section
    /// 7.6.3), so it must have at least one character.
    #[error("received_by must not be empty")]
    EmptyReceivedBy,
    /// `received_by` contained a byte that is not a valid `tchar`. Emitting it
    /// into the Via header could corrupt the field or split the message (RFC
    /// 9110 sections 5.6.2, 7.6.3).
    #[error("received_by contains illegal byte 0x{0:02x}")]
    IllegalReceivedByByte(u8),
}

/// The proxy result type.
pub type ProxyResult<T> = std::result::Result<T, ProxyError>;
