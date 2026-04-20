use std::{
    future::poll_fn,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    backend_writer::{ProxyBackendBodyWriter, ProxyBackendWriter},
    body_forwarder::ProxyBodyForwarder,
    error::ProxyCopyError,
};
use jrpxy_frontend::reader::FrontendBodyReader;

/// Forwards the frontend request body to the backend incrementally. Can be
/// polled independently while waiting for the backend to respond, then
/// continued alongside a response forwarder once the response arrives.
///
/// When both sides use chunked transfer encoding the original chunk boundaries
/// are preserved: each incoming chunk is forwarded with its declared size
/// intact, avoiding unnecessary rebuffering or resizing.
///
/// Construct via [`ProxyRequestBodyForwarder::new`], then drive with
/// [`ProxyRequestBodyForwarder::poll_forward`] or the async
/// [`ProxyRequestBodyForwarder::forward`] wrapper.
pub struct ProxyRequestBodyForwarder<FR, BW> {
    inner: ProxyBodyForwarder<FR, BW>,
}

impl<FR, BW> ProxyRequestBodyForwarder<FR, BW> {
    /// Create a new forwarder from a frontend body reader and a proxy backend
    /// body writer.
    ///
    /// When both the reader and writer use chunked transfer encoding the
    /// forwarder operates in chunk-preserving mode, keeping the original chunk
    /// boundaries intact. All other framing combinations use the generic
    /// buffered path.
    pub fn new(
        reader: FrontendBodyReader<FR>,
        body_writer: ProxyBackendBodyWriter<BW>,
        chunk_size: usize,
    ) -> Result<Self, ProxyCopyError> {
        ProxyBodyForwarder::new(reader, body_writer, chunk_size).map(|inner| Self { inner })
    }
}

impl<FR, BW> ProxyRequestBodyForwarder<FR, BW>
where
    FR: AsyncRead + Unpin,
    BW: AsyncWrite + Unpin,
{
    /// Poll the forwarder, copying bytes from the frontend reader to the
    /// backend writer. Returns `Ready(Ok(()))` when the body has been fully
    /// forwarded; call [`finish`](Self::finish) after that to recover the
    /// backend writer.
    pub fn poll_forward(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), ProxyCopyError>> {
        self.inner.poll_forward(cx)
    }

    /// Recover the backend writer after [`poll_forward`](Self::poll_forward)
    /// has returned `Ready(Ok(()))`. Panics if called before forwarding
    /// completes.
    pub fn finish(self) -> ProxyBackendWriter<BW> {
        self.inner.finish()
    }

    /// Drive the forwarder to completion, returning the recovered
    /// [`ProxyBackendWriter`] when done.
    pub async fn forward(mut self) -> Result<ProxyBackendWriter<BW>, ProxyCopyError> {
        poll_fn(|cx| self.poll_forward(cx)).await?;
        Ok(self.finish())
    }
}
