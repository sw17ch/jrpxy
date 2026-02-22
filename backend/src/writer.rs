use jrpxy_body::{BodyWriterState, ChunkedBodyWriter, ContentLengthBodyWriter, is_framing_header};
use jrpxy_http_message::{framing::HeadFraming, message::Request};
use tokio::io::{self, AsyncWriteExt};

use crate::error::{BackendError, BackendResult};

pub struct BackendWriter<I> {
    io: I,
}
impl<I: AsyncWriteExt + Unpin> BackendWriter<I> {
    pub fn new(io: I) -> Self {
        Self { io }
    }

    pub async fn send_as_chunked(self, request: &Request) -> BackendResult<BackendBodyWriter<I>> {
        let Self { mut io } = self;
        write_request_to(request, HeadFraming::Chunked, &mut io)
            .await
            .map_err(BackendError::WriteError)?;
        Ok(BackendBodyWriter {
            io,
            state: BodyWriterState::TE(ChunkedBodyWriter::new()),
        })
    }

    pub async fn send_as_content_length(
        self,
        request: &Request,
        body_len: u64,
    ) -> BackendResult<BackendBodyWriter<I>> {
        let Self { mut io } = self;
        write_request_to(request, HeadFraming::Length(body_len), &mut io)
            .await
            .map_err(BackendError::WriteError)?;
        Ok(BackendBodyWriter {
            io,
            state: BodyWriterState::CL(ContentLengthBodyWriter::new(body_len)),
        })
    }

    // TODO: we need to support not sending a content-length header at all for
    // things like 3xx, 204, and cases where the origin responds to a HEAD
    // without specifying a content-length.

    /// Send the response to the backend as bodyless. There's no mechanism for
    /// specifying `content-length` or `transfer-encoding: chunked` on a
    /// request, so in this case, we just elide any framing headers.
    pub async fn send_as_bodyless(
        self,
        request: &Request,
    ) -> Result<BackendBodyWriter<I>, BackendError> {
        let Self { mut io } = self;
        write_request_to(request, HeadFraming::NoFraming, &mut io)
            .await
            .map_err(BackendError::WriteError)?;
        Ok(BackendBodyWriter {
            io,
            state: BodyWriterState::CL(ContentLengthBodyWriter::new(0)),
        })
    }

    pub fn into_inner(self) -> I {
        let Self { io } = self;
        io
    }
}

async fn write_request_to<W: AsyncWriteExt + Unpin>(
    req: &Request,
    framing: HeadFraming,
    mut w: W,
) -> io::Result<()> {
    let method = req.method();
    let path = req.path();
    let version = req.version().to_static();

    // TODO: write vectored

    // write out the request line
    w.write_all(method).await?;
    w.write_all(b" ").await?;
    w.write_all(path).await?;
    w.write_all(b" ").await?;
    // TODO: we should always send HTTP/1.1
    w.write_all(version.as_bytes()).await?;
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
        HeadFraming::NoFraming => {}
        HeadFraming::Length(l) => {
            let cl = format!("content-length: {l}\r\n");
            w.write_all(cl.as_bytes()).await?;
        }
        HeadFraming::Chunked => {
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

pub struct BackendBodyWriter<I> {
    io: I,
    state: BodyWriterState,
}

impl<I: AsyncWriteExt + Unpin> BackendBodyWriter<I> {
    pub async fn write(&mut self, buf: &[u8]) -> BackendResult<()> {
        self.state
            .write(&mut self.io, buf)
            .await
            .map_err(BackendError::BodyWriteError)
    }

    pub async fn abort(self) -> BackendResult<()> {
        let Self { mut io, state } = self;
        state
            .abort(&mut io)
            .await
            .map_err(BackendError::BodyWriteError)?;
        Ok(())
    }

    // TODO: add an "abort" that explicitly abandons the write. for
    // chunk-encoding, we probably want this to write out an invalid chunk.

    pub async fn finish(self) -> BackendResult<BackendWriter<I>> {
        let Self { mut io, state } = self;
        state
            .finish(&mut io)
            .await
            .map_err(BackendError::BodyWriteError)?;
        Ok(BackendWriter { io })
    }
}

#[cfg(test)]
mod test {
    use jrpxy_body::BodyError;
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
