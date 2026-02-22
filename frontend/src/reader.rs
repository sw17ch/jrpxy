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
//!     let frontend_reader = FrontendReader::new(&buf[..], 8192);
//!     let frontend_request = frontend_reader.read().await.expect("invalid request");
//!     let (request, mut frontend_body_reader) = frontend_request.into_parts();
//!
//!     let body = frontend_body_reader.read(20).await.unwrap().unwrap();
//!     assert_eq!(b"0123456789", body.as_ref());
//!     let frontend_reader = frontend_body_reader.drain().await.unwrap();
//!
//!     // Read the second request in the pipeline
//!     let frontend_request = frontend_reader.read().await.expect("invalid request");
//!     let (request, mut frontend_body_reader) = frontend_request.into_parts();
//!
//!     let body = frontend_body_reader.read(20).await.unwrap().unwrap();
//!     assert_eq!(b"abcde", body.as_ref());
//! }
//! ```

use bytes::Bytes;
use bytes::BytesMut;
use jrpxy_body::{BodyReadMode, BodyReader};
use jrpxy_http_message::{framing::HeadFraming, message::Request};
use jrpxy_util::buffer;
use tokio::io::AsyncReadExt;

use crate::error::{FrontendError, FrontendResult};

/// Reads a request from a frontend reader. When `read()` is called, this type
/// is consumed, and a [`FrontendRequest`] is returned. This can be used to
/// inspect the request and peek the body.
pub struct FrontendReader<I> {
    io: I,
    max_head_length: usize,
    buffer: buffer::Buffer,
}

impl<I: AsyncReadExt + Unpin> FrontendReader<I> {
    pub(crate) const MAX_HEADERS: usize = 256;

    /// Createa a new [`FrontendReader`].
    pub fn new(io: I, max_head_length: usize) -> Self {
        Self::new_with_buffer(io, max_head_length, BytesMut::new())
    }

    pub(crate) fn new_with_buffer(
        io: I,
        max_head_length: usize,
        buffer: BytesMut,
    ) -> FrontendReader<I> {
        Self {
            io,
            max_head_length,
            buffer: buffer::Buffer::new(buffer),
        }
    }

    /// Wait for the full frontend head to be available
    pub(crate) async fn head(&mut self) -> FrontendResult<Request> {
        loop {
            if let Some(req) = Request::parse(&mut self.buffer, Self::MAX_HEADERS)
                .map_err(FrontendError::HttpRequestParseError)?
            {
                return Ok(req);
            } else if self.buffer.len() >= self.max_head_length {
                return Err(FrontendError::MaxHeadLenExceeded(
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
    pub async fn read(mut self) -> FrontendResult<FrontendRequest<I>> {
        let req = self.head().await?;
        let Self {
            io,
            max_head_length,
            buffer,
        } = self;

        let reader = match req.framing()? {
            HeadFraming::Length(cl) => FrontendBodyReader::new(
                io,
                max_head_length,
                buffer.into_inner(),
                BodyReadMode::ContentLength(cl),
            ),
            HeadFraming::Chunked => FrontendBodyReader::new(
                io,
                max_head_length,
                buffer.into_inner(),
                BodyReadMode::Chunk,
            ),
            HeadFraming::NoFraming => FrontendBodyReader::new(
                io,
                max_head_length,
                buffer.into_inner(),
                BodyReadMode::ContentLength(0),
            ),
        };

        Ok(FrontendRequest { req, reader })
    }
}

/// A request read from a [`FrontendReader`]. Provides access to the underlying
/// request, and allows the request body, if any, to be peeked before it is
/// consumed or drained.
///
/// Use [`FrontendRequest::into_parts`] to retrieve the [`Request`] and
/// [`FrontendRequestBody`].
pub struct FrontendRequest<I> {
    pub(crate) req: Request,
    pub(crate) reader: FrontendBodyReader<I>,
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

    /// Split into the [`Request`] and [`FrontendRequestBody`].
    pub fn into_parts(self) -> (Request, FrontendBodyReader<I>) {
        let Self { req, reader } = self;
        (req, reader)
    }

    /// Indicates the mode with which the body will be read.
    pub fn mode(&self) -> BodyReadMode {
        self.reader.mode()
    }
}

impl<I: AsyncReadExt + Unpin> FrontendRequest<I> {
    /// Peek bytes of the body, but do not drain them. Peek always starts from
    /// the beginning of the body.
    pub async fn peek_body(&mut self, len: usize) -> Result<(bool, &[u8]), FrontendError> {
        self.reader.peek(len).await
    }
}

/// A frontend request body reader.
pub struct FrontendBodyReader<I> {
    pub(crate) max_head_length: usize,
    pub(crate) reader: BodyReader<I>,
}

impl<I> FrontendBodyReader<I> {
    pub fn mode(&self) -> BodyReadMode {
        self.reader.mode()
    }
}

impl<I: AsyncReadExt + Unpin> FrontendBodyReader<I> {
    pub(crate) fn new(io: I, max_head_length: usize, buffer: BytesMut, mode: BodyReadMode) -> Self {
        Self {
            max_head_length,
            reader: BodyReader::new(io, buffer, mode),
        }
    }

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
        let max_head_len = self.max_head_length;
        let (io, buffer) = self
            .reader
            .drain()
            .await
            .map_err(FrontendError::BodyReadError)?;
        Ok(FrontendReader::new_with_buffer(
            io,
            max_head_len,
            buffer.into_inner(),
        ))
    }
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use bytes::BytesMut;
    use jrpxy_body::BodyError;
    use tokio::io::AsyncWriteExt;

    use crate::{
        error::FrontendError,
        reader::{FrontendBodyReader, FrontendReader},
    };

    #[tokio::test]
    async fn read_cl_frontend() {
        let buf = b"\
            GET /cl HTTP/1.1\r\n\
            Content-Length: 5\r\n\
            \r\n\
            01234\
            extra bytes\
            ";

        let reader = FrontendReader::new(buf.as_slice(), 128);
        let (req, body) = reader
            .read()
            .await
            .expect("can't break into parts")
            .into_parts();
        let mut body = FrontendBodyReader::from(body);

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

        let reader = FrontendReader::new(buf.as_slice(), 128);
        let (req, body) = reader
            .read()
            .await
            .expect("can't break into parts")
            .into_parts();
        let mut body = FrontendBodyReader::from(body);

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

        let reader = FrontendReader::new(buf.as_slice(), 128);
        let (req, body) = reader
            .read()
            .await
            .expect("can't break into parts")
            .into_parts();
        let mut body = FrontendBodyReader::from(body);

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

        let reader = FrontendReader::new(buf.as_slice(), 128);
        let (req, body) = reader
            .read()
            .await
            .expect("can't break into parts")
            .into_parts();
        let body = FrontendBodyReader::from(body);

        assert_eq!(&b"GET"[..], req.method());
        assert_eq!(&b"/te"[..], req.path());

        // first request finished. get another reader out for the following request
        let reader = body.drain().await.unwrap();
        let (req, body) = reader
            .read()
            .await
            .expect("can't break into parts")
            .into_parts();
        let mut body = FrontendBodyReader::from(body);
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

        let reader = FrontendReader::new(buf.as_slice(), 128);
        assert!(matches!(
            reader.read().await,
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

        let reader = FrontendReader::new(buf.as_slice(), 128);
        let (req, body) = reader
            .read()
            .await
            .expect("can't break into parts")
            .into_parts();
        let mut body = FrontendBodyReader::from(body);

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

        let reader = FrontendReader::new(buf.as_slice(), 128);
        let (_req, body) = reader
            .read()
            .await
            .expect("can't break into parts")
            .into_parts();
        let mut body = FrontendBodyReader::from(body);

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

        let reader = FrontendReader::new(buf.as_slice(), 128);
        let (_req, body) = reader
            .read()
            .await
            .expect("can't break into parts")
            .into_parts();
        let mut body = FrontendBodyReader::from(body);

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

        let reader = FrontendReader::new(buf.as_slice(), 128);
        let (req, body) = reader
            .read()
            .await
            .expect("can't break into parts")
            .into_parts();
        let mut body = FrontendBodyReader::from(body);

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
            let reader = FrontendReader::new(right, 128);
            let (req, body) = reader
                .read()
                .await
                .expect("can't break into parts")
                .into_parts();
            let mut body = FrontendBodyReader::from(body);

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
            let reader = FrontendReader::new(right, 128);
            let (req, body) = reader
                .read()
                .await
                .expect("can't break into parts")
                .into_parts();
            let mut body = FrontendBodyReader::from(body);

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

    #[tokio::test]
    async fn invalid_te_trailer_trailing_space() {
        // invalid trailing space before \r in first-trailer
        let buf = b"\
            GET /te HTTP/1.1\r\n\
            Transfer-Encoding: chunked\r\n\
            \r\n\
            3;extensions=suck\r\n\
            012\r\n\
            2\r\n\
            34\r\n\
            0\r\n\
            first-trailer: 1 \r\n\
            second-trailer: 2\r\n\
            \r\n\
            extra bytes\
            ";

        let reader = FrontendReader::new(buf.as_slice(), 128);
        let (req, body) = reader
            .read()
            .await
            .expect("can't break into parts")
            .into_parts();
        let mut body = FrontendBodyReader::from(body);

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
            body.read(1).await.unwrap_err(),
            FrontendError::BodyReadError(BodyError::TrailerError(
                jrpxy_body::TrailerError::InvalidFieldValue
            ))
        ));
    }
}
