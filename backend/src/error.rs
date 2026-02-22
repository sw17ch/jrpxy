use jrpxy_body::BodyError;
use jrpxy_http_message::{header::HeaderError, message::MessageError};
use tokio::io;

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("Failed to write to backend: {0}")]
    WriteError(io::Error),
    #[error("Failed to write body to backend: {0}")]
    BodyWriteError(BodyError),
    #[error("Failed to read backend: {0}")]
    ReadError(io::Error),
    #[error("Failed to read body: {0}")]
    BodyReadError(BodyError),
    #[error("First read failed")]
    FirstReadEOF,
    #[error("Unexpected end of file while reading")]
    UnexpectedEOF,
    #[error("Failed to parse response: {0}")]
    HttpResponseParseError(MessageError),
    #[error("Header error: {0}")]
    HeaderError(#[from] HeaderError),
    #[error("101-switching-protocols unsupported")]
    HttpSwitchingProtocolsUnsupported,
    #[error("102-processing deprecated")]
    HttpProcessingUnsupported,
    #[error("Unsupported unknown informational status: {0}")]
    HttpUnsupportedInformational(u16),
    #[error("Response head exceeded size limit: {0} >= {1}")]
    MaxHeadLenExceeded(usize, usize),
    #[error("1xx response contains framing headers")]
    FramingHeadersOnInformationalResposne,
    #[error("204 No Content response contains framing headers")]
    FramingHeadersOn204NoContentResposne,
}

pub type BackendResult<T> = Result<T, BackendError>;
