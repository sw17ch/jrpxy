use bytes::{Bytes, BytesMut};
use jrpxy_body::{BodyReadMode, BodyReader};
use jrpxy_http_message::{
    framing::HeadFraming,
    message::{ParseSlots, Response},
};
use jrpxy_util::buffer;
use tokio::io::AsyncReadExt;

use crate::error::{BackendError, BackendResult};

#[derive(Debug)]
pub struct BackendReader<I> {
    io: I,
    buffer: buffer::Buffer,
}

impl<I: AsyncReadExt + Unpin> BackendReader<I> {
    const MAX_HEADERS: usize = 256;

    pub fn new(io: I) -> Self {
        Self::new_with_buffer(io, BytesMut::new())
    }

    fn new_with_buffer(io: I, buffer: BytesMut) -> BackendReader<I> {
        Self {
            io,
            buffer: buffer::Buffer::new(buffer),
        }
    }

    async fn head(&mut self, max_head_length: usize) -> BackendResult<Response> {
        let mut parse_slots = ParseSlots::new(Self::MAX_HEADERS);
        loop {
            if let Some(res) = parse_slots.parse_response(&mut self.buffer)
                .map_err(BackendError::HttpResponseParseError)?
            {
                return Ok(res);
            } else if self.buffer.len() >= max_head_length {
                return Err(BackendError::MaxHeadLenExceeded(
                    self.buffer.len(),
                    max_head_length,
                ));
            }

            let first_read = self.buffer.is_empty();

            // read some data into the buffer
            let target_read_len = max_head_length.saturating_sub(self.buffer.len());
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
    /// be present such as is the case with a HEAD or CONNECT request.
    pub async fn read(
        mut self,
        allow_body: bool,
        max_head_length: usize,
    ) -> BackendResult<ResponseStream<I>> {
        let res = self.head(max_head_length).await?;
        let Self { io, buffer } = self;

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
                        reader: Self { io, buffer },
                    },
                )),
                101 => Err(BackendError::HttpSwitchingProtocolsUnsupported),
                102 => Err(BackendError::HttpProcessingUnsupported),
                103 => Ok(ResponseStream::Informational(
                    res,
                    BackendStreamReader {
                        allow_body,
                        max_head_length,
                        reader: Self { io, buffer },
                    },
                )),
                unk => Err(BackendError::HttpUnsupportedInformational(unk)),
            };
        }

        let reader = if !expect_body {
            BackendBodyReader::new(io, buffer.into_inner(), BodyReadMode::Bodyless)
        } else {
            match framing {
                HeadFraming::Length(cl) => {
                    BackendBodyReader::new(io, buffer.into_inner(), BodyReadMode::ContentLength(cl))
                }
                HeadFraming::Chunked => {
                    BackendBodyReader::new(io, buffer.into_inner(), BodyReadMode::Chunk)
                }
                HeadFraming::NoFraming => {
                    BackendBodyReader::new(io, buffer.into_inner(), BodyReadMode::Bodyless)
                }
            }
        };

        Ok(ResponseStream::Response(BackendResponse { res, reader }))
    }

    pub fn into_parts(self) -> (I, BytesMut) {
        let Self { io, buffer } = self;
        (io, buffer.into_inner())
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
    pub fn try_into_response(self) -> Result<BackendResponse<I>, Self> {
        match self {
            ResponseStream::Response(r) => Ok(r),
            s @ ResponseStream::Informational(_, _) => Err(s),
        }
    }

    pub fn try_into_informational(self) -> Result<(Response, BackendStreamReader<I>), Self> {
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

impl<I: AsyncReadExt + Unpin> BackendResponse<I> {
    pub async fn peek_body(&mut self, len: usize) -> BackendResult<(bool, &[u8])> {
        self.reader.peek(len).await
    }
}

pub struct BackendBodyReader<I> {
    reader: BodyReader<I>,
}

impl<I> BackendBodyReader<I> {
    pub fn mode(&self) -> BodyReadMode {
        self.reader.mode()
    }
}

impl<I: AsyncReadExt + Unpin> BackendBodyReader<I> {
    fn new(io: I, buffer: BytesMut, mode: BodyReadMode) -> Self {
        Self {
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
        let (io, buffer) = self
            .reader
            .drain()
            .await
            .map_err(BackendError::BodyReadError)?;
        Ok(BackendReader::new_with_buffer(io, buffer.into_inner()))
    }
}

#[cfg(test)]
mod test {
    use bytes::Bytes;
    use jrpxy_http_message::version::HttpVersion;

    use crate::reader::{BackendBodyReader, BackendReader};

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
        let (res, br) = reader
            .read(true, 128)
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

        let (rest_unread, rest_buffered) = reader.into_parts();
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

        let (rest_unread, rest_buffered) = reader.into_parts();
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
        let (res, br) = reader
            .read(true, 128)
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

        let (rest_unread, rest_buffered) = reader.into_parts();
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

        let (rest_unread, rest_buffered) = reader.into_parts();
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
        let (res, br) = reader
            .read(false, 128)
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

        let (rest_unread, rest_buffered) = reader.into_parts();
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
        let (res, br) = reader
            .read(false, 128)
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

        let (rest_unread, rest_buffered) = reader.into_parts();
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
        let (_res, br) = reader
            .read(true, 128)
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

        let (rest_unread, rest_buffered) = reader.into_parts();
        let rest = rest_unread
            .iter()
            .chain(rest_buffered.as_ref().iter())
            .map(|b| *b)
            .collect::<Vec<_>>();
        assert_eq!("dangling-ignored-data".as_bytes(), rest);
    }
}
