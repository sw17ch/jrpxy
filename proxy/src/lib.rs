use std::borrow::Cow;
use std::task::Poll;

use bytes::Bytes;
use jrpxy_backend::{
    error::{BackendError, BackendResult},
    reader::{
        BackendBodyReader, BackendReader, BackendResponse, BackendStreamReader, ResponseStream,
    },
    writer::{BackendBodyWriter, BackendWriter},
};
use jrpxy_frontend::{
    error::FrontendError,
    reader::{FrontendBodyReader, FrontendReader, FrontendRequest},
    writer::FrontendWriter,
};
use jrpxy_http_message::{
    header::{HeaderError, Headers},
    message::{Request, Response},
    version::HttpVersion,
};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};

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
    #[error("Error copying the fronend body to the backend, but response completed: {0}")]
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

type ProxyResult<T> = std::result::Result<T, ProxyError>;

pub trait BackendProxyProvider {
    /// The backend reader inner type
    type BR;
    /// The backend writer inner type
    type BW;

    fn get_connection(
        &mut self,
    ) -> impl Future<Output = ProxyResult<(BackendReader<Self::BR>, BackendWriter<Self::BW>)>>;

    fn give_connection(&mut self, reader: BackendReader<Self::BR>, writer: BackendWriter<Self::BW>);
}

pub struct OneshotBackend<BR, BW> {
    inner: Option<(BackendReader<BR>, BackendWriter<BW>)>,
}

pin_project_lite::pin_project! {
    pub struct OneshotConnector<BR,BW> {
        #[pin]
        inner: Option<(BR,BW)>,
    }
}

impl<BR, BW> OneshotBackend<BR, BW>
where
    BR: AsyncReadExt + Unpin,
    BW: AsyncWrite + Unpin,
{
    pub fn new(backend_reader: BR, backend_writer: BW) -> Self {
        Self {
            inner: Some((
                BackendReader::new(backend_reader),
                BackendWriter::new(backend_writer),
            )),
        }
    }
}

impl<BR: Unpin, BW: Unpin> Future for OneshotConnector<BR, BW> {
    type Output = ProxyResult<(BR, BW)>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<Self::Output> {
        let proj = self.project();
        let mut inner = proj.inner;
        if let Some(bp) = inner.take() {
            Poll::Ready(Ok(bp))
        } else {
            Poll::Ready(Err(ProxyError::NoBackendConnection))
        }
    }
}

impl<BR: Unpin, BW: Unpin> BackendProxyProvider for OneshotBackend<BR, BW> {
    type BR = BR;
    type BW = BW;

    fn get_connection(
        &mut self,
    ) -> impl Future<
        Output = Result<
            (
                BackendReader<<Self as BackendProxyProvider>::BR>,
                BackendWriter<<Self as BackendProxyProvider>::BW>,
            ),
            ProxyError,
        >,
    > {
        OneshotConnector {
            inner: self.inner.take(),
        }
    }

    fn give_connection(
        &mut self,
        reader: BackendReader<Self::BR>,
        writer: BackendWriter<Self::BW>,
    ) {
        self.inner = Some((reader, writer));
    }
}

pub struct ProxyOptions {
    pub max_head_length: usize,
    pub body_chunk_size: usize,
    pub received_by: Cow<'static, str>,
}

impl Default for ProxyOptions {
    fn default() -> Self {
        Self {
            max_head_length: 8192,
            body_chunk_size: 8192,
            received_by: Cow::Borrowed("jrpxy"),
        }
    }
}

pub struct ProxyClient<FR, FW, BP> {
    frontend_reader: FrontendReader<FR>,
    frontend_writer: FrontendWriter<FW>,
    backend_provider: BP,
    options: ProxyOptions,
}

impl<FR, FW, BP> ProxyClient<FR, FW, BP> {
    pub fn into_parts(self) -> (FrontendReader<FR>, FrontendWriter<FW>, BP) {
        let Self {
            frontend_reader,
            frontend_writer,
            backend_provider,
            options: _,
        } = self;
        (frontend_reader, frontend_writer, backend_provider)
    }
}

impl<FR, FW, BR, BW, BP> ProxyClient<FR, FW, BP>
where
    FR: AsyncReadExt + Unpin,
    FW: AsyncWriteExt + Unpin,
    BR: AsyncReadExt + Unpin,
    BW: AsyncWriteExt + Unpin,
    BP: BackendProxyProvider<BR = BR, BW = BW>,
{
    pub fn new(
        frontend_reader: FR,
        frontend_writer: FW,
        backend_provider: BP,
        options: ProxyOptions,
    ) -> Self {
        let frontend_reader = FrontendReader::new(frontend_reader, options.max_head_length);
        let frontend_writer = FrontendWriter::new(frontend_writer);
        Self {
            frontend_reader,
            frontend_writer,
            backend_provider,
            options,
        }
    }

    pub async fn start(self) -> ProxyResult<ReadFrontendRequest<FR, FW, BP>> {
        let Self {
            frontend_reader,
            frontend_writer,
            backend_provider,
            options,
        } = self;
        let req = frontend_reader.read().await?;
        let proxy_request = ProxyRequest::new(req, &options.received_by);

        Ok(ReadFrontendRequest {
            options,
            proxy_request,
            frontend_writer,
            backend_provider,
        })
    }
}

pub struct ReadFrontendRequest<FR, FW, BP> {
    proxy_request: ProxyRequest<FR>,
    frontend_writer: FrontendWriter<FW>,
    backend_provider: BP,
    options: ProxyOptions,
}

impl<FR, FW, BP> ReadFrontendRequest<FR, FW, BP> {
    pub fn as_proxy_request(&self) -> &ProxyRequest<FR> {
        &self.proxy_request
    }
    pub fn as_proxy_request_mut(&mut self) -> &ProxyRequest<FR> {
        &mut self.proxy_request
    }
}

impl<FR, FW, BP> ReadFrontendRequest<FR, FW, BP>
where
    FR: AsyncReadExt + Unpin,
    FW: AsyncWriteExt + Unpin,
{
    pub async fn peek_body(&mut self, length: usize) -> ProxyResult<(bool, &[u8])> {
        self.proxy_request
            .frontend
            .peek_body(length)
            .await
            .map_err(|e| e.into())
    }
}

impl<FR, FW, BR, BW, BP> ReadFrontendRequest<FR, FW, BP>
where
    BR: AsyncReadExt + Unpin,
    BW: AsyncWriteExt + Unpin,
    BP: BackendProxyProvider<BR = BR, BW = BW>,
{
    pub async fn write_backend_request(
        self,
    ) -> ProxyResult<WroteBackendRequest<FR, FW, BR, BW, BP>> {
        let Self {
            options,
            proxy_request,
            frontend_writer,
            mut backend_provider,
        } = self;

        let allow_response_body = proxy_request.req().method() != b"HEAD".as_slice();

        let (backend_reader, backend_writer) = backend_provider.get_connection().await?;

        let (backend_request, frontend_body_reader) = proxy_request.into_backend_request();

        let backend_body_writer = backend_writer.send_as_same(&backend_request).await?;

        Ok(WroteBackendRequest {
            options,
            allow_response_body,
            frontend_body_reader,
            frontend_writer,
            backend_reader,
            backend_body_writer,
            backend_provider,
        })
    }
}

pub struct WroteBackendRequest<FR, FW, BR, BW, BP> {
    options: ProxyOptions,
    allow_response_body: bool,
    frontend_body_reader: FrontendBodyReader<FR>,
    frontend_writer: FrontendWriter<FW>,
    backend_reader: BackendReader<BR>,
    backend_body_writer: BackendBodyWriter<BW>,
    backend_provider: BP,
}

pub enum BackendResponseStatus<FR, FW, BR, BW, BP: BackendProxyProvider<BR = BR, BW = BW>> {
    Informational(ReadBackendInformationalResponse<FR, FW, BR, BW, BP>),
    Response(ReadBackendResponse<FR, FW, BR, BW, BP>),
}

impl<FR, FW, BR, BW, BP> WroteBackendRequest<FR, FW, BR, BW, BP>
where
    BR: AsyncReadExt + Unpin,
    BW: AsyncWriteExt + Unpin,
    BP: BackendProxyProvider<BR = BR, BW = BW>,
{
    pub async fn read_backend_response(
        self,
    ) -> ProxyResult<BackendResponseStatus<FR, FW, BR, BW, BP>> {
        let Self {
            options,
            allow_response_body,
            frontend_body_reader,
            frontend_writer,
            backend_reader,
            backend_body_writer,
            backend_provider,
        } = self;

        let response_stream = backend_reader
            .read(allow_response_body, options.max_head_length)
            .await?;
        let response_stream = ProxyResponseStream::new(response_stream, &options.received_by);
        Ok(match response_stream {
            ProxyResponseStream::Response(proxy_response) => {
                BackendResponseStatus::Response(ReadBackendResponse {
                    options,
                    frontend_body_reader,
                    frontend_writer,
                    proxy_response,
                    backend_body_writer,
                    backend_provider,
                })
            }
            ProxyResponseStream::Informational(
                proxy_informational_response,
                proxy_stream_reader,
            ) => BackendResponseStatus::Informational(ReadBackendInformationalResponse {
                options,
                proxy_informational_response,
                frontend_body_reader,
                frontend_writer,
                proxy_stream_reader,
                backend_body_writer,
                backend_provider,
            }),
        })
    }
}

pub struct ReadBackendInformationalResponse<
    FR,
    FW,
    BR,
    BW,
    BP: BackendProxyProvider<BR = BR, BW = BW>,
> {
    proxy_informational_response: ProxyInformationalResponse,
    frontend_body_reader: FrontendBodyReader<FR>,
    frontend_writer: FrontendWriter<FW>,
    proxy_stream_reader: ProxyStreamReader<BR>,
    backend_body_writer: BackendBodyWriter<BW>,
    backend_provider: BP,
    options: ProxyOptions,
}

impl<FR, FW, BR, BW, BP> ReadBackendInformationalResponse<FR, FW, BR, BW, BP>
where
    FW: AsyncWriteExt + Unpin,
    BR: AsyncReadExt + Unpin,
    BW: AsyncWriteExt + Unpin,
    BP: BackendProxyProvider<BR = BR, BW = BW>,
{
    pub fn as_response(&self) -> &ProxyInformationalResponse {
        &self.proxy_informational_response
    }

    pub fn as_response_mut(&mut self) -> &mut ProxyInformationalResponse {
        &mut self.proxy_informational_response
    }

    pub fn drop_informational_response(self) -> BackendResponseStatus<FR, FW, BR, BW, BP> {
        todo!()
    }

    pub async fn forward_informational_response(
        self,
    ) -> ProxyResult<BackendResponseStatus<FR, FW, BR, BW, BP>> {
        let Self {
            proxy_informational_response,
            frontend_body_reader,
            frontend_writer,
            proxy_stream_reader,
            backend_body_writer,
            backend_provider,
            options,
        } = self;

        let response = proxy_informational_response.into_frontend_response();
        let frontend_body_writer = frontend_writer.send_as_bodyless(&response).await?;
        let frontend_writer = frontend_body_writer.finish().await?;

        match proxy_stream_reader.read().await? {
            ProxyResponseStream::Response(proxy_response) => {
                Ok(BackendResponseStatus::Response(ReadBackendResponse {
                    frontend_body_reader,
                    frontend_writer,
                    proxy_response,
                    backend_body_writer,
                    backend_provider,
                    options,
                }))
            }
            ProxyResponseStream::Informational(
                proxy_informational_response,
                proxy_stream_reader,
            ) => Ok(BackendResponseStatus::Informational(
                ReadBackendInformationalResponse {
                    proxy_informational_response,
                    frontend_body_reader,
                    frontend_writer,
                    proxy_stream_reader,
                    backend_body_writer,
                    backend_provider,
                    options,
                },
            )),
        }
    }
}

pub struct ReadBackendResponse<FR, FW, BR, BW, BP: BackendProxyProvider<BR = BR, BW = BW>> {
    frontend_body_reader: FrontendBodyReader<FR>,
    frontend_writer: FrontendWriter<FW>,
    proxy_response: ProxyResponse<BR>,
    backend_body_writer: BackendBodyWriter<BW>,
    backend_provider: BP,
    options: ProxyOptions,
}

impl<FR, FW, BR, BW, BP> ReadBackendResponse<FR, FW, BR, BW, BP>
where
    FR: AsyncReadExt + Unpin,
    FW: AsyncWriteExt + Unpin,
    BR: AsyncReadExt + Unpin,
    BW: AsyncWriteExt + Unpin,
    BP: BackendProxyProvider<BR = BR, BW = BW> + Unpin,
{
    pub fn as_response(&self) -> &ProxyResponse<BR> {
        &self.proxy_response
    }

    pub fn as_response_mut(&mut self) -> &mut ProxyResponse<BR> {
        &mut self.proxy_response
    }

    pub async fn forward_response(self) -> ProxyResult<ProxyClient<FR, FW, BP>> {
        let Self {
            mut frontend_body_reader,
            frontend_writer,
            proxy_response,
            mut backend_body_writer,
            mut backend_provider,
            options,
        } = self;

        let (response, mut backend_body_reader) = proxy_response.into_frontend_response();
        let mut frontend_body_writer = frontend_writer.send_as_same(&response).await?;

        let f2b_fut = async move {
            let ret;
            loop {
                let buf = match frontend_body_reader.read(options.body_chunk_size).await {
                    Ok(Some(buf)) => buf,
                    Ok(None) => {
                        match backend_body_writer.finish().await {
                            Ok(w) => match frontend_body_reader.drain().await {
                                Ok(r) => {
                                    ret = Ok((r, w));
                                }
                                Err(e) => {
                                    ret = Err(ProxyCopyError::from(e));
                                }
                            },
                            Err(e) => ret = Err(ProxyCopyError::from(e)),
                        }
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
                        match frontend_body_writer.finish().await {
                            Ok(w) => match backend_body_reader.drain().await {
                                Ok(r) => {
                                    ret = Ok((r, w));
                                }
                                Err(e) => {
                                    ret = Err(ProxyCopyError::from(e));
                                }
                            },
                            Err(e) => ret = Err(ProxyCopyError::from(e)),
                        }
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

        // Poll both futures in a loop until the backend-to-frontend future
        // resolves.
        let mut f2b_res = None;
        let b2f_res = loop {
            tokio::select! {
                f2b = &mut f2b_fut_pinned, if f2b_res.is_none() => f2b_res = Some(f2b),
                b2f = &mut b2f_fut_pinned => break b2f,
            }
        };

        // If polling both didn't result in the frontend-to-backend copy
        // completing, let's poll it once more to make sure it wasn't just about
        // to be finished (this can happen for short requests).
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

        backend_provider.give_connection(backend_reader, backend_writer);

        Ok(ProxyClient {
            frontend_reader,
            frontend_writer,
            backend_provider,
            options,
        })
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
    // TODO: should this list contain Trailer?
    // https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Connection
    // seems to think so.
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

impl<I: AsyncReadExt + Unpin> ProxyResponse<I> {
    pub async fn peek_body(&mut self, len: usize) -> BackendResult<(bool, &[u8])> {
        self.backend.peek_body(len).await
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
    /// Process a [`ResponseStream`] into a [`ProxyResponseStream`].
    ///
    /// `received_by` is the proxy identifier inserted into the `Via` header
    /// per RFC 9110 7.6.3 (e.g. a hostname or pseudonym).
    pub fn new(stream: ResponseStream<I>, received_by: &str) -> Self {
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
    use jrpxy_frontend::reader::FrontendReader;

    use crate::{BackendResponseStatus, OneshotBackend, ProxyClient, ProxyOptions};

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

        let bp = OneshotBackend::new(backend_reader.as_ref(), &mut backend_writer);

        let proxy_client = ProxyClient::new(
            frontend_reader.as_ref(),
            &mut frontend_writer,
            bp,
            ProxyOptions::default(),
        );

        let did_fe_read = proxy_client.start().await.expect("client start failed");

        let did_be_write = did_fe_read
            .write_backend_request()
            .await
            .expect("backend write failed");

        let be_res_stat = did_be_write
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        let rbir = match be_res_stat {
            BackendResponseStatus::Response(_read_backend_response) => {
                panic!("incorrect response type")
            }
            BackendResponseStatus::Informational(read_backend_informational_response) => {
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

        let rbr = match be_res_stat {
            BackendResponseStatus::Response(rbr) => rbr,
            BackendResponseStatus::Informational(_rbir) => {
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

        let client = rbr
            .forward_response()
            .await
            .expect("failed to forward response");

        // split up the client into pieces, and make sure they all reflect
        // what's expected.
        let (_frontend_reader, frontend_writer, backend_provider) = client.into_parts();
        let frontend_writer = frontend_writer.into_inner();
        let (_backend_reader, backend_writer) = backend_provider.inner.unwrap();
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
    async fn test_oneshot_transaction_drop_informational() {
        // todo!("test dropping the informational response when client is HTTP/1.0")
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
