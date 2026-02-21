use jrpxy_backend::{BackendError, BackendReader, BackendWriter};
use jrpxy_frontend::{FrontendError, FrontendReader, FrontendRequest, FrontendWriter};
use jrpxy_http_message::message::ResponseBuilder;
use jrpxy_proxy::{Proxy, ProxyError, TryProxyResult};

use std::{io, net::SocketAddr};

use tokio::{
    io::{BufReader, BufWriter},
    net::{
        TcpListener, TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    runtime::Handle,
};

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Frontend error: {0}")]
    FrontendError(#[from] FrontendError),
    #[error("Backend error: {0}")]
    BackendError(#[from] BackendError),

    #[error("Proxy error: {0}")]
    ProxyError(#[from] ProxyError),
}

pub type SessionResult<T> = Result<T, SessionError>;

type SessionReader = BufReader<OwnedReadHalf>;
type SessionWriter = BufWriter<OwnedWriteHalf>;

struct Session {
    state: Option<(FrontendReader<SessionReader>, FrontendWriter<SessionWriter>)>,
    _remote: SocketAddr,
}

async fn respond_with(
    spawner: &Handle,
    frontend_req: FrontendRequest<SessionReader>,
    frontend_writer: FrontendWriter<SessionWriter>,
    code: u16,
    reason: &str,
    message: &str,
) -> SessionResult<(FrontendReader<SessionReader>, FrontendWriter<SessionWriter>)> {
    let mut b = ResponseBuilder::new(8);
    let (req, frontend_body_reader) = frontend_req.into_parts();
    let res = b
        .with_code(code)
        .with_reason(reason)
        .with_version(req.version())
        .with_header("content-type", b"text-plain")
        .build();

    // TODO: add support for draining a maximum amount
    let frontend_reader_drain = spawner.spawn(frontend_body_reader.drain());

    let message = message.as_bytes();
    let mut frontend_body_writer = frontend_writer
        .send_as_content_length(&res, message.len() as u64)
        .await?;
    frontend_body_writer.write(message).await?;

    let frontend_writer = frontend_body_writer.finish().await?;
    // TODO: don't expect here
    let frontend_reader = frontend_reader_drain.await.expect("task panic")?;

    Ok((frontend_reader, frontend_writer))
}

async fn proxy(
    spawner: &Handle,
    frontend_req: FrontendRequest<SessionReader>,
    frontend_writer: FrontendWriter<SessionWriter>,
) -> SessionResult<(FrontendReader<SessionReader>, FrontendWriter<SessionWriter>)> {
    // make sure the request contains a host header
    let Some(_host) = frontend_req.req().get_header("host") else {
        return respond_with(
            spawner,
            frontend_req,
            frontend_writer,
            400,
            "Bad Request",
            "missing host header",
        )
        .await;
    };

    let backend_stream = tokio::net::TcpStream::connect("127.0.0.1:9002")
        .await
        .expect("can connect");
    let (backend_rx, backend_tx) = backend_stream.into_split();
    let backend_writer = BackendWriter::new(BufWriter::with_capacity(4096, backend_tx));
    let backend_reader = BackendReader::new(BufReader::with_capacity(4096, backend_rx), 8192);

    let p = Proxy::new(
        frontend_req,
        frontend_writer,
        backend_reader,
        backend_writer,
    );

    let (frontend_reader, frontend_writer, backend_reader, backend_writer) =
        match p.try_proxy().await {
            TryProxyResult::Error(proxy_error) => {
                return Err(proxy_error.into());
            }
            TryProxyResult::InitialBackendWriteError {
                frontend_request: _,
                frontend_writer: _,
                error,
            } => {
                eprintln!("Initial backend write failed: {error:?}");
                return Err(error.into());
            }
            TryProxyResult::Complete {
                frontend_reader,
                frontend_writer,
                backend_reader,
                backend_writer,
            } => (
                frontend_reader,
                frontend_writer,
                backend_reader,
                backend_writer,
            ),
            TryProxyResult::ResponseCompleteWithUnexpectedClientError(proxy_error) => {
                eprintln!("Response complete, but request incomplete: {proxy_error:?}");
                return Err(proxy_error.into());
            }
        };

    let _ = backend_reader;
    let _ = backend_writer;

    Ok((frontend_reader, frontend_writer))
}

async fn once(
    spawner: &Handle,
    rx: FrontendReader<SessionReader>,
    tx: FrontendWriter<SessionWriter>,
) -> SessionResult<(FrontendReader<SessionReader>, FrontendWriter<SessionWriter>)> {
    let req = rx.read().await?;
    let (rx, tx) = proxy(spawner, req, tx).await?;

    Ok((rx, tx))
}

impl Session {
    fn new(client: TcpStream, remote: SocketAddr) -> Self {
        let (client_reader, client_writer) = client.into_split();
        let client_reader = BufReader::with_capacity(4096, client_reader);
        let client_writer = BufWriter::with_capacity(4096, client_writer);
        Self {
            state: Some((
                FrontendReader::new(client_reader, 8192),
                FrontendWriter::new(client_writer),
            )),
            _remote: remote,
        }
    }

    async fn serve(&mut self, spawner: Handle) -> Result<(), SessionError> {
        while let Some((rx, tx)) = self.state.take() {
            let (rx, tx) = once(&spawner, rx, tx).await?;
            self.state = Some((rx, tx))
        }

        Ok(())
    }
}

async fn server(spawner: Handle) -> io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:9001").await?;

    loop {
        let (stream, remote) = listener.accept().await?;
        let mut sess = Session::new(stream, remote);
        let _jh = spawner.spawn(async move {
            if let Err(e) = sess.serve(tokio::runtime::Handle::current()).await
                && !matches!(e, SessionError::FrontendError(FrontendError::FirstReadEOF))
            {
                dbg!(e);
            }
        });
    }
}

fn main() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("can build runtime");

    let () = rt.block_on(server(rt.handle().clone())).unwrap();
}
