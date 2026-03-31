//! Demonstrates custom request and response header injection using `jrpxy-proxy`.
//!
//! The proxy:
//! - injects `x-request-time` (nanoseconds since the Unix epoch) into every
//!   forwarded request
//! - injects `x-elapsed-nanos` (nanoseconds elapsed between request send and
//!   response receipt) into every forwarded response
//!
//! A minimal HTTP backend is co-located in the same process for convenience.
//!
//! # Usage
//!
//! ```text
//! cargo run --example timing_headers
//! # then in another terminal:
//! curl -v http://127.0.0.1:3000/
//! ```

use std::time::Instant;

use jrpxy_backend::{reader::BackendReader, writer::BackendWriter};
use jrpxy_frontend::{reader::FrontendReader, writer::FrontendWriter};
use jrpxy_proxy::{ProxyClient, ProxyOptions, ReceivedResponseStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PROXY_ADDR: &str = "127.0.0.1:3000";
const BACKEND_ADDR: &str = "127.0.0.1:3001";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Spawn the trivial backend server.
    tokio::spawn(async {
        let listener = TcpListener::bind(BACKEND_ADDR).await.unwrap();
        println!("Backend  listening on {BACKEND_ADDR}");
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            tokio::spawn(handle_backend_connection(socket));
        }
    });

    // Give the backend a moment to bind before the proxy starts accepting.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let listener = TcpListener::bind(PROXY_ADDR).await?;
    println!("Proxy    listening on {PROXY_ADDR}");
    println!("Run:  curl -v http://{PROXY_ADDR}/");

    loop {
        let (socket, peer) = listener.accept().await?;
        println!("[proxy] accepted connection from {peer}");
        tokio::spawn(handle_client_connection(socket));
    }
}

async fn handle_client_connection(mut socket: TcpStream) {
    let t_start = Instant::now();

    let (reader, writer) = socket.split();
    let client = ProxyClient::new(
        FrontendReader::new(reader, 16384),
        FrontendWriter::new(writer),
        ProxyOptions::default(),
    );

    // step 1: read the incoming request
    let received = match client.start().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[proxy] failed to read request: {e}");
            return;
        }
    };
    let t_delta_rx_request = t_start.elapsed().as_secs_f32() * 1000.0;

    // step 2: forward the request to the backend
    let mut backend = match TcpStream::connect(BACKEND_ADDR).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[proxy] failed to connect to backend: {e}");
            return;
        }
    };
    let (br, bw) = backend.split();

    let forwarded = match received
        .write_backend_request((BackendReader::new(br), BackendWriter::new(bw)))
        .await
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[proxy] failed to write backend request: {e:?}");
            return;
        }
    };
    let t_delta_tx_request = t_start.elapsed().as_secs_f32() * 1000.0;

    // step 3: read the backend response
    // Forward any 1xx informational responses along the way, then settle on
    // the final response.
    let mut stream = match forwarded.read_backend_response().await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[proxy] failed to read backend response: {e:?}");
            return;
        }
    };

    let mut rx_times = Vec::new();
    let mut response = loop {
        match stream {
            ReceivedResponseStream::Response(r) => {
                rx_times.push((t_start.elapsed().as_secs_f32() * 1000.0).to_string());
                break r;
            }
            ReceivedResponseStream::Informational(mut info) => {
                rx_times.push((t_start.elapsed().as_secs_f32() * 1000.0).to_string());
                let res = info.as_response_mut().res_mut();
                let delta = t_start.elapsed().as_secs_f32() * 1000.0;
                res.headers_mut()
                    .push("x-time-delta-informational-response", delta.to_string());
                stream = match info.forward_informational_response().await {
                    Ok(result) => result.into_inner(),
                    Err(e) => {
                        eprintln!("[proxy] failed to forward informational response: {e:?}");
                        return;
                    }
                };
            }
        }
    };

    let rx_times = rx_times.join(" ");
    let timing =
        format!("rx-req={t_delta_rx_request}, tx-req={t_delta_tx_request}, rx-times=[{rx_times}]");

    response
        .as_response_mut()
        .res_mut()
        .headers_mut()
        .push("x-timing", timing.clone());

    println!("[proxy] timing={timing}");

    // step 4: stream the response back to the client
    if let Err(e) = response.forward_response().await {
        eprintln!("[proxy] failed to forward response: {e}");
    }
}

/// Minimal HTTP/1.1 server: reads the request head and replies with a fixed
/// body. Exists only to give the proxy a real backend to talk to.
async fn handle_backend_connection(socket: TcpStream) {
    let (mut reader, mut writer) = socket.into_split();

    // Accumulate bytes until we see the blank line that ends the request head.
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
        .write_all(
            b"\
            HTTP/1.1 100 Continue\r\n\
            x-info: 1\r\n\
            \r\n\
            HTTP/1.1 100 Continue\r\n\
            x-info: 2\r\n\
            \r\n\
            HTTP/1.1 200 OK\r\n\
            content-length: 14\r\n\
            \r\n\
            Hello, proxy!\n",
        )
        .await;
}
