//! Tools for writing responses to a HTTP/1.x frontend.
//!
//! We can use [`FrontendWriter`] to write a stream of requests.
//!
//! ```rust
//! use jrpxy_proxy::frontend::writer::FrontendWriter;
//! use jrpxy_http_message::{message::ResponseBuilder, version::HttpVersion};
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() {
//!    let mut buf = Vec::new();
//!
//!    // Write the first response
//!    let frontend_writer = FrontendWriter::new(&mut buf);
//!    let mut builder = ResponseBuilder::new(4);
//!    let resp = builder
//!        .with_version(HttpVersion::Http11)
//!        .with_code(200)
//!        .with_reason("Ok")
//!        .build()
//!        .expect("failed to build");
//!    let mut frontend_body_writer = frontend_writer
//!        .send_as_content_length(&resp, 5)
//!        .await
//!        .unwrap();
//!    frontend_body_writer.write(b"ab").await.unwrap();
//!    frontend_body_writer.write(b"cde").await.unwrap();
//!    let frontend_writer = frontend_body_writer
//!        .finish()
//!        .await
//!        .expect("failed to finish")
//!        .expect("failed to recycle frontend writer");
//!
//!    // Write the second response
//!    let mut builder = ResponseBuilder::new(4);
//!    let resp = builder
//!        .with_version(HttpVersion::Http11)
//!        .with_code(404)
//!        .with_reason("Not Found")
//!        .build()
//!        .expect("failed to build");
//!    let mut frontend_body_writer = frontend_writer.send_as_chunked(&resp).await.unwrap();
//!    frontend_body_writer.write(b"01234").await.unwrap();
//!    frontend_body_writer.write(b"567").await.unwrap();
//!    frontend_body_writer.write(b"89").await.unwrap();
//!    let _frontend_writer = frontend_body_writer.finish().await.unwrap();
//!
//!    assert_eq!(
//!        b"\
//!            HTTP/1.1 200 Ok\r\n\
//!            content-length: 5\r\n\
//!            \r\n\
//!            abcde\
//!            HTTP/1.1 404 Not Found\r\n\
//!            transfer-encoding: chunked\r\n\
//!            \r\n\
//!            5\r\n\
//!            01234\r\n\
//!            3\r\n\
//!            567\r\n\
//!            2\r\n\
//!            89\r\n\
//!            0\r\n\
//!            \r\n\
//!            ",
//!        &buf[..]
//!    );
//! }
//! ```

use crate::frontend::error::{FrontendError, FrontendResult};
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
    message::Response,
};
use std::{
    future::poll_fn,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{self, AsyncWrite, AsyncWriteExt};

/// Writes a response to a frontend.
#[derive(Debug)]
pub struct FrontendWriter<I> {
    writer: I,
}

impl<I> FrontendWriter<I> {
    pub fn into_inner(self) -> I {
        let Self { writer } = self;
        writer
    }

    pub fn as_inner(&self) -> &I {
        &self.writer
    }
}

impl<I: AsyncWriteExt + Unpin> FrontendWriter<I> {
    /// Create a new frontend writer that will write responses into the
    /// specified `io`.
    pub fn new(writer: I) -> Self {
        Self { writer }
    }

    /// Send the response with a body terminated by connection close (RFC 9112
    /// section 6.3). Any framing headers from the origin are stripped. Use this
    /// when the client cannot decode chunked transfer encoding (e.g. HTTP/1.0).
    ///
    /// Finishing the returned [`FrontendBodyWriter`] does not produce a
    /// [`FrontendWriter`] - the connection must be closed after the body is
    /// sent.
    ///
    /// Note that dropping this body has the same effect as finishing the body.
    /// It is impossible for the client to determine if it received an entire
    /// response based only on the standard headers and body.
    pub async fn send_as_eof(self, response: &Response) -> FrontendResult<FrontendBodyWriter<I>> {
        let Self { mut writer } = self;
        write_response_to(response, WriteFraming::StripFraming, &mut writer)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(FrontendBodyWriter::Close(FrontendEofBodyWriter { writer }))
    }

    /// Send the specified response with a chunked response body.
    pub async fn send_as_chunked(
        self,
        response: &Response,
    ) -> FrontendResult<FrontendBodyWriter<I>> {
        let Self { mut writer } = self;
        write_response_to(response, WriteFraming::Chunked, &mut writer)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(FrontendBodyWriter::TE(Some(FrontendIdleWriter {
            inner: IdleWriter::new(writer),
        })))
    }

    /// Send the specified response with a content-length delimited response
    /// body. The [`FrontendBodyWriter`] will only accept the specified number
    /// of bytes.
    pub async fn send_as_content_length(
        self,
        response: &Response,
        body_len: u64,
    ) -> FrontendResult<FrontendBodyWriter<I>> {
        let Self { mut writer } = self;
        write_response_to(response, WriteFraming::Length(body_len), &mut writer)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(FrontendBodyWriter::CL(FrontendContentLengthBodyWriter {
            inner: ContentLengthBodyWriter::new(body_len, writer),
        }))
    }

    /// Send the response to the frontend with no body, preserving any framing
    /// headers (`content-length`, `transfer-encoding`) from the origin.
    ///
    /// Use this for responses to `HEAD` requests and `304 Not Modified`
    /// responses, where framing headers describe the representation that
    /// *would* have been sent rather than an actual body.
    pub async fn send_as_bodyless_keep_framing(
        self,
        response: &Response,
    ) -> Result<FrontendBodyWriter<I>, FrontendError> {
        let Self { mut writer } = self;
        write_response_to(response, WriteFraming::PreserveFraming, &mut writer)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(FrontendBodyWriter::Bodyless(FrontendBodylessBodyWriter {
            inner: BodylessBodyWriter::new(writer),
        }))
    }

    /// Send the response to the frontend with no body, stripping any framing
    /// headers from the origin.
    ///
    /// Use this for responses that must never carry a body by definition: `1xx`
    /// informational responses and `204 No Content`. Forwarding a
    /// `content-length` on these would corrupt the client's parser.
    ///
    /// Note that this returns [`FrontendBodylessBodyWriter`] rather than
    /// [`FrontendBodyWriter`].
    ///
    /// # Panics (debug)
    /// Asserts in debug builds that the response carries no framing headers,
    /// catching misbehaving origins during development.
    pub async fn send_as_no_content(
        self,
        response: &Response,
    ) -> Result<FrontendBodylessBodyWriter<I>, FrontendError> {
        debug_assert!(
            !response.headers().iter().any(|(n, _)| is_framing_header(n)),
            "send_as_no_content called on a response that contains framing headers \
             (content-length or transfer-encoding); the origin is misbehaving"
        );
        let Self { mut writer } = self;
        write_response_to(response, WriteFraming::StripFraming, &mut writer)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(FrontendBodylessBodyWriter {
            inner: BodylessBodyWriter::new(writer),
        })
    }
}

async fn write_response_to<W: AsyncWriteExt + Unpin>(
    res: &Response,
    framing: WriteFraming,
    mut w: W,
) -> io::Result<()> {
    let code = res.code();
    let reason = res.reason();

    // TODO: we can avoid this heap allocation
    let code = format!("{code}");

    // we always send HTTP/1.1 from the proxy even if the client or server is
    // HTTP/1.0.
    w.write_all(b"HTTP/1.1").await?;
    w.write_all(b" ").await?;
    w.write_all(code.as_bytes()).await?;
    w.write_all(b" ").await?;
    w.write_all(reason).await?;
    w.write_all(b"\r\n").await?;

    // NoFraming leaves the origin's framing headers in place (HEAD / 304).
    // All other variants strip them and optionally append a new one.
    let headers = res
        .headers()
        .iter()
        .filter(|(n, _)| framing.preserves_framing() || !is_framing_header(n));

    // write out each header
    for (n, v) in headers {
        w.write_all(n).await?;
        w.write_all(b": ").await?;
        w.write_all(v).await?;
        w.write_all(b"\r\n").await?;
    }

    // add the framing header
    match framing {
        WriteFraming::PreserveFraming | WriteFraming::StripFraming => {}
        WriteFraming::Length(l) => {
            // TODO: we can avoid this heap allocation
            let cl = format!("content-length: {l}\r\n");
            w.write_all(cl.as_bytes()).await?;
        }
        WriteFraming::Chunked => {
            w.write_all(b"transfer-encoding: chunked\r\n").await?;
        }
    }

    // write out the \r\n indicating the end of the head
    w.write_all(b"\r\n").await?;

    // flush the response head so that the backend has a chance to respond to
    // just the head in case this writer is buffered.
    w.flush().await?;

    Ok(())
}

/// Frontend writer for bodies terminated by connection close (RFC 9112 section
/// 6.3).
///
/// Produced by [`FrontendWriter::send_as_eof`]. Finishing this writer does not
/// return a [`FrontendWriter`] — the connection is closed once the body is
/// sent.
pub struct FrontendEofBodyWriter<I> {
    writer: I,
}

impl<I: AsyncWrite + Unpin> FrontendEofBodyWriter<I> {
    pub fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<FrontendResult<usize>> {
        Pin::new(&mut self.writer)
            .poll_write(cx, buf)
            .map_err(FrontendError::WriteError)
    }

    pub fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<FrontendResult<()>> {
        Pin::new(&mut self.writer)
            .poll_flush(cx)
            .map_err(FrontendError::WriteError)
    }
}

impl<I: AsyncWriteExt + Unpin> FrontendEofBodyWriter<I> {
    pub async fn write(&mut self, buf: &[u8]) -> FrontendResult<()> {
        self.writer
            .write_all(buf)
            .await
            .map_err(FrontendError::WriteError)
    }

    pub async fn finish(self) -> FrontendResult<()> {
        let Self { mut writer } = self;
        writer.flush().await.map_err(FrontendError::WriteError)
    }
}

/// Frontend wrapper for [`BodylessBodyWriter`].
pub struct FrontendBodylessBodyWriter<I> {
    inner: BodylessBodyWriter<I>,
}

impl<I: AsyncWriteExt + Unpin> FrontendBodylessBodyWriter<I> {
    pub fn finish(self) -> FrontendResult<FrontendWriter<I>> {
        let Self { inner } = self;
        Ok(FrontendWriter {
            writer: inner.finish(),
        })
    }
}

/// Frontend wrapper for [`ContentLengthBodyWriter`].
pub struct FrontendContentLengthBodyWriter<I> {
    inner: ContentLengthBodyWriter<I>,
}

impl<I: AsyncWriteExt + Unpin> FrontendContentLengthBodyWriter<I> {
    pub fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<FrontendResult<usize>> {
        self.inner
            .poll_write(cx, buf)
            .map_err(FrontendError::BodyWriteError)
    }

    pub fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<FrontendResult<()>> {
        self.inner
            .poll_flush(cx)
            .map_err(FrontendError::BodyWriteError)
    }

    pub async fn write(&mut self, buf: &[u8]) -> FrontendResult<()> {
        let Self { inner } = self;
        inner
            .write(buf)
            .await
            .map_err(FrontendError::BodyWriteError)
    }

    pub async fn finish(self) -> FrontendResult<FrontendWriter<I>> {
        let Self { mut inner } = self;
        inner.flush().await.map_err(FrontendError::BodyWriteError)?;
        let writer = inner.into_writer().map_err(FrontendError::BodyWriteError)?;
        Ok(FrontendWriter { writer })
    }
}

/// Frontend typestate wrapper for [`IdleWriter`]. Represents a chunked body
/// writer at a chunk boundary, ready to begin a new chunk or finish the body.
pub struct FrontendIdleWriter<I> {
    inner: IdleWriter<I>,
}

impl<I> FrontendIdleWriter<I> {
    /// Begin a new chunk of the given length, returning a [`FrontendHeadWriter`]
    /// that will write the chunk header.
    pub fn start(self, chunk_length: u64) -> FrontendHeadWriter<I> {
        FrontendHeadWriter {
            inner: self.inner.start(chunk_length),
        }
    }

    /// Consume the writer and return a [`FrontendChunkedBodyFinisher`] that
    /// writes the terminal zero-lengthed chunk and any trailers.
    pub fn into_finisher(self, trailers: &Headers) -> FrontendChunkedBodyFinisher<I> {
        FrontendChunkedBodyFinisher {
            inner: Some(self.inner.into_final(trailers)),
        }
    }

    /// Consume the writer and return a [`FrontendChunkedBodyAborter`] that
    /// writes the abort byte (`x`) and flushes.
    pub fn into_aborter(self) -> FrontendChunkedBodyAborter<I> {
        FrontendChunkedBodyAborter {
            inner: Some(self.inner.abort()),
        }
    }
}

impl<I: AsyncWrite + Unpin> FrontendIdleWriter<I> {
    /// Write a single complete chunk (head + data + footer). Returns the updated
    /// [`FrontendIdleWriter`] ready for the next chunk on success.
    ///
    /// No-ops when `buf` is empty to avoid writing a zero-length chunk.
    pub async fn write(self, buf: &[u8]) -> FrontendResult<Self> {
        if buf.is_empty() {
            return Ok(self);
        }
        let idle = self
            .inner
            .start(buf.len() as u64)
            .write()
            .await
            .map_err(FrontendError::BodyWriteError)?
            .write(buf)
            .await
            .map_err(FrontendError::BodyWriteError)?
            .write()
            .await
            .map_err(FrontendError::BodyWriteError)?;
        Ok(FrontendIdleWriter { inner: idle })
    }
}

/// Frontend typestate wrapper for [`HeadWriter`]. Writes the chunk header. Call
/// [`finish`](FrontendHeadWriter::finish) to obtain a [`FrontendDataWriter`]
/// once [`poll_write`](FrontendHeadWriter::poll_write) completes.
pub struct FrontendHeadWriter<I> {
    inner: HeadWriter<I>,
}

impl<I: AsyncWrite + Unpin> FrontendHeadWriter<I> {
    /// Write the entire chunk header, returning a [`FrontendDataWriter`] on
    /// success.
    pub async fn write(mut self) -> FrontendResult<FrontendDataWriter<I>> {
        poll_fn(|cx| self.poll_write(cx)).await?;
        Ok(self.finish())
    }

    pub fn poll_write(&mut self, cx: &mut Context<'_>) -> Poll<FrontendResult<()>> {
        self.inner
            .poll_write(cx)
            .map_err(FrontendError::BodyWriteError)
    }

    /// Finish the head writer and return a [`FrontendDataWriter`].
    ///
    /// # Panics
    ///
    /// Panics if the chunk header has not been fully written via
    /// [`Self::poll_write`].
    pub fn finish(self) -> FrontendDataWriter<I> {
        FrontendDataWriter {
            inner: self.inner.finish(),
        }
    }
}

/// Frontend typestate wrapper for [`DataWriter`]. Writes the data bytes of a
/// single chunk.
pub struct FrontendDataWriter<I> {
    inner: DataWriter<I>,
}

impl<I> FrontendDataWriter<I> {
    /// Returns `true` when all declared bytes have been written.
    pub fn is_complete(&self) -> bool {
        self.inner.is_complete()
    }

    /// Finish the data writer and return a [`FrontendDataCompleter`].
    ///
    /// # Panics
    ///
    /// Panics if all declared bytes have not been written via
    /// [`Self::poll_write`].
    pub fn finish(self) -> FrontendDataCompleter<I> {
        FrontendDataCompleter {
            inner: self.inner.finish(),
        }
    }
}

impl<I: AsyncWrite + Unpin> FrontendDataWriter<I> {
    pub async fn write_all(&mut self, buf: &[u8]) -> FrontendResult<usize> {
        poll_fn(|cx| self.poll_write(cx, buf)).await
    }

    pub fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<FrontendResult<usize>> {
        self.inner
            .poll_write(cx, buf)
            .map_err(FrontendError::BodyWriteError)
    }
}

/// Frontend typestate wrapper for [`DataCompleter`]. Writes the chunk footer
/// (`\r\n`).
pub struct FrontendDataCompleter<I> {
    inner: DataCompleter<I>,
}

impl<I> FrontendDataCompleter<I> {
    /// Finish the completer and return a [`FrontendIdleWriter`] ready for the
    /// next chunk.
    ///
    /// # Panics
    ///
    /// Panics if the chunk footer has not been fully written via
    /// [`Self::poll_complete`].
    pub fn into_idle_writer(self) -> FrontendIdleWriter<I> {
        FrontendIdleWriter {
            inner: self.inner.finish(),
        }
    }
}

impl<I: AsyncWrite + Unpin> FrontendDataCompleter<I> {
    /// Write the chunk footer, returning a [`FrontendIdleWriter`] on success.
    pub async fn write(mut self) -> FrontendResult<FrontendIdleWriter<I>> {
        poll_fn(|cx| self.poll_write(cx)).await?;
        Ok(self.into_idle_writer())
    }

    pub fn poll_write(&mut self, cx: &mut Context<'_>) -> Poll<FrontendResult<()>> {
        self.inner
            .poll_complete(cx)
            .map_err(FrontendError::BodyWriteError)
    }
}

/// Aborts a chunked frontend response by writing an invalid chunk header byte
/// (`x`) and flushing.
pub struct FrontendChunkedBodyAborter<I> {
    /// `None` once the abort has been written and flushed.
    inner: Option<AbortWriter<I>>,
}

impl<I: AsyncWrite + Unpin> FrontendChunkedBodyAborter<I> {
    pub async fn abort(mut self) -> FrontendResult<()> {
        poll_fn(|cx| self.poll_abort(cx)).await
    }

    pub fn poll_abort(&mut self, cx: &mut Context<'_>) -> Poll<FrontendResult<()>> {
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

/// Writes the terminal chunk and trailers for a chunked frontend response.
pub struct FrontendChunkedBodyFinisher<I> {
    inner: Option<FinalWriter<I>>,
}

impl<I: AsyncWrite + Unpin> FrontendChunkedBodyFinisher<I> {
    pub fn poll_finish(&mut self, cx: &mut Context<'_>) -> Poll<FrontendResult<FrontendWriter<I>>> {
        let Some(inner) = self.inner.as_mut() else {
            return ready_error(BodyError::WriteAfterError);
        };
        match inner.poll_write(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => ready_error(e),
            Poll::Ready(Ok(())) => {
                let writer = self.inner.take().unwrap().finish();
                Poll::Ready(Ok(FrontendWriter { writer }))
            }
        }
    }
}

impl<I: AsyncWriteExt + Unpin> FrontendChunkedBodyFinisher<I> {
    pub async fn finish(mut self) -> FrontendResult<FrontendWriter<I>> {
        poll_fn(|cx| self.poll_finish(cx)).await
    }
}

/// The body writer for a frontend connection; it is typed so that finishing,
/// when possible, can produce a [`FrontendWriter`] without exposing raw IO.
pub enum FrontendBodyWriter<I> {
    Bodyless(FrontendBodylessBodyWriter<I>),
    CL(FrontendContentLengthBodyWriter<I>),
    TE(Option<FrontendIdleWriter<I>>),
    Close(FrontendEofBodyWriter<I>),
}

impl<I: AsyncWriteExt + Unpin> FrontendBodyWriter<I> {
    pub async fn write(&mut self, buf: &[u8]) -> FrontendResult<()> {
        match self {
            FrontendBodyWriter::Bodyless(_) => Err(FrontendError::BodyWriteError(
                jrpxy_body::error::BodyError::BodyOverflow(buf.len() as u64),
            )),
            FrontendBodyWriter::CL(w) => w.write(buf).await,
            FrontendBodyWriter::TE(opt) => {
                let idle = opt
                    .take()
                    .ok_or(FrontendError::BodyWriteError(BodyError::WriteAfterError))?;
                *opt = Some(idle.write(buf).await?);
                Ok(())
            }
            FrontendBodyWriter::Close(w) => w.write(buf).await,
        }
    }

    pub async fn finish(self) -> FrontendResult<Option<FrontendWriter<I>>> {
        match self {
            FrontendBodyWriter::Bodyless(w) => w.finish().map(Some),
            FrontendBodyWriter::CL(w) => w.finish().await.map(Some),
            FrontendBodyWriter::TE(opt) => {
                let idle = opt.ok_or(FrontendError::BodyWriteError(BodyError::WriteAfterError))?;
                idle.into_finisher(&Default::default())
                    .finish()
                    .await
                    .map(Some)
            }
            FrontendBodyWriter::Close(w) => {
                let () = w.finish().await?;
                Ok(None)
            }
        }
    }

    pub async fn abort(self) -> FrontendResult<()> {
        match self {
            FrontendBodyWriter::Bodyless(_)
            | FrontendBodyWriter::CL(_)
            | FrontendBodyWriter::Close(_) => Ok(()),
            FrontendBodyWriter::TE(opt) => {
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

fn ready_error<T>(e: BodyError) -> Poll<FrontendResult<T>> {
    Poll::Ready(Err(FrontendError::BodyWriteError(e)))
}

#[cfg(test)]
mod test {
    use jrpxy_body::error::BodyError;
    use jrpxy_http_message::{message::ResponseBuilder, version::HttpVersion};
    use jrpxy_util::debug::AsciiDebug;

    use crate::frontend::{error::FrontendError, writer::FrontendWriter};

    #[tokio::test]
    async fn frontend_writer() {
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok".into())
            .with_version(HttpVersion::Http11)
            .with_header("x-hello", &b"World"[..])
            .with_header("content-LENGTH", &b"400"[..])
            .with_header("TRANSFER-ENCODING", &b"chunked"[..])
            .build()
            .expect("failed to build response")
            .into();

        let mut buf = Vec::new();
        let cw = FrontendWriter::new(&mut buf);
        let r = cw.send_as_content_length(&res, 0).await.expect("can write");

        let buf = r
            .finish()
            .await
            .expect("failed to finish")
            .unwrap()
            .into_inner();

        // now we validate that the response contains what we expect. Notably:
        // - expect that the content-length has been replaced with 0
        // - expect we strip out transfer-encoding header
        // - expect the remaining header to be present

        let expected = b"\
            HTTP/1.1 200 Ok\r\n\
            x-hello: World\r\n\
            content-length: 0\r\n\
            \r\n\
        ";
        assert_eq!(AsciiDebug(expected), AsciiDebug(buf));
    }

    #[tokio::test]
    async fn frontend_writer_no_body() {
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok".into())
            .with_version(HttpVersion::Http11)
            .build()
            .expect("failed to build response")
            .into();

        let mut buf = Vec::new();
        let cw = FrontendWriter::new(&mut buf);
        let mut r = cw.send_as_content_length(&res, 0).await.expect("can write");

        // expect that attempting to write to the body results in an unexpected EOF
        let r = r.write(&b"hello"[..]).await.expect_err("wasn't an error");
        assert!(matches!(
            r,
            FrontendError::BodyWriteError(BodyError::BodyOverflow(0))
        ));
    }

    // TODO: boundary tests around u64::MAX

    #[tokio::test]
    async fn frontend_writer_with_content_length() {
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok".into())
            .with_version(HttpVersion::Http11)
            .build()
            .expect("failed to build response")
            .into();

        let mut buf = Vec::new();
        let cw = FrontendWriter::new(&mut buf);
        let mut r = cw
            .send_as_content_length(&res, 10)
            .await
            .expect("can write");

        r.write(&b"01234"[..]).await.expect("write works");
        r.write(&b"56789"[..]).await.expect("write works");

        assert!(matches!(
            r.write(&b"X"[..]).await.expect_err("this overflows"),
            FrontendError::BodyWriteError(BodyError::BodyOverflow(10))
        ));

        let _buf = r
            .finish()
            .await
            .expect("failed to finish")
            .unwrap()
            .into_inner();
    }

    #[tokio::test]
    async fn frontend_writer_with_content_length_incomplete_body() {
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok".into())
            .with_version(HttpVersion::Http11)
            .build()
            .expect("failed to build response")
            .into();

        let mut buf = Vec::new();
        let cw = FrontendWriter::new(&mut buf);
        let mut r = cw
            .send_as_content_length(&res, 10)
            .await
            .expect("can write");

        r.write(&b"01234"[..]).await.expect("write works");

        // do not write all 10 bytes, and then try to finish. finishing will fail.

        assert!(matches!(
            r.finish().await.expect_err("failed to emit error"),
            FrontendError::BodyWriteError(BodyError::IncompleteBody {
                expected: 10,
                actual: 5
            })
        ));
    }

    #[tokio::test]
    async fn frontend_writer_with_chunk_encoding() {
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok".into())
            .with_version(HttpVersion::Http11)
            .build()
            .expect("failed to build response")
            .into();

        let mut buf = Vec::new();
        let cw = FrontendWriter::new(&mut buf);
        let mut r = cw.send_as_chunked(&res).await.expect("can write");

        r.write(&b"01234"[..]).await.expect("write works");
        r.write(&b"56789"[..]).await.expect("write works");
        r.write(&b""[..])
            .await
            .expect("works, but shouldn't generate a chunk");

        let _buf = r
            .finish()
            .await
            .expect("failed to finish")
            .unwrap()
            .into_inner();
    }

    #[tokio::test]
    async fn frontend_writer_pipeline() {
        let mut output_buf = Vec::with_capacity(4096);
        let cw = FrontendWriter::new(&mut output_buf);

        // first response
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok".into())
            .with_version(HttpVersion::Http11)
            .with_header("x-req", b"first")
            .build()
            .expect("failed to build response")
            .into();
        let body_writer = cw
            .send_as_content_length(&res, 0)
            .await
            .expect("first write works");
        let cw = body_writer
            .finish()
            .await
            .expect("failed to finish")
            .expect("failed to return writer");

        // second response
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok".into())
            .with_version(HttpVersion::Http11)
            .with_header("x-req", b"second")
            .build()
            .expect("failed to build response")
            .into();
        let mut body_writer = cw
            .send_as_content_length(&res, 5)
            .await
            .expect("first write works");
        body_writer.write(b"01234").await.expect("failed to write");
        let cw = body_writer
            .finish()
            .await
            .expect("failed to finish")
            .expect("failed to return writer");

        // third response
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok".into())
            .with_version(HttpVersion::Http11)
            .with_header("x-req", b"third")
            .build()
            .expect("failed to build response")
            .into();
        let mut body_writer = cw.send_as_chunked(&res).await.expect("first write works");
        body_writer.write(b"01234").await.expect("failed to write");
        body_writer.write(b"5").await.expect("failed to write");
        body_writer.write(b"6789").await.expect("failed to write");
        let cw = body_writer.finish().await.expect("failed to finish");

        // check the output
        let output_buf = cw.unwrap().into_inner().as_slice();

        let expected = b"\
            HTTP/1.1 200 Ok\r\n\
            x-req: first\r\n\
            content-length: 0\r\n\
            \r\n\
            HTTP/1.1 200 Ok\r\n\
            x-req: second\r\n\
            content-length: 5\r\n\
            \r\n\
            01234\
            HTTP/1.1 200 Ok\r\n\
            x-req: third\r\n\
            transfer-encoding: chunked\r\n\
            \r\n\
            5\r\n\
            01234\r\n\
            1\r\n\
            5\r\n\
            4\r\n\
            6789\r\n\
            0\r\n\
            \r\n\
        ";

        assert_eq!(AsciiDebug(expected), AsciiDebug(output_buf));
    }
}
