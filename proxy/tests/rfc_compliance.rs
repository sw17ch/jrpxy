//! RFC 9110 / RFC 9112 compliance gaps.
//!
//! The tests below are written to FAIL until the corresponding
//! validation is added to the proxy. Each test cites the specific
//! RFC clause it exercises and the security/interop concern.

use jrpxy_backend::{reader::BackendReader, writer::BackendWriter};
use jrpxy_frontend::{reader::FrontendReader, writer::FrontendWriter};
use jrpxy_proxy::{BackendResponseStream, FrontendProxyRequest, ProxyFrontendError, ProxyOptions};

/// RFC 9112 section 3.2.2: "A server MUST respond with a 400 (Bad
/// Request) status code to any HTTP/1.1 request message that
/// lacks a Host header field and to any request message that
/// contains more than one Host header field..."
#[tokio::test]
async fn rfc9112_request_with_multiple_host_headers_is_rejected() {
    let frontend_reader = b"\
        GET / HTTP/1.1\r\n\
        Host: example.com\r\n\
        Host: evil.example.com\r\n\
        \r\n";
    let mut frontend_writer: Vec<u8> = Vec::new();

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    if FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options).is_ok() {
        panic!(
            "RFC 9112 section 3.2.2: an HTTP/1.1 request with more than one \
             Host header field MUST be rejected."
        );
    }
}

/// RFC 9112 section 3.2.2: "A server MUST respond with a 400 (Bad
/// Request) status code to any HTTP/1.1 request message that
/// lacks a Host header field..."
#[tokio::test]
async fn rfc9112_http11_request_without_host_is_rejected() {
    let frontend_reader = b"\
        GET / HTTP/1.1\r\n\
        \r\n";
    let mut frontend_writer: Vec<u8> = Vec::new();

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    if FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options).is_ok() {
        panic!(
            "RFC 9112 section 3.2.2: an HTTP/1.1 request without a Host \
             header field MUST be rejected."
        );
    }
}

/// RFC 9112 section 6.1: "Transfer-Encoding was added in HTTP/1.1." A
/// recipient that observes Transfer-Encoding on an HTTP/1.0
/// request has either an actual HTTP/1.0 sender that is
/// non-conformant or a confused intermediary; in either case the
/// proxy must not blindly forward it.
#[tokio::test]
async fn rfc9112_http10_request_with_transfer_encoding_is_rejected() {
    let frontend_reader = b"\
        POST / HTTP/1.0\r\n\
        Host: example.com\r\n\
        Transfer-Encoding: chunked\r\n\
        \r\n\
        0\r\n\
        \r\n";
    if FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, 8192)
        .await
        .is_ok()
    {
        panic!(
            "RFC 9112 section 6.1: Transfer-Encoding is HTTP/1.1-only; an \
             HTTP/1.0 request bearing Transfer-Encoding must be \
             rejected at read."
        );
    }
}

/// RFC 9112 section 6.1, response direction: "Transfer-Encoding was added in
/// HTTP/1.1." A recipient that observes Transfer-Encoding on an HTTP/1.0
/// response has either a non-conformant HTTP/1.0 origin or a confused
/// intermediary; in either case the proxy must not blindly forward it. This is
/// the response-side counterpart to
/// `rfc9112_http10_request_with_transfer_encoding_is_rejected`.
#[tokio::test]
async fn rfc9112_http10_response_with_transfer_encoding_is_rejected() {
    let frontend_reader = b"\
        GET / HTTP/1.1\r\n\
        Host: example.com\r\n\
        \r\n";
    let mut frontend_writer: Vec<u8> = Vec::new();

    let backend_reader = b"\
        HTTP/1.0 200 Ok\r\n\
        Transfer-Encoding: chunked\r\n\
        \r\n\
        0\r\n\
        \r\n";
    let mut backend_writer = Vec::new();

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let req = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed");

    let backend_connection = (
        BackendReader::new(backend_reader.as_ref(), 256),
        BackendWriter::new(&mut backend_writer),
    );
    let backend_request = req
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("backend write failed");

    if backend_request.read_backend_response().await.is_ok() {
        panic!(
            "RFC 9112 section 6.1: Transfer-Encoding is HTTP/1.1-only; an \
             HTTP/1.0 response bearing Transfer-Encoding must be \
             rejected."
        );
    }
}

/// RFC 9112 section 3.2.2: "When a proxy receives a request with an
/// absolute-form of request-target, the proxy MUST ignore the
/// received Host header field (if any) and instead replace it
/// with the host information of the request-target."
///
/// This is also a security concern: if the absolute-form URI and
/// the Host header disagree, treating the Host header as
/// authoritative lets a client route the request to one origin
/// while an upstream cache (or the origin itself) sees a
/// different routing key than the request body suggests, which
/// is a cache-poisoning / request-routing-confusion vector.
///
/// Per section 3.2.1, a proxy MUST also accept the absolute-form, and
/// per section 3.2.2 a client MUST send absolute-form to a proxy, so
/// the parse path itself must not refuse it.
#[tokio::test]
async fn rfc9112_absolute_form_uri_authority_replaces_host() {
    let frontend_reader = b"\
        GET http://origin.example.com/path HTTP/1.1\r\n\
        Host: spoofed.example.com\r\n\
        \r\n";
    let mut frontend_writer: Vec<u8> = Vec::new();

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let req = match FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options) {
        Ok(r) => r,
        Err(_) => panic!(
            "RFC 9112 section 3.2.1: a proxy MUST accept absolute-form \
             request-targets (clients are required to send them to \
             a proxy per section 3.2.2)."
        ),
    };

    let backend_req = req.into_backend_request();
    let host = backend_req
        .req()
        .headers()
        .get_header("host")
        .next()
        .map(|(_, v)| v);

    let host_str = host
        .as_ref()
        .map(|v| std::str::from_utf8(v.as_ref()).unwrap_or("<non-utf8>"))
        .unwrap_or("<missing>");
    if host_str != "origin.example.com" {
        panic!(
            "RFC 9112 section 3.2.2: forwarded Host must be derived from \
             the absolute-form URI authority (expected \
             \"origin.example.com\"), got {host_str:?}."
        );
    }
}

/// RFC 9112 section 3.2.2 follow-on: after Host substitution from the
/// absolute-form authority, the path forwarded to the origin must be in
/// origin-form (the absolute-form scheme + authority are dropped).
#[tokio::test]
async fn rfc9112_absolute_form_path_rewritten_to_origin_form() {
    let frontend_reader = b"\
        GET http://origin.example.com/path?q=1 HTTP/1.1\r\n\
        Host: spoofed.example.com\r\n\
        \r\n";
    let mut frontend_writer: Vec<u8> = Vec::new();

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let req = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("must accept absolute-form");
    let backend_req = req.into_backend_request();
    assert_eq!(
        backend_req.req().path().as_ref(),
        b"/path?q=1",
        "absolute-form path must be rewritten to origin-form before forwarding"
    );
}

/// RFC 9112 section 3.2.1: when the target URI path is empty, the
/// origin-form path MUST be `/`.
#[tokio::test]
async fn rfc9112_absolute_form_empty_path_becomes_slash() {
    let frontend_reader = b"\
        GET http://example.com HTTP/1.1\r\n\
        \r\n";
    let mut frontend_writer: Vec<u8> = Vec::new();

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let req = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("absolute-form with empty path must be accepted");
    let backend_req = req.into_backend_request();
    assert_eq!(
        backend_req.req().path().as_ref(),
        b"/",
        "absolute-form with empty path must be forwarded as `/`"
    );
}

/// RFC 9110 section 4.2.4 deprecates `userinfo@host` in URIs; forwarding
/// embedded credentials silently is a credential-leak / impersonation
/// footgun, so the proxy rejects them outright.
#[tokio::test]
async fn absolute_form_with_userinfo_is_rejected() {
    let frontend_reader = b"\
        GET http://user:pass@example.com/x HTTP/1.1\r\n\
        \r\n";
    let mut frontend_writer: Vec<u8> = Vec::new();

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    if FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options).is_ok() {
        panic!(
            "RFC 9110 section 4.2.4: absolute-form with userinfo@host must be \
             rejected to avoid silently forwarding embedded credentials."
        );
    }
}

/// OPTIONS targets the gateway itself (Max-Forwards, the origin-wide `*`
/// request-target), so it cannot be blindly forwarded. The proxy rejects every
/// OPTIONS request and leaves handling to the caller.
#[tokio::test]
async fn options_is_rejected() {
    let frontend_reader = b"\
        OPTIONS * HTTP/1.1\r\n\
        Host: example.com\r\n\
        \r\n";
    let mut frontend_writer: Vec<u8> = Vec::new();

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let req_err = match FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options) {
        Ok(_) => panic!("expected error for OPTIONS, got Ok"),
        Err(e) => e,
    };
    assert!(matches!(
        req_err.error(),
        ProxyFrontendError::OptionsNotSupported
    ));
}

/// Even though absolute-form requests are normalized to origin-form for
/// forwarding (and Host is substituted from the URI authority), the
/// pre-normalization bytes must remain inspectable on the
/// `FrontendProxyRequest`. Callers building bot-fingerprinting / abuse
/// policies rely on signal from the bytes the client actually sent.
#[tokio::test]
async fn absolute_form_preserves_un_normalized_values() {
    let frontend_reader = b"\
        GET http://origin.example.com/path HTTP/1.1\r\n\
        Host: spoofed.example.com\r\n\
        \r\n";
    let mut frontend_writer: Vec<u8> = Vec::new();

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let req = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("absolute-form must be accepted");

    // The request-target bytes the client sent are preserved on the original.
    assert_eq!(
        req.original().path().as_ref(),
        b"http://origin.example.com/path"
    );

    // The (potentially spoofed) Host the client sent is preserved on the
    // original, separate from the substituted Host on the forwarded request.
    let original_host = req
        .original()
        .headers()
        .get_header("host")
        .next()
        .expect("original Host must be preserved");
    assert_eq!(original_host.1.as_ref(), b"spoofed.example.com");

    // Sanity check: the forwarded request bears the authority-derived Host.
    let new_host = req
        .req()
        .headers()
        .get_header("host")
        .next()
        .expect("Host present");
    assert_eq!(new_host.1.as_ref(), b"origin.example.com");
}

/// Origin-form requests pass through unchanged, so the preserved original
/// matches the forwarded request.
#[tokio::test]
async fn origin_form_original_matches_forwarded() {
    let frontend_reader = b"\
        GET /foo HTTP/1.1\r\n\
        Host: example.com\r\n\
        \r\n";
    let mut frontend_writer: Vec<u8> = Vec::new();

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let req = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("origin-form must be accepted");

    assert_eq!(req.original().path().as_ref(), b"/foo");
    assert_eq!(req.req().path().as_ref(), b"/foo");
}

/// The client's Host header on an origin-form request must be readable from
/// `original()` so callers (audit logging, abuse detection) can attribute the
/// request to the host the client actually addressed.
#[tokio::test]
async fn origin_form_original_exposes_client_host() {
    let frontend_reader = b"\
        GET /foo HTTP/1.1\r\n\
        Host: client.example.com\r\n\
        \r\n";
    let mut frontend_writer: Vec<u8> = Vec::new();

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let req = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("origin-form must be accepted");

    let original_host = req
        .original()
        .headers()
        .get_header("host")
        .next()
        .expect("original Host must be present");
    assert_eq!(original_host.1.as_ref(), b"client.example.com");
}

/// On absolute-form, the proxy substitutes Host from the request-target
/// authority - but the client's Host on the wire must remain readable via
/// `original()` for downstream inspection.
#[tokio::test]
async fn absolute_form_original_exposes_client_host_pre_substitution() {
    let frontend_reader = b"\
        GET http://origin.example.com/x HTTP/1.1\r\n\
        Host: spoofed.example.com\r\n\
        \r\n";
    let mut frontend_writer: Vec<u8> = Vec::new();

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let req = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("absolute-form must be accepted");

    let original_host = req
        .original()
        .headers()
        .get_header("host")
        .next()
        .expect("original Host must be present");
    assert_eq!(original_host.1.as_ref(), b"spoofed.example.com");

    let forwarded_host = req
        .req()
        .headers()
        .get_header("host")
        .next()
        .expect("forwarded Host must be present");
    assert_eq!(forwarded_host.1.as_ref(), b"origin.example.com");

    assert_ne!(
        original_host.1.as_ref(),
        forwarded_host.1.as_ref(),
        "original and forwarded Host must diverge for absolute-form"
    );
}

/// RFC 9112 section 9.6: "The 'close' connection option is defined as a
/// signal that the sender will close this connection after completion of the
/// response." A proxy that receives `Connection: close` from the client MUST
/// NOT reuse the frontend connection for another request - `BodyExchanger::
/// finish` is the choke point that decides whether to hand the caller back a
/// reusable frontend connection; an honored close must surface as `None` there.
#[tokio::test]
async fn rfc9112_request_connection_close_discards_frontend() {
    let frontend_reader = b"\
        GET / HTTP/1.1\r\n\
        Host: example.com\r\n\
        Connection: close\r\n\
        \r\n";
    let mut frontend_writer: Vec<u8> = Vec::new();

    let backend_reader = b"\
        HTTP/1.1 200 Ok\r\n\
        content-length: 5\r\n\
        \r\n\
        hello";
    let mut backend_writer = Vec::new();

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let req = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed");

    let backend_connection = (
        BackendReader::new(backend_reader.as_ref(), 256),
        BackendWriter::new(&mut backend_writer),
    );
    let backend_request = req
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("backend write failed");

    let final_response = match backend_request
        .read_backend_response()
        .await
        .expect("failed to read backend response")
    {
        BackendResponseStream::Final(r) => r,
        BackendResponseStream::Informational(_) => {
            panic!("unexpected informational response from fixed backend")
        }
    };

    let (client, _backend_connection) = final_response
        .forward_response_head()
        .await
        .expect("forward_response_head failed")
        .finish()
        .await
        .expect("failed to forward response");

    if client.is_some() {
        panic!(
            "RFC 9112 section 9.6: a frontend that sent `Connection: close` must \
             not be reused; finish() must return None for the frontend \
             connection."
        );
    }
}

/// RFC 9112 section 9.6, backend direction: an origin that returns
/// `Connection: close` is announcing it will close the connection after the
/// response, so the proxy MUST NOT reuse that backend connection for a
/// subsequent request. `BodyExchanger::finish` reports reusability via the
/// `Option<BackendConnection>` it returns; an honored backend close must
/// surface as `None` there.
#[tokio::test]
async fn rfc9112_response_connection_close_discards_backend() {
    let frontend_reader = b"\
        GET / HTTP/1.1\r\n\
        Host: example.com\r\n\
        \r\n";
    let mut frontend_writer: Vec<u8> = Vec::new();

    // Content-Length framing (rather than EOF) is used deliberately: an EOF-
    // framed response would force the backend connection closed regardless of
    // any `Connection` header, masking the question this test asks.
    let backend_reader = b"\
        HTTP/1.1 200 Ok\r\n\
        content-length: 5\r\n\
        Connection: close\r\n\
        \r\n\
        hello";
    let mut backend_writer = Vec::new();

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let req = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed");

    let backend_connection = (
        BackendReader::new(backend_reader.as_ref(), 256),
        BackendWriter::new(&mut backend_writer),
    );
    let backend_request = req
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("backend write failed");

    let final_response = match backend_request
        .read_backend_response()
        .await
        .expect("failed to read backend response")
    {
        BackendResponseStream::Final(r) => r,
        BackendResponseStream::Informational(_) => {
            panic!("unexpected informational response from fixed backend")
        }
    };

    let (_client, backend_connection) = final_response
        .forward_response_head()
        .await
        .expect("forward_response_head failed")
        .finish()
        .await
        .expect("failed to forward response");

    if backend_connection.is_some() {
        panic!(
            "RFC 9112 section 9.6: a backend that sent `Connection: close` must \
             not be reused; finish() must return None for the backend \
             connection."
        );
    }
}

/// RFC 9112 section 9.3: "If the received protocol is HTTP/1.0, the 'keep-
/// alive' connection option [must] be present...; otherwise, [the recipient]
/// will assume the connection will be closed after the current
/// request/response is complete." An HTTP/1.0 frontend that does not opt in
/// to keep-alive must therefore have its connection discarded after the
/// exchange, even when the response body is Content-Length-framed (i.e. body
/// delimitation alone would not force the close).
#[tokio::test]
async fn rfc9112_http10_frontend_without_keep_alive_discards_frontend() {
    let frontend_reader = b"\
        GET / HTTP/1.0\r\n\
        Host: example.com\r\n\
        \r\n";
    let mut frontend_writer: Vec<u8> = Vec::new();

    // Content-Length framing keeps the close decision from being forced by
    // body delimitation; the only reason to close here is the HTTP/1.0
    // default-close rule.
    let backend_reader = b"\
        HTTP/1.1 200 Ok\r\n\
        content-length: 5\r\n\
        \r\n\
        hello";
    let mut backend_writer = Vec::new();

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let req = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed");

    let backend_connection = (
        BackendReader::new(backend_reader.as_ref(), 256),
        BackendWriter::new(&mut backend_writer),
    );
    let backend_request = req
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("backend write failed");

    let final_response = match backend_request
        .read_backend_response()
        .await
        .expect("failed to read backend response")
    {
        BackendResponseStream::Final(r) => r,
        BackendResponseStream::Informational(_) => {
            panic!("unexpected informational response from fixed backend")
        }
    };

    let (client, _backend_connection) = final_response
        .forward_response_head()
        .await
        .expect("forward_response_head failed")
        .finish()
        .await
        .expect("failed to forward response");

    if client.is_some() {
        panic!(
            "RFC 9112 section 9.3: an HTTP/1.0 frontend without `Connection: \
             keep-alive` must not be reused; finish() must return None for \
             the frontend connection."
        );
    }
}

/// RFC 9112 section 9.3, backend direction: when the proxy talks to an
/// HTTP/1.0 origin that does not advertise `Connection: keep-alive`, the
/// proxy must assume the origin will close the connection after the response
/// and therefore must not reuse the backend connection. Content-Length
/// framing is used so the close decision is driven solely by the HTTP/1.0
/// default-close rule, not by body delimitation.
#[tokio::test]
async fn rfc9112_http10_backend_without_keep_alive_discards_backend() {
    let frontend_reader = b"\
        GET / HTTP/1.1\r\n\
        Host: example.com\r\n\
        \r\n";
    let mut frontend_writer: Vec<u8> = Vec::new();

    let backend_reader = b"\
        HTTP/1.0 200 Ok\r\n\
        content-length: 5\r\n\
        \r\n\
        hello";
    let mut backend_writer = Vec::new();

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let req = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed");

    let backend_connection = (
        BackendReader::new(backend_reader.as_ref(), 256),
        BackendWriter::new(&mut backend_writer),
    );
    let backend_request = req
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("backend write failed");

    let final_response = match backend_request
        .read_backend_response()
        .await
        .expect("failed to read backend response")
    {
        BackendResponseStream::Final(r) => r,
        BackendResponseStream::Informational(_) => {
            panic!("unexpected informational response from fixed backend")
        }
    };

    let (_client, backend_connection) = final_response
        .forward_response_head()
        .await
        .expect("forward_response_head failed")
        .finish()
        .await
        .expect("failed to forward response");

    if backend_connection.is_some() {
        panic!(
            "RFC 9112 section 9.3: an HTTP/1.0 backend without `Connection: \
             keep-alive` must not be reused; finish() must return None for \
             the backend connection."
        );
    }
}

/// RFC 9112 section 3.2.4: asterisk-form is invalid for non-OPTIONS methods.
#[tokio::test]
async fn asterisk_form_rejected_for_non_options() {
    let frontend_reader = b"\
        GET * HTTP/1.1\r\n\
        Host: example.com\r\n\
        \r\n";
    let mut frontend_writer: Vec<u8> = Vec::new();

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    if FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options).is_ok() {
        panic!(
            "RFC 9112 section 3.2.4: asterisk-form request-target is only \
             valid for OPTIONS."
        );
    }
}

/// RFC 9112 section 3.2.1: when an absolute-form request-target has an empty
/// path but a non-empty query (e.g. `http://example.com?q=1`), the origin-form
/// MUST be `/?q=1` -- the empty path becomes `/` and the query is appended.
///
/// The current code rejects this case because `?q=1` does not start with `/`,
/// failing `validate_origin_form`. The fix is to prepend `/` when the
/// path-abempty is empty and a query is present.
#[tokio::test]
async fn rfc9112_absolute_form_empty_path_with_query_becomes_slash_query() {
    let frontend_reader = b"\
        GET http://example.com?q=1 HTTP/1.1\r\n\
        \r\n";
    let mut frontend_writer: Vec<u8> = Vec::new();

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let req = match FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options) {
        Ok(r) => r,
        Err(_) => panic!(
            "RFC 9112 section 3.2.1: absolute-form with empty path and a \
             query (http://example.com?q=1) must be accepted and \
             normalized to /?q=1, not rejected."
        ),
    };

    assert_eq!(
        req.req().path().as_ref(),
        b"/?q=1",
        "RFC 9112 section 3.2.1: empty path + query must normalize to /?q=1"
    );
}

/// RFC 9110 section 15.2 (robustness): a gateway that receives an unknown 1xx
/// informational response from its backend MUST NOT hard-error. Even though the
/// MUST-forward requirement targets forward proxies specifically, a gateway
/// acting as origin server should be robust to newly-registered 1xx codes. At
/// minimum it should consume and discard them without failing the transaction.
///
/// The exact wording is: "A client MUST be able to parse one or more 1xx
/// responses received prior to a final response, even if the client does not
/// expect one. A user agent MAY ignore unexpected 1xx responses." The gateway
/// acts as a client, so at least should discard the unknown 1xx responses
/// rather than error.
#[tokio::test]
async fn rfc9110_unknown_1xx_from_backend_does_not_hard_error() {
    let frontend_reader = b"\
        GET / HTTP/1.1\r\n\
        Host: example.com\r\n\
        \r\n";
    let mut frontend_writer: Vec<u8> = Vec::new();

    // Backend sends a hypothetical 199 informational response followed by
    // the final 200.
    let backend_reader = b"\
        HTTP/1.1 199 Custom Info\r\n\
        X-Hint: something\r\n\
        \r\n\
        HTTP/1.1 200 Ok\r\n\
        content-length: 5\r\n\
        \r\n\
        hello";
    let mut backend_writer = Vec::new();

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let req = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed");

    let backend_connection = (
        BackendReader::new(backend_reader.as_ref(), 256),
        BackendWriter::new(&mut backend_writer),
    );
    let response_reader = req
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("backend write failed");

    // The gateway must not return an error just because the backend sent an
    // unrecognized 1xx code. It should either forward it or silently consume
    // it, then continue reading the final response.
    let stream = response_reader.read_backend_response().await.expect(
        "RFC 9110 section 15.2: unknown 1xx informational responses from \
             the backend must not cause a hard error. The gateway should \
             consume or forward them and continue to the final response.",
    );

    // Verify we can reach the final 200 response.
    let final_response = match stream {
        BackendResponseStream::Final(r) => r,
        BackendResponseStream::Informational(info) => {
            // The unknown 1xx was surfaced as informational -- that's fine,
            // as long as we can continue to the final response.
            let next = info
                .forward_informational_response()
                .await
                .expect("forwarding unknown 1xx must not fail");
            match next.into_inner() {
                BackendResponseStream::Final(r) => r,
                BackendResponseStream::Informational(_) => {
                    panic!("expected final response after unknown 1xx")
                }
            }
        }
    };

    assert_eq!(final_response.as_response().res().code(), 200);
}

/// RFC 9110 section 8.6: "If a message is received with [...] multiple
/// Content-Length header fields having field values consisting of the same
/// decimal value [...], the recipient MUST [...] accept" it. This requirement
/// is addressed to "a recipient," so it applies to a gateway receiving
/// responses from its backend (and requests from its frontend).
#[tokio::test]
async fn rfc9110_identical_duplicate_content_length_accepted() {
    let frontend_reader = b"\
        GET / HTTP/1.1\r\n\
        Host: example.com\r\n\
        \r\n";
    let mut frontend_writer: Vec<u8> = Vec::new();

    // Backend sends two Content-Length headers with the same value.
    let backend_reader = b"\
        HTTP/1.1 200 Ok\r\n\
        Content-Length: 5\r\n\
        Content-Length: 5\r\n\
        \r\n\
        hello";
    let mut backend_writer = Vec::new();

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let req = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed");

    let backend_connection = (
        BackendReader::new(backend_reader.as_ref(), 256),
        BackendWriter::new(&mut backend_writer),
    );
    let response_reader = req
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("backend write failed");

    let stream = response_reader.read_backend_response().await.expect(
        "RFC 9110 section 8.6: duplicate Content-Length headers with \
         identical values MUST be accepted by the recipient.",
    );

    let final_response = match stream {
        BackendResponseStream::Final(r) => r,
        BackendResponseStream::Informational(_) => {
            panic!("unexpected informational response")
        }
    };

    assert_eq!(final_response.as_response().res().code(), 200);
}

/// RFC 9110 section 8.6: a single Content-Length field with a comma-separated
/// list of identical values (e.g. `Content-Length: 5, 5`) MUST be treated the
/// same as a single occurrence of that value.
#[tokio::test]
async fn rfc9110_comma_separated_identical_content_length_accepted() {
    let frontend_reader = b"\
        GET / HTTP/1.1\r\n\
        Host: example.com\r\n\
        \r\n";
    let mut frontend_writer: Vec<u8> = Vec::new();

    // Backend sends a single Content-Length header with comma-separated
    // identical values.
    let backend_reader = b"\
        HTTP/1.1 200 Ok\r\n\
        Content-Length: 5, 5\r\n\
        \r\n\
        hello";
    let mut backend_writer = Vec::new();

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let req = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed");

    let backend_connection = (
        BackendReader::new(backend_reader.as_ref(), 256),
        BackendWriter::new(&mut backend_writer),
    );
    let response_reader = req
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("backend write failed");

    let stream = response_reader.read_backend_response().await.expect(
        "RFC 9110 section 8.6: a single Content-Length field with \
         comma-separated identical values (e.g. '5, 5') MUST be \
         accepted as equivalent to a single value.",
    );

    let final_response = match stream {
        BackendResponseStream::Final(r) => r,
        BackendResponseStream::Informational(_) => {
            panic!("unexpected informational response")
        }
    };

    assert_eq!(final_response.as_response().res().code(), 200);
}

/// RFC 9110 section 6.6.1: "An origin server MUST send a Date header field in
/// all cases" (with narrow exceptions). Since a gateway acts as origin server
/// to clients, it MUST inject a Date header when the backend omits one.
#[tokio::test]
async fn rfc9110_date_header_injected_when_backend_omits_it() {
    let frontend_reader = b"\
        GET / HTTP/1.1\r\n\
        Host: example.com\r\n\
        \r\n";
    let mut frontend_writer: Vec<u8> = Vec::new();

    // Backend deliberately omits the Date header.
    let backend_reader = b"\
        HTTP/1.1 200 Ok\r\n\
        content-length: 5\r\n\
        \r\n\
        hello";
    let mut backend_writer = Vec::new();

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let req = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed");

    let backend_connection = (
        BackendReader::new(backend_reader.as_ref(), 256),
        BackendWriter::new(&mut backend_writer),
    );
    let response_reader = req
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("backend write failed");

    let final_response = match response_reader
        .read_backend_response()
        .await
        .expect("failed to read backend response")
    {
        BackendResponseStream::Final(r) => r,
        BackendResponseStream::Informational(_) => {
            panic!("unexpected informational response")
        }
    };

    let (_client, _backend_connection) = final_response
        .forward_response_head()
        .await
        .expect("forward_response_head failed")
        .finish()
        .await
        .expect("failed to forward response");

    let output = String::from_utf8_lossy(&frontend_writer);
    assert!(
        output.contains("Date: ") || output.contains("date: "),
        "RFC 9110 section 6.6.1: a gateway acting as origin server MUST \
         inject a Date header when the backend omits one. Got:\n{output}"
    );
}

/// RFC 9110 section 6.6.1: "the origin server MUST generate a Date header
/// field that contains the time the message was generated". When the
/// backend already supplied a Date, the gateway must forward that exact
/// value rather than overwrite it - the backend's timestamp is closer to
/// message generation. The chosen fixture date is clearly in the past, so
/// any regression that stamps the current time instead would fail the
/// substring check below.
#[tokio::test]
async fn rfc9110_date_header_from_backend_is_preserved_unchanged() {
    let frontend_reader = b"\
        GET / HTTP/1.1\r\n\
        Host: example.com\r\n\
        \r\n";
    let mut frontend_writer: Vec<u8> = Vec::new();

    let backend_reader = b"\
        HTTP/1.1 200 Ok\r\n\
        Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
        content-length: 5\r\n\
        \r\n\
        hello";
    let mut backend_writer = Vec::new();

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let req = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed");

    let backend_connection = (
        BackendReader::new(backend_reader.as_ref(), 256),
        BackendWriter::new(&mut backend_writer),
    );
    let response_reader = req
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("backend write failed");

    let final_response = match response_reader
        .read_backend_response()
        .await
        .expect("failed to read backend response")
    {
        BackendResponseStream::Final(r) => r,
        BackendResponseStream::Informational(_) => {
            panic!("unexpected informational response")
        }
    };

    let (_client, _backend_connection) = final_response
        .forward_response_head()
        .await
        .expect("forward_response_head failed")
        .finish()
        .await
        .expect("failed to forward response");

    let output = String::from_utf8_lossy(&frontend_writer);
    assert!(
        output.contains("Date: Mon, 01 Jan 2026 00:00:00 GMT"),
        "RFC 9110 section 6.6.1: a backend-supplied Date MUST be \
         forwarded unchanged. Got:\n{output}"
    );
    let date_count = output.matches("Date:").count() + output.matches("date:").count();
    assert_eq!(
        1, date_count,
        "Exactly one Date header must appear in the forwarded response; \
         the gateway must not stamp a second one. Got:\n{output}"
    );
}
