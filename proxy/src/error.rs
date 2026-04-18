use jrpxy_backend::error::BackendError;
use jrpxy_frontend::error::FrontendError;
use jrpxy_http_message::header::HeaderError;

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
}

pub type ProxyResult<T> = std::result::Result<T, ProxyError>;
