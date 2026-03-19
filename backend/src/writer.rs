pub mod body;
pub mod bodyless;
pub mod chunked;
pub mod content_length;

use jrpxy_body::writer::{
    bodyless as body_bodyless, chunked as body_chunked, content_length as body_content_length,
};
use jrpxy_http_message::{
    framing::{WriteFraming, is_framing_header},
    message::Request,
};
use tokio::io::{self, AsyncWrite, AsyncWriteExt};

use crate::error::{BackendError, BackendResult};

/// Connection-level writer. Each `send_as_*` writes the request head and
/// hands back the per-encoding body-writer `Ready` for the chosen
/// framing.
#[derive(Debug)]
pub struct BackendWriter<I> {
    writer: I,
}

impl<I> BackendWriter<I> {
    pub fn new(writer: I) -> Self {
        Self { writer }
    }

    pub fn into_inner(self) -> I {
        let Self { writer } = self;
        writer
    }
}

impl<I> BackendWriter<I>
where
    I: AsyncWrite + Unpin,
{
    pub async fn send_as_chunked(self, request: &Request) -> BackendResult<chunked::Ready<I>> {
        let Self { mut writer } = self;
        write_request_to(request, WriteFraming::Chunked, &mut writer)
            .await
            .map_err(BackendError::WriteError)?;
        Ok(chunked::Ready::new(body_chunked::Ready::new(writer)))
    }

    pub async fn send_as_content_length(
        self,
        request: &Request,
        body_len: u64,
    ) -> BackendResult<content_length::Ready<I>> {
        let Self { mut writer } = self;
        write_request_to(request, WriteFraming::Length(body_len), &mut writer)
            .await
            .map_err(BackendError::WriteError)?;
        Ok(content_length::Ready::new(body_content_length::Ready::new(
            writer, body_len,
        )))
    }

    /// Send the request to the backend without a body. There is no mechanism
    /// for specifying `content-length` or `transfer-encoding: chunked` on a
    /// request, so we just elide any framing headers.
    pub async fn send_as_bodyless(self, request: &Request) -> BackendResult<bodyless::Ready<I>> {
        let Self { mut writer } = self;
        write_request_to(request, WriteFraming::StripFraming, &mut writer)
            .await
            .map_err(BackendError::WriteError)?;
        Ok(bodyless::Ready::new(body_bodyless::Ready::new(writer)))
    }
}

async fn write_request_to<W>(req: &Request, framing: WriteFraming, mut w: W) -> io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    // PreserveFraming is a response-writer concept (e.g. 304 Not Modified
    // quoting the framing of the would-be 200 response). The proxy is
    // authoritative over the framing of the request it sends to the backend,
    // so passing client framing headers through verbatim is never correct on
    // this path.
    debug_assert!(
        !matches!(framing, WriteFraming::PreserveFraming),
        "PreserveFraming is not valid when writing requests to a backend"
    );

    let method = req.method();
    let path = req.path();

    // TODO: write vectored

    // write out the request line
    w.write_all(method).await?;
    w.write_all(b" ").await?;
    w.write_all(path).await?;
    w.write_all(b" ").await?;
    // we always send HTTP/1.1 from the proxy even if the client or server is
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
    // just the head in case this writer is buffered. Many servers can start
    // processing the response without waiting for any of the body.
    w.flush().await?;

    Ok(())
}

#[cfg(test)]
mod test {
    use jrpxy_body::error::BodyError;
    use jrpxy_http_message::{message::RequestBuilder, version::HttpVersion};

    use crate::writer::{BackendWriter, body};

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
            .expect("failed to build");

        let ready = writer
            .send_as_content_length(&req, 0)
            .await
            .expect("failed to send");

        let err = ready.open(5).expect_err("open should overflow");
        assert!(matches!(err, BodyError::BodyOverflow(0)));
    }

    #[tokio::test]
    async fn write_cl_backend_finish() {
        let mut buf = Vec::new();
        let writer = BackendWriter::new(&mut buf);

        let mut b = RequestBuilder::new(8);
        let req = b
            .with_method("GET")
            .with_path("/cl")
            .with_version(HttpVersion::Http11)
            .with_header("hello", b"world")
            .build()
            .expect("failed to build");

        let ready = writer
            .send_as_content_length(&req, 0)
            .await
            .expect("failed to send");

        let backend = ready
            .finish()
            .expect("finish")
            .finish()
            .await
            .expect("flush");
        let buf = backend.into_inner();

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
            .expect("failed to build");

        let ready = writer.send_as_chunked(&req).await.expect("failed to send");

        // drive several chunks of varying sizes through the unifier API.
        let ready: body::Ready<_> = ready.into();
        let ready = drive_chunk(ready, b"01234").await;
        let ready = drive_chunk(ready, b"56").await;
        let ready = drive_chunk(ready, b"7").await;
        let ready = drive_chunk(ready, b"8").await;

        let backend = ready
            .finish()
            .expect("finish")
            .finish()
            .await
            .expect("flush");
        let buf = backend.into_inner();

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

    /// Drive a single `Open(N) -> Write -> Close -> Ready` cycle on the
    /// unifier.
    async fn drive_chunk<I>(ready: body::Ready<I>, data: &[u8]) -> body::Ready<I>
    where
        I: tokio::io::AsyncWrite + Unpin,
    {
        let open = ready.open(data.len() as u64).expect("open");
        let mut write = open.open().await.expect("open async");
        write.write_all(data).await.expect("write_all");
        let close = write.into_close().expect("into_close");
        close.close().await.expect("close")
    }
}
