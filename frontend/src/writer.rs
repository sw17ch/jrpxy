//! Tools for writing responses to a HTTP/1.x frontend.
//!
//! Each `send_as_*` method on [`FrontendWriter`] writes the response
//! head and hands back a per-encoding [`Ready`](body::Ready)-shaped
//! state. The state machine for each encoding mirrors the body crate
//! shape (`Ready -> Open -> Write -> Close -> Ready`, terminating in
//! `Finish` or `Abort`). The encoding-generic [`body`] module wraps the
//! four per-encoding states in an enum that exposes only their common
//! surface; chunked-only operations such as trailer support live on
//! [`chunked::Ready`] directly.
//!
//! ```rust
//! use jrpxy_frontend::writer::FrontendWriter;
//! use jrpxy_http_message::{message::ResponseBuilder, version::HttpVersion};
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() {
//!     let mut buf = Vec::new();
//!     let frontend_writer = FrontendWriter::new(&mut buf);
//!
//!     let mut builder = ResponseBuilder::new(4);
//!     let resp = builder
//!         .with_version(HttpVersion::Http11)
//!         .with_code(200)
//!         .with_reason("Ok")
//!         .build()
//!         .expect("failed to build")
//!         .into();
//!
//!     // send the response head with content-length: 5
//!     let ready = frontend_writer
//!         .send_as_content_length(&resp, 5)
//!         .await
//!         .expect("failed to send");
//!
//!     // drive a single Open/Write/Close cycle of 5 bytes
//!     let open = ready.open(5).expect("open");
//!     let mut write = open.open().await.expect("open async");
//!     write.write_all(b"abcde").await.expect("write_all");
//!     let close = write.into_close().expect("into_close");
//!     let ready = close.close().await.expect("close");
//!
//!     // terminate cleanly
//!     let _frontend_writer = ready
//!         .finish()
//!         .expect("finish")
//!         .finish()
//!         .await
//!         .expect("flush");
//!
//!     assert_eq!(
//!         &b"HTTP/1.1 200 Ok\r\ncontent-length: 5\r\n\r\nabcde"[..],
//!         &buf[..]
//!     );
//! }
//! ```

pub mod body;
pub mod bodyless;
pub mod chunked;
pub mod content_length;
pub mod eof;

use jrpxy_body::writer::{
    bodyless as body_bodyless, chunked as body_chunked, content_length as body_content_length,
    eof as body_eof,
};
use jrpxy_http_message::{
    framing::{WriteFraming, is_framing_header},
    message::Response,
};
use tokio::io::{self, AsyncWrite, AsyncWriteExt};

use crate::error::{FrontendError, FrontendResult};

/// Connection-level writer. Each `send_as_*` writes the response head
/// and hands back the per-encoding body-writer `Ready` for the chosen
/// framing.
#[derive(Debug)]
pub struct FrontendWriter<I> {
    writer: I,
}

impl<I> FrontendWriter<I> {
    /// Create a new [`FrontendWriter`].
    pub fn new(writer: I) -> Self {
        Self { writer }
    }

    /// Convert back to the contained writer.
    pub fn into_inner(self) -> I {
        let Self { writer } = self;
        writer
    }
}

impl<I> FrontendWriter<I>
where
    I: AsyncWrite + Unpin,
{
    /// Send the response head with a body terminated by connection close (RFC
    /// 9112 section 6.3). Any framing headers from the origin are stripped. Use
    /// this when the client cannot decode chunked transfer encoding (e.g.
    /// HTTP/1.0).
    ///
    /// The returned [`eof::Ready`] cannot be recycled into a
    /// [`FrontendWriter`]; the connection must be closed after the body is
    /// sent.
    ///
    /// Framing headers will be stripped from `response`.
    pub async fn send_as_eof(self, response: &Response) -> FrontendResult<eof::Ready<I>> {
        let Self { mut writer } = self;
        write_response_to(response, WriteFraming::StripFraming, &mut writer)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(eof::Ready::new(body_eof::Ready::new(writer)))
    }

    /// Send the response head with a chunked body (RFC9112 section 7.1).
    ///
    /// Framing headers will be stripped from `response`, and replaced with
    /// `transfer-encoding: chunked`.
    pub async fn send_as_chunked(self, response: &Response) -> FrontendResult<chunked::Ready<I>> {
        let Self { mut writer } = self;
        write_response_to(response, WriteFraming::Chunked, &mut writer)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(chunked::Ready::new(body_chunked::Ready::new(writer)))
    }

    /// Send the response head with a `content-length`-delimited body.
    ///
    /// Framing headers will be stripped from `response`, and replaced with
    /// `content-length: <body_len>`.
    pub async fn send_as_content_length(
        self,
        response: &Response,
        body_len: u64,
    ) -> FrontendResult<content_length::Ready<I>> {
        let Self { mut writer } = self;
        write_response_to(response, WriteFraming::Length(body_len), &mut writer)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(content_length::Ready::new(body_content_length::Ready::new(
            writer, body_len,
        )))
    }

    /// Send the response head with no body, preserving any framing headers.
    ///
    /// Use this for responses to `HEAD` requests and `304 Not Modified`
    /// responses, where framing headers describe the representation that
    /// *would* have been sent rather than an actual body.
    ///
    /// Framing headers in `response` will be preserved.
    pub async fn send_as_bodyless_keep_framing(
        self,
        response: &Response,
    ) -> FrontendResult<bodyless::Ready<I>> {
        let Self { mut writer } = self;
        write_response_to(response, WriteFraming::PreserveFraming, &mut writer)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(bodyless::Ready::new(body_bodyless::Ready::new(writer)))
    }

    /// Send the response head with no body, stripping any framing
    /// headers from the origin.
    ///
    /// Use this for responses that must never carry a body by
    /// definition: `1xx` informational responses and `204 No Content`.
    /// Forwarding a `content-length` on these would corrupt the client's
    /// parser.
    ///
    /// Framing headers in `response` will be stripped.
    pub async fn send_as_no_content(
        self,
        response: &Response,
    ) -> FrontendResult<bodyless::Ready<I>> {
        let Self { mut writer } = self;
        write_response_to(response, WriteFraming::StripFraming, &mut writer)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(bodyless::Ready::new(body_bodyless::Ready::new(writer)))
    }
}

async fn write_response_to<W>(res: &Response, framing: WriteFraming, mut w: W) -> io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
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

    // PreserveFraming leaves the origin's framing headers in place (HEAD /
    // 304). All other variants strip them and optionally append a new one.
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

    // add the framing header, if any
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

    // flush the response head so that the client has a chance to start
    // processing the response without waiting for any of the body. Many
    // clients can act on just the head.
    w.flush().await?;

    Ok(())
}

#[cfg(test)]
mod test {
    use jrpxy_body::error::BodyError;
    use jrpxy_http_message::{message::ResponseBuilder, version::HttpVersion};
    use jrpxy_util::debug::AsciiDebug;
    use tokio::io::AsyncWrite;

    use crate::writer::{FrontendWriter, body};

    /// Drive a single `Open(N) -> Write -> Close -> Ready` cycle on the
    /// unifier.
    async fn drive_chunk<I>(ready: body::Ready<I>, data: &[u8]) -> body::Ready<I>
    where
        I: AsyncWrite + Unpin,
    {
        let open = ready.open(data.len() as u64).expect("open");
        let mut write = open.open().await.expect("open async");
        write.write_all(data).await.expect("write_all");
        let close = write.into_close().expect("into_close");
        close.close().await.expect("close")
    }

    #[tokio::test]
    async fn frontend_writer() {
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok")
            .with_version(HttpVersion::Http11)
            .with_header("x-hello", &b"World"[..])
            .with_header("content-LENGTH", &b"400"[..])
            .with_header("TRANSFER-ENCODING", &b"chunked"[..])
            .build()
            .expect("failed to build response");

        let mut buf = Vec::new();
        let cw = FrontendWriter::new(&mut buf);
        let ready = cw.send_as_content_length(&res, 0).await.expect("can write");

        let buf = ready
            .finish()
            .expect("finish")
            .finish()
            .await
            .expect("flush")
            .into_inner();

        // we strip the origin's framing headers and emit our own:
        // - content-length is replaced with 0
        // - transfer-encoding is dropped
        // - the unrelated header is preserved
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
            .with_reason("Ok")
            .with_version(HttpVersion::Http11)
            .build()
            .expect("failed to build response");

        let mut buf = Vec::new();
        let cw = FrontendWriter::new(&mut buf);
        let ready = cw.send_as_content_length(&res, 0).await.expect("can write");

        let err = ready.open(5).expect_err("open should overflow");
        assert!(matches!(err, BodyError::BodyOverflow(0)));
    }

    // TODO: boundary tests around u64::MAX

    #[tokio::test]
    async fn frontend_writer_with_content_length() {
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok")
            .with_version(HttpVersion::Http11)
            .build()
            .expect("failed to build response");

        let mut buf = Vec::new();
        let cw = FrontendWriter::new(&mut buf);
        let ready: body::Ready<_> = cw
            .send_as_content_length(&res, 10)
            .await
            .expect("can write")
            .into();

        let ready = drive_chunk(ready, b"01234").await;
        let ready = drive_chunk(ready, b"56789").await;

        let err = ready.open(1).expect_err("open should overflow");
        assert!(matches!(
            err,
            crate::error::FrontendError::BodyWriteError(BodyError::BodyOverflow(10))
        ));
    }

    #[tokio::test]
    async fn frontend_writer_with_content_length_incomplete_body() {
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok")
            .with_version(HttpVersion::Http11)
            .build()
            .expect("failed to build response");

        let mut buf = Vec::new();
        let cw = FrontendWriter::new(&mut buf);
        let ready: body::Ready<_> = cw
            .send_as_content_length(&res, 10)
            .await
            .expect("can write")
            .into();

        // do not write all 10 bytes, and then try to finish. finishing will
        // fail.
        let ready = drive_chunk(ready, b"01234").await;

        let err = ready.finish().expect_err("finish should error");
        assert!(matches!(
            err,
            crate::error::FrontendError::BodyWriteError(BodyError::IncompleteBody {
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
            .with_reason("Ok")
            .with_version(HttpVersion::Http11)
            .build()
            .expect("failed to build response");

        let mut buf = Vec::new();
        let cw = FrontendWriter::new(&mut buf);
        let ready: body::Ready<_> = cw.send_as_chunked(&res).await.expect("can write").into();

        let ready = drive_chunk(ready, b"01234").await;
        let ready = drive_chunk(ready, b"56789").await;

        let _frontend = ready
            .finish()
            .expect("finish")
            .finish()
            .await
            .expect("flush")
            .expect("not eof");
    }

    #[tokio::test]
    async fn frontend_writer_pipeline() {
        let mut output_buf = Vec::with_capacity(4096);
        let cw = FrontendWriter::new(&mut output_buf);

        // first response: empty CL body
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok")
            .with_version(HttpVersion::Http11)
            .with_header("x-req", b"first")
            .build()
            .expect("failed to build response");
        let ready = cw
            .send_as_content_length(&res, 0)
            .await
            .expect("first write works");
        // The Finish wrapper's `finish().await?` hands back the
        // FrontendWriter directly, ready for the next response.
        let cw = ready
            .finish()
            .expect("finish")
            .finish()
            .await
            .expect("flush");

        // second response: CL body of 5 bytes
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok")
            .with_version(HttpVersion::Http11)
            .with_header("x-req", b"second")
            .build()
            .expect("failed to build response");
        let ready: body::Ready<_> = cw
            .send_as_content_length(&res, 5)
            .await
            .expect("second write works")
            .into();
        let ready = drive_chunk(ready, b"01234").await;
        let cw = ready
            .finish()
            .expect("finish")
            .finish()
            .await
            .expect("flush")
            .expect("not eof");

        // third response: chunked
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok")
            .with_version(HttpVersion::Http11)
            .with_header("x-req", b"third")
            .build()
            .expect("failed to build response");
        let ready: body::Ready<_> = cw
            .send_as_chunked(&res)
            .await
            .expect("third write works")
            .into();
        let ready = drive_chunk(ready, b"01234").await;
        let ready = drive_chunk(ready, b"5").await;
        let ready = drive_chunk(ready, b"6789").await;
        let cw = ready
            .finish()
            .expect("finish")
            .finish()
            .await
            .expect("flush")
            .expect("not eof");

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

    #[tokio::test]
    async fn frontend_writer_eof() {
        let mut buf = Vec::new();
        let cw = FrontendWriter::new(&mut buf);

        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok")
            .with_version(HttpVersion::Http11)
            .build()
            .expect("failed to build response");

        // EOF is not addressable through the unifier's recycle path - the
        // unifier's Finish::into_writer returns None for EOF.
        let ready: body::Ready<_> = cw.send_as_eof(&res).await.expect("can write").into();
        let ready = drive_chunk(ready, b"hello").await;

        let recycled = ready
            .finish()
            .expect("finish")
            .finish()
            .await
            .expect("flush");
        assert!(recycled.is_none(), "EOF must not yield a recyclable writer");

        let expected = b"\
            HTTP/1.1 200 Ok\r\n\
            \r\n\
            hello\
        ";
        assert_eq!(AsciiDebug(expected), AsciiDebug(&buf[..]));
    }
}
