use std::task::Poll;

use jrpxy_backend::{reader::BackendReader, writer::BackendWriter};
use tokio::io::{AsyncReadExt, AsyncWrite};

type BackendConnection<BR, BW> = (BackendReader<BR>, BackendWriter<BW>);

#[derive(thiserror::Error, Debug)]
pub enum PoolError {
    #[error("No available backend connection")]
    NoBackendConnection,
}

pub type PoolResult<T> = Result<T, PoolError>;

pub trait BackendProxyProvider {
    /// The backend reader inner type
    type BR;
    /// The backend writer inner type
    type BW;

    /// Get a connection from the provider. If a clean, established connection
    /// is not available, one can be created, or an error is returned.
    fn get_connection(
        &mut self,
    ) -> impl Future<Output = PoolResult<BackendConnection<Self::BR, Self::BW>>>;

    /// Return a connection to the provider. The connection *must* be clean.
    /// That is, the connection must be fully drained, and should remain idle
    /// until another request is sent.
    fn give_connection(&mut self, reader: BackendReader<Self::BR>, writer: BackendWriter<Self::BW>);
}

/// Holds a single backend connection. Mostly useful for testing. Does not open
/// new connections if empty.
pub struct OneshotBackend<BR, BW> {
    pub inner: Option<(BackendReader<BR>, BackendWriter<BW>)>,
}

pin_project_lite::pin_project! {
    pub struct OneshotConnector<BR, BW> {
        #[pin]
        inner: Option<(BR, BW)>,
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
                BackendReader::new(backend_reader, 256),
                BackendWriter::new(backend_writer),
            )),
        }
    }
}

impl<BR: Unpin, BW: Unpin> Future for OneshotConnector<BR, BW> {
    type Output = PoolResult<(BR, BW)>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<Self::Output> {
        let proj = self.project();
        let mut inner = proj.inner;
        if let Some(bp) = inner.take() {
            Poll::Ready(Ok(bp))
        } else {
            Poll::Ready(Err(PoolError::NoBackendConnection))
        }
    }
}

impl<BR: Unpin, BW: Unpin> BackendProxyProvider for OneshotBackend<BR, BW> {
    type BR = BR;
    type BW = BW;

    fn get_connection(
        &mut self,
    ) -> impl Future<Output = PoolResult<BackendConnection<Self::BR, Self::BW>>> {
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
