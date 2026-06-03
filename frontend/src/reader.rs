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
//!     let frontend_request = frontend_reader.read(8192, 8192).await.expect("invalid request");
//!     let (request, mut frontend_body_reader) = frontend_request.into_parts();
//!
//!     let body = frontend_body_reader.read(20).await.unwrap().unwrap();
//!     assert_eq!(b"0123456789", body.as_ref());
//!     let frontend_reader = frontend_body_reader.drain().await.unwrap().unwrap();
//!
//!     // Read the second request in the pipeline.
//!     let frontend_request = frontend_reader.read(128, 128).await.expect("invalid request");
//!     let (request, mut frontend_body_reader) = frontend_request.into_parts();
//!
//!     let body = frontend_body_reader.read(20).await.unwrap().unwrap();
//!     assert_eq!(b"abcde", body.as_ref());
//! }
//! ```

use bytes::Bytes;
use jrpxy_body::reader::{BodylessBodyReader, ChunkedBodyReader, ContentLengthBodyReader};
use jrpxy_http_message::{
    framing::ParsedFraming,
    header::Headers,
    message::{ParseSlots, Request},
};
use jrpxy_util::io_buffer::BytesReader;
use tokio::io::AsyncReadExt;

use crate::error::{FrontendError, FrontendResult};

/// Reads a request from a frontend reader. When `read()` is called, this type
/// is consumed, and a [`FrontendRequest`] is returned.
#[derive(Debug)]
pub struct FrontendReader<I> {
    reader: BytesReader<I>,
    parse_slots: ParseSlots,
}

impl<I> FrontendReader<I> {
    /// Convert the reader into the inner reader.
    pub fn into_inner(self) -> BytesReader<I> {
        let Self {
            reader,
            parse_slots: _,
        } = self;
        reader
    }
    /// An immutable slice of the internal buffer already read from the IO type,
    /// but not yet consumed.
    pub fn as_buf_slice(&self) -> &[u8] {
        self.reader.as_bytes()
    }
}

impl<I> FrontendReader<I>
where
    I: AsyncReadExt + Unpin,
{
    /// Create a new [`FrontendReader`].
    pub fn new(reader: I, max_headers: usize) -> Self {
        Self::new_with_buffer(BytesReader::new(reader), max_headers)
    }

    /// Create a new [`FrontendReader`], but with a specified maximum number of
    /// headers and a previously defined [`BytesReader`].
    pub fn new_with_buffer(reader: BytesReader<I>, max_headers: usize) -> FrontendReader<I> {
        Self {
            reader,
            parse_slots: ParseSlots::new(max_headers),
        }
    }

    /// Wait for the full frontend head to be available.
    async fn head(&mut self, max_head_length: usize) -> FrontendResult<Request> {
        loop {
            if let Some(req) = self
                .parse_slots
                .parse_request(&mut self.reader)
                .map_err(FrontendError::HttpRequestParseError)?
            {
                return Ok(req);
            } else if self.reader.len() >= max_head_length {
                return Err(FrontendError::MaxHeadLenExceeded(
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
    pub async fn read(
        mut self,
        max_head_length: usize,
        max_chunk_header_length: usize,
    ) -> FrontendResult<FrontendRequest<I>> {
        let req = self.head(max_head_length).await?;
        let Self {
            reader,
            parse_slots,
        } = self;

        let recyclable = req.headers().connection_is_persistent(req.version())?;

        let reader = match req.framing()? {
            ParsedFraming::Length(cl) => FrontendBodyReader::CL(FrontendContentLengthBodyReader {
                inner: ContentLengthBodyReader::new(cl, reader, parse_slots),
                recyclable,
            }),
            ParsedFraming::Chunked => FrontendBodyReader::TE(FrontendChunkedBodyReader {
                inner: ChunkedBodyReader::new(
                    reader,
                    parse_slots,
                    max_chunk_header_length,
                    max_head_length,
                ),
                recyclable,
            }),
            ParsedFraming::NoFraming => FrontendBodyReader::Bodyless(FrontendBodylessBodyReader {
                inner: BodylessBodyReader::new(reader, parse_slots),
                recyclable,
            }),
        };

        Ok(FrontendRequest { req, reader })
    }
}

/// A request read from a [`FrontendReader`]. Provides access to the underlying
/// request.
pub struct FrontendRequest<I> {
    req: Request,
    reader: FrontendBodyReader<I>,
}

impl<I> FrontendRequest<I> {
    /// Access the underlying [`Request`] without taking ownership.
    pub fn req(&self) -> &Request {
        &self.req
    }

    /// Access the underlying [`Request`] as mutable.
    pub fn req_mut(&mut self) -> &mut Request {
        &mut self.req
    }

    pub fn recyclable(&self) -> bool {
        self.reader.recyclable()
    }

    /// Split into the [`Request`] and [`FrontendBodyReader`].
    pub fn into_parts(self) -> (Request, FrontendBodyReader<I>) {
        let Self { req, reader } = self;
        (req, reader)
    }
}

/// Frontend wrapper for [`BodylessBodyReader`].
pub struct FrontendBodylessBodyReader<I> {
    inner: BodylessBodyReader<I>,
    recyclable: bool,
}

impl<I> FrontendBodylessBodyReader<I>
where
    I: AsyncReadExt + Unpin,
{
    /// Drain the body reader. Returns `Some(FrontendReader)` when the
    /// frontend connection is eligible for reuse; `None` when the request
    /// head signaled that the connection must be closed.
    pub fn drain(self) -> FrontendResult<Option<FrontendReader<I>>> {
        let Self { inner, recyclable } = self;
        let (reader, parse_slots) = inner.drain();
        if !recyclable {
            return Ok(None);
        }
        Ok(Some(FrontendReader {
            reader,
            parse_slots,
        }))
    }
}

/// Frontend wrapper for [`ContentLengthBodyReader`].
pub struct FrontendContentLengthBodyReader<I> {
    inner: ContentLengthBodyReader<I>,
    recyclable: bool,
}

impl<I> FrontendContentLengthBodyReader<I> {
    /// The total expected body length.
    pub fn content_length(&self) -> u64 {
        self.inner.content_length()
    }
}

impl<I> FrontendContentLengthBodyReader<I>
where
    I: AsyncReadExt + Unpin,
{
    /// Read up to `max_len` bytes from the body. Returns `Ok(None)` when the
    /// body is completely read.
    pub async fn read(&mut self, max_len: usize) -> FrontendResult<Option<Bytes>> {
        self.inner
            .read(max_len)
            .await
            .map_err(FrontendError::BodyReadError)
    }

    /// Drain the body, discarding any unread bytes. Returns
    /// `Some(FrontendReader)` when the frontend connection is eligible for
    /// reuse; `None` when the request head signaled that the connection must
    /// be closed.
    pub async fn drain(self) -> FrontendResult<Option<FrontendReader<I>>> {
        let Self { inner, recyclable } = self;
        let (reader, parse_slots) = inner.drain().await.map_err(FrontendError::BodyReadError)?;
        if !recyclable {
            return Ok(None);
        }
        Ok(Some(FrontendReader {
            reader,
            parse_slots,
        }))
    }
}

/// Frontend wrapper for [`ChunkedBodyReader`]. Draining it returns both a
/// new [`FrontendReader`] and the trailers from the chunked body.
pub struct FrontendChunkedBodyReader<I> {
    inner: ChunkedBodyReader<I>,
    recyclable: bool,
}

impl<I> FrontendChunkedBodyReader<I> {
    /// True when the body is drained.
    pub fn drained(&self) -> bool {
        self.inner.drained()
    }
}

impl<I> FrontendChunkedBodyReader<I>
where
    I: AsyncReadExt + Unpin,
{
    /// Read up to `max_len` bytes from the body. Returns `Ok(None)` when the
    /// body is completely read.
    pub async fn read(&mut self, max_len: usize) -> FrontendResult<Option<Bytes>> {
        self.inner
            .read(max_len)
            .await
            .map_err(FrontendError::BodyReadError)
    }

    /// Drain the body, discarding any unread bytes. The trailers are always
    /// returned; the [`FrontendReader`] is `Some` only when the frontend
    /// connection is eligible for reuse.
    pub async fn drain(self) -> FrontendResult<(Option<FrontendReader<I>>, Headers)> {
        let Self { inner, recyclable } = self;
        let (reader, parse_slots, trailers) =
            inner.drain().await.map_err(FrontendError::BodyReadError)?;
        let next = recyclable.then(|| FrontendReader {
            reader,
            parse_slots,
        });
        Ok((next, trailers))
    }
}

/// A frontend request body reader. Each variant corresponds to a different
/// framing mode.
pub enum FrontendBodyReader<I> {
    Bodyless(FrontendBodylessBodyReader<I>),
    CL(FrontendContentLengthBodyReader<I>),
    TE(FrontendChunkedBodyReader<I>),
}

impl<I> FrontendBodyReader<I> {
    pub fn recyclable(&self) -> bool {
        match self {
            FrontendBodyReader::Bodyless(r) => r.recyclable,
            FrontendBodyReader::CL(r) => r.recyclable,
            FrontendBodyReader::TE(r) => r.recyclable,
        }
    }
}

impl<I> FrontendBodyReader<I>
where
    I: AsyncReadExt + Unpin,
{
    /// Read up to `max_len` bytes from the body. Returns `None` when the body
    /// is complete.
    pub async fn read(&mut self, max_len: usize) -> FrontendResult<Option<Bytes>> {
        match self {
            FrontendBodyReader::Bodyless(_) => Ok(None),
            FrontendBodyReader::CL(r) => r.read(max_len).await,
            FrontendBodyReader::TE(r) => r.read(max_len).await,
        }
    }

    /// Returns true when the body is fully drained.
    pub fn drained(&self) -> bool {
        match self {
            FrontendBodyReader::Bodyless(_) => true,
            FrontendBodyReader::CL(r) => r.inner.drained(),
            FrontendBodyReader::TE(r) => r.inner.drained(),
        }
    }

    /// Drain all remaining bytes from the body. Returns `Some(FrontendReader)`
    /// when the frontend connection is eligible for reuse, `None` otherwise.
    pub async fn drain(self) -> FrontendResult<Option<FrontendReader<I>>> {
        match self {
            FrontendBodyReader::Bodyless(r) => r.drain(),
            FrontendBodyReader::CL(r) => r.drain().await,
            FrontendBodyReader::TE(r) => {
                let (reader, _trailers) = r.drain().await?;
                Ok(reader)
            }
        }
    }
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use bytes::BytesMut;
    use jrpxy_body::error::BodyError;
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
            .read(128, 128)
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
            .read(128, 128)
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
            .read(128, 128)
            .await
            .expect("can't break into parts")
            .into_parts();
        assert_eq!(&b"GET"[..], req.method());
        assert_eq!(&b"/te"[..], req.path());

        // first request finished. get another reader out for the following request
        let reader = body
            .drain()
            .await
            .unwrap()
            .expect("frontend should be recyclable");
        let (req, mut body) = reader
            .read(128, 128)
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
            reader.read(128, 128).await,
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
            .read(128, 128)
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
            .read(128, 128)
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
            .read(128, 128)
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
            .read(128, 128)
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
                .read(128, 128)
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
                .read(128, 128)
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
