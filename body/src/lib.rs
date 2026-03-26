use bytes::{Bytes, BytesMut};
use jrpxy_http_message::header::Headers;
use jrpxy_http_message::message::{MessageError, ParseSlots};
use jrpxy_util::buffer::Buffer;
use jrpxy_util::io_buffer::{IoBuffer, IoBufferError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn map_trailer_error(e: MessageError) -> BodyError {
    let trailer_error = match e {
        MessageError::Parse(httparse::Error::TooManyHeaders) => TrailerError::TooManyFields,
        MessageError::Parse(e) => TrailerError::InvalidField(e),
        _ => unreachable!("parse_headers only produces Parse errors"),
    };
    BodyError::TrailerError(trailer_error)
}

#[derive(Debug, thiserror::Error)]
pub enum TrailerError {
    #[error("Invalid trailer field: {0}")]
    InvalidField(httparse::Error),
    #[error("Too many trailer fields")]
    TooManyFields,
}

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
    #[error("Invalid chunk footer: expected 0x{0:x} got 0x{1:x}")]
    InvalidChunkFooter(u8, u8),
    #[error("Attempted to read after previous failure")]
    ReadAfterError,
}

impl From<IoBufferError> for BodyError {
    fn from(e: IoBufferError) -> Self {
        match e {
            IoBufferError::Io(e) => BodyError::BodyReadError(e),
            IoBufferError::UnexpectedEOF => BodyError::UnexpectedEOF,
        }
    }
}

pub type BodyResult<T> = Result<T, BodyError>;

const FRAMING_HEADERS: &[&[u8]] = &[b"content-length", b"transfer-encoding"];
const DRAIN_SIZE: usize = 4096;

pub fn is_framing_header(name: &[u8]) -> bool {
    FRAMING_HEADERS.iter().any(|h| h.eq_ignore_ascii_case(name))
}

#[derive(Debug)]
pub enum BodyWriterKind {
    Bodyless,
    CL(ContentLengthBodyWriter),
    TE(ChunkedBodyWriter),
}

impl BodyWriterKind {
    pub async fn write<I: AsyncWriteExt + Unpin>(
        &mut self,
        mut io: I,
        buf: &[u8],
    ) -> BodyResult<()> {
        match self {
            BodyWriterKind::Bodyless => {
                return Err(BodyError::BodyOverflow(buf.len() as u64));
            }
            BodyWriterKind::CL(w) => w.write(&mut io, buf).await,
            BodyWriterKind::TE(w) => w.write(&mut io, buf).await,
        }
    }

    pub async fn finish<I: AsyncWriteExt + Unpin>(self, mut io: I) -> BodyResult<()> {
        match self {
            BodyWriterKind::Bodyless => {}
            BodyWriterKind::CL(w) => w.finish(&mut io).await?,
            BodyWriterKind::TE(w) => w.finish(&mut io).await?,
        }
        io.flush().await.map_err(BodyError::BodyWriteError)?;
        Ok(())
    }

    /// Explicitly abort a body write. This will, for certain transfer types,
    /// perform an invalid write which may help prevent insufficiently strict
    /// body readers from considering the body complete.
    pub async fn abort<I: AsyncWriteExt + Unpin>(self, mut io: I) -> BodyResult<()> {
        match self {
            BodyWriterKind::Bodyless => {
                // nothing to do when aborting. we can't even break framing. the
                // only thing we can do is drop the connection.
            }
            BodyWriterKind::CL(_w) => {
                // do nothing when aborting a content-length body. just drop the
                // connection.
            }
            BodyWriterKind::TE(w) => {
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

    pub async fn finish_with_trailers<W: AsyncWriteExt + Unpin>(
        self,
        mut io: W,
        trailers: &Headers,
    ) -> BodyResult<()> {
        // TODO: write vectored

        io.write_all(b"0\r\n")
            .await
            .map_err(BodyError::BodyWriteError)?;
        for (name, value) in trailers.iter() {
            io.write_all(name)
                .await
                .map_err(BodyError::BodyWriteError)?;
            io.write_all(b": ")
                .await
                .map_err(BodyError::BodyWriteError)?;
            io.write_all(value)
                .await
                .map_err(BodyError::BodyWriteError)?;
            io.write_all(b"\r\n")
                .await
                .map_err(BodyError::BodyWriteError)?;
        }
        io.write_all(b"\r\n")
            .await
            .map_err(BodyError::BodyWriteError)
    }

    pub async fn finish<W: AsyncWriteExt + Unpin>(self, io: W) -> BodyResult<()> {
        self.finish_with_trailers(io, &Default::default()).await
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
pub struct ContentLengthBodyReader<I> {
    /// the total body length specified by the content-length header.
    length: u64,
    /// the amount of the body already read
    offset: u64,
    /// the io associated with this body read
    io: IoBuffer<I>,
}

impl<I> ContentLengthBodyReader<I>
where
    I: AsyncReadExt + Unpin,
{
    fn new(length: u64, io: IoBuffer<I>) -> Self {
        Self {
            length,
            offset: 0,
            io,
        }
    }

    /// Read bytes and return them as a [`Bytes`].
    pub async fn read(&mut self, max_len: usize) -> BodyResult<Option<Bytes>> {
        let remaining = self.remaining();
        if remaining == 0 {
            return Ok(None);
        }

        self.io.ensure().await?;

        let at = remaining
            .try_into()
            .unwrap_or(usize::MAX)
            .min(self.io.len())
            .min(max_len);

        self.offset += at as u64;

        Ok(Some(self.io.split_to(at)))
    }

    fn remaining(&self) -> u64 {
        debug_assert!(self.offset <= self.length);
        self.length - self.offset
    }
}

/// The extensions attached to a chunk header. Currently opaque; full parsing
/// is a TODO.
pub struct ChunkExtensions;

/// Positioned at the start of the next chunk (or the terminal chunk). Owns
/// the underlying IO for the duration of inter-chunk parsing.
pub struct ChunkedBodyReader<I> {
    inner: Option<ChunkedBodyChunkStream<I>>,
}

enum ChunkedBodyChunkStream<I> {
    InChunk(ChunkBodyReader<I>),
    BetweenChunk(ChunkHeadReader<I>),
    Done(FinalChunkReader<I>),
}

impl<I> From<ChunkBodyReader<I>> for ChunkedBodyChunkStream<I> {
    fn from(value: ChunkBodyReader<I>) -> Self {
        Self::InChunk(value)
    }
}
impl<I> From<ChunkHeadReader<I>> for ChunkedBodyChunkStream<I> {
    fn from(value: ChunkHeadReader<I>) -> Self {
        Self::BetweenChunk(value)
    }
}
impl<I> From<FinalChunkReader<I>> for ChunkedBodyChunkStream<I> {
    fn from(value: FinalChunkReader<I>) -> Self {
        Self::Done(value)
    }
}

impl<I> std::fmt::Debug for ChunkedBodyReader<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChunkedBodyReader").finish_non_exhaustive()
    }
}

impl<I: AsyncReadExt + Unpin> ChunkedBodyReader<I> {
    async fn read(&mut self, max_len: usize) -> BodyResult<Option<Bytes>> {
        loop {
            let Some(current) = self.inner.take() else {
                // We get left with a None after an error, which does not
                // replace inner.
                return Err(BodyError::ReadAfterError);
            };

            match current {
                ChunkedBodyChunkStream::InChunk(mut in_chunk_reader) => {
                    // We're in a chunk. Delegate to the current chunk reader.
                    match in_chunk_reader.read(max_len).await? {
                        Some(bytes) => {
                            self.inner = Some(in_chunk_reader.into());
                            return Ok(Some(bytes));
                        }
                        None => {
                            let between_chunk_reader = in_chunk_reader.drain().await?;
                            self.inner = Some(between_chunk_reader.into());
                        }
                    }
                }
                ChunkedBodyChunkStream::BetweenChunk(between_chunk_reader) => {
                    match between_chunk_reader.read_chunk().await? {
                        NextChunk::Data(in_chunk_reader) => {
                            self.inner = Some(in_chunk_reader.into())
                        }
                        NextChunk::Final(done_chunk_reader) => {
                            self.inner = Some(done_chunk_reader.into())
                        }
                    }
                }
                ChunkedBodyChunkStream::Done(done_chunk_reader) => {
                    self.inner = Some(done_chunk_reader.into());
                    return Ok(None);
                }
            }
        }
    }

    fn drained(&self) -> bool {
        matches!(self.inner, None | Some(ChunkedBodyChunkStream::Done { .. }))
    }

    async fn drain(self) -> BodyResult<(IoBuffer<I>, Headers)> {
        let Self { inner } = self;
        let Some(mut inner) = inner else {
            return Err(BodyError::ReadAfterError);
        };

        loop {
            inner = match inner {
                ChunkedBodyChunkStream::InChunk(in_chunk_reader) => {
                    in_chunk_reader.drain().await?.into()
                }
                ChunkedBodyChunkStream::BetweenChunk(between_chunk_reader) => {
                    match between_chunk_reader.read_chunk().await? {
                        NextChunk::Data(in_chunk_reader) => in_chunk_reader.into(),
                        NextChunk::Final(done_chunk_reader) => done_chunk_reader.into(),
                    }
                }
                ChunkedBodyChunkStream::Done(done_chunk_reader) => {
                    let (io, parse_slots, trailers) = done_chunk_reader.into_parts();
                    // TODO: pass these back up
                    let _ignored_parse_slots = parse_slots;
                    return Ok((io, trailers));
                }
            }
        }
    }
}

#[derive(Debug)]
enum ChunkBodyState {
    InBody,
    InFooterNeedCR,
    InFooterNeedLF,
    Done,
}

/// Positioned within a single chunk. Holds the chunk's declared size and any
/// extensions. Owns the IO for the duration of reading this chunk.
pub struct ChunkBodyReader<I> {
    io: IoBuffer<I>,
    parse_slots: ParseSlots,
    size: u64,
    extensions: ChunkExtensions,
    remaining: u64,
    state: ChunkBodyState,
}

impl<I> std::fmt::Debug for ChunkBodyReader<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChunkReader")
            .field("size", &self.size)
            .field("remaining", &self.remaining)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl<I> ChunkBodyReader<I> {
    /// The declared byte length of this chunk.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The extensions attached to this chunk header, if any.
    pub fn extensions(&self) -> &ChunkExtensions {
        &self.extensions
    }
}

impl<I: AsyncReadExt + Unpin> ChunkBodyReader<I> {
    /// Read up to `max_len` bytes from this chunk's body. Returns `None` when
    /// the chunk is fully drained and the chunk footer (`\r\n`) has been
    /// consumed.
    pub async fn read(&mut self, max_len: usize) -> BodyResult<Option<Bytes>> {
        loop {
            match self.state {
                ChunkBodyState::InBody => {
                    if self.remaining == 0 {
                        self.state = ChunkBodyState::InFooterNeedCR;
                        continue;
                    }
                    self.io.ensure().await?;
                    let at = self.remaining.min(self.io.len() as u64).min(max_len as u64) as usize;
                    self.remaining -= at as u64;
                    return Ok(Some(self.io.split_to(at)));
                }
                ChunkBodyState::InFooterNeedCR => {
                    self.io.ensure().await?;
                    match self.io.get_u8() {
                        b'\r' => {
                            self.state = ChunkBodyState::InFooterNeedLF;
                        }
                        unexpected => {
                            return Err(BodyError::InvalidChunkFooter(b'\r', unexpected));
                        }
                    }
                }
                ChunkBodyState::InFooterNeedLF => {
                    self.io.ensure().await?;
                    match self.io.get_u8() {
                        b'\n' => {
                            self.state = ChunkBodyState::Done;
                        }
                        unexpected => {
                            return Err(BodyError::InvalidChunkFooter(b'\n', unexpected));
                        }
                    }
                }
                ChunkBodyState::Done => return Ok(None),
            }
        }
    }

    pub async fn drain(mut self) -> BodyResult<ChunkHeadReader<I>> {
        while let Some(_drained) = self.read(DRAIN_SIZE).await? {
            // drain bytes until we get to the end of the chunk
        }
        let Self {
            io,
            parse_slots,
            size: _,
            extensions: _,
            remaining: _,
            state,
        } = self;
        debug_assert!(matches!(state, ChunkBodyState::Done));
        Ok(ChunkHeadReader { io, parse_slots })
    }
}

pub enum NextChunk<I> {
    Data(ChunkBodyReader<I>),
    Final(FinalChunkReader<I>),
}

pub struct ChunkHeadReader<I> {
    io: IoBuffer<I>,
    parse_slots: ParseSlots,
}

impl<I> ChunkHeadReader<I>
where
    I: AsyncReadExt + Unpin,
{
    pub async fn read_chunk(self) -> BodyResult<NextChunk<I>> {
        let Self {
            mut io,
            mut parse_slots,
        } = self;
        io.ensure().await?;
        loop {
            match httparse::parse_chunk_size(io.as_bytes())
                .map_err(BodyError::InvalidChunkHeader)?
            {
                httparse::Status::Partial => {
                    io.extend().await?;
                    continue;
                }
                httparse::Status::Complete((header_len, chunk_size)) => {
                    // TODO: these ignored bytes contain the chunk extensions we
                    // want to preserve eventually
                    let _ignored_chunk_header_bytes = io.split_to(header_len);
                    if chunk_size == 0 {
                        // terminal chunk: drain the trailer section
                        let trailers = loop {
                            io.ensure().await?;
                            match parse_slots
                                .parse_headers(io.as_buffer_mut())
                                .map_err(map_trailer_error)?
                            {
                                None => {
                                    io.extend().await?;
                                }
                                Some(trailers) => break trailers,
                            }
                        };
                        return Ok(NextChunk::Final(FinalChunkReader {
                            io,
                            parse_slots,
                            trailers,
                        }));
                    } else {
                        return Ok(NextChunk::Data(ChunkBodyReader {
                            io,
                            parse_slots,
                            size: chunk_size,
                            extensions: ChunkExtensions,
                            remaining: chunk_size,
                            state: ChunkBodyState::InBody,
                        }));
                    }
                }
            }
        }
    }
}

pub struct FinalChunkReader<I> {
    io: IoBuffer<I>,
    parse_slots: ParseSlots,
    trailers: Headers,
}

impl<I> FinalChunkReader<I> {
    pub fn into_parts(self) -> (IoBuffer<I>, ParseSlots, Headers) {
        let Self {
            io,
            parse_slots,
            trailers,
        } = self;
        (io, parse_slots, trailers)
    }
}

#[derive(Debug)]
enum BodyReaderKind<I> {
    Bodyless(BodylessBodyReader<I>),
    CL(ContentLengthBodyReader<I>),
    TE(ChunkedBodyReader<I>),
}

#[derive(Debug)]
pub struct BodylessBodyReader<I>(IoBuffer<I>);

pub struct BodyReader<I> {
    state: BodyReaderKind<I>,
}

impl<I> BodyReader<I> {
    pub fn mode(&self) -> BodyReadMode {
        match &self.state {
            BodyReaderKind::Bodyless(_) => BodyReadMode::Bodyless,
            BodyReaderKind::CL(r) => BodyReadMode::ContentLength(r.length),
            BodyReaderKind::TE(_) => BodyReadMode::Chunk,
        }
    }

    pub fn peekable(self) -> PeekableBodyReader<I> {
        PeekableBodyReader::new(self)
    }
}

impl<I: AsyncReadExt + Unpin> BodyReader<I> {
    pub fn new(io: IoBuffer<I>, mode: BodyReadMode, parse_slots: ParseSlots) -> Self {
        let state = match mode {
            BodyReadMode::Bodyless => BodyReaderKind::Bodyless(BodylessBodyReader(io)),
            BodyReadMode::Chunk => BodyReaderKind::TE(ChunkedBodyReader {
                inner: Some(ChunkedBodyChunkStream::BetweenChunk(ChunkHeadReader {
                    io,
                    parse_slots,
                })),
            }),
            BodyReadMode::ContentLength(length) => {
                BodyReaderKind::CL(ContentLengthBodyReader::new(length, io))
            }
        };
        Self { state }
    }

    pub async fn read(&mut self, max_len: usize) -> BodyResult<Option<Bytes>> {
        match &mut self.state {
            BodyReaderKind::Bodyless(_io) => Ok(None),
            BodyReaderKind::CL(cl) => Ok(cl.read(max_len).await?),
            BodyReaderKind::TE(te) => Ok(te.read(max_len).await?),
        }
    }

    pub fn drained(&self) -> bool {
        match &self.state {
            BodyReaderKind::Bodyless(_io) => true,
            BodyReaderKind::CL(cl) => cl.remaining() == 0,
            BodyReaderKind::TE(te) => te.drained(),
        }
    }

    /// Ensure the body is fully drained from the socket
    pub async fn drain(mut self) -> BodyResult<IoBuffer<I>> {
        const DRAIN_SIZE: usize = 2 * 4096;

        while let Some(_buf) = self.read(DRAIN_SIZE).await? {
            // drop buffers until we get to the end of the body
        }

        debug_assert!(
            self.drained(),
            "in spite of our best attempts, we failed to drain"
        );

        let Self { state } = self;

        let io = match state {
            BodyReaderKind::Bodyless(BodylessBodyReader(io)) => io,
            BodyReaderKind::CL(ContentLengthBodyReader { length, offset, io }) => {
                debug_assert_eq!(offset, length);
                io
            }
            BodyReaderKind::TE(te) => {
                let (io, trailers) = te.drain().await?;
                let _ignored_trailers = trailers;
                io
            }
        };

        Ok(io)
    }
}

pub struct PeekableBodyReader<I> {
    /// a buffer in which we store data peeked off the socket, but not yet read
    peeked: Buffer,
    inner: BodyReader<I>,
}

impl<I> PeekableBodyReader<I> {
    pub fn new(reader: BodyReader<I>) -> Self {
        Self {
            peeked: Buffer::new(BytesMut::new()),
            inner: reader,
        }
    }

    pub fn mode(&self) -> BodyReadMode {
        self.inner.mode()
    }
}

impl<I: AsyncReadExt + Unpin> PeekableBodyReader<I> {
    /// Peek data from the body, and allow it to be observed, but do not remove
    /// it from the read buffer. Always returns data starting from the first
    /// byte not yet read. That is, repeated calls to this method with the same
    /// length will always yield the same result unless read calls are
    /// interspersed with peek calls.
    pub async fn peek(&mut self, target_len: usize) -> BodyResult<(bool, &[u8])> {
        let mut remaining_len = target_len.saturating_sub(self.peeked.len());
        loop {
            let complete = match self.inner.read(remaining_len).await? {
                None => true,
                Some(bytes) => {
                    self.peeked.extend_from_slice(&bytes);
                    self.inner.drained()
                }
            };

            remaining_len = target_len.saturating_sub(self.peeked.len());
            if !complete && remaining_len > 0 {
                continue;
            }

            let len = self.peeked.len().min(target_len);
            return Ok((complete, &self.peeked[0..len]));
        }
    }

    pub async fn read(&mut self, max_len: usize) -> BodyResult<Option<Bytes>> {
        if !self.peeked.is_empty() {
            // If there's already data in the peek buffer, return that first.
            let at = self.peeked.len().min(max_len);
            return Ok(Some(self.peeked.split_to(at).freeze()));
        }
        self.inner.read(max_len).await
    }

    pub fn drained(&self) -> bool {
        self.inner.drained()
    }

    /// Ensure the body is fully drained from the socket
    pub async fn drain(self) -> BodyResult<IoBuffer<I>> {
        // peeked bytes have already been consumed from the underlying IO,
        // so we just need to drain the inner reader
        self.inner.drain().await
    }
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use jrpxy_http_message::message::ParseSlots;
    use jrpxy_util::io_buffer::IoBuffer;
    use tokio::io::AsyncWriteExt;

    use crate::{BodyError, BodyReadMode, BodyReader, ChunkedBodyWriter};

    #[tokio::test]
    async fn cl_peek_full() {
        let input = b"\
            0123456789
            ";

        let mut br = BodyReader::new(
            IoBuffer::new(&input[..]),
            BodyReadMode::ContentLength(10),
            ParseSlots::default(),
        )
        .peekable();
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
    async fn cl_peek_full_then_partial() {
        let input = b"\
            0123456789
            ";

        let mut br = BodyReader::new(
            IoBuffer::new(&input[..]),
            BodyReadMode::ContentLength(10),
            ParseSlots::default(),
        )
        .peekable();
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

        // peek one more time, but smaller than the previous peek
        let (complete, slice) = br.peek(1).await.expect("peek works");
        assert!(complete);
        assert_eq!(b"0", slice);

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

        let mut br = BodyReader::new(
            IoBuffer::new(&input[..]),
            BodyReadMode::ContentLength(10),
            ParseSlots::default(),
        )
        .peekable();

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
            let mut reader = BodyReader::new(
                IoBuffer::new(right),
                BodyReadMode::ContentLength(10),
                ParseSlots::default(),
            )
            .peekable();
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

        let mut br = BodyReader::new(
            IoBuffer::new(&input[..]),
            BodyReadMode::Chunk,
            ParseSlots::default(),
        )
        .peekable();
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

        let mut br = BodyReader::new(
            IoBuffer::new(&input[..]),
            BodyReadMode::Bodyless,
            ParseSlots::default(),
        )
        .peekable();
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

        let mut br = BodyReader::new(
            IoBuffer::new(&input[..]),
            BodyReadMode::Chunk,
            ParseSlots::default(),
        )
        .peekable();
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

        let mut br = BodyReader::new(
            IoBuffer::new(&input[..]),
            BodyReadMode::Chunk,
            ParseSlots::default(),
        );

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

        let mut br = BodyReader::new(
            IoBuffer::new(&input[..]),
            BodyReadMode::Chunk,
            ParseSlots::default(),
        );

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
