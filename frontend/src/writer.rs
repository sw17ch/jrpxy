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
//!        .build();
//!    let mut frontend_body_writer = frontend_writer
//!        .send_as_content_length(&resp, 5)
//!        .await
//!        .unwrap();
//!    frontend_body_writer.write(b"ab").await.unwrap();
//!    frontend_body_writer.write(b"cde").await.unwrap();
//!    let frontend_writer = frontend_body_writer.finish().await.unwrap();
//!
//!    // Write the second response
//!    let mut builder = ResponseBuilder::new(4);
//!    let resp = builder
//!        .with_version(HttpVersion::Http11)
//!        .with_code(404)
//!        .with_reason("Not Found")
//!        .build();
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

use jrpxy_body::BodyWriterState;
use jrpxy_body::ChunkedBodyWriter;
use jrpxy_body::ContentLengthBodyWriter;
use jrpxy_body::is_framing_header;
use jrpxy_http_message::framing::HeadFraming;
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

impl<I: AsyncWriteExt + Unpin> FrontendWriter<I> {
    /// Create a new frontend writer that will write responses into the
    /// specified `io`.
    pub fn new(io: I) -> Self {
        Self { io }
    }

    /// Send the specified response with a chunked response body.
    pub async fn send_as_chunked(
        self,
        response: &Response,
    ) -> FrontendResult<FrontendBodyWriter<I>> {
        let Self { mut io } = self;
        write_response_to(response, HeadFraming::Chunked, &mut io)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(FrontendBodyWriter {
            io,
            state: BodyWriterState::TE(ChunkedBodyWriter::new()),
        })
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
        write_response_to(response, HeadFraming::Length(body_len), &mut io)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(FrontendBodyWriter {
            io,
            state: BodyWriterState::CL(ContentLengthBodyWriter::new(body_len)),
        })
    }

    /// Send the response to the frontend as bodyless. This is used most
    /// frequently as a response to a `HEAD` request. This will not remove any
    /// framing header sent from the origin, and will not add its own. This
    /// means that the request may specify a `content-length` or
    /// `transfer-encoding: chunked` header, and it will remain in place when
    /// forwarding to the frontend. If this is used with something like a
    /// 1xx-informational response, it is assumed the user has ensured that the
    /// response adheres to the standard (no content-length or transfer-encoding
    /// headers in the 1xx response, etc).
    pub async fn send_as_bodyless(
        self,
        response: &Response,
    ) -> Result<FrontendBodyWriter<I>, FrontendError> {
        let Self { mut io } = self;
        write_response_to(response, HeadFraming::NoFraming, &mut io)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(FrontendBodyWriter {
            io,
            state: BodyWriterState::CL(ContentLengthBodyWriter::new(0)),
        })
    }

    pub fn into_inner(self) -> I {
        let Self { io } = self;
        io
    }
}

pub(crate) async fn write_response_to<W: AsyncWriteExt + Unpin>(
    res: &Response,
    framing: HeadFraming,
    mut w: W,
) -> io::Result<()> {
    let version = res.version().to_static();
    let code = res.code();
    let reason = res.reason();

    // TODO: we can avoid this heap allocation
    let code = format!("{code}");

    // write out the status line
    // TODO: we should always send HTTP/1.1
    w.write_all(version.as_bytes()).await?;
    w.write_all(b" ").await?;
    w.write_all(code.as_bytes()).await?;
    w.write_all(b" ").await?;
    w.write_all(reason).await?;
    w.write_all(b"\r\n").await?;

    // if framing is specified, remove any existing framing header. otherwise,
    // we'll leave the framing header specified by the origin in place. this is
    // mostly useful for responses to HEAD requests.
    let headers = res
        .headers()
        .iter()
        .filter(|(n, _)| framing.is_no_framing() || !is_framing_header(n));

    // write out each header
    for (n, v) in headers {
        w.write_all(n).await?;
        w.write_all(b": ").await?;
        w.write_all(v).await?;
        w.write_all(b"\r\n").await?;
    }

    // add the framing header
    match framing {
        HeadFraming::NoFraming => {}
        HeadFraming::Length(l) => {
            // TODO: we can avoid this heap allocation
            let cl = format!("content-length: {l}\r\n");
            w.write_all(cl.as_bytes()).await?;
        }
        HeadFraming::Chunked => {
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

/// A frontend response body writer.
pub struct FrontendBodyWriter<I> {
    pub(crate) io: I,
    pub(crate) state: BodyWriterState,
}

impl<I: AsyncWriteExt + Unpin> FrontendBodyWriter<I> {
    pub async fn write(&mut self, buf: &[u8]) -> FrontendResult<()> {
        self.state
            .write(&mut self.io, buf)
            .await
            .map_err(FrontendError::BodyWriteError)
    }

    pub async fn finish(self) -> FrontendResult<FrontendWriter<I>> {
        let Self { mut io, state } = self;
        state
            .finish(&mut io)
            .await
            .map_err(FrontendError::BodyWriteError)?;
        Ok(FrontendWriter { io })
    }
}

#[cfg(test)]
mod test {
    use jrpxy_body::BodyError;
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
            .into();

        let mut buf = Vec::new();
        let cw = FrontendWriter::new(&mut buf);
        let r = cw.send_as_content_length(&res, 0).await.expect("can write");

        let buf = r.finish().await.expect("failed to finish").into_inner();

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

        let _buf = r.finish().await.expect("failed to finish").into_inner();
    }

    #[tokio::test]
    async fn frontend_writer_with_content_length_incomplete_body() {
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok".into())
            .with_version(HttpVersion::Http11)
            .build()
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
            .into();

        let mut buf = Vec::new();
        let cw = FrontendWriter::new(&mut buf);
        let mut r = cw.send_as_chunked(&res).await.expect("can write");

        r.write(&b"01234"[..]).await.expect("write works");
        r.write(&b"56789"[..]).await.expect("write works");
        r.write(&b""[..])
            .await
            .expect("works, but shouldn't generate a chunk");

        let _buf = r.finish().await.expect("failed to finish").into_inner();
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
            .into();
        let body_writer = cw
            .send_as_content_length(&res, 0)
            .await
            .expect("first write works");
        let cw = body_writer.finish().await.expect("failed to finish");

        // second response
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok".into())
            .with_version(HttpVersion::Http11)
            .with_header("x-req", b"second")
            .build()
            .into();
        let mut body_writer = cw
            .send_as_content_length(&res, 5)
            .await
            .expect("first write works");
        body_writer.write(b"01234").await.expect("failed to write");
        let cw = body_writer.finish().await.expect("failed to finish");

        // third response
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok".into())
            .with_version(HttpVersion::Http11)
            .with_header("x-req", b"third")
            .build()
            .into();
        let mut body_writer = cw.send_as_chunked(&res).await.expect("first write works");
        body_writer.write(b"01234").await.expect("failed to write");
        body_writer.write(b"5").await.expect("failed to write");
        body_writer.write(b"6789").await.expect("failed to write");
        let cw = body_writer.finish().await.expect("failed to finish");

        // check the output
        let output_buf = cw.into_inner().as_slice();

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
