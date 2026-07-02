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

/// Returned when [`FrontendProxyRequest::from_request`] rejects a request.
///
/// Carries the parsed [`FrontendRequest`] and a [`PendingFrontendResponse`] so
/// the caller can send an error response and, if it chooses, drain the request
/// body to reuse the connection. It deliberately exposes no way to forward the
/// request: a request that `from_request` rejected must not reach a backend.
pub struct FrontendRequestError<FR, FW> {
    request: FrontendRequest<FR>,
    pending: PendingFrontendResponse<FW>,
    error: ProxyFrontendError,
}

impl<FR, FW> FrontendRequestError<FR, FW> {
    /// A reference to the inner error associated with the request error.
    pub fn error(&self) -> &ProxyFrontendError {
        &self.error
    }

    /// Consumes the error and returns the parsed request and the pending
    /// response, so the caller can send an error response and optionally drain
    /// the request body for connection reuse.
    pub fn into_parts(self) -> (FrontendRequest<FR>, PendingFrontendResponse<FW>) {
        let Self {
            request,
            pending,
            error: _,
        } = self;
        (request, pending)
    }

    /// Consumes the error and returns just the [`PendingFrontendResponse`], for
    /// callers that only need to respond. The request body is dropped, so the
    /// connection cannot be reused afterward.
    pub fn into_pending_response(self) -> PendingFrontendResponse<FW> {
        let Self {
            request: _,
            pending,
            error: _,
        } = self;
        pending
    }
}

impl<FR, FW> std::fmt::Debug for FrontendRequestError<FR, FW> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrontendRequestError")
            .field("error", &self.error)
            .finish()
    }
}

impl<FR, FW> From<FrontendRequestError<FR, FW>> for ProxyError {
    fn from(value: FrontendRequestError<FR, FW>) -> Self {
        Self::CopyError(value.error.into())
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
/// [`BackendInformationalProxyResponseForwarder::forward_informational_response`].
///
/// When the failure is due to the backend, a frontend pending response can be
/// extracted so that the client can be told something went wrong.
pub enum InformationalProxyResponseForwardError<FW> {
    /// Something went wrong writing the informational response to the frontend.
    /// We can't recover anything from this.
    Frontend(ProxyFrontendError),
    /// Something went wrong reading response data from the backend. A
    /// [`PendingFrontendResponse`] can be extracted from the
    /// [`BackendStreamError`] so that an error response can still be written to
    /// the client.
    Backend(BackendStreamError<FW>),
}

impl<FW> std::fmt::Debug for InformationalProxyResponseForwardError<FW> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Frontend(e) => f.debug_tuple("Frontend").field(e).finish(),
            Self::Backend(e) => f.debug_tuple("Backend").field(e).finish(),
        }
    }
}

impl<FW> From<InformationalProxyResponseForwardError<FW>> for ProxyError {
    fn from(e: InformationalProxyResponseForwardError<FW>) -> Self {
        match e {
            InformationalProxyResponseForwardError::Frontend(e) => Self::CopyError(e.into()),
            InformationalProxyResponseForwardError::Backend(e) => Self::CopyError(e.error),
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
pub struct BackendProxyResponseReader<FR, FW, BR, BW> {
    request: BackendProxyRequest<FR, FW>,
    backend_reader: BackendReader<BR>,
    backend_body_writer: jrpxy_backend::writer::body::Ready<BW>,
}

/// Backend responses come as a stream. Sometimes the stream is just the final
/// response. Other times, it may be a stream of 1xx Informational responses
/// before a final response. This encodes these possibilities.
pub enum BackendProxyResponseStream<FR, FW, BR, BW> {
    /// A 1xx Informational response. Another response is expected to follow.
    Informational(BackendInformationalProxyResponseForwarder<FR, FW, BR, BW>),
    /// The final response. This is the last one of the stream.
    Final(BackendFinalProxyResponseForwarder<FR, FW, BR, BW>),
}

impl<FR, FW, BR, BW> BackendProxyResponseReader<FR, FW, BR, BW>
where
    FR: AsyncReadExt + Unpin,
    BR: AsyncReadExt + Unpin,
    BW: AsyncWriteExt + Unpin,
{
    /// Read a backend response as a [`BackendProxyResponseStream`].
    pub async fn read_backend_response(
        self,
    ) -> Result<BackendProxyResponseStream<FR, FW, BR, BW>, BackendResponseError<FW>> {
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

        let response_stream = match NormalizedResponseStream::new(response_stream) {
            Ok(s) => s,
            Err(e) => {
                return Err(BackendResponseError {
                    pending,
                    error: e.into(),
                });
            }
        };
        Ok(attach_frontend_state(
            response_stream,
            request_body_forwarder,
            pending,
        ))
    }
}

/// A result of trying to forward an informational response to the frontend.
pub enum InformationalProxyResponseForwardResult<FR, FW, BR, BW> {
    /// The informational response was forwarded to the frontend.
    Forwarded(BackendProxyResponseStream<FR, FW, BR, BW>),
    /// The informational response was dropped as the frontend doesn't support
    /// informational responses (as is the case with HTTP/1.0 clients).
    Dropped(BackendProxyResponseStream<FR, FW, BR, BW>),
}

impl<FR, FW, BR, BW> InformationalProxyResponseForwardResult<FR, FW, BR, BW> {
    /// Convert to the inner [`BackendProxyResponseStream`].
    pub fn into_inner(self) -> BackendProxyResponseStream<FR, FW, BR, BW> {
        match self {
            Self::Forwarded(r) | Self::Dropped(r) => r,
        }
    }
}

/// An informational response from a backend. Contains all the information
/// needed to forward the response to the frontend and then continue reading
/// more responses from the backend.
pub struct BackendInformationalProxyResponseForwarder<FR, FW, BR, BW> {
    proxy_informational_response: BackendInformationalProxyResponse,
    request_body_forwarder: RequestBodyForwarder<FR, BW>,
    pending: PendingFrontendResponse<FW>,
    proxy_stream_reader: NormalizedStreamReader<BR>,
}

impl<FR, FW, BR, BW> BackendInformationalProxyResponseForwarder<FR, FW, BR, BW> {
    /// A reference to the internal [`BackendInformationalProxyResponse`].
    pub fn as_response(&self) -> &BackendInformationalProxyResponse {
        &self.proxy_informational_response
    }

    /// A mutable reference to the internal [`BackendInformationalProxyResponse`].
    pub fn as_response_mut(&mut self) -> &mut BackendInformationalProxyResponse {
        &mut self.proxy_informational_response
    }
}

impl<FR, FW, BR, BW> BackendInformationalProxyResponseForwarder<FR, FW, BR, BW>
where
    FR: AsyncReadExt + Unpin,
    FW: AsyncWriteExt + Unpin,
    BR: AsyncReadExt + Unpin,
    BW: AsyncWriteExt + Unpin,
{
    /// Forward the informational response to the frontend. See
    /// [`InformationalProxyResponseForwardResult`] for possible successful outcomes.
    pub async fn forward_informational_response(
        self,
    ) -> Result<
        InformationalProxyResponseForwardResult<FR, FW, BR, BW>,
        InformationalProxyResponseForwardError<FW>,
    > {
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
                .map_err(InformationalProxyResponseForwardError::Frontend)?
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
                return Err(InformationalProxyResponseForwardError::Backend(
                    BackendStreamError { pending, error: e },
                ));
            }
        };
        let response_stream = match read_result {
            Ok(r) => r,
            Err(error) => {
                return Err(InformationalProxyResponseForwardError::Backend(
                    BackendStreamError {
                        pending,
                        error: error.into(),
                    },
                ));
            }
        };

        let response_stream =
            attach_frontend_state(response_stream, request_body_forwarder, pending);

        Ok(if client_supports_informational_response {
            InformationalProxyResponseForwardResult::Forwarded(response_stream)
        } else {
            InformationalProxyResponseForwardResult::Dropped(response_stream)
        })
    }
}

/// The last backend response in a response stream.
pub struct BackendFinalProxyResponseForwarder<FR, FW, BR, BW> {
    request_body_forwarder: RequestBodyForwarder<FR, BW>,
    pending: PendingFrontendResponse<FW>,
    proxy_response: BackendFinalProxyResponse<BR>,
}

impl<FR, FW, BR, BW> BackendFinalProxyResponseForwarder<FR, FW, BR, BW> {
    /// The [`BackendFinalProxyResponse`] associated with this backend response.
    pub fn as_response(&self) -> &BackendFinalProxyResponse<BR> {
        &self.proxy_response
    }

    /// A mutable reference to the [`BackendFinalProxyResponse`] associated with this
    /// backend response.
    pub fn as_response_mut(&mut self) -> &mut BackendFinalProxyResponse<BR> {
        &mut self.proxy_response
    }
}

impl<FR, FW, BR, BW> BackendFinalProxyResponseForwarder<FR, FW, BR, BW>
where
    FR: AsyncReadExt + Unpin,
    FW: AsyncWriteExt + Unpin,
    BR: AsyncReadExt + Unpin,
    BW: AsyncWriteExt + Unpin,
{
    /// Send the response head to the frontend and return a [`ProxyBodyExchanger`]
    /// ready to drive the body copy in both directions.
    ///
    /// Use [`ProxyBodyExchanger::finish`] to drive the copy internally, or
    /// [`ProxyBodyExchanger::into_parts`] to take ownership of the body readers
    /// and writers and drive the copy yourself.
    pub async fn forward_response_head(
        self,
    ) -> Result<ProxyBodyExchanger<FR, FW, BR, BW>, ProxyError> {
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

        Ok(ProxyBodyExchanger {
            options,
            request_body_forwarder,
            response_body_forwarder,
        })
    }
}

/// Drives the bidirectional body exchange between the frontend and backend
/// after the response head has been forwarded.
///
/// Produced by [`BackendFinalProxyResponseForwarder::forward_response_head`].
pub struct ProxyBodyExchanger<FR, FW, BR, BW> {
    options: ProxyOptions,
    request_body_forwarder: RequestBodyForwarder<FR, BW>,
    response_body_forwarder: ResponseBodyForwarder<BR, FW>,
}

/// The individual body readers and writers that make up a [`ProxyBodyExchanger`].
///
/// Produced by [`ProxyBodyExchanger::into_parts`]. The caller is responsible for
/// driving the body copies concurrently. This is typically done using
/// `tokio::select!` or `tokio::spawn`. See [`ProxyBodyExchanger::finish`] for a
/// reference implementation.
pub struct ProxyBodyExchangerParts<FR, FW, BR, BW> {
    /// ProxyOptions governing behavior so far
    pub options: ProxyOptions,
    /// Forwards the request body from the frontend to the backend.
    pub request_body_forwarder: RequestBodyForwarder<FR, BW>,
    /// Forwards the response body from the backend to the frontend.
    pub response_body_forwarder: ResponseBodyForwarder<BR, FW>,
}

impl<FR, FW, BR, BW> ProxyBodyExchanger<FR, FW, BR, BW> {
    /// Decompose into the individual body readers and writers. The caller is
    /// responsible for driving the f2b and b2f copies concurrently.
    pub fn into_parts(self) -> ProxyBodyExchangerParts<FR, FW, BR, BW> {
        let Self {
            options,
            request_body_forwarder,
            response_body_forwarder,
        } = self;
        ProxyBodyExchangerParts {
            options,
            request_body_forwarder,
            response_body_forwarder,
        }
    }
}

impl<FR, FW, BR, BW> ProxyBodyExchanger<FR, FW, BR, BW>
where
    FR: AsyncReadExt + Unpin,
    FW: AsyncWriteExt + Unpin,
    BR: AsyncReadExt + Unpin,
    BW: AsyncWriteExt + Unpin,
{
    /// Drive the body copy in both directions concurrently. On completion,
    /// returns frontend and backend readers and writers that can be reused.
    pub async fn finish(
        self,
    ) -> ProxyResult<(
        Option<(FrontendReader<FR>, FrontendWriter<FW>)>,
        Option<(BackendReader<BR>, BackendWriter<BW>)>,
    )> {
        let Self {
            options: _,
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
        let frontend_connection = frontend_reader.zip(frontend_writer);

        Ok((frontend_connection, backend_connection))
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

/// Trailer-section fields the proxy strips before forwarding, on top of the
/// connection-management set already removed by [`is_standard_hop_by_hop`].
///
/// RFC 9110 section 6.5.1 forbids sending these as trailers: a recipient that
/// folds them into the header section has framing, routing, auth, caching, or
/// content decisions changed after that section was acted on (response
/// splitting / request smuggling / cache poisoning). The gateway cannot know
/// whether a downstream folds trailers, so it strips them.
///
/// Section 6.5.1 lists fields by category and example, not as a closed set.
/// Entries are grouped by category: the RFC-named examples verbatim, plus
/// unnamed siblings in the same categories that pose the same hazard - the
/// request conditionals, the cookie / challenge side of auth, and fields whose
/// specific danger is a shared cache replaying a folded trailer to every client
/// (see the per-field notes). Framing's `transfer-encoding` / `te` are omitted
/// here as [`is_standard_hop_by_hop`] already covers them.
const FORBIDDEN_TRAILER_HEADERS: &[&[u8]] = &[
    // Framing (6.5.1: "Content-Length").
    b"content-length",
    // Routing (6.5.1: "Host").
    b"host",
    // Request modifiers (6.5.1: "Cache-Control, Expect, Max-Forwards, TE"; TE
    // is hop-by-hop), that category's conditionals (section 13), and range
    // (section 14): a folded range desyncs a range-aware cache's key from the
    // bytes actually served, or turns a stored 200 into a 206.
    b"cache-control",
    b"expect",
    b"max-forwards",
    b"if-match",
    b"if-none-match",
    b"if-modified-since",
    b"if-unmodified-since",
    b"if-range",
    b"range",
    // Authentication (6.5.1: "Authorization"; Proxy-Auth* are hop-by-hop) and
    // its cookie / challenge siblings (section 11, RFC 6265).
    b"authorization",
    b"www-authenticate",
    b"set-cookie",
    b"cookie",
    // Response control data (6.5.1: "Age, Cache-Control, Expires, Date"), plus
    // two siblings a shared cache would replay to every client: vary (section
    // 12.5.5) redefines the secondary cache key (RFC 9111 4.1); location
    // (section 10.2.2) on a cacheable 301/308 becomes a persistent redirect.
    b"age",
    b"expires",
    b"date",
    b"vary",
    b"location",
    // Payload processing (6.5.1: "Content-Encoding, Content-Type,
    // Content-Range") and content-location (section 8.7), which can bind the
    // stored response to a different URI than the one requested.
    b"content-encoding",
    b"content-type",
    b"content-range",
    b"content-location",
    // Trailer belongs in the header section announcing the trailers (6.5.1),
    // never among them.
    b"trailer",
];

// Deliberately absent, though a deployment may want them: forwarding metadata
// (X-Forwarded-For / -Host / -Proto, Forwarded) and security response headers
// (Content-Security-Policy, Strict-Transport-Security, X-Frame-Options,
// Access-Control-Allow-*). A folded X-Forwarded-Host reflected into a cached
// redirect or link is a potent cache-poisoning vector, and a folded
// X-Forwarded-For can spoof a client address for downstream authz and logging.
// They are left out because they are non-standard (no RFC 9110 6.5.1 lineage)
// and their trust model is deployment-specific; the intended home is an
// operator-supplied extension to this filter, not a baked-in default. Not
// wired up yet.

/// True when `name` must not be forwarded in a trailer section: the standard
/// hop-by-hop set (see [`HOP_BY_HOP_HEADERS`]) plus the RFC 9110 section 6.5.1
/// categories in [`FORBIDDEN_TRAILER_HEADERS`].
fn is_forbidden_trailer_field(name: &[u8]) -> bool {
    is_standard_hop_by_hop(name)
        || FORBIDDEN_TRAILER_HEADERS
            .iter()
            .any(|h| h.eq_ignore_ascii_case(name))
}

/// Remove every field that [`is_forbidden_trailer_field`] rejects from
/// `trailers`, in place, before the trailer section is forwarded to a peer.
fn sanitize_trailers(trailers: &mut Headers) {
    trailers.retain(|(name, _)| !is_forbidden_trailer_field(name));
}

/// Inject a `Via` header (RFC 9110 sec 7.6.3) into `headers`.
fn insert_proxy_headers(headers: &mut Headers, version: HttpVersion, received_by: &str) {
    let via_version = match version {
        HttpVersion::Http10 => "1.0",
        HttpVersion::Http11 => "1.1",
    };
    headers.push_raw(
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
    headers.push_raw(Bytes::from_static(b"Date"), Bytes::from(now));
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
    // See from_request: Err carries recoverable state, hence large.
    #[allow(clippy::result_large_err)]
    fn new(
        mut frontend: FrontendRequest<FR>,
        pending: PendingFrontendResponse<FW>,
        connection_tokens: &[Bytes],
    ) -> Result<Self, FrontendRequestError<FR, FW>> {
        let original = frontend.req().clone();
        if let Err(e) = request_target::normalize(frontend.req_mut()) {
            return Err(FrontendRequestError {
                request: frontend,
                pending,
                error: e,
            });
        }
        strip_hop_by_hop_headers(frontend.req_mut().headers_mut(), connection_tokens);
        Ok(Self {
            frontend,
            original,
            pending,
        })
    }

    /// Build a [`FrontendProxyRequest`] from an already-parsed
    /// [`FrontendRequest`]. Runs validation, hop-by-hop stripping, and
    /// request-target normalization.
    // The `Err` variant carries the recoverable state (body reader + pending
    // response) so the caller can send a custom error response, which makes it
    // large; boxing it would be a cold-path-only ergonomics tax.
    #[allow(clippy::result_large_err)]
    pub fn from_request(
        request: FrontendRequest<FR>,
        writer: FrontendWriter<FW>,
        options: ProxyOptions,
    ) -> Result<Self, FrontendRequestError<FR, FW>> {
        let is_head = request.req().method() == b"HEAD".as_slice();
        let version = request.req().version();

        let pending = PendingFrontendResponse {
            frontend_writer: writer,
            options,
            client_options: ClientOptions { is_head, version },
        };

        let connection_tokens = match get_connection_tokens(request.req().headers()) {
            Ok(c) => c,
            Err(e) => {
                return Err(FrontendRequestError {
                    request,
                    pending,
                    error: e.into(),
                });
            }
        };

        // Method rejections run first, ahead of request-target classification.
        // CONNECT must be handled independently. OPTIONS and TRACE may target
        // the gateway/proxy itself, so they cannot be blindly forwarded. The
        // caller must intercept all three before the proxy decision.
        let method = request.req().method();
        if method == b"CONNECT".as_slice() {
            return Err(FrontendRequestError {
                request,
                pending,
                error: ProxyFrontendError::ConnectNotSupported,
            });
        }
        if method == b"TRACE".as_slice() {
            return Err(FrontendRequestError {
                request,
                pending,
                error: ProxyFrontendError::TraceNotSupported,
            });
        }
        if method == b"OPTIONS".as_slice() {
            return Err(FrontendRequestError {
                request,
                pending,
                error: ProxyFrontendError::OptionsNotSupported,
            });
        }

        // RFC 9110 section 7.2: a server MUST respond 400 to any request with
        // more than one Host field, regardless of version. Multiple Hosts let
        // the proxy and origin disagree on routing/authorization, so reject
        // unconditionally. The presence requirement is HTTP/1.1-only per RFC
        // 9112 section 3.2.2; HTTP/1.0 may omit Host. These checks run against
        // the request as received, before normalization: RFC 9112 section 3.2.2
        // requires a client to send Host in every HTTP/1.1 request even in
        // absolute-form, so a missing Host is a client fault even though
        // normalization could otherwise synthesize one from the request-target
        // authority.
        let host_count = request.req().headers().get_header("host").count();
        if host_count > 1 {
            return Err(FrontendRequestError {
                request,
                pending,
                error: ProxyFrontendError::MultipleHosts,
            });
        }
        if version == HttpVersion::Http11 && host_count == 0 {
            return Err(FrontendRequestError {
                request,
                pending,
                error: ProxyFrontendError::MissingHost,
            });
        }

        Self::new(request, pending, &connection_tokens)
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
    ) -> Result<BackendProxyResponseReader<FR, FW, BR, BW>, BackendRequestError<FR, FW>>
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
            Ok(backend_body_writer) => Ok(BackendProxyResponseReader {
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

/// Reads the next response in a stream, producing a [`NormalizedResponseStream`].
///
/// Wraps [`BackendStreamReader`] and applies hop-by-hop removal to each
/// response it yields.
struct NormalizedStreamReader<I> {
    reader: BackendStreamReader<I>,
}

impl<I> NormalizedStreamReader<I>
where
    I: AsyncReadExt + Unpin,
{
    /// Read the next response in the stream.
    async fn read(self) -> Result<NormalizedResponseStream<I>, ProxyBackendError> {
        let stream = self.reader.read().await?;
        let stream = NormalizedResponseStream::new(stream)?;
        Ok(stream)
    }
}

/// The proxy-processed form of [`ResponseStream`].
///
/// Each variant has had its hop-by-hop headers stripped.
enum NormalizedResponseStream<I> {
    /// A regular (non-1xx) response.
    Final(BackendFinalProxyResponse<I>),
    /// A 1xx informational response. More responses can be read from the
    /// accompanying [`NormalizedStreamReader`].
    Informational(BackendInformationalProxyResponse, NormalizedStreamReader<I>),
}

impl<I> NormalizedResponseStream<I> {
    fn new(stream: ResponseStream<I>) -> Result<Self, ProxyBackendError> {
        match stream {
            ResponseStream::Final(mut backend) => {
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
                    NormalizedStreamReader { reader },
                ))
            }
        }
    }
}

/// Fold the per-transaction frontend state - the pending response and the
/// in-flight request-body forwarder - onto a normalized backend response,
/// yielding the next [`BackendProxyResponseStream`]. The initial response read and
/// the post-1xx continuation both reach this same point, so the variant-by-
/// variant rewrap lives here rather than being reproduced at each call site.
fn attach_frontend_state<FR, FW, BR, BW>(
    stream: NormalizedResponseStream<BR>,
    request_body_forwarder: RequestBodyForwarder<FR, BW>,
    pending: PendingFrontendResponse<FW>,
) -> BackendProxyResponseStream<FR, FW, BR, BW> {
    match stream {
        NormalizedResponseStream::Final(proxy_response) => {
            BackendProxyResponseStream::Final(BackendFinalProxyResponseForwarder {
                request_body_forwarder,
                pending,
                proxy_response,
            })
        }
        NormalizedResponseStream::Informational(
            proxy_informational_response,
            proxy_stream_reader,
        ) => {
            BackendProxyResponseStream::Informational(BackendInformationalProxyResponseForwarder {
                proxy_informational_response,
                request_body_forwarder,
                pending,
                proxy_stream_reader,
            })
        }
    }
}

impl<I> std::fmt::Debug for NormalizedResponseStream<I>
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
    use jrpxy_frontend::writer::FrontendWriter;
    use jrpxy_http_message::{message::ResponseBuilder, version::HttpVersion};

    use bytes::Bytes;
    use jrpxy_http_message::header::Headers;

    use crate::{
        ClientOptions, PendingFrontendResponse, ProxyOptions, is_forbidden_trailer_field,
        sanitize_trailers,
    };

    #[test]
    fn forbidden_trailer_fields_cover_each_rfc_9110_6_5_1_category() {
        // One representative per RFC 9110 6.5.1 category, plus the connection-
        // management fields that is_standard_hop_by_hop already denies.
        for name in [
            b"content-length".as_slice(), // framing
            b"transfer-encoding",         // framing (hop-by-hop)
            b"host",                      // routing
            b"max-forwards",              // request modifier
            b"if-none-match",             // request conditional
            b"authorization",             // authentication
            b"set-cookie",                // authentication (sibling)
            b"date",                      // response control data
            b"content-type",              // payload processing
            b"trailer",                   // trailer announcement
            b"connection",                // hop-by-hop
        ] {
            assert!(
                is_forbidden_trailer_field(name),
                "{} must be forbidden in a trailer section",
                String::from_utf8_lossy(name)
            );
        }
    }

    #[test]
    fn caching_motivated_trailer_fields_are_forbidden() {
        // Siblings added because a shared cache would replay a folded trailer
        // to every client (cache-key desync or poisoned stored response).
        for name in [
            b"vary".as_slice(),
            b"location",
            b"content-location",
            b"range",
        ] {
            assert!(
                is_forbidden_trailer_field(name),
                "{} must be forbidden in a trailer section",
                String::from_utf8_lossy(name)
            );
        }
    }

    #[test]
    fn benign_trailer_fields_are_preserved() {
        // Case-insensitive matching must not clobber unrelated fields, and the
        // validators that are the whole point of trailers stay allowed.
        for name in [
            b"x-checksum".as_slice(),
            b"etag",
            b"last-modified",
            b"expires-soon",
        ] {
            assert!(
                !is_forbidden_trailer_field(name),
                "{} should be allowed in a trailer section",
                String::from_utf8_lossy(name)
            );
        }
    }

    #[test]
    fn sanitize_trailers_strips_forbidden_preserves_benign() {
        let mut trailers = Headers::with_capacity(4);
        // Mixed case to prove the filter is case-insensitive.
        trailers.push_raw(
            Bytes::from_static(b"Set-Cookie"),
            Bytes::from_static(b"s=1"),
        );
        trailers.push_raw(
            Bytes::from_static(b"Transfer-Encoding"),
            Bytes::from_static(b"chunked"),
        );
        trailers.push_raw(
            Bytes::from_static(b"X-Checksum"),
            Bytes::from_static(b"abc"),
        );

        sanitize_trailers(&mut trailers);

        let remaining: Vec<_> = trailers
            .iter()
            .map(|(n, _)| String::from_utf8_lossy(n).to_string())
            .collect();
        assert_eq!(remaining, vec!["X-Checksum".to_string()]);
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
}
