//! Tools for reading requests from an HTTP/1.x frontend.
//!
//! We can use [`FrontendReader`] to read a stream of requests.
//!
//! ```rust
//! use jrpxy_frontend::reader::FrontendReader;
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() {
//!     // a buffer with two pipelined requests
//!     let buf = b"\
//!         POST / HTTP/1.1\r\n\
//!         Host: localhost\r\n\
//!         Content-Length: 10\r\n\
//!         \r\n\
//!         0123456789\
//!         POST /next HTTP/1.1\r\n\
//!         Host: localhost\r\n\
//!         Content-Length: 5\r\n\
//!         \r\n\
//!         abcde";
//!
//!     // Read the first request in the pipeline
//!     let frontend_reader = FrontendReader::new(&buf[..], 256);
//!     let frontend_request = frontend_reader.read(8192).await.expect("invalid request");
//!     let (request, mut frontend_body_reader) = frontend_request.into_parts();
//!
//!     let body = frontend_body_reader.read(20).await.unwrap().unwrap();
//!     assert_eq!(b"0123456789", body.as_ref());
//!     let frontend_reader = frontend_body_reader.drain().await.unwrap();
//!
//!     // Read the second request in the pipeline.
//!     let frontend_request = frontend_reader.read(128).await.expect("invalid request");
//!     let (request, mut frontend_body_reader) = frontend_request.into_parts();
//!
//!     let body = frontend_body_reader.read(20).await.unwrap().unwrap();
//!     assert_eq!(b"abcde", body.as_ref());
//! }
//! ```

use bytes::Bytes;
use jrpxy_body::{
    BodyReadMode, BodyReader, BodyReaderKind, BodylessBodyReader, ChunkedBodyReader,
    ContentLengthBodyReader, PeekableBodyReader,
};
use jrpxy_http_message::{
    framing::ParsedFraming,
    header::Headers,
    message::{ParseSlots, Request},
};
use jrpxy_util::io_buffer::IoBuffer;
use tokio::io::AsyncReadExt;

use crate::error::{FrontendError, FrontendResult};

/// Reads a request from a frontend reader. When `read()` is called, this type
/// is consumed, and a [`FrontendRequest`] is returned. This can be used to
/// inspect the request and peek the body.
#[derive(Debug)]
pub struct FrontendReader<I> {
    io_buffer: IoBuffer<I>,
    parse_slots: ParseSlots,
}

impl<I> FrontendReader<I> {
    pub fn into_inner(self) -> IoBuffer<I> {
        let Self {
            io_buffer,
            parse_slots: _,
        } = self;
        io_buffer
    }
    /// An immutable slice of the internal buffer already read from the IO type,
    /// but not yet consumed.
    pub fn as_buf_slice(&self) -> &[u8] {
        self.io_buffer.as_bytes()
    }
    /// An immutable reference to the underlying IO type.
    pub fn as_inner(&self) -> &I {
        self.io_buffer.as_io()
    }
}

impl<I: AsyncReadExt + Unpin> FrontendReader<I> {
    const HEAD_FILL_LEN: usize = 4096;

    /// Create a new [`FrontendReader`].
    pub fn new(io: I, max_headers: usize) -> Self {
        Self::new_with_buffer(
            IoBuffer::new_with_fill_len(io, Self::HEAD_FILL_LEN),
            max_headers,
        )
    }

    pub fn new_with_buffer(io: IoBuffer<I>, max_headers: usize) -> FrontendReader<I> {
        Self {
            io_buffer: io,
            parse_slots: ParseSlots::new(max_headers),
        }
    }

    /// Wait for the full frontend head to be available.
    pub(crate) async fn head(&mut self, max_head_length: usize) -> FrontendResult<Request> {
        loop {
            if let Some(req) = self
                .parse_slots
                .parse_request(self.io_buffer.as_buffer_mut())
                .map_err(FrontendError::HttpRequestParseError)?
            {
                return Ok(req);
            } else if self.io_buffer.len() >= max_head_length {
                return Err(FrontendError::MaxHeadLenExceeded(
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
                .map_err(FrontendError::ReadError)?;
            if 0 == len {
                return if first_read {
                    // if the first attempt to read data into the buffer for a
                    // request finds EOF, the frontend has gone away and won't be
                    // sending us more data in the pipeline.
                    Err(FrontendError::FirstReadEOF)
                } else {
                    Err(FrontendError::UnexpectedEOF)
                };
            }
        }
    }

    /// Read a request from the frontend.
    pub async fn read(mut self, max_head_length: usize) -> FrontendResult<FrontendRequest<I>> {
        let req = self.head(max_head_length).await?;
        let Self {
            io_buffer,
            parse_slots,
        } = self;

        let reader = match req.framing()? {
            ParsedFraming::Length(cl) => {
                FrontendBodyReader::new(io_buffer, BodyReadMode::ContentLength(cl), parse_slots)
            }
            ParsedFraming::Chunked => {
                FrontendBodyReader::new(io_buffer, BodyReadMode::Chunk, parse_slots)
            }
            ParsedFraming::NoFraming => {
                FrontendBodyReader::new(io_buffer, BodyReadMode::Bodyless, parse_slots)
            }
        };

        Ok(FrontendRequest { req, reader })
    }
}

/// A request read from a [`FrontendReader`]. Provides access to the underlying
/// request, and allows the request body, if any, to be peeked before it is
/// consumed or drained.
///
/// Use [`FrontendRequest::into_parts`] to retrieve the [`Request`] and
/// [`FrontendBodyReader`].
pub struct FrontendRequest<I> {
    req: Request,
    reader: FrontendBodyReader<I>,
}

impl<I> FrontendRequest<I> {
    /// Access the underlying [`Request`] without taking ownership.
    pub fn req(&self) -> &Request {
        &self.req
    }

    /// Acess the underlying [`Request`] as mutable.
    pub fn req_mut(&mut self) -> &mut Request {
        &mut self.req
    }

    /// Split into the [`Request`] and [`FrontendBodyReader`].
    pub fn into_parts(self) -> (Request, FrontendBodyReader<I>) {
        let Self { req, reader } = self;
        (req, reader)
    }

    /// Indicates the mode with which the body will be read.
    pub fn mode(&self) -> BodyReadMode {
        self.reader.mode()
    }
}

/// Frontend wrapper for [`BodylessBodyReader`].
pub struct FrontendBodylessBodyReader<I> {
    inner: BodylessBodyReader<I>,
}

impl<I: AsyncReadExt + Unpin> FrontendBodylessBodyReader<I> {
    pub fn drain(self) -> FrontendResult<FrontendReader<I>> {
        let (io, parse_slots) = self.inner.drain();
        Ok(FrontendReader {
            io_buffer: io,
            parse_slots,
        })
    }
}

/// Frontend wrapper for [`ContentLengthBodyReader`].
pub struct FrontendContentLengthBodyReader<I> {
    inner: ContentLengthBodyReader<I>,
}

impl<I: AsyncReadExt + Unpin> FrontendContentLengthBodyReader<I> {
    pub async fn read(&mut self, max_len: usize) -> FrontendResult<Option<Bytes>> {
        self.inner
            .read(max_len)
            .await
            .map_err(FrontendError::BodyReadError)
    }

    pub async fn drain(self) -> FrontendResult<FrontendReader<I>> {
        let (io, parse_slots) = self
            .inner
            .drain()
            .await
            .map_err(FrontendError::BodyReadError)?;
        Ok(FrontendReader {
            io_buffer: io,
            parse_slots,
        })
    }
}

/// Frontend wrapper for [`ChunkedBodyReader`]. Draining it returns both a
/// new [`FrontendReader`] and the trailers from the chunked body.
pub struct FrontendChunkedBodyReader<I> {
    inner: ChunkedBodyReader<I>,
}

impl<I> FrontendChunkedBodyReader<I> {
    pub fn drained(&self) -> bool {
        self.inner.drained()
    }
}

impl<I: AsyncReadExt + Unpin> FrontendChunkedBodyReader<I> {
    pub async fn read(&mut self, max_len: usize) -> FrontendResult<Option<Bytes>> {
        self.inner
            .read(max_len)
            .await
            .map_err(FrontendError::BodyReadError)
    }

    pub async fn drain(self) -> FrontendResult<(FrontendReader<I>, Headers)> {
        let (io, parse_slots, trailers) = self
            .inner
            .drain()
            .await
            .map_err(FrontendError::BodyReadError)?;
        Ok((
            FrontendReader {
                io_buffer: io,
                parse_slots,
            },
            trailers,
        ))
    }
}

/// The body reader kind for a frontend request, typed so that draining
/// always produces a [`FrontendReader`] without exposing raw IO.
pub enum FrontendBodyReaderKind<I> {
    Bodyless(FrontendBodylessBodyReader<I>),
    CL(FrontendContentLengthBodyReader<I>),
    TE(FrontendChunkedBodyReader<I>),
}

impl<I: AsyncReadExt + Unpin> FrontendBodyReaderKind<I> {
    pub async fn read(&mut self, max_len: usize) -> FrontendResult<Option<Bytes>> {
        match self {
            FrontendBodyReaderKind::Bodyless(_) => Ok(None),
            FrontendBodyReaderKind::CL(r) => r.read(max_len).await,
            FrontendBodyReaderKind::TE(r) => r.read(max_len).await,
        }
    }

    /// Drain the body and return the next [`FrontendReader`], discarding
    /// any trailers. Use the `TE` variant directly to access trailers.
    pub async fn drain(self) -> FrontendResult<FrontendReader<I>> {
        match self {
            FrontendBodyReaderKind::Bodyless(r) => r.drain(),
            FrontendBodyReaderKind::CL(r) => r.drain().await,
            FrontendBodyReaderKind::TE(r) => {
                let (reader, _trailers) = r.drain().await?;
                Ok(reader)
            }
        }
    }
}

/// A frontend request body reader.
pub struct FrontendBodyReader<I> {
    reader: BodyReader<I>,
}

impl<I> FrontendBodyReader<I> {
    pub fn mode(&self) -> BodyReadMode {
        self.reader.mode()
    }

    pub fn into_kind(self) -> FrontendBodyReaderKind<I> {
        let Self { reader } = self;
        match reader.into_kind() {
            BodyReaderKind::Bodyless(inner) => {
                FrontendBodyReaderKind::Bodyless(FrontendBodylessBodyReader { inner })
            }
            BodyReaderKind::CL(inner) => {
                FrontendBodyReaderKind::CL(FrontendContentLengthBodyReader { inner })
            }
            BodyReaderKind::TE(inner) => {
                FrontendBodyReaderKind::TE(FrontendChunkedBodyReader { inner })
            }
        }
    }
}

impl<I: AsyncReadExt + Unpin> FrontendBodyReader<I> {
    fn new(io: IoBuffer<I>, mode: BodyReadMode, parse_slots: ParseSlots) -> Self {
        Self {
            reader: BodyReader::new(io, mode, parse_slots),
        }
    }

    /// Read up to `max_len` bytes from the body. Returns `None` when the body
    /// is complete.
    pub async fn read(&mut self, max_len: usize) -> FrontendResult<Option<Bytes>> {
        self.reader
            .read(max_len)
            .await
            .map_err(FrontendError::BodyReadError)
    }

    /// Returns true when the body is fully drained.
    pub fn drained(&self) -> bool {
        self.reader.drained()
    }

    /// Convert this reader into a [`FrontendPeekableBodyReader`] that supports
    /// peeking at body bytes without consuming them.
    pub fn peekable(self) -> FrontendPeekableBodyReader<I> {
        FrontendPeekableBodyReader::new(self)
    }

    /// Drain all remaining bytes from the body and return a new
    /// [`FrontendReader`] ready to read the next request in the pipeline.
    pub async fn drain(self) -> FrontendResult<FrontendReader<I>> {
        let Self { reader } = self;
        let (io, parse_slots) = reader.drain().await.map_err(FrontendError::BodyReadError)?;
        Ok(FrontendReader {
            io_buffer: io,
            parse_slots,
        })
    }
}

/// A frontend request body reader with peek support.
pub struct FrontendPeekableBodyReader<I> {
    reader: PeekableBodyReader<I>,
}

impl<I> FrontendPeekableBodyReader<I> {
    pub fn new(reader: FrontendBodyReader<I>) -> Self {
        let FrontendBodyReader { reader } = reader;
        Self {
            reader: reader.peekable(),
        }
    }

    pub fn mode(&self) -> BodyReadMode {
        self.reader.mode()
    }
}

impl<I: AsyncReadExt + Unpin> FrontendPeekableBodyReader<I> {
    /// Peek bytes from the body. Repeated calls to peek will start from the
    /// same offset until the bytes are read from the body. That is, peek always
    /// starts with the same data until that data is read out.
    pub async fn peek(&mut self, max_len: usize) -> FrontendResult<(bool, &[u8])> {
        self.reader
            .peek(max_len)
            .await
            .map_err(FrontendError::BodyReadError)
    }

    /// Read up to `max_len` bytes from the body. Returns `None` when the body
    /// is complete.
    pub async fn read(&mut self, max_len: usize) -> FrontendResult<Option<Bytes>> {
        self.reader
            .read(max_len)
            .await
            .map_err(FrontendError::BodyReadError)
    }

    /// Returns true when the body is fully drained.
    pub fn drained(&self) -> bool {
        self.reader.drained()
    }

    /// Drain all remaining bytes from the body and return a new
    /// [`FrontendReader`] ready to read the next request in the pipeline.
    pub async fn drain(self) -> FrontendResult<FrontendReader<I>> {
        let Self { reader } = self;
        let (io, parse_slots) = reader.drain().await.map_err(FrontendError::BodyReadError)?;
        Ok(FrontendReader {
            io_buffer: io,
            parse_slots,
        })
    }
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use bytes::BytesMut;
    use jrpxy_body::BodyError;
    use tokio::io::AsyncWriteExt;

    use crate::{error::FrontendError, reader::FrontendReader};

    #[tokio::test]
    async fn read_cl_frontend() {
        let buf = b"\
            GET /cl HTTP/1.1\r\n\
            Content-Length: 5\r\n\
            \r\n\
            01234\
            extra bytes\
            ";

        let reader = FrontendReader::new(buf.as_slice(), 256);
        let (req, mut body) = reader
            .read(128)
            .await
            .expect("can't break into parts")
            .into_parts();

        assert_eq!(&b"GET"[..], req.method());
        assert_eq!(&b"/cl"[..], req.path());

        assert_eq!(&b"0"[..], body.read(1).await.unwrap().unwrap());
        assert_eq!(&b""[..], body.read(0).await.unwrap().unwrap());
        assert_eq!(&b"1234"[..], body.read(4).await.unwrap().unwrap());

        // once the body is fully drained, always return none.
        assert!(body.read(1).await.unwrap().is_none());
        assert!(body.read(0).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn peek_cl_frontend() {
        let buf = b"\
            GET /cl HTTP/1.1\r\n\
            Content-Length: 5\r\n\
            \r\n\
            01234\
            extra bytes\
            ";

        let reader = FrontendReader::new(buf.as_slice(), 256);
        let (req, body) = reader
            .read(128)
            .await
            .expect("can't break into parts")
            .into_parts();
        let mut body = body.peekable();

        assert_eq!(&b"GET"[..], req.method());
        assert_eq!(&b"/cl"[..], req.path());

        // same as read tests, but peek first.
        let (complete, x) = body.peek(5).await.expect("can peek");
        assert!(complete);
        assert_eq!(&b"01234"[..], x);

        assert_eq!(&b"0"[..], body.read(1).await.unwrap().unwrap());
        assert_eq!(&b""[..], body.read(0).await.unwrap().unwrap());
        assert_eq!(&b"1234"[..], body.read(4).await.unwrap().unwrap());

        // peek again after reading all the data, make sure we're complete with
        // no data.
        let (complete, x) = body.peek(5).await.expect("can peek");
        assert!(complete);
        assert_eq!(&b""[..], x);

        // once the body is fully drained, always return none.
        assert!(body.read(1).await.unwrap().is_none());
        assert!(body.read(0).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn read_te_frontend() {
        let buf = b"\
            GET /te HTTP/1.1\r\n\
            Transfer-Encoding: chunked\r\n\
            \r\n\
            3;extensions=suck\r\n\
            012\r\n\
            2\r\n\
            34\r\n\
            0\r\n\
            first-trailer: 1\r\n\
            second-trailer: 2\r\n\
            \r\n\
            extra bytes\
            ";

        let reader = FrontendReader::new(buf.as_slice(), 256);
        let (req, mut body) = reader
            .read(128)
            .await
            .expect("can't break into parts")
            .into_parts();

        assert_eq!(&b"GET"[..], req.method());
        assert_eq!(&b"/te"[..], req.path());

        // read one byte out
        assert_eq!(&b"0"[..], body.read(1).await.unwrap().unwrap());
        // read zero bytes out
        assert_eq!(&b""[..], body.read(0).await.unwrap().unwrap());
        // try to read out 4 bytes, but only 2 remain in the chunk
        assert_eq!(&b"12"[..], body.read(4).await.unwrap().unwrap());
        // try to read out 4 bytes, but the next chunk is only two bytes
        assert_eq!(&b"34"[..], body.read(4).await.unwrap().unwrap());

        // once the body is fully drained, always return none.
        assert!(body.read(1).await.unwrap().is_none());
        assert!(body.read(0).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn pipeline() {
        let buf = b"\
            GET /te HTTP/1.1\r\n\
            Transfer-Encoding: chunked\r\n\
            \r\n\
            3\r\n\
            012\r\n\
            2\r\n\
            34\r\n\
            0\r\n\
            first-trailer: 1\r\n\
            second-trailer: 2\r\n\
            \r\n\
            POST /cl HTTP/1.1\r\n\
            Content-Length: 5\r\n\
            \r\n\
            01234\
            ";

        let reader = FrontendReader::new(buf.as_slice(), 256);
        let (req, body) = reader
            .read(128)
            .await
            .expect("can't break into parts")
            .into_parts();
        assert_eq!(&b"GET"[..], req.method());
        assert_eq!(&b"/te"[..], req.path());

        // first request finished. get another reader out for the following request
        let reader = body.drain().await.unwrap();
        let (req, mut body) = reader
            .read(128)
            .await
            .expect("can't break into parts")
            .into_parts();
        assert_eq!(&b"POST"[..], req.method());
        assert_eq!(&b"/cl"[..], req.path());
        let buf = body.read(128).await.unwrap().unwrap();
        assert_eq!(b"01234".as_slice(), buf);
    }

    #[tokio::test]
    async fn partial_head() {
        // missing header \r\n
        let buf = b"\
            GET /cl HTTP/1.1\r\n\
            Some-Header: xxx\
            ";

        let reader = FrontendReader::new(buf.as_slice(), 256);
        assert!(matches!(
            reader.read(128).await,
            Err(FrontendError::UnexpectedEOF)
        ));
    }

    #[tokio::test]
    async fn partial_cl() {
        // stops part way through body
        let buf = b"\
            GET /cl HTTP/1.1\r\n\
            Content-Length: 5\r\n\
            \r\n\
            012\
            ";

        let reader = FrontendReader::new(buf.as_slice(), 256);
        let (req, mut body) = reader
            .read(128)
            .await
            .expect("can't break into parts")
            .into_parts();

        assert_eq!(&b"GET"[..], req.method());
        assert_eq!(&b"/cl"[..], req.path());

        assert_eq!(&b"0"[..], body.read(1).await.unwrap().unwrap());
        assert_eq!(&b""[..], body.read(0).await.unwrap().unwrap());
        assert_eq!(&b"12"[..], body.read(4).await.unwrap().unwrap());

        assert!(matches!(
            body.read(4).await,
            Err(FrontendError::BodyReadError(BodyError::UnexpectedEOF)),
        ));
    }

    #[tokio::test]
    async fn partial_te() {
        // stops part way through chunk extension
        let buf = b"\
            GET /te HTTP/1.1\r\n\
            Transfer-Encoding: chunked\r\n\
            \r\n\
            3;extensions=su\
            ";

        let reader = FrontendReader::new(buf.as_slice(), 256);
        let (_req, mut body) = reader
            .read(128)
            .await
            .expect("can't break into parts")
            .into_parts();

        assert!(matches!(
            body.read(128).await,
            Err(FrontendError::BodyReadError(BodyError::UnexpectedEOF)),
        ));

        // stops part way through chunk
        let buf = b"\
            GET /te HTTP/1.1\r\n\
            Transfer-Encoding: chunked\r\n\
            \r\n\
            3;extensions=suck\r\n\
            01\
            ";

        let reader = FrontendReader::new(buf.as_slice(), 256);
        let (_req, body) = reader
            .read(128)
            .await
            .expect("can't break into parts")
            .into_parts();
        let mut body = body;

        assert_eq!(&b"01"[..], body.read(128).await.unwrap().unwrap());
        assert!(matches!(
            body.read(128).await,
            Err(FrontendError::BodyReadError(BodyError::UnexpectedEOF)),
        ));

        // stops part way through trailers
        let buf = b"\
            GET /te HTTP/1.1\r\n\
            Transfer-Encoding: chunked\r\n\
            \r\n\
            3;extensions=suck\r\n\
            012\r\n\
            2\r\n\
            34\r\n\
            0\r\n\
            first-trai\
            ";

        let reader = FrontendReader::new(buf.as_slice(), 256);
        let (req, body) = reader
            .read(128)
            .await
            .expect("can't break into parts")
            .into_parts();
        let mut body = body;

        assert_eq!(&b"GET"[..], req.method());
        assert_eq!(&b"/te"[..], req.path());

        // read one byte out
        assert_eq!(&b"0"[..], body.read(1).await.unwrap().unwrap());
        // read zero bytes out
        assert_eq!(&b""[..], body.read(0).await.unwrap().unwrap());
        // try to read out 4 bytes, but only 2 remain in the chunk
        assert_eq!(&b"12"[..], body.read(4).await.unwrap().unwrap());
        // try to read out 4 bytes, but the next chunk is only two bytes
        assert_eq!(&b"34"[..], body.read(4).await.unwrap().unwrap());

        assert!(matches!(
            body.read(128).await,
            Err(FrontendError::BodyReadError(BodyError::UnexpectedEOF)),
        ));
    }

    #[tokio::test]
    async fn trickle_cl() {
        let (mut left, right) = tokio::io::duplex(3);

        // this task feeds bytes slowly
        let w = tokio::spawn(async move {
            let buf = b"\
                GET /cl HTTP/1.1\r\n\
                Content-Length: 5\r\n\
                \r\n\
                01234\
                extra bytes\
                ";

            for c in buf.chunks(3) {
                if left.write_all(c).await.is_err() {
                    break;
                }
            }
        });

        let r = tokio::spawn(tokio::time::timeout(Duration::from_secs(10), async move {
            let reader = FrontendReader::new(right, 256);
            let (req, body) = reader
                .read(128)
                .await
                .expect("can't break into parts")
                .into_parts();
            let mut body = body;

            assert_eq!(&b"GET"[..], req.method());
            assert_eq!(&b"/cl"[..], req.path());

            let mut body_buf = BytesMut::with_capacity(64);
            while let Some(buf) = body.read(64).await.unwrap() {
                body_buf.extend(buf);
            }

            assert_eq!(&b"01234"[..], body_buf);
        }));

        let () = w.await.unwrap();
        let () = r.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn trickle_te() {
        let (mut left, right) = tokio::io::duplex(3);

        // this task feeds bytes slowly
        let w = tokio::spawn(async move {
            let buf = b"\
                GET /te HTTP/1.1\r\n\
                Transfer-Encoding: chunked\r\n\
                \r\n\
                3;extensions=suck\r\n\
                ABC\r\n\
                2\r\n\
                DE\r\n\
                0\r\n\
                first-trailer: 1\r\n\
                second-trailer: 2\r\n\
                \r\n\
                extra bytes\
                ";

            for c in buf.chunks(3) {
                if left.write_all(c).await.is_err() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    break;
                }
            }
        });

        let r = tokio::spawn(tokio::time::timeout(Duration::from_secs(10), async move {
            let reader = FrontendReader::new(right, 256);
            let (req, body) = reader
                .read(128)
                .await
                .expect("can't break into parts")
                .into_parts();
            let mut body = body;

            assert_eq!(&b"GET"[..], req.method());
            assert_eq!(&b"/te"[..], req.path());

            let mut body_buf = BytesMut::with_capacity(64);
            while let Some(buf) = body.read(64).await.unwrap() {
                body_buf.extend(buf);
            }

            assert_eq!(&b"ABCDE"[..], body_buf);
        }));

        let () = w.await.unwrap();
        let () = r.await.unwrap().unwrap();
    }
}
