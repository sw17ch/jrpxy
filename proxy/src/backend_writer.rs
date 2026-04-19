use jrpxy_backend::error::{BackendError, BackendResult};
use jrpxy_body::{
    error::BodyError,
    writer::{
        bodyless::BodylessBodyWriter,
        chunked::{AbortWriter, DataCompleter, DataWriter, FinalWriter, HeadWriter, IdleWriter},
        content_length::ContentLengthBodyWriter,
    },
};
use jrpxy_http_message::{
    framing::{WriteFraming, is_framing_header},
    message::Request,
};
use std::{
    future::poll_fn,
    task::{Context, Poll},
};
use tokio::io::{self, AsyncWrite, AsyncWriteExt};

/// Proxy-owned backend writer. Mirrors [`jrpxy_backend::writer::BackendWriter`]
/// but produces a [`ProxyBackendWriter`] on completion rather than a
/// [`jrpxy_backend::writer::BackendWriter`], keeping the proxy's type system
/// sealed.
pub struct ProxyBackendWriter<I> {
    writer: I,
}

impl<I: AsyncWriteExt + Unpin> ProxyBackendWriter<I> {
    pub fn new(writer: I) -> Self {
        Self { writer }
    }

    pub async fn send_as_chunked(
        self,
        request: &Request,
    ) -> BackendResult<ProxyBackendBodyWriter<I>> {
        let Self { mut writer } = self;
        write_request_to(request, WriteFraming::Chunked, &mut writer)
            .await
            .map_err(BackendError::WriteError)?;
        Ok(ProxyBackendBodyWriter::TE(Some(ProxyBackendIdleWriter {
            inner: IdleWriter::new(writer),
        })))
    }

    pub async fn send_as_content_length(
        self,
        request: &Request,
        body_len: u64,
    ) -> BackendResult<ProxyBackendBodyWriter<I>> {
        let Self { mut writer } = self;
        write_request_to(request, WriteFraming::Length(body_len), &mut writer)
            .await
            .map_err(BackendError::WriteError)?;
        Ok(ProxyBackendBodyWriter::CL(
            ProxyBackendContentLengthBodyWriter {
                inner: ContentLengthBodyWriter::new(body_len, writer),
            },
        ))
    }

    pub async fn send_as_bodyless(
        self,
        request: &Request,
    ) -> BackendResult<ProxyBackendBodyWriter<I>> {
        let Self { mut writer } = self;
        write_request_to(request, WriteFraming::PreserveFraming, &mut writer)
            .await
            .map_err(BackendError::WriteError)?;
        Ok(ProxyBackendBodyWriter::Bodyless(
            ProxyBackendBodylessBodyWriter {
                inner: BodylessBodyWriter::new(writer),
            },
        ))
    }

    pub fn into_inner(self) -> I {
        self.writer
    }
}

async fn write_request_to<W: AsyncWriteExt + Unpin>(
    req: &Request,
    framing: WriteFraming,
    mut w: W,
) -> io::Result<()> {
    let method = req.method();
    let path = req.path();

    w.write_all(method).await?;
    w.write_all(b" ").await?;
    w.write_all(path).await?;
    w.write_all(b" ").await?;
    w.write_all(b"HTTP/1.1").await?;
    w.write_all(b"\r\n").await?;

    let headers = req.headers().iter().filter(|(n, _)| !is_framing_header(n));

    for (n, v) in headers {
        w.write_all(n).await?;
        w.write_all(b": ").await?;
        w.write_all(v).await?;
        w.write_all(b"\r\n").await?;
    }

    match framing {
        WriteFraming::PreserveFraming | WriteFraming::StripFraming => {}
        WriteFraming::Length(l) => {
            let cl = format!("content-length: {l}\r\n");
            w.write_all(cl.as_bytes()).await?;
        }
        WriteFraming::Chunked => {
            w.write_all(b"transfer-encoding: chunked\r\n").await?;
        }
    }

    w.write_all(b"\r\n").await?;
    w.flush().await?;

    Ok(())
}

/// Proxy-owned wrapper for [`BodylessBodyWriter`].
pub struct ProxyBackendBodylessBodyWriter<I> {
    inner: BodylessBodyWriter<I>,
}

impl<I: AsyncWriteExt + Unpin> ProxyBackendBodylessBodyWriter<I> {
    pub fn finish(self) -> BackendResult<ProxyBackendWriter<I>> {
        let Self { inner } = self;
        Ok(ProxyBackendWriter::new(inner.finish()))
    }
}

/// Proxy-owned wrapper for [`ContentLengthBodyWriter`].
pub struct ProxyBackendContentLengthBodyWriter<I> {
    inner: ContentLengthBodyWriter<I>,
}

impl<I: AsyncWriteExt + Unpin> ProxyBackendContentLengthBodyWriter<I> {
    pub async fn write(&mut self, buf: &[u8]) -> BackendResult<()> {
        self.inner
            .write(buf)
            .await
            .map_err(BackendError::BodyWriteError)
    }

    pub fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<BackendResult<usize>> {
        self.inner
            .poll_write(cx, buf)
            .map_err(BackendError::BodyWriteError)
    }

    pub async fn flush(&mut self) -> BackendResult<()> {
        poll_fn(|cx| self.poll_flush(cx)).await
    }

    pub fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<BackendResult<()>> {
        self.inner
            .poll_flush(cx)
            .map_err(BackendError::BodyWriteError)
    }

    pub async fn finish(mut self) -> BackendResult<ProxyBackendWriter<I>> {
        self.flush().await?;
        self.into_writer()
    }

    pub fn into_writer(self) -> BackendResult<ProxyBackendWriter<I>> {
        let writer = self
            .inner
            .into_writer()
            .map_err(BackendError::BodyWriteError)?;
        Ok(ProxyBackendWriter::new(writer))
    }
}

/// Proxy-owned typestate wrapper for [`IdleWriter`]. Represents a chunked body
/// writer at a chunk boundary, ready to begin a new chunk or finish the body.
pub struct ProxyBackendIdleWriter<I> {
    inner: IdleWriter<I>,
}

impl<I> ProxyBackendIdleWriter<I> {
    /// Begin a new chunk of the given length, returning a
    /// [`ProxyBackendHeadWriter`] that will write the chunk header.
    pub fn start(self, chunk_length: u64) -> ProxyBackendHeadWriter<I> {
        ProxyBackendHeadWriter {
            inner: self.inner.start(chunk_length),
        }
    }

    /// Consume the writer and return a [`ProxyBackendChunkedBodyFinisher`] that
    /// writes the terminal zero-length chunk and any trailers.
    pub fn into_finisher(
        self,
        trailers: &jrpxy_http_message::header::Headers,
    ) -> ProxyBackendChunkedBodyFinisher<I> {
        ProxyBackendChunkedBodyFinisher {
            inner: Some(self.inner.into_final(trailers)),
        }
    }

    /// Consume the writer and return a [`ProxyBackendChunkedBodyAborter`] that
    /// writes the abort byte (`x`) and flushes.
    pub fn into_aborter(self) -> ProxyBackendChunkedBodyAborter<I> {
        ProxyBackendChunkedBodyAborter {
            inner: Some(self.inner.abort()),
        }
    }
}

impl<I: AsyncWrite + Unpin> ProxyBackendIdleWriter<I> {
    /// Write a single complete chunk (head + data + footer). Returns the
    /// updated [`ProxyBackendIdleWriter`] ready for the next chunk on success.
    ///
    /// No-ops when `buf` is empty to avoid writing a zero-length chunk.
    pub async fn write(self, buf: &[u8]) -> BackendResult<Self> {
        if buf.is_empty() {
            return Ok(self);
        }
        let idle = self
            .inner
            .start(buf.len() as u64)
            .write()
            .await
            .map_err(BackendError::BodyWriteError)?
            .write(buf)
            .await
            .map_err(BackendError::BodyWriteError)?
            .write()
            .await
            .map_err(BackendError::BodyWriteError)?;
        Ok(ProxyBackendIdleWriter { inner: idle })
    }
}

/// Proxy-owned typestate wrapper for [`HeadWriter`]. Writes the chunk header.
pub struct ProxyBackendHeadWriter<I> {
    inner: HeadWriter<I>,
}

impl<I: AsyncWrite + Unpin> ProxyBackendHeadWriter<I> {
    /// Write the entire chunk header, returning a [`ProxyBackendDataWriter`]
    /// on success.
    pub async fn write(mut self) -> BackendResult<ProxyBackendDataWriter<I>> {
        poll_fn(|cx| self.poll_write(cx)).await?;
        Ok(self.finish())
    }

    pub fn poll_write(&mut self, cx: &mut Context<'_>) -> Poll<BackendResult<()>> {
        self.inner
            .poll_write(cx)
            .map_err(BackendError::BodyWriteError)
    }

    /// Finish the head writer and return a [`ProxyBackendDataWriter`].
    ///
    /// # Panics
    ///
    /// Panics if the chunk header has not been fully written via
    /// [`Self::poll_write`].
    pub fn finish(self) -> ProxyBackendDataWriter<I> {
        ProxyBackendDataWriter {
            inner: self.inner.finish(),
        }
    }
}

/// Proxy-owned typestate wrapper for [`DataWriter`]. Writes the data bytes of
/// a single chunk.
pub struct ProxyBackendDataWriter<I> {
    inner: DataWriter<I>,
}

impl<I> ProxyBackendDataWriter<I> {
    /// Returns `true` when all declared bytes have been written.
    pub fn is_complete(&self) -> bool {
        self.inner.is_complete()
    }

    /// Finish the data writer and return a [`ProxyBackendDataCompleter`].
    ///
    /// # Panics
    ///
    /// Panics if all declared bytes have not been written via
    /// [`Self::poll_write`].
    pub fn finish(self) -> ProxyBackendDataCompleter<I> {
        ProxyBackendDataCompleter {
            inner: self.inner.finish(),
        }
    }
}

impl<I: AsyncWrite + Unpin> ProxyBackendDataWriter<I> {
    pub async fn write(&mut self, buf: &[u8]) -> BackendResult<usize> {
        poll_fn(|cx| self.poll_write(cx, buf)).await
    }

    pub fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<BackendResult<usize>> {
        self.inner
            .poll_write(cx, buf)
            .map_err(BackendError::BodyWriteError)
    }
}

/// Proxy-owned typestate wrapper for [`DataCompleter`]. Writes the chunk
/// footer (`\r\n`).
pub struct ProxyBackendDataCompleter<I> {
    inner: DataCompleter<I>,
}

impl<I> ProxyBackendDataCompleter<I> {
    /// Finish the completer and return a [`ProxyBackendIdleWriter`] ready for
    /// the next chunk.
    ///
    /// # Panics
    ///
    /// Panics if the chunk footer has not been fully written via
    /// [`Self::poll_write`].
    pub fn into_idle_writer(self) -> ProxyBackendIdleWriter<I> {
        ProxyBackendIdleWriter {
            inner: self.inner.finish(),
        }
    }
}

impl<I: AsyncWrite + Unpin> ProxyBackendDataCompleter<I> {
    /// Write the chunk footer, returning a [`ProxyBackendIdleWriter`] on
    /// success.
    pub async fn write(mut self) -> BackendResult<ProxyBackendIdleWriter<I>> {
        poll_fn(|cx| self.poll_write(cx)).await?;
        Ok(self.into_idle_writer())
    }

    pub fn poll_write(&mut self, cx: &mut Context<'_>) -> Poll<BackendResult<()>> {
        self.inner
            .poll_complete(cx)
            .map_err(BackendError::BodyWriteError)
    }
}

/// Aborts a chunked backend request by writing an invalid chunk header byte
/// (`x`) and flushing.
pub struct ProxyBackendChunkedBodyAborter<I> {
    /// `None` once the abort has been written and flushed.
    inner: Option<AbortWriter<I>>,
}

impl<I: AsyncWrite + Unpin> ProxyBackendChunkedBodyAborter<I> {
    pub async fn abort(mut self) -> BackendResult<()> {
        poll_fn(|cx| self.poll_abort(cx)).await
    }

    pub fn poll_abort(&mut self, cx: &mut Context<'_>) -> Poll<BackendResult<()>> {
        let Some(inner) = self.inner.as_mut() else {
            return ready_error(BodyError::WriteAfterError);
        };
        match inner.poll_abort(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => ready_error(e),
            Poll::Ready(Ok(())) => {
                self.inner = None;
                Poll::Ready(Ok(()))
            }
        }
    }
}

/// Writes the terminal chunk and trailers for a chunked backend request,
/// returning a [`ProxyBackendWriter`] on completion.
pub struct ProxyBackendChunkedBodyFinisher<I> {
    /// `None` once the terminal chunk has been fully written.
    inner: Option<FinalWriter<I>>,
}

impl<I: AsyncWrite + Unpin> ProxyBackendChunkedBodyFinisher<I> {
    pub fn poll_finish(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<BackendResult<ProxyBackendWriter<I>>> {
        let Some(inner) = self.inner.as_mut() else {
            return ready_error(BodyError::WriteAfterError);
        };
        match inner.poll_write(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => ready_error(e),
            Poll::Ready(Ok(())) => {
                let writer = self.inner.take().unwrap().finish();
                Poll::Ready(Ok(ProxyBackendWriter::new(writer)))
            }
        }
    }
}

impl<I: AsyncWriteExt + Unpin> ProxyBackendChunkedBodyFinisher<I> {
    pub async fn finish(mut self) -> BackendResult<ProxyBackendWriter<I>> {
        poll_fn(|cx| self.poll_finish(cx)).await
    }
}

/// The body writer for a proxy-owned backend connection. Finishing always
/// produces a [`ProxyBackendWriter`] without exposing raw IO.
pub enum ProxyBackendBodyWriter<I> {
    Bodyless(ProxyBackendBodylessBodyWriter<I>),
    CL(ProxyBackendContentLengthBodyWriter<I>),
    TE(Option<ProxyBackendIdleWriter<I>>),
}

impl<I: AsyncWriteExt + Unpin> ProxyBackendBodyWriter<I> {
    pub async fn write(&mut self, buf: &[u8]) -> BackendResult<()> {
        match self {
            ProxyBackendBodyWriter::Bodyless(_) => Err(BackendError::BodyWriteError(
                BodyError::BodyOverflow(buf.len() as u64),
            )),
            ProxyBackendBodyWriter::CL(w) => w.write(buf).await,
            ProxyBackendBodyWriter::TE(opt) => {
                let idle = opt
                    .take()
                    .ok_or(BackendError::BodyWriteError(BodyError::WriteAfterError))?;
                *opt = Some(idle.write(buf).await?);
                Ok(())
            }
        }
    }

    pub async fn finish(self) -> BackendResult<ProxyBackendWriter<I>> {
        match self {
            ProxyBackendBodyWriter::Bodyless(w) => w.finish(),
            ProxyBackendBodyWriter::CL(w) => w.finish().await,
            ProxyBackendBodyWriter::TE(opt) => {
                let idle = opt.ok_or(BackendError::BodyWriteError(BodyError::WriteAfterError))?;
                idle.into_finisher(&Default::default()).finish().await
            }
        }
    }

    pub async fn abort(self) -> BackendResult<()> {
        match self {
            ProxyBackendBodyWriter::Bodyless(_) | ProxyBackendBodyWriter::CL(_) => Ok(()),
            ProxyBackendBodyWriter::TE(opt) => {
                if let Some(idle) = opt {
                    idle.into_aborter().abort().await?;
                }
                Ok(())
            }
        }
    }
}

fn ready_error<T>(e: BodyError) -> Poll<BackendResult<T>> {
    Poll::Ready(Err(BackendError::BodyWriteError(e)))
}
