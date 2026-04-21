use std::io;

use jrpxy_backend::error::BackendError;
use jrpxy_body::error::BodyError;
use jrpxy_frontend::error::FrontendError;
use jrpxy_http_message::{header::HeaderError, message::MessageError};

use crate::ConnectionTokenParserError;

#[derive(thiserror::Error, Debug)]
pub enum ProxyError {
    #[error("Invalid request framing: {0}")]
    InvalidRequestFraming(HeaderError),
    #[error("Body copy error: {0}")]
    CopyError(#[from] ProxyCopyError),
    #[error("Error copying the frontend body to the backend, but response completed: {0}")]
    FrontendCopyError(ProxyCopyError),
    #[error("Failed to completely copy the frontend body to the backend, but response completed")]
    FrontendCopyIncomplete,
    #[error("Proxy error on frontend: {0}")]
    ProxyFrontend(#[from] ProxyFrontendError),
    #[error("Proxy error on backend: {0}")]
    ProxyBackend(#[from] ProxyBackendError),
}

#[derive(thiserror::Error, Debug)]
pub enum ProxyFrontendError {
    #[error("Frontend error: {0}")]
    FrontendError(#[from] FrontendError),
    #[error("CONNECT method is not supported")]
    ConnectNotSupported,
    #[error("TRACE method is not supported")]
    TraceNotSupported,
    #[error("Invalid connection header")]
    InvalidConnectionHeader(#[from] ConnectionTokenParserError),
}

#[derive(thiserror::Error, Debug)]
pub enum ProxyBackendError {
    #[error("Backend error: {0}")]
    BackendError(#[from] BackendError),
    #[error("Invalid connection header")]
    InvalidConnectionHeader(#[from] ConnectionTokenParserError),
}

#[derive(thiserror::Error, Debug)]
pub enum ProxyCopyError {
    #[error("Frontend Error: {0}")]
    FrontendError(#[from] FrontendError),
    #[error("Backend Error: {0}")]
    BackendError(#[from] BackendError),
    #[error("The frontend writer went away while processing copying")]
    FrontendWriterGone,
    #[error("The backend writer went away while processing copying")]
    BackendWriterGone,
}

pub type ProxyResult<T> = std::result::Result<T, ProxyError>;

#[derive(thiserror::Error, Debug)]
pub enum ProxyBackendReaderError {
    #[error("Failed to read backend: {0}")]
    ReadError(std::io::Error),
    #[error("Failed to read body: {0}")]
    BodyReadError(BodyError),
    #[error("First read returned EOF")]
    FirstReadEOF,
    #[error("Unexpected end of file while reading")]
    UnexpectedEOF,
    #[error("Failed to parse response: {0}")]
    ParseError(MessageError),
    #[error("Header error: {0}")]
    HeaderError(#[from] HeaderError),
    #[error("Response head exceeded size limit: {0} >= {1}")]
    MaxHeadLenExceeded(usize, usize),
    #[error("101 Switching Protocols is not supported")]
    SwitchingProtocolsUnsupported,
    #[error("102 Processing is deprecated and not supported")]
    ProcessingUnsupported,
    #[error("Unsupported informational status code: {0}")]
    UnsupportedInformational(u16),
    #[error("1xx informational response contains framing headers")]
    FramingHeadersOnInformationalResponse,
    #[error("204 No Content response contains framing headers")]
    FramingHeadersOn204NoContentResponse,
}

pub type ProxyBackendReaderResult<T> = Result<T, ProxyBackendReaderError>;

#[derive(thiserror::Error, Debug)]
pub enum ProxyFrontendReaderError {
    #[error("Failed to read frontend: {0}")]
    ReadError(std::io::Error),
    #[error("Failed to read body: {0}")]
    BodyReadError(BodyError),
    #[error("First read returned EOF")]
    FirstReadEOF,
    #[error("Unexpected end of file while reading")]
    UnexpectedEOF,
    #[error("Failed to parse request: {0}")]
    ParseError(MessageError),
    #[error("Header error: {0}")]
    HeaderError(#[from] HeaderError),
    #[error("Request head exceeded size limit: {0} >= {1}")]
    MaxHeadLenExceeded(usize, usize),
}

pub type ProxyFrontendReaderResult<T> = Result<T, ProxyFrontendReaderError>;

#[derive(thiserror::Error, Debug)]
pub enum ProxyBackendWriterError {
    #[error("Write error: {0}")]
    WriteError(io::Error),
}

pub type ProxyBackendWriterResult<T> = Result<T, ProxyBackendWriterError>;

#[derive(thiserror::Error, Debug)]
pub enum ProxyFrontendWriterError {
    #[error("Write error: {0}")]
    WriteError(io::Error),
}

pub type ProxyFrontendWriterResult<T> = Result<T, ProxyFrontendWriterError>;

#[derive(thiserror::Error, Debug)]
pub enum ProxyRequestBodyForwarderError {
    #[error("Frontend chunked body reader is not at a chunk boundary; cannot forward")]
    ChunkedNotAtBoundary,
    #[error("Frontend chunked body reader is in an error state")]
    ChunkedInErrorState,
    #[error("Body error: {0}")]
    BodyError(#[from] BodyError),
}

pub type ProxyRequestBodyForwarderResult<T> = Result<T, ProxyRequestBodyForwarderError>;
