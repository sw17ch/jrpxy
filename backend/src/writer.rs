use jrpxy_body::{
    error::BodyError,
    writer::{
        bodyless::BodylessBodyWriter,
        chunked::{DataCompleter, DataWriter, FinalWriter, HeadWriter, IdleWriter},
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

use crate::error::{BackendError, BackendResult};

pub struct BackendWriter<I> {
    writer: I,
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
        Ok(BackendBodyWriter::TE(BackendChunkedBodyWriter {
            state: Some(BackendChunkState::Idle(IdleWriter::new(writer))),
        }))
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

macro_rules! park {
    ($self:expr, $state:expr) => {{
        $self.state = Some($state);
        return Poll::Pending;
    }};
}

enum BackendChunkState<I> {
    Idle(IdleWriter<I>),
    WritingHead(HeadWriter<I>),
    WritingData {
        writer: DataWriter<I>,
        written: usize,
    },
    WritingFooter(DataCompleter<I>),
}

/// Backend wrapper for a chunked body writer. Finishing it with trailers
/// forwards them to the backend as chunked trailer fields.
pub struct BackendChunkedBodyWriter<I> {
    /// `None` after an error or once the writer has been finished/aborted.
    state: Option<BackendChunkState<I>>,
}

impl<I> BackendChunkedBodyWriter<I> {
    /// Consume the writer and return a [`BackendChunkedBodyFinisher`] that
    /// writes the terminal chunk and trailers. Returns an error if the writer
    /// is not at a chunk boundary (i.e. not in the idle state).
    pub fn into_finisher(self, trailers: &Headers) -> BackendResult<BackendChunkedBodyFinisher<I>> {
        match self.state {
            Some(BackendChunkState::Idle(idle)) => Ok(BackendChunkedBodyFinisher {
                inner: Some(idle.into_final(trailers)),
            }),
            _ => Err(BackendError::BodyWriteError(BodyError::WriteAfterError)),
        }
    }
}

impl<I: AsyncWrite + Unpin> BackendChunkedBodyWriter<I> {
    pub async fn write(&mut self, buf: &[u8]) -> BackendResult<()> {
        poll_fn(|cx| self.poll_write(cx, buf)).await
    }

    pub fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<BackendResult<()>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(()));
        }
        loop {
            let state = match self.state.take() {
                None => return ready_error(BodyError::WriteAfterError),
                Some(s) => s,
            };
            match state {
                BackendChunkState::Idle(idle) => {
                    self.state = Some(BackendChunkState::WritingHead(idle.start(buf.len() as u64)));
                }
                BackendChunkState::WritingHead(mut head) => match head.poll_write(cx) {
                    Poll::Pending => park!(self, BackendChunkState::WritingHead(head)),
                    Poll::Ready(Err(e)) => return ready_error(e),
                    Poll::Ready(Ok(())) => {
                        self.state = Some(BackendChunkState::WritingData {
                            writer: head.finish(),
                            written: 0,
                        });
                    }
                },
                BackendChunkState::WritingData {
                    mut writer,
                    written,
                } => match writer.poll_write(cx, &buf[written..]) {
                    Poll::Pending => {
                        park!(self, BackendChunkState::WritingData { writer, written })
                    }
                    Poll::Ready(Err(e)) => return ready_error(e),
                    Poll::Ready(Ok(n)) => {
                        let written = written + n;
                        if writer.is_complete() {
                            self.state = Some(BackendChunkState::WritingFooter(writer.finish()));
                        } else {
                            park!(self, BackendChunkState::WritingData { writer, written });
                        }
                    }
                },
                BackendChunkState::WritingFooter(mut footer) => match footer.poll_complete(cx) {
                    Poll::Pending => park!(self, BackendChunkState::WritingFooter(footer)),
                    Poll::Ready(Err(e)) => return ready_error(e),
                    Poll::Ready(Ok(())) => {
                        self.state = Some(BackendChunkState::Idle(footer.finish()));
                        return Poll::Ready(Ok(()));
                    }
                },
            }
        }
    }

    pub async fn finish_with_trailers(self, trailers: &Headers) -> BackendResult<BackendWriter<I>> {
        self.into_finisher(trailers)?.finish().await
    }

    pub async fn finish(self) -> BackendResult<BackendWriter<I>> {
        self.finish_with_trailers(&Default::default()).await
    }

    pub async fn abort(self) -> BackendResult<()> {
        // Only write the abort byte when idle (between chunks). Mid-chunk,
        // the partial write already leaves framing broken — which is the goal.
        if let Some(BackendChunkState::Idle(idle)) = self.state {
            idle.abort()
                .write()
                .await
                .map_err(BackendError::BodyWriteError)?;
        }
        Ok(())
    }
}

/// Writes the terminal chunk and trailers for a chunked backend request.
/// Obtained from [`BackendChunkedBodyWriter::into_finisher`].
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
    TE(BackendChunkedBodyWriter<I>),
}

impl<I: AsyncWriteExt + Unpin> BackendBodyWriter<I> {
    pub async fn write(&mut self, buf: &[u8]) -> BackendResult<()> {
        match self {
            BackendBodyWriter::Bodyless(_) => Err(BackendError::BodyWriteError(
                jrpxy_body::error::BodyError::BodyOverflow(buf.len() as u64),
            )),
            BackendBodyWriter::CL(w) => w.write(buf).await,
            BackendBodyWriter::TE(w) => w.write(buf).await,
        }
    }

    pub async fn finish(self) -> BackendResult<BackendWriter<I>> {
        match self {
            BackendBodyWriter::Bodyless(w) => w.finish(),
            BackendBodyWriter::CL(w) => w.finish().await,
            BackendBodyWriter::TE(w) => w.finish().await,
        }
    }

    pub async fn abort(self) -> BackendResult<()> {
        match self {
            BackendBodyWriter::Bodyless(_) | BackendBodyWriter::CL(_) => Ok(()),
            BackendBodyWriter::TE(w) => w.abort().await,
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

    use crate::{error::BackendError, writer::BackendWriter};

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
