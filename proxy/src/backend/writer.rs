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
    header::Headers,
    message::Request,
};
use std::{
    future::poll_fn,
    task::{Context, Poll},
};

use tokio::io::{self, AsyncWrite, AsyncWriteExt};

use crate::backend::error::{BackendError, BackendResult};

pub struct BackendWriter<I> {
    writer: I,
}

impl<I> BackendWriter<I> {
    /// Reassemble a [`BackendWriter`] from its raw inner IO. Used by the
    /// request body pump to hand a writer back to the caller once the request
    /// body has been fully forwarded.
    pub(crate) fn from_writer(writer: I) -> Self {
        Self { writer }
    }
}

impl<I: AsyncWriteExt + Unpin> BackendWriter<I> {
    pub fn new(writer: I) -> Self {
        Self { writer }
    }

    pub async fn send_as_chunked(self, request: &Request) -> BackendResult<BackendBodyWriter<I>> {
        let Self { mut writer } = self;
        write_request_to(request, WriteFraming::Chunked, &mut writer)
            .await
            .map_err(BackendError::WriteError)?;
        Ok(BackendBodyWriter::TE(Some(BackendIdleWriter {
            inner: IdleWriter::new(writer),
        })))
    }

    pub async fn send_as_content_length(
        self,
        request: &Request,
        body_len: u64,
    ) -> BackendResult<BackendBodyWriter<I>> {
        let Self { mut writer } = self;
        write_request_to(request, WriteFraming::Length(body_len), &mut writer)
            .await
            .map_err(BackendError::WriteError)?;
        Ok(BackendBodyWriter::CL(BackendContentLengthBodyWriter {
            inner: ContentLengthBodyWriter::new(body_len, writer),
        }))
    }

    /// Send the response to the backend as bodyless. There's no mechanism for
    /// specifying `content-length` or `transfer-encoding: chunked` on a
    /// request, so in this case, we just elide any framing headers.
    pub async fn send_as_bodyless(
        self,
        request: &Request,
    ) -> Result<BackendBodyWriter<I>, BackendError> {
        let Self { mut writer } = self;
        write_request_to(request, WriteFraming::PreserveFraming, &mut writer)
            .await
            .map_err(BackendError::WriteError)?;
        Ok(BackendBodyWriter::Bodyless(BackendBodylessBodyWriter {
            inner: BodylessBodyWriter::new(writer),
        }))
    }

    pub fn into_inner(self) -> I {
        let Self { writer } = self;
        writer
    }
}

async fn write_request_to<W: AsyncWriteExt + Unpin>(
    req: &Request,
    framing: WriteFraming,
    mut w: W,
) -> io::Result<()> {
    let method = req.method();
    let path = req.path();

    // TODO: write vectored

    // write out the request line
    w.write_all(method).await?;
    w.write_all(b" ").await?;
    w.write_all(path).await?;
    w.write_all(b" ").await?;
    // we alaways send HTTP/1.1 from the proxy even if the client or server is
    // HTTP/1.0.
    w.write_all(b"HTTP/1.1").await?;
    w.write_all(b"\r\n").await?;

    // slice each header, and filter out framing headers
    let headers = req.headers().iter().filter(|(n, _)| !is_framing_header(n));

    // write out each header
    for (n, v) in headers {
        w.write_all(n).await?;
        w.write_all(b": ").await?;
        w.write_all(v).await?;
        w.write_all(b"\r\n").await?;
    }

    // add the framing header, if any
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

    // write out the \r\n indicating the end of the head
    w.write_all(b"\r\n").await?;

    // flush the request head so that the backend has a chance to respond to
    // just the head in case this writer is buffered.
    w.flush().await?;

    Ok(())
}

/// Backend wrapper for [`BodylessBodyWriter`].
pub struct BackendBodylessBodyWriter<I> {
    inner: BodylessBodyWriter<I>,
}

impl<I> BackendBodylessBodyWriter<I> {
    pub(crate) fn into_inner(self) -> BodylessBodyWriter<I> {
        let Self { inner } = self;
        inner
    }
}

impl<I: AsyncWriteExt + Unpin> BackendBodylessBodyWriter<I> {
    pub fn finish(self) -> BackendResult<BackendWriter<I>> {
        let Self { inner } = self;
        Ok(BackendWriter::new(inner.finish()))
    }
}

/// Backend wrapper for [`ContentLengthBodyWriter`].
pub struct BackendContentLengthBodyWriter<I> {
    inner: ContentLengthBodyWriter<I>,
}

impl<I> BackendContentLengthBodyWriter<I> {
    pub(crate) fn into_inner(self) -> ContentLengthBodyWriter<I> {
        let Self { inner } = self;
        inner
    }
}

impl<I: AsyncWriteExt + Unpin> BackendContentLengthBodyWriter<I> {
    pub async fn write(&mut self, buf: &[u8]) -> BackendResult<()> {
        let Self { inner } = self;
        inner.write(buf).await.map_err(BackendError::BodyWriteError)
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

    pub async fn finish(mut self) -> BackendResult<BackendWriter<I>> {
        self.flush().await?;
        self.into_writer()
    }

    pub fn into_writer(self) -> BackendResult<BackendWriter<I>> {
        let Self { inner } = self;
        let writer = inner.into_writer().map_err(BackendError::BodyWriteError)?;
        Ok(BackendWriter::new(writer))
    }
}

/// Backend typestate wrapper for [`IdleWriter`]. Represents a chunked body
/// writer at a chunk boundary, ready to begin a new chunk or finish the body.
pub struct BackendIdleWriter<I> {
    inner: IdleWriter<I>,
}

impl<I> BackendIdleWriter<I> {
    pub(crate) fn into_inner(self) -> IdleWriter<I> {
        let Self { inner } = self;
        inner
    }

    /// Begin a new chunk of the given length, returning a [`BackendHeadWriter`]
    /// that will write the chunk header.
    pub fn start(self, chunk_length: u64) -> BackendHeadWriter<I> {
        BackendHeadWriter {
            inner: self.inner.start(chunk_length),
        }
    }

    /// Consume the writer and return a [`BackendChunkedBodyFinisher`] that
    /// writes the terminal zero-length chunk and any trailers.
    pub fn into_finisher(self, trailers: &Headers) -> BackendChunkedBodyFinisher<I> {
        BackendChunkedBodyFinisher {
            inner: Some(self.inner.into_final(trailers)),
        }
    }

    /// Consume the writer and return a [`BackendChunkedBodyAborter`] that
    /// writes the abort byte (`x`) and flushes.
    pub fn into_aborter(self) -> BackendChunkedBodyAborter<I> {
        BackendChunkedBodyAborter {
            inner: Some(self.inner.abort()),
        }
    }
}

impl<I: AsyncWrite + Unpin> BackendIdleWriter<I> {
    /// Write a single complete chunk (head + data + footer). Returns the updated
    /// [`BackendIdleWriter`] ready for the next chunk on success.
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
        Ok(BackendIdleWriter { inner: idle })
    }
}

/// Backend typestate wrapper for [`HeadWriter`]. Writes the chunk header.
pub struct BackendHeadWriter<I> {
    inner: HeadWriter<I>,
}

impl<I: AsyncWrite + Unpin> BackendHeadWriter<I> {
    /// Write the entire chunk header, returning a [`BackendDataWriter`] on
    /// success.
    pub async fn write(mut self) -> BackendResult<BackendDataWriter<I>> {
        poll_fn(|cx| self.poll_write(cx)).await?;
        Ok(self.finish())
    }

    pub fn poll_write(&mut self, cx: &mut Context<'_>) -> Poll<BackendResult<()>> {
        self.inner
            .poll_write(cx)
            .map_err(BackendError::BodyWriteError)
    }

    /// Finish the head writer and return a [`BackendDataWriter`].
    ///
    /// # Panics
    ///
    /// Panics if the chunk header has not been fully written via
    /// [`Self::poll_write`].
    pub fn finish(self) -> BackendDataWriter<I> {
        BackendDataWriter {
            inner: self.inner.finish(),
        }
    }
}

/// Backend typestate wrapper for [`DataWriter`]. Writes the data bytes of a
/// single chunk.
pub struct BackendDataWriter<I> {
    inner: DataWriter<I>,
}

impl<I> BackendDataWriter<I> {
    /// Returns `true` when all declared bytes have been written.
    pub fn is_complete(&self) -> bool {
        self.inner.is_complete()
    }

    /// Finish the data writer and return a [`BackendDataCompleter`].
    ///
    /// # Panics
    ///
    /// Panics if all declared bytes have not been written via
    /// [`Self::poll_write`].
    pub fn finish(self) -> BackendDataCompleter<I> {
        BackendDataCompleter {
            inner: self.inner.finish(),
        }
    }
}

impl<I: AsyncWrite + Unpin> BackendDataWriter<I> {
    pub async fn write(&mut self, buf: &[u8]) -> BackendResult<usize> {
        poll_fn(|cx| self.poll_write(cx, buf)).await
    }

    pub fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<BackendResult<usize>> {
        self.inner
            .poll_write(cx, buf)
            .map_err(BackendError::BodyWriteError)
    }
}

/// Backend typestate wrapper for [`DataCompleter`]. Writes the chunk footer
/// (`\r\n`).
pub struct BackendDataCompleter<I> {
    inner: DataCompleter<I>,
}

impl<I> BackendDataCompleter<I> {
    /// Finish the completer and return a [`BackendIdleWriter`] ready for the
    /// next chunk.
    ///
    /// # Panics
    ///
    /// Panics if the chunk footer has not been fully written via
    /// [`Self::poll_write`].
    pub fn into_idle_writer(self) -> BackendIdleWriter<I> {
        BackendIdleWriter {
            inner: self.inner.finish(),
        }
    }
}

impl<I: AsyncWrite + Unpin> BackendDataCompleter<I> {
    /// Write the chunk footer, returning a [`BackendIdleWriter`] on success.
    pub async fn write(mut self) -> BackendResult<BackendIdleWriter<I>> {
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
pub struct BackendChunkedBodyAborter<I> {
    /// `None` once the abort has been written and flushed.
    inner: Option<AbortWriter<I>>,
}

impl<I: AsyncWrite + Unpin> BackendChunkedBodyAborter<I> {
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

/// Writes the terminal chunk and trailers for a chunked backend request.
pub struct BackendChunkedBodyFinisher<I> {
    inner: Option<FinalWriter<I>>,
}

impl<I: AsyncWrite + Unpin> BackendChunkedBodyFinisher<I> {
    pub fn poll_finish(&mut self, cx: &mut Context<'_>) -> Poll<BackendResult<BackendWriter<I>>> {
        let Some(inner) = self.inner.as_mut() else {
            return ready_error(BodyError::WriteAfterError);
        };
        match inner.poll_write(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => ready_error(e),
            Poll::Ready(Ok(())) => {
                let writer = self.inner.take().unwrap().finish();
                Poll::Ready(Ok(BackendWriter::new(writer)))
            }
        }
    }
}

impl<I: AsyncWriteExt + Unpin> BackendChunkedBodyFinisher<I> {
    pub async fn finish(mut self) -> BackendResult<BackendWriter<I>> {
        poll_fn(|cx| self.poll_finish(cx)).await
    }
}

/// The body writer for a backend connection. Finishing always produces a
/// [`BackendWriter`] without exposing raw IO.
pub enum BackendBodyWriter<I> {
    Bodyless(BackendBodylessBodyWriter<I>),
    CL(BackendContentLengthBodyWriter<I>),
    TE(Option<BackendIdleWriter<I>>),
}

impl<I: AsyncWriteExt + Unpin> BackendBodyWriter<I> {
    pub async fn write(&mut self, buf: &[u8]) -> BackendResult<()> {
        match self {
            BackendBodyWriter::Bodyless(_) => Err(BackendError::BodyWriteError(
                jrpxy_body::error::BodyError::BodyOverflow(buf.len() as u64),
            )),
            BackendBodyWriter::CL(w) => w.write(buf).await,
            BackendBodyWriter::TE(opt) => {
                let idle = opt
                    .take()
                    .ok_or(BackendError::BodyWriteError(BodyError::WriteAfterError))?;
                *opt = Some(idle.write(buf).await?);
                Ok(())
            }
        }
    }

    pub async fn finish(self) -> BackendResult<BackendWriter<I>> {
        match self {
            BackendBodyWriter::Bodyless(w) => w.finish(),
            BackendBodyWriter::CL(w) => w.finish().await,
            BackendBodyWriter::TE(opt) => {
                let idle = opt.ok_or(BackendError::BodyWriteError(BodyError::WriteAfterError))?;
                idle.into_finisher(&Default::default()).finish().await
            }
        }
    }

    pub async fn abort(self) -> BackendResult<()> {
        match self {
            BackendBodyWriter::Bodyless(_) | BackendBodyWriter::CL(_) => Ok(()),
            BackendBodyWriter::TE(opt) => {
                // Only write the abort byte when idle (between chunks). Mid-chunk,
                // the partial write already leaves framing broken — which is the goal.
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

#[cfg(test)]
mod test {
    use jrpxy_body::error::BodyError;
    use jrpxy_http_message::{message::RequestBuilder, version::HttpVersion};

    use crate::backend::{error::BackendError, writer::BackendWriter};

    #[tokio::test]
    async fn write_cl_backend() {
        let mut buf = Vec::new();
        let writer = BackendWriter::new(&mut buf);

        let mut b = RequestBuilder::new(8);
        let req = b
            .with_method("GET")
            .with_path("/cl")
            .with_version(HttpVersion::Http11)
            .with_header("hello", b"world")
            .build()
            .expect("failed to build")
            .into();

        let mut w = writer
            .send_as_content_length(&req, 0)
            .await
            .expect("failed to send");

        // expect that attempting to write to the body results in an unexpected EOF
        let e = w.write(&b"hello"[..]).await.expect_err("wasn't an error");
        assert!(matches!(
            e,
            BackendError::BodyWriteError(BodyError::BodyOverflow(0))
        ));

        let w = w.finish().await.expect("could not finish");
        let buf = w.into_inner();

        let expected = b"\
            GET /cl HTTP/1.1\r\n\
            hello: world\r\n\
            content-length: 0\r\n\
            \r\n\
            ";

        assert_eq!(&expected[..], buf);
    }

    #[tokio::test]
    async fn write_te_backend() {
        let mut buf = Vec::new();
        let writer = BackendWriter::new(&mut buf);

        let mut b = RequestBuilder::new(8);
        let req = b
            .with_method("GET")
            .with_path("/te")
            .with_version(HttpVersion::Http11)
            .with_header("hello", b"world")
            .build()
            .expect("failed to build")
            .into();

        let mut w = writer.send_as_chunked(&req).await.expect("failed to send");

        w.write(&b"01234"[..]).await.expect("failed to write");
        w.write(&b"56"[..]).await.expect("failed to write");
        w.write(&b"7"[..]).await.expect("failed to write");
        w.write(&b""[..]).await.expect("failed to write"); // should not result in a zero chunk
        w.write(&b"8"[..]).await.expect("failed to write");

        let w = w.finish().await.expect("could not finish");
        let buf = w.into_inner();

        let expected = b"\
            GET /te HTTP/1.1\r\n\
            hello: world\r\n\
            transfer-encoding: chunked\r\n\
            \r\n\
            5\r\n\
            01234\r\n\
            2\r\n\
            56\r\n\
            1\r\n\
            7\r\n\
            1\r\n\
            8\r\n\
            0\r\n\
            \r\n\
            ";

        assert_eq!(&expected[..], buf);
    }
}
