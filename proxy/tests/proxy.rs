use jrpxy_backend::{reader::BackendReader, writer::BackendWriter};
use jrpxy_frontend::{reader::FrontendReader, writer::FrontendWriter};
use jrpxy_http_message::{message::ResponseBuilder, version::HttpVersion};

use jrpxy_proxy::{
    BackendProxyRequest, BackendProxyResponseStream, FrontendProxyRequest,
    InformationalProxyResponseForwardResult, ProxyFrontendError, ProxyOptions,
};

async fn make_frontend_proxy_request(raw: &[u8]) -> FrontendProxyRequest<&[u8], tokio::io::Sink> {
    let reader = FrontendReader::new(raw, 256);
    let req = reader.read(8192, 8192).await.expect("valid request");
    FrontendProxyRequest::from_request(
        req,
        FrontendWriter::new(tokio::io::sink()),
        ProxyOptions::default(),
    )
    .expect("can build proxy request")
}

async fn make_backend_proxy_request(raw: &[u8]) -> BackendProxyRequest<&[u8], tokio::io::Sink> {
    make_frontend_proxy_request(raw)
        .await
        .into_backend_request()
}

/// Drive one full proxy transaction over a pair of duplex pipes: read the
/// frontend request, forward it to the backend, relay the final response, and
/// drain both body directions to completion. Panics on any proxy-layer error.
///
/// The streaming tests below care about *when* bytes move between a mock client
/// and a mock origin, not about this bookkeeping; hoisting it here lets each of
/// them read as just its client and origin behavior.
async fn run_proxy_transaction(
    frontend_server: tokio::io::DuplexStream,
    backend_client: tokio::io::DuplexStream,
) {
    let (frontend_server_r, frontend_server_w) = tokio::io::split(frontend_server);
    let (backend_client_r, backend_client_w) = tokio::io::split(backend_client);

    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_server_r, 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");
    let backend_connection = (
        BackendReader::new(backend_client_r, 256),
        BackendWriter::new(backend_client_w),
    );

    let response = FrontendProxyRequest::from_request(
        fe_request,
        FrontendWriter::new(frontend_server_w),
        fe_options,
    )
    .expect("client start failed")
    .into_backend_request()
    .forward(backend_connection)
    .await
    .expect("backend write failed")
    .read_backend_response()
    .await
    .expect("failed to read backend response");

    let final_response = match response {
        BackendProxyResponseStream::Final(rbr) => rbr,
        BackendProxyResponseStream::Informational(_) => panic!("unexpected informational response"),
    };

    final_response
        .forward_response_head()
        .await
        .expect("forward_response_head failed")
        .finish()
        .await
        .expect("failed to finish");
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

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let did_fe_read = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed");

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
        BackendProxyResponseStream::Final(_read_backend_response) => {
            panic!("incorrect response type")
        }
        BackendProxyResponseStream::Informational(read_backend_informational_response) => {
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
        BackendProxyResponseStream::Final(rbr) => rbr,
        BackendProxyResponseStream::Informational(_rbir) => {
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

    let (_frontend_reader, frontend_writer) = client;
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

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let did_fe_read = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed");

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
        BackendProxyResponseStream::Final(_) => panic!("incorrect response type"),
        BackendProxyResponseStream::Informational(rbir) => rbir,
    };

    let ifr = rbir
        .forward_informational_response()
        .await
        .expect("failed to forward");
    let be_res_stat = match ifr {
        crate::InformationalProxyResponseForwardResult::Dropped(r) => r,
        crate::InformationalProxyResponseForwardResult::Forwarded(_r) => {
            panic!("forwarded response when we expected a drop")
        }
    };

    let rbr = match be_res_stat {
        BackendProxyResponseStream::Final(rbr) => rbr,
        BackendProxyResponseStream::Informational(_) => panic!("incorrect response type"),
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

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");
    let backend_connection = (
        BackendReader::new(backend_reader.as_ref(), 256),
        BackendWriter::new(&mut backend_writer),
    );
    let be_res_stat = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed")
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("backend write failed")
        .read_backend_response()
        .await
        .expect("failed to read backend response");

    let rbr = match be_res_stat {
        BackendProxyResponseStream::Final(rbr) => rbr,
        BackendProxyResponseStream::Informational(_) => panic!("unexpected informational"),
    };

    let (client, _backend_connection) = rbr
        .forward_response_head()
        .await
        .expect("forward_response_head failed")
        .finish()
        .await
        .expect("failed to forward response");
    let client = client.expect("failed to recycle client");

    let (_frontend_reader, frontend_writer) = client;
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

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");
    let backend_connection = (
        BackendReader::new(backend_reader.as_ref(), 256),
        BackendWriter::new(&mut backend_writer),
    );

    let be_res_stat = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed")
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("backend write failed")
        .read_backend_response()
        .await
        .expect("failed to read backend response");

    let rbir = match be_res_stat {
        BackendProxyResponseStream::Informational(rbir) => rbir,
        BackendProxyResponseStream::Final(_) => panic!("expected informational"),
    };

    let be_res_stat = rbir
        .forward_informational_response()
        .await
        .expect("failed to forward informational")
        .into_inner();

    let rbr = match be_res_stat {
        BackendProxyResponseStream::Final(rbr) => rbr,
        BackendProxyResponseStream::Informational(_) => panic!("expected final response"),
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

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");
    let backend_connection = (
        BackendReader::new(backend_reader.as_ref(), 256),
        BackendWriter::new(&mut backend_writer),
    );

    let be_res_stat = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed")
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("backend write failed")
        .read_backend_response()
        .await
        .expect("failed to read backend response");

    let rbr = match be_res_stat {
        BackendProxyResponseStream::Final(rbr) => rbr,
        BackendProxyResponseStream::Informational(_) => panic!("unexpected informational"),
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

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");
    let backend_connection = (
        BackendReader::new(backend_reader.as_ref(), 256),
        BackendWriter::new(&mut backend_writer),
    );
    let be_res_stat = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed")
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("backend write failed")
        .read_backend_response()
        .await
        .expect("failed to read backend response");

    let rbr = match be_res_stat {
        BackendProxyResponseStream::Final(rbr) => rbr,
        BackendProxyResponseStream::Informational(_) => panic!("unexpected informational"),
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

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");
    let backend_connection = (
        BackendReader::new(backend_reader.as_ref(), 256),
        BackendWriter::new(&mut backend_writer),
    );
    let be_res_stat = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed")
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("backend write failed")
        .read_backend_response()
        .await
        .expect("failed to read backend response");

    let rbr = match be_res_stat {
        BackendProxyResponseStream::Final(rbr) => rbr,
        BackendProxyResponseStream::Informational(_) => panic!("unexpected informational"),
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

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");
    let backend_connection = (
        BackendReader::new(backend_reader.as_ref(), 256),
        BackendWriter::new(&mut backend_writer),
    );
    let be_res_stat = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed")
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("backend write failed")
        .read_backend_response()
        .await
        .expect("failed to read backend response");

    let rbr = match be_res_stat {
        BackendProxyResponseStream::Final(rbr) => rbr,
        BackendProxyResponseStream::Informational(_) => panic!("unexpected informational"),
    };

    let (client, _backend_connection) = rbr
        .forward_response_head()
        .await
        .expect("forward_response_head failed")
        .finish()
        .await
        .expect("failed to forward response");
    let client = client.expect("failed to recycle client");

    let (_frontend_reader, frontend_writer) = client;
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

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let be_res_stat = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed")
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("backend write failed")
        .read_backend_response()
        .await
        .expect("failed to read backend response");

    let rbr = match be_res_stat {
        BackendProxyResponseStream::Final(rbr) => rbr,
        BackendProxyResponseStream::Informational(_) => panic!("unexpected informational"),
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

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let be_res_stat = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed")
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("backend write failed")
        .read_backend_response()
        .await
        .expect("failed to read backend response");

    let rbr = match be_res_stat {
        BackendProxyResponseStream::Final(rbr) => rbr,
        BackendProxyResponseStream::Informational(_) => panic!("unexpected informational"),
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
async fn eof_from_backend_reframed_as_chunked_for_http11_client() {
    // An HTTP/1.0 backend delimits its body by closing the connection (no
    // content-length, no transfer-encoding). For an HTTP/1.1 client the
    // proxy must re-frame that EOF body as chunked so the frontend
    // connection can stay open and be reused. The recycling shape proves
    // the asymmetry: the chunked-reframed frontend is reusable (Some) while
    // the EOF-delimited backend connection is consumed by close (None).
    let frontend_reader = b"\
            GET / HTTP/1.1\r\n\
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

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let be_res_stat = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed")
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("backend write failed")
        .read_backend_response()
        .await
        .expect("failed to read backend response");

    let rbr = match be_res_stat {
        BackendProxyResponseStream::Final(rbr) => rbr,
        BackendProxyResponseStream::Informational(_) => panic!("unexpected informational"),
    };

    let (client, backend_connection) = rbr
        .forward_response_head()
        .await
        .expect("forward_response_head failed")
        .finish()
        .await
        .expect("failed to forward response");

    // The frontend connection survives because the body was re-framed as
    // chunked; the EOF-delimited backend connection cannot be reused.
    let client = client.expect("expected frontend client to be recyclable");
    assert!(
        backend_connection.is_none(),
        "expected backend to be None for EOF-framed response"
    );

    let (_frontend_reader, frontend_writer) = client;
    let frontend_writer = frontend_writer.into_inner();

    let expected_frontend_writer = b"\
            HTTP/1.1 200 Ok\r\n\
            Date: Mon, 01 Jan 2026 00:00:00 GMT\r\n\
            Via: 1.0 jrpxy\r\n\
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

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");
    let backend_connection = (
        BackendReader::new(backend_reader.as_ref(), 256),
        BackendWriter::new(&mut backend_writer),
    );
    let be_res_stat = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed")
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("backend write failed")
        .read_backend_response()
        .await
        .expect("failed to read backend response");

    let rbr = match be_res_stat {
        BackendProxyResponseStream::Final(rbr) => rbr,
        BackendProxyResponseStream::Informational(_) => panic!("unexpected informational"),
    };

    let (client, _backend_connection) = rbr
        .forward_response_head()
        .await
        .expect("forward_response_head failed")
        .finish()
        .await
        .expect("failed to forward response");
    let client = client.expect("failed to recycle client");

    let (_frontend_reader, frontend_writer) = client;
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

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");
    let backend_connection = (
        BackendReader::new(backend_reader.as_ref(), 256),
        BackendWriter::new(&mut backend_writer),
    );
    let be_res_stat = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed")
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("backend write failed")
        .read_backend_response()
        .await
        .expect("failed to read backend response");

    let rbr = match be_res_stat {
        BackendProxyResponseStream::Final(rbr) => rbr,
        BackendProxyResponseStream::Informational(_) => panic!("unexpected informational"),
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
    let (frontend_reader, _) = client;

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
    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = frontend_reader
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let be_res_stat = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("second start failed")
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("second backend write failed")
        .read_backend_response()
        .await
        .expect("second read failed");

    let rbr = match be_res_stat {
        BackendProxyResponseStream::Final(rbr) => rbr,
        BackendProxyResponseStream::Informational(_) => panic!("unexpected informational"),
    };

    let (client, _) = rbr
        .forward_response_head()
        .await
        .expect("forward_response_head failed")
        .finish()
        .await
        .expect("second forward failed");
    let client = client.expect("failed to recycle client");

    let (_, fw) = client;
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

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");
    let backend_connection = (
        BackendReader::new(backend_reader.as_ref(), 256),
        BackendWriter::new(&mut backend_writer),
    );
    let be_res_stat = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed")
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("backend write failed")
        .read_backend_response()
        .await
        .expect("failed to read backend response");

    let rbr = match be_res_stat {
        BackendProxyResponseStream::Final(rbr) => rbr,
        BackendProxyResponseStream::Informational(_) => panic!("unexpected informational"),
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

    let (frontend_reader, frontend_writer) = client;
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

    // now run another request; this time we expect a response body. We
    // make a new frontend writer so we can distinguish the previous
    // response from the next response. The backend connection is reused
    // directly from the first cycle's return value.
    let mut frontend_writer = Vec::new();
    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = frontend_reader
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let be_res_stat = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
        .expect("client start failed")
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("backend write failed")
        .read_backend_response()
        .await
        .expect("failed to read backend response");

    let rbr = match be_res_stat {
        BackendProxyResponseStream::Final(rbr) => rbr,
        BackendProxyResponseStream::Informational(_) => panic!("unexpected informational"),
    };

    let (client, _backend_connection) = rbr
        .forward_response_head()
        .await
        .expect("forward_response_head failed")
        .finish()
        .await
        .expect("failed to forward response");
    let client = client.expect("failed to recycle client");

    let (_frontend_reader, frontend_writer) = client;
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

    let req = make_backend_proxy_request(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n").await;
    let backend_connection = (
        BackendReader::new(raw.as_slice(), 256),
        BackendWriter::new(tokio::io::sink()),
    );
    let stream = req
        .forward(backend_connection)
        .await
        .expect("backend write failed")
        .read_backend_response()
        .await
        .expect("failed to read backend response");
    let forwarder = match stream {
        BackendProxyResponseStream::Final(f) => f,
        BackendProxyResponseStream::Informational(_) => panic!("unexpected informational"),
    };
    let pr = forwarder.as_response();

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

    let req = make_backend_proxy_request(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n").await;
    let backend_connection = (
        BackendReader::new(raw.as_slice(), 256),
        BackendWriter::new(tokio::io::sink()),
    );
    let stream = req
        .forward(backend_connection)
        .await
        .expect("backend write failed")
        .read_backend_response()
        .await
        .expect("failed to read backend response");
    let forwarder = match stream {
        BackendProxyResponseStream::Final(f) => f,
        BackendProxyResponseStream::Informational(_) => panic!("unexpected informational"),
    };
    let pr = forwarder.as_response();

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
async fn informational_response_hop_by_hop() {
    let raw_backend = b"\
            HTTP/1.1 100 Continue\r\n\
            Connection: x-interim-hop\r\n\
            X-Interim-Hop: val\r\n\
            X-Hint: keep\r\n\
            \r\n\
            HTTP/1.1 200 Ok\r\n\
            Content-Length: 0\r\n\
            \r\n";

    let mut frontend_writer = Vec::new();
    let fe_options = ProxyOptions::builder()
        .received_by("proxy.example.com")
        .build()
        .expect("valid pseudonym");
    let fe_request = FrontendReader::new(
        b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n".as_slice(),
        256,
    )
    .read(8192, fe_options.max_chunk_header_length())
    .await
    .expect("frontend read failed");
    let did_fe_read = FrontendProxyRequest::from_request(
        fe_request,
        FrontendWriter::new(&mut frontend_writer),
        fe_options,
    )
    .expect("client start failed");

    let backend_connection = (
        BackendReader::new(raw_backend.as_slice(), 256),
        BackendWriter::new(tokio::io::sink()),
    );
    let stream = did_fe_read
        .into_backend_request()
        .forward(backend_connection)
        .await
        .expect("backend write failed")
        .read_backend_response()
        .await
        .expect("failed to read backend response");

    let info = match stream {
        BackendProxyResponseStream::Informational(info) => info,
        BackendProxyResponseStream::Final(_) => panic!("expected informational"),
    };

    let info_names: Vec<_> = info
        .as_response()
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
        .as_response()
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

    // Forwarding the 1xx writes it to the frontend with a Via header added.
    // Recover the final response first, then drop it to release the borrow on
    // frontend_writer before inspecting the bytes that were written.
    let final_stream = info
        .forward_informational_response()
        .await
        .expect("failed to forward informational response")
        .into_inner();
    let final_res = match final_stream {
        BackendProxyResponseStream::Final(f) => f,
        BackendProxyResponseStream::Informational(_) => panic!("expected final response"),
    };
    assert_eq!(final_res.as_response().res().code(), 200);
    drop(final_res);

    let output = String::from_utf8(frontend_writer).unwrap();
    assert!(
        output.contains("Via: 1.1 proxy.example.com"),
        "expected Via header in forwarded 1xx: {output:?}"
    );
}

/// CONNECT is not supported - test frontend request handling errors
#[tokio::test]
async fn connect_method_not_supported() {
    let frontend_reader = b"\
            CONNECT example.com:443 HTTP/1.1\r\n\
            Host: example.com:443\r\n\
            \r\n";
    let mut frontend_writer = Vec::new();

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let req_err = match FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options) {
        Ok(_) => panic!("expected error for CONNECT, got Ok"),
        Err(e) => e,
    };

    assert!(matches!(
        req_err.error(),
        ProxyFrontendError::ConnectNotSupported
    ));

    let pending = req_err.into_pending_response();

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

    let fe_writer = FrontendWriter::new(&mut frontend_writer);
    let fe_options = ProxyOptions::default();
    let fe_request = FrontendReader::new(frontend_reader.as_ref(), 256)
        .read(8192, fe_options.max_chunk_header_length())
        .await
        .expect("frontend read failed");

    let req_err = match FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options) {
        Ok(_) => panic!("expected error for TRACE, got Ok"),
        Err(e) => e,
    };

    assert!(matches!(
        req_err.error(),
        ProxyFrontendError::TraceNotSupported
    ));

    let pending = req_err.into_pending_response();

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

    let req = make_backend_proxy_request(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n").await;
    let backend_connection = (
        BackendReader::new(raw.as_slice(), 256),
        BackendWriter::new(tokio::io::sink()),
    );
    let stream = req
        .forward(backend_connection)
        .await
        .expect("backend write failed")
        .read_backend_response()
        .await
        .expect("failed to read backend response");

    let info1 = match stream {
        BackendProxyResponseStream::Informational(info) => info,
        BackendProxyResponseStream::Final(_) => panic!("expected informational"),
    };
    assert_eq!(info1.as_response().res().code(), 100);

    let stream = info1
        .forward_informational_response()
        .await
        .expect("failed to forward first informational")
        .into_inner();
    let info2 = match stream {
        BackendProxyResponseStream::Informational(info) => info,
        BackendProxyResponseStream::Final(_) => panic!("expected second informational"),
    };
    assert_eq!(info2.as_response().res().code(), 103);

    let stream = info2
        .forward_informational_response()
        .await
        .expect("failed to forward second informational")
        .into_inner();
    let final_res = match stream {
        BackendProxyResponseStream::Final(f) => f,
        BackendProxyResponseStream::Informational(_) => panic!("expected final response"),
    };
    assert_eq!(final_res.as_response().res().code(), 200);
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

    let proxy_task = tokio::spawn(run_proxy_transaction(frontend_server, backend_client));

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

/// A backend that sends its complete response *before* consuming the request
/// body. The early response must not truncate the upload: the proxy must keep
/// pumping the request body through to the backend to completion. This
/// exercises the unconditional both-directions drain in
/// `ProxyBodyExchanger::finish` - abandoning the request copy once the response
/// finished would leave the backend short of the body it is still reading.
#[tokio::test]
async fn proxy_drains_request_body_even_when_response_arrives_early() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Larger than the duplex pipe buffers below so the body cannot be flushed
    // in a single step: draining it must span several reactor turns, which is
    // what makes this a real guard rather than something an in-memory copy
    // would complete synchronously regardless of the drain policy.
    const BODY_LEN: usize = 64 * 1024;

    let (frontend_client, frontend_server) = tokio::io::duplex(4096);
    let (backend_client, backend_server) = tokio::io::duplex(4096);

    // Client: stream the head and a large body, then read the response back.
    let client_task = tokio::spawn(async move {
        let (mut fc_r, mut fc_w) = tokio::io::split(frontend_client);

        fc_w.write_all(
            b"POST / HTTP/1.1\r\n\
                  Host: example.com\r\n\
                  Content-Length: 65536\r\n\
                  \r\n",
        )
        .await
        .unwrap();
        fc_w.write_all(&vec![b'x'; BODY_LEN]).await.unwrap();
        fc_w.flush().await.unwrap();

        // Drain the response so the proxy's write-back path never stalls, and
        // confirm the early response reached the client.
        let mut sink = Vec::new();
        fc_r.read_to_end(&mut sink).await.unwrap();
        assert!(
            sink.starts_with(b"HTTP/1.1 200"),
            "client did not observe the early response"
        );
    });

    // Backend origin: respond immediately after the head, *then* read the body
    // the proxy is expected to keep pumping. Returns the number of body bytes
    // actually received.
    let backend_task = tokio::spawn(async move {
        let (backend_server_r, mut backend_server_w) = tokio::io::split(backend_server);

        let req = FrontendReader::new(backend_server_r, 256)
            .read(8192, 8192)
            .await
            .expect("backend: failed to read request head");
        let (_, mut body_reader) = req.into_parts();

        backend_server_w
            .write_all(
                b"HTTP/1.1 200 Ok\r\n\
                      content-length: 0\r\n\
                      \r\n",
            )
            .await
            .unwrap();
        backend_server_w.flush().await.unwrap();

        let mut received = 0usize;
        while let Some(chunk) = body_reader.read(8192).await.unwrap() {
            assert!(
                chunk.iter().all(|&b| b == b'x'),
                "request body corrupted in transit"
            );
            received += chunk.len();
        }
        received
    });

    let proxy_task = tokio::spawn(run_proxy_transaction(frontend_server, backend_client));

    let received = tokio::time::timeout(Duration::from_secs(5), async move {
        client_task.await.unwrap();
        proxy_task.await.unwrap();
        backend_task.await.unwrap()
    })
    .await
    .expect("timed out - proxy did not drain the request body after the early response");

    assert_eq!(
        received, BODY_LEN,
        "backend must receive the entire request body despite the early response"
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
/// `BackendInformationalProxyResponseForwarder::forward_informational_response`, this
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

        let fe_writer = FrontendWriter::new(frontend_server_w);
        let fe_options = ProxyOptions::default();
        let fe_request = FrontendReader::new(frontend_server_r, 256)
            .read(8192, fe_options.max_chunk_header_length())
            .await
            .expect("frontend read failed");
        let backend_connection = (
            BackendReader::new(backend_client_r, 256),
            BackendWriter::new(backend_client_w),
        );

        let stream = FrontendProxyRequest::from_request(fe_request, fe_writer, fe_options)
            .expect("client start failed")
            .into_backend_request()
            .forward(backend_connection)
            .await
            .expect("backend write failed")
            .read_backend_response()
            .await
            .expect("failed to read backend response");

        let info = match stream {
            BackendProxyResponseStream::Informational(info) => info,
            BackendProxyResponseStream::Final(_) => panic!("expected 100 Continue first"),
        };

        let next = info
            .forward_informational_response()
            .await
            .expect("forward_informational_response failed");
        let stream = match next {
            InformationalProxyResponseForwardResult::Forwarded(s) => s,
            InformationalProxyResponseForwardResult::Dropped(s) => s,
        };
        let rbr = match stream {
            BackendProxyResponseStream::Final(rbr) => rbr,
            BackendProxyResponseStream::Informational(_) => {
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
