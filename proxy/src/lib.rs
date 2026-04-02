use std::borrow::Cow;
use std::future::Future;
use std::task::Poll;

use bytes::Bytes;
use jrpxy_backend::{
    error::{BackendError, BackendResult},
    reader::{
        BackendBodyReader, BackendBodyReaderKind, BackendReader,
        BackendResponse as BackendReaderResponse, BackendStreamReader, ResponseStream,
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
    header::Headers,
    message::{Request, Response},
    version::HttpVersion,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub mod error;
pub use error::{ProxyCopyError, ProxyError, ProxyResult};

pub use crate::error::ProxyFrontendError;

/// Options used to govern the behavior of a [`ProxyClient`]
#[derive(Debug)]
pub struct ProxyOptions {
    pub max_frontend_head_length: usize,
    pub max_backend_head_length: usize,
    pub body_chunk_size: usize,
    /// TODO: make a builder for ProxyOptions so that we can validate
    /// `received_by` doesn't contain something like \r\n which would do bad
    /// things when inserting it into the `Via` header.
    pub received_by: Cow<'static, str>,
}

impl Default for ProxyOptions {
    fn default() -> Self {
        Self {
            max_frontend_head_length: 8192,
            max_backend_head_length: 8192,
            body_chunk_size: 8192,
            received_by: Cow::Borrowed("jrpxy"),
        }
    }
}

/// Initial state of a proxied request. It transitions to either
/// [`FrontendProxyRequest`] or [`FrontendRequestError`] by calling the
/// [`ProxyClient::start()`] method.
///
/// The contained frontend writer is fully flushed, and the frontend reader is
/// expected to be fully drained. Assuming a well behaved client and server, it
/// is safe to extract the reader and writer using
/// [`ProxyClient::into_parts()`].
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
    /// Create a new [`ProxyClient`] from a frontend reader that is aligned to
    /// the start of a new request, and a frontend writer that is fully flushed.
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

    /// Start the proxy transaction by attempting to read and verify a request
    /// from the frontend reader. Returns a [`FrontendProxyRequest`] on success,
    /// or [`FrontendRequestError`] on failure.
    pub async fn start(self) -> Result<FrontendProxyRequest<FR, FW>, FrontendRequestError<FR, FW>> {
        let Self {
            frontend_reader,
            frontend_writer,
            options,
        } = self;

        let req = match frontend_reader.read(options.max_frontend_head_length).await {
            Ok(req) => req,
            Err(error) => {
                return Err(FrontendRequestError::new_frontend(
                    frontend_writer,
                    options,
                    HttpVersion::Http11,
                    error,
                ));
            }
        };

        let is_head = req.req().method() == b"HEAD".as_slice();
        let version = req.req().version();
        let pending = PendingFrontendResponse {
            frontend_writer,
            options,
            client_options: ClientOptions { is_head, version },
        };
        let frontend_request = FrontendProxyRequest::new(req, pending);

        let method = frontend_request.req().method();
        if method == b"CONNECT".as_slice() {
            return Err(FrontendRequestError::new_request(
                frontend_request,
                ProxyFrontendError::ConnectNotSupported,
            ));
        }
        if method == b"TRACE".as_slice() {
            return Err(FrontendRequestError::new_request(
                frontend_request,
                ProxyFrontendError::TraceNotSupported,
            ));
        }

        Ok(frontend_request)
    }
}

/// A valid request has been received from the frontend. We can forward this
/// Returned when [`ProxyClient::start`] fails.
pub enum FrontendRequestError<FR, FW> {
    /// The frontend sent a request that could not be parsed.
    Frontend(FrontendRequestErrorKindRead<FW>),
    /// The frontend sent a valid request that the proxy refuses to handle.
    Request(FrontendRequestErrorKindRequest<FR, FW>),
}

impl<FR, FW> FrontendRequestError<FR, FW> {
    /// Convenience method to construct a new request error.
    fn new_request(
        frontend_request: FrontendProxyRequest<FR, FW>,
        error: ProxyFrontendError,
    ) -> FrontendRequestError<FR, FW> {
        Self::Request(FrontendRequestErrorKindRequest {
            reader: frontend_request,
            error,
        })
    }

    fn new_frontend(
        frontend_writer: FrontendWriter<FW>,
        options: ProxyOptions,
        version: HttpVersion,
        error: FrontendError,
    ) -> FrontendRequestError<FR, FW> {
        FrontendRequestError::Frontend(FrontendRequestErrorKindRead {
            pending: PendingFrontendResponse {
                frontend_writer,
                options,
                client_options: ClientOptions {
                    is_head: false,
                    version,
                },
            },
            error,
        })
    }
}

impl<FR, FW> std::fmt::Debug for FrontendRequestError<FR, FW> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Frontend(arg0) => f.debug_tuple("Frontend").field(&arg0.error).finish(),
            Self::Request(arg0) => f.debug_tuple("Request").field(&arg0.error).finish(),
        }
    }
}

impl<FR, FW> From<FrontendRequestError<FR, FW>> for ProxyError {
    fn from(value: FrontendRequestError<FR, FW>) -> Self {
        match value {
            FrontendRequestError::Frontend(read) => ProxyError::FrontendError(read.error),
            FrontendRequestError::Request(request) => ProxyError::ProxyFrontend(request.error),
        }
    }
}

/// An error occurred while reading the request from the frontend. We cannot
/// recover the reader, but we can write something back.
pub struct FrontendRequestErrorKindRead<FW> {
    pending: PendingFrontendResponse<FW>,
    error: FrontendError,
}

impl<FW> FrontendRequestErrorKindRead<FW> {
    pub fn error(&self) -> &FrontendError {
        &self.error
    }

    /// Consumes the error and returns a [`PendingFrontendResponse`] so the
    /// caller can send an error response to the frontend client.
    pub fn into_pending_response(self) -> PendingFrontendResponse<FW> {
        let Self { pending, error: _ } = self;
        pending
    }
}

/// A valid request was received from the frontend, but there is a semantic
/// problem with that request. We can still send back an error message,
/// and optionally drain the body so that we can keep the connection open.
pub struct FrontendRequestErrorKindRequest<FR, FW> {
    reader: FrontendProxyRequest<FR, FW>,
    error: ProxyFrontendError,
}

impl<FR, FW> FrontendRequestErrorKindRequest<FR, FW> {
    pub fn error(&self) -> &ProxyFrontendError {
        &self.error
    }

    /// Consumes the error and returns the [`FrontendProxyRequest`] so the
    /// caller can inspect the request, send an error response, or drain the
    /// body for connection reuse.
    pub fn into_frontend_request(self) -> FrontendProxyRequest<FR, FW> {
        let Self { reader, error: _ } = self;
        reader
    }
}

/// Returned when writing the request head to the backend fails.
///
/// The failed backend connection is discarded. The request is preserved so
/// the caller can retry with a different backend via
/// [`into_backend_request`] or send an error response to the frontend.
///
/// [`into_backend_request`]: BackendRequestError::into_backend_request
pub struct BackendRequestError<FR, FW> {
    request: BackendProxyRequest<FR, FW>,
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

    /// Consumes the error and returns the [`BackendProxyRequest`] so the caller
    /// can retry with a different backend or send an error response to the
    /// frontend via [`BackendProxyRequest::into_pending_response`].
    pub fn into_backend_request(self) -> BackendProxyRequest<FR, FW> {
        let Self { request, error: _ } = self;
        request
    }
}

/// Returned when reading the response head from the backend fails.
///
/// The broken backend connection is discarded. The request is preserved so
/// the caller can retry with a different backend via
/// [`into_backend_request`] or send an error response to the frontend.
///
/// [`into_backend_request`]: BackendResponseError::into_backend_request
pub struct BackendResponseError<FR, FW> {
    request: BackendProxyRequest<FR, FW>,
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

    /// Consumes the error and returns the [`BackendProxyRequest`] so the caller
    /// can retry with a different backend or send an error response to the
    /// frontend via [`BackendProxyRequest::into_pending_response`].
    pub fn into_backend_request(self) -> BackendProxyRequest<FR, FW> {
        let Self { request, error: _ } = self;
        request
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
        let Self {
            frontend_body_reader,
            pending,
            error: _,
        } = self;
        (pending, frontend_body_reader)
    }
}

/// A frontend connection that has not yet received a response.
///
/// This type is intentionally distinct from a reusable [`FrontendWriter`] to
/// make it clear that a response still needs to be sent before the connection
/// can be reused or closed.
///
/// The required proxy headers (e.g. `Via`) are injected automatically by each
/// `send_as_*` method - the caller does not need to add them manually.
pub struct PendingFrontendResponse<FW> {
    frontend_writer: FrontendWriter<FW>,
    options: ProxyOptions,
    client_options: ClientOptions,
}

impl<FW: AsyncWriteExt + Unpin> PendingFrontendResponse<FW> {
    /// Send a 1xx informational response to the client. Proxy headers are
    /// injected automatically.
    ///
    /// Unlike the terminal `send_as_*` methods, this returns a fresh
    /// [`PendingFrontendResponse`] because a 1xx response is non-terminal -
    /// the frontend is still awaiting the final response to the same request.
    pub async fn send_informational(
        self,
        mut response: Response,
    ) -> Result<PendingFrontendResponse<FW>, FrontendError> {
        let Self {
            frontend_writer,
            options,
            client_options,
        } = self;
        let version = response.version();
        insert_proxy_headers(response.headers_mut(), version, &options.received_by);
        let body_writer = frontend_writer.send_as_no_content(&response).await?;
        let frontend_writer = body_writer.finish().await?;
        Ok(PendingFrontendResponse {
            frontend_writer,
            options,
            client_options,
        })
    }

    /// Send the response with a chunked body. Proxy headers are injected
    /// automatically.
    pub async fn send_as_chunked(
        self,
        mut response: Response,
    ) -> Result<(ProxyOptions, FrontendBodyWriter<FW>), FrontendError> {
        let Self {
            frontend_writer,
            options,
            client_options: _,
        } = self;
        let version = response.version();
        insert_proxy_headers(response.headers_mut(), version, &options.received_by);
        let body_writer = frontend_writer.send_as_chunked(&response).await?;
        Ok((options, body_writer))
    }

    /// Send the response with a content-length delimited body. Proxy headers
    /// are injected automatically.
    pub async fn send_as_content_length(
        self,
        mut response: Response,
        body_len: u64,
    ) -> Result<(ProxyOptions, FrontendBodyWriter<FW>), FrontendError> {
        let Self {
            frontend_writer,
            options,
            client_options: _,
        } = self;
        let version = response.version();
        insert_proxy_headers(response.headers_mut(), version, &options.received_by);
        let body_writer = frontend_writer
            .send_as_content_length(&response, body_len)
            .await?;
        Ok((options, body_writer))
    }

    /// Send the response with no body, preserving any framing headers from
    /// the origin (for `HEAD` and `304 Not Modified`). Proxy headers are
    /// injected automatically.
    pub async fn send_as_bodyless_keep_framing(
        self,
        mut response: Response,
    ) -> Result<(ProxyOptions, FrontendBodyWriter<FW>), FrontendError> {
        let Self {
            frontend_writer,
            options,
            client_options: _,
        } = self;
        let version = response.version();
        insert_proxy_headers(response.headers_mut(), version, &options.received_by);
        let body_writer = frontend_writer
            .send_as_bodyless_keep_framing(&response)
            .await?;
        Ok((options, body_writer))
    }

    /// Send the response with no body and no framing headers (for
    /// `204 No Content`). Proxy headers are injected automatically.
    ///
    /// For 1xx informational responses use [`send_informational`] instead.
    ///
    /// [`send_informational`]: PendingFrontendResponse::send_informational
    pub async fn send_as_no_content(
        self,
        mut response: Response,
    ) -> Result<(ProxyOptions, FrontendBodyWriter<FW>), FrontendError> {
        let Self {
            frontend_writer,
            options,
            client_options: _,
        } = self;
        let version = response.version();
        insert_proxy_headers(response.headers_mut(), version, &options.received_by);
        let body_writer = frontend_writer.send_as_no_content(&response).await?;
        Ok((options, body_writer))
    }
}

/// Error returned by [`BackendInformationalResponse::forward_informational_response`].
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

type BackendConnection<BR, BW> = (BackendReader<BR>, BackendWriter<BW>);

#[derive(Clone, Copy, Debug)]
struct ClientOptions {
    is_head: bool,
    version: HttpVersion,
}

pub struct ResponseReader<FR, FW, BR, BW> {
    request: BackendProxyRequest<FR, FW>,
    backend_reader: BackendReader<BR>,
    backend_body_writer: BackendBodyWriter<BW>,
}

pub enum BackendResponseStream<FR, FW, BR, BW> {
    Informational(BackendInformationalResponse<FR, FW, BR, BW>),
    Response(BackendResponse<FR, FW, BR, BW>),
}

impl<FR, FW, BR, BW> ResponseReader<FR, FW, BR, BW>
where
    BR: AsyncReadExt + Unpin,
    BW: AsyncWriteExt + Unpin,
{
    pub async fn read_backend_response(
        self,
    ) -> Result<BackendResponseStream<FR, FW, BR, BW>, BackendResponseError<FR, FW>> {
        let Self {
            request,
            backend_reader,
            backend_body_writer,
        } = self;

        let response_stream = match backend_reader
            .read(
                !request.pending.client_options.is_head,
                request.pending.options.max_backend_head_length,
            )
            .await
        {
            Ok(r) => r,
            Err(error) => {
                // backend_reader and backend_body_writer are both discarded;
                // the backend connection is broken.
                drop(backend_body_writer);
                return Err(BackendResponseError { request, error });
            }
        };
        let (_, frontend_body_reader, pending) = request.into_parts();
        let response_stream = ProxyResponseStream::new(response_stream);
        Ok(match response_stream {
            ProxyResponseStream::Response(proxy_response) => {
                BackendResponseStream::Response(BackendResponse {
                    frontend_body_reader,
                    pending,
                    proxy_response,
                    backend_body_writer,
                })
            }
            ProxyResponseStream::Informational(
                proxy_informational_response,
                proxy_stream_reader,
            ) => BackendResponseStream::Informational(BackendInformationalResponse {
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
    Forwarded(BackendResponseStream<FR, FW, BR, BW>),
    Dropped(BackendResponseStream<FR, FW, BR, BW>),
}

impl<FR, FW, BR, BW> InformationalForwardResult<FR, FW, BR, BW> {
    pub fn into_inner(self) -> BackendResponseStream<FR, FW, BR, BW> {
        match self {
            InformationalForwardResult::Forwarded(r) => r,
            InformationalForwardResult::Dropped(r) => r,
        }
    }
}

pub struct BackendInformationalResponse<FR, FW, BR, BW> {
    proxy_informational_response: ProxyInformationalResponse,
    frontend_body_reader: FrontendBodyReader<FR>,
    pending: PendingFrontendResponse<FW>,
    proxy_stream_reader: ProxyStreamReader<BR>,
    backend_body_writer: BackendBodyWriter<BW>,
}

impl<FR, FW, BR, BW> BackendInformationalResponse<FR, FW, BR, BW>
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
            pending,
            proxy_stream_reader,
            backend_body_writer,
        } = self;

        // RFC9110 section 15.2 states that a server MUST NOT send any 1xx
        // response to a HTTP/1.0 client.
        let client_supports_informational_response =
            pending.client_options.version == HttpVersion::Http11;

        let pending = if client_supports_informational_response {
            let response = proxy_informational_response.into_frontend_response();
            pending
                .send_informational(response)
                .await
                .map_err(InformationalForwardError::Frontend)?
        } else {
            drop(proxy_informational_response);
            pending
        };

        let response_stream = match proxy_stream_reader.read().await {
            Ok(r) => r,
            Err(error) => {
                return Err(InformationalForwardError::Backend(BackendStreamError {
                    frontend_body_reader,
                    pending,
                    error,
                }));
            }
        };

        let response_stream = match response_stream {
            ProxyResponseStream::Response(proxy_response) => {
                BackendResponseStream::Response(BackendResponse {
                    frontend_body_reader,
                    pending,
                    proxy_response,
                    backend_body_writer,
                })
            }
            ProxyResponseStream::Informational(
                proxy_informational_response,
                proxy_stream_reader,
            ) => BackendResponseStream::Informational(BackendInformationalResponse {
                proxy_informational_response,
                frontend_body_reader,
                pending,
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

pub struct BackendResponse<FR, FW, BR, BW> {
    frontend_body_reader: FrontendBodyReader<FR>,
    pending: PendingFrontendResponse<FW>,
    proxy_response: ProxyResponse<BR>,
    backend_body_writer: BackendBodyWriter<BW>,
}

impl<FR, FW, BR, BW> BackendResponse<FR, FW, BR, BW>
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

    /// Send the response head to the frontend and return a [`BodyExchanger`]
    /// ready to drive the body copy in both directions.
    ///
    /// Use [`BodyExchanger::finish`] to drive the copy internally, or
    /// [`BodyExchanger::into_parts`] to take ownership of the body readers
    /// and writers and drive the copy yourself.
    pub async fn forward_response_head(self) -> Result<BodyExchanger<FR, FW, BR, BW>, ProxyError> {
        let Self {
            frontend_body_reader,
            pending,
            proxy_response,
            backend_body_writer,
        } = self;

        // TODO: use client_options to decide if we need to buffer the response
        // (chunk-encoded) or if we can send back a content-length.
        let is_head = pending.client_options.is_head;
        let (response, backend_body_reader) = proxy_response.into_frontend_response();
        let (options, frontend_body_writer) = match backend_body_reader.mode() {
            BodyReadMode::Chunk => pending.send_as_chunked(response).await?,
            BodyReadMode::ContentLength(cl) => pending.send_as_content_length(response, cl).await?,
            BodyReadMode::Bodyless => {
                // HEAD responses and 304 Not Modified carry framing headers
                // that describe the representation, not an actual body; those
                // must be forwarded. All other bodyless codes (204, etc.) must
                // not carry framing headers.
                if is_head || response.code() == 304 {
                    pending.send_as_bodyless_keep_framing(response).await?
                } else {
                    pending.send_as_no_content(response).await?
                }
            }
        };

        Ok(BodyExchanger {
            options,
            frontend_body_reader,
            backend_body_writer,
            backend_body_reader,
            frontend_body_writer,
        })
    }
}

/// Drives the bidirectional body exchange between the frontend and backend
/// after the response head has been forwarded.
///
/// Produced by [`BackendResponse::forward_response_head`].
pub struct BodyExchanger<FR, FW, BR, BW> {
    options: ProxyOptions,
    frontend_body_reader: FrontendBodyReader<FR>,
    backend_body_writer: BackendBodyWriter<BW>,
    backend_body_reader: BackendBodyReader<BR>,
    frontend_body_writer: FrontendBodyWriter<FW>,
}

/// The individual body readers and writers that make up a [`BodyExchanger`].
///
/// Produced by [`BodyExchanger::into_parts`]. The caller is responsible for
/// driving the body copies concurrently. This is typically done using
/// `tokio::select!` or `tokio::spawn`. See [`BodyExchanger::finish`] for a
/// reference implementation.
pub struct BodyExchangerParts<FR, FW, BR, BW> {
    /// ProxyOptions governing behavior so far
    pub options: ProxyOptions,
    /// Reads the request body from the frontend (f2b source).
    pub frontend_body_reader: FrontendBodyReader<FR>,
    /// Writes the request body to the backend (f2b sink).
    pub backend_body_writer: BackendBodyWriter<BW>,
    /// Reads the response body from the backend (b2f source).
    pub backend_body_reader: BackendBodyReader<BR>,
    /// Writes the response body to the frontend (b2f sink).
    pub frontend_body_writer: FrontendBodyWriter<FW>,
}

impl<FR, FW, BR, BW> BodyExchanger<FR, FW, BR, BW>
where
    FR: AsyncReadExt + Unpin,
    FW: AsyncWriteExt + Unpin,
    BR: AsyncReadExt + Unpin,
    BW: AsyncWriteExt + Unpin,
{
    /// Decompose into the individual body readers and writers. The caller is
    /// responsible for driving the f2b and b2f copies concurrently.
    pub fn into_parts(self) -> BodyExchangerParts<FR, FW, BR, BW> {
        let Self {
            options,
            frontend_body_reader,
            backend_body_writer,
            backend_body_reader,
            frontend_body_writer,
        } = self;
        BodyExchangerParts {
            options,
            frontend_body_reader,
            backend_body_writer,
            backend_body_reader,
            frontend_body_writer,
        }
    }

    /// Drive the body copy in both directions concurrently and return a
    /// [`ProxyClient`] ready for the next request.
    pub async fn finish(self) -> ProxyResult<(ProxyClient<FR, FW>, BackendConnection<BR, BW>)> {
        let Self {
            options,
            frontend_body_reader,
            backend_body_writer,
            backend_body_reader,
            frontend_body_writer,
        } = self;

        let body_chunk_size = options.body_chunk_size;
        let mut frontend_reader_kind = frontend_body_reader.into_kind();
        let mut frontend_writer_kind = frontend_body_writer.into_kind();
        let mut backend_reader_kind = backend_body_reader.into_kind();
        let mut backend_writer_kind = backend_body_writer.into_kind();

        let f2b_fut = async move {
            let ret;
            loop {
                let buf = match frontend_reader_kind.read(body_chunk_size).await {
                    Ok(Some(buf)) => buf,
                    Ok(None) => {
                        ret = async {
                            match (frontend_reader_kind, backend_writer_kind) {
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
                match backend_writer_kind.write(&buf).await {
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
                let buf = match backend_reader_kind.read(body_chunk_size).await {
                    Ok(Some(buf)) => buf,
                    Ok(None) => {
                        ret = async {
                            match (backend_reader_kind, frontend_writer_kind) {
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
                match frontend_writer_kind.write(&buf).await {
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
/// - `Connection` - RFC 9110 7.6.1
/// - `Keep-Alive` - RFC 9112 9.3
/// - `Proxy-Authenticate` - RFC 9110 11.7.1 (consumed by the receiving proxy)
/// - `Proxy-Authorization` - RFC 9110 11.7.2 (consumed by the receiving proxy)
/// - `TE` - RFC 9110 10.1.4 (explicitly defined as a hop-by-hop field)
/// - `Transfer-Encoding` - RFC 9112 6.1 (HTTP/1.1 transfer coding)
/// - `Upgrade` - RFC 9110 7.8
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

/// Remove all hop-by-hop headers from `headers` (RFC 9110 7.6.1, RFC 9112).
///
/// Returns the removed headers.
fn strip_hop_by_hop_headers(headers: &mut Headers) -> Headers {
    let mut connection_tokens = Vec::new();

    // Collect any extra hop-by-hop names listed in any Connection header.
    for (_h, v) in headers.get_header("connection") {
        // TODO: let's make this an actual parser eventually
        let tokens = v
            .split(|&b| b == b',')
            .map(|s| Bytes::copy_from_slice(s.trim_ascii()))
            .filter(|s| !s.is_empty());
        for tok in tokens {
            connection_tokens.push(tok);
        }
    }

    headers.remove(|(name, _)| {
        is_standard_hop_by_hop(name)
            || connection_tokens
                .iter()
                .any(|t| t.as_ref().eq_ignore_ascii_case(name))
    })
}

/// Inject a `Via` header (RFC 9110 sec 7.6.3) into `headers`.
fn insert_proxy_headers(headers: &mut Headers, version: HttpVersion, received_by: &str) {
    let via_version = match version {
        HttpVersion::Http10 => "1.0",
        HttpVersion::Http11 => "1.1",
    };
    headers.push(
        Bytes::from_static(b"Via"),
        Bytes::from(format!("{via_version} {received_by}")),
    );
}

/// A proxy request received from the frontend.
///
/// Hop-by-hop headers are stripped on construction. The request can be
/// inspected and modified before forwarding. Call [`into_backend_request`] to
/// produce a [`BackendProxyRequest`] with proxy headers (e.g. `Via`) merged
/// in, ready to write to a backend connection.
///
/// Also carries a [`PendingFrontendResponse`] so the caller can send an error
/// response to the frontend at any point via [`into_pending_response`].
///
/// [`into_backend_request`]: FrontendProxyRequest::into_backend_request
/// [`into_pending_response`]: FrontendProxyRequest::into_pending_response
pub struct FrontendProxyRequest<FR, FW> {
    frontend: FrontendRequest<FR>,
    hop_by_hop: Headers,
    pending: PendingFrontendResponse<FW>,
}

impl<FR, FW> FrontendProxyRequest<FR, FW> {
    fn new(mut frontend: FrontendRequest<FR>, pending: PendingFrontendResponse<FW>) -> Self {
        let hop_by_hop = strip_hop_by_hop_headers(frontend.req_mut().headers_mut());
        Self {
            frontend,
            hop_by_hop,
            pending,
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
    /// construction.
    pub fn hop_by_hop_headers(&self) -> &Headers {
        &self.hop_by_hop
    }

    /// Convert this frontend request into a [`BackendProxyRequest`] ready to
    /// write to a backend connection.
    ///
    /// Proxy headers (e.g. `Via`) are merged in using the `received_by`
    /// identifier from [`ProxyOptions`]. The pending frontend response is
    /// carried through into the [`BackendProxyRequest`].
    pub fn into_backend_request(self) -> BackendProxyRequest<FR, FW> {
        let Self {
            mut frontend,
            hop_by_hop: _,
            pending,
        } = self;
        let version = frontend.req().version();
        insert_proxy_headers(
            frontend.req_mut().headers_mut(),
            version,
            &pending.options.received_by,
        );
        let (request, body_reader) = frontend.into_parts();
        BackendProxyRequest {
            request,
            body_reader,
            pending,
        }
    }

    /// Discard the request and return the [`PendingFrontendResponse`] so the
    /// caller can send an error response to the frontend.
    pub fn into_pending_response(self) -> PendingFrontendResponse<FW> {
        self.pending
    }
}

/// A backend request produced by [`FrontendProxyRequest::into_backend_request`].
///
/// Proxy headers (e.g. `Via`) have been merged in. Carries the prepared
/// request, the frontend body reader, and the pending frontend response so all
/// three are available for writing, retry, or error handling.
pub struct BackendProxyRequest<FR, FW> {
    request: Request,
    body_reader: FrontendBodyReader<FR>,
    pending: PendingFrontendResponse<FW>,
}

impl<FR, FW> BackendProxyRequest<FR, FW> {
    /// The underlying [`Request`] with hop-by-hop headers removed and proxy
    /// headers merged in.
    pub fn req(&self) -> &Request {
        &self.request
    }

    /// The underlying [`Request`] as mutable.
    pub fn req_mut(&mut self) -> &mut Request {
        &mut self.request
    }

    /// The framing mode derived from the frontend body reader.
    pub fn mode(&self) -> BodyReadMode {
        self.body_reader.mode()
    }

    /// Return the [`FrontendBodyReader`] and the [`PendingFrontendResponse`] so
    /// the caller can send an error response to the frontend and optionally
    /// drain the request from body from the frontend so the connection can be
    /// reused.
    pub fn into_pending_response(self) -> (FrontendBodyReader<FR>, PendingFrontendResponse<FW>) {
        let Self {
            request: _,
            body_reader,
            pending,
        } = self;
        (body_reader, pending)
    }

    /// Decompose into the prepared request, frontend body reader, and pending
    /// frontend response.
    fn into_parts(self) -> (Request, FrontendBodyReader<FR>, PendingFrontendResponse<FW>) {
        (self.request, self.body_reader, self.pending)
    }

    /// Write this backend request to the given backend connection.
    ///
    /// On success, returns a [`ResponseReader`] ready for
    /// [`read_backend_response`]. On failure, returns a [`BackendRequestError`]
    /// that carries this request intact so the caller can retry with a
    /// different backend or send an error response via
    /// [`BackendRequestError::into_backend_request`].
    ///
    /// [`read_backend_response`]: ResponseReader::read_backend_response
    pub async fn forward<BR, BW>(
        self,
        backend_connection: BackendConnection<BR, BW>,
    ) -> Result<ResponseReader<FR, FW, BR, BW>, BackendRequestError<FR, FW>>
    where
        BR: AsyncReadExt + Unpin,
        BW: AsyncWriteExt + Unpin,
    {
        let (backend_reader, backend_writer) = backend_connection;
        let write_result = match self.body_reader.mode() {
            BodyReadMode::Chunk => backend_writer.send_as_chunked(&self.request).await,
            BodyReadMode::ContentLength(cl) => {
                backend_writer
                    .send_as_content_length(&self.request, cl)
                    .await
            }
            BodyReadMode::Bodyless => backend_writer.send_as_bodyless(&self.request).await,
        };
        match write_result {
            Ok(backend_body_writer) => Ok(ResponseReader {
                request: self,
                backend_reader,
                backend_body_writer,
            }),
            Err(error) => Err(BackendRequestError {
                request: self,
                error,
            }),
        }
    }
}

/// The proxy-processed form of a regular (non-1xx) backend response.
///
/// Hop-by-hop headers have been stripped on construction and are available
/// via [`ProxyResponse::hop_by_hop_headers`].
pub struct ProxyResponse<I> {
    backend: BackendReaderResponse<I>,
    hop_by_hop: Headers,
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

    /// The end-to-end headers on this response.
    pub fn end_to_end_headers(&self) -> &Headers {
        self.res().headers()
    }

    /// Convert the [`ProxyResponse`] into a response ready to send to the
    /// frontend. Hop-by-hop headers have already been removed.
    pub fn into_frontend_response(self) -> (Response, BackendBodyReader<I>) {
        let Self {
            backend,
            hop_by_hop: _,
        } = self;
        backend.into_parts()
    }
}

/// The proxy-processed form of a 1xx informational response.
///
/// Hop-by-hop headers have been stripped on construction and are available
/// via [`ProxyInformationalResponse::hop_by_hop_headers`].
pub struct ProxyInformationalResponse {
    res: Response,
    hop_by_hop: Headers,
}

impl ProxyInformationalResponse {
    /// The underlying [`Response`] with hop-by-hop headers removed.
    pub fn res(&self) -> &Response {
        &self.res
    }

    /// A mutable reference to the underlying [`Response`] with hop-by-hop
    /// headers removed.
    pub fn res_mut(&mut self) -> &mut Response {
        &mut self.res
    }

    /// The end-to-end headers on this response.
    pub fn end_to_end_headers(&self) -> &Headers {
        self.res().headers()
    }

    /// The hop-by-hop headers that were removed from the response on
    /// construction.
    pub fn hop_by_hop_headers(&self) -> &Headers {
        &self.hop_by_hop
    }

    /// Convert the [`ProxyInformationalResponse`] into a response ready to
    /// send to the frontend. Hop-by-hop headers have already been removed.
    pub fn into_frontend_response(self) -> Response {
        let Self { res, hop_by_hop: _ } = self;
        res
    }
}

/// Reads the next response in a stream, producing a [`ProxyResponseStream`].
///
/// Wraps [`BackendStreamReader`] and applies hop-by-hop removal to each
/// response it yields.
pub struct ProxyStreamReader<I> {
    reader: BackendStreamReader<I>,
}

impl<I: AsyncReadExt + Unpin> ProxyStreamReader<I> {
    pub async fn read(self) -> BackendResult<ProxyResponseStream<I>> {
        let stream = self.reader.read().await?;
        Ok(ProxyResponseStream::new(stream))
    }
}

/// The proxy-processed form of [`ResponseStream`].
///
/// Each variant has had its hop-by-hop headers stripped.
pub enum ProxyResponseStream<I> {
    /// A regular (non-1xx) response.
    Response(ProxyResponse<I>),
    /// A 1xx informational response. More responses can be read from the
    /// accompanying [`ProxyStreamReader`].
    Informational(ProxyInformationalResponse, ProxyStreamReader<I>),
}

impl<I> ProxyResponseStream<I> {
    fn new(stream: ResponseStream<I>) -> Self {
        match stream {
            ResponseStream::Response(mut backend) => {
                let hop_by_hop = strip_hop_by_hop_headers(backend.res_mut().headers_mut());
                ProxyResponseStream::Response(ProxyResponse {
                    backend,
                    hop_by_hop,
                })
            }
            ResponseStream::Informational(mut res, reader) => {
                let hop_by_hop = strip_hop_by_hop_headers(res.headers_mut());
                ProxyResponseStream::Informational(
                    ProxyInformationalResponse { res, hop_by_hop },
                    ProxyStreamReader { reader },
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

    use jrpxy_http_message::{message::ResponseBuilder, version::HttpVersion};

    use crate::{
        BackendResponseStream, ClientOptions, FrontendRequestError, PendingFrontendResponse,
        ProxyClient, ProxyFrontendError, ProxyOptions,
    };

    use super::{
        BackendProxyRequest, FrontendProxyRequest, ProxyInformationalResponse, ProxyResponse,
        ProxyResponseStream, ProxyStreamReader,
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

    async fn make_frontend_proxy_request(
        raw: &[u8],
    ) -> FrontendProxyRequest<&[u8], tokio::io::Sink> {
        let reader = FrontendReader::new(raw);
        let req = reader.read(8192).await.expect("valid request");
        let is_head = req.req().method() == b"HEAD".as_slice();
        let version = req.req().version();
        let pending = PendingFrontendResponse {
            frontend_writer: FrontendWriter::new(tokio::io::sink()),
            options: ProxyOptions::default(),
            client_options: ClientOptions { is_head, version },
        };
        FrontendProxyRequest::new(req, pending)
    }

    async fn make_backend_proxy_request(raw: &[u8]) -> BackendProxyRequest<&[u8], tokio::io::Sink> {
        make_frontend_proxy_request(raw)
            .await
            .into_backend_request()
    }

    async fn make_proxy_response(raw: &[u8]) -> ProxyResponseStream<&[u8]> {
        let reader = BackendReader::new(raw);
        let stream = reader.read(true, 8192).await.expect("valid response");
        ProxyResponseStream::new(stream)
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
            FrontendReader::new(frontend_reader.as_ref()),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let did_fe_read = proxy_client.start().await.expect("client start failed");

        let backend_connection = bp.get_connection().await.expect("no backend connection");
        let did_be_write = did_fe_read
            .into_backend_request()
            .forward(backend_connection)
            .await
            .expect("backend write failed");

        let be_res_stat = did_be_write
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        let rbir = match be_res_stat {
            BackendResponseStream::Response(_read_backend_response) => {
                panic!("incorrect response type")
            }
            BackendResponseStream::Informational(read_backend_informational_response) => {
                read_backend_informational_response
            }
        };

        // Verify we can read the end-to-end header in the informational response
        assert_eq!(
            b"1".as_slice(),
            rbir.as_response()
                .end_to_end_headers()
                .get_header("x-hdr")
                .next()
                .unwrap()
                .1
        );

        // Verify we can read the end-to-end header in the informational response
        assert_eq!(
            b"filter".as_slice(),
            rbir.as_response()
                .hop_by_hop_headers()
                .get_header("connection")
                .next()
                .unwrap()
                .1
        );
        assert_eq!(
            b"2".as_slice(),
            rbir.as_response()
                .hop_by_hop_headers()
                .get_header("filter")
                .next()
                .unwrap()
                .1
        );

        // Ensure the hop-by-hop headers are NOT in the end-to-end headers
        assert!(
            rbir.as_response()
                .end_to_end_headers()
                .get_header("filter")
                .next()
                .is_none()
        );
        assert!(
            rbir.as_response()
                .end_to_end_headers()
                .get_header("connection")
                .next()
                .is_none()
        );

        let be_res_stat = rbir
            .forward_informational_response()
            .await
            .expect("failed to forward informational response");

        let rbr = match be_res_stat.into_inner() {
            BackendResponseStream::Response(rbr) => rbr,
            BackendResponseStream::Informational(_rbir) => {
                panic!("incorrect response type")
            }
        };

        // Verify we can read the end-to-end header in the response
        assert_eq!(
            b"10".as_slice(),
            rbr.as_response()
                .end_to_end_headers()
                .get_header("y-hdr")
                .next()
                .unwrap()
                .1
        );

        // Verify we can read the end-to-end header in the informational response
        assert_eq!(
            b"filter-res".as_slice(),
            rbr.as_response()
                .hop_by_hop_headers()
                .get_header("connection")
                .next()
                .unwrap()
                .1
        );
        assert_eq!(
            b"20".as_slice(),
            rbr.as_response()
                .hop_by_hop_headers()
                .get_header("filter-res")
                .next()
                .unwrap()
                .1
        );

        // Ensure the hop-by-hop headers are NOT in the end-to-end headers
        assert!(
            rbr.as_response()
                .end_to_end_headers()
                .get_header("filter-res")
                .next()
                .is_none()
        );
        assert!(
            rbr.as_response()
                .end_to_end_headers()
                .get_header("connection")
                .next()
                .is_none()
        );

        let (client, (backend_reader, backend_writer)) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
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
            FrontendReader::new(frontend_reader.as_ref()),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let did_fe_read = proxy_client.start().await.expect("client start failed");

        let backend_connection = bp.get_connection().await.expect("no backend connection");
        let did_be_write = did_fe_read
            .into_backend_request()
            .forward(backend_connection)
            .await
            .expect("backend write failed");

        let be_res_stat = did_be_write
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        // First, we get the informational response from the backend
        let rbir = match be_res_stat {
            BackendResponseStream::Response(_) => panic!("incorrect response type"),
            BackendResponseStream::Informational(rbir) => rbir,
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
            BackendResponseStream::Response(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("incorrect response type"),
        };

        // Normal responses can be forwarded to HTTP/1.0 clients.
        let (client, _backend_connection) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
            .await
            .expect("failed to forward response");

        let (_frontend_reader, frontend_writer) = client.into_parts();
        let frontend_writer = frontend_writer.into_inner();

        // The 1xx informational response must NOT be forwarded to an HTTP/1.0 client.
        // Via uses "1.1" because the backend responded with HTTP/1.1.
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
            FrontendReader::new(frontend_reader.as_ref()),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let backend_connection = bp.get_connection().await.expect("no backend connection");
        let be_res_stat = proxy_client
            .start()
            .await
            .expect("client start failed")
            .into_backend_request()
            .forward(backend_connection)
            .await
            .expect("backend write failed")
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        let rbr = match be_res_stat {
            BackendResponseStream::Response(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, _backend_connection) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
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
    async fn informational_response_via_injected() {
        let frontend_reader = b"\
            GET / HTTP/1.1\r\n\
            Host: example.com\r\n\
            \r\n";
        let mut frontend_writer = Vec::new();

        let backend_reader = b"\
            HTTP/1.1 100 Continue\r\n\
            \r\n\
            HTTP/1.1 200 Ok\r\n\
            content-length: 0\r\n\
            \r\n";
        let mut backend_writer = Vec::new();

        let mut bp = OneshotBackend::new(backend_reader.as_ref(), &mut backend_writer);
        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref()),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let backend_connection = bp.get_connection().await.expect("no backend connection");
        let be_res_stat = proxy_client
            .start()
            .await
            .expect("client start failed")
            .into_backend_request()
            .forward(backend_connection)
            .await
            .expect("backend write failed")
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        let rbir = match be_res_stat {
            BackendResponseStream::Informational(rbir) => rbir,
            BackendResponseStream::Response(_) => panic!("expected informational"),
        };

        let be_res_stat = rbir
            .forward_informational_response()
            .await
            .expect("failed to forward informational")
            .into_inner();

        let rbr = match be_res_stat {
            BackendResponseStream::Response(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("expected final response"),
        };

        rbr.forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
            .await
            .expect("failed to forward response");

        let expected = b"\
            HTTP/1.1 100 Continue\r\n\
            Via: 1.1 jrpxy\r\n\
            \r\n\
            HTTP/1.1 200 Ok\r\n\
            Via: 1.1 jrpxy\r\n\
            content-length: 0\r\n\
            \r\n";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected.as_slice()),
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
            FrontendReader::new(frontend_reader.as_ref()),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let backend_connection = bp.get_connection().await.expect("no backend connection");
        let be_res_stat = proxy_client
            .start()
            .await
            .expect("client start failed")
            .into_backend_request()
            .forward(backend_connection)
            .await
            .expect("backend write failed")
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        let rbr = match be_res_stat {
            BackendResponseStream::Response(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, (_backend_reader, backend_writer)) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
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
            FrontendReader::new(frontend_reader.as_ref()),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let backend_connection = bp.get_connection().await.expect("no backend connection");
        let be_res_stat = proxy_client
            .start()
            .await
            .expect("client start failed")
            .into_backend_request()
            .forward(backend_connection)
            .await
            .expect("backend write failed")
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        let rbr = match be_res_stat {
            BackendResponseStream::Response(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (_client, (_backend_reader, backend_writer)) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
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
            FrontendReader::new(frontend_reader.as_ref()),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let backend_connection = bp.get_connection().await.expect("no backend connection");
        let be_res_stat = proxy_client
            .start()
            .await
            .expect("client start failed")
            .into_backend_request()
            .forward(backend_connection)
            .await
            .expect("backend write failed")
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        let rbr = match be_res_stat {
            BackendResponseStream::Response(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (_client, (_backend_reader, backend_writer)) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
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
            FrontendReader::new(frontend_reader.as_ref()),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let backend_connection = bp.get_connection().await.expect("no backend connection");
        let be_res_stat = proxy_client
            .start()
            .await
            .expect("client start failed")
            .into_backend_request()
            .forward(backend_connection)
            .await
            .expect("backend write failed")
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        let rbr = match be_res_stat {
            BackendResponseStream::Response(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, _backend_connection) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
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
            FrontendReader::new(frontend_reader.as_ref()),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let backend_connection = bp.get_connection().await.expect("no backend connection");
        let be_res_stat = proxy_client
            .start()
            .await
            .expect("client start failed")
            .into_backend_request()
            .forward(backend_connection)
            .await
            .expect("backend write failed")
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        let rbr = match be_res_stat {
            BackendResponseStream::Response(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, _backend_connection) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
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
            FrontendReader::new(frontend_reader.as_ref()),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let backend_connection = bp.get_connection().await.expect("no backend connection");
        let be_res_stat = proxy_client
            .start()
            .await
            .expect("client start failed")
            .into_backend_request()
            .forward(backend_connection)
            .await
            .expect("backend write failed")
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        let rbr = match be_res_stat {
            BackendResponseStream::Response(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, backend_connection) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
            .await
            .expect("failed to forward response");

        // Release the mutable borrow on frontend_writer before asserting.
        let (frontend_reader, _) = client.into_parts();

        // content-length must be forwarded (RFC 9110 section 15.4.5).
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
        // next response - no body bytes were consumed from the 304.
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
            .into_backend_request()
            .forward(backend_connection)
            .await
            .expect("second backend write failed")
            .read_backend_response()
            .await
            .expect("second read failed");

        let rbr = match be_res_stat {
            BackendResponseStream::Response(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, _) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
            .await
            .expect("second forward failed");

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
            FrontendReader::new(frontend_reader.as_ref()),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let backend_connection = bp.get_connection().await.expect("no backend connection");
        let be_res_stat = proxy_client
            .start()
            .await
            .expect("client start failed")
            .into_backend_request()
            .forward(backend_connection)
            .await
            .expect("backend write failed")
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        let rbr = match be_res_stat {
            BackendResponseStream::Response(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, backend_connection) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
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
            .into_backend_request()
            .forward(backend_connection)
            .await
            .expect("backend write failed")
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        let rbr = match be_res_stat {
            BackendResponseStream::Response(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, _backend_connection) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
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

        let pr = make_frontend_proxy_request(raw).await;

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
        // Note this request has two connection headers. Tokens from both should
        // be removed.
        let raw = b"\
            GET / HTTP/1.1\r\n\
            Host: example.com\r\n\
            X-Custom-Hop-1: value\r\n\
            Connection: x-custom-hop-2\r\n\
            X-Custom-Hop-2: value\r\n\
            Connection: x-custom-hop-1\r\n\
            \r\n";

        let pr = make_frontend_proxy_request(raw).await;

        let names: Vec<_> = pr.req().headers().iter().map(|(n, _)| n.clone()).collect();
        assert!(
            names
                .iter()
                .all(|n| !n.eq_ignore_ascii_case(b"x-custom-hop-1")
                    && !n.eq_ignore_ascii_case(b"x-custom-hop-2"))
        );

        let hop: Vec<_> = pr
            .hop_by_hop_headers()
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        assert!(
            hop.iter()
                .any(|n| n.eq_ignore_ascii_case(b"x-custom-hop-1"))
        );
        assert!(
            hop.iter()
                .any(|n| n.eq_ignore_ascii_case(b"x-custom-hop-2"))
        );
    }

    #[tokio::test]
    async fn adds_via_header_to_request() {
        let raw = b"\
            GET / HTTP/1.1\r\n\
            Host: example.com\r\n\
            \r\n";

        let pr = make_backend_proxy_request(raw).await;

        let via = pr
            .req()
            .headers()
            .get_header("via")
            .next()
            .expect("Via must be present");
        assert_eq!(via.1.as_ref(), b"1.1 jrpxy");
    }

    #[tokio::test]
    async fn request_via_version_http10() {
        let raw = b"\
            GET / HTTP/1.0\r\n\
            Host: example.com\r\n\
            \r\n";

        let pr = make_backend_proxy_request(raw).await;

        let via = pr
            .req()
            .headers()
            .get_header("via")
            .next()
            .expect("Via must be present");
        assert_eq!(via.1.as_ref(), b"1.0 jrpxy");
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
        let mut buf = Vec::new();
        let pending = PendingFrontendResponse {
            frontend_writer: FrontendWriter::new(&mut buf),
            options: ProxyOptions {
                received_by: "proxy.example.com".into(),
                ..Default::default()
            },
            client_options: ClientOptions {
                is_head: false,
                version: HttpVersion::Http11,
            },
        };

        let response = ResponseBuilder::new(4)
            .with_version(HttpVersion::Http11)
            .with_code(200)
            .with_reason("Ok")
            .build()
            .expect("failed to build response")
            .into();

        let (_, body_writer) = pending
            .send_as_content_length(response, 0)
            .await
            .expect("send failed");
        body_writer.finish().await.expect("finish failed");

        let output = String::from_utf8(buf).unwrap();
        assert!(
            output.contains("Via: 1.1 proxy.example.com"),
            "expected Via header in output: {output:?}"
        );
    }

    #[tokio::test]
    async fn informational_response_hop_by_hop() {
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
        let proxy_stream = ProxyResponseStream::new(stream);

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

        // Via is injected when the 1xx is forwarded via PendingFrontendResponse.
        let mut buf = Vec::new();
        let pending = PendingFrontendResponse {
            frontend_writer: FrontendWriter::new(&mut buf),
            options: ProxyOptions {
                received_by: "proxy.example.com".into(),
                ..Default::default()
            },
            client_options: ClientOptions {
                is_head: false,
                version: HttpVersion::Http11,
            },
        };
        let forwarded = info.into_frontend_response();
        let (_, bw) = pending
            .send_as_no_content(forwarded)
            .await
            .expect("send failed");
        bw.finish().await.expect("finish failed");
        let output = String::from_utf8(buf).unwrap();
        assert!(
            output.contains("Via: 1.1 proxy.example.com"),
            "expected Via header in forwarded 1xx: {output:?}"
        );

        // The following final response is also processed
        let final_stream = next_reader.read().await.expect("read final response");
        let final_res = into_response(final_stream);
        assert_eq!(final_res.res().code(), 200);
    }

    /// CONNECT is not supported - test frontend request handling errors
    #[tokio::test]
    async fn connect_method_not_supported() {
        let frontend_reader = b"\
            CONNECT example.com:443 HTTP/1.1\r\n\
            Host: example.com:443\r\n\
            \r\n";
        let mut frontend_writer = Vec::new();

        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref()),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let req_err = match proxy_client.start().await {
            Ok(_) => panic!("expected error for CONNECT, got Ok"),
            Err(FrontendRequestError::Request(e)) => e,
            Err(e) => panic!("unexpected error variant: {e:?}"),
        };

        assert!(matches!(
            req_err.error(),
            ProxyFrontendError::ConnectNotSupported
        ));

        let pending = req_err.into_frontend_request().into_pending_response();

        let response = ResponseBuilder::new(4)
            .with_version(HttpVersion::Http11)
            .with_code(405)
            .with_reason("Method Not Allowed")
            .build()
            .expect("failed to build 405 response")
            .into();

        let (_, body_writer) = pending
            .send_as_content_length(response, 0)
            .await
            .expect("failed to send error response");
        body_writer.finish().await.expect("failed to finish");

        let expected = b"\
            HTTP/1.1 405 Method Not Allowed\r\n\
            Via: 1.1 jrpxy\r\n\
            content-length: 0\r\n\
            \r\n";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected.as_slice()),
            jrpxy_util::debug::AsciiDebug(&frontend_writer)
        );
    }

    /// TRACE is not supported - test frontend request handling errors
    #[tokio::test]
    async fn trace_method_not_supported() {
        let frontend_reader = b"\
            TRACE / HTTP/1.1\r\n\
            Host: example.com\r\n\
            \r\n";
        let mut frontend_writer = Vec::new();

        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref()),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let req_err = match proxy_client.start().await {
            Ok(_) => panic!("expected error for TRACE, got Ok"),
            Err(FrontendRequestError::Request(e)) => e,
            Err(e) => panic!("unexpected error variant: {e:?}"),
        };

        assert!(matches!(
            req_err.error(),
            ProxyFrontendError::TraceNotSupported
        ));

        let pending = req_err.into_frontend_request().into_pending_response();

        let response = ResponseBuilder::new(4)
            .with_version(HttpVersion::Http11)
            .with_code(405)
            .with_reason("Method Not Allowed")
            .build()
            .expect("failed to build 405 response")
            .into();

        let (_, body_writer) = pending
            .send_as_content_length(response, 0)
            .await
            .expect("failed to send error response");
        body_writer.finish().await.expect("failed to finish");

        let expected = b"\
            HTTP/1.1 405 Method Not Allowed\r\n\
            Via: 1.1 jrpxy\r\n\
            content-length: 0\r\n\
            \r\n";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected.as_slice()),
            jrpxy_util::debug::AsciiDebug(&frontend_writer)
        );
    }

    /// When the frontend sends a malformed request, start() returns a
    /// FrontendRequestError::Frontend that still carries a PendingFrontendResponse,
    /// so the proxy can write a 400 Bad Request back to the client.
    #[tokio::test]
    async fn malformed_request_can_send_error_response() {
        // A request line with no path or HTTP version - not parseable.
        let frontend_reader = b"GET\r\n\r\n";
        let mut frontend_writer = Vec::new();

        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref()),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let read_err = match proxy_client.start().await {
            Ok(_) => panic!("expected parse error, got Ok"),
            Err(FrontendRequestError::Frontend(e)) => e,
            Err(e) => panic!("unexpected error variant: {e:?}"),
        };

        let pending = read_err.into_pending_response();

        let response = ResponseBuilder::new(4)
            .with_version(HttpVersion::Http11)
            .with_code(400)
            .with_reason("Bad Request")
            .build()
            .expect("failed to build 400 response")
            .into();

        let (_, body_writer) = pending
            .send_as_content_length(response, 0)
            .await
            .expect("failed to send error response");
        body_writer.finish().await.expect("failed to finish");

        let expected = b"\
            HTTP/1.1 400 Bad Request\r\n\
            Via: 1.1 jrpxy\r\n\
            content-length: 0\r\n\
            \r\n";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected.as_slice()),
            jrpxy_util::debug::AsciiDebug(&frontend_writer)
        );
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
        let proxy_stream = ProxyResponseStream::new(stream);

        let (info1, r1) = into_informational(proxy_stream);
        assert_eq!(info1.res().code(), 100);

        let (info2, r2) = into_informational(r1.read().await.expect("read 2"));
        assert_eq!(info2.res().code(), 103);

        let final_res = into_response(r2.read().await.expect("read final"));
        assert_eq!(final_res.res().code(), 200);
    }
}
