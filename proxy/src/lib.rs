use jrpxy_backend::{BackendError, BackendReader, BackendWriter, ResponseStream};
use jrpxy_frontend::{FrontendReader, FrontendRequest, FrontendWriter};
use jrpxy_request_forwarder::{RequestForwarder, RequestForwarderError};
use jrpxy_response_forwarder::{
    InformationalResponseForwarder, ResponseForwarder, ResponseForwarderError,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub mod hop_by_hop;
pub mod request;
pub mod response;

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("Failed to forward request to backend: {0}")]
    ForwardRequest(#[from] RequestForwarderError),
    #[error("Failed to forward response to frontend: {0}")]
    ForwardResponse(#[from] ResponseForwarderError),
    #[error("Failed to read response from backend: {0}")]
    BackendError(#[from] BackendError),
    #[error(
        "Backend sent informational response with version HTTP/1.0 which does not support informational responses"
    )]
    BackendSentInformationalResponseWithHttp10,
}

// TODO: add a types named 'ProxyRequest' and 'ProxyResponse' that implement
// 'From<RequestForwarder>' and 'From<ResponseForwarder>' respectively. The
// purpose is to remove hop-by-hop headers, and make other preparations to the
// request for proxying. The resulting type lets the user add headers destined
// for the backend including hop-by-hop headers between the proxy and the
// backend.

pub type ProxyResult<T> = Result<T, ProxyError>;

pub enum TryProxyResult<FR, FW, BR, BW> {
    Error(ProxyError),
    InitialBackendWriteError {
        frontend_request: FrontendRequest<FR>,
        frontend_writer: FrontendWriter<FW>,
        error: ProxyError,
    },
    Complete {
        frontend_reader: FrontendReader<FR>,
        frontend_writer: FrontendWriter<FW>,
        backend_reader: BackendReader<BR>,
        backend_writer: BackendWriter<BW>,
    },
    ResponseCompleteWithUnexpectedClientError(ProxyError),
}

impl<FR, FW, BR, BW> From<ProxyError> for TryProxyResult<FR, FW, BR, BW> {
    fn from(e: ProxyError) -> Self {
        Self::Error(e)
    }
}

pub struct Proxy<FR, FW, BR, BW> {
    frontend_request: FrontendRequest<FR>,
    frontend_writer: FrontendWriter<FW>,
    backend_reader: BackendReader<BR>,
    backend_writer: BackendWriter<BW>,
}

impl<FR, FW, BR, BW> Proxy<FR, FW, BR, BW> {
    pub fn new(
        frontend_request: FrontendRequest<FR>,
        frontend_writer: FrontendWriter<FW>,
        backend_reader: BackendReader<BR>,
        backend_writer: BackendWriter<BW>,
    ) -> Self {
        Self {
            frontend_request,
            frontend_writer,
            backend_reader,
            backend_writer,
        }
    }
}

impl<FR, FW, BR, BW> Proxy<FR, FW, BR, BW>
where
    FR: AsyncReadExt + Unpin,
    FW: AsyncWriteExt + Unpin,
    BR: AsyncReadExt + Unpin,
    BW: AsyncWriteExt + Unpin,
{
    pub async fn try_proxy(self) -> TryProxyResult<FR, FW, BR, BW> {
        // Try to proxy the frontend request to the backend, and the backend
        // response to the frontend. We do this with a single task so that all
        // our type parameters do not need to be Send or 'static. We also do not
        // use a normal return type because it cannot express everything we want
        // to express.

        // TODO: we should not allow non-idempotent requests to be retried (RFC
        // 9110 sec 9.2.2). Idempotent methods include PUT, DELETE, and the safe
        // methods (GET, HEAD, OPTIONS, and TRACE).

        // TODO: we need to reject or strip hop-by-hop headers in the
        // FrontendRequest before passing it to the backend (and then inserting
        // our own).

        let Self {
            frontend_request,
            frontend_writer,
            backend_reader,
            backend_writer,
        } = self;

        let frontend_version = frontend_request.req().version();

        let frontend_supports_informational_response =
            frontend_version.supports_informational_response();

        let frontend_request_expects_body = frontend_request.req().method() != &b"HEAD"[..];

        // When the frontend sends a `Expect: 100-continue` header, it will not
        // send the body until it gets a `100 Continue` response from the
        // backend. If we get a `100 Continue` from the backend, we need to
        // forward it back to the frontend before we can expect to see any data
        // from the frontend. Then we will need to send the following responses
        // (including other possible 1XX responses) from the backend.

        let request_forwarder = RequestForwarder::new(frontend_request);
        let request_body_forwarder = match request_forwarder.try_forward_to(backend_writer).await {
            Ok(request_body_forwarder) => request_body_forwarder,
            Err((request_forwarder, error)) => {
                return TryProxyResult::InitialBackendWriteError {
                    frontend_request: request_forwarder.into_inner(),
                    frontend_writer,
                    error: error.into(),
                };
            }
        };

        // A task to drive forwarding the body.
        let request_body_forward_fut = request_body_forwarder.forward_body();
        let mut request_body_forward_result = None;
        tokio::pin!(request_body_forward_fut);

        // Now we create an async block that drives the full read of the
        // response and response body. We will drive this at the same time we
        // drive the request body forwarding future.
        let backend_job_fut = async {
            let mut frontend_writer = frontend_writer;
            let mut backend_reader = backend_reader;

            // To handle informational http responses, we need to possibly read
            // multiple requests from the backend.
            let (frontend_writer, backend_response) = loop {
                // Read a response from the backend.
                let backend_stream = backend_reader.read(frontend_request_expects_body).await?;
                let (informational_response, next_backend_reader) = match backend_stream {
                    ResponseStream::Response(backend_response) => {
                        break (frontend_writer, backend_response);
                    }
                    ResponseStream::Informational(response, backend_reader) => {
                        (response, backend_reader)
                    }
                };

                if !informational_response
                    .version()
                    .supports_informational_response()
                {
                    return Err(ProxyError::BackendSentInformationalResponseWithHttp10);
                }

                let next_frontend_writer = if frontend_supports_informational_response {
                    // Forward the informational response back to the frontend.
                    InformationalResponseForwarder::new(informational_response.into())
                        .forward_informational_to(frontend_writer)
                        .await?
                } else {
                    frontend_writer
                };

                // Setup for the next loop iteration
                frontend_writer = next_frontend_writer;
                backend_reader = next_backend_reader;
            };

            let backend_response = backend_response.into_version(frontend_version);

            // Send the response back to the frontend.
            let response_body_forwarder = ResponseForwarder::new(backend_response)
                .forward_to(frontend_writer)
                .await?;

            // Forward the response body to the frontend.
            Ok::<(BackendReader<BR>, FrontendWriter<FW>), ProxyError>(
                response_body_forwarder.forward_body().await?,
            )
        };
        tokio::pin!(backend_job_fut);

        // Copy the bodies in both directions until one fails, or forwarding the
        // response completes.
        let (backend_reader, frontend_writer) = loop {
            tokio::select! {
                req_body_forward = (&mut request_body_forward_fut), if request_body_forward_result.is_none() => {
                    let req_body_forward = match req_body_forward {
                        Ok(rbf) => rbf,
                        Err(e) => return TryProxyResult::from(ProxyError::from(e)),
                    };
                    request_body_forward_result = Some(req_body_forward);
                }

                res_body_forward = (&mut backend_job_fut) => {
                    let res_body_forward = match res_body_forward {
                        Ok(rbf) => rbf,
                        Err(e) => return TryProxyResult::from(e),
                    };
                    break res_body_forward;
                }

            }
        };

        if let Some((frontend_reader, backend_writer)) = request_body_forward_result.take() {
            TryProxyResult::Complete {
                frontend_reader,
                frontend_writer,
                backend_reader,
                backend_writer,
            }
        } else {
            // we're not done forwarding the frontend body to the backend even
            // though we have fully forwarded the response from the backend to
            // the frontend. we will try to finish sending the client body to
            // the server. if it fails, we'll return a special sort of success
            // indicating that we did fully send a response to the client, but
            // that the server did not receive the entire body.
            match request_body_forward_fut.await {
                Ok((frontend_reader, backend_writer)) => {
                    // we managed to finish sending the client body without an
                    // error. return the fully drained frontend and backend
                    TryProxyResult::Complete {
                        frontend_reader,
                        frontend_writer,
                        backend_reader,
                        backend_writer,
                    }
                }
                Err(e) => {
                    // we fully fowarded the backend response body to the
                    // frontend, but something went wrong sending the frontend
                    // body to the backend (likely either the client stopped
                    // sending the body, or the server hung up). since this
                    // leaves both sides in a partially drained/flushed state,
                    // we cannot safely return the frontend and backend.
                    TryProxyResult::ResponseCompleteWithUnexpectedClientError(e.into())
                }
            }
        }
    }

    pub async fn proxy(
        self,
    ) -> ProxyResult<(
        FrontendReader<FR>,
        FrontendWriter<FW>,
        BackendReader<BR>,
        BackendWriter<BW>,
    )> {
        match self.try_proxy().await {
            TryProxyResult::Error(proxy_error) => Err(proxy_error),
            TryProxyResult::InitialBackendWriteError {
                error: proxy_error, ..
            } => Err(proxy_error),
            TryProxyResult::Complete {
                frontend_reader,
                frontend_writer,
                backend_reader,
                backend_writer,
            } => Ok((
                frontend_reader,
                frontend_writer,
                backend_reader,
                backend_writer,
            )),
            TryProxyResult::ResponseCompleteWithUnexpectedClientError(proxy_error) => {
                Err(proxy_error)
            }
        }
    }
}

#[cfg(test)]
mod test {
    use jrpxy_backend::{BackendError, BackendReader, BackendWriter};
    use jrpxy_frontend::{FrontendReader, FrontendWriter};
    use jrpxy_util::debug::AsciiDebug;

    use crate::{Proxy, ProxyError};

    #[tokio::test]
    async fn proxy() {
        let frontend_request_buf = b"\
            GET / HTTP/1.1\r\n\
            content-length: 5\r\n\
            \r\n\
            01234\
            ";
        let mut frontend_response_buf = Vec::new();
        let mut backend_request_buf = Vec::new();
        let backend_response_buf = b"\
            HTTP/1.1 200 Ok\r\n\
            content-length: 10\r\n\
            \r\n\
            0123456789\
            ";

        let frontend_reader = FrontendReader::new(&frontend_request_buf[..], 8192);
        let frontend_writer = FrontendWriter::new(&mut frontend_response_buf);
        let backend_writer = BackendWriter::new(&mut backend_request_buf);
        let backend_reader = BackendReader::new(&backend_response_buf[..], 128);
        let frontend_request = frontend_reader
            .read()
            .await
            .expect("failed to read request head");

        let proxy = Proxy::new(
            frontend_request,
            frontend_writer,
            backend_reader,
            backend_writer,
        );

        let (_frontend_reader, _frontend_writer, _backend_reader, _backend_writer) =
            proxy.proxy().await.expect("failed to proxy");

        assert_eq!(
            AsciiDebug(&frontend_request_buf[..]),
            AsciiDebug(&backend_request_buf)
        );
        assert_eq!(
            AsciiDebug(&backend_response_buf[..]),
            AsciiDebug(&frontend_response_buf)
        );
    }

    #[tokio::test]
    async fn http_100_continue_forwarded_to_http11_client() {
        let frontend_request_buf = b"\
            GET / HTTP/1.1\r\n\
            expect: 100-continue\r\n\
            content-length: 5\r\n\
            \r\n\
            01234\
            ";

        let backend_response_buf = b"\
            HTTP/1.1 100 Continue\r\n\
            \r\n\
            HTTP/1.1 103 Early Hints\r\n\
            Link: a-link-goes-here\r\n\
            \r\n\
            HTTP/1.1 200 Ok\r\n\
            content-length: 10\r\n\
            \r\n\
            0123456789\
            ";

        let mut frontend_response_buf = Vec::new();
        let mut backend_request_buf = Vec::new();

        let frontend_reader = FrontendReader::new(&frontend_request_buf[..], 8192);
        let frontend_writer = FrontendWriter::new(&mut frontend_response_buf);
        let backend_writer = BackendWriter::new(&mut backend_request_buf);
        let backend_reader = BackendReader::new(&backend_response_buf[..], 128);
        let frontend_request = frontend_reader
            .read()
            .await
            .expect("failed to read request head");

        let proxy = Proxy::new(
            frontend_request,
            frontend_writer,
            backend_reader,
            backend_writer,
        );

        let (_frontend_reader, _frontend_writer, _backend_reader, _backend_writer) =
            proxy.proxy().await.expect("failed to proxy");

        assert_eq!(
            AsciiDebug(&frontend_request_buf[..]),
            AsciiDebug(&backend_request_buf)
        );
        assert_eq!(
            AsciiDebug(&backend_response_buf[..]),
            AsciiDebug(&frontend_response_buf)
        );
    }

    #[tokio::test]
    async fn http_100_continue_not_forwarded_to_http10_client() {
        // This test is the same as
        // http_100_continue_forwarded_to_http11_client, except that the client
        // has presented HTTP/1.0 as its version, so we cannot send it
        // 1xx-informational responses.

        let frontend_request_buf = b"\
            GET / HTTP/1.0\r\n\
            expect: 100-continue\r\n\
            content-length: 5\r\n\
            \r\n\
            01234\
            ";

        let backend_response_buf = b"\
            HTTP/1.1 100 Continue\r\n\
            \r\n\
            HTTP/1.1 103 Early Hints\r\n\
            Link: a-link-goes-here\r\n\
            \r\n\
            HTTP/1.1 200 Ok\r\n\
            content-length: 10\r\n\
            \r\n\
            0123456789\
            ";

        let frontend_response_expected = b"\
            HTTP/1.0 200 Ok\r\n\
            content-length: 10\r\n\
            \r\n\
            0123456789\
            ";

        let mut frontend_response_buf = Vec::new();
        let mut backend_request_buf = Vec::new();

        let frontend_reader = FrontendReader::new(&frontend_request_buf[..], 8192);
        let frontend_writer = FrontendWriter::new(&mut frontend_response_buf);
        let backend_writer = BackendWriter::new(&mut backend_request_buf);
        let backend_reader = BackendReader::new(&backend_response_buf[..], 128);
        let frontend_request = frontend_reader
            .read()
            .await
            .expect("failed to read request head");

        let proxy = Proxy::new(
            frontend_request,
            frontend_writer,
            backend_reader,
            backend_writer,
        );

        let (_frontend_reader, _frontend_writer, _backend_reader, _backend_writer) =
            proxy.proxy().await.expect("failed to proxy");

        assert_eq!(
            AsciiDebug(&frontend_request_buf[..]),
            AsciiDebug(&backend_request_buf)
        );
        assert_eq!(
            AsciiDebug(&frontend_response_expected[..]),
            AsciiDebug(&frontend_response_buf[..])
        );
    }

    #[tokio::test]
    async fn http_100_continue_from_http10_origin_is_error() {
        let frontend_request_buf = b"\
            GET / HTTP/1.0\r\n\
            expect: 100-continue\r\n\
            content-length: 5\r\n\
            \r\n\
            01234\
            ";

        let backend_response_buf = b"\
            HTTP/1.0 100 Continue\r\n\
            \r\n\
            HTTP/1.0 200 Ok\r\n\
            content-length: 10\r\n\
            \r\n\
            0123456789\
            ";

        let mut frontend_response_buf = Vec::new();
        let mut backend_request_buf = Vec::new();

        let frontend_reader = FrontendReader::new(&frontend_request_buf[..], 8192);
        let frontend_writer = FrontendWriter::new(&mut frontend_response_buf);
        let backend_writer = BackendWriter::new(&mut backend_request_buf);
        let backend_reader = BackendReader::new(&backend_response_buf[..], 128);
        let frontend_request = frontend_reader
            .read()
            .await
            .expect("failed to read request head");

        let proxy = Proxy::new(
            frontend_request,
            frontend_writer,
            backend_reader,
            backend_writer,
        );

        let Err(e) = proxy.proxy().await else {
            panic!("did not get error when expected");
        };
        assert!(matches!(
            e,
            ProxyError::BackendSentInformationalResponseWithHttp10
        ));
    }

    #[tokio::test]
    async fn head_request_does_not_expect_body() {
        let frontend_request_buf = b"\
            HEAD / HTTP/1.0\r\n\
            \r\n\
            ";

        let backend_response_buf = b"\
            HTTP/1.0 200 Ok\r\n\
            content-length: 10\r\n\
            \r\n\
            ";

        let mut frontend_response_buf = Vec::new();
        let mut backend_request_buf = Vec::new();

        let frontend_reader = FrontendReader::new(&frontend_request_buf[..], 8192);
        let frontend_writer = FrontendWriter::new(&mut frontend_response_buf);
        let backend_writer = BackendWriter::new(&mut backend_request_buf);
        let backend_reader = BackendReader::new(&backend_response_buf[..], 128);
        let frontend_request = frontend_reader
            .read()
            .await
            .expect("failed to read request head");

        let proxy = Proxy::new(
            frontend_request,
            frontend_writer,
            backend_reader,
            backend_writer,
        );

        let _ = proxy.proxy().await.expect("proxy failed");

        assert_eq!(backend_response_buf.as_ref(), &frontend_response_buf);
    }

    #[tokio::test]
    async fn invalid_head_response_does_not_get_body_read() {
        let frontend_request_buf = b"\
            HEAD / HTTP/1.0\r\n\
            \r\n\
            ";

        let backend_response_buf = b"\
            HTTP/1.0 200 Ok\r\n\
            content-length: 5\r\n\
            \r\n\
            01234
            ";

        let mut frontend_response_buf = Vec::new();
        let mut backend_request_buf = Vec::new();

        let frontend_reader = FrontendReader::new(&frontend_request_buf[..], 8192);
        let frontend_writer = FrontendWriter::new(&mut frontend_response_buf);
        let backend_writer = BackendWriter::new(&mut backend_request_buf);
        let backend_reader = BackendReader::new(&backend_response_buf[..], 128);
        let frontend_request = frontend_reader
            .read()
            .await
            .expect("failed to read request head");

        let proxy = Proxy::new(
            frontend_request,
            frontend_writer,
            backend_reader,
            backend_writer,
        );

        let (_fr, _fw, br, _bw) = proxy.proxy().await.expect("proxy failed");

        let valid_response_buf = b"\
            HTTP/1.0 200 Ok\r\n\
            content-length: 5\r\n\
            \r\n\
            ";
        assert_eq!(
            valid_response_buf.as_ref(),
            &backend_response_buf[0..valid_response_buf.len()],
            "test requires that valid_response_buf be a prefix of backend_response_buf"
        );

        assert_eq!(valid_response_buf.as_ref(), &frontend_response_buf);

        let backend_err = br
            .read(true)
            .await
            .expect_err("the body bytes remaining in the backend should result in a framing error");

        assert!(matches!(
            backend_err,
            BackendError::HttpResponseParseError(httparse::Error::Version)
        ));
    }
}
