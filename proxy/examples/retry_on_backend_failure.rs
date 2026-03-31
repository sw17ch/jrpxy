//! Demonstrates request retry against a secondary backend.
//!
//! Two backends are run in the same process:
//!   - PRIMARY  (127.0.0.1:3001): accepts connections then immediately drops
//!     them, causing [`write_backend_request`] to fail with
//!     [`BackendRequestError`].
//!   - SECONDARY (127.0.0.1:3002): a healthy backend that replies with 200 OK.
//!
//! The proxy tries the primary first. When it gets a [`BackendRequestError`] it
//! calls [`retry_backend_request`] with a connection to the secondary. If both
//! backends fail, a 502 Bad Gateway is sent to the client using the
//! [`PendingFrontendResponse`] preserved inside the error.
//!
//! # Usage
//!
//! ```text
//! cargo run --example retry_on_backend_failure
//! # then in another terminal:
//! curl -v http://127.0.0.1:3000/
//! ```

use jrpxy_backend::{reader::BackendReader, writer::BackendWriter};
use jrpxy_frontend::{reader::FrontendReader, writer::FrontendWriter};
use jrpxy_http_message::{message::ResponseBuilder, version::HttpVersion};
use jrpxy_proxy::{PendingFrontendResponse, ProxyClient, ProxyOptions, ReceivedResponseStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PROXY_ADDR: &str = "127.0.0.1:3000";
const PRIMARY_ADDR: &str = "127.0.0.1:3001";
const SECONDARY_ADDR: &str = "127.0.0.1:3002";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Primary: accepts every connection then immediately drops the socket.
    // This makes write_backend_request fail with a broken-pipe error.
    tokio::spawn(async {
        let listener = TcpListener::bind(PRIMARY_ADDR).await.unwrap();
        println!("Primary  (broken)  listening on {PRIMARY_ADDR}");
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            drop(socket);
        }
    });

    // Secondary: healthy — drains the request head and returns 200 OK.
    tokio::spawn(async {
        let listener = TcpListener::bind(SECONDARY_ADDR).await.unwrap();
        println!("Secondary (healthy) listening on {SECONDARY_ADDR}");
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            tokio::spawn(handle_healthy_backend(socket));
        }
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let listener = TcpListener::bind(PROXY_ADDR).await?;
    println!("Proxy    listening on {PROXY_ADDR}");
    println!("Run:  curl -v http://{PROXY_ADDR}/\n");

    loop {
        let (socket, peer) = listener.accept().await?;
        println!("[proxy] connection from {peer}");
        tokio::spawn(handle_client(socket));
    }
}

async fn handle_client(mut socket: TcpStream) {
    // step 1: read the incoming request 
    let (reader, writer) = socket.split();
    let client = ProxyClient::new(
        FrontendReader::new(reader, 16384),
        FrontendWriter::new(writer),
        ProxyOptions::default(),
    );

    let received = match client.start().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[proxy] failed to read request: {e}");
            return;
        }
    };

    // step 2: try the primary backend 
    let primary = match TcpStream::connect(PRIMARY_ADDR).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[proxy] cannot connect to primary: {e}");
            return;
        }
    };
    let (br, bw) = primary.into_split();

    let forwarded = match received
        .write_backend_request((BackendReader::new(br), BackendWriter::new(bw)))
        .await
    {
        Ok(f) => f,

        Err(req_err) => {
            // The write to the primary failed. The request and the frontend
            // connection are preserved inside `req_err`; we can retry or give up.
            println!(
                "[proxy] primary backend failed ({}), retrying with secondary",
                req_err.error()
            );

            // Connect to the secondary backend.
            let secondary = match TcpStream::connect(SECONDARY_ADDR).await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[proxy] cannot connect to secondary: {e}");
                    // Both backends are unreachable. Extract the preserved
                    // frontend state and send 502.
                    let (pending, _frontend_body) = req_err.into_pending_response();
                    send_502(pending).await;
                    return;
                }
            };
            let (br2, bw2) = secondary.into_split();

            // retry_backend_request re-sends the already-prepared request
            // (hop-by-hop headers stripped, Via injected) to the new backend.
            match req_err
                .retry_backend_request((BackendReader::new(br2), BackendWriter::new(bw2)))
                .await
            {
                Ok(f) => {
                    println!("[proxy] secondary backend accepted the request");
                    f
                }
                Err(req_err2) => {
                    eprintln!(
                        "[proxy] secondary also failed ({}), sending 502",
                        req_err2.error()
                    );
                    let (pending, _frontend_body) = req_err2.into_pending_response();
                    send_502(pending).await;
                    return;
                }
            }
        }
    };

    // step 3: read and forward the response 
    let mut stream = match forwarded.read_backend_response().await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[proxy] failed to read backend response: {e:?}");
            return;
        }
    };
    let response = loop {
        match stream {
            ReceivedResponseStream::Response(r) => break r,
            ReceivedResponseStream::Informational(info) => {
                stream = match info.forward_informational_response().await {
                    Ok(result) => result.into_inner(),
                    Err(e) => {
                        eprintln!("[proxy] informational error: {e:?}");
                        return;
                    }
                };
            }
        }
    };
    if let Err(e) = response.forward_response().await {
        eprintln!("[proxy] failed to forward response: {e}");
    }
}

/// Send a 502 Bad Gateway to the client. `pending` is the preserved frontend
/// connection extracted from a [`BackendRequestError`] via
/// [`into_pending_response`].
async fn send_502<FW: AsyncWriteExt + Unpin>(pending: PendingFrontendResponse<FW>) {
    let body = b"Bad Gateway";

    let mut b = ResponseBuilder::new(0);
    let response = b
        .with_version(HttpVersion::Http11)
        .with_code(502)
        .with_reason("Bad Gateway")
        .build()
        .expect("valid static 502 response");

    let mut body_writer = match pending
        .send_as_content_length(response, body.len() as u64)
        .await
    {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[proxy] failed to send 502 head: {e}");
            return;
        }
    };

    if let Err(e) = body_writer.write(body).await {
        eprintln!("[proxy] failed to write 502 body: {e}");
        return;
    }
    let _ = body_writer.finish().await;
}

/// Minimal HTTP/1.1 backend: reads until the blank line that ends the request
/// head, then writes a fixed 200 OK response.
async fn handle_healthy_backend(socket: TcpStream) {
    let (mut reader, mut writer) = socket.into_split();

    let mut buf = vec![0u8; 4096];
    let mut filled = 0;
    loop {
        match reader.read(&mut buf[filled..]).await {
            Ok(0) | Err(_) => return,
            Ok(n) => filled += n,
        }
        if buf[..filled].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }

    let _ = writer
        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 22\r\n\r\nfrom secondary backend")
        .await;
}
