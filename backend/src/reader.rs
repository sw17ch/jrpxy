use bytes::Bytes;
use jrpxy_body::{
    BodyReadMode, BodyReader, BodyReaderKind, BodylessBodyReader, ChunkedBodyReader,
    ContentLengthBodyReader, PeekableBodyReader,
};
use jrpxy_http_message::header::Headers;
use jrpxy_http_message::{
    framing::HeadFraming,
    message::{ParseSlots, Response},
};
use jrpxy_util::io_buffer::IoBuffer;
use tokio::io::AsyncReadExt;

use crate::error::{BackendError, BackendResult};

#[derive(Debug)]
pub struct BackendReader<I> {
    io_buffer: IoBuffer<I>,
    parse_slots: ParseSlots,
}

impl<I: AsyncReadExt + Unpin> BackendReader<I> {
    const MAX_HEADERS: usize = 256;

    pub fn new(io: I) -> Self {
        Self::new_with_iobuffer(IoBuffer::new(io))
    }

    pub fn new_with_iobuffer(io_buffer: IoBuffer<I>) -> Self {
        Self {
            io_buffer,
            parse_slots: ParseSlots::new(Self::MAX_HEADERS),
        }
    }

    async fn head(&mut self, max_head_length: usize) -> BackendResult<Response> {
        loop {
            if let Some(res) = self
                .parse_slots
                .parse_response(self.io_buffer.as_buffer_mut())
                .map_err(BackendError::HttpResponseParseError)?
            {
                return Ok(res);
            } else if self.io_buffer.len() >= max_head_length {
                return Err(BackendError::MaxHeadLenExceeded(
                    self.io_buffer.len(),
                    max_head_length,
                ));
            }

            let first_read = self.io_buffer.is_empty();

            // read some data into the buffer
            let target_read_len = max_head_length.saturating_sub(self.io_buffer.len());
            let len = self
                .io_buffer
                .read_with_len(target_read_len)
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
    /// be present such as is the case with a HEAD or CONNECT request.
    pub async fn read(
        mut self,
        allow_body: bool,
        max_head_length: usize,
    ) -> BackendResult<ResponseStream<I>> {
        let res = self.head(max_head_length).await?;
        let Self {
            io_buffer,
            parse_slots,
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
        // While RFC 9112 section 6.3 specifies that a 304 does not contain body
        // data, section 6.1 allows a transfer-encoding header in the response
        // to indicate the server would have applied this a transfer-encoding in
        // an unconditional GET. RFC 9110 section 8.6 says the same about 304
        // and content-length. Therefor, we only reject status codes with
        // framing that are 100..=199 and 204.
        let expect_body = allow_body && !matches!(res.code(), 100..200 | 204 | 304);

        if let Some(info_code) = is_informational {
            return match info_code {
                100 => Ok(ResponseStream::Informational(
                    res,
                    BackendStreamReader {
                        allow_body,
                        max_head_length,
                        reader: Self {
                            io_buffer,
                            parse_slots,
                        },
                    },
                )),
                101 => Err(BackendError::HttpSwitchingProtocolsUnsupported),
                102 => Err(BackendError::HttpProcessingUnsupported),
                103 => Ok(ResponseStream::Informational(
                    res,
                    BackendStreamReader {
                        allow_body,
                        max_head_length,
                        reader: Self {
                            io_buffer,
                            parse_slots,
                        },
                    },
                )),
                unk => Err(BackendError::HttpUnsupportedInformational(unk)),
            };
        }

        let reader = if !expect_body {
            BackendBodyReader::new(io_buffer, BodyReadMode::Bodyless, parse_slots)
        } else {
            match framing {
                HeadFraming::Length(cl) => {
                    BackendBodyReader::new(io_buffer, BodyReadMode::ContentLength(cl), parse_slots)
                }
                HeadFraming::Chunked => {
                    BackendBodyReader::new(io_buffer, BodyReadMode::Chunk, parse_slots)
                }
                HeadFraming::NoFraming => {
                    BackendBodyReader::new(io_buffer, BodyReadMode::Bodyless, parse_slots)
                }
            }
        };

        Ok(ResponseStream::Response(BackendResponse { res, reader }))
    }

    pub fn into_parts(self) -> IoBuffer<I> {
        let Self {
            io_buffer,
            parse_slots: _,
        } = self;
        io_buffer
    }
}

pub enum ResponseStream<I> {
    /// We got a regular response. We will not read another until we've drained
    /// out the body, if any.
    Response(BackendResponse<I>),
    /// We got an informational response.
    Informational(Response, BackendStreamReader<I>),
}

impl<I> ResponseStream<I> {
    pub fn try_into_response(self) -> Result<BackendResponse<I>, Box<Self>> {
        match self {
            ResponseStream::Response(r) => Ok(r),
            s @ ResponseStream::Informational(_, _) => Err(Box::new(s)),
        }
    }

    pub fn try_into_informational(self) -> Result<(Response, BackendStreamReader<I>), Box<Self>> {
        match self {
            r @ ResponseStream::Response(_) => Err(Box::new(r)),
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

pub struct BackendStreamReader<I> {
    allow_body: bool,
    max_head_length: usize,
    reader: BackendReader<I>,
}

impl<I: AsyncReadExt + Unpin> BackendStreamReader<I> {
    pub async fn read(self) -> BackendResult<ResponseStream<I>> {
        let Self {
            allow_body,
            max_head_length,
            reader,
        } = self;
        reader.read(allow_body, max_head_length).await
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
}

/// A backend response body reader.
/// Backend wrapper for [`BodylessBodyReader`].
pub struct BackendBodylessBodyReader<I> {
    inner: BodylessBodyReader<I>,
}

impl<I: AsyncReadExt + Unpin> BackendBodylessBodyReader<I> {
    pub async fn drain(self) -> BackendResult<BackendReader<I>> {
        let Self { inner } = self;
        Ok(BackendReader::new_with_iobuffer(inner.drain()))
    }
}

/// Backend wrapper for [`ContentLengthBodyReader`].
pub struct BackendContentLengthBodyReader<I> {
    inner: ContentLengthBodyReader<I>,
}

impl<I: AsyncReadExt + Unpin> BackendContentLengthBodyReader<I> {
    pub async fn read(&mut self, max_len: usize) -> BackendResult<Option<Bytes>> {
        self.inner
            .read(max_len)
            .await
            .map_err(BackendError::BodyReadError)
    }

    pub async fn drain(self) -> BackendResult<BackendReader<I>> {
        let Self { inner } = self;
        Ok(BackendReader::new_with_iobuffer(
            inner.drain().await.map_err(BackendError::BodyReadError)?,
        ))
    }
}

/// Backend wrapper for [`ChunkedBodyReader`]. Draining it returns both a
/// new [`BackendReader`] and the trailers from the chunked body.
pub struct BackendChunkedBodyReader<I> {
    inner: ChunkedBodyReader<I>,
}

impl<I> BackendChunkedBodyReader<I> {
    pub fn drained(&self) -> bool {
        self.inner.drained()
    }
}

impl<I: AsyncReadExt + Unpin> BackendChunkedBodyReader<I> {
    pub async fn read(&mut self, max_len: usize) -> BackendResult<Option<Bytes>> {
        self.inner
            .read(max_len)
            .await
            .map_err(BackendError::BodyReadError)
    }

    pub async fn drain(self) -> BackendResult<(BackendReader<I>, Headers)> {
        let Self { inner } = self;
        let (io, trailers) = inner.drain().await.map_err(BackendError::BodyReadError)?;
        Ok((BackendReader::new_with_iobuffer(io), trailers))
    }
}

/// The body reader kind for a backend response, typed so that draining
/// always produces a [`BackendReader`] without exposing raw IO.
pub enum BackendBodyReaderKind<I> {
    Bodyless(BackendBodylessBodyReader<I>),
    CL(BackendContentLengthBodyReader<I>),
    TE(BackendChunkedBodyReader<I>),
}

impl<I: AsyncReadExt + Unpin> BackendBodyReaderKind<I> {
    pub async fn read(&mut self, max_len: usize) -> BackendResult<Option<Bytes>> {
        match self {
            BackendBodyReaderKind::Bodyless(_) => Ok(None),
            BackendBodyReaderKind::CL(r) => r.read(max_len).await,
            BackendBodyReaderKind::TE(r) => r.read(max_len).await,
        }
    }

    /// Drain the body and return the next [`BackendReader`], discarding
    /// any trailers. Use the `TE` variant directly to access trailers.
    pub async fn drain(self) -> BackendResult<BackendReader<I>> {
        match self {
            BackendBodyReaderKind::Bodyless(r) => r.drain().await,
            BackendBodyReaderKind::CL(r) => r.drain().await,
            BackendBodyReaderKind::TE(r) => {
                let (reader, _trailers) = r.drain().await?;
                Ok(reader)
            }
        }
    }
}

pub struct BackendBodyReader<I> {
    reader: BodyReader<I>,
}

impl<I> BackendBodyReader<I> {
    pub fn mode(&self) -> BodyReadMode {
        self.reader.mode()
    }

    pub fn into_kind(self) -> BackendBodyReaderKind<I> {
        let Self { reader } = self;
        match reader.into_kind() {
            BodyReaderKind::Bodyless(inner) => {
                BackendBodyReaderKind::Bodyless(BackendBodylessBodyReader { inner })
            }
            BodyReaderKind::CL(inner) => {
                BackendBodyReaderKind::CL(BackendContentLengthBodyReader { inner })
            }
            BodyReaderKind::TE(inner) => {
                BackendBodyReaderKind::TE(BackendChunkedBodyReader { inner })
            }
        }
    }
}

impl<I: AsyncReadExt + Unpin> BackendBodyReader<I> {
    fn new(io: IoBuffer<I>, mode: BodyReadMode, parse_slots: ParseSlots) -> Self {
        Self {
            reader: BodyReader::new(io, mode, parse_slots),
        }
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

    pub fn peekable(self) -> BackendPeekableBodyReader<I> {
        BackendPeekableBodyReader::new(self)
    }

    pub async fn drain(self) -> BackendResult<BackendReader<I>> {
        let io = self
            .reader
            .drain()
            .await
            .map_err(BackendError::BodyReadError)?;
        Ok(BackendReader::new_with_iobuffer(io))
    }
}

/// A backend response body reader with peek support.
pub struct BackendPeekableBodyReader<I> {
    reader: PeekableBodyReader<I>,
}

impl<I> BackendPeekableBodyReader<I> {
    pub fn new(reader: BackendBodyReader<I>) -> Self {
        Self {
            reader: reader.reader.peekable(),
        }
    }

    pub fn mode(&self) -> BodyReadMode {
        self.reader.mode()
    }
}

impl<I: AsyncReadExt + Unpin> BackendPeekableBodyReader<I> {
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
        let io = self
            .reader
            .drain()
            .await
            .map_err(BackendError::BodyReadError)?;
        Ok(BackendReader::new_with_iobuffer(io))
    }
}

#[cfg(test)]
mod test {
    use bytes::Bytes;
    use jrpxy_http_message::version::HttpVersion;

    use crate::reader::BackendReader;

    #[tokio::test]
    async fn read_cl_backend() {
        let buf = b"\
            HTTP/1.1 200 Ok\r\n\
            x-req: second\r\n\
            content-length: 5\r\n\
            \r\n\
            01234\
            ";

        let reader = BackendReader::new(buf.as_slice());
        let (res, mut br) = reader
            .read(true, 128)
            .await
            .expect("can split into parts")
            .try_into_response()
            .unwrap()
            .into_parts();

        assert_eq!(HttpVersion::Http11, res.version());
        assert_eq!(200, res.code());
        assert_eq!(&b"Ok"[..], res.reason());

        let x = br.read(3).await.expect("can read").unwrap();
        assert_eq!(Bytes::from_static(b"012"), x);
        let x = br.read(3).await.expect("can read").unwrap();
        assert_eq!(Bytes::from_static(b"34"), x);

        let reader = br.drain().await.expect("failed to drian");

        let io_buffer = reader.into_parts();
        let (rest_unread, rest_buffered) = io_buffer.into_parts();
        assert!(rest_unread.is_empty());
        assert!(rest_buffered.is_empty());
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

        let reader = BackendReader::new(buf.as_slice());
        let (res, br) = reader
            .read(true, 128)
            .await
            .expect("can split into parts")
            .try_into_response()
            .unwrap()
            .into_parts();
        let mut br = br.peekable();

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

        let io_buffer = reader.into_parts();
        let (rest_unread, rest_buffered) = io_buffer.into_parts();
        assert!(rest_unread.is_empty());
        assert!(rest_buffered.is_empty());
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

        let reader = BackendReader::new(buf.as_slice());
        let (res, mut br) = reader
            .read(true, 128)
            .await
            .expect("can split into parts")
            .try_into_response()
            .unwrap()
            .into_parts();

        assert_eq!(HttpVersion::Http11, res.version());
        assert_eq!(200, res.code());
        assert_eq!(&b"Ok"[..], res.reason());

        let x = br.read(3).await.expect("can read").unwrap();
        assert_eq!(Bytes::from_static(b"012"), x);
        let x = br.read(3).await.expect("can read").unwrap();
        assert_eq!(Bytes::from_static(b"345"), x);

        let reader = br.drain().await.expect("failed to drian");

        let io_buffer = reader.into_parts();
        let (rest_unread, rest_buffered) = io_buffer.into_parts();
        assert!(rest_unread.is_empty());
        assert!(rest_buffered.is_empty());
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

        let reader = BackendReader::new(buf.as_slice());
        let (res, br) = reader
            .read(true, 128)
            .await
            .expect("can split into parts")
            .try_into_response()
            .unwrap()
            .into_parts();
        let mut br = br.peekable();

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

        let io_buffer = reader.into_parts();
        let (rest_unread, rest_buffered) = io_buffer.into_parts();
        assert!(rest_unread.is_empty());
        assert!(rest_buffered.is_empty());
    }

    #[tokio::test]
    async fn read_cl_bodyless_backend() {
        let buf = b"\
            HTTP/1.1 200 Ok\r\n\
            content-length: 5\r\n\
            \r\n\
            ";

        let reader = BackendReader::new(buf.as_slice());
        let (res, mut br) = reader
            .read(false, 128)
            .await
            .expect("can split into parts")
            .try_into_response()
            .unwrap()
            .into_parts();

        assert_eq!(HttpVersion::Http11, res.version());
        assert_eq!(200, res.code());
        assert_eq!(&b"Ok"[..], res.reason());

        assert!(br.read(5).await.expect("can read").is_none());
        let reader = br.drain().await.expect("failed to drian");

        let io_buffer = reader.into_parts();
        let (rest_unread, rest_buffered) = io_buffer.into_parts();
        assert!(rest_unread.is_empty());
        assert!(rest_buffered.is_empty());
    }

    #[tokio::test]
    async fn read_te_bodyless_backend() {
        let buf = b"\
            HTTP/1.1 200 Ok\r\n\
            transfer-encoding: chunked\r\n\
            \r\n\
            ";

        let reader = BackendReader::new(buf.as_slice());
        let (res, mut br) = reader
            .read(false, 128)
            .await
            .expect("can split into parts")
            .try_into_response()
            .unwrap()
            .into_parts();

        assert_eq!(HttpVersion::Http11, res.version());
        assert_eq!(200, res.code());
        assert_eq!(&b"Ok"[..], res.reason());

        assert!(br.read(5).await.expect("can read").is_none());
        let reader = br.drain().await.expect("failed to drian");

        let io_buffer = reader.into_parts();
        let (rest_unread, rest_buffered) = io_buffer.into_parts();
        assert!(rest_unread.is_empty());
        assert!(rest_buffered.is_empty());
    }

    #[tokio::test]
    async fn backend_has_no_framing_headers() {
        let buf = b"\
            HTTP/1.1 200 Ok\r\n\
            \r\n\
            ";

        let reader = BackendReader::new(buf.as_slice());
        let (_res, mut br) = reader
            .read(true, 128)
            .await
            .expect("can split into parts")
            .try_into_response()
            .unwrap()
            .into_parts();
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

        let reader = BackendReader::new(buf.as_slice());
        let result = reader.read(true, 128).await;

        assert!(result.is_err());

        let buf = b"\
            HTTP/1.1 204 No Content\r\n\
            content-length: 0\r\n\
            \r\n\
            ";

        let reader = BackendReader::new(buf.as_slice());
        let result = reader.read(true, 128).await;

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

        let reader = BackendReader::new(buf.as_slice());
        let result = reader.read(true, 128).await;

        assert!(result.is_err());

        let buf = b"\
            HTTP/1.1 204 No Content\r\n\
            transfer-encoding: chunked\r\n\
            \r\n\
            ";

        let reader = BackendReader::new(buf.as_slice());
        let result = reader.read(true, 128).await;

        assert!(result.is_err(),);
    }

    #[tokio::test]
    async fn status_1xx_stream() {
        let buf = b"\
            HTTP/1.1 100 Continue\r\n\
            x-res: 1\r\n\
            \r\n\
            HTTP/1.1 100 Continue\r\n\
            x-res: 2\r\n\
            \r\n\
            HTTP/1.1 200 Ok\r\n\
            x-res: 3\r\n\
            \r\n\
            ";

        let reader = BackendReader::new(buf.as_slice());
        let result = reader.read(true, 128).await.expect("failed to read");

        let (res, reader) = result.try_into_informational().expect("not informational");
        let xres = res.get_header("x-res").expect("header not present");
        assert_eq!(xres, b"1".as_ref());

        let result = reader.read().await.expect("failed to read");
        let (res, reader) = result.try_into_informational().expect("not informational");
        let xres = res.get_header("x-res").expect("header not present");
        assert_eq!(xres, b"2".as_ref());

        let result = reader.read().await.expect("read failed");
        let res = result.try_into_response().expect("not response");
        let (res, body_reader) = res.into_parts();
        let xres = res.get_header("x-res").expect("header not present");
        assert_eq!(xres, b"3".as_ref());
        let _out = body_reader.drain().await.expect("failed to drain");
    }

    #[tokio::test]
    async fn status_304_has_no_body() {
        let buf = b"\
            HTTP/1.1 304 Ok\r\n\
            content-length: 200\r\n\
            \r\n\
            dangling-ignored-data\
            ";
        let reader = BackendReader::new(buf.as_slice());
        let result = reader.read(true, 128).await.expect("failed to read");
        let response = result.try_into_response().expect("not a response");
        let (_response, mut body_reader) = response.into_parts();
        let body = body_reader.read(200).await.expect("failed to read body");
        assert!(body.is_none());
        let reader = body_reader.drain().await.expect("failed to drain");

        let io_buffer = reader.into_parts();
        let (rest_unread, rest_buffered) = io_buffer.into_parts();
        let rest = rest_unread
            .iter()
            .chain(rest_buffered.as_ref().iter())
            .map(|b| *b)
            .collect::<Vec<_>>();
        assert_eq!("dangling-ignored-data".as_bytes(), rest);
    }
}
