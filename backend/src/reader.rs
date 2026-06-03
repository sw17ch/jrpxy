use bytes::Bytes;
use jrpxy_body::reader::{
    BodylessBodyReader, ChunkedBodyReader, ContentLengthBodyReader, EofBodyReader,
};
use jrpxy_http_message::{
    framing::ParsedFraming,
    header::Headers,
    message::{ParseSlots, Response},
    version::HttpVersion,
};
use jrpxy_util::io_buffer::BytesReader;
use tokio::io::AsyncReadExt;

use crate::error::{BackendError, BackendResult};

#[derive(Debug)]
pub struct BackendReader<I> {
    reader: BytesReader<I>,
    parse_slots: ParseSlots,
}

impl<I> BackendReader<I>
where
    I: AsyncReadExt + Unpin,
{
    pub fn new(reader: I, max_headers: usize) -> Self {
        Self::new_with_iobuffer(BytesReader::new(reader), max_headers)
    }

    pub fn new_with_iobuffer(reader: BytesReader<I>, max_headers: usize) -> Self {
        Self {
            reader,
            parse_slots: ParseSlots::new(max_headers),
        }
    }

    async fn head(&mut self, max_head_length: usize) -> BackendResult<Response> {
        loop {
            if let Some(res) = self
                .parse_slots
                .parse_response(&mut self.reader)
                .map_err(BackendError::HttpResponseParseError)?
            {
                return Ok(res);
            } else if self.reader.len() >= max_head_length {
                return Err(BackendError::MaxHeadLenExceeded(
                    self.reader.len(),
                    max_head_length,
                ));
            }

            let first_read = self.reader.is_empty();

            // read some data into the buffer
            let target_read_len = max_head_length.saturating_sub(self.reader.len());
            let len = self
                .reader
                .read(target_read_len)
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
        max_chunk_header_length: usize,
    ) -> BackendResult<ResponseStream<I>> {
        let res = self.head(max_head_length).await?;
        let Self {
            reader,
            parse_slots,
        } = self;

        let framing = res.framing()?;
        let is_informational = res.is_informational();

        // RFC 9110 section 8.6 prohibits an 1xx or 204 response from containing
        // a content-length header. it also specifies that a content-length
        // header MUST NOT be sent in a response to a CONNECT method.
        //
        // RFC 9112 section 6.1 prohibits an 1xx or 204 response from containing
        // a transfer-encoding header. it also specifies a transfer-encoding
        // header must not be sent in response to a CONNECT request.
        if res.code() == 204 && !framing.is_no_framing() {
            return Err(BackendError::FramingHeadersOn204NoContentResponse);
        }
        if is_informational.is_some() && !framing.is_no_framing() {
            return Err(BackendError::FramingHeadersOnInformationalResponse);
        }
        if res.version() == HttpVersion::Http10
            && res
                .headers()
                .get_header("transfer-encoding")
                .next()
                .is_some()
        {
            // RFC 9112 section 6.1: A server or client that receives an
            // HTTP/1.0 message containing a Transfer-Encoding header field MUST
            // treat the message as if the framing is faulty, even if a
            // Content-Length is present, and close the connection after
            // processing the message.
            return Err(BackendError::TransferEncodingOnHttp10Server);
        }

        // NOTE: we explicitly allow HTTP/1.0 origins to send us 1xx responses.
        // According to RFC 9110 section 15.2, we are not allowed to forward
        // these to HTTP/1.0 clients, but we also MUST be able to parse one or
        // more 1xx responses before the final response (even if we do not
        // expect one). Section 15.2 binds what we send to clients, but not what
        // we accept from servers.

        // While RFC 9112 section 6.3 specifies that a 304 does not contain body
        // data, section 6.1 allows a transfer-encoding header in the response
        // to indicate the server would have applied this a transfer-encoding in
        // an unconditional GET. RFC 9110 section 8.6 says the same about 304
        // and content-length. Therefore, we only reject status codes with
        // framing that are 100..=199 and 204.
        let expect_body = allow_body && !matches!(res.code(), 100..=199 | 204 | 304);

        if let Some(info_code) = is_informational {
            return match info_code {
                // RFC 9110 section 15.2: a client (the gateway here) MUST be
                // able to parse 1xx responses even when it does not expect
                // one, and a proxy MUST forward them. Surface every 1xx code
                // as Informational so the caller can forward or drop it.
                // 101 and 102 are the deliberate exceptions: 101 changes
                // connection semantics in ways the gateway cannot safely
                // tunnel, and 102 is deprecated by RFC 7231 appendix B.
                101 => Err(BackendError::HttpSwitchingProtocolsUnsupported),
                102 => Err(BackendError::HttpProcessingUnsupported),
                _ => Ok(ResponseStream::Informational(
                    res,
                    BackendStreamReader {
                        allow_body,
                        max_head_length,
                        max_chunk_header_length,
                        reader: Self {
                            reader,
                            parse_slots,
                        },
                    },
                )),
            };
        }

        // We need to spend more time looking at Message Body Length from RFC
        // 9112 section 6.3.
        // https://www.rfc-editor.org/rfc/rfc9112.html#name-message-body-length
        let is_no_framing = matches!(framing, ParsedFraming::NoFraming);

        let recyclable = res.headers().connection_is_persistent(res.version())?;

        let reader = if !expect_body {
            BackendBodyReader::Bodyless(BackendBodylessBodyReader {
                inner: BodylessBodyReader::new(reader, parse_slots),
                recyclable,
            })
        } else if is_no_framing && expect_body {
            // No framing headers: body is terminated by connection close.
            // RFC 9112 section 6.3 rule 8.
            BackendBodyReader::Eof(BackendEofBodyReader {
                inner: EofBodyReader::new(reader, parse_slots),
            })
        } else {
            match framing {
                ParsedFraming::Length(cl) => {
                    BackendBodyReader::CL(BackendContentLengthBodyReader {
                        inner: ContentLengthBodyReader::new(cl, reader, parse_slots),
                        recyclable,
                    })
                }
                ParsedFraming::Chunked => BackendBodyReader::TE(BackendChunkedBodyReader {
                    inner: ChunkedBodyReader::new(
                        reader,
                        parse_slots,
                        max_chunk_header_length,
                        max_head_length,
                    ),
                    recyclable,
                }),
                ParsedFraming::NoFraming => {
                    BackendBodyReader::Bodyless(BackendBodylessBodyReader {
                        inner: BodylessBodyReader::new(reader, parse_slots),
                        recyclable,
                    })
                }
            }
        };

        Ok(ResponseStream::Response(BackendResponse { res, reader }))
    }

    pub fn into_parts(self) -> BytesReader<I> {
        let Self {
            reader,
            parse_slots: _,
        } = self;
        reader
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

impl<I> std::fmt::Debug for ResponseStream<I>
where
    I: std::fmt::Debug,
{
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
    max_chunk_header_length: usize,
    reader: BackendReader<I>,
}

impl<I> BackendStreamReader<I>
where
    I: AsyncReadExt + Unpin,
{
    pub async fn read(self) -> BackendResult<ResponseStream<I>> {
        let Self {
            allow_body,
            max_head_length,
            max_chunk_header_length,
            reader,
        } = self;
        // TODO: move max_head_length into the BackendReader in the same way
        // it's on the FrontendReader.
        reader
            .read(allow_body, max_head_length, max_chunk_header_length)
            .await
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

    pub fn recyclable(&self) -> bool {
        self.reader.recyclable()
    }

    pub fn into_parts(self) -> (Response, BackendBodyReader<I>) {
        let Self { res, reader } = self;
        (res, reader)
    }
}

/// A backend response body reader.
/// Backend wrapper for [`BodylessBodyReader`].
pub struct BackendBodylessBodyReader<I> {
    inner: BodylessBodyReader<I>,
    recyclable: bool,
}

impl<I> BackendBodylessBodyReader<I>
where
    I: AsyncReadExt + Unpin,
{
    pub fn drain(self) -> BackendResult<Option<BackendReader<I>>> {
        let Self { inner, recyclable } = self;
        let (reader, parse_slots) = inner.drain();
        if !recyclable {
            return Ok(None);
        }
        Ok(Some(BackendReader {
            reader,
            parse_slots,
        }))
    }
}

/// Backend wrapper for [`ContentLengthBodyReader`].
pub struct BackendContentLengthBodyReader<I> {
    inner: ContentLengthBodyReader<I>,
    recyclable: bool,
}

impl<I> BackendContentLengthBodyReader<I> {
    pub fn content_length(&self) -> u64 {
        self.inner.content_length()
    }
}

impl<I> BackendContentLengthBodyReader<I>
where
    I: AsyncReadExt + Unpin,
{
    pub async fn read(&mut self, max_len: usize) -> BackendResult<Option<Bytes>> {
        self.inner
            .read(max_len)
            .await
            .map_err(BackendError::BodyReadError)
    }

    pub async fn drain(self) -> BackendResult<Option<BackendReader<I>>> {
        let Self { inner, recyclable } = self;
        let (reader, parse_slots) = inner.drain().await.map_err(BackendError::BodyReadError)?;
        if !recyclable {
            return Ok(None);
        }
        Ok(Some(BackendReader {
            reader,
            parse_slots,
        }))
    }
}

/// Backend wrapper for [`ChunkedBodyReader`]. Draining it returns both a
/// new [`BackendReader`] and the trailers from the chunked body.
pub struct BackendChunkedBodyReader<I> {
    inner: ChunkedBodyReader<I>,
    recyclable: bool,
}

impl<I> BackendChunkedBodyReader<I> {
    pub fn drained(&self) -> bool {
        self.inner.drained()
    }
}

impl<I> BackendChunkedBodyReader<I>
where
    I: AsyncReadExt + Unpin,
{
    pub async fn read(&mut self, max_len: usize) -> BackendResult<Option<Bytes>> {
        self.inner
            .read(max_len)
            .await
            .map_err(BackendError::BodyReadError)
    }

    pub async fn drain(self) -> BackendResult<(Option<BackendReader<I>>, Headers)> {
        let Self { inner, recyclable } = self;
        let (reader, parse_slots, trailers) =
            inner.drain().await.map_err(BackendError::BodyReadError)?;
        let next = recyclable.then(|| BackendReader {
            reader,
            parse_slots,
        });
        Ok((next, trailers))
    }
}

/// Body reader for close-delimited response bodies.
pub struct BackendEofBodyReader<I> {
    inner: EofBodyReader<I>,
}

impl<I> BackendEofBodyReader<I>
where
    I: AsyncReadExt + Unpin,
{
    pub async fn read(&mut self, max_len: usize) -> BackendResult<Option<Bytes>> {
        self.inner
            .read(max_len)
            .await
            .map_err(BackendError::BodyReadError)
    }

    pub fn drain(self) -> ParseSlots {
        let Self { inner } = self;
        inner.drain()
    }
}

/// The body reader for a backend response. Each variant corresponds to a
/// different framing mode. Draining always produces a [`BackendReader`].
pub enum BackendBodyReader<I> {
    Bodyless(BackendBodylessBodyReader<I>),
    CL(BackendContentLengthBodyReader<I>),
    TE(BackendChunkedBodyReader<I>),
    Eof(BackendEofBodyReader<I>),
}

impl<I> BackendBodyReader<I> {
    pub fn recyclable(&self) -> bool {
        match self {
            BackendBodyReader::Bodyless(r) => r.recyclable,
            BackendBodyReader::CL(r) => r.recyclable,
            BackendBodyReader::TE(r) => r.recyclable,
            // EOF-framed bodies are delimited by connection close, so the
            // backend connection is never reusable.
            BackendBodyReader::Eof(_) => false,
        }
    }
}

impl<I> BackendBodyReader<I>
where
    I: AsyncReadExt + Unpin,
{
    pub async fn read(&mut self, max_len: usize) -> BackendResult<Option<Bytes>> {
        match self {
            BackendBodyReader::Bodyless(_) => Ok(None),
            BackendBodyReader::CL(r) => r.read(max_len).await,
            BackendBodyReader::TE(r) => r.read(max_len).await,
            BackendBodyReader::Eof(r) => r.read(max_len).await,
        }
    }

    pub fn drained(&self) -> bool {
        match self {
            BackendBodyReader::Bodyless(_) => true,
            BackendBodyReader::CL(r) => r.inner.drained(),
            BackendBodyReader::TE(r) => r.inner.drained(),
            BackendBodyReader::Eof(r) => r.inner.drained(),
        }
    }

    /// Drain the body and return the next [`BackendReader`], discarding
    /// any trailers.
    pub async fn drain(self) -> BackendResult<Option<BackendReader<I>>> {
        match self {
            BackendBodyReader::Bodyless(r) => r.drain(),
            BackendBodyReader::CL(r) => r.drain().await,
            BackendBodyReader::TE(r) => {
                let (reader, _trailers) = r.drain().await?;
                Ok(reader)
            }
            BackendBodyReader::Eof(r) => {
                // TODO: we could probably figure out a way to reuse these
                // instead of dropping them.
                let _parse_slots = r.drain();
                Ok(None)
            }
        }
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

        let reader = BackendReader::new(buf.as_slice(), 256);
        let (res, mut br) = reader
            .read(true, 128, 128)
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

        let reader = br.drain().await.expect("failed to drain");

        let io_buffer = reader.unwrap().into_parts();
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

        let reader = BackendReader::new(buf.as_slice(), 256);
        let (res, mut br) = reader
            .read(true, 128, 128)
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

        let reader = br.drain().await.expect("failed to drain");

        let io_buffer = reader.unwrap().into_parts();
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

        let reader = BackendReader::new(buf.as_slice(), 256);
        let (res, mut br) = reader
            .read(false, 128, 128)
            .await
            .expect("can split into parts")
            .try_into_response()
            .unwrap()
            .into_parts();

        assert_eq!(HttpVersion::Http11, res.version());
        assert_eq!(200, res.code());
        assert_eq!(&b"Ok"[..], res.reason());

        assert!(br.read(5).await.expect("can read").is_none());
        let reader = br.drain().await.expect("failed to drain");

        let io_buffer = reader.unwrap().into_parts();
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

        let reader = BackendReader::new(buf.as_slice(), 256);
        let (res, mut br) = reader
            .read(false, 128, 128)
            .await
            .expect("can split into parts")
            .try_into_response()
            .unwrap()
            .into_parts();

        assert_eq!(HttpVersion::Http11, res.version());
        assert_eq!(200, res.code());
        assert_eq!(&b"Ok"[..], res.reason());

        assert!(br.read(5).await.expect("can read").is_none());
        let reader = br.drain().await.expect("failed to drain");

        let io_buffer = reader.unwrap().into_parts();
        let (rest_unread, rest_buffered) = io_buffer.into_parts();
        assert!(rest_unread.is_empty());
        assert!(rest_buffered.is_empty());
    }

    #[tokio::test]
    async fn backend_has_no_framing_headers() {
        let buf = b"\
            HTTP/1.1 204 No Content\r\n\
            \r\n\
            ";

        let reader = BackendReader::new(buf.as_slice(), 256);
        let (_res, mut br) = reader
            .read(true, 128, 128)
            .await
            .expect("can split into parts")
            .try_into_response()
            .unwrap()
            .into_parts();
        let body = br.read(1024).await.expect("can read");
        assert!(body.is_none());
    }

    /// RFC 9112 section 6.3 point 8: when a 200 response has no framing headers,
    /// the body is terminated by connection close. This is not yet implemented.
    #[tokio::test]
    async fn read_200_no_framing_eof_body() {
        let buf = b"\
            HTTP/1.1 200 Ok\r\n\
            \r\n\
            body terminated by eof\
            ";

        let reader = BackendReader::new(buf.as_slice(), 256);
        let (_res, mut br) = reader
            .read(true, 128, 128)
            .await
            .expect("can split into parts")
            .try_into_response()
            .unwrap()
            .into_parts();
        let body = br
            .read(1024)
            .await
            .expect("can read")
            .expect("body present");
        assert_eq!(body, &b"body terminated by eof"[..]);
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

        let reader = BackendReader::new(buf.as_slice(), 256);
        let result = reader.read(true, 128, 128).await;

        assert!(result.is_err());

        let buf = b"\
            HTTP/1.1 204 No Content\r\n\
            content-length: 0\r\n\
            \r\n\
            ";

        let reader = BackendReader::new(buf.as_slice(), 256);
        let result = reader.read(true, 128, 128).await;

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

        let reader = BackendReader::new(buf.as_slice(), 256);
        let result = reader.read(true, 128, 128).await;

        assert!(result.is_err());

        let buf = b"\
            HTTP/1.1 204 No Content\r\n\
            transfer-encoding: chunked\r\n\
            \r\n\
            ";

        let reader = BackendReader::new(buf.as_slice(), 256);
        let result = reader.read(true, 128, 128).await;

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
            content-length: 0\r\n\
            \r\n\
            ";

        let reader = BackendReader::new(buf.as_slice(), 256);
        let result = reader.read(true, 128, 128).await.expect("failed to read");

        let (res, reader) = result.try_into_informational().expect("not informational");
        let xres = res
            .headers()
            .get_header("x-res")
            .next()
            .expect("header not present");
        assert_eq!(xres.1, b"1".as_ref());

        let result = reader.read().await.expect("failed to read");
        let (res, reader) = result.try_into_informational().expect("not informational");
        let xres = res
            .headers()
            .get_header("x-res")
            .next()
            .expect("header not present");
        assert_eq!(xres.1, b"2".as_ref());

        let result = reader.read().await.expect("read failed");
        let res = result.try_into_response().expect("not response");
        let (res, body_reader) = res.into_parts();
        let xres = res
            .headers()
            .get_header("x-res")
            .next()
            .expect("header not present");
        assert_eq!(xres.1, b"3".as_ref());
        let _out = body_reader.drain().await.expect("failed to drain");
    }

    /// RFC 9110 section 15.2: a client MUST be able to parse 1xx responses
    /// even when it does not expect one. The reader must surface
    /// unrecognized 1xx codes as Informational rather than hard-error, so
    /// the caller can decide whether to forward or drop them.
    #[tokio::test]
    async fn unknown_1xx_surfaced_as_informational() {
        let buf = b"\
            HTTP/1.1 199 Custom Info\r\n\
            x-hint: maybe\r\n\
            \r\n\
            HTTP/1.1 200 Ok\r\n\
            content-length: 0\r\n\
            \r\n";

        let reader = BackendReader::new(buf.as_slice(), 256);
        let result = reader.read(true, 128, 128).await.expect("failed to read");

        let (res, reader) = result
            .try_into_informational()
            .expect("unknown 1xx must be surfaced as Informational, not an error");
        assert_eq!(res.code(), 199);

        let result = reader.read().await.expect("failed to read");
        let res = result.try_into_response().expect("not response");
        assert_eq!(res.res().code(), 200);
    }

    #[tokio::test]
    async fn status_304_has_no_body() {
        let buf = b"\
            HTTP/1.1 304 Ok\r\n\
            content-length: 200\r\n\
            \r\n\
            dangling-ignored-data\
            ";
        let reader = BackendReader::new(buf.as_slice(), 256);
        let result = reader.read(true, 128, 128).await.expect("failed to read");
        let response = result.try_into_response().expect("not a response");
        let (_response, mut body_reader) = response.into_parts();
        let body = body_reader.read(200).await.expect("failed to read body");
        assert!(body.is_none());
        let reader = body_reader.drain().await.expect("failed to drain");

        let io_buffer = reader.unwrap().into_parts();
        let (rest_unread, rest_buffered) = io_buffer.into_parts();
        let rest = rest_unread
            .iter()
            .chain(rest_buffered.as_ref().iter())
            .copied()
            .collect::<Vec<_>>();
        assert_eq!("dangling-ignored-data".as_bytes(), rest);
    }
}
