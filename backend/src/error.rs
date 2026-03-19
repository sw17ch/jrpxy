use jrpxy_body::error::BodyError;
use jrpxy_http_message::{
    header::{ConnectionTokenParserError, HeaderError},
    message::MessageError,
};
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
    #[error("Invalid connection header: {0}")]
    InvalidConnectionHeader(#[from] ConnectionTokenParserError),
    #[error("101-switching-protocols unsupported")]
    HttpSwitchingProtocolsUnsupported,
    #[error("102-processing deprecated")]
    HttpProcessingUnsupported,
    #[error("Response head exceeded size limit: {0} >= {1}")]
    MaxHeadLenExceeded(usize, usize),
    #[error("1xx response contains framing headers")]
    FramingHeadersOnInformationalResponse,
    #[error("204 No Content response contains framing headers")]
    FramingHeadersOn204NoContentResponse,
    #[error("Received transfer-encoding header from HTTP/1.0 server")]
    TransferEncodingOnHttp10Server,
}

pub type BackendResult<T> = Result<T, BackendError>;

impl From<BodyError> for BackendError {
    fn from(e: BodyError) -> Self {
        BackendError::BodyWriteError(e)
    }
}
