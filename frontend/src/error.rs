use jrpxy_http_message::{header::HeaderError, message::MessageError};

use jrpxy_body::BodyError;
use tokio::io;

/// Possible errors from interacting with frontends.
#[derive(Debug, thiserror::Error)]
pub enum FrontendError {
    #[error("Failed to write to frontend: {0}")]
    WriteError(io::Error),
    #[error("Failed to write body to frontend: {0}")]
    BodyWriteError(BodyError),
    #[error("Failed to read frontend: {0}")]
    ReadError(io::Error),
    #[error("Failed to read body: {0}")]
    BodyReadError(BodyError),
    #[error("First read failed")]
    FirstReadEOF,
    #[error("Unexpected end of file while reading")]
    UnexpectedEOF,
    #[error("Failed to parse request: {0}")]
    HttpRequestParseError(MessageError),
    #[error("Header error: {0}")]
    HeaderError(#[from] HeaderError),
    #[error("Request head exceeded size limit: {0} >= {1}")]
    MaxHeadLenExceeded(usize, usize),
}

/// A result type where the error is [`FrontendError`].
pub type FrontendResult<T> = Result<T, FrontendError>;
