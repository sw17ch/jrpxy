use std::io;

use bytes::{Bytes, BytesMut};
use jrpxy_body::{
    BodyError, BodyReadMode, BodyReader, BodyWriterState, ChunkedBodyWriter,
    ContentLengthBodyWriter, is_framing_header,
};
use jrpxy_http_message::{
    framing::HeadFraming,
    header::HeaderError,
    message::{MessageError, Request, Response},
    version::HttpVersion,
};
use jrpxy_util::buffer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("Failed to write to backend: {0}")]
    WriteError(io::Error),
    #[error("Failed to write body to backend: {0}")]
    BodyWriteError(BodyError),
    #[error("Failed to read backend: {0}")]
    ReadError(io::Error),
    #[error("Failed to read body: {0}")]
    BodyReadError(BodyError),
    #[error("First read failed")]
    FirstReadEOF,
    #[error("Unexpected end of file while reading")]
    UnexpectedEOF,
    #[error("Failed to parse response: {0}")]
    HttpResponseParseError(MessageError),
    #[error("Header error: {0}")]
    HeaderError(#[from] HeaderError),
    #[error("101-switching-protocols unsupported")]
    HttpSwitchingProtocolsUnsupported,
    #[error("102-processing deprecated")]
    HttpProcessingUnsupported,
    #[error("Unsupported unknown informational status: {0}")]
    HttpUnsupportedInformational(u16),
    #[error("Response head exceeded size limit: {0} >= {1}")]
    MaxHeadLenExceeded(usize, usize),
    #[error("1xx response contains framing headers")]
    FramingHeadersOnInformationalResposne,
    #[error("204 No Content response contains framing headers")]
    FramingHeadersOn204NoContentResposne,
}

pub type BackendResult<T> = Result<T, BackendError>;

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

pub struct BackendReader<I> {
    io: I,
    max_head_length: usize,
    buffer: buffer::Buffer,
}

impl<I: AsyncReadExt + Unpin> BackendReader<I> {
    const MAX_HEADERS: usize = 256;

    pub fn new(io: I, max_head_length: usize) -> Self {
        Self::new_with_buffer(io, max_head_length, BytesMut::new())
    }

    fn new_with_buffer(io: I, max_head_length: usize, buffer: BytesMut) -> BackendReader<I> {
        Self {
            io,
            max_head_length,
            buffer: buffer::Buffer::new(buffer),
        }
    }

    async fn head(&mut self) -> BackendResult<Response> {
        loop {
            if let Some(res) = Response::parse(&mut self.buffer, Self::MAX_HEADERS)
                .map_err(BackendError::HttpResponseParseError)?
            {
                return Ok(res);
            } else if self.buffer.len() >= self.max_head_length {
                return Err(BackendError::MaxHeadLenExceeded(
                    self.buffer.len(),
                    self.max_head_length,
                ));
            }

            let first_read = self.buffer.is_empty();

            // read some data into the buffer
            let target_read_len = self.max_head_length.saturating_sub(self.buffer.len());
            let len = self
                .buffer
                .read_from(&mut self.io, target_read_len)
                .await
                .map_err(BackendError::ReadError)?;
            if 0 == len {
                return if first_read {
                    // if the first attempt to read data into the buffer for a
                    // request finds EOF, the client has gone away and won't be
                    // sending us more data in the pipeline.
                    Err(BackendError::FirstReadEOF)
                } else {
                    Err(BackendError::UnexpectedEOF)
                };
            }
        }
    }

    /// `allow_body` should be set to false when we do not expect the backend to
    /// respond with a body even if the response headers indicate a body would
    /// be present such as is the case with a HEAD request.
    pub async fn read(mut self, allow_body: bool) -> BackendResult<ResponseStream<I>> {
        let res = self.head().await?;
        let Self {
            io,
            max_head_length,
            buffer,
        } = self;

        let framing = res.framing()?;
        let is_informational = res.is_informational();

        // RFC 9110 section 8.6 prohibits an 1xx or 204 response from containing
        // a content-length header. it also specifies that a content-length
        // header MUST NOT be sent in a response to a CONNECT method.
        //
        // RFC 9112 section 6.1 prohobits an 1xx or 204 response from containing
        // a transfer-encoding header. it also specifies a transfer-encoding
        // header must not be sent in response to a CONNECT request.
        if res.code() == 204 && !framing.is_no_framing() {
            return Err(BackendError::FramingHeadersOn204NoContentResposne);
        }
        if is_informational.is_some() && !framing.is_no_framing() {
            return Err(BackendError::FramingHeadersOnInformationalResposne);
        }

        if let Some(info_code) = is_informational {
            return match info_code {
                100 => Ok(ResponseStream::Informational(
                    res,
                    Self {
                        io,
                        max_head_length,
                        buffer,
                    },
                )),
                101 => Err(BackendError::HttpSwitchingProtocolsUnsupported),
                102 => Err(BackendError::HttpProcessingUnsupported),
                103 => Ok(ResponseStream::Informational(
                    res,
                    Self {
                        io,
                        max_head_length,
                        buffer,
                    },
                )),
                unk => Err(BackendError::HttpUnsupportedInformational(unk)),
            };
        }

        let reader = if !allow_body {
            BackendBodyReader::new(
                io,
                max_head_length,
                buffer.into_inner(),
                BodyReadMode::Bodyless,
            )
        } else {
            match framing {
                HeadFraming::Length(cl) => BackendBodyReader::new(
                    io,
                    max_head_length,
                    buffer.into_inner(),
                    BodyReadMode::ContentLength(cl),
                ),
                HeadFraming::Chunked => BackendBodyReader::new(
                    io,
                    max_head_length,
                    buffer.into_inner(),
                    BodyReadMode::Chunk,
                ),
                HeadFraming::NoFraming => BackendBodyReader::new(
                    io,
                    max_head_length,
                    buffer.into_inner(),
                    BodyReadMode::Bodyless,
                ),
            }
        };

        Ok(ResponseStream::Response(BackendResponse { res, reader }))
    }

    pub fn into_inner(self) -> I {
        let Self { io, .. } = self;
        io
    }
}

pub enum ResponseStream<I> {
    /// We got a regular response. We will not read another until we've drained
    /// out the body, if any.
    Response(BackendResponse<I>),
    /// We got an informational response.
    Informational(Response, BackendReader<I>),
}

impl<I> ResponseStream<I> {
    pub fn try_into_response(self) -> Result<BackendResponse<I>, Self> {
        match self {
            ResponseStream::Response(r) => Ok(r),
            s @ ResponseStream::Informational(_, _) => Err(s),
        }
    }

    pub fn try_into_informational(self) -> Result<(Response, BackendReader<I>), Self> {
        match self {
            r @ ResponseStream::Response(_) => Err(r),
            ResponseStream::Informational(res, reader) => Ok((res, reader)),
        }
    }
}

impl<I: std::fmt::Debug> std::fmt::Debug for ResponseStream<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Response(_) => write!(f, "Response"),
            Self::Informational(_, _) => write!(f, "Informational"),
        }
    }
}

pub struct BackendResponse<I> {
    res: Response,
    reader: BackendBodyReader<I>,
}

impl<I> BackendResponse<I> {
    pub fn res(&self) -> &Response {
        &self.res
    }

    pub fn res_mut(&mut self) -> &mut Response {
        &mut self.res
    }

    pub fn into_parts(self) -> (Response, BackendBodyReader<I>) {
        let Self { res, reader } = self;
        (res, reader)
    }

    pub fn mode(&self) -> BodyReadMode {
        self.reader.mode()
    }

    pub fn into_version(self, frontend_version: HttpVersion) -> Self {
        let Self { res, reader } = self;
        Self {
            res: res.into_version(frontend_version),
            reader,
        }
    }
}

impl<I: AsyncReadExt + Unpin> BackendResponse<I> {
    pub async fn peek_body(&mut self, len: usize) -> BackendResult<(bool, &[u8])> {
        self.reader.peek(len).await
    }
}

pub struct BackendBodyReader<I> {
    max_head_length: usize,
    reader: BodyReader<I>,
}

impl<I> BackendBodyReader<I> {
    pub fn mode(&self) -> BodyReadMode {
        self.reader.mode()
    }
}

impl<I: AsyncReadExt + Unpin> BackendBodyReader<I> {
    fn new(io: I, max_head_length: usize, buffer: BytesMut, mode: BodyReadMode) -> Self {
        Self {
            max_head_length,
            reader: BodyReader::new(io, buffer, mode),
        }
    }

    pub async fn peek(&mut self, len: usize) -> BackendResult<(bool, &[u8])> {
        self.reader
            .peek(len)
            .await
            .map_err(BackendError::BodyReadError)
    }

    pub async fn read(&mut self, max_len: usize) -> BackendResult<Option<Bytes>> {
        self.reader
            .read(max_len)
            .await
            .map_err(BackendError::BodyReadError)
    }

    pub fn drained(&self) -> bool {
        self.reader.drained()
    }

    pub async fn drain(self) -> BackendResult<BackendReader<I>> {
        let max_head_length = self.max_head_length;
        let (io, buffer) = self
            .reader
            .drain()
            .await
            .map_err(BackendError::BodyReadError)?;
        Ok(BackendReader::new_with_buffer(
            io,
            max_head_length,
            buffer.into_inner(),
        ))
    }
}

#[cfg(test)]
mod test {
    use bytes::Bytes;
    use jrpxy_body::BodyError;
    use jrpxy_http_message::{message::RequestBuilder, version::HttpVersion};

    use crate::{BackendBodyReader, BackendError, BackendReader};

    use super::BackendWriter;

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

    #[tokio::test]
    async fn read_cl_backend() {
        let buf = b"\
            HTTP/1.1 200 Ok\r\n\
            x-req: second\r\n\
            content-length: 5\r\n\
            \r\n\
            01234\
            ";

        let reader = BackendReader::new(buf.as_slice(), 128);
        let (res, br) = reader
            .read(true)
            .await
            .expect("can split into parts")
            .try_into_response()
            .unwrap()
            .into_parts();
        let mut br = BackendBodyReader::from(br);

        assert_eq!(HttpVersion::Http11, res.version());
        assert_eq!(200, res.code());
        assert_eq!(&b"Ok"[..], res.reason());

        let x = br.read(3).await.expect("can read").unwrap();
        assert_eq!(Bytes::from_static(b"012"), x);
        let x = br.read(3).await.expect("can read").unwrap();
        assert_eq!(Bytes::from_static(b"34"), x);

        let reader = br.drain().await.expect("failed to drian");

        let rest = reader.into_inner();
        assert!(rest.is_empty());
    }

    #[tokio::test]
    async fn peek_cl_backend() {
        let buf = b"\
            HTTP/1.1 200 Ok\r\n\
            x-req: second\r\n\
            content-length: 5\r\n\
            \r\n\
            01234\
            ";

        let reader = BackendReader::new(buf.as_slice(), 128);
        let (res, br) = reader
            .read(true)
            .await
            .expect("can split into parts")
            .try_into_response()
            .unwrap()
            .into_parts();
        let mut br = BackendBodyReader::from(br);

        assert_eq!(HttpVersion::Http11, res.version());
        assert_eq!(200, res.code());
        assert_eq!(&b"Ok"[..], res.reason());

        // same as read tests, but peek first.
        let (complete, x) = br.peek(5).await.expect("can peek");
        assert!(complete);
        assert_eq!(&b"01234"[..], x);

        let x = br.read(3).await.expect("can read").unwrap();
        assert_eq!(Bytes::from_static(b"012"), x);
        let x = br.read(3).await.expect("can read").unwrap();
        assert_eq!(Bytes::from_static(b"34"), x);

        // peek again after reading all the data, make sure we're complete with
        // no data.
        let (complete, x) = br.peek(5).await.expect("can peek");
        assert!(complete);
        assert_eq!(&b""[..], x);

        let reader = br.drain().await.expect("failed to drian");

        let rest = reader.into_inner();
        assert!(rest.is_empty());
    }

    #[tokio::test]
    async fn read_te_backend() {
        let buf = b"\
            HTTP/1.1 200 Ok\r\n\
            x-req: second\r\n\
            transfer-encoding: chunked\r\n\
            \r\n\
            3\r\n\
            012\r\n\
            3\r\n\
            345\r\n\
            0\r\n\
            \r\n\
            ";

        let reader = BackendReader::new(buf.as_slice(), 128);
        let (res, br) = reader
            .read(true)
            .await
            .expect("can split into parts")
            .try_into_response()
            .unwrap()
            .into_parts();
        let mut br = BackendBodyReader::from(br);

        assert_eq!(HttpVersion::Http11, res.version());
        assert_eq!(200, res.code());
        assert_eq!(&b"Ok"[..], res.reason());

        let x = br.read(3).await.expect("can read").unwrap();
        assert_eq!(Bytes::from_static(b"012"), x);
        let x = br.read(3).await.expect("can read").unwrap();
        assert_eq!(Bytes::from_static(b"345"), x);

        let reader = br.drain().await.expect("failed to drian");

        let rest = reader.into_inner();
        assert!(rest.is_empty());
    }

    #[tokio::test]
    async fn peek_te_backend() {
        let buf = b"\
            HTTP/1.1 200 Ok\r\n\
            x-req: second\r\n\
            transfer-encoding: chunked\r\n\
            \r\n\
            3\r\n\
            012\r\n\
            3\r\n\
            345\r\n\
            0\r\n\
            \r\n\
            ";

        let reader = BackendReader::new(buf.as_slice(), 128);
        let (res, br) = reader
            .read(true)
            .await
            .expect("can split into parts")
            .try_into_response()
            .unwrap()
            .into_parts();
        let mut br = BackendBodyReader::from(br);

        assert_eq!(HttpVersion::Http11, res.version());
        assert_eq!(200, res.code());
        assert_eq!(&b"Ok"[..], res.reason());

        // same as read tests, but peek first. peek one extra byte so we know
        // it's complete.
        let (complete, x) = br.peek(7).await.expect("can peek");
        assert!(complete);
        assert_eq!(&b"012345"[..], x);

        let x = br.read(3).await.expect("can read").unwrap();
        assert_eq!(Bytes::from_static(b"012"), x);
        let x = br.read(3).await.expect("can read").unwrap();
        assert_eq!(Bytes::from_static(b"345"), x);

        // peek again after reading all the data, make sure we're complete with
        // no data.
        let (complete, x) = br.peek(7).await.expect("can peek");
        assert!(complete);
        assert_eq!(&b""[..], x);

        let reader = br.drain().await.expect("failed to drian");

        let rest = reader.into_inner();
        assert!(rest.is_empty());
    }

    #[tokio::test]
    async fn read_cl_bodyless_backend() {
        let buf = b"\
            HTTP/1.1 200 Ok\r\n\
            content-length: 5\r\n\
            \r\n\
            ";

        let reader = BackendReader::new(buf.as_slice(), 128);
        let (res, br) = reader
            .read(false)
            .await
            .expect("can split into parts")
            .try_into_response()
            .unwrap()
            .into_parts();
        let mut br = BackendBodyReader::from(br);

        assert_eq!(HttpVersion::Http11, res.version());
        assert_eq!(200, res.code());
        assert_eq!(&b"Ok"[..], res.reason());

        assert!(br.read(5).await.expect("can read").is_none());
        let reader = br.drain().await.expect("failed to drian");
        let rest = reader.into_inner();
        assert!(rest.is_empty());
    }

    #[tokio::test]
    async fn read_te_bodyless_backend() {
        let buf = b"\
            HTTP/1.1 200 Ok\r\n\
            transfer-encoding: chunked\r\n\
            \r\n\
            ";

        let reader = BackendReader::new(buf.as_slice(), 128);
        let (res, br) = reader
            .read(false)
            .await
            .expect("can split into parts")
            .try_into_response()
            .unwrap()
            .into_parts();
        let mut br = BackendBodyReader::from(br);

        assert_eq!(HttpVersion::Http11, res.version());
        assert_eq!(200, res.code());
        assert_eq!(&b"Ok"[..], res.reason());

        assert!(br.read(5).await.expect("can read").is_none());
        let reader = br.drain().await.expect("failed to drian");
        let rest = reader.into_inner();
        assert!(rest.is_empty());
    }

    #[tokio::test]
    async fn backend_has_no_framing_headers() {
        let buf = b"\
            HTTP/1.1 200 Ok\r\n\
            \r\n\
            ";

        let reader = BackendReader::new(buf.as_slice(), 128);
        let (_res, br) = reader
            .read(true)
            .await
            .expect("can split into parts")
            .try_into_response()
            .unwrap()
            .into_parts();
        let mut br = BackendBodyReader::from(br);
        let body = br.read(1024).await.expect("can read");
        assert!(body.is_none());
    }

    /// RFC 9110 Section 8.6 states: "A server MUST NOT send a Content-Length
    /// header field in any response with a status code of 1xx (Informational)
    /// or 204 (No Content)."
    #[tokio::test]
    async fn status_1xx_and_204_response_with_content_length_rejected() {
        let buf = b"\
            HTTP/1.1 100 Continue\r\n\
            content-length: 500\r\n\
            \r\n\
            HTTP/1.1 200 Ok\r\n\
            content-length: 5\r\n\
            \r\n\
            01234\
            ";

        let reader = BackendReader::new(buf.as_slice(), 128);
        let result = reader.read(true).await;

        assert!(result.is_err());

        let buf = b"\
            HTTP/1.1 204 No Content\r\n\
            content-length: 0\r\n\
            \r\n\
            ";

        let reader = BackendReader::new(buf.as_slice(), 128);
        let result = reader.read(true).await;

        assert!(result.is_err(),);
    }

    /// RFC 9112 Section 6.1 states: "A server MUST NOT send a Transfer-Encoding
    /// header field in any response with a status code of 1xx (Informational)
    /// or 204 (No Content)."
    #[tokio::test]
    async fn status_1xx_and_204_response_with_transfer_encoding_rejected() {
        let buf = b"\
            HTTP/1.1 100 Continue\r\n\
            transfer-encoding: chunked\r\n\
            \r\n\
            HTTP/1.1 200 Ok\r\n\
            content-length: 5\r\n\
            \r\n\
            01234\
            ";

        let reader = BackendReader::new(buf.as_slice(), 128);
        let result = reader.read(true).await;

        assert!(result.is_err());

        let buf = b"\
            HTTP/1.1 204 No Content\r\n\
            transfer-encoding: chunked\r\n\
            \r\n\
            ";

        let reader = BackendReader::new(buf.as_slice(), 128);
        let result = reader.read(true).await;

        assert!(result.is_err(),);
    }
}
