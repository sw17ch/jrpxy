//! Tools for implementing an HTTP proxy.

use std::task::Poll;

use bytes::Bytes;
use jrpxy_backend::{
    error::BackendError,
    reader::{
        BackendBodyReader, BackendReader, BackendResponse as BackendReaderResponse,
        BackendStreamReader, ResponseStream,
    },
    writer::BackendWriter,
};
use jrpxy_frontend::{
    reader::{FrontendBodyReader, FrontendReader, FrontendRequest},
    writer::FrontendWriter,
};
pub use jrpxy_http_message::header::ConnectionTokenParserError;
use jrpxy_http_message::{
    header::Headers,
    message::{Request, Response},
    version::HttpVersion,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub mod body;
pub mod error;
mod options;
mod request_target;
pub use body::{RequestBodyForwarder, ResponseBodyForwarder};
pub use error::{ProxyCopyError, ProxyError, ProxyOptionsError, ProxyResult};
pub use options::{ProxyOptions, ProxyOptionsBuilder};

use crate::error::ProxyBackendError;
pub use crate::error::ProxyFrontendError;

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
    /// Convert the [`ProxyClient`] back into the reader and writer from which
    /// it was built. Assuming the [`ProxyClient`] was constructed with a fully
    /// drained reader and a fully flushed writer, this can never give the user
    /// back reader or writer mid-frame.
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

        let req = match frontend_reader
            .read(
                options.max_frontend_head_length(),
                options.max_chunk_header_length(),
            )
            .await
        {
            Ok(req) => req,
            Err(error) => {
                return Err(FrontendRequestError::new_frontend(
                    frontend_writer,
                    options,
                    HttpVersion::Http11,
                    error.into(),
                ));
            }
        };

        FrontendProxyRequest::from_request(req, frontend_writer, options)
    }
}

/// A valid request has been received from the frontend. We can forward this
/// Returned when [`ProxyClient::start`] fails.
///
/// The `Request` variant is larger than the `Frontend` variant because it
/// owns the full [`FrontendProxyRequest`] - by design, so the caller can
/// recover the body reader and send a custom error response. Boxing the
/// variant to silence `clippy::large_enum_variant` would be a public-API
/// break for a cold path; the size disparity does not matter here.
#[allow(clippy::large_enum_variant)]
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
    ) -> Self {
        Self::Request(FrontendRequestErrorKindRequest {
            reader: frontend_request,
            error,
        })
    }

    fn new_frontend(
        frontend_writer: FrontendWriter<FW>,
        options: ProxyOptions,
        version: HttpVersion,
        error: ProxyFrontendError,
    ) -> Self {
        Self::Frontend(FrontendRequestErrorKindRead {
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
            FrontendRequestError::Frontend(read) => Self::CopyError(read.error.into()),
            FrontendRequestError::Request(request) => Self::CopyError(request.error.into()),
        }
    }
}

// TODO: consider some better names for FrontendRequestErrorKindRead and
// FrontendRequestErrorKindRequest.

/// An error occurred while reading the request from the frontend. We cannot
/// recover the reader, but we can write something back.
pub struct FrontendRequestErrorKindRead<FW> {
    pending: PendingFrontendResponse<FW>,
    error: ProxyFrontendError,
}

impl<FW> FrontendRequestErrorKindRead<FW> {
    /// A reference to the inner error associated with the request error.
    pub fn error(&self) -> &ProxyFrontendError {
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
    /// A reference to the inner error associated with the request error.
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
        Self::CopyError(e.error.into())
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
/// The broken backend connection is discarded. A [`PendingFrontendResponse`] is
/// preserved so that the caller can write back a response to the client before
/// dropping the connection.
pub struct BackendResponseError<FW> {
    pending: PendingFrontendResponse<FW>,
    error: ProxyCopyError,
}

impl<FW> std::fmt::Debug for BackendResponseError<FW> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendResponseError")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl<FW> From<BackendResponseError<FW>> for ProxyError {
    fn from(e: BackendResponseError<FW>) -> Self {
        Self::CopyError(e.error)
    }
}

impl<FW> BackendResponseError<FW> {
    /// Returns the backend error that caused this failure.
    pub fn error(&self) -> &ProxyCopyError {
        &self.error
    }

    /// Consumes the error and returns the [`PendingFrontendResponse`] so the
    /// caller can send an error response to the frontend.
    pub fn into_pending_response(self) -> PendingFrontendResponse<FW> {
        let Self { pending, error: _ } = self;
        pending
    }
}

/// Returned when reading the next response from the backend fails after at
/// least one informational response has already been received from the backend.
///
/// Retry is not offered: the backend has already acknowledged the request
/// (evidenced by the 1xx it sent), so retrying risks duplicate processing.
/// The frontend connection is still alive; an error can be sent to the client.
pub struct BackendStreamError<FW> {
    pending: PendingFrontendResponse<FW>,
    error: ProxyCopyError,
}

impl<FW> std::fmt::Debug for BackendStreamError<FW> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendStreamError")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl<FW> From<BackendStreamError<FW>> for ProxyError {
    fn from(e: BackendStreamError<FW>) -> Self {
        Self::CopyError(e.error)
    }
}

impl<FW> BackendStreamError<FW> {
    /// Returns the backend error that caused this failure.
    pub fn error(&self) -> &ProxyCopyError {
        &self.error
    }

    /// Consumes the error and returns a [`PendingFrontendResponse`] and the
    /// frontend body reader. The body reader is returned separately so the
    /// caller can decide whether to drain it (for connection reuse) or drop it
    /// (to close the read half of the socket immediately).
    pub fn into_pending_response(self) -> PendingFrontendResponse<FW> {
        let Self { pending, error: _ } = self;
        pending
    }
}

/// A frontend connection that has not yet received a response.
///
/// This type is intentionally distinct from a reusable [`FrontendWriter`] to
/// make it clear that a response still needs to be sent before the connection
/// can be reused or closed.
///
/// The required proxy headers (`Via`, `Date`) are injected automatically by
/// each `send_as_*` method - the caller does not need to add them manually.
/// `Date` is only stamped when the response does not already carry one, so
/// any backend-supplied timestamp is preserved.
pub struct PendingFrontendResponse<FW> {
    frontend_writer: FrontendWriter<FW>,
    options: ProxyOptions,
    client_options: ClientOptions,
}

impl<FW> PendingFrontendResponse<FW>
where
    FW: AsyncWriteExt + Unpin,
{
    /// Send a 1xx informational response to the client. Proxy headers are
    /// injected automatically.
    ///
    /// Unlike the terminal `send_as_*` methods, this returns a fresh
    /// [`PendingFrontendResponse`] because a 1xx response is non-terminal -
    /// the frontend is still awaiting the final response to the same request.
    pub async fn send_informational(
        self,
        mut response: Response,
    ) -> Result<Self, ProxyFrontendError> {
        let Self {
            frontend_writer,
            options,
            client_options,
        } = self;
        let version = response.version();
        insert_proxy_headers(response.headers_mut(), version, options.received_by());
        let body_writer = frontend_writer.send_as_no_content(&response).await?;
        let frontend_writer = body_writer.finish().finish().await?;
        Ok(Self {
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
    ) -> Result<(ProxyOptions, jrpxy_frontend::writer::chunked::Ready<FW>), ProxyFrontendError>
    {
        let Self {
            frontend_writer,
            options,
            client_options: _,
        } = self;
        let version = response.version();
        insert_proxy_headers(response.headers_mut(), version, options.received_by());
        insert_date_header_if_absent(response.headers_mut());
        let body_writer = frontend_writer.send_as_chunked(&response).await?;
        Ok((options, body_writer))
    }

    /// Send the response with a content-length delimited body. Proxy headers
    /// are injected automatically.
    pub async fn send_as_content_length(
        self,
        mut response: Response,
        body_len: u64,
    ) -> Result<
        (
            ProxyOptions,
            jrpxy_frontend::writer::content_length::Ready<FW>,
        ),
        ProxyFrontendError,
    > {
        let Self {
            frontend_writer,
            options,
            client_options: _,
        } = self;
        let version = response.version();
        insert_proxy_headers(response.headers_mut(), version, options.received_by());
        insert_date_header_if_absent(response.headers_mut());
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
    ) -> Result<(ProxyOptions, jrpxy_frontend::writer::bodyless::Ready<FW>), ProxyFrontendError>
    {
        let Self {
            frontend_writer,
            options,
            client_options: _,
        } = self;
        let version = response.version();
        insert_proxy_headers(response.headers_mut(), version, options.received_by());
        insert_date_header_if_absent(response.headers_mut());
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
    ) -> Result<(ProxyOptions, jrpxy_frontend::writer::bodyless::Ready<FW>), ProxyFrontendError>
    {
        let Self {
            frontend_writer,
            options,
            client_options: _,
        } = self;
        let version = response.version();
        insert_proxy_headers(response.headers_mut(), version, options.received_by());
        insert_date_header_if_absent(response.headers_mut());
        let body_writer = frontend_writer.send_as_no_content(&response).await?;
        Ok((options, body_writer))
    }

    /// Send the response using EOF encoding. This should only be done when the
    /// length of the data is not known and the client does not support chunk
    /// encoding.
    async fn send_as_eof(
        self,
        mut response: Response,
    ) -> Result<(ProxyOptions, jrpxy_frontend::writer::eof::Ready<FW>), ProxyFrontendError> {
        let Self {
            frontend_writer,
            options,
            client_options: _,
        } = self;
        let version = response.version();
        insert_proxy_headers(response.headers_mut(), version, options.received_by());
        insert_date_header_if_absent(response.headers_mut());
        let body_writer = frontend_writer.send_as_eof(&response).await?;
        Ok((options, body_writer))
    }
}

/// Error returned by
/// [`BackendInformationalResponse::forward_informational_response`].
///
/// When the failure is due to the backend, a frontend pending response can be
/// extracted so that the client can be told something went wrong.
pub enum InformationalForwardError<FW> {
    /// Something went wrong writing the informational response to the frontend.
    /// We can't recover anything from this.
    Frontend(ProxyFrontendError),
    /// Something went wrong reading response data from the backend. A
    /// [`PendingFrontendResponse`] can be extracted from the
    /// [`BackendStreamError`] so that an error response can still be written to
    /// the client.
    Backend(BackendStreamError<FW>),
}

impl<FW> std::fmt::Debug for InformationalForwardError<FW> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Frontend(e) => f.debug_tuple("Frontend").field(e).finish(),
            Self::Backend(e) => f.debug_tuple("Backend").field(e).finish(),
        }
    }
}

impl<FW> From<InformationalForwardError<FW>> for ProxyError {
    fn from(e: InformationalForwardError<FW>) -> Self {
        match e {
            InformationalForwardError::Frontend(e) => Self::CopyError(e.into()),
            InformationalForwardError::Backend(e) => Self::CopyError(e.error),
        }
    }
}

type BackendConnection<BR, BW> = (BackendReader<BR>, BackendWriter<BW>);

#[derive(Clone, Copy, Debug)]
struct ClientOptions {
    is_head: bool,
    version: HttpVersion,
}

/// A reader of a response. Holds everything needed to forward the response back
/// to the frontend.
pub struct ResponseReader<FR, FW, BR, BW> {
    request: BackendProxyRequest<FR, FW>,
    backend_reader: BackendReader<BR>,
    backend_body_writer: jrpxy_backend::writer::body::Ready<BW>,
}

/// Backend responses come as a stream. Sometimes the stream is just the final
/// response. Other times, it may be a stream of 1xx Informational responses
/// before a final response. This encodes these possibilities.
pub enum BackendResponseStream<FR, FW, BR, BW> {
    /// A 1xx Informational response. Another response is expected to follow.
    Informational(BackendInformationalResponse<FR, FW, BR, BW>),
    /// The final response. This is the last one of the stream.
    Final(BackendFinalResponse<FR, FW, BR, BW>),
}

impl<FR, FW, BR, BW> ResponseReader<FR, FW, BR, BW>
where
    FR: AsyncReadExt + Unpin,
    BR: AsyncReadExt + Unpin,
    BW: AsyncWriteExt + Unpin,
{
    /// Read a backend response as a [`BackendResponseStream`].
    pub async fn read_backend_response(
        self,
    ) -> Result<BackendResponseStream<FR, FW, BR, BW>, BackendResponseError<FW>> {
        let Self {
            request,
            backend_reader,
            backend_body_writer,
        } = self;

        let BackendProxyRequest {
            request: _,
            body_reader: frontend_body_reader,
            pending,
        } = request;

        let mut request_body_forwarder = RequestBodyForwarder::new(
            pending.options.body_chunk_size(),
            frontend_body_reader,
            backend_body_writer,
        );

        let allow_body = !pending.client_options.is_head;
        let max_backend_head_length = pending.options.max_backend_head_length();
        let max_chunk_header_length = pending.options.max_chunk_header_length();

        // Drive the request body forwarder concurrently with the read of the
        // response head: some origins will not respond until they have received
        // the full request body, which would otherwise deadlock.
        let read_result = match request_body_forwarder
            .forward_while(backend_reader.read(
                allow_body,
                max_backend_head_length,
                max_chunk_header_length,
            ))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Err(BackendResponseError { pending, error: e });
            }
        };
        let response_stream = match read_result {
            Ok(r) => r,
            Err(error) => {
                // backend_reader and backend_body_writer are both discarded;
                // the backend connection is broken.
                return Err(BackendResponseError {
                    pending,
                    error: error.into(),
                });
            }
        };

        let response_stream = match ProxyResponseStream::new(response_stream) {
            Ok(s) => s,
            Err(e) => {
                return Err(BackendResponseError {
                    pending,
                    error: e.into(),
                });
            }
        };
        Ok(match response_stream {
            ProxyResponseStream::Final(proxy_response) => {
                BackendResponseStream::Final(BackendFinalResponse {
                    request_body_forwarder,
                    pending,
                    proxy_response,
                })
            }
            ProxyResponseStream::Informational(
                proxy_informational_response,
                proxy_stream_reader,
            ) => BackendResponseStream::Informational(BackendInformationalResponse {
                proxy_informational_response,
                request_body_forwarder,
                pending,
                proxy_stream_reader,
            }),
        })
    }
}

/// A result of trying to forward an informational response to the frontend.
pub enum InformationalForwardResult<FR, FW, BR, BW> {
    /// The informational response was forwarded to the frontend.
    Forwarded(BackendResponseStream<FR, FW, BR, BW>),
    /// The informational response was dropped as the frontend doesn't support
    /// informational responses (as is the case with HTTP/1.0 clients).
    Dropped(BackendResponseStream<FR, FW, BR, BW>),
}

impl<FR, FW, BR, BW> InformationalForwardResult<FR, FW, BR, BW> {
    /// Convert to the inner [`BackendResponseStream`].
    pub fn into_inner(self) -> BackendResponseStream<FR, FW, BR, BW> {
        match self {
            Self::Forwarded(r) | Self::Dropped(r) => r,
        }
    }
}

/// An informational response from a backend. Contains all the information
/// needed to forward the response to the frontend and then continue reading
/// more responses from the backend.
pub struct BackendInformationalResponse<FR, FW, BR, BW> {
    proxy_informational_response: BackendInformationalProxyResponse,
    request_body_forwarder: RequestBodyForwarder<FR, BW>,
    pending: PendingFrontendResponse<FW>,
    proxy_stream_reader: ProxyStreamReader<BR>,
}

impl<FR, FW, BR, BW> BackendInformationalResponse<FR, FW, BR, BW>
where
    FR: AsyncReadExt + Unpin,
    FW: AsyncWriteExt + Unpin,
    BR: AsyncReadExt + Unpin,
    BW: AsyncWriteExt + Unpin,
{
    /// A reference to the internal [`BackendInformationalProxyResponse`].
    pub fn as_response(&self) -> &BackendInformationalProxyResponse {
        &self.proxy_informational_response
    }

    /// A mutable reference to the internal [`BackendInformationalProxyResponse`].
    pub fn as_response_mut(&mut self) -> &mut BackendInformationalProxyResponse {
        &mut self.proxy_informational_response
    }

    /// Forward the informational response to the frontend. See
    /// [`InformationalForwardResult`] for possible successful outcomes.
    pub async fn forward_informational_response(
        self,
    ) -> Result<InformationalForwardResult<FR, FW, BR, BW>, InformationalForwardError<FW>> {
        let Self {
            proxy_informational_response,
            mut request_body_forwarder,
            pending,
            proxy_stream_reader,
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

        // Drive the request body forwarder concurrently with the read of the
        // next backend response head. After a 100 Continue is delivered to the
        // client, the client may now begin streaming the request body that the
        // backend is waiting on. Awaiting the next backend read without pumping
        // the body deadlocks: backend waits on body, client waits on
        // back-pressure relief, proxy waits on backend.
        let read_result = match request_body_forwarder
            .forward_while(proxy_stream_reader.read())
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Err(InformationalForwardError::Backend(BackendStreamError {
                    pending,
                    error: e,
                }));
            }
        };
        let response_stream = match read_result {
            Ok(r) => r,
            Err(error) => {
                return Err(InformationalForwardError::Backend(BackendStreamError {
                    pending,
                    error: error.into(),
                }));
            }
        };

        let response_stream = match response_stream {
            ProxyResponseStream::Final(proxy_response) => {
                BackendResponseStream::Final(BackendFinalResponse {
                    request_body_forwarder,
                    pending,
                    proxy_response,
                })
            }
            ProxyResponseStream::Informational(
                proxy_informational_response,
                proxy_stream_reader,
            ) => BackendResponseStream::Informational(Self {
                proxy_informational_response,
                request_body_forwarder,
                pending,
                proxy_stream_reader,
            }),
        };

        Ok(if client_supports_informational_response {
            InformationalForwardResult::Forwarded(response_stream)
        } else {
            InformationalForwardResult::Dropped(response_stream)
        })
    }
}

/// The last backend response in a response stream.
pub struct BackendFinalResponse<FR, FW, BR, BW> {
    request_body_forwarder: RequestBodyForwarder<FR, BW>,
    pending: PendingFrontendResponse<FW>,
    proxy_response: BackendFinalProxyResponse<BR>,
}

impl<FR, FW, BR, BW> BackendFinalResponse<FR, FW, BR, BW>
where
    FR: AsyncReadExt + Unpin,
    FW: AsyncWriteExt + Unpin,
    BR: AsyncReadExt + Unpin,
    BW: AsyncWriteExt + Unpin,
{
    /// The [`BackendFinalProxyResponse`] associated with this backend response.
    pub fn as_response(&self) -> &BackendFinalProxyResponse<BR> {
        &self.proxy_response
    }

    /// A mutable reference to the [`BackendFinalProxyResponse`] associated with this
    /// backend response.
    pub fn as_response_mut(&mut self) -> &mut BackendFinalProxyResponse<BR> {
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
            request_body_forwarder,
            pending,
            proxy_response,
        } = self;

        let is_head = pending.client_options.is_head;
        let client_cannot_chunk = pending.client_options.version == HttpVersion::Http10;
        let chunk_size = pending.options.body_chunk_size();
        let (response, backend_body_reader) = proxy_response.into_frontend_response();
        let (options, frontend_body_writer) = match &backend_body_reader {
            BackendBodyReader::TE(_) => {
                if client_cannot_chunk {
                    let (opt, writer) = pending.send_as_eof(response).await?;
                    (opt, jrpxy_frontend::writer::body::Ready::from(writer))
                } else {
                    let (opt, writer) = pending.send_as_chunked(response).await?;
                    (opt, jrpxy_frontend::writer::body::Ready::from(writer))
                }
            }
            BackendBodyReader::CL(r) => {
                let (opt, writer) = pending
                    .send_as_content_length(response, r.content_length())
                    .await?;
                (opt, jrpxy_frontend::writer::body::Ready::from(writer))
            }
            BackendBodyReader::Bodyless(_) => {
                // HEAD responses and 304 Not Modified carry framing headers
                // that describe the representation, not an actual body; those
                // must be forwarded. All other bodyless codes (204, etc.) must
                // not carry framing headers.
                if is_head || response.code() == 304 {
                    let (opt, writer) = pending.send_as_bodyless_keep_framing(response).await?;
                    (opt, jrpxy_frontend::writer::body::Ready::from(writer))
                } else {
                    let (opt, writer) = pending.send_as_no_content(response).await?;
                    (opt, jrpxy_frontend::writer::body::Ready::from(writer))
                }
            }
            // The backend sent an EOF-terminated body (no framing headers). If
            // supported, re-frame as chunked when forwarding to the frontend so
            // the frontend connection can remain open.
            BackendBodyReader::Eof(_) => {
                if client_cannot_chunk {
                    let (opt, writer) = pending.send_as_eof(response).await?;
                    (opt, jrpxy_frontend::writer::body::Ready::from(writer))
                } else {
                    let (opt, writer) = pending.send_as_chunked(response).await?;
                    (opt, jrpxy_frontend::writer::body::Ready::from(writer))
                }
            }
        };

        let response_body_forwarder =
            ResponseBodyForwarder::new(chunk_size, backend_body_reader, frontend_body_writer);

        Ok(BodyExchanger {
            options,
            request_body_forwarder,
            response_body_forwarder,
        })
    }
}

/// Drives the bidirectional body exchange between the frontend and backend
/// after the response head has been forwarded.
///
/// Produced by [`BackendFinalResponse::forward_response_head`].
pub struct BodyExchanger<FR, FW, BR, BW> {
    options: ProxyOptions,
    request_body_forwarder: RequestBodyForwarder<FR, BW>,
    response_body_forwarder: ResponseBodyForwarder<BR, FW>,
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
    /// Forwards the request body from the frontend to the backend.
    pub request_body_forwarder: RequestBodyForwarder<FR, BW>,
    /// Forwards the response body from the backend to the frontend.
    pub response_body_forwarder: ResponseBodyForwarder<BR, FW>,
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
            request_body_forwarder,
            response_body_forwarder,
        } = self;
        BodyExchangerParts {
            options,
            request_body_forwarder,
            response_body_forwarder,
        }
    }

    /// Drive the body copy in both directions concurrently and return a
    /// [`ProxyClient`] ready for the next request.
    pub async fn finish(
        self,
    ) -> ProxyResult<(
        Option<ProxyClient<FR, FW>>,
        Option<BackendConnection<BR, BW>>,
    )> {
        let Self {
            options,
            request_body_forwarder,
            response_body_forwarder,
        } = self;

        let f2b_fut = request_body_forwarder.finish();
        let b2f_fut = response_body_forwarder.finish();

        let mut f2b_fut_pinned = std::pin::pin!(f2b_fut);
        let mut b2f_fut_pinned = std::pin::pin!(b2f_fut);

        let mut f2b_res = None;
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
                // TODO: there's also an argument that we should drive the
                // frontend-to-backend body to completion and rely on the origin
                // to hang up on us if it wants to terminate early.
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
            Some(Err(e)) => return Err(ProxyError::CopyError(e)),
            None => {
                return Err(ProxyError::CopyError(
                    ProxyCopyError::FrontendCopyIncomplete,
                ));
            }
        };

        let backend_connection = backend_reader.map(move |br| (br, backend_writer));
        let proxy_client =
            frontend_reader
                .zip(frontend_writer)
                .map(move |(frontend_reader, frontend_writer)| ProxyClient {
                    frontend_reader,
                    frontend_writer,
                    options,
                });

        Ok((proxy_client, backend_connection))
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

fn get_connection_tokens(headers: &Headers) -> Result<Vec<Bytes>, ConnectionTokenParserError> {
    headers.connection_tokens().collect()
}

fn is_hop_by_hop(name: &[u8], connection_tokens: &[Bytes]) -> bool {
    is_standard_hop_by_hop(name)
        || connection_tokens
            .iter()
            .any(|t| t.as_ref().eq_ignore_ascii_case(name))
}

/// Remove all hop-by-hop headers from `headers` in place (RFC 9110 7.6.1,
/// RFC 9112).
fn strip_hop_by_hop_headers(headers: &mut Headers, connection_tokens: &[Bytes]) {
    headers.retain(|(name, _)| !is_hop_by_hop(name, connection_tokens));
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

/// Inject a `Date` header (RFC 9110 sec 6.6.1) into `headers` if one is not
/// already present. The gateway acts as origin server to the client, so a
/// final response missing a Date from the backend must be stamped before
/// being forwarded. An existing Date is preserved untouched - the backend's
/// timestamp is closer to message generation than ours.
fn insert_date_header_if_absent(headers: &mut Headers) {
    if headers.get_header("date").next().is_some() {
        return;
    }
    let now = httpdate::fmt_http_date(std::time::SystemTime::now());
    headers.push(Bytes::from_static(b"Date"), Bytes::from(now));
}

/// A proxy request received from the frontend.
///
/// Holds both the request as received and a separate copy that the proxy
/// rewrites for forwarding. Keeping the as-received copy untouched lets
/// callers compare wire bytes against proxy output - abuse detection,
/// fingerprinting, and audit logging rely on the client's exact behavior,
/// which proxy normalization would otherwise erase.
pub struct FrontendProxyRequest<FR, FW> {
    frontend: FrontendRequest<FR>,
    original: Request,
    pending: PendingFrontendResponse<FW>,
}

impl<FR, FW> FrontendProxyRequest<FR, FW> {
    fn new(
        mut frontend: FrontendRequest<FR>,
        pending: PendingFrontendResponse<FW>,
        connection_tokens: &[Bytes],
    ) -> Self {
        let original = frontend.req().clone();
        strip_hop_by_hop_headers(frontend.req_mut().headers_mut(), connection_tokens);
        Self {
            frontend,
            original,
            pending,
        }
    }

    /// Build a [`FrontendProxyRequest`] from an already-parsed
    /// [`FrontendRequest`]. Runs the same validation, hop-by-hop stripping, and
    /// request-target normalization as [`ProxyClient::start`], but skips the
    /// I/O step - useful when the request head was obtained from a source other
    /// than a [`FrontendReader`] (e.g. an alternate protocol front-end, a test
    /// fixture, or a higher-level dispatcher), or when the user wants to
    /// pre-filter requests before sending them into the proxy.
    ///
    /// The caller is responsible for any size limits on the request head;
    /// the `max_frontend_head_length` field of [`ProxyOptions`] is not
    /// consulted on this path.
    // The `Err` variant is intentionally large - it carries the recoverable
    // state so the caller can send a custom error response. Boxing it would
    // be a cold-path-only ergonomics tax and a public-API break, matching the
    // existing `large_enum_variant` allowance on FrontendRequestError above.
    #[allow(clippy::result_large_err)]
    pub fn from_request(
        request: FrontendRequest<FR>,
        writer: FrontendWriter<FW>,
        options: ProxyOptions,
    ) -> Result<Self, FrontendRequestError<FR, FW>> {
        let is_head = request.req().method() == b"HEAD".as_slice();
        let version = request.req().version();
        let connection_tokens = match get_connection_tokens(request.req().headers()) {
            Ok(c) => c,
            Err(e) => {
                return Err(FrontendRequestError::new_frontend(
                    writer,
                    options,
                    version,
                    e.into(),
                ));
            }
        };

        // RFC 9112 section 6.1: A server or client that receives an HTTP/1.0
        // message containing a Transfer-Encoding header field MUST treat the
        // message as if the framing is faulty, even if a Content-Length is
        // present, and close the connection after processing the message. The
        // message sender might have retained a portion of the message, in
        // buffer, that could be misinterpreted by further use of the
        // connection.
        if version == HttpVersion::Http10
            && request
                .req()
                .headers()
                .get_header("transfer-encoding")
                .next()
                .is_some()
        {
            return Err(FrontendRequestError::new_frontend(
                writer,
                options,
                version,
                ProxyFrontendError::TransferEncodingOnHttp10Client,
            ));
        }

        let pending = PendingFrontendResponse {
            frontend_writer: writer,
            options,
            client_options: ClientOptions { is_head, version },
        };
        let mut frontend_request = Self::new(request, pending, &connection_tokens);

        // Method rejections run first, ahead of request-target classification.
        // CONNECT must be handled independently. OPTIONS and TRACE may target
        // the gateway/proxy itself, so they cannot be blindly forwarded. The
        // caller must intercept all three before the proxy decision.
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
        if method == b"OPTIONS".as_slice() {
            return Err(FrontendRequestError::new_request(
                frontend_request,
                ProxyFrontendError::OptionsNotSupported,
            ));
        }

        if let Err(e) = request_target::normalize(frontend_request.req_mut()) {
            return Err(FrontendRequestError::new_request(frontend_request, e));
        }

        // RFC 9110 section 7.2: a server MUST respond 400 to any request with
        // more than one Host field, regardless of version. Multiple Hosts let
        // the proxy and origin disagree on routing/authorization, so reject
        // unconditionally. The presence requirement is HTTP/1.1-only per RFC
        // 9112 section 3.2.2; HTTP/1.0 may omit Host. After absolute-form
        // normalization above, every forward-eligible request reaches this
        // check with the Host derived from the request-target authority
        // (absolute-form) or from the client's own Host header (origin-form /
        // asterisk-form).
        let host_count = frontend_request.req().headers().get_header("host").count();
        if host_count > 1 {
            return Err(FrontendRequestError::new_request(
                frontend_request,
                ProxyFrontendError::MultipleHosts,
            ));
        }
        if version == HttpVersion::Http11 && host_count == 0 {
            return Err(FrontendRequestError::new_request(
                frontend_request,
                ProxyFrontendError::MissingHost,
            ));
        }

        Ok(frontend_request)
    }

    /// The rewritten request that will be forwarded upstream.
    pub fn req(&self) -> &Request {
        self.frontend.req()
    }

    /// Mutable access to the rewritten request. Changes here do not affect
    /// [`original`], so callers can mutate freely without losing the
    /// as-received view.
    ///
    /// [`original`]: FrontendProxyRequest::original
    pub fn req_mut(&mut self) -> &mut Request {
        self.frontend.req_mut()
    }

    /// The request as received, before proxy-side rewriting. Read-only so the
    /// snapshot stays trustworthy as evidence of what the client actually
    /// sent.
    pub fn original(&self) -> &Request {
        &self.original
    }

    /// Convert this frontend request into a [`BackendProxyRequest`] ready to
    /// write to a backend connection.
    ///
    /// Proxy headers (e.g. `Via`) are merged in using the `received_by`
    /// identifier from [`ProxyOptions`].
    pub fn into_backend_request(self) -> BackendProxyRequest<FR, FW> {
        let Self {
            mut frontend,
            original: _,
            pending,
        } = self;
        let version = frontend.req().version();
        insert_proxy_headers(
            frontend.req_mut().headers_mut(),
            version,
            pending.options.received_by(),
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
        let Self {
            frontend: _,
            original: _,
            pending,
        } = self;
        pending
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

    /// Write this backend request to the given backend connection.
    ///
    /// On failure, returns a [`BackendRequestError`] that carries this request
    /// intact so the caller can recover it (via
    /// [`BackendRequestError::into_backend_request`]) and either retry against
    /// a different backend or send an error response to the frontend.
    pub async fn forward<BR, BW>(
        self,
        backend_connection: BackendConnection<BR, BW>,
    ) -> Result<ResponseReader<FR, FW, BR, BW>, BackendRequestError<FR, FW>>
    where
        BR: AsyncReadExt + Unpin,
        BW: AsyncWriteExt + Unpin,
    {
        let (backend_reader, backend_writer) = backend_connection;
        let write_result = match &self.body_reader {
            FrontendBodyReader::TE(_) => backend_writer
                .send_as_chunked(&self.request)
                .await
                .map(jrpxy_backend::writer::body::Ready::Chunked),
            FrontendBodyReader::CL(r) => backend_writer
                .send_as_content_length(&self.request, r.content_length())
                .await
                .map(jrpxy_backend::writer::body::Ready::CL),
            FrontendBodyReader::Bodyless(_) => backend_writer
                .send_as_bodyless(&self.request)
                .await
                .map(jrpxy_backend::writer::body::Ready::Bodyless),
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
/// Holds both the response as received and a separate copy that the proxy
/// rewrites for the frontend. The as-received copy is kept untouched so
/// callers can compare wire bytes against forwarded bytes - useful for
/// observability and audit paths where proxy normalization would otherwise
/// erase information.
pub struct BackendFinalProxyResponse<I> {
    backend: BackendReaderResponse<I>,
    original: Response,
}

impl<I> BackendFinalProxyResponse<I> {
    /// The rewritten response that will be forwarded to the frontend.
    pub fn res(&self) -> &Response {
        self.backend.res()
    }

    /// Mutable access to the rewritten response. Changes here do not affect
    /// [`original`], so callers can mutate freely without losing the
    /// as-received view.
    ///
    /// [`original`]: BackendFinalProxyResponse::original
    pub fn res_mut(&mut self) -> &mut Response {
        self.backend.res_mut()
    }

    /// The response as received from the backend, before proxy-side
    /// rewriting. Read-only so the snapshot stays trustworthy as evidence of
    /// what the backend actually sent.
    pub fn original(&self) -> &Response {
        &self.original
    }

    /// Convert the [`BackendFinalProxyResponse`] into a response ready to
    /// send to the frontend. Hop-by-hop headers have already been removed.
    pub fn into_frontend_response(self) -> (Response, BackendBodyReader<I>) {
        let Self {
            backend,
            original: _,
        } = self;
        backend.into_parts()
    }
}

/// The proxy-processed form of a 1xx informational response.
///
/// Holds both the response as received and a separate copy that the proxy
/// rewrites for the frontend. The as-received copy is kept untouched so
/// callers can compare wire bytes against forwarded bytes.
pub struct BackendInformationalProxyResponse {
    res: Response,
    original: Response,
}

impl BackendInformationalProxyResponse {
    /// The rewritten response that will be forwarded to the frontend.
    pub fn res(&self) -> &Response {
        &self.res
    }

    /// Mutable access to the rewritten response.
    pub fn res_mut(&mut self) -> &mut Response {
        &mut self.res
    }

    /// The response as received from the backend, before proxy-side
    /// rewriting.
    pub fn original(&self) -> &Response {
        &self.original
    }

    /// Convert the [`BackendInformationalProxyResponse`] into a response
    /// ready to send to the frontend. Hop-by-hop headers have already been
    /// removed.
    pub fn into_frontend_response(self) -> Response {
        let Self { res, original: _ } = self;
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

impl<I> ProxyStreamReader<I>
where
    I: AsyncReadExt + Unpin,
{
    /// Read the next response in the stream.
    pub async fn read(self) -> Result<ProxyResponseStream<I>, ProxyBackendError> {
        let stream = self.reader.read().await?;
        let stream = ProxyResponseStream::new(stream)?;
        Ok(stream)
    }
}

/// The proxy-processed form of [`ResponseStream`].
///
/// Each variant has had its hop-by-hop headers stripped.
pub enum ProxyResponseStream<I> {
    /// A regular (non-1xx) response.
    Final(BackendFinalProxyResponse<I>),
    /// A 1xx informational response. More responses can be read from the
    /// accompanying [`ProxyStreamReader`].
    Informational(BackendInformationalProxyResponse, ProxyStreamReader<I>),
}

impl<I> ProxyResponseStream<I> {
    fn new(stream: ResponseStream<I>) -> Result<Self, ProxyBackendError> {
        match stream {
            ResponseStream::Response(mut backend) => {
                let connection_tokens = get_connection_tokens(backend.res().headers())?;
                let original = backend.res().clone();
                strip_hop_by_hop_headers(backend.res_mut().headers_mut(), &connection_tokens);
                Ok(Self::Final(BackendFinalProxyResponse { backend, original }))
            }
            ResponseStream::Informational(mut res, reader) => {
                let connection_tokens = get_connection_tokens(res.headers())?;
                let original = res.clone();
                strip_hop_by_hop_headers(res.headers_mut(), &connection_tokens);
                Ok(Self::Informational(
                    BackendInformationalProxyResponse { res, original },
                    ProxyStreamReader { reader },
                ))
            }
        }
    }
}

impl<I> std::fmt::Debug for ProxyResponseStream<I>
where
    I: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Final(_) => write!(f, "Final"),
            Self::Informational(_, _) => write!(f, "Informational"),
        }
    }
}

#[cfg(test)]
mod test {
    use jrpxy_backend::{reader::BackendReader, writer::BackendWriter};
    use jrpxy_frontend::{reader::FrontendReader, writer::FrontendWriter};
    use jrpxy_http_message::{message::ResponseBuilder, version::HttpVersion};

    use crate::{
        BackendResponseStream, ClientOptions, FrontendRequestError, InformationalForwardResult,
        PendingFrontendResponse, ProxyClient, ProxyFrontendError, ProxyOptions,
        get_connection_tokens,
    };

    use super::{
        BackendFinalProxyResponse, BackendInformationalProxyResponse, BackendProxyRequest,
        FrontendProxyRequest, ProxyResponseStream, ProxyStreamReader,
    };

    fn into_response<I>(s: ProxyResponseStream<I>) -> BackendFinalProxyResponse<I> {
        match s {
            ProxyResponseStream::Final(r) => r,
            ProxyResponseStream::Informational(_, _) => panic!("expected Response variant"),
        }
    }

    fn into_informational<I>(
        s: ProxyResponseStream<I>,
    ) -> (BackendInformationalProxyResponse, ProxyStreamReader<I>) {
        match s {
            ProxyResponseStream::Informational(res, reader) => (res, reader),
            ProxyResponseStream::Final(_) => panic!("expected Informational variant"),
        }
    }

    async fn make_frontend_proxy_request(
        raw: &[u8],
    ) -> FrontendProxyRequest<&[u8], tokio::io::Sink> {
        let reader = FrontendReader::new(raw, 256);
        let req = reader.read(8192, 8192).await.expect("valid request");
        let is_head = req.req().method() == b"HEAD".as_slice();
        let version = req.req().version();
        let pending = PendingFrontendResponse {
            frontend_writer: FrontendWriter::new(tokio::io::sink()),
            options: ProxyOptions::default(),
            client_options: ClientOptions { is_head, version },
        };
        let connection_tokens =
            get_connection_tokens(req.req().headers()).expect("failed to get connection tokens");
        FrontendProxyRequest::new(req, pending, &connection_tokens)
    }

    async fn make_backend_proxy_request(raw: &[u8]) -> BackendProxyRequest<&[u8], tokio::io::Sink> {
        make_frontend_proxy_request(raw)
            .await
            .into_backend_request()
    }

    async fn make_proxy_response(raw: &[u8]) -> ProxyResponseStream<&[u8]> {
        let reader = BackendReader::new(raw, 256);
        let stream = reader.read(true, 8192, 8192).await.expect("valid response");
        ProxyResponseStream::new(stream).expect("failed to build response stream")
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
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
            y-hdr: 10\r\n\
            connection: filter-res\r\n\
            filter-res: 20\r\n\
            content-length: 5\r\n\
            \r\n\
            01234";

        let mut backend_writer = Vec::new();

        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref(), 256),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let did_fe_read = proxy_client.start().await.expect("client start failed");

        let backend_connection = (
            BackendReader::new(backend_reader.as_ref(), 256),
            BackendWriter::new(&mut backend_writer),
        );
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
            BackendResponseStream::Final(_read_backend_response) => {
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
                .res()
                .headers()
                .get_header("x-hdr")
                .next()
                .unwrap()
                .1
        );

        // Verify the original informational response retains its hop-by-hop
        // headers.
        assert_eq!(
            b"filter".as_slice(),
            rbir.as_response()
                .original()
                .headers()
                .get_header("connection")
                .next()
                .unwrap()
                .1
        );
        assert_eq!(
            b"2".as_slice(),
            rbir.as_response()
                .original()
                .headers()
                .get_header("filter")
                .next()
                .unwrap()
                .1
        );

        // Ensure the hop-by-hop headers are NOT in the end-to-end headers
        assert!(
            rbir.as_response()
                .res()
                .headers()
                .get_header("filter")
                .next()
                .is_none()
        );
        assert!(
            rbir.as_response()
                .res()
                .headers()
                .get_header("connection")
                .next()
                .is_none()
        );

        let be_res_stat = rbir
            .forward_informational_response()
            .await
            .expect("failed to forward informational response");

        let rbr = match be_res_stat.into_inner() {
            BackendResponseStream::Final(rbr) => rbr,
            BackendResponseStream::Informational(_rbir) => {
                panic!("incorrect response type")
            }
        };

        // Verify we can read the end-to-end header in the response
        assert_eq!(
            b"10".as_slice(),
            rbr.as_response()
                .res()
                .headers()
                .get_header("y-hdr")
                .next()
                .unwrap()
                .1
        );

        // Verify the original final response retains its hop-by-hop headers.
        assert_eq!(
            b"filter-res".as_slice(),
            rbr.as_response()
                .original()
                .headers()
                .get_header("connection")
                .next()
                .unwrap()
                .1
        );
        assert_eq!(
            b"20".as_slice(),
            rbr.as_response()
                .original()
                .headers()
                .get_header("filter-res")
                .next()
                .unwrap()
                .1
        );

        // Ensure the hop-by-hop headers are NOT in the end-to-end headers
        assert!(
            rbr.as_response()
                .res()
                .headers()
                .get_header("filter-res")
                .next()
                .is_none()
        );
        assert!(
            rbr.as_response()
                .res()
                .headers()
                .get_header("connection")
                .next()
                .is_none()
        );

        let (client, backend_connection) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
            .await
            .expect("failed to forward response");
        let client = client.expect("failed to recycle client");
        let (_backend_reader, backend_writer) = backend_connection.unwrap();

        let (_frontend_reader, frontend_writer) = client.into_parts();
        let frontend_writer = frontend_writer.into_inner();
        let backend_writer = backend_writer.into_inner();

        let expected_backend_writer = b"\
            GET / HTTP/1.1\r\n\
            Host: example.com\r\n\
            Via: 1.1 jrpxy\r\n\
            \r\n";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected_backend_writer.as_slice()),
            jrpxy_util::debug::AsciiDebug(backend_writer)
        );

        let expected_frontend_writer = b"\
            HTTP/1.1 100 Continue\r\n\
            x-hdr: 1\r\n\
            Via: 1.1 jrpxy\r\n\
            \r\n\
            HTTP/1.1 200 Ok\r\n\
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
            y-hdr: 10\r\n\
            Via: 1.1 jrpxy\r\n\
            content-length: 5\r\n\
            \r\n\
            01234";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected_frontend_writer.as_slice()),
            jrpxy_util::debug::AsciiDebug(frontend_writer)
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
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
            content-length: 5\r\n\
            \r\n\
            01234";

        let mut backend_writer = Vec::new();

        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref(), 256),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );

        let did_fe_read = proxy_client.start().await.expect("client start failed");

        let backend_connection = (
            BackendReader::new(backend_reader.as_ref(), 256),
            BackendWriter::new(&mut backend_writer),
        );
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
            BackendResponseStream::Final(_) => panic!("incorrect response type"),
            BackendResponseStream::Informational(rbir) => rbir,
        };

        let ifr = rbir
            .forward_informational_response()
            .await
            .expect("failed to forward");
        let be_res_stat = match ifr {
            crate::InformationalForwardResult::Dropped(r) => r,
            crate::InformationalForwardResult::Forwarded(_r) => {
                panic!("forwarded response when we expected a drop")
            }
        };

        let rbr = match be_res_stat {
            BackendResponseStream::Final(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("incorrect response type"),
        };

        let (client, _backend_connection) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
            .await
            .expect("failed to forward response");

        // An HTTP/1.0 client that did not send `Connection: keep-alive` is not
        // eligible for reuse (RFC 9112 section 9.3).
        assert!(client.is_none(), "HTTP/1.0 client must not be recycled");

        // The 1xx informational response must NOT be forwarded to an HTTP/1.0 client.
        // Via uses "1.1" because the backend responded with HTTP/1.1.
        let expected_frontend_writer = b"\
            HTTP/1.1 200 Ok\r\n\
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
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
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
            content-length: 5\r\n\
            \r\n\
            hello";
        let mut backend_writer = Vec::new();

        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref(), 256),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );
        let backend_connection = (
            BackendReader::new(backend_reader.as_ref(), 256),
            BackendWriter::new(&mut backend_writer),
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
            BackendResponseStream::Final(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, _backend_connection) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
            .await
            .expect("failed to forward response");
        let client = client.expect("failed to recycle client");

        let (_frontend_reader, frontend_writer) = client.into_parts();
        let frontend_writer = frontend_writer.into_inner();

        let expected_frontend_writer = b"\
            HTTP/1.1 200 Ok\r\n\
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
            Via: 1.1 jrpxy\r\n\
            content-length: 5\r\n\
            \r\n\
            hello";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected_frontend_writer.as_slice()),
            jrpxy_util::debug::AsciiDebug(frontend_writer)
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
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
            content-length: 0\r\n\
            \r\n";
        let mut backend_writer = Vec::new();

        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref(), 256),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );
        let backend_connection = (
            BackendReader::new(backend_reader.as_ref(), 256),
            BackendWriter::new(&mut backend_writer),
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

        let rbir = match be_res_stat {
            BackendResponseStream::Informational(rbir) => rbir,
            BackendResponseStream::Final(_) => panic!("expected informational"),
        };

        let be_res_stat = rbir
            .forward_informational_response()
            .await
            .expect("failed to forward informational")
            .into_inner();

        let rbr = match be_res_stat {
            BackendResponseStream::Final(rbr) => rbr,
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
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
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

        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref(), 256),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );
        let backend_connection = (
            BackendReader::new(backend_reader.as_ref(), 256),
            BackendWriter::new(&mut backend_writer),
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
            BackendResponseStream::Final(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, backend_connection) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
            .await
            .expect("failed to forward response");
        let (_backend_reader, backend_writer) = backend_connection.unwrap();
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
            jrpxy_util::debug::AsciiDebug(backend_writer)
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

        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref(), 256),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );
        let backend_connection = (
            BackendReader::new(backend_reader.as_ref(), 256),
            BackendWriter::new(&mut backend_writer),
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
            BackendResponseStream::Final(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (_client, backend_connection) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
            .await
            .expect("failed to forward response");
        let (_backend_reader, backend_writer) = backend_connection.unwrap();
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
            jrpxy_util::debug::AsciiDebug(backend_writer)
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
            x-trailer: some-value\r\n\
            \r\n";
        let mut frontend_writer = Vec::new();

        let backend_reader = b"\
            HTTP/1.1 200 Ok\r\n\
            content-length: 0\r\n\
            \r\n";
        let mut backend_writer = Vec::new();

        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref(), 256),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );
        let backend_connection = (
            BackendReader::new(backend_reader.as_ref(), 256),
            BackendWriter::new(&mut backend_writer),
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
            BackendResponseStream::Final(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (_client, backend_connection) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
            .await
            .expect("failed to forward response");
        let (_backend_reader, backend_writer) = backend_connection.unwrap();
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
            x-trailer: some-value\r\n\
            \r\n";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected_backend_writer.as_slice()),
            jrpxy_util::debug::AsciiDebug(backend_writer)
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
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
            transfer-encoding: chunked\r\n\
            \r\n\
            5\r\n\
            hello\r\n\
            0\r\n\
            \r\n";
        let mut backend_writer = Vec::new();

        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref(), 256),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );
        let backend_connection = (
            BackendReader::new(backend_reader.as_ref(), 256),
            BackendWriter::new(&mut backend_writer),
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
            BackendResponseStream::Final(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, _backend_connection) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
            .await
            .expect("failed to forward response");
        let client = client.expect("failed to recycle client");

        let (_frontend_reader, frontend_writer) = client.into_parts();
        let frontend_writer = frontend_writer.into_inner();

        let expected_frontend_writer = b"\
            HTTP/1.1 200 Ok\r\n\
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
            Via: 1.1 jrpxy\r\n\
            transfer-encoding: chunked\r\n\
            \r\n\
            5\r\n\
            hello\r\n\
            0\r\n\
            \r\n";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected_frontend_writer.as_slice()),
            jrpxy_util::debug::AsciiDebug(frontend_writer)
        );
    }

    #[tokio::test]
    async fn chunked_backend_response_downgraded_for_http10_client() {
        // When a backend responds with 'transfer-encoding: chunked' but the
        // frontend client is HTTP/1.0, the proxy must not forward chunked
        // encoding (RFC 9112 section 6.1). Instead it should decode the chunks
        // from the backend, and write the decoded stream of bytes to the
        // frontend. The end of the body should be indicated by closing the
        // frontend connection. The response head must have no transfer-encoding
        // or content-length header, and the returned client must be None (the
        // connection cannot be reused).
        let frontend_reader = b"\
            GET / HTTP/1.0\r\n\
            Host: example.com\r\n\
            \r\n";
        let mut frontend_writer = Vec::new();

        let backend_reader = b"\
            HTTP/1.1 200 Ok\r\n\
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
            transfer-encoding: chunked\r\n\
            \r\n\
            5\r\n\
            hello\r\n\
            0\r\n\
            \r\n";
        let mut backend_writer = Vec::new();

        let backend_reader = BackendReader::new(backend_reader.as_ref(), 128);
        let backend_writer = BackendWriter::new(&mut backend_writer);
        let backend_connection = (backend_reader, backend_writer);

        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref(), 256),
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
            BackendResponseStream::Final(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, _backend_connection) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
            .await
            .expect("failed to forward response");

        // The connection must not be recycled after an EOF-framed response.
        assert!(
            client.is_none(),
            "expected client to be None for HTTP/1.0 EOF response"
        );

        let expected_frontend_writer = b"\
            HTTP/1.1 200 Ok\r\n\
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
            Via: 1.1 jrpxy\r\n\
            \r\n\
            hello";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected_frontend_writer.as_slice()),
            jrpxy_util::debug::AsciiDebug(&frontend_writer)
        );
    }

    #[tokio::test]
    async fn eof_from_backend_needs_to_be_eof_for_http10_client() {
        // When a backend sends us an EOF encoded response, and we have a
        // request from an HTTP/1.0 client, we need to use EOF encoding to the
        // client as well.
        let frontend_reader = b"\
            GET / HTTP/1.0\r\n\
            Host: example.com\r\n\
            \r\n";
        let mut frontend_writer = Vec::new();

        let backend_reader = b"\
            HTTP/1.0 200 Ok\r\n\
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
            \r\n\
            hello";
        let mut backend_writer = Vec::new();

        let backend_reader = BackendReader::new(backend_reader.as_ref(), 128);
        let backend_writer = BackendWriter::new(&mut backend_writer);
        let backend_connection = (backend_reader, backend_writer);

        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref(), 256),
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
            BackendResponseStream::Final(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, _backend_connection) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
            .await
            .expect("failed to forward response");

        // The connection must not be recycled after an EOF-framed response.
        assert!(
            client.is_none(),
            "expected client to be None for HTTP/1.0 EOF response"
        );

        let expected_frontend_writer = b"\
            HTTP/1.1 200 Ok\r\n\
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
            Via: 1.0 jrpxy\r\n\
            \r\n\
            hello";
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
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
            transfer-encoding: chunked\r\n\
            \r\n\
            5\r\n\
            hello\r\n\
            0\r\n\
            x-trailer: some-value\r\n\
            \r\n";
        let mut backend_writer = Vec::new();

        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref(), 256),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );
        let backend_connection = (
            BackendReader::new(backend_reader.as_ref(), 256),
            BackendWriter::new(&mut backend_writer),
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
            BackendResponseStream::Final(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, _backend_connection) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
            .await
            .expect("failed to forward response");
        let client = client.expect("failed to recycle client");

        let (_frontend_reader, frontend_writer) = client.into_parts();
        let frontend_writer = frontend_writer.into_inner();

        // The trailer field must appear between the terminal chunk and the
        // final CRLF, i.e. `0\r\nx-trailer: some-value\r\n\r\n`.
        let expected_frontend_writer = b"\
            HTTP/1.1 200 Ok\r\n\
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
            Via: 1.1 jrpxy\r\n\
            transfer-encoding: chunked\r\n\
            \r\n\
            5\r\n\
            hello\r\n\
            0\r\n\
            x-trailer: some-value\r\n\
            \r\n";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected_frontend_writer.as_slice()),
            jrpxy_util::debug::AsciiDebug(frontend_writer)
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
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
            content-length: 512\r\n\
            etag: \"abc\"\r\n\
            \r\n\
            HTTP/1.1 200 Ok\r\n\
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
            content-length: 5\r\n\
            \r\n\
            hello";
        let mut backend_writer = Vec::new();

        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref(), 256),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );
        let backend_connection = (
            BackendReader::new(backend_reader.as_ref(), 256),
            BackendWriter::new(&mut backend_writer),
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
            BackendResponseStream::Final(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, backend_connection) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
            .await
            .expect("failed to forward response");
        let backend_connection = backend_connection.unwrap();
        let client = client.expect("failed to recycle client");

        // Release the mutable borrow on frontend_writer before asserting.
        let (frontend_reader, _) = client.into_parts();

        // content-length must be forwarded (RFC 9110 section 15.4.5).
        let expected = b"\
            HTTP/1.1 304 Not Modified\r\n\
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
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
            BackendResponseStream::Final(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, _) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
            .await
            .expect("second forward failed");
        let client = client.expect("failed to recycle client");

        let (_, fw) = client.into_parts();
        let fw = fw.into_inner();

        // For a normal response with a body the proxy rewrites the framing
        // header, so content-length is appended after Via rather than
        // preserved in its original position.
        let expected_second = b"\
            HTTP/1.1 200 Ok\r\n\
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
            Via: 1.1 jrpxy\r\n\
            content-length: 5\r\n\
            \r\nhello";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected_second.as_slice()),
            jrpxy_util::debug::AsciiDebug(fw),
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
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
            content-length: 5\r\n\
            \r\n\
            HTTP/1.1 200 Ok\r\n\
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
            content-length: 5\r\n\
            \r\n\
            01234";
        let mut backend_writer = Vec::new();

        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref(), 256),
            FrontendWriter::new(&mut frontend_writer),
            ProxyOptions::default(),
        );
        let backend_connection = (
            BackendReader::new(backend_reader.as_ref(), 256),
            BackendWriter::new(&mut backend_writer),
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
            BackendResponseStream::Final(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, backend_connection) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
            .await
            .expect("failed to forward response");
        let backend_connection = backend_connection.unwrap();
        let client = client.expect("failed to recycle client");

        let (frontend_reader, frontend_writer) = client.into_parts();
        let frontend_writer = frontend_writer.into_inner();

        // The content-length header must be preserved (the client needs to
        // know the representation size) but no body bytes must follow.
        let expected_frontend_writer = b"\
            HTTP/1.1 200 Ok\r\n\
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
            content-length: 5\r\n\
            Via: 1.1 jrpxy\r\n\
            \r\n";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected_frontend_writer.as_slice()),
            jrpxy_util::debug::AsciiDebug(frontend_writer)
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
            BackendResponseStream::Final(rbr) => rbr,
            BackendResponseStream::Informational(_) => panic!("unexpected informational"),
        };

        let (client, _backend_connection) = rbr
            .forward_response_head()
            .await
            .expect("forward_response_head failed")
            .finish()
            .await
            .expect("failed to forward response");
        let client = client.expect("failed to recycle client");

        let (_frontend_reader, frontend_writer) = client.into_parts();
        let frontend_writer = frontend_writer.into_inner();

        // The content-length header must be preserved (the client needs to
        // know the representation size) but no body bytes must follow.
        let expected_frontend_writer = b"\
            HTTP/1.1 200 Ok\r\n\
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
            Via: 1.1 jrpxy\r\n\
            content-length: 5\r\n\
            \r\n\
            01234";
        assert_eq!(
            jrpxy_util::debug::AsciiDebug(expected_frontend_writer.as_slice()),
            jrpxy_util::debug::AsciiDebug(frontend_writer)
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

        let original_names: Vec<_> = pr
            .original()
            .headers()
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        assert!(
            original_names
                .iter()
                .any(|n| n.eq_ignore_ascii_case(b"connection"))
        );
        assert!(
            original_names
                .iter()
                .any(|n| n.eq_ignore_ascii_case(b"keep-alive"))
        );
        assert!(
            original_names
                .iter()
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

        let original_names: Vec<_> = pr
            .original()
            .headers()
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        assert!(
            original_names
                .iter()
                .any(|n| n.eq_ignore_ascii_case(b"x-custom-hop-1"))
        );
        assert!(
            original_names
                .iter()
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

        let original_names: Vec<_> = pr
            .original()
            .headers()
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        assert!(
            original_names
                .iter()
                .any(|n| n.eq_ignore_ascii_case(b"connection"))
        );
        assert!(
            original_names
                .iter()
                .any(|n| n.eq_ignore_ascii_case(b"keep-alive"))
        );
        assert!(
            original_names
                .iter()
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
            content-length: 0\r\n\
            \r\n";

        let pr = into_response(make_proxy_response(raw).await);

        let names: Vec<_> = pr.res().headers().iter().map(|(n, _)| n.clone()).collect();
        assert!(
            names
                .iter()
                .all(|n| !n.eq_ignore_ascii_case(b"x-server-hop"))
        );
        assert!(names.iter().any(|n| n.eq_ignore_ascii_case(b"x-keep")));

        let original_names: Vec<_> = pr
            .original()
            .headers()
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        assert!(
            original_names
                .iter()
                .any(|n| n.eq_ignore_ascii_case(b"x-server-hop"))
        );
    }

    #[tokio::test]
    async fn adds_via_header_to_response() {
        let mut buf = Vec::new();
        let pending = PendingFrontendResponse {
            frontend_writer: FrontendWriter::new(&mut buf),
            options: ProxyOptions::builder()
                .received_by("proxy.example.com")
                .build()
                .expect("valid pseudonym"),
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
            .expect("failed to build response");

        let (_, body_writer) = pending
            .send_as_content_length(response, 0)
            .await
            .expect("send failed");
        body_writer
            .finish()
            .expect("finish failed")
            .finish()
            .await
            .expect("finish flush failed");

        let output = String::from_utf8(buf).unwrap();
        assert!(
            output.contains("Via: 1.1 proxy.example.com"),
            "expected Via header in output: {output:?}"
        );
    }

    #[tokio::test]
    async fn informational_response_hop_by_hop() {
        let raw = b"\
            HTTP/1.1 100 Continue\r\n\
            Connection: x-interim-hop\r\n\
            X-Interim-Hop: val\r\n\
            X-Hint: keep\r\n\
            \r\n\
            HTTP/1.1 200 Ok\r\n\
            Content-Length: 0\r\n\
            \r\n";

        let reader = BackendReader::new(raw.as_slice(), 256);
        let stream = reader.read(true, 8192, 8192).await.expect("valid response");
        let proxy_stream = ProxyResponseStream::new(stream).expect("failed to build proxy stream");

        let (info, next_reader) = into_informational(proxy_stream);

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

        let info_original: Vec<_> = info
            .original()
            .headers()
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        assert!(
            info_original
                .iter()
                .any(|n| n.eq_ignore_ascii_case(b"connection"))
        );
        assert!(
            info_original
                .iter()
                .any(|n| n.eq_ignore_ascii_case(b"x-interim-hop"))
        );

        let mut buf = Vec::new();
        let pending = PendingFrontendResponse {
            frontend_writer: FrontendWriter::new(&mut buf),
            options: ProxyOptions::builder()
                .received_by("proxy.example.com")
                .build()
                .expect("valid pseudonym"),
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
        bw.finish().finish().await.expect("finish flush failed");
        let output = String::from_utf8(buf).unwrap();
        assert!(
            output.contains("Via: 1.1 proxy.example.com"),
            "expected Via header in forwarded 1xx: {output:?}"
        );

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
            FrontendReader::new(frontend_reader.as_ref(), 256),
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
            .with_header("Date", &b"Mon, 01 Jan 2026 00:00:00 GMT"[..])
            .build()
            .expect("failed to build 405 response");

        let (_, body_writer) = pending
            .send_as_content_length(response, 0)
            .await
            .expect("failed to send error response");
        body_writer
            .finish()
            .expect("failed to finish")
            .finish()
            .await
            .expect("failed to flush finish");

        let expected = b"\
            HTTP/1.1 405 Method Not Allowed\r\n\
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
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
            FrontendReader::new(frontend_reader.as_ref(), 256),
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
            .with_header("Date", &b"Mon, 01 Jan 2026 00:00:00 GMT"[..])
            .build()
            .expect("failed to build 405 response");

        let (_, body_writer) = pending
            .send_as_content_length(response, 0)
            .await
            .expect("failed to send error response");
        body_writer
            .finish()
            .expect("failed to finish")
            .finish()
            .await
            .expect("failed to flush finish");

        let expected = b"\
            HTTP/1.1 405 Method Not Allowed\r\n\
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
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
        let frontend_reader = b"GET\r\n\r\n";
        let mut frontend_writer = Vec::new();

        let proxy_client = ProxyClient::new(
            FrontendReader::new(frontend_reader.as_ref(), 256),
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
            .with_header("Date", &b"Mon, 01 Jan 2026 00:00:00 GMT"[..])
            .build()
            .expect("failed to build 400 response");

        let (_, body_writer) = pending
            .send_as_content_length(response, 0)
            .await
            .expect("failed to send error response");
        body_writer
            .finish()
            .expect("failed to finish")
            .finish()
            .await
            .expect("failed to flush finish");

        let expected = b"\
            HTTP/1.1 400 Bad Request\r\n\
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
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
            content-length: 0\r\n\
            \r\n";

        let reader = BackendReader::new(raw.as_slice(), 256);
        let stream = reader.read(true, 8192, 8192).await.expect("valid response");
        let proxy_stream = ProxyResponseStream::new(stream).expect("failed to build proxy stream");

        let (info1, r1) = into_informational(proxy_stream);
        assert_eq!(info1.res().code(), 100);

        let (info2, r2) = into_informational(r1.read().await.expect("read 2"));
        assert_eq!(info2.res().code(), 103);

        let final_res = into_response(r2.read().await.expect("read final"));
        assert_eq!(final_res.res().code(), 200);
    }

    /// A backend that will not respond until it has received the complete request
    /// body. The proxy must pump the request body to the backend concurrently
    /// with waiting for the response head, otherwise both sides deadlock.
    ///
    /// This test is expected to fail (timeout) until that concurrency is
    /// implemented.
    #[tokio::test]
    async fn proxy_completes_when_backend_requires_full_request_body_before_responding() {
        use std::time::Duration;
        use tokio::io::AsyncWriteExt;

        // frontend_client is the "browser" side; frontend_server connects to the proxy.
        let (mut frontend_client, frontend_server) = tokio::io::duplex(4096);
        // backend_client connects to the proxy; backend_server is the "origin".
        let (backend_client, backend_server) = tokio::io::duplex(4096);

        // Write the request (head + body) into the frontend pipe. The 4096-byte
        // pipe buffer is large enough to hold it without blocking.
        frontend_client
            .write_all(
                b"POST / HTTP/1.1\r\n\
                  Host: example.com\r\n\
                  Content-Length: 5\r\n\
                  \r\n\
                  hello",
            )
            .await
            .unwrap();

        // Keep frontend_client alive so the proxy's write-back path doesn't get a
        // broken-pipe when it eventually sends the response.
        let _frontend_client = frontend_client;

        // Mock origin server: reads the full request body before responding.
        // Uses FrontendReader because from the server's perspective it is
        // receiving an HTTP request from the proxy.
        let backend_task = tokio::spawn(async move {
            let (backend_server_r, mut backend_server_w) = tokio::io::split(backend_server);

            let req = FrontendReader::new(backend_server_r, 256)
                .read(8192, 8192)
                .await
                .expect("backend: failed to read request head");
            let (_, mut body_reader) = req.into_parts();

            while body_reader.read(8192).await.unwrap().is_some() {}

            backend_server_w
                .write_all(
                    b"HTTP/1.1 200 Ok\r\n\
                      content-length: 0\r\n\
                      \r\n",
                )
                .await
                .unwrap();
            backend_server_w.flush().await.unwrap();
        });

        let proxy_task = tokio::spawn(async move {
            let (frontend_server_r, frontend_server_w) = tokio::io::split(frontend_server);
            let (backend_client_r, backend_client_w) = tokio::io::split(backend_client);

            let proxy_client = ProxyClient::new(
                FrontendReader::new(frontend_server_r, 256),
                FrontendWriter::new(frontend_server_w),
                ProxyOptions::default(),
            );
            let backend_connection = (
                BackendReader::new(backend_client_r, 256),
                BackendWriter::new(backend_client_w),
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
                BackendResponseStream::Final(rbr) => rbr,
                BackendResponseStream::Informational(_) => panic!("unexpected informational"),
            };

            rbr.forward_response_head()
                .await
                .expect("forward_response_head failed")
                .finish()
                .await
                .expect("failed to finish");
        });

        tokio::time::timeout(Duration::from_secs(1), async move {
            proxy_task.await.unwrap();
            backend_task.await.unwrap();
        })
        .await
        .expect(
            "timed out - proxy deadlocked waiting for backend response \
             before forwarding the request body",
        );
    }

    /// Once a 100 Continue has been forwarded to the client, the proxy must
    /// keep pumping the request body to the backend while waiting for the
    /// next backend response. Otherwise the backend stays parked waiting on
    /// the body, the proxy stays parked waiting on the backend, and the
    /// transaction deadlocks.
    ///
    /// The client in this test only sends the body after observing the 100
    /// Continue from the proxy. That is what forces the deadlock onto the
    /// post-1xx path: at the time `read_backend_response` runs, the
    /// frontend body reader is parked with nothing to pump, so any
    /// concurrency at that layer cannot mask the bug.
    ///
    /// Without the select!-based concurrency in
    /// `BackendInformationalResponse::forward_informational_response`, this
    /// test will hit the timeout.
    #[tokio::test]
    async fn forwards_request_body_concurrently_with_post_informational_read() {
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (frontend_client, frontend_server) = tokio::io::duplex(4096);
        let (backend_client, backend_server) = tokio::io::duplex(4096);

        // Client task: send the head with Expect: 100-continue, wait for the
        // 100 to arrive, then send the body. This ensures the request body
        // is NOT in the frontend pipe while `read_backend_response` is
        // running, so the only path that can drain it is
        // `forward_informational_response`.
        let client_task = tokio::spawn(async move {
            let (mut fc_r, mut fc_w) = tokio::io::split(frontend_client);

            fc_w.write_all(
                b"POST / HTTP/1.1\r\n\
                  Host: example.com\r\n\
                  Content-Length: 5\r\n\
                  Expect: 100-continue\r\n\
                  \r\n",
            )
            .await
            .unwrap();
            fc_w.flush().await.unwrap();

            // "HTTP/1.1 100 Continue\r\n\r\n" is 25 bytes.
            let mut buf = [0u8; 25];
            fc_r.read_exact(&mut buf).await.unwrap();
            assert!(buf.starts_with(b"HTTP/1.1 100"));

            fc_w.write_all(b"hello").await.unwrap();
            fc_w.flush().await.unwrap();

            // Drain the final response so the proxy's writer isn't blocked
            // on a full pipe.
            let mut sink = Vec::new();
            let _ = fc_r.read_to_end(&mut sink).await;
        });

        let backend_task = tokio::spawn(async move {
            let (backend_server_r, mut backend_server_w) = tokio::io::split(backend_server);

            let req = FrontendReader::new(backend_server_r, 256)
                .read(8192, 8192)
                .await
                .expect("backend: failed to read request head");

            backend_server_w
                .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
                .await
                .unwrap();
            backend_server_w.flush().await.unwrap();

            let (_, mut body_reader) = req.into_parts();
            while body_reader.read(8192).await.unwrap().is_some() {}

            backend_server_w
                .write_all(
                    b"HTTP/1.1 200 Ok\r\n\
                      content-length: 0\r\n\
                      \r\n",
                )
                .await
                .unwrap();
            backend_server_w.flush().await.unwrap();
        });

        let proxy_task = tokio::spawn(async move {
            let (frontend_server_r, frontend_server_w) = tokio::io::split(frontend_server);
            let (backend_client_r, backend_client_w) = tokio::io::split(backend_client);

            let proxy_client = ProxyClient::new(
                FrontendReader::new(frontend_server_r, 256),
                FrontendWriter::new(frontend_server_w),
                ProxyOptions::default(),
            );
            let backend_connection = (
                BackendReader::new(backend_client_r, 256),
                BackendWriter::new(backend_client_w),
            );

            let stream = proxy_client
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

            let info = match stream {
                BackendResponseStream::Informational(info) => info,
                BackendResponseStream::Final(_) => panic!("expected 100 Continue first"),
            };

            let next = info
                .forward_informational_response()
                .await
                .expect("forward_informational_response failed");
            let stream = match next {
                InformationalForwardResult::Forwarded(s) => s,
                InformationalForwardResult::Dropped(s) => s,
            };
            let rbr = match stream {
                BackendResponseStream::Final(rbr) => rbr,
                BackendResponseStream::Informational(_) => {
                    panic!("unexpected second informational")
                }
            };

            rbr.forward_response_head()
                .await
                .expect("forward_response_head failed")
                .finish()
                .await
                .expect("failed to finish");
        });

        tokio::time::timeout(Duration::from_secs(2), async move {
            proxy_task.await.unwrap();
            backend_task.await.unwrap();
            client_task.await.unwrap();
        })
        .await
        .expect(
            "timed out - proxy deadlocked after forwarding 100 Continue, \
             not pumping request body to backend",
        );
    }
}
