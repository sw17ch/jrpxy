mod parse_trailers;

pub use parse_trailers::TrailerError;

use bytes::{Bytes, BytesMut};
use jrpxy_util::buffer::Buffer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::parse_trailers::parse_trailers;

#[derive(Debug, thiserror::Error)]
pub enum BodyError {
    #[error("Attempted to write more than the maximum allowed bytes: {0}")]
    BodyOverflow(u64),
    #[error("Could not write body: {0}")]
    BodyWriteError(std::io::Error),
    #[error("Could not read body: {0}")]
    BodyReadError(std::io::Error),
    #[error("Body expected {expected} bytes, but only has {actual} bytes")]
    IncompleteBody { expected: u64, actual: u64 },
    #[error("Invalid chunk header: {0}")]
    InvalidChunkHeader(httparse::InvalidChunkSize),
    #[error("Invalid HTTP trailer: {0}")]
    TrailerError(#[from] TrailerError),
    #[error("Unexpected EOF")]
    UnexpectedEOF,
    #[error("Invalid chunk footer: expected {0} got {0}")]
    InvalidChunkFooter(u8, u8),
}

pub type BodyResult<T> = Result<T, BodyError>;

const FRAMING_HEADERS: &[&[u8]] = &[b"content-length", b"transfer-encoding"];

pub fn is_framing_header(name: &[u8]) -> bool {
    FRAMING_HEADERS.iter().any(|h| h.eq_ignore_ascii_case(name))
}

#[derive(Debug)]
pub enum BodyWriterState {
    CL(ContentLengthBodyWriter),
    TE(ChunkedBodyWriter),
}

impl BodyWriterState {
    pub async fn write<I: AsyncWriteExt + Unpin>(
        &mut self,
        mut io: I,
        buf: &[u8],
    ) -> BodyResult<()> {
        match self {
            BodyWriterState::CL(w) => w.write(&mut io, buf).await,
            BodyWriterState::TE(w) => w.write(&mut io, buf).await,
        }
    }

    pub async fn finish<I: AsyncWriteExt + Unpin>(self, mut io: I) -> BodyResult<()> {
        match self {
            BodyWriterState::CL(w) => w.finish(&mut io).await?,
            BodyWriterState::TE(w) => w.finish(&mut io).await?,
        }
        io.flush().await.map_err(BodyError::BodyWriteError)?;
        Ok(())
    }

    /// Explicitly abort a body write. This will, for certain transfer types,
    /// perform an invalid write which may help prevent insufficiently strict
    /// body readers from considering the body complete.
    pub async fn abort<I: AsyncWriteExt + Unpin>(self, mut io: I) -> BodyResult<()> {
        match self {
            BodyWriterState::CL(_w) => {
                // do nothing when aborting a content-length body. just drop the
                // connection.
            }
            BodyWriterState::TE(w) => {
                // there are chunked body readers out there that don't wait
                // around for the empty chunk to consider a body complete, so we
                // do something special.
                w.abort(&mut io).await?
            }
        }
        io.flush().await.map_err(BodyError::BodyWriteError)?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct ContentLengthBodyWriter {
    /// the total body length specified by the content-length header.
    length: u64,
    /// the amount of the body already written
    offset: u64,
}

impl ContentLengthBodyWriter {
    pub fn new(length: u64) -> Self {
        Self { length, offset: 0 }
    }

    pub async fn write<W: AsyncWriteExt + Unpin>(
        &mut self,
        mut io: W,
        buffer: &[u8],
    ) -> BodyResult<()> {
        let Self { length, offset } = self;

        let Some(next_offset) = offset.checked_add(buffer.len() as u64) else {
            return Err(BodyError::BodyOverflow(*length));
        };

        if next_offset > *length {
            return Err(BodyError::BodyOverflow(*length));
        }

        let () = io
            .write_all(buffer)
            .await
            .map_err(BodyError::BodyWriteError)?;

        *offset = next_offset;

        Ok(())
    }

    pub fn empty() -> ContentLengthBodyWriter {
        Self {
            length: 0,
            offset: 0,
        }
    }

    pub async fn finish<W: AsyncWriteExt + Unpin>(self, _io: W) -> BodyResult<()> {
        let Self { length, offset } = self;
        debug_assert!(offset <= length, "offset exceeds length");
        if offset < length {
            return Err(BodyError::IncompleteBody {
                expected: length,
                actual: offset,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct ChunkedBodyWriter {}

impl ChunkedBodyWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn write<W: AsyncWriteExt + Unpin>(
        &mut self,
        mut io: W,
        buffer: &[u8],
    ) -> BodyResult<()> {
        // TODO: use vectored writes

        if buffer.is_empty() {
            // if there's nothing in the buffer, don't do anything else.
            // attempting to write a zero-length chunk will break framing.
            return Ok(());
        }

        // TODO: we can avoid this heap allocation
        let chunk_header = format!("{:x}\r\n", buffer.len());
        let chunk_footer = "\r\n";

        io.write_all(chunk_header.as_bytes())
            .await
            .map_err(BodyError::BodyWriteError)?;
        io.write_all(buffer)
            .await
            .map_err(BodyError::BodyWriteError)?;
        io.write_all(chunk_footer.as_bytes())
            .await
            .map_err(BodyError::BodyWriteError)?;

        Ok(())
    }

    pub async fn finish<W: AsyncWriteExt + Unpin>(self, mut io: W) -> BodyResult<()> {
        io.write_all(b"0\r\n\r\n")
            .await
            .map_err(BodyError::BodyWriteError)
    }

    async fn abort<I: AsyncWriteExt + Unpin>(&self, mut io: I) -> BodyResult<()> {
        // we allow users to explicitly abandon a transfer-encoding:chunked
        // write by emitting a bad chunk header. this should be a little more
        // durable when badly-configured body readers don't wait for the
        // terminating empty chunk to consider a body complete.
        //
        // we write an 'x' because it is not a valid hexadecimal character
        io.write_all(b"x").await.map_err(BodyError::BodyWriteError)
    }
}

#[derive(Debug)]
pub enum BodyReadMode {
    Chunk,
    ContentLength(u64),
    Bodyless,
}

#[derive(Debug)]
pub struct ContentLengthBodyReader {
    /// the total body length specified by the content-length header.
    length: u64,
    /// the amount of the body already read
    offset: u64,
}
impl ContentLengthBodyReader {
    fn new(length: u64) -> Self {
        Self { length, offset: 0 }
    }

    async fn read_bytes(
        &mut self,
        max_len: usize,
        buffer: &mut Buffer,
    ) -> BodyResult<BodyReadResult> {
        let remaining = self.remaining();
        if remaining == 0 {
            return Ok(BodyReadResult::Complete);
        }

        if buffer.is_empty() {
            return Ok(BodyReadResult::NeedRead);
        }

        let at = remaining
            .try_into()
            .unwrap_or(usize::MAX)
            .min(buffer.len())
            .min(max_len);

        self.offset += at as u64;

        Ok(BodyReadResult::DidRead(buffer.split_to(at).freeze()))
    }

    /// Attempt to read more data from `buffer` into `peeked`. Returns
    async fn peek(
        &mut self,
        max_len: usize,
        buffer: &mut Buffer,
        peeked: &mut Buffer,
    ) -> BodyResult<BodyPeekResult> {
        let len = match self.read_bytes(max_len, buffer).await? {
            BodyReadResult::Complete => 0,
            BodyReadResult::DidRead(bytes) => {
                peeked.extend_from_slice(&bytes);
                bytes.len()
            }
            BodyReadResult::NeedRead => {
                return Ok(BodyPeekResult::NeedRead);
            }
        };

        Ok(BodyPeekResult::DidRead(self.remaining() == 0, len))
    }

    async fn read(
        &mut self,
        max_len: usize,
        buffer: &mut Buffer,
        peeked: &mut Buffer,
    ) -> BodyResult<BodyReadResult> {
        if !peeked.is_empty() {
            // If there's already data in the peek buffer, return that first.
            let at = peeked.len().min(max_len);
            return Ok(BodyReadResult::DidRead(peeked.split_to(at).freeze()));
        }

        self.read_bytes(max_len, buffer).await
    }

    fn remaining(&self) -> u64 {
        debug_assert!(self.offset <= self.length);
        self.length - self.offset
    }
}

#[derive(Debug)]
enum ChunkedBodyReaderState {
    InChunkHeader,
    InChunkBody { remaining: u64 },
    InChunkFooterNeedCR,
    InChunkFooterNeedLF,
    InTrailers,
    Done,
}

#[derive(Debug)]
pub struct ChunkedBodyReader {
    state: ChunkedBodyReaderState,
}

impl ChunkedBodyReader {
    async fn read_bytes(
        &mut self,
        max_len: usize,
        buffer: &mut Buffer,
    ) -> BodyResult<BodyReadResult> {
        loop {
            match &mut self.state {
                ChunkedBodyReaderState::InChunkHeader => {
                    match httparse::parse_chunk_size(buffer)
                        .map_err(BodyError::InvalidChunkHeader)?
                    {
                        httparse::Status::Complete((body_at, chunk_len)) => {
                            let _chunk_header = buffer.split_to(body_at);
                            if chunk_len == 0 {
                                self.state = ChunkedBodyReaderState::InTrailers;
                            } else {
                                self.state = ChunkedBodyReaderState::InChunkBody {
                                    remaining: chunk_len,
                                };
                            }
                        }
                        httparse::Status::Partial => return Ok(BodyReadResult::NeedRead),
                    }
                }
                ChunkedBodyReaderState::InChunkBody { remaining } => {
                    if *remaining == 0 {
                        self.state = ChunkedBodyReaderState::InChunkFooterNeedCR;
                    } else if buffer.is_empty() {
                        return Ok(BodyReadResult::NeedRead);
                    } else {
                        let at = (*remaining).min(buffer.len() as u64).min(max_len as u64);
                        *remaining -= at;
                        let body_buf = buffer.split_to(at as usize);
                        return Ok(BodyReadResult::DidRead(body_buf.freeze()));
                    }
                }
                ChunkedBodyReaderState::InChunkFooterNeedCR => match buffer.try_get_u8() {
                    Ok(b'\r') => {
                        self.state = ChunkedBodyReaderState::InChunkFooterNeedLF;
                    }
                    Ok(unexpected) => {
                        return Err(BodyError::InvalidChunkFooter(b'\r', unexpected));
                    }
                    Err(_) => {
                        return Ok(BodyReadResult::NeedRead);
                    }
                },
                ChunkedBodyReaderState::InChunkFooterNeedLF => match buffer.try_get_u8() {
                    Ok(b'\n') => {
                        self.state = ChunkedBodyReaderState::InChunkHeader;
                    }
                    Ok(unexpected) => {
                        return Err(BodyError::InvalidChunkFooter(b'\n', unexpected));
                    }
                    Err(_) => {
                        return Ok(BodyReadResult::NeedRead);
                    }
                },
                ChunkedBodyReaderState::InTrailers => match parse_trailers(buffer)? {
                    httparse::Status::Complete(end_at) => {
                        let _trailers = buffer.split_to(end_at);
                        self.state = ChunkedBodyReaderState::Done;
                        return Ok(BodyReadResult::Complete);
                    }
                    httparse::Status::Partial => return Ok(BodyReadResult::NeedRead),
                },
                ChunkedBodyReaderState::Done => return Ok(BodyReadResult::Complete),
            }
        }
    }

    async fn peek(
        &mut self,
        max_len: usize,
        buffer: &mut Buffer,
        peeked: &mut Buffer,
    ) -> BodyResult<BodyPeekResult> {
        let len = match self.read_bytes(max_len, buffer).await? {
            BodyReadResult::Complete => 0,
            BodyReadResult::DidRead(bytes) => {
                peeked.extend_from_slice(&bytes);
                bytes.len()
            }
            BodyReadResult::NeedRead => {
                return Ok(BodyPeekResult::NeedRead);
            }
        };

        Ok(BodyPeekResult::DidRead(self.drained(), len))
    }

    async fn read(
        &mut self,
        max_len: usize,
        buffer: &mut Buffer,
        peeked: &mut Buffer,
    ) -> BodyResult<BodyReadResult> {
        if !peeked.is_empty() {
            // If there's already data in the peek buffer, return that first.
            let at = peeked.len().min(max_len);
            return Ok(BodyReadResult::DidRead(peeked.split_to(at).freeze()));
        }

        self.read_bytes(max_len, buffer).await
    }

    fn drained(&self) -> bool {
        matches!(self.state, ChunkedBodyReaderState::Done)
    }

    fn new() -> Self {
        Self {
            state: ChunkedBodyReaderState::InChunkHeader,
        }
    }
}

#[derive(Debug)]
enum BodyPeekResult {
    NeedRead,
    // true when the body has been fully peeked
    DidRead(bool, usize),
}

#[derive(Debug)]
enum BodyReadResult {
    NeedRead,
    DidRead(Bytes),
    Complete,
}

#[derive(Debug)]
enum BodyReaderState {
    Bodyless,
    CL(ContentLengthBodyReader),
    TE(ChunkedBodyReader),
}

pub struct BodyReader<I> {
    io: I,
    /// a buffer in which we store data peeked off the socket, but not yet read
    peeked: Buffer,
    buffer: Buffer,
    state: BodyReaderState,
}

impl<I> BodyReader<I> {
    pub fn mode(&self) -> BodyReadMode {
        match &self.state {
            BodyReaderState::Bodyless => BodyReadMode::Bodyless,
            BodyReaderState::CL(r) => BodyReadMode::ContentLength(r.length),
            BodyReaderState::TE(_) => BodyReadMode::Chunk,
        }
    }
}

impl<I: AsyncReadExt + Unpin> BodyReader<I> {
    const CHUNK_READ_LEN: usize = 8 * 1024;

    pub fn new(io: I, buffer: BytesMut, mode: BodyReadMode) -> Self {
        let buffer = Buffer::new(buffer);
        let peeked = Buffer::new(BytesMut::new());
        let state = match mode {
            BodyReadMode::Bodyless => BodyReaderState::Bodyless,
            BodyReadMode::Chunk => BodyReaderState::TE(ChunkedBodyReader::new()),
            BodyReadMode::ContentLength(length) => {
                BodyReaderState::CL(ContentLengthBodyReader::new(length))
            }
        };
        Self {
            io,
            buffer,
            state,
            peeked,
        }
    }

    /// Peek data from the body, and allow it to be observed, but do not remove
    /// it from the read buffer. Always returns data starting from the first
    /// byte not yet read. That is, repeated calls to this method with the same
    /// length will always yield the same result unless read calls are
    /// interspersed with peek calls.
    pub async fn peek(&mut self, target_len: usize) -> BodyResult<(bool, &[u8])> {
        let mut remaining_len = target_len.saturating_sub(self.peeked.len());
        loop {
            let res = match &mut self.state {
                BodyReaderState::Bodyless => Ok(BodyPeekResult::DidRead(true, 0)),
                BodyReaderState::CL(cl) => {
                    cl.peek(remaining_len, &mut self.buffer, &mut self.peeked)
                        .await
                }
                BodyReaderState::TE(te) => {
                    te.peek(remaining_len, &mut self.buffer, &mut self.peeked)
                        .await
                }
            };
            let res = res?;
            match res {
                BodyPeekResult::NeedRead => {
                    let len = self
                        .buffer
                        .read_from(&mut self.io, Self::CHUNK_READ_LEN)
                        .await
                        .map_err(BodyError::BodyReadError)?;
                    if len == 0 {
                        return Err(BodyError::UnexpectedEOF);
                    }
                }
                BodyPeekResult::DidRead(complete, read_len) => {
                    debug_assert!(read_len <= remaining_len);
                    remaining_len -= read_len;

                    // stop when either we've completed peeking the entire body,
                    // or when we've reached the target peek length
                    if remaining_len == 0 || complete {
                        return Ok((complete, &self.peeked));
                    }
                }
            }
        }
    }

    pub async fn read(&mut self, max_len: usize) -> BodyResult<Option<Bytes>> {
        loop {
            let res = match &mut self.state {
                BodyReaderState::Bodyless => Ok(BodyReadResult::Complete),
                BodyReaderState::CL(cl) => {
                    cl.read(max_len, &mut self.buffer, &mut self.peeked).await
                }
                BodyReaderState::TE(te) => {
                    te.read(max_len, &mut self.buffer, &mut self.peeked).await
                }
            };
            let res = res?;
            match res {
                BodyReadResult::NeedRead => {
                    let len = self
                        .buffer
                        .read_from(&mut self.io, Self::CHUNK_READ_LEN)
                        .await
                        .map_err(BodyError::BodyReadError)?;
                    if len == 0 {
                        return Err(BodyError::UnexpectedEOF);
                    }
                }
                BodyReadResult::DidRead(bytes) => return Ok(Some(bytes)),
                BodyReadResult::Complete => return Ok(None),
            }
        }
    }

    pub fn drained(&self) -> bool {
        match &self.state {
            BodyReaderState::Bodyless => true,
            BodyReaderState::CL(cl) => cl.remaining() == 0,
            BodyReaderState::TE(te) => te.drained(),
        }
    }

    /// Ensure the body is fully drained from the socket
    pub async fn drain(mut self) -> BodyResult<(I, Buffer)> {
        const DRAIN_SIZE: usize = 2 * 4096;

        while let Some(_buf) = self.read(DRAIN_SIZE).await? {
            // drop buffers until we get to the end of the body
        }

        debug_assert!(
            self.drained(),
            "in spite of our best attempts, we failed to drain"
        );

        let Self {
            io,
            peeked: _,
            buffer,
            state: _,
        } = self;

        Ok((io, buffer))
    }
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use bytes::BytesMut;
    use tokio::io::AsyncWriteExt;

    use crate::{BodyError, BodyReadMode, BodyReader, ChunkedBodyWriter};

    #[tokio::test]
    async fn cl_peek_full() {
        let input = b"\
            0123456789
            ";

        let mut br = BodyReader::new(&input[..], BytesMut::new(), BodyReadMode::ContentLength(10));
        let (complete, slice) = br.peek(5).await.expect("peek works");
        assert!(!complete);
        assert_eq!(b"01234", slice);

        let (complete, slice) = br.peek(10).await.expect("peek works");
        assert!(complete);
        assert_eq!(b"0123456789", slice);

        // peek again, but with a value longer than the total IO
        let (complete, slice) = br.peek(11).await.expect("peek works");
        assert!(complete);
        assert_eq!(b"0123456789", slice);

        let buf = br
            .read(10)
            .await
            .expect("peek works")
            .expect("didn't get buf");
        assert_eq!(&b"0123456789"[..], buf);
    }

    #[tokio::test]
    async fn cl_peek_partial() {
        let input = b"\
            0123456789
            ";

        let mut br = BodyReader::new(&input[..], BytesMut::new(), BodyReadMode::ContentLength(10));

        // read one byte
        let buf = br
            .read(1)
            .await
            .expect("peek works")
            .expect("didn't get buf");
        assert_eq!(&b"0"[..], buf);

        // peek some data. doesn't contain data already read.
        let (complete, slice) = br.peek(5).await.expect("peek works");
        assert!(!complete);
        assert_eq!(b"12345", slice);

        // read less than the amount peeked
        let buf = br
            .read(4)
            .await
            .expect("peek works")
            .expect("didn't get buf");
        assert_eq!(&b"1234"[..], buf);

        // peek the rest (using an oversized target length)
        let (complete, slice) = br.peek(10).await.expect("peek works");
        assert!(complete);
        assert_eq!(b"56789", slice);

        // read the rest (again using an oversized target length)
        let buf = br
            .read(10)
            .await
            .expect("peek works")
            .expect("didn't get buf");
        assert_eq!(&b"56789"[..], buf);
    }

    #[tokio::test]
    async fn trickle_peek_cl() {
        let (mut left, right) = tokio::io::duplex(3);

        // this task feeds bytes slowly
        let w = tokio::spawn(async move {
            let buf = b"\
                0123456789\
                ";

            for c in buf.chunks(2) {
                tokio::time::sleep(Duration::from_millis(10)).await;
                if left.write_all(c).await.is_err() {
                    break;
                }
            }
        });

        let r = tokio::spawn(tokio::time::timeout(Duration::from_secs(10), async move {
            let mut reader =
                BodyReader::new(right, BytesMut::new(), BodyReadMode::ContentLength(10));
            let (complete, buf) = reader.peek(9).await.expect("can't break into parts");

            assert!(!complete);
            assert_eq!(&b"012345678"[..], buf);

            let (complete, buf) = reader.peek(10).await.expect("can't break into parts");

            assert!(complete);
            assert_eq!(&b"0123456789"[..], buf);
        }));

        let () = w.await.unwrap();
        let () = r.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn chunked_peek_full() {
        let input = b"\
            4\r\n\
            0123\r\n\
            3\r\n\
            456\r\n\
            1\r\n\
            7\r\n\
            2\r\n\
            89\r\n\
            0\r\n\
            \r\n\
            ";

        let mut br = BodyReader::new(&input[..], BytesMut::new(), BodyReadMode::Chunk);
        let (complete, slice) = br.peek(5).await.expect("peek works");
        assert!(!complete);
        assert_eq!(b"01234", slice);

        let (complete, slice) = br.peek(10).await.expect("peek works");
        // even though we read the entire body, the reader doesn't know that yet
        // as we haven't yet seen the final chunk. we need to try and read one
        // more byte to discover that we're finished.
        assert!(!complete);
        assert_eq!(b"0123456789", slice);

        // peek again, but with a value longer than the total IO
        let (complete, slice) = br.peek(11).await.expect("peek works");
        assert!(complete);
        assert_eq!(b"0123456789", slice);

        let buf = br
            .read(10)
            .await
            .expect("peek works")
            .expect("didn't get buf");
        assert_eq!(&b"0123456789"[..], buf);
    }

    #[tokio::test]
    async fn bodyless_peek() {
        let input = b"";

        let mut br = BodyReader::new(&input[..], BytesMut::new(), BodyReadMode::Bodyless);
        let (complete, slice) = br.peek(5).await.expect("peek works");
        assert!(complete);
        assert_eq!(b"", slice);
    }

    #[tokio::test]
    async fn chunked_read_empty_chunk_extension() {
        // Make sure we handle correctly parsing chunk-extensions without any
        // names. See
        // https://www.imperva.com/blog/smuggling-requests-with-chunked-extensions-a-new-http-desync-trick/
        let input = b"\
            4;\r\n\
            0123\r\n\
            3;a=b\r\n\
            012\r\n\
            0\r\n\
            \r\n\
            ";

        let mut br = BodyReader::new(&input[..], BytesMut::new(), BodyReadMode::Chunk);
        let (complete, slice) = br.peek(10).await.expect("peek works");
        assert!(complete);
        assert_eq!(b"0123012", slice);
    }

    #[tokio::test]
    async fn chunked_reject_invalid_chunk_footer() {
        // Note that the 'X' is where the '\r' should be.
        let input = b"\
            4\r\n\
            0123X\r\n\
            0\r\n\
            \r\n\
            ";

        let mut br = BodyReader::new(&input[..], BytesMut::new(), BodyReadMode::Chunk);

        // The body bytes themselves are valid and should be readable.
        let chunk = br.read(4).await.expect("reading body bytes works").unwrap();
        assert_eq!(b"0123", chunk.as_ref());

        // Reading past the body must result in an error.
        assert!(matches!(
            br.read(1).await.unwrap_err(),
            BodyError::InvalidChunkFooter(b'\r', b'X'),
        ));

        // Note that the 'X' is where the '\n' should be.
        let input = b"\
            4\r\n\
            0123\rX\n\
            0\r\n\
            \r\n\
            ";

        let mut br = BodyReader::new(&input[..], BytesMut::new(), BodyReadMode::Chunk);

        // The body bytes themselves are valid and should be readable.
        let chunk = br.read(4).await.expect("reading body bytes works").unwrap();
        assert_eq!(b"0123", chunk.as_ref());

        // Reading past the body must result in an error.
        assert!(matches!(
            br.read(1).await.unwrap_err(),
            BodyError::InvalidChunkFooter(b'\n', b'X'),
        ));
    }

    #[tokio::test]
    async fn chunked_write_abort() {
        let mut write_buf = Vec::new();
        let mut bw = ChunkedBodyWriter::new();
        bw.write(&mut write_buf, b"hello").await.unwrap();
        bw.write(&mut write_buf, b"there").await.unwrap();
        bw.abort(&mut write_buf).await.unwrap();

        assert_eq!(
            "\
            5\r\n\
            hello\r\n\
            5\r\n\
            there\r\n\
            x",
            std::str::from_utf8(&write_buf).unwrap(),
        );
    }

    #[tokio::test]
    async fn chunked_write() {
        let mut write_buf = Vec::new();
        let mut bw = ChunkedBodyWriter::new();
        bw.write(&mut write_buf, b"hello").await.unwrap();
        bw.write(&mut write_buf, b"there").await.unwrap();
        bw.finish(&mut write_buf).await.unwrap();

        assert_eq!(
            "\
            5\r\n\
            hello\r\n\
            5\r\n\
            there\r\n\
            0\r\n\
            \r\n",
            std::str::from_utf8(&write_buf).unwrap(),
        );
    }
}
