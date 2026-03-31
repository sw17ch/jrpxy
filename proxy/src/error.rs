use jrpxy_backend::error::BackendError;
use jrpxy_frontend::error::FrontendError;
use jrpxy_http_message::header::HeaderError;

#[derive(thiserror::Error, Debug)]
pub enum ProxyError {
    #[error("Frontend Error: {0}")]
    FrontendError(#[from] FrontendError),
    #[error("Backend Error: {0}")]
    BackendError(#[from] BackendError),
    #[error("No available backend connection")]
    NoBackendConnection,
    #[error("Invalid request framing: {0}")]
    InvalidRequestFraming(HeaderError),
    #[error("Body copy error: {0}")]
    CopyError(#[from] ProxyCopyError),
    #[error("Error copying the frontend body to the backend, but response completed: {0}")]
    FrontendCopyError(ProxyCopyError),
    #[error("Failed to completely copy the frontend body to the backend, but response completed")]
    FrontendCopyIncomplete,
    #[error("Proxy error on frontend: {0}")]
    ProxyFrontend(ProxyFrontendError),
}

#[derive(thiserror::Error, Debug)]
pub enum ProxyFrontendError {
    #[error("CONNECT method is not supported")]
    ConnectNotSupported,
    #[error("TRACE method is not supported")]
    TraceNotSupported,
}

#[derive(thiserror::Error, Debug)]
pub enum ProxyCopyError {
    #[error("Frontend Error: {0}")]
    FrontendError(#[from] FrontendError),
    #[error("Backend Error: {0}")]
    BackendError(#[from] BackendError),
}

pub type ProxyResult<T> = std::result::Result<T, ProxyError>;
