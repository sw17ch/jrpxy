use jrpxy_backend::{BackendBodyWriter, BackendWriter};
use jrpxy_body::BodyReadMode;
use jrpxy_frontend::{FrontendBodyReader, FrontendReader, FrontendRequest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, thiserror::Error)]
pub enum RequestForwarderError {
    #[error("Frontend error: {0}")]
    Frontend(#[from] jrpxy_frontend::FrontendError),
    #[error("Backend error: {0}")]
    Backend(#[from] jrpxy_backend::BackendError),
}

pub type RequestForwarderResult<T> = Result<T, RequestForwarderError>;

pub struct RequestForwarder<I> {
    request: FrontendRequest<I>,
}

impl<I> RequestForwarder<I> {
    pub fn new(request: FrontendRequest<I>) -> Self {
        // TODO: remove framing headers from request
        Self { request }
    }
}

impl<R: AsyncReadExt + Unpin> RequestForwarder<R> {
    pub async fn try_forward_to<W: AsyncWriteExt + Unpin>(
        self,
        backend_writer: BackendWriter<W>,
    ) -> Result<RequestBodyForwarder<R, W>, (Self, RequestForwarderError)> {
        // If the backend write fails before we drain any of the body from
        // the frontend reader, then we could retry with a different backend
        // writer. This means we would like to be able to recover the client
        // reader when the backend fails before writing any of the body.

        let Self { request } = self;

        let r = match request.mode() {
            BodyReadMode::Chunk => backend_writer.send_as_chunked(request.req()).await,
            BodyReadMode::ContentLength(cl) => {
                backend_writer
                    .send_as_content_length(request.req(), cl)
                    .await
            }
            BodyReadMode::Bodyless => backend_writer.send_as_bodyless(request.req()).await,
        };

        match r {
            Ok(writer) => {
                let (_request, reader) = request.into_parts();
                Ok(RequestBodyForwarder { reader, writer })
            }
            Err(e) => Err((Self { request }, e.into())),
        }
    }

    /// Forward the request body to the specified backend using the specified
    /// backend request. On successful completion, the frontend reader and
    /// backend writer will be fully drained and flushed respectively. The
    /// returned frontend reader and backend writer are both ready to handle the
    /// next request in the pipeline.
    pub async fn forward_to<W: AsyncWriteExt + Unpin>(
        self,
        backend_writer: BackendWriter<W>,
    ) -> RequestForwarderResult<RequestBodyForwarder<R, W>> {
        self.try_forward_to(backend_writer)
            .await
            .map_err(|(_, e)| e)
    }

    pub fn into_inner(self) -> FrontendRequest<R> {
        // this is only safe as long as this type never modifies the request.
        // if, at any point, the request has had bytes drained out of the body,
        // this is no longer safe.
        let Self { request } = self;
        request
    }
}

pub struct RequestBodyForwarder<R, W> {
    reader: FrontendBodyReader<R>,
    writer: BackendBodyWriter<W>,
}

impl<R: AsyncReadExt + Unpin, W: AsyncWriteExt + Unpin> RequestBodyForwarder<R, W> {
    pub async fn forward_body(
        self,
    ) -> RequestForwarderResult<(FrontendReader<R>, BackendWriter<W>)> {
        let Self {
            mut reader,
            mut writer,
        } = self;

        // TODO: figure out a way to forward trailers

        while let Some(buf) = reader.read(8192).await? {
            writer.write(&buf).await?;
        }
        let reader = reader.drain().await?;
        let writer = writer.finish().await?;
        Ok((reader, writer))
    }
}

#[cfg(test)]
mod test {
    use jrpxy_backend::BackendWriter;
    use jrpxy_frontend::FrontendReader;
    use jrpxy_util::debug::AsciiDebug;

    use crate::RequestForwarder;

    #[tokio::test]
    async fn forward() {
        let frontend_request_buf = b"\
            GET / HTTP/1.1\r\n\
            content-length: 5\r\n\
            \r\n\
            01234\
            ";
        let mut backend_request_buf = Vec::new();

        let frontend_reader = FrontendReader::new(&frontend_request_buf[..], 128);
        let backend_writer = BackendWriter::new(&mut backend_request_buf);
        let frontend_request = frontend_reader
            .read()
            .await
            .expect("failed to read request head");

        let f = RequestForwarder::new(frontend_request);
        let r = f
            .forward_to(backend_writer)
            .await
            .expect("could not forward request");
        let (_frontend_reader, _backend_writer) = r
            .forward_body()
            .await
            .expect("could not forward request body");

        assert_eq!(
            AsciiDebug(&frontend_request_buf[..]),
            AsciiDebug(&backend_request_buf)
        );
    }
}
