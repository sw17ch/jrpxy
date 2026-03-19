use jrpxy_http_message::{
    header::{ConnectionTokenParserError, HeaderError},
    message::MessageError,
};

use jrpxy_body::error::BodyError;
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
    #[error("Invalid connection header: {0}")]
    InvalidConnectionHeader(#[from] ConnectionTokenParserError),
    #[error("Request head exceeded size limit: {0} >= {1}")]
    MaxHeadLenExceeded(usize, usize),
}

/// A result type where the error is [`FrontendError`].
pub type FrontendResult<T> = Result<T, FrontendError>;

impl From<BodyError> for FrontendError {
    fn from(e: BodyError) -> Self {
        FrontendError::BodyWriteError(e)
    }
}
