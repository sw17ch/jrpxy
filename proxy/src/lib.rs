use std::borrow::Cow;
use std::future::Future;
use std::task::Poll;

use bytes::Bytes;
use jrpxy_backend::{
    error::{BackendError, BackendResult},
    reader::{
        BackendBodyReader, BackendBodyReaderKind, BackendReader, BackendResponse,
        BackendStreamReader, ResponseStream,
    },
    writer::{BackendBodyWriter, BackendBodyWriterKind, BackendWriter},
};
use jrpxy_body::BodyReadMode;
use jrpxy_frontend::{
    error::FrontendError,
    reader::{FrontendBodyReader, FrontendBodyReaderKind, FrontendReader, FrontendRequest},
    writer::{FrontendBodyWriter, FrontendBodyWriterKind, FrontendWriter},
};
use jrpxy_http_message::{
    header::{HeaderError, Headers},
    message::{Request, Response},
    version::HttpVersion,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
}

#[derive(thiserror::Error, Debug)]
pub enum ProxyCopyError {
    #[error("Frontend Error: {0}")]
    FrontendError(#[from] FrontendError),
    #[error("Backend Error: {0}")]
    BackendError(#[from] BackendError),
}

pub type ProxyResult<T> = std::result::Result<T, ProxyError>;

/// Returned when writing the request head to the backend fails.
///
/// Holds the decomposed request so the caller can retry with a different
/// backend (via [`retry_backend_request`]) or write an error response to the
/// client (via [`into_pending_response`]). The failed backend connection is
/// discarded.
///
/// [`retry_backend_request`]: BackendRequestError::retry_backend_request
/// [`into_pending_response`]: BackendRequestError::into_pending_response
pub struct BackendRequestError<FR, FW> {
    /// The request that was being forwarded, with hop-by-hop headers removed
    /// and proxy headers (e.g. `Via`) already merged in.
    request: Request,
    frontend_body_reader: FrontendBodyReader<FR>,
    pending: PendingFrontendResponse<FW>,
    error: BackendError,
}

impl<FR, FW> std::fmt::Debug for BackendRequestError<FR, FW> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendRequestError")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl<FR, FW> From<BackendRequestError<FR, FW>> for ProxyError {
    fn from(e: BackendRequestError<FR, FW>) -> Self {
        ProxyError::BackendError(e.error)
    }
}

impl<FR, FW> BackendRequestError<FR, FW> {
    /// Returns the backend error that caused this failure.
    pub fn error(&self) -> &BackendError {
        &self.error
    }

    /// Consumes the error and returns a [`PendingFrontendResponse`] and the
    /// frontend body reader. The body reader is returned separately so the
    /// caller can decide whether to drain it (for connection reuse) or drop it
    /// (to close the read half of the socket immediately).
    pub fn into_pending_response(self) -> (PendingFrontendResponse<FW>, FrontendBodyReader<FR>) {
        (self.pending, self.frontend_body_reader)
    }
}

/// Returned when reading the response head from the backend fails.
///
/// The broken backend connection is discarded. The frontend state and the
/// original request are preserved so the caller can retry with a different
/// backend (via [`retry_backend_request`]) or write an error response to the
/// client (via [`into_pending_response`]).
///
/// [`retry_backend_request`]: BackendResponseError::retry_backend_request
/// [`into_pending_response`]: BackendResponseError::into_pending_response
pub struct BackendResponseError<FR, FW> {
    /// The request that was forwarded, ready to be sent to a different backend.
    request: Request,
    frontend_body_reader: FrontendBodyReader<FR>,
    pending: PendingFrontendResponse<FW>,
    error: BackendError,
}

impl<FR, FW> std::fmt::Debug for BackendResponseError<FR, FW> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendResponseError")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl<FR, FW> From<BackendResponseError<FR, FW>> for ProxyError {
    fn from(e: BackendResponseError<FR, FW>) -> Self {
        ProxyError::BackendError(e.error)
    }
}

impl<FR, FW> BackendResponseError<FR, FW> {
    /// Returns the backend error that caused this failure.
    pub fn error(&self) -> &BackendError {
        &self.error
    }

    /// Consumes the error and returns a [`PendingFrontendResponse`] and the
    /// frontend body reader. The body reader is returned separately so the
    /// caller can decide whether to drain it (for connection reuse) or drop it
    /// (to close the read half of the socket immediately).
    pub fn into_pending_response(self) -> (PendingFrontendResponse<FW>, FrontendBodyReader<FR>) {
        (self.pending, self.frontend_body_reader)
    }
}

/// Returned when reading the next response from the backend fails after at
/// least one informational response has already been received from the backend.
///
/// Retry is not offered: the backend has already acknowledged the request
/// (evidenced by the 1xx it sent), so retrying risks duplicate processing.
/// The frontend connection is still alive; an error can be sent to the client.
pub struct BackendStreamError<FR, FW> {
    frontend_body_reader: FrontendBodyReader<FR>,
    pending: PendingFrontendResponse<FW>,
    error: BackendError,
}

impl<FR, FW> std::fmt::Debug for BackendStreamError<FR, FW> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendStreamError")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl<FR, FW> From<BackendStreamError<FR, FW>> for ProxyError {
    fn from(e: BackendStreamError<FR, FW>) -> Self {
        ProxyError::BackendError(e.error)
    }
}

impl<FR, FW> BackendStreamError<FR, FW> {
    /// Returns the backend error that caused this failure.
    pub fn error(&self) -> &BackendError {
        &self.error
    }

    /// Consumes the error and returns a [`PendingFrontendResponse`] and the
    /// frontend body reader. The body reader is returned separately so the
    /// caller can decide whether to drain it (for connection reuse) or drop it
    /// (to close the read half of the socket immediately).
    pub fn into_pending_response(self) -> (PendingFrontendResponse<FW>, FrontendBodyReader<FR>) {
        (self.pending, self.frontend_body_reader)
    }
}

/// A frontend connection that has not yet received a response.
///
/// This type is intentionally distinct from a reusable [`FrontendWriter`] to
/// make it clear that a response still needs to be sent before the connection
/// can be reused or closed.
///
/// The required proxy headers (e.g. `Via`) are injected automatically by each
/// `send_as_*` method — the caller does not need to add them manually.
pub struct PendingFrontendResponse<FW> {
    frontend_writer: FrontendWriter<FW>,
    options: ProxyOptions,
    client_options: ClientOptions,
}

impl<FW: AsyncWriteExt + Unpin> PendingFrontendResponse<FW> {
    /// Send the response with a chunked body. Proxy headers are injected
    /// automatically.
    pub async fn send_as_chunked(
        self,
        response: &mut Response,
    ) -> Result<FrontendBodyWriter<FW>, FrontendError> {
        insert_proxy_headers(
            response,
            self.client_options.version,
            &self.options.received_by,
        );
        self.frontend_writer.send_as_chunked(response).await
    }

    /// Send the response with a content-length delimited body. Proxy headers
    /// are injected automatically.
    pub async fn send_as_content_length(
        self,
        response: &mut Response,
        body_len: u64,
    ) -> Result<FrontendBodyWriter<FW>, FrontendError> {
        insert_proxy_headers(
            response,
            self.client_options.version,
            &self.options.received_by,
        );
        self.frontend_writer
            .send_as_content_length(response, body_len)
            .await
    }

    /// Send the response with no body, preserving any framing headers from
    /// the origin (for `HEAD` and `304 Not Modified`). Proxy headers are
    /// injected automatically.
    pub async fn send_as_bodyless_keep_framing(
        self,
        response: &mut Response,
    ) -> Result<FrontendBodyWriter<FW>, FrontendError> {
        insert_proxy_headers(
            response,
            self.client_options.version,
            &self.options.received_by,
        );
        self.frontend_writer
            .send_as_bodyless_keep_framing(response)
            .await
    }

    /// Send the response with no body and no framing headers (for `1xx` and
    /// `204 No Content`). Proxy headers are injected automatically.
    pub async fn send_as_no_content(
        self,
        response: &mut Response,
    ) -> Result<FrontendBodyWriter<FW>, FrontendError> {
        insert_proxy_headers(
            response,
            self.client_options.version,
            &self.options.received_by,
        );
        self.frontend_writer.send_as_no_content(response).await
    }
}

/// Error returned by [`ReceivedInformationalResponse::forward_informational_response`].
///
/// Two distinct failure modes arise in that method:
///
/// - [`Frontend`]: writing the 1xx to the client failed; `frontend_writer` has
///   been consumed and cannot be recovered.
/// - [`Backend`]: reading the next response from the backend failed; the
///   frontend is still alive and the error carries `frontend_writer` so the
///   caller can send an error response to the client.
///
/// [`Frontend`]: InformationalForwardError::Frontend
/// [`Backend`]: InformationalForwardError::Backend
pub enum InformationalForwardError<FR, FW> {
    Frontend(FrontendError),
    Backend(BackendStreamError<FR, FW>),
}

impl<FR, FW> std::fmt::Debug for InformationalForwardError<FR, FW> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InformationalForwardError::Frontend(e) => f.debug_tuple("Frontend").field(e).finish(),
            InformationalForwardError::Backend(e) => f.debug_tuple("Backend").field(e).finish(),
        }
    }
}

impl<FR, FW> From<InformationalForwardError<FR, FW>> for ProxyError {
    fn from(e: InformationalForwardError<FR, FW>) -> Self {
        match e {
            InformationalForwardError::Frontend(e) => ProxyError::FrontendError(e),
            InformationalForwardError::Backend(e) => ProxyError::BackendError(e.error),
        }
    }
}

impl<FR, FW> BackendResponseError<FR, FW>
where
    FW: AsyncWriteExt + Unpin,
{
    /// Retry forwarding the request to a different backend connection.
    ///
    /// On success, returns a [`ForwardedRequest`] ready for
    /// [`read_backend_response`]. On failure, returns another
    /// [`BackendResponseError`] with the same request and frontend state, so
    /// the caller can try yet another backend or give up.
    ///
    /// [`read_backend_response`]: ForwardedRequest::read_backend_response
    pub async fn retry_backend_request<BR, BW>(
        self,
        backend_connection: BackendConnection<BR, BW>,
    ) -> Result<ForwardedRequest<FR, FW, BR, BW>, BackendResponseError<FR, FW>>
    where
        BR: AsyncReadExt + Unpin,
        BW: AsyncWriteExt + Unpin,
    {
        let Self {
            request,
            frontend_body_reader,
            pending:
                PendingFrontendResponse {
                    frontend_writer,
                    options,
                    client_options,
                },
            error: _,
        } = self;

        let (backend_reader, backend_writer) = backend_connection;
        let write_result = match frontend_body_reader.mode() {
            BodyReadMode::Chunk => backend_writer.send_as_chunked(&request).await,
            BodyReadMode::ContentLength(cl) => {
                backend_writer.send_as_content_length(&request, cl).await
            }
            BodyReadMode::Bodyless => backend_writer.send_as_bodyless(&request).await,
        };

        match write_result {
            Ok(backend_body_writer) => Ok(ForwardedRequest {
                request,
                frontend_body_reader,
                pending: PendingFrontendResponse {
                    frontend_writer,
                    options,
                    client_options,
                },
                backend_reader,
                backend_body_writer,
            }),
            Err(error) => Err(BackendResponseError {
                request,
                frontend_body_reader,
                pending: PendingFrontendResponse {
                    frontend_writer,
                    options,
                    client_options,
                },
                error,
            }),
        }
    }
}

impl<FR, FW> BackendRequestError<FR, FW>
where
    FW: AsyncWriteExt + Unpin,
{
    /// Retry forwarding the request to a different backend connection.
    ///
    /// On success, returns a [`ForwardedRequest`] ready for
    /// [`read_backend_response`]. On failure, returns another
    /// [`BackendRequestError`] with the same request and frontend state, so
    /// the caller can try yet another backend or give up.
    ///
    /// [`read_backend_response`]: ForwardedRequest::read_backend_response
    pub async fn retry_backend_request<BR, BW>(
        self,
        backend_connection: BackendConnection<BR, BW>,
    ) -> Result<ForwardedRequest<FR, FW, BR, BW>, BackendRequestError<FR, FW>>
    where
        BR: AsyncReadExt + Unpin,
        BW: AsyncWriteExt + Unpin,
    {
        let Self {
            request,
            frontend_body_reader,
            pending:
                PendingFrontendResponse {
                    frontend_writer,
                    options,
                    client_options,
                },
            error: _,
        } = self;

        let (backend_reader, backend_writer) = backend_connection;
        let write_result = match frontend_body_reader.mode() {
            BodyReadMode::Chunk => backend_writer.send_as_chunked(&request).await,
            BodyReadMode::ContentLength(cl) => {
                backend_writer.send_as_content_length(&request, cl).await
            }
            BodyReadMode::Bodyless => backend_writer.send_as_bodyless(&request).await,
        };

        match write_result {
            Ok(backend_body_writer) => Ok(ForwardedRequest {
                request,
                frontend_body_reader,
                pending: PendingFrontendResponse {
                    frontend_writer,
                    options,
                    client_options,
                },
                backend_reader,
                backend_body_writer,
            }),
            Err(error) => Err(BackendRequestError {
                request,
                frontend_body_reader,
                pending: PendingFrontendResponse {
                    frontend_writer,
                    options,
                    client_options,
                },
                error,
            }),
        }
    }
}

type BackendConnection<BR, BW> = (BackendReader<BR>, BackendWriter<BW>);

pub struct ProxyOptions {
    pub max_backend_head_length: usize,
    pub body_chunk_size: usize,
    /// TODO: make a builder for ProxyOptions so that we can validate
    /// `received_by` doesn't contain something like \r\n which would do bad
    /// things when inserting it into the `Via` header.
    pub received_by: Cow<'static, str>,
}

struct ClientOptions {
    is_head: bool,
    version: HttpVersion,
}

impl Default for ProxyOptions {
    fn default() -> Self {
        Self {
            max_backend_head_length: 8192,
            body_chunk_size: 8192,
            received_by: Cow::Borrowed("jrpxy"),
        }
    }
}

pub struct ProxyClient<FR, FW> {
    frontend_reader: FrontendReader<FR>,
    frontend_writer: FrontendWriter<FW>,
    options: ProxyOptions,
}

impl<FR, FW> ProxyClient<FR, FW> {
    pub fn into_parts(self) -> (FrontendReader<FR>, FrontendWriter<FW>) {
        let Self {
            frontend_reader,
            frontend_writer,
            options: _,
        } = self;
        (frontend_reader, frontend_writer)
    }
}

impl<FR, FW> ProxyClient<FR, FW>
where
    FR: AsyncReadExt + Unpin,
    FW: AsyncWriteExt + Unpin,
{
    pub fn new(
        frontend_reader: FrontendReader<FR>,
        frontend_writer: FrontendWriter<FW>,
        options: ProxyOptions,
    ) -> Self {
        Self {
            frontend_reader,
            frontend_writer,
            options,
        }
    }

    pub async fn start(self) -> ProxyResult<ReceivedRequest<FR, FW>> {
        let Self {
            frontend_reader,
            frontend_writer,
            options,
        } = self;
        let req = frontend_reader.read().await?;
        let proxy_request = ProxyRequest::new(req, &options.received_by);
        let is_head = proxy_request.req().method() == b"HEAD".as_slice();

        // TODO: detect CONNECT and TRACE methods. RFC 9110 section 9.3.6 says
        // we must reject CONNECT methods if they are not supported. Typically
        // this is done by sending a '400 Bad Request'.

        let version = proxy_request.req().version();
        Ok(ReceivedRequest {
            proxy_request,
            pending: PendingFrontendResponse {
                frontend_writer,
                options,
                client_options: ClientOptions { is_head, version },
            },
        })
    }
}

pub struct ReceivedRequest<FR, FW> {
    proxy_request: ProxyRequest<FR>,
    pending: PendingFrontendResponse<FW>,
}

impl<FR, FW> ReceivedRequest<FR, FW> {
    pub fn as_proxy_request(&self) -> &ProxyRequest<FR> {
        &self.proxy_request
    }
    pub fn as_proxy_request_mut(&mut self) -> &mut ProxyRequest<FR> {
        &mut self.proxy_request
    }
}

impl<FR, FW> ReceivedRequest<FR, FW>
where
    FW: AsyncWriteExt + Unpin,
{
    /// Write the request to the backend connection.
    ///
    /// The backend reader and writer are taken here, allowing the caller to
    /// select the backend based on the request (e.g. by hostname or path)
    /// after inspecting the request via [`ReceivedRequest::as_proxy_request`].
    ///
    /// On error, returns a [`BackendRequestError`] holding the request and
    /// frontend state. Use [`BackendRequestError::retry_backend_request`] to
    /// try a different backend, or write an error response to the client via
    /// `frontend_writer`. The failed backend connection is discarded.
    pub async fn write_backend_request<BR, BW>(
        self,
        backend_connection: BackendConnection<BR, BW>,
    ) -> Result<ForwardedRequest<FR, FW, BR, BW>, BackendRequestError<FR, FW>>
    where
        BR: AsyncReadExt + Unpin,
        BW: AsyncWriteExt + Unpin,
    {
        let Self {
            proxy_request,
            pending,
        } = self;
        let PendingFrontendResponse {
            frontend_writer,
            options,
            client_options,
        } = pending;

        let (backend_reader, backend_writer) = backend_connection;
        let (backend_request, frontend_body_reader) = proxy_request.into_backend_request();

        let write_result = match frontend_body_reader.mode() {
            BodyReadMode::Chunk => backend_writer.send_as_chunked(&backend_request).await,
            BodyReadMode::ContentLength(cl) => {
                backend_writer
                    .send_as_content_length(&backend_request, cl)
                    .await
            }
            BodyReadMode::Bodyless => backend_writer.send_as_bodyless(&backend_request).await,
        };

        match write_result {
            Ok(backend_body_writer) => Ok(ForwardedRequest {
                request: backend_request,
                frontend_body_reader,
                pending: PendingFrontendResponse {
                    frontend_writer,
                    options,
                    client_options,
                },
                backend_reader,
                backend_body_writer,
            }),
            Err(error) => Err(BackendRequestError {
                request: backend_request,
                frontend_body_reader,
                pending: PendingFrontendResponse {
                    frontend_writer,
                    options,
                    client_options,
                },
                error,
            }),
        }
    }
}

pub struct ForwardedRequest<FR, FW, BR, BW> {
    request: Request,
    frontend_body_reader: FrontendBodyReader<FR>,
    pending: PendingFrontendResponse<FW>,
    backend_reader: BackendReader<BR>,
    backend_body_writer: BackendBodyWriter<BW>,
}

pub enum ReceivedResponseStream<FR, FW, BR, BW> {
    Informational(ReceivedInformationalResponse<FR, FW, BR, BW>),
    Response(ReceivedResponse<FR, FW, BR, BW>),
}

impl<FR, FW, BR, BW> ForwardedRequest<FR, FW, BR, BW>
where
    BR: AsyncReadExt + Unpin,
    BW: AsyncWriteExt + Unpin,
{
    pub async fn read_backend_response(
        self,
    ) -> Result<ReceivedResponseStream<FR, FW, BR, BW>, BackendResponseError<FR, FW>> {
        let Self {
            request,
            frontend_body_reader,
            pending,
            backend_reader,
            backend_body_writer,
        } = self;

        let response_stream = match backend_reader
            .read(
                !pending.client_options.is_head,
                pending.options.max_backend_head_length,
            )
            .await
        {
            Ok(r) => r,
            Err(error) => {
                // backend_reader and backend_body_writer are both discarded;
                // the backend connection is broken.
                drop(backend_body_writer);
                return Err(BackendResponseError {
                    request,
                    frontend_body_reader,
                    pending,
                    error,
                });
            }
        };
        let response_stream =
            ProxyResponseStream::new(response_stream, &pending.options.received_by);
        Ok(match response_stream {
            ProxyResponseStream::Response(proxy_response) => {
                ReceivedResponseStream::Response(ReceivedResponse {
                    frontend_body_reader,
                    pending,
                    proxy_response,
                    backend_body_writer,
                })
            }
            ProxyResponseStream::Informational(
                proxy_informational_response,
                proxy_stream_reader,
            ) => ReceivedResponseStream::Informational(ReceivedInformationalResponse {
                proxy_informational_response,
                frontend_body_reader,
                pending,
                proxy_stream_reader,
                backend_body_writer,
            }),
        })
    }
}

pub enum InformationalForwardResult<FR, FW, BR, BW> {
    Forwarded(ReceivedResponseStream<FR, FW, BR, BW>),
    Dropped(ReceivedResponseStream<FR, FW, BR, BW>),
}

impl<FR, FW, BR, BW> InformationalForwardResult<FR, FW, BR, BW> {
    pub fn into_inner(self) -> ReceivedResponseStream<FR, FW, BR, BW> {
        match self {
            InformationalForwardResult::Forwarded(r) => r,
            InformationalForwardResult::Dropped(r) => r,
        }
    }
}

pub struct ReceivedInformationalResponse<FR, FW, BR, BW> {
    proxy_informational_response: ProxyInformationalResponse,
    frontend_body_reader: FrontendBodyReader<FR>,
    pending: PendingFrontendResponse<FW>,
    proxy_stream_reader: ProxyStreamReader<BR>,
    backend_body_writer: BackendBodyWriter<BW>,
}

impl<FR, FW, BR, BW> ReceivedInformationalResponse<FR, FW, BR, BW>
where
    FW: AsyncWriteExt + Unpin,
    BR: AsyncReadExt + Unpin,
    BW: AsyncWriteExt + Unpin,
{
    pub fn as_response(&self) -> &ProxyInformationalResponse {
        &self.proxy_informational_response
    }

    pub fn as_response_mut(&mut self) -> &mut ProxyInformationalResponse {
        &mut self.proxy_informational_response
    }

    pub async fn forward_informational_response(
        self,
    ) -> Result<InformationalForwardResult<FR, FW, BR, BW>, InformationalForwardError<FR, FW>> {
        let Self {
            proxy_informational_response,
            frontend_body_reader,
            pending:
                PendingFrontendResponse {
                    mut frontend_writer,
                    options,
                    client_options,
                },
            proxy_stream_reader,
            backend_body_writer,
        } = self;

        // RFC9110 section 15.2 states that a server MUST NOT send any 1xx
        // response to a HTTP/1.0 client.
        let client_supports_informational_response = client_options.version == HttpVersion::Http11;

        if client_supports_informational_response {
            let response = proxy_informational_response.into_frontend_response();
            let frontend_body_writer = frontend_writer
                .send_as_no_content(&response)
                .await
                .map_err(InformationalForwardError::Frontend)?;
            frontend_writer = frontend_body_writer
                .finish()
                .await
                .map_err(InformationalForwardError::Frontend)?;
        } else {
            drop(proxy_informational_response);
        }

        let response_stream = match proxy_stream_reader.read().await {
            Ok(r) => r,
            Err(error) => {
                return Err(InformationalForwardError::Backend(BackendStreamError {
                    frontend_body_reader,
                    pending: PendingFrontendResponse {
                        frontend_writer,
                        options,
                        client_options,
                    },
                    error,
                }));
            }
        };

        let response_stream = match response_stream {
            ProxyResponseStream::Response(proxy_response) => {
                ReceivedResponseStream::Response(ReceivedResponse {
                    frontend_body_reader,
                    pending: PendingFrontendResponse {
                        frontend_writer,
                        options,
                        client_options,
                    },
                    proxy_response,
                    backend_body_writer,
                })
            }
            ProxyResponseStream::Informational(
                proxy_informational_response,
                proxy_stream_reader,
            ) => ReceivedResponseStream::Informational(ReceivedInformationalResponse {
                proxy_informational_response,
                frontend_body_reader,
                pending: PendingFrontendResponse {
                    frontend_writer,
                    options,
                    client_options,
                },
                proxy_stream_reader,
                backend_body_writer,
            }),
        };

        Ok(if client_supports_informational_response {
            InformationalForwardResult::Forwarded(response_stream)
        } else {
            InformationalForwardResult::Dropped(response_stream)
        })
    }
}

pub struct ReceivedResponse<FR, FW, BR, BW> {
    frontend_body_reader: FrontendBodyReader<FR>,
    pending: PendingFrontendResponse<FW>,
    proxy_response: ProxyResponse<BR>,
    backend_body_writer: BackendBodyWriter<BW>,
}

impl<FR, FW, BR, BW> ReceivedResponse<FR, FW, BR, BW>
where
    FR: AsyncReadExt + Unpin,
    FW: AsyncWriteExt + Unpin,
    BR: AsyncReadExt + Unpin,
    BW: AsyncWriteExt + Unpin,
{
    pub fn as_response(&self) -> &ProxyResponse<BR> {
        &self.proxy_response
    }

    pub fn as_response_mut(&mut self) -> &mut ProxyResponse<BR> {
        &mut self.proxy_response
    }

    pub async fn forward_response(
        self,
    ) -> ProxyResult<(ProxyClient<FR, FW>, BackendConnection<BR, BW>)> {
        let Self {
            mut frontend_body_reader,
            pending:
                PendingFrontendResponse {
                    frontend_writer,
                    options,
                    client_options,
                },
            proxy_response,
            mut backend_body_writer,
        } = self;

        // TODO: use client_options to decide if we need to buffer the response
        // (chunk-encoded) or if we can send back a content-length.
        let is_head = client_options.is_head;

        let (response, mut backend_body_reader) = proxy_response.into_frontend_response();
        let mut frontend_body_writer = match backend_body_reader.mode() {
            BodyReadMode::Chunk => frontend_writer.send_as_chunked(&response).await?,
            BodyReadMode::ContentLength(cl) => {
                frontend_writer
                    .send_as_content_length(&response, cl)
                    .await?
            }
            BodyReadMode::Bodyless => {
                // HEAD responses and 304 Not Modified carry framing headers
                // that describe the representation, not an actual body; those
                // must be forwarded. All other bodyless codes (204, etc.) must
                // not carry framing headers.
                if is_head || response.code() == 304 {
                    frontend_writer
                        .send_as_bodyless_keep_framing(&response)
                        .await?
                } else {
                    frontend_writer.send_as_no_content(&response).await?
                }
            }
        };

        let f2b_fut = async move {
            let ret;
            loop {
                let buf = match frontend_body_reader.read(options.body_chunk_size).await {
                    Ok(Some(buf)) => buf,
                    Ok(None) => {
                        let frontend_kind = frontend_body_reader.into_kind();
                        let backend_kind = backend_body_writer.into_kind();

                        ret = async {
                            match (frontend_kind, backend_kind) {
                                (FrontendBodyReaderKind::TE(fr), BackendBodyWriterKind::TE(bw)) => {
                                    let (next_reader, trailers) = fr.drain().await?;
                                    let next_backend = bw.finish_with_trailers(&trailers).await?;
                                    Ok((next_reader, next_backend))
                                }
                                (fr, bw) => {
                                    let next_reader = fr.drain().await?;
                                    let next_backend = bw.finish().await?;
                                    Ok((next_reader, next_backend))
                                }
                            }
                        }
                        .await;
                        break;
                    }
                    Err(e) => {
                        ret = Err(ProxyCopyError::from(e));
                        break;
                    }
                };
                match backend_body_writer.write(&buf).await {
                    Ok(()) => {
                        // successfuly wrote buffer; loop around for another one
                    }
                    Err(e) => {
                        ret = Err(ProxyCopyError::from(e));
                        break;
                    }
                }
            }
            ret
        };

        let b2f_fut = async move {
            let ret;
            loop {
                let buf = match backend_body_reader.read(options.body_chunk_size).await {
                    Ok(Some(buf)) => buf,
                    Ok(None) => {
                        let backend_kind = backend_body_reader.into_kind();
                        let frontend_kind = frontend_body_writer.into_kind();

                        ret = async {
                            match (backend_kind, frontend_kind) {
                                (BackendBodyReaderKind::TE(br), FrontendBodyWriterKind::TE(fw)) => {
                                    let (next_backend, trailers) = br.drain().await?;
                                    let next_frontend = fw.finish_with_trailers(&trailers).await?;
                                    Ok((next_backend, next_frontend))
                                }
                                (br, fw) => {
                                    let next_backend = br.drain().await?;
                                    let next_frontend = fw.finish().await?;
                                    Ok((next_backend, next_frontend))
                                }
                            }
                        }
                        .await;
                        break;
                    }
                    Err(e) => {
                        ret = Err(ProxyCopyError::from(e));
                        break;
                    }
                };
                match frontend_body_writer.write(&buf).await {
                    Ok(()) => {}
                    Err(e) => {
                        ret = Err(ProxyCopyError::from(e));
                        break;
                    }
                }
            }
            ret
        };

        let mut f2b_fut_pinned = std::pin::pin!(f2b_fut);
        let mut b2f_fut_pinned = std::pin::pin!(b2f_fut);

        let mut f2b_res = None;
        // Poll both futures in a loop until the backend-to-frontend future
        // resolves.
        let b2f_res = loop {
            tokio::select! {
                f2b = &mut f2b_fut_pinned, if f2b_res.is_none() => f2b_res = Some(f2b),
                b2f = &mut b2f_fut_pinned => break b2f,
            }
        };

        // If polling both didn't result in the frontend-to-backend copy
        // completing, let's poll it once more to make sure it wasn't just about
        // to be finished (this can happen for short requests where we've sent
        // the write, but haven't yet allowed the future to resolve).
        if f2b_res.is_none() {
            match std::future::poll_fn(|cx| match f2b_fut_pinned.as_mut().poll(cx) {
                Poll::Ready(r) => Poll::Ready(Some(r)),
                Poll::Pending => Poll::Ready(None),
            })
            .await
            {
                Some(r) => {
                    // polling once was enough to complete the copy. update the
                    // frontend-to-backend result.
                    f2b_res = Some(r)
                }
                None => {
                    // polling once still didn't complete, so we'll leave it
                    // incomplete.
                }
            }
        }

        let (backend_reader, frontend_writer) = b2f_res?;
        let (frontend_reader, backend_writer) = match f2b_res {
            Some(Ok(r)) => r,
            Some(Err(e)) => return Err(ProxyError::FrontendCopyError(e)),
            None => return Err(ProxyError::FrontendCopyIncomplete),
        };

        Ok((
            ProxyClient {
                frontend_reader,
                frontend_writer,
                options,
            },
            (backend_reader, backend_writer),
        ))
    }
}

/// Headers that are always connection-scoped and must be removed before
/// forwarding. See RFC 9110 and RFC 9112.
///
/// - `Connection` — RFC 9110 7.6.1
/// - `Keep-Alive` — RFC 9112 9.3
/// - `Proxy-Authenticate` — RFC 9110 11.7.1 (consumed by the receiving proxy)
/// - `Proxy-Authorization` — RFC 9110 11.7.2 (consumed by the receiving proxy)
/// - `TE` — RFC 9110 10.1.4 (explicitly defined as a hop-by-hop field)
/// - `Transfer-Encoding` — RFC 9112 6.1 (HTTP/1.1 transfer coding)
/// - `Upgrade` — RFC 9110 7.8
const HOP_BY_HOP_HEADERS: &[&[u8]] = &[
    b"connection",
    b"keep-alive",
    b"proxy-authenticate",
    b"proxy-authorization",
    b"te",
    b"transfer-encoding",
    b"upgrade",
    // `trailer is intentionally absent: the proxy forwards chunked trailer
    // fields end-to-end, so the `trailer` announcement header remains accurate
    // and should be preserved.
];

fn is_standard_hop_by_hop(name: &[u8]) -> bool {
    HOP_BY_HOP_HEADERS
        .iter()
        .any(|h| h.eq_ignore_ascii_case(name))
}

/// Parse the comma-separated token list in a `Connection` header value into
/// individual header name bytes.
fn parse_connection_tokens(value: &[u8]) -> Vec<Bytes> {
    // TODO: let's make this an actual parser eventually
    value
        .split(|&b| b == b',')
        .map(|s| Bytes::copy_from_slice(s.trim_ascii()))
        .filter(|s| !s.is_empty())
        .collect()
}

/// Remove all hop-by-hop headers from `headers` and build the `Via` header
/// (RFC 9110 7.6.3).
///
/// Returns `(hop_by_hop, proxy_headers)` where `hop_by_hop` contains the
/// removed headers and `proxy_headers` contains the headers added by this
/// proxy.
fn process_hop_by_hop(
    headers: &mut Headers,
    version: HttpVersion,
    received_by: &str,
) -> (Headers, Headers) {
    // TODO: this should be two functions: split_out_hop_by_hop_headers and
    // add_proxy_headers.

    // Collect any extra hop-by-hop names listed in the Connection header.
    let connection_tokens = headers
        .get_header("connection")
        .map(|v| parse_connection_tokens(v))
        .unwrap_or_default();

    let hop_by_hop = headers.remove(|(name, _)| {
        is_standard_hop_by_hop(name)
            || connection_tokens
                .iter()
                .any(|t| t.as_ref().eq_ignore_ascii_case(name))
    });

    let via_version = match version {
        HttpVersion::Http10 => "1.0",
        HttpVersion::Http11 => "1.1",
    };
    let mut proxy_headers = Headers::with_capacity(1);
    proxy_headers.push(
        Bytes::from_static(b"Via"),
        Bytes::from(format!("{via_version} {received_by}")),
    );

    (hop_by_hop, proxy_headers)
}

/// Inject a `Via` header (RFC 9110 sec 7.6.3) directly into `response`.
fn insert_proxy_headers(response: &mut Response, version: HttpVersion, received_by: &str) {
    let via_version = match version {
        HttpVersion::Http10 => "1.0",
        HttpVersion::Http11 => "1.1",
    };
    response.headers_mut().push(
        Bytes::from_static(b"Via"),
        Bytes::from(format!("{via_version} {received_by}")),
    );
}

/// A proxy request constructed from a [`FrontendRequest`].
///
/// On construction, all hop-by-hop headers are stripped from the request and
/// stored separately. Proxy headers are built and stored as a distinct set.
///
/// The three header sets exposed by this type are:
///
/// - **original headers**: the end-to-end headers remaining after hop-by-hop
///   removal.
/// - **hop-by-hop headers**: the headers that were removed.
/// - **proxy headers**: headers added by this proxy.
pub struct ProxyRequest<I> {
    frontend: FrontendRequest<I>,
    hop_by_hop: Headers,
    proxy_headers: Headers,
}

impl<I> ProxyRequest<I> {
    /// Construct a [`ProxyRequest`] from a [`FrontendRequest`].
    ///
    /// `received_by` is the proxy identifier inserted into the `Via` header
    /// per RFC 9110 7.6.3 (e.g. a hostname or pseudonym).
    pub fn new(mut frontend: FrontendRequest<I>, received_by: &str) -> Self {
        let version = frontend.req().version();
        let (hop_by_hop, proxy_headers) =
            process_hop_by_hop(frontend.req_mut().headers_mut(), version, received_by);

        Self {
            frontend,
            hop_by_hop,
            proxy_headers,
        }
    }

    /// The underlying [`Request`] with hop-by-hop headers removed.
    pub fn req(&self) -> &Request {
        self.frontend.req()
    }

    /// The underlying [`Request`] as mutable.
    pub fn req_mut(&mut self) -> &mut Request {
        self.frontend.req_mut()
    }

    /// The hop-by-hop headers that were removed from the request on
    /// construction. These are here to be referenced in case they are required
    /// for some other internal proxy state, or if the user wants to add them
    /// back into the backend response.
    pub fn hop_by_hop_headers(&self) -> &Headers {
        &self.hop_by_hop
    }

    /// A reference to headers added by this proxy (e.g. `Via`). This allows the
    /// user to inspect the proxy headers.
    pub fn proxy_headers(&self) -> &Headers {
        &self.proxy_headers
    }

    /// A mutable reference to the proxy headers that will be added to the
    /// request before forwarding to the next hop. This allows the user to add
    /// or remove proxy headers before forwarding.
    pub fn proxy_headers_mut(&mut self) -> &mut Headers {
        &mut self.proxy_headers
    }

    /// Convert the [`ProxyRequest`] into a request with all the hop-by-hop
    /// headers removed, and proxy headers added.
    fn into_backend_request(self) -> (Request, FrontendBodyReader<I>) {
        let Self {
            mut frontend,
            hop_by_hop: _,
            proxy_headers,
        } = self;

        // merge the proxy headers into the request
        let current_headers = frontend.req_mut().headers_mut();
        current_headers.merge(&proxy_headers);

        // then we can return the pieces normally
        frontend.into_parts()
    }
}

/// The proxy-processed form of a regular (non-1xx) backend response.
///
/// The three header sets exposed by this type are:
///
/// - **original headers**: the end-to-end headers remaining after hop-by-hop
///   removal.
/// - **hop-by-hop headers**: the headers that were removed.
/// - **proxy headers**: headers added by this proxy.
pub struct ProxyResponse<I> {
    backend: BackendResponse<I>,
    hop_by_hop: Headers,
    proxy_headers: Headers,
}

impl<I> ProxyResponse<I> {
    /// The underlying [`Response`] with hop-by-hop headers removed.
    pub fn res(&self) -> &Response {
        self.backend.res()
    }

    /// The underlying [`Response`] as mutable.
    pub fn res_mut(&mut self) -> &mut Response {
        self.backend.res_mut()
    }

    /// The hop-by-hop headers that were removed from the response on
    /// construction.
    pub fn hop_by_hop_headers(&self) -> &Headers {
        &self.hop_by_hop
    }

    /// The proxy headers that will be by this proxy (e.g. `Via`).
    pub fn proxy_headers(&self) -> &Headers {
        &self.proxy_headers
    }

    /// A mutable reference to the proxy headers that will be added to the
    /// response before forwarding to the next hop. This allows the user to add
    /// or remove proxy headers before forwarding.
    pub fn proxy_headers_mut(&mut self) -> &mut Headers {
        &mut self.proxy_headers
    }

    /// The end-to-end headers on this response
    pub fn end_to_end_headers(&self) -> &Headers {
        self.res().headers()
    }

    /// Convert the [`ProxyRequest`] into a request with all the hop-by-hop
    /// headers removed, and proxy headers added.
    pub fn into_frontend_response(self) -> (Response, BackendBodyReader<I>) {
        let Self {
            mut backend,
            hop_by_hop: _,
            proxy_headers,
        } = self;

        // merge the proxy headers into the request
        let current_headers = backend.res_mut().headers_mut();
        current_headers.merge(&proxy_headers);

        // having removed hop-by-hop headers, and merged in proxy headers, we
        // can now break the backend into parts and return them as a frontend
        // response
        backend.into_parts()
    }
}

/// The proxy-processed form of a 1xx informational response.
///
/// The three header sets exposed by this type are:
///
/// - **original headers**: the end-to-end headers remaining after hop-by-hop
///   removal.
/// - **hop-by-hop headers**: the headers that were removed.
/// - **proxy headers**: headers added by this proxy.
pub struct ProxyInformationalResponse {
    res: Response,
    hop_by_hop: Headers,
    proxy_headers: Headers,
}

impl ProxyInformationalResponse {
    /// The underlying [`Response`] with hop-by-hop headers removed.
    pub fn res(&self) -> &Response {
        &self.res
    }

    /// The end-to-end headers on this response
    pub fn end_to_end_headers(&self) -> &Headers {
        self.res().headers()
    }

    /// The hop-by-hop headers that were removed from the response on
    /// construction.
    pub fn hop_by_hop_headers(&self) -> &Headers {
        &self.hop_by_hop
    }

    /// The proxy headers that will be by this proxy (e.g. `Via`).
    pub fn proxy_headers(&self) -> &Headers {
        &self.proxy_headers
    }

    /// A mutable reference to the proxy headers that will be added to the
    /// response before forwarding to the next hop. This allows the user to add
    /// or remove proxy headers before forwarding.
    pub fn proxy_headers_mut(&mut self) -> &mut Headers {
        &mut self.proxy_headers
    }

    /// Convert the [`ProxyRequest`] into a request with all the hop-by-hop
    /// headers removed, and proxy headers added.
    pub fn into_frontend_response(self) -> Response {
        let Self {
            mut res,
            hop_by_hop: _,
            proxy_headers,
        } = self;

        // merge the proxy headers into the request
        let current_headers = res.headers_mut();
        current_headers.merge(&proxy_headers);

        // then we can return the pieces normally
        res
    }
}

/// Reads the next response in a stream, producing a [`ProxyResponseStream`].
///
/// Wraps [`BackendStreamReader`] and applies the same hop-by-hop removal and
/// `Via` injection to each response it yields.
pub struct ProxyStreamReader<I> {
    received_by: Box<str>,
    reader: BackendStreamReader<I>,
}

impl<I: AsyncReadExt + Unpin> ProxyStreamReader<I> {
    pub async fn read(self) -> BackendResult<ProxyResponseStream<I>> {
        let stream = self.reader.read().await?;
        Ok(ProxyResponseStream::new(stream, &self.received_by))
    }
}

/// The proxy-processed form of [`ResponseStream`].
///
/// Each variant carries the same three header sets as [`ProxyRequest`]:
/// end-to-end headers remaining on the message, removed hop-by-hop headers,
/// and headers added by this proxy.
pub enum ProxyResponseStream<I> {
    /// A regular (non-1xx) response.
    Response(ProxyResponse<I>),
    /// A 1xx informational response. More responses can be read from the
    /// accompanying [`ProxyStreamReader`].
    Informational(ProxyInformationalResponse, ProxyStreamReader<I>),
}

impl<I> ProxyResponseStream<I> {
    fn new(stream: ResponseStream<I>, received_by: &str) -> Self {
        match stream {
            ResponseStream::Response(mut backend) => {
                let version = backend.res().version();
                let (hop_by_hop, proxy_headers) =
                    process_hop_by_hop(backend.res_mut().headers_mut(), version, received_by);
                ProxyResponseStream::Response(ProxyResponse {
                    backend,
                    hop_by_hop,
                    proxy_headers,
                })
            }
            ResponseStream::Informational(mut res, reader) => {
                let version = res.version();
                let (hop_by_hop, proxy_headers) =
                    process_hop_by_hop(res.headers_mut(), version, received_by);
                ProxyResponseStream::Informational(
                    ProxyInformationalResponse {
                        res,
                        hop_by_hop,
                        proxy_headers,
                    },
                    ProxyStreamReader {
                        received_by: received_by.into(),
                        reader,
                    },
                )
            }
        }
    }
}

impl<I: std::fmt::Debug> std::fmt::Debug for ProxyResponseStream<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Response(_) => write!(f, "Response"),
            Self::Informational(_, _) => write!(f, "Informational"),
        }
    }
}

#[cfg(test)]
mod test {
    use jrpxy_backend::reader::BackendReader;
    use jrpxy_frontend::{reader::FrontendReader, writer::FrontendWriter};
    use jrpxy_pool::{BackendProxyProvider, OneshotBackend};

    use crate::{ProxyClient, ProxyOptions, ReceivedResponseStream};

    use super::{
        ProxyInformationalResponse, ProxyRequest, ProxyResponse, ProxyResponseStream,
        ProxyStreamReader,
    };

    fn into_response<I>(s: ProxyResponseStream<I>) -> ProxyResponse<I> {
        match s {
            ProxyResponseStream::Response(r) => r,
            ProxyResponseStream::Informational(_, _) => panic!("expected Response variant"),
        }
    }

    fn into_informational<I>(
        s: ProxyResponseStream<I>,
    ) -> (ProxyInformationalResponse, ProxyStreamReader<I>) {
        match s {
            ProxyResponseStream::Informational(res, reader) => (res, reader),
            ProxyResponseStream::Response(_) => panic!("expected Informational variant"),
        }
    }

    async fn make_proxy_request(raw: &[u8]) -> ProxyRequest<&[u8]> {
        let reader = FrontendReader::new(raw, 8192);
        let frontend = reader.read().await.expect("valid request");
        ProxyRequest::new(frontend, "proxy.example.com")
    }

    async fn make_proxy_response(raw: &[u8]) -> ProxyResponseStream<&[u8]> {
        let reader = BackendReader::new(raw);
        let stream = reader.read(true, 8192).await.expect("valid response");
        ProxyResponseStream::new(stream, "proxy.example.com")
    }

    #[tokio::test]
    async fn test_oneshot_transaction() {
        let frontend_reader = b"\
            GET / HTTP/1.1\r\n\
            Host: example.com\r\n\
            \r\n";
        let mut frontend_writer = Vec::new();

        let backend_reader = b"\
            HTTP/1.1 100 Continue\r\n\
            x-hdr: 1\r\n\
            connection: filter\r\n\
            filter: 2\r\n\
            \r\n\
            HTTP/1.1 200 Ok\r\n\
            y-hdr: 10\r\n\
            connection: filter-res\r\n\
            filter-res: 20\r\n\
            content-length: 5\r\n\
            \r\n\
            01234";

        let mut backend_writer = Vec::new();

        let mut bp = OneshotBackend::new(backend_reader.as_ref(), &mut backend_writer);

        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref(), 8192),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let did_fe_read = proxy_client.start().await.expect("client start failed");

        let backend_connection = bp.get_connection().await.expect("no backend connection");
        let did_be_write = did_fe_read
            .write_backend_request(backend_connection)
            .await
            .expect("backend write failed");

        let be_res_stat = did_be_write
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        let rbir = match be_res_stat {
            ReceivedResponseStream::Response(_read_backend_response) => {
                panic!("incorrect response type")
            }
            ReceivedResponseStream::Informational(read_backend_informational_response) => {
                read_backend_informational_response
            }
        };

        // Verify we can read the end-to-end header in the informational response
        assert_eq!(
            b"1".as_slice(),
            rbir.as_response()
                .end_to_end_headers()
                .get_header("x-hdr")
                .unwrap()
        );

        // Verify we can read the end-to-end header in the informational response
        assert_eq!(
            b"filter".as_slice(),
            rbir.as_response()
                .hop_by_hop_headers()
                .get_header("connection")
                .unwrap()
        );
        assert_eq!(
            b"2".as_slice(),
            rbir.as_response()
                .hop_by_hop_headers()
                .get_header("filter")
                .unwrap()
        );

        // Ensure the hop-by-hop headers are NOT in the end-to-end headers
        assert!(
            rbir.as_response()
                .end_to_end_headers()
                .get_header("filter")
                .is_none()
        );
        assert!(
            rbir.as_response()
                .end_to_end_headers()
                .get_header("connection")
                .is_none()
        );

        let be_res_stat = rbir
            .forward_informational_response()
            .await
            .expect("failed to forward informational response");

        let rbr = match be_res_stat.into_inner() {
            ReceivedResponseStream::Response(rbr) => rbr,
            ReceivedResponseStream::Informational(_rbir) => {
                panic!("incorrect response type")
            }
        };

        // Verify we can read the end-to-end header in the response
        assert_eq!(
            b"10".as_slice(),
            rbr.as_response()
                .end_to_end_headers()
                .get_header("y-hdr")
                .unwrap()
        );

        // Verify we can read the end-to-end header in the informational response
        assert_eq!(
            b"filter-res".as_slice(),
            rbr.as_response()
                .hop_by_hop_headers()
                .get_header("connection")
                .unwrap()
        );
        assert_eq!(
            b"20".as_slice(),
            rbr.as_response()
                .hop_by_hop_headers()
                .get_header("filter-res")
                .unwrap()
        );

        // Ensure the hop-by-hop headers are NOT in the end-to-end headers
        assert!(
            rbr.as_response()
                .end_to_end_headers()
                .get_header("filter-res")
                .is_none()
        );
        assert!(
            rbr.as_response()
                .end_to_end_headers()
                .get_header("connection")
                .is_none()
        );

        let (client, (backend_reader, backend_writer)) = rbr
            .forward_response()
            .await
            .expect("failed to forward response");
        bp.give_connection(backend_reader, backend_writer);

        // split up the client into pieces, and make sure they all reflect
        // what's expected.
        let (_frontend_reader, frontend_writer) = client.into_parts();
        let frontend_writer = frontend_writer.into_inner();
        let (_backend_reader, backend_writer) = bp.inner.unwrap();
        let backend_writer = backend_writer.into_inner();

        let expected_backend_writer = b"\
            GET / HTTP/1.1\r\n\
            Host: example.com\r\n\
            Via: 1.1 jrpxy\r\n\
            \r\n";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected_backend_writer.as_slice()),
            jrpxy_util::debug::AsciiDebug(&backend_writer)
        );

        let expected_frontend_writer = b"\
            HTTP/1.1 100 Continue\r\n\
            x-hdr: 1\r\n\
            Via: 1.1 jrpxy\r\n\
            \r\n\
            HTTP/1.1 200 Ok\r\n\
            y-hdr: 10\r\n\
            Via: 1.1 jrpxy\r\n\
            content-length: 5\r\n\
            \r\n\
            01234";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected_frontend_writer.as_slice()),
            jrpxy_util::debug::AsciiDebug(&frontend_writer)
        );
    }

    #[tokio::test]
    async fn test_oneshot_transaction_drops_informational_for_http10_client() {
        let frontend_reader = b"\
            GET / HTTP/1.0\r\n\
            Host: example.com\r\n\
            \r\n";
        let mut frontend_writer = Vec::new();

        let backend_reader = b"\
            HTTP/1.1 100 Continue\r\n\
            x-hdr: 1\r\n\
            \r\n\
            HTTP/1.1 200 Ok\r\n\
            content-length: 5\r\n\
            \r\n\
            01234";

        let mut backend_writer = Vec::new();

        let mut bp = OneshotBackend::new(backend_reader.as_ref(), &mut backend_writer);

        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref(), 8192),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let did_fe_read = proxy_client.start().await.expect("client start failed");

        let backend_connection = bp.get_connection().await.expect("no backend connection");
        let did_be_write = did_fe_read
            .write_backend_request(backend_connection)
            .await
            .expect("backend write failed");

        let be_res_stat = did_be_write
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        // First, we get the informational response from the backend
        let rbir = match be_res_stat {
            ReceivedResponseStream::Response(_) => panic!("incorrect response type"),
            ReceivedResponseStream::Informational(rbir) => rbir,
        };

        // We attempt to forward the informational to the HTTP/1.0 client.
        let ifr = rbir
            .forward_informational_response()
            .await
            .expect("failed to forward");
        let be_res_stat = match ifr {
            crate::InformationalForwardResult::Dropped(r) => r,
            crate::InformationalForwardResult::Forwarded(_r) => {
                panic!("forwarded resposne when we expected a drop")
            }
        };

        // Now we can read the next response from the origin. This one is a
        // normal response.
        let rbr = match be_res_stat {
            ReceivedResponseStream::Response(rbr) => rbr,
            ReceivedResponseStream::Informational(_) => panic!("incorrect response type"),
        };

        // Normal responses can be forwarded to HTTP/1.0 clients.
        let (client, _backend_connection) = rbr
            .forward_response()
            .await
            .expect("failed to forward response");

        let (_frontend_reader, frontend_writer) = client.into_parts();
        let frontend_writer = frontend_writer.into_inner();

        // The 1xx informational response must NOT be forwarded to an HTTP/1.0 client.
        let expected_frontend_writer = b"\
            HTTP/1.1 200 Ok\r\n\
            Via: 1.1 jrpxy\r\n\
            content-length: 5\r\n\
            \r\n\
            01234";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected_frontend_writer.as_slice()),
            jrpxy_util::debug::AsciiDebug(&frontend_writer)
        );
    }

    #[tokio::test]
    async fn content_length_backend_response_forwarded() {
        let frontend_reader = b"\
            GET / HTTP/1.1\r\n\
            Host: example.com\r\n\
            \r\n";
        let mut frontend_writer = Vec::new();

        let backend_reader = b"\
            HTTP/1.1 200 Ok\r\n\
            content-length: 5\r\n\
            \r\n\
            hello";
        let mut backend_writer = Vec::new();

        let mut bp = OneshotBackend::new(backend_reader.as_ref(), &mut backend_writer);
        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref(), 8192),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let backend_connection = bp.get_connection().await.expect("no backend connection");
        let be_res_stat = proxy_client
            .start()
            .await
            .expect("client start failed")
            .write_backend_request(backend_connection)
            .await
            .expect("backend write failed")
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        let rbr = match be_res_stat {
            ReceivedResponseStream::Response(rbr) => rbr,
            ReceivedResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, _backend_connection) = rbr
            .forward_response()
            .await
            .expect("failed to forward response");

        let (_frontend_reader, frontend_writer) = client.into_parts();
        let frontend_writer = frontend_writer.into_inner();

        let expected_frontend_writer = b"\
            HTTP/1.1 200 Ok\r\n\
            Via: 1.1 jrpxy\r\n\
            content-length: 5\r\n\
            \r\n\
            hello";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected_frontend_writer.as_slice()),
            jrpxy_util::debug::AsciiDebug(&frontend_writer)
        );
    }

    #[tokio::test]
    async fn content_length_frontend_request_forwarded() {
        let frontend_reader = b"\
            POST / HTTP/1.1\r\n\
            Host: example.com\r\n\
            Content-Length: 5\r\n\
            \r\n\
            hello";
        let mut frontend_writer = Vec::new();

        let backend_reader = b"\
            HTTP/1.1 200 Ok\r\n\
            content-length: 0\r\n\
            \r\n";
        let mut backend_writer = Vec::new();

        let mut bp = OneshotBackend::new(backend_reader.as_ref(), &mut backend_writer);
        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref(), 8192),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let backend_connection = bp.get_connection().await.expect("no backend connection");
        let be_res_stat = proxy_client
            .start()
            .await
            .expect("client start failed")
            .write_backend_request(backend_connection)
            .await
            .expect("backend write failed")
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        let rbr = match be_res_stat {
            ReceivedResponseStream::Response(rbr) => rbr,
            ReceivedResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, (_backend_reader, backend_writer)) = rbr
            .forward_response()
            .await
            .expect("failed to forward response");
        let _ = client;
        let backend_writer = backend_writer.into_inner();

        let expected_backend_writer = b"\
            POST / HTTP/1.1\r\n\
            Host: example.com\r\n\
            Via: 1.1 jrpxy\r\n\
            content-length: 5\r\n\
            \r\n\
            hello";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected_backend_writer.as_slice()),
            jrpxy_util::debug::AsciiDebug(&backend_writer)
        );
    }

    #[tokio::test]
    async fn chunked_frontend_request_forwarded() {
        let frontend_reader = b"\
            POST / HTTP/1.1\r\n\
            Host: example.com\r\n\
            Transfer-Encoding: chunked\r\n\
            \r\n\
            5\r\n\
            hello\r\n\
            0\r\n\
            \r\n";
        let mut frontend_writer = Vec::new();

        let backend_reader = b"\
            HTTP/1.1 200 Ok\r\n\
            content-length: 0\r\n\
            \r\n";
        let mut backend_writer = Vec::new();

        let mut bp = OneshotBackend::new(backend_reader.as_ref(), &mut backend_writer);
        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref(), 8192),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let backend_connection = bp.get_connection().await.expect("no backend connection");
        let be_res_stat = proxy_client
            .start()
            .await
            .expect("client start failed")
            .write_backend_request(backend_connection)
            .await
            .expect("backend write failed")
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        let rbr = match be_res_stat {
            ReceivedResponseStream::Response(rbr) => rbr,
            ReceivedResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (_client, (_backend_reader, backend_writer)) = rbr
            .forward_response()
            .await
            .expect("failed to forward response");
        let backend_writer = backend_writer.into_inner();

        let expected_backend_writer = b"\
            POST / HTTP/1.1\r\n\
            Host: example.com\r\n\
            Via: 1.1 jrpxy\r\n\
            transfer-encoding: chunked\r\n\
            \r\n\
            5\r\n\
            hello\r\n\
            0\r\n\
            \r\n";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected_backend_writer.as_slice()),
            jrpxy_util::debug::AsciiDebug(&backend_writer)
        );
    }

    #[tokio::test]
    async fn chunked_frontend_request_trailers_forwarded() {
        let frontend_reader = b"\
            POST / HTTP/1.1\r\n\
            Host: example.com\r\n\
            Transfer-Encoding: chunked\r\n\
            \r\n\
            5\r\n\
            hello\r\n\
            0\r\n\
            x-trailer: somevalue\r\n\
            \r\n";
        let mut frontend_writer = Vec::new();

        let backend_reader = b"\
            HTTP/1.1 200 Ok\r\n\
            content-length: 0\r\n\
            \r\n";
        let mut backend_writer = Vec::new();

        let mut bp = OneshotBackend::new(backend_reader.as_ref(), &mut backend_writer);
        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref(), 8192),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let backend_connection = bp.get_connection().await.expect("no backend connection");
        let be_res_stat = proxy_client
            .start()
            .await
            .expect("client start failed")
            .write_backend_request(backend_connection)
            .await
            .expect("backend write failed")
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        let rbr = match be_res_stat {
            ReceivedResponseStream::Response(rbr) => rbr,
            ReceivedResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (_client, (_backend_reader, backend_writer)) = rbr
            .forward_response()
            .await
            .expect("failed to forward response");
        let backend_writer = backend_writer.into_inner();

        let expected_backend_writer = b"\
            POST / HTTP/1.1\r\n\
            Host: example.com\r\n\
            Via: 1.1 jrpxy\r\n\
            transfer-encoding: chunked\r\n\
            \r\n\
            5\r\n\
            hello\r\n\
            0\r\n\
            x-trailer: somevalue\r\n\
            \r\n";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected_backend_writer.as_slice()),
            jrpxy_util::debug::AsciiDebug(&backend_writer)
        );
    }

    #[tokio::test]
    async fn chunked_backend_response_forwarded() {
        // Demonstrates that the proxy correctly forwards a chunked backend
        // response body to the frontend. This is a prerequisite for trailer
        // forwarding (see chunked_trailers_forwarded).
        let frontend_reader = b"\
            GET / HTTP/1.1\r\n\
            Host: example.com\r\n\
            \r\n";
        let mut frontend_writer = Vec::new();

        let backend_reader = b"\
            HTTP/1.1 200 Ok\r\n\
            transfer-encoding: chunked\r\n\
            \r\n\
            5\r\n\
            hello\r\n\
            0\r\n\
            \r\n";
        let mut backend_writer = Vec::new();

        let mut bp = OneshotBackend::new(backend_reader.as_ref(), &mut backend_writer);
        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref(), 8192),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let backend_connection = bp.get_connection().await.expect("no backend connection");
        let be_res_stat = proxy_client
            .start()
            .await
            .expect("client start failed")
            .write_backend_request(backend_connection)
            .await
            .expect("backend write failed")
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        let rbr = match be_res_stat {
            ReceivedResponseStream::Response(rbr) => rbr,
            ReceivedResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, _backend_connection) = rbr
            .forward_response()
            .await
            .expect("failed to forward response");

        let (_frontend_reader, frontend_writer) = client.into_parts();
        let frontend_writer = frontend_writer.into_inner();

        let expected_frontend_writer = b"\
            HTTP/1.1 200 Ok\r\n\
            Via: 1.1 jrpxy\r\n\
            transfer-encoding: chunked\r\n\
            \r\n\
            5\r\n\
            hello\r\n\
            0\r\n\
            \r\n";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected_frontend_writer.as_slice()),
            jrpxy_util::debug::AsciiDebug(&frontend_writer)
        );
    }

    #[tokio::test]
    async fn chunked_backend_response_trailers_forwarded() {
        let frontend_reader = b"\
            GET / HTTP/1.1\r\n\
            Host: example.com\r\n\
            \r\n";
        let mut frontend_writer = Vec::new();

        // Backend sends a chunked response with a trailer field.
        let backend_reader = b"\
            HTTP/1.1 200 Ok\r\n\
            transfer-encoding: chunked\r\n\
            \r\n\
            5\r\n\
            hello\r\n\
            0\r\n\
            x-trailer: somevalue\r\n\
            \r\n";
        let mut backend_writer = Vec::new();

        let mut bp = OneshotBackend::new(backend_reader.as_ref(), &mut backend_writer);
        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref(), 8192),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let backend_connection = bp.get_connection().await.expect("no backend connection");
        let be_res_stat = proxy_client
            .start()
            .await
            .expect("client start failed")
            .write_backend_request(backend_connection)
            .await
            .expect("backend write failed")
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        let rbr = match be_res_stat {
            ReceivedResponseStream::Response(rbr) => rbr,
            ReceivedResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, _backend_connection) = rbr
            .forward_response()
            .await
            .expect("failed to forward response");

        let (_frontend_reader, frontend_writer) = client.into_parts();
        let frontend_writer = frontend_writer.into_inner();

        // The trailer field must appear between the terminal chunk and the
        // final CRLF, i.e. `0\r\nx-trailer: somevalue\r\n\r\n`.
        let expected_frontend_writer = b"\
            HTTP/1.1 200 Ok\r\n\
            Via: 1.1 jrpxy\r\n\
            transfer-encoding: chunked\r\n\
            \r\n\
            5\r\n\
            hello\r\n\
            0\r\n\
            x-trailer: somevalue\r\n\
            \r\n";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected_frontend_writer.as_slice()),
            jrpxy_util::debug::AsciiDebug(&frontend_writer)
        );
    }

    /// RFC 9110 15.4.5 / RFC 9112 6.3: a 304 Not Modified response MUST NOT
    /// include a message body, but MAY include headers such as `content-length`
    /// and `transfer-encoding` to indicate what the representation would have
    /// looked like. The proxy must forward `content-length` to the client and
    /// must not read any body bytes from the backend connection.
    #[tokio::test]
    async fn not_modified_response_preserves_content_length() {
        let frontend_reader = b"\
            GET / HTTP/1.1\r\n\
            Host: example.com\r\n\
            If-None-Match: \"abc\"\r\n\
            \r\n\
            GET / HTTP/1.1\r\n\
            Host: example.com\r\n\
            \r\n";
        let mut frontend_writer = Vec::new();

        // Backend sends 304 with content-length but no body, followed by a
        // second response to prove the connection is left in a clean state.
        let backend_reader = b"\
            HTTP/1.1 304 Not Modified\r\n\
            content-length: 512\r\n\
            etag: \"abc\"\r\n\
            \r\n\
            HTTP/1.1 200 Ok\r\n\
            content-length: 5\r\n\
            \r\n\
            hello";
        let mut backend_writer = Vec::new();

        let mut bp = OneshotBackend::new(backend_reader.as_ref(), &mut backend_writer);
        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref(), 8192),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let backend_connection = bp.get_connection().await.expect("no backend connection");
        let be_res_stat = proxy_client
            .start()
            .await
            .expect("client start failed")
            .write_backend_request(backend_connection)
            .await
            .expect("backend write failed")
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        let rbr = match be_res_stat {
            ReceivedResponseStream::Response(rbr) => rbr,
            ReceivedResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, backend_connection) = rbr
            .forward_response()
            .await
            .expect("failed to forward response");

        // Release the mutable borrow on frontend_writer before asserting.
        let (frontend_reader, _) = client.into_parts();

        // content-length must be forwarded (RFC 9110 §15.4.5).
        let expected = b"\
            HTTP/1.1 304 Not Modified\r\n\
            content-length: 512\r\n\
            etag: \"abc\"\r\n\
            Via: 1.1 jrpxy\r\n\
            \r\n";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected.as_slice()),
            jrpxy_util::debug::AsciiDebug(frontend_writer.as_slice()),
        );

        // The backend connection must be positioned right at the start of the
        // next response — no body bytes were consumed from the 304.
        let mut frontend_writer = Vec::new();
        let proxy_client = ProxyClient::new(
            frontend_reader,
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let be_res_stat = proxy_client
            .start()
            .await
            .expect("second start failed")
            .write_backend_request(backend_connection)
            .await
            .expect("second backend write failed")
            .read_backend_response()
            .await
            .expect("second read failed");

        let rbr = match be_res_stat {
            ReceivedResponseStream::Response(rbr) => rbr,
            ReceivedResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, _) = rbr.forward_response().await.expect("second forward failed");

        let (_, fw) = client.into_parts();

        // For a normal response with a body the proxy rewrites the framing
        // header, so content-length is appended after Via rather than
        // preserved in its original position.
        let expected_second = b"\
            HTTP/1.1 200 Ok\r\n\
            Via: 1.1 jrpxy\r\n\
            content-length: 5\r\n\
            \r\nhello";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected_second.as_slice()),
            jrpxy_util::debug::AsciiDebug(fw.as_inner()),
        );
    }

    /// A HEAD request must receive a bodyless response. The origin correctly
    /// sends no body but includes a `content-length` header indicating the size
    /// of the representation. The proxy must forward the header (so the client
    /// knows the body size) without writing any body bytes.
    ///
    /// To be sure framing is handled correctly, we pipeline a request with a
    /// GET request that is otherwise identical to the first HEAD request.
    #[tokio::test]
    async fn head_request_response_forwarded_without_body() {
        let frontend_reader = b"\
            HEAD / HTTP/1.1\r\n\
            Host: example.com\r\n\
            \r\n\
            GET / HTTP/1.1\r\n\
            Host: example.com\r\n\
            \r\n";
        let mut frontend_writer = Vec::new();

        // Origin correctly omits the body but includes content-length.
        let backend_reader = b"\
            HTTP/1.1 200 Ok\r\n\
            content-length: 5\r\n\
            \r\n\
            HTTP/1.1 200 Ok\r\n\
            content-length: 5\r\n\
            \r\n\
            01234";
        let mut backend_writer = Vec::new();

        let mut bp = OneshotBackend::new(backend_reader.as_ref(), &mut backend_writer);
        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref(), 8192),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let backend_connection = bp.get_connection().await.expect("no backend connection");
        let be_res_stat = proxy_client
            .start()
            .await
            .expect("client start failed")
            .write_backend_request(backend_connection)
            .await
            .expect("backend write failed")
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        let rbr = match be_res_stat {
            ReceivedResponseStream::Response(rbr) => rbr,
            ReceivedResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, backend_connection) = rbr
            .forward_response()
            .await
            .expect("failed to forward response");

        let (frontend_reader, frontend_writer) = client.into_parts();

        // The content-length header must be preserved (the client needs to
        // know the representation size) but no body bytes must follow.
        let expected_frontend_writer = b"\
            HTTP/1.1 200 Ok\r\n\
            content-length: 5\r\n\
            Via: 1.1 jrpxy\r\n\
            \r\n";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected_frontend_writer.as_slice()),
            jrpxy_util::debug::AsciiDebug(frontend_writer.as_inner())
        );

        // now run another ProxyClient; this time we expect a response body. We
        // make a new frontend writer so we can distinguish the previous
        // response from the next response. The backend connection is reused
        // directly from the first cycle's return value.
        let mut frontend_writer = Vec::new();
        let proxy_client = ProxyClient::new(
            frontend_reader,
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let be_res_stat = proxy_client
            .start()
            .await
            .expect("client start failed")
            .write_backend_request(backend_connection)
            .await
            .expect("backend write failed")
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        let rbr = match be_res_stat {
            ReceivedResponseStream::Response(rbr) => rbr,
            ReceivedResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, _backend_connection) = rbr
            .forward_response()
            .await
            .expect("failed to forward response");

        let (_frontend_reader, frontend_writer) = client.into_parts();
        let frontend_writer = frontend_writer.into_inner();

        // The content-length header must be preserved (the client needs to
        // know the representation size) but no body bytes must follow.
        let expected_frontend_writer = b"\
            HTTP/1.1 200 Ok\r\n\
            Via: 1.1 jrpxy\r\n\
            content-length: 5\r\n\
            \r\n\
            01234";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected_frontend_writer.as_slice()),
            jrpxy_util::debug::AsciiDebug(&frontend_writer)
        );
    }

    #[tokio::test]
    async fn removes_standard_hop_by_hop_headers() {
        let raw = b"\
            GET / HTTP/1.1\r\n\
            Host: example.com\r\n\
            Connection: keep-alive\r\n\
            Keep-Alive: timeout=5\r\n\
            Transfer-Encoding: chunked\r\n\
            \r\n\
            0\r\n\
            \r\n";

        let pr = make_proxy_request(raw).await;

        let names: Vec<_> = pr.req().headers().iter().map(|(n, _)| n.clone()).collect();
        assert!(names.iter().all(|n| !n.eq_ignore_ascii_case(b"connection")));
        assert!(names.iter().all(|n| !n.eq_ignore_ascii_case(b"keep-alive")));
        assert!(
            names
                .iter()
                .all(|n| !n.eq_ignore_ascii_case(b"transfer-encoding"))
        );
        assert!(names.iter().any(|n| n.eq_ignore_ascii_case(b"host")));

        let hop: Vec<_> = pr
            .hop_by_hop_headers()
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        assert!(hop.iter().any(|n| n.eq_ignore_ascii_case(b"connection")));
        assert!(hop.iter().any(|n| n.eq_ignore_ascii_case(b"keep-alive")));
        assert!(
            hop.iter()
                .any(|n| n.eq_ignore_ascii_case(b"transfer-encoding"))
        );
    }

    #[tokio::test]
    async fn removes_connection_listed_headers() {
        let raw = b"\
            GET / HTTP/1.1\r\n\
            Host: example.com\r\n\
            Connection: x-custom-hop\r\n\
            X-Custom-Hop: value\r\n\
            \r\n";

        let pr = make_proxy_request(raw).await;

        let names: Vec<_> = pr.req().headers().iter().map(|(n, _)| n.clone()).collect();
        assert!(
            names
                .iter()
                .all(|n| !n.eq_ignore_ascii_case(b"x-custom-hop"))
        );

        let hop: Vec<_> = pr
            .hop_by_hop_headers()
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        assert!(hop.iter().any(|n| n.eq_ignore_ascii_case(b"x-custom-hop")));
    }

    #[tokio::test]
    async fn adds_via_header_to_request() {
        let raw = b"\
            GET / HTTP/1.1\r\n\
            Host: example.com\r\n\
            \r\n";

        let pr = make_proxy_request(raw).await;

        let via = pr
            .proxy_headers()
            .get_header("via")
            .expect("Via must be present");
        assert_eq!(via.as_ref(), b"1.1 proxy.example.com");
    }

    #[tokio::test]
    async fn request_via_version_http10() {
        let raw = b"\
            GET / HTTP/1.0\r\n\
            Host: example.com\r\n\
            \r\n";

        let pr = make_proxy_request(raw).await;

        let via = pr
            .proxy_headers()
            .get_header("via")
            .expect("Via must be present");
        assert_eq!(via.as_ref(), b"1.0 proxy.example.com");
    }

    #[tokio::test]
    async fn removes_hop_by_hop_from_response() {
        let raw = b"\
            HTTP/1.1 200 Ok\r\n\
            Connection: keep-alive\r\n\
            Keep-Alive: timeout=5\r\n\
            Transfer-Encoding: chunked\r\n\
            X-Custom: keep-me\r\n\
            \r\n\
            0\r\n\
            \r\n";

        let pr = into_response(make_proxy_response(raw).await);

        let names: Vec<_> = pr.res().headers().iter().map(|(n, _)| n.clone()).collect();
        assert!(names.iter().all(|n| !n.eq_ignore_ascii_case(b"connection")));
        assert!(names.iter().all(|n| !n.eq_ignore_ascii_case(b"keep-alive")));
        assert!(
            names
                .iter()
                .all(|n| !n.eq_ignore_ascii_case(b"transfer-encoding"))
        );
        assert!(names.iter().any(|n| n.eq_ignore_ascii_case(b"x-custom")));

        let hop: Vec<_> = pr
            .hop_by_hop_headers()
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        assert!(hop.iter().any(|n| n.eq_ignore_ascii_case(b"connection")));
        assert!(hop.iter().any(|n| n.eq_ignore_ascii_case(b"keep-alive")));
        assert!(
            hop.iter()
                .any(|n| n.eq_ignore_ascii_case(b"transfer-encoding"))
        );
    }

    #[tokio::test]
    async fn removes_connection_listed_headers_from_response() {
        let raw = b"\
            HTTP/1.1 200 Ok\r\n\
            Connection: x-server-hop\r\n\
            X-Server-Hop: value\r\n\
            X-Keep: value\r\n\
            \r\n";

        let pr = into_response(make_proxy_response(raw).await);

        let names: Vec<_> = pr.res().headers().iter().map(|(n, _)| n.clone()).collect();
        assert!(
            names
                .iter()
                .all(|n| !n.eq_ignore_ascii_case(b"x-server-hop"))
        );
        assert!(names.iter().any(|n| n.eq_ignore_ascii_case(b"x-keep")));

        let hop: Vec<_> = pr
            .hop_by_hop_headers()
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        assert!(hop.iter().any(|n| n.eq_ignore_ascii_case(b"x-server-hop")));
    }

    #[tokio::test]
    async fn adds_via_header_to_response() {
        let raw = b"\
            HTTP/1.1 200 Ok\r\n\
            Content-Length: 0\r\n\
            \r\n";

        let pr = into_response(make_proxy_response(raw).await);

        let via = pr
            .proxy_headers()
            .get_header("via")
            .expect("Via must be present");
        assert_eq!(via.as_ref(), b"1.1 proxy.example.com");
    }

    #[tokio::test]
    async fn informational_response_hop_by_hop_and_via() {
        // A 100 Continue with a hop-by-hop header, followed by the real response.
        let raw = b"\
            HTTP/1.1 100 Continue\r\n\
            Connection: x-interim-hop\r\n\
            X-Interim-Hop: val\r\n\
            X-Hint: keep\r\n\
            \r\n\
            HTTP/1.1 200 Ok\r\n\
            Content-Length: 0\r\n\
            \r\n";

        let reader = BackendReader::new(raw.as_slice());
        let stream = reader.read(true, 8192).await.expect("valid response");
        let proxy_stream = ProxyResponseStream::new(stream, "proxy.example.com");

        let (info, next_reader) = into_informational(proxy_stream);

        // hop-by-hop headers removed from 1xx
        let info_names: Vec<_> = info
            .res()
            .headers()
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        assert!(
            info_names
                .iter()
                .all(|n| !n.eq_ignore_ascii_case(b"connection"))
        );
        assert!(
            info_names
                .iter()
                .all(|n| !n.eq_ignore_ascii_case(b"x-interim-hop"))
        );
        assert!(info_names.iter().any(|n| n.eq_ignore_ascii_case(b"x-hint")));

        // hop-by-hop set is populated
        let info_hop: Vec<_> = info
            .hop_by_hop_headers()
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        assert!(
            info_hop
                .iter()
                .any(|n| n.eq_ignore_ascii_case(b"connection"))
        );
        assert!(
            info_hop
                .iter()
                .any(|n| n.eq_ignore_ascii_case(b"x-interim-hop"))
        );

        // Via is injected into the 1xx
        let via = info
            .proxy_headers()
            .get_header("via")
            .expect("Via must be present");
        assert_eq!(via.as_ref(), b"1.1 proxy.example.com");

        // The following final response is also processed
        let final_stream = next_reader.read().await.expect("read final response");
        let final_res = into_response(final_stream);

        let via = final_res
            .proxy_headers()
            .get_header("via")
            .expect("Via must be present");
        assert_eq!(via.as_ref(), b"1.1 proxy.example.com");
    }

    #[tokio::test]
    async fn multiple_informational_responses() {
        let raw = b"\
            HTTP/1.1 100 Continue\r\n\
            X-Seq: 1\r\n\
            \r\n\
            HTTP/1.1 103 Early Hints\r\n\
            X-Seq: 2\r\n\
            \r\n\
            HTTP/1.1 200 Ok\r\n\
            X-Seq: 3\r\n\
            \r\n";

        let reader = BackendReader::new(raw.as_slice());
        let stream = reader.read(true, 8192).await.expect("valid response");
        let proxy_stream = ProxyResponseStream::new(stream, "proxy.example.com");

        let (info1, r1) = into_informational(proxy_stream);
        assert_eq!(info1.res().code(), 100);

        let (info2, r2) = into_informational(r1.read().await.expect("read 2"));
        assert_eq!(info2.res().code(), 103);

        let final_res = into_response(r2.read().await.expect("read final"));
        assert_eq!(final_res.res().code(), 200);
    }
}
