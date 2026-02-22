//! Tools for reading requests and writing responses to a HTTP/1.x frontend.
//!
//! # Reader Example
//!
//! We can use [`FrontendReader`] to read a stream of requests.
//!
//! ```rust
//! use jrpxy_frontend::FrontendReader;
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
//!
//! # Writer Example
//!
//! We can use [`FrontendWriter`] to write a stream of requests.
//!
//! ```rust
//! use jrpxy_frontend::FrontendWriter;
//! use jrpxy_http_message::{message::ResponseBuilder, version::HttpVersion};
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() {
//!    let mut buf = Vec::new();
//!
//!    // Write the first response
//!    let frontend_writer = FrontendWriter::new(&mut buf);
//!    let mut builder = ResponseBuilder::new(4);
//!    let resp = builder
//!        .with_version(HttpVersion::Http11)
//!        .with_code(200)
//!        .with_reason("Ok")
//!        .build();
//!    let mut frontend_body_writer = frontend_writer
//!        .send_as_content_length(&resp, 5)
//!        .await
//!        .unwrap();
//!    frontend_body_writer.write(b"ab").await.unwrap();
//!    frontend_body_writer.write(b"cde").await.unwrap();
//!    let frontend_writer = frontend_body_writer.finish().await.unwrap();
//!
//!    // Write the second response
//!    let mut builder = ResponseBuilder::new(4);
//!    let resp = builder
//!        .with_version(HttpVersion::Http11)
//!        .with_code(404)
//!        .with_reason("Not Found")
//!        .build();
//!    let mut frontend_body_writer = frontend_writer.send_as_chunked(&resp).await.unwrap();
//!    frontend_body_writer.write(b"01234").await.unwrap();
//!    frontend_body_writer.write(b"567").await.unwrap();
//!    frontend_body_writer.write(b"89").await.unwrap();
//!    let _frontend_writer = frontend_body_writer.finish().await.unwrap();
//!
//!    assert_eq!(
//!        b"\
//!            HTTP/1.1 200 Ok\r\n\
//!            content-length: 5\r\n\
//!            \r\n\
//!            abcde\
//!            HTTP/1.1 404 Not Found\r\n\
//!            transfer-encoding: chunked\r\n\
//!            \r\n\
//!            5\r\n\
//!            01234\r\n\
//!            3\r\n\
//!            567\r\n\
//!            2\r\n\
//!            89\r\n\
//!            0\r\n\
//!            \r\n\
//!            ",
//!        &buf[..]
//!    );
//! }
//! ```

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
};
use jrpxy_util::buffer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Possible errors from interacting with frontends.
#[derive(Debug, thiserror::Error)]
pub enum FrontendError {
    #[error("Failed to write to frontend: {0}")]
    WriteError(io::Error),
    #[error("Failed to write body to frontend: {0}")]
    BodyWriteError(BodyError),
    #[error("Failed to read frontend: {0}")]
    ReadError(io::Error),
    #[error("Failed to read body: {0}")]
    BodyReadError(BodyError),
    #[error("First read failed")]
    FirstReadEOF,
    #[error("Unexpected end of file while reading")]
    UnexpectedEOF,
    #[error("Failed to parse request: {0}")]
    HttpRequestParseError(MessageError),
    #[error("Header error: {0}")]
    HeaderError(#[from] HeaderError),
    #[error("Request head exceeded size limit: {0} >= {1}")]
    MaxHeadLenExceeded(usize, usize),
}

/// A result type where the error is [`FrontendError`].
pub type FrontendResult<T> = Result<T, FrontendError>;

/// Reads a request from a frontend reader. When `read()` is called, this type
/// is consumed, and a [`FrontendRequest`] is returned. This can be used to
/// inspect the request and peek the body.
pub struct FrontendReader<I> {
    io: I,
    max_head_length: usize,
    buffer: buffer::Buffer,
}

impl<I: AsyncReadExt + Unpin> FrontendReader<I> {
    const MAX_HEADERS: usize = 256;

    /// Createa a new [`FrontendReader`].
    pub fn new(io: I, max_head_length: usize) -> Self {
        Self::new_with_buffer(io, max_head_length, BytesMut::new())
    }

    fn new_with_buffer(io: I, max_head_length: usize, buffer: BytesMut) -> FrontendReader<I> {
        Self {
            io,
            max_head_length,
            buffer: buffer::Buffer::new(buffer),
        }
    }

    /// Wait for the full frontend head to be available
    async fn head(&mut self) -> FrontendResult<Request> {
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
    max_head_length: usize,
    reader: BodyReader<I>,
}

impl<I> FrontendBodyReader<I> {
    pub fn mode(&self) -> BodyReadMode {
        self.reader.mode()
    }
}

impl<I: AsyncReadExt + Unpin> FrontendBodyReader<I> {
    fn new(io: I, max_head_length: usize, buffer: BytesMut, mode: BodyReadMode) -> Self {
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

/// Writes a response to a frontend.
#[derive(Debug)]
pub struct FrontendWriter<I> {
    io: I,
}

impl<I: AsyncWriteExt + Unpin> FrontendWriter<I> {
    /// Create a new frontend writer that will write responses into the
    /// specified `io`.
    pub fn new(io: I) -> Self {
        Self { io }
    }

    /// Send the specified response with a chunked response body.
    pub async fn send_as_chunked(
        self,
        response: &Response,
    ) -> FrontendResult<FrontendBodyWriter<I>> {
        let Self { mut io } = self;
        write_response_to(response, HeadFraming::Chunked, &mut io)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(FrontendBodyWriter {
            io,
            state: BodyWriterState::TE(ChunkedBodyWriter::new()),
        })
    }

    /// Send the specified response with a content-length delimited response
    /// body. The [`FrontendBodyWriter`] will only accept the specified number
    /// of bytes.
    pub async fn send_as_content_length(
        self,
        response: &Response,
        body_len: u64,
    ) -> FrontendResult<FrontendBodyWriter<I>> {
        let Self { mut io } = self;
        write_response_to(response, HeadFraming::Length(body_len), &mut io)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(FrontendBodyWriter {
            io,
            state: BodyWriterState::CL(ContentLengthBodyWriter::new(body_len)),
        })
    }

    /// Send the response to the frontend as bodyless. This is used most
    /// frequently as a response to a `HEAD` request. This will not remove any
    /// framing header sent from the origin, and will not add its own. This
    /// means that the request may specify a `content-length` or
    /// `transfer-encoding: chunked` header, and it will remain in place when
    /// forwarding to the frontend. If this is used with something like a
    /// 1xx-informational response, it is assumed the user has ensured that the
    /// response adheres to the standard (no content-length or transfer-encoding
    /// headers in the 1xx response, etc).
    pub async fn send_as_bodyless(
        self,
        response: &Response,
    ) -> Result<FrontendBodyWriter<I>, FrontendError> {
        let Self { mut io } = self;
        write_response_to(response, HeadFraming::NoFraming, &mut io)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(FrontendBodyWriter {
            io,
            state: BodyWriterState::CL(ContentLengthBodyWriter::new(0)),
        })
    }

    pub fn into_inner(self) -> I {
        let Self { io } = self;
        io
    }
}

async fn write_response_to<W: AsyncWriteExt + Unpin>(
    res: &Response,
    framing: HeadFraming,
    mut w: W,
) -> io::Result<()> {
    let version = res.version().to_static();
    let code = res.code();
    let reason = res.reason();

    // TODO: we can avoid this heap allocation
    let code = format!("{code}");

    // write out the status line
    // TODO: we should always send HTTP/1.1
    w.write_all(version.as_bytes()).await?;
    w.write_all(b" ").await?;
    w.write_all(code.as_bytes()).await?;
    w.write_all(b" ").await?;
    w.write_all(reason).await?;
    w.write_all(b"\r\n").await?;

    // if framing is specified, remove any existing framing header. otherwise,
    // we'll leave the framing header specified by the origin in place. this is
    // mostly useful for responses to HEAD requests.
    let headers = res
        .headers()
        .iter()
        .filter(|(n, _)| framing.is_no_framing() || !is_framing_header(n));

    // write out each header
    for (n, v) in headers {
        w.write_all(n).await?;
        w.write_all(b": ").await?;
        w.write_all(v).await?;
        w.write_all(b"\r\n").await?;
    }

    // add the framing header
    match framing {
        HeadFraming::NoFraming => {}
        HeadFraming::Length(l) => {
            // TODO: we can avoid this heap allocation
            let cl = format!("content-length: {l}\r\n");
            w.write_all(cl.as_bytes()).await?;
        }
        HeadFraming::Chunked => {
            w.write_all(b"transfer-encoding: chunked\r\n").await?;
        }
    }

    // write out the \r\n indicating the end of the head
    w.write_all(b"\r\n").await?;

    // flush the response head so that the backend has a chance to respond to
    // just the head in case this writer is buffered.
    w.flush().await?;

    Ok(())
}

/// A frontend response body writer.
pub struct FrontendBodyWriter<I> {
    io: I,
    state: BodyWriterState,
}

impl<I: AsyncWriteExt + Unpin> FrontendBodyWriter<I> {
    pub async fn write(&mut self, buf: &[u8]) -> FrontendResult<()> {
        self.state
            .write(&mut self.io, buf)
            .await
            .map_err(FrontendError::BodyWriteError)
    }

    pub async fn finish(self) -> FrontendResult<FrontendWriter<I>> {
        let Self { mut io, state } = self;
        state
            .finish(&mut io)
            .await
            .map_err(FrontendError::BodyWriteError)?;
        Ok(FrontendWriter { io })
    }
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use bytes::BytesMut;
    use jrpxy_body::BodyError;
    use jrpxy_http_message::{message::ResponseBuilder, version::HttpVersion};
    use jrpxy_util::debug::AsciiDebug;
    use tokio::io::AsyncWriteExt;

    use crate::{FrontendBodyReader, FrontendWriter};

    use super::{FrontendError, FrontendReader};

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

    #[tokio::test]
    async fn frontend_writer() {
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok".into())
            .with_version(HttpVersion::Http11)
            .with_header("x-hello", &b"World"[..])
            .with_header("content-LENGTH", &b"400"[..])
            .with_header("TRANSFER-ENCODING", &b"chunked"[..])
            .build()
            .into();

        let mut buf = Vec::new();
        let cw = FrontendWriter::new(&mut buf);
        let r = cw.send_as_content_length(&res, 0).await.expect("can write");

        let buf = r.finish().await.expect("failed to finish").into_inner();

        // now we validate that the response contains what we expect. Notably:
        // - expect that the content-length has been replaced with 0
        // - expect we strip out transfer-encoding header
        // - expect the remaining header to be present

        let expected = b"\
            HTTP/1.1 200 Ok\r\n\
            x-hello: World\r\n\
            content-length: 0\r\n\
            \r\n\
        ";
        assert_eq!(AsciiDebug(expected), AsciiDebug(buf));
    }

    #[tokio::test]
    async fn frontend_writer_no_body() {
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok".into())
            .with_version(HttpVersion::Http11)
            .build()
            .into();

        let mut buf = Vec::new();
        let cw = FrontendWriter::new(&mut buf);
        let mut r = cw.send_as_content_length(&res, 0).await.expect("can write");

        // expect that attempting to write to the body results in an unexpected EOF
        let r = r.write(&b"hello"[..]).await.expect_err("wasn't an error");
        assert!(matches!(
            r,
            FrontendError::BodyWriteError(BodyError::BodyOverflow(0))
        ));
    }

    // TODO: boundary tests around u64::MAX

    #[tokio::test]
    async fn frontend_writer_with_content_length() {
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok".into())
            .with_version(HttpVersion::Http11)
            .build()
            .into();

        let mut buf = Vec::new();
        let cw = FrontendWriter::new(&mut buf);
        let mut r = cw
            .send_as_content_length(&res, 10)
            .await
            .expect("can write");

        r.write(&b"01234"[..]).await.expect("write works");
        r.write(&b"56789"[..]).await.expect("write works");

        assert!(matches!(
            r.write(&b"X"[..]).await.expect_err("this overflows"),
            FrontendError::BodyWriteError(BodyError::BodyOverflow(10))
        ));

        let _buf = r.finish().await.expect("failed to finish").into_inner();
    }

    #[tokio::test]
    async fn frontend_writer_with_content_length_incomplete_body() {
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok".into())
            .with_version(HttpVersion::Http11)
            .build()
            .into();

        let mut buf = Vec::new();
        let cw = FrontendWriter::new(&mut buf);
        let mut r = cw
            .send_as_content_length(&res, 10)
            .await
            .expect("can write");

        r.write(&b"01234"[..]).await.expect("write works");

        // do not write all 10 bytes, and then try to finish. finishing will fail.

        assert!(matches!(
            r.finish().await.expect_err("failed to emit error"),
            FrontendError::BodyWriteError(BodyError::IncompleteBody {
                expected: 10,
                actual: 5
            })
        ));
    }

    #[tokio::test]
    async fn frontend_writer_with_chunk_encoding() {
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok".into())
            .with_version(HttpVersion::Http11)
            .build()
            .into();

        let mut buf = Vec::new();
        let cw = FrontendWriter::new(&mut buf);
        let mut r = cw.send_as_chunked(&res).await.expect("can write");

        r.write(&b"01234"[..]).await.expect("write works");
        r.write(&b"56789"[..]).await.expect("write works");
        r.write(&b""[..])
            .await
            .expect("works, but shouldn't generate a chunk");

        let _buf = r.finish().await.expect("failed to finish").into_inner();
    }

    #[tokio::test]
    async fn frontend_writer_pipeline() {
        let mut output_buf = Vec::with_capacity(4096);
        let cw = FrontendWriter::new(&mut output_buf);

        // first response
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok".into())
            .with_version(HttpVersion::Http11)
            .with_header("x-req", b"first")
            .build()
            .into();
        let body_writer = cw
            .send_as_content_length(&res, 0)
            .await
            .expect("first write works");
        let cw = body_writer.finish().await.expect("failed to finish");

        // second response
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok".into())
            .with_version(HttpVersion::Http11)
            .with_header("x-req", b"second")
            .build()
            .into();
        let mut body_writer = cw
            .send_as_content_length(&res, 5)
            .await
            .expect("first write works");
        body_writer.write(b"01234").await.expect("failed to write");
        let cw = body_writer.finish().await.expect("failed to finish");

        // third response
        let mut cr = ResponseBuilder::new(8);
        let res = cr
            .with_code(200)
            .with_reason("Ok".into())
            .with_version(HttpVersion::Http11)
            .with_header("x-req", b"third")
            .build()
            .into();
        let mut body_writer = cw.send_as_chunked(&res).await.expect("first write works");
        body_writer.write(b"01234").await.expect("failed to write");
        body_writer.write(b"5").await.expect("failed to write");
        body_writer.write(b"6789").await.expect("failed to write");
        let cw = body_writer.finish().await.expect("failed to finish");

        // check the output
        let output_buf = cw.into_inner().as_slice();

        let expected = b"\
            HTTP/1.1 200 Ok\r\n\
            x-req: first\r\n\
            content-length: 0\r\n\
            \r\n\
            HTTP/1.1 200 Ok\r\n\
            x-req: second\r\n\
            content-length: 5\r\n\
            \r\n\
            01234\
            HTTP/1.1 200 Ok\r\n\
            x-req: third\r\n\
            transfer-encoding: chunked\r\n\
            \r\n\
            5\r\n\
            01234\r\n\
            1\r\n\
            5\r\n\
            4\r\n\
            6789\r\n\
            0\r\n\
            \r\n\
        ";

        assert_eq!(AsciiDebug(expected), AsciiDebug(output_buf));
    }
}
