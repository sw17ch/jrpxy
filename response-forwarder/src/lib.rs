use jrpxy_backend::{BackendBodyReader, BackendReader, BackendResponse};
use jrpxy_body::BodyReadMode;
use jrpxy_frontend::{FrontendBodyWriter, FrontendWriter};
use jrpxy_http_message::message::InformationalResponse;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, thiserror::Error)]
pub enum ResponseForwarderError {
    #[error("Frontend error: {0}")]
    Frontend(#[from] jrpxy_frontend::FrontendError),
    #[error("Backend error: {0}")]
    Backend(#[from] jrpxy_backend::BackendError),
}

pub type ResponseForwarderResult<T> = Result<T, ResponseForwarderError>;

pub struct ResponseForwarder<R> {
    response: BackendResponse<R>,
}

impl<I> ResponseForwarder<I> {
    pub fn new(response: BackendResponse<I>) -> Self {
        Self { response }
    }
}

impl<R: AsyncReadExt + Unpin> ResponseForwarder<R> {
    /// Forward the request body to the specified backend using the specified
    /// backend request. On successful completion, the frontend reader and
    /// backend writer will be fully drained and flushed respectively. The
    /// returned frontend reader and backend writer are both ready to handle the
    /// next request in the pipeline.
    pub async fn forward_to<W: AsyncWriteExt + Unpin>(
        self,
        frontend_writer: FrontendWriter<W>,
    ) -> ResponseForwarderResult<ResponseBodyForwarder<R, W>> {
        let Self { response } = self;

        let (response, reader) = response.into_parts();

        let r = match reader.mode() {
            BodyReadMode::Chunk => frontend_writer.send_as_chunked(&response).await,
            BodyReadMode::ContentLength(cl) => {
                frontend_writer.send_as_content_length(&response, cl).await
            }
            BodyReadMode::Bodyless => frontend_writer.send_as_bodyless(&response).await,
        };

        let writer = r?;

        Ok(ResponseBodyForwarder { reader, writer })
    }
}

pub struct ResponseBodyForwarder<R, W> {
    reader: BackendBodyReader<R>,
    writer: FrontendBodyWriter<W>,
}

impl<R: AsyncReadExt + Unpin, W: AsyncWriteExt + Unpin> ResponseBodyForwarder<R, W> {
    pub async fn forward_body(
        self,
    ) -> ResponseForwarderResult<(BackendReader<R>, FrontendWriter<W>)> {
        let Self {
            mut reader,
            mut writer,
        } = self;
        while let Some(buf) = reader.read(8192).await? {
            writer.write(&buf).await?;
        }
        let reader = reader.drain().await?;
        let writer = writer.finish().await?;
        Ok((reader, writer))
    }
}

pub struct InformationalResponseForwarder {
    response: InformationalResponse,
}

impl InformationalResponseForwarder {
    pub fn new(response: InformationalResponse) -> Self {
        Self { response }
    }

    /// Forward an informational response to the frontend. Informational
    /// responses, as defined by RFC9110, cannot contain content or trailers.
    /// They end immediately after the header section.
    pub async fn forward_informational_to<W: AsyncWriteExt + Unpin>(
        self,
        frontend_writer: FrontendWriter<W>,
    ) -> ResponseForwarderResult<FrontendWriter<W>> {
        let Self { response } = self;
        let response = response.into_inner();
        let writer = frontend_writer.send_as_bodyless(&response).await?;
        let frontend_writer = writer.finish().await?;
        Ok(frontend_writer)
    }
}

#[cfg(test)]
mod test {
    use jrpxy_backend::BackendReader;
    use jrpxy_frontend::FrontendWriter;
    use jrpxy_util::debug::AsciiDebug;

    use crate::ResponseForwarder;

    #[tokio::test]
    async fn forward() {
        let backend_response_buf = b"\
            HTTP/1.1 200 Ok\r\n\
            content-length: 5\r\n\
            \r\n\
            01234\
            ";
        let mut frontend_response_buf = Vec::new();

        let backend_reader = BackendReader::new(&backend_response_buf[..], 128);
        let frontend_writer = FrontendWriter::new(&mut frontend_response_buf);
        let backend_response = backend_reader
            .read(true)
            .await
            .expect("failed to read response head")
            .try_into_response()
            .unwrap();

        let f = ResponseForwarder::new(backend_response);
        let r = f
            .forward_to(frontend_writer)
            .await
            .expect("could not forward response");
        let (_backend_reader, _frontend_writer) = r
            .forward_body()
            .await
            .expect("could not forward response body");

        assert_eq!(
            AsciiDebug(&backend_response_buf[..]),
            AsciiDebug(&frontend_response_buf)
        );
    }

    #[tokio::test]
    async fn forward_bodyless_cl() {
        let backend_response_buf = b"\
            HTTP/1.1 200 Ok\r\n\
            content-length: 5\r\n\
            \r\n\
            01234\
            ";
        let mut frontend_response_buf = Vec::new();

        let backend_reader = BackendReader::new(&backend_response_buf[..], 128);
        let frontend_writer = FrontendWriter::new(&mut frontend_response_buf);
        let backend_response = backend_reader
            .read(false)
            .await
            .expect("failed to read response head")
            .try_into_response()
            .unwrap();

        let f = ResponseForwarder::new(backend_response);
        let r = f
            .forward_to(frontend_writer)
            .await
            .expect("could not forward response");
        let (_backend_reader, _frontend_writer) = r
            .forward_body()
            .await
            .expect("could not forward response body");

        let backend_response_no_body_buf = b"\
            HTTP/1.1 200 Ok\r\n\
            content-length: 5\r\n\
            \r\n\
            ";

        assert_eq!(
            AsciiDebug(&backend_response_no_body_buf[..]),
            AsciiDebug(&frontend_response_buf)
        );
    }

    #[tokio::test]
    async fn forward_bodyless_chunked() {
        let backend_response_buf = b"\
            HTTP/1.1 200 Ok\r\n\
            transfer-encoding: chunked\r\n\
            \r\n\
            5\r\n
            01234\r\n\
            0\r\n\
            \r\n\
            ";
        let mut frontend_response_buf = Vec::new();

        let backend_reader = BackendReader::new(&backend_response_buf[..], 128);
        let frontend_writer = FrontendWriter::new(&mut frontend_response_buf);
        let backend_response = backend_reader
            .read(false)
            .await
            .expect("failed to read response head")
            .try_into_response()
            .unwrap();

        let f = ResponseForwarder::new(backend_response);
        let r = f
            .forward_to(frontend_writer)
            .await
            .expect("could not forward response");
        let (_backend_reader, _frontend_writer) = r
            .forward_body()
            .await
            .expect("could not forward response body");

        let backend_response_no_body_buf = b"\
            HTTP/1.1 200 Ok\r\n\
            transfer-encoding: chunked\r\n\
            \r\n\
            ";

        assert_eq!(
            AsciiDebug(&backend_response_no_body_buf[..]),
            AsciiDebug(&frontend_response_buf)
        );
    }
}
