//! Tools for writing responses to a HTTP/1.x frontend.
//!
//! We can use [`FrontendWriter`] to write a stream of requests.
//!
//! ```rust
//! use jrpxy_frontend::writer::FrontendWriter;
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

use jrpxy_body::is_framing_header;
use jrpxy_body::writer::{BodylessBodyWriter, ChunkedBodyWriter, ContentLengthBodyWriter};
use jrpxy_http_message::framing::WriteFraming;
use jrpxy_http_message::header::Headers;
use jrpxy_http_message::message::Response;
use tokio::io;
use tokio::io::AsyncWriteExt;

use crate::error::FrontendError;
use crate::error::FrontendResult;

/// Writes a response to a frontend.
#[derive(Debug)]
pub struct FrontendWriter<I> {
    pub(crate) io: I,
}

impl<I> FrontendWriter<I> {
    pub fn into_inner(self) -> I {
        let Self { io } = self;
        io
    }

    pub fn as_inner(&self) -> &I {
        &self.io
    }
}

impl<I: AsyncWriteExt + Unpin> FrontendWriter<I> {
    /// Create a new frontend writer that will write responses into the
    /// specified `io`.
    pub fn new(io: I) -> Self {
        Self { io }
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
        let Self { mut io } = self;
        write_response_to(response, WriteFraming::StripFraming, &mut io)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(FrontendBodyWriter::Close(FrontendEofBodyWriter { io }))
    }

    /// Send the specified response with a chunked response body.
    pub async fn send_as_chunked(
        self,
        response: &Response,
    ) -> FrontendResult<FrontendBodyWriter<I>> {
        let Self { mut io } = self;
        write_response_to(response, WriteFraming::Chunked, &mut io)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(FrontendBodyWriter::TE(FrontendChunkedBodyWriter {
            inner: ChunkedBodyWriter::new(io),
        }))
    }

    /// Send the specified response with a content-length delimited response
    /// body. The [`FrontendBodyWriter`] will only accept the specified number
    /// of bytes.
    pub async fn send_as_content_length(
        self,
        response: &Response,
        body_len: u64,
    ) -> FrontendResult<FrontendBodyWriter<I>> {
        let Self { mut io } = self;
        write_response_to(response, WriteFraming::Length(body_len), &mut io)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(FrontendBodyWriter::CL(FrontendContentLengthBodyWriter {
            inner: ContentLengthBodyWriter::new(body_len, io),
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
        let Self { mut io } = self;
        write_response_to(response, WriteFraming::PreserveFraming, &mut io)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(FrontendBodyWriter::Bodyless(FrontendBodylessBodyWriter {
            inner: BodylessBodyWriter::new(io),
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
        let Self { mut io } = self;
        write_response_to(response, WriteFraming::StripFraming, &mut io)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(FrontendBodylessBodyWriter {
            inner: BodylessBodyWriter::new(io),
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
    io: I,
}

impl<I: AsyncWriteExt + Unpin> FrontendEofBodyWriter<I> {
    pub async fn write(&mut self, buf: &[u8]) -> FrontendResult<()> {
        self.io
            .write_all(buf)
            .await
            .map_err(FrontendError::WriteError)
    }

    pub async fn finish(self) -> FrontendResult<()> {
        let Self { mut io } = self;
        io.flush().await.map_err(FrontendError::WriteError)
    }
}

/// Frontend wrapper for [`BodylessBodyWriter`].
pub struct FrontendBodylessBodyWriter<I> {
    inner: BodylessBodyWriter<I>,
}

impl<I: AsyncWriteExt + Unpin> FrontendBodylessBodyWriter<I> {
    pub fn finish(self) -> FrontendResult<FrontendWriter<I>> {
        let Self { inner } = self;
        Ok(FrontendWriter { io: inner.finish() })
    }
}

/// Frontend wrapper for [`ContentLengthBodyWriter`].
pub struct FrontendContentLengthBodyWriter<I> {
    inner: ContentLengthBodyWriter<I>,
}

impl<I: AsyncWriteExt + Unpin> FrontendContentLengthBodyWriter<I> {
    pub async fn write(&mut self, buf: &[u8]) -> FrontendResult<()> {
        let Self { inner } = self;
        inner
            .write(buf)
            .await
            .map_err(FrontendError::BodyWriteError)
    }

    pub async fn finish(self) -> FrontendResult<FrontendWriter<I>> {
        let Self { inner } = self;
        let io = inner
            .finish()
            .await
            .map_err(FrontendError::BodyWriteError)?;
        Ok(FrontendWriter { io })
    }
}

/// Frontend wrapper for [`ChunkedBodyWriter`]. Finishing it with trailers
/// forwards them to the frontend as chunked trailer fields.
pub struct FrontendChunkedBodyWriter<I> {
    inner: ChunkedBodyWriter<I>,
}

impl<I: AsyncWriteExt + Unpin> FrontendChunkedBodyWriter<I> {
    pub async fn write(&mut self, buf: &[u8]) -> FrontendResult<()> {
        let Self { inner } = self;
        inner
            .write(buf)
            .await
            .map_err(FrontendError::BodyWriteError)
    }

    pub async fn finish_with_trailers(
        self,
        trailers: &Headers,
    ) -> FrontendResult<FrontendWriter<I>> {
        let Self { inner } = self;
        let io = inner
            .finish_with_trailers(trailers)
            .await
            .map_err(FrontendError::BodyWriteError)?;
        Ok(FrontendWriter { io })
    }

    pub async fn finish(self) -> FrontendResult<FrontendWriter<I>> {
        self.finish_with_trailers(&Default::default()).await
    }

    pub async fn abort(self) -> FrontendResult<()> {
        let Self { inner } = self;
        inner.abort().await.map_err(FrontendError::BodyWriteError)?;
        Ok(())
    }
}

/// The body writer for a frontend connection; it is typed so that finishing,
/// when possible, can produce a [`FrontendWriter`] without exposing raw IO.
pub enum FrontendBodyWriter<I> {
    Bodyless(FrontendBodylessBodyWriter<I>),
    CL(FrontendContentLengthBodyWriter<I>),
    TE(FrontendChunkedBodyWriter<I>),
    Close(FrontendEofBodyWriter<I>),
}

impl<I: AsyncWriteExt + Unpin> FrontendBodyWriter<I> {
    pub async fn write(&mut self, buf: &[u8]) -> FrontendResult<()> {
        match self {
            FrontendBodyWriter::Bodyless(_) => Err(FrontendError::BodyWriteError(
                jrpxy_body::error::BodyError::BodyOverflow(buf.len() as u64),
            )),
            FrontendBodyWriter::CL(w) => w.write(buf).await,
            FrontendBodyWriter::TE(w) => w.write(buf).await,
            FrontendBodyWriter::Close(w) => w.write(buf).await,
        }
    }

    pub async fn finish(self) -> FrontendResult<Option<FrontendWriter<I>>> {
        match self {
            FrontendBodyWriter::Bodyless(w) => w.finish().map(Some),
            FrontendBodyWriter::CL(w) => w.finish().await.map(Some),
            FrontendBodyWriter::TE(w) => w.finish().await.map(Some),
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
            FrontendBodyWriter::TE(w) => w.abort().await,
        }
    }
}

#[cfg(test)]
mod test {
    use jrpxy_body::error::BodyError;
    use jrpxy_http_message::{message::ResponseBuilder, version::HttpVersion};
    use jrpxy_util::debug::AsciiDebug;

    use crate::{error::FrontendError, writer::FrontendWriter};

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
