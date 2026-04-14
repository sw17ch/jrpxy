use std::future::poll_fn;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt};

use jrpxy_http_message::header::Headers;
use jrpxy_http_message::message::{MessageError, ParseSlots};
use jrpxy_util::io_buffer::BytesReader;

use crate::error::{BodyError, BodyResult, TrailerError};

const DRAIN_SIZE: usize = 4096;
const IO_FILL_LEN: usize = 4096;

fn poll_ensure<I: AsyncRead + Unpin>(
    cx: &mut Context<'_>,
    reader: &mut BytesReader<I>,
) -> Poll<BodyResult<()>> {
    let mut ensure = reader.ensure(IO_FILL_LEN);
    Poll::Ready(ready!(Pin::new(&mut ensure).poll(cx)).map_err(BodyError::from))
}

fn poll_extend<I: AsyncRead + Unpin>(
    cx: &mut Context<'_>,
    reader: &mut BytesReader<I>,
) -> Poll<BodyResult<()>> {
    let mut extend = reader.extend(IO_FILL_LEN);
    Poll::Ready(ready!(Pin::new(&mut extend).poll(cx)).map_err(BodyError::from))
}

fn map_trailer_error(e: MessageError) -> BodyError {
    let trailer_error = match e {
        MessageError::Parse(httparse::Error::TooManyHeaders) => TrailerError::TooManyFields,
        MessageError::Parse(e) => TrailerError::InvalidField(e),
        _ => unreachable!("parse_headers only produces Parse errors"),
    };
    BodyError::TrailerError(trailer_error)
}

#[derive(Debug)]
pub struct ContentLengthBodyReader<I> {
    /// the total body length specified by the content-length header.
    length: u64,
    /// the amount of the body already read
    offset: u64,
    /// the io associated with this body read
    reader: BytesReader<I>,
    /// These are not ever used while processing a content-length body. They
    /// exist here in order to allow higher level calls to reuse the slot
    /// allocation.
    parse_slots: ParseSlots,
}

impl<I> ContentLengthBodyReader<I> {
    pub fn new(length: u64, reader: BytesReader<I>, parse_slots: ParseSlots) -> Self {
        Self {
            length,
            offset: 0,
            reader,
            parse_slots,
        }
    }

    pub fn content_length(&self) -> u64 {
        self.length
    }

    pub fn drained(&self) -> bool {
        self.remaining() == 0
    }

    fn remaining(&self) -> u64 {
        debug_assert!(self.offset <= self.length);
        self.length - self.offset
    }
}

impl<I> ContentLengthBodyReader<I>
where
    I: AsyncRead + Unpin,
{
    pub async fn read(&mut self, max_len: usize) -> BodyResult<Option<Bytes>> {
        poll_fn(|cx| Self::poll_read(self, cx, max_len)).await
    }

    pub fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        max_len: usize,
    ) -> Poll<BodyResult<Option<Bytes>>> {
        let remaining = self.remaining();
        if remaining == 0 {
            return Poll::Ready(Ok(None));
        }

        if let Err(e) = ready!(poll_ensure(cx, &mut self.reader)) {
            return Poll::Ready(Err(e));
        }

        let at = remaining
            .try_into()
            .unwrap_or(usize::MAX)
            .min(self.reader.len())
            .min(max_len);

        self.offset += at as u64;

        Poll::Ready(Ok(Some(self.reader.split_to(at))))
    }

    /// Drain the [`ContentLengthBodyReader`] and return the inner
    /// [`IoBuffer`] and [`ParseSlots`].
    pub async fn drain(mut self) -> BodyResult<(BytesReader<I>, ParseSlots)> {
        while let Some(_buf) = self.read(DRAIN_SIZE).await? {
            // drop buffers until we get to the end of the body
        }
        let Self {
            length,
            offset,
            reader,
            parse_slots,
        } = self;
        debug_assert_eq!(offset, length);
        Ok((reader, parse_slots))
    }
}

/// The extensions attached to a chunk header. Currently opaque; full parsing
/// is a TODO.
pub struct ChunkExtensions {
    chunk_header_bytes: Bytes,
}

impl ChunkExtensions {
    fn new(chunk_header_bytes: Bytes) -> Self {
        Self { chunk_header_bytes }
    }

    pub fn header_bytes(&self) -> &Bytes {
        &self.chunk_header_bytes
    }
}

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

impl<I> ChunkedBodyReader<I> {
    pub fn new(reader: BytesReader<I>, parse_slots: ParseSlots) -> Self {
        Self {
            inner: Some(ChunkedBodyChunkStream::BetweenChunk(ChunkHeadReader::new(
                reader,
                parse_slots,
            ))),
        }
    }

    pub fn drained(&self) -> bool {
        matches!(self.inner, None | Some(ChunkedBodyChunkStream::Done { .. }))
    }
}

impl<I: AsyncReadExt + Unpin> ChunkedBodyReader<I> {
    pub async fn read(&mut self, max_len: usize) -> BodyResult<Option<Bytes>> {
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

    pub async fn drain(self) -> BodyResult<(BytesReader<I>, ParseSlots, Headers)> {
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
                    return Ok(done_chunk_reader.into_parts());
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
    reader: BytesReader<I>,
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

impl<I: AsyncRead + Unpin> ChunkBodyReader<I> {
    /// Read up to `max_len` bytes from this chunk's body. Returns `None` when
    /// the chunk is fully drained and the chunk footer (`\r\n`) has been
    /// consumed.
    pub async fn read(&mut self, max_len: usize) -> BodyResult<Option<Bytes>> {
        poll_fn(|cx| Self::poll_read(self, cx, max_len)).await
    }

    pub fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        max_len: usize,
    ) -> Poll<BodyResult<Option<Bytes>>> {
        loop {
            match self.state {
                ChunkBodyState::InBody => {
                    if self.remaining == 0 {
                        self.state = ChunkBodyState::InFooterNeedCR;
                        continue;
                    }
                    if let Err(e) = ready!(poll_ensure(cx, &mut self.reader)) {
                        return Poll::Ready(Err(e));
                    }
                    let at = self
                        .remaining
                        .min(self.reader.len() as u64)
                        .min(max_len as u64) as usize;
                    self.remaining -= at as u64;
                    return Poll::Ready(Ok(Some(self.reader.split_to(at))));
                }
                ChunkBodyState::InFooterNeedCR => {
                    if let Err(e) = ready!(poll_ensure(cx, &mut self.reader)) {
                        return Poll::Ready(Err(e));
                    }
                    match self.reader.get_u8() {
                        b'\r' => {
                            self.state = ChunkBodyState::InFooterNeedLF;
                        }
                        unexpected => {
                            return Poll::Ready(Err(BodyError::InvalidChunkFooter(
                                b'\r', unexpected,
                            )));
                        }
                    }
                }
                ChunkBodyState::InFooterNeedLF => {
                    if let Err(e) = ready!(poll_ensure(cx, &mut self.reader)) {
                        return Poll::Ready(Err(e));
                    }
                    match self.reader.get_u8() {
                        b'\n' => {
                            self.state = ChunkBodyState::Done;
                        }
                        unexpected => {
                            return Poll::Ready(Err(BodyError::InvalidChunkFooter(
                                b'\n', unexpected,
                            )));
                        }
                    }
                }
                ChunkBodyState::Done => return Poll::Ready(Ok(None)),
            }
        }
    }

    pub fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        loop {
            match ready!(self.poll_read(cx, DRAIN_SIZE)) {
                Err(e) => return Poll::Ready(Err(e)),
                Ok(None) => return Poll::Ready(Ok(())),
                Ok(Some(_)) => continue,
            }
        }
    }

    pub async fn drain(mut self) -> BodyResult<ChunkHeadReader<I>> {
        poll_fn(|cx| Self::poll_drain(&mut self, cx)).await?;
        Ok(self.finish())
    }

    /// Finish the body chunk body reader returning a head reader.
    ///
    /// # Panics
    ///
    /// Panics if the body reader is not yet fully drained.
    pub fn finish(self) -> ChunkHeadReader<I> {
        let Self {
            reader,
            parse_slots,
            size: _,
            extensions: _,
            remaining: _,
            state,
        } = self;
        assert!(
            matches!(state, ChunkBodyState::Done),
            "attempted to finish the chunk reader before it was fully drained"
        );
        ChunkHeadReader::new(reader, parse_slots)
    }
}

pub enum NextChunk<I> {
    Data(ChunkBodyReader<I>),
    Final(FinalChunkReader<I>),
}

enum ChunkHeadReaderState {
    ReadingSize,
    ReadingTrailers { chunk_header_bytes: Bytes },
    Ready(PollNextChunk),
}

enum PollNextChunk {
    Data {
        size: u64,
        chunk_header: Bytes,
    },
    Final {
        chunk_header: Bytes,
        trailers: Headers,
    },
}

pub struct ChunkHeadReader<I> {
    reader: BytesReader<I>,
    parse_slots: ParseSlots,
    state: ChunkHeadReaderState,
}

impl<I> ChunkHeadReader<I> {
    fn new(reader: BytesReader<I>, parse_slots: ParseSlots) -> Self {
        Self {
            reader,
            parse_slots,
            state: ChunkHeadReaderState::ReadingSize,
        }
    }
}

impl<I> ChunkHeadReader<I>
where
    I: AsyncRead + Unpin,
{
    /// Read the next chunk.
    pub async fn read_chunk(mut self) -> BodyResult<NextChunk<I>> {
        let () = poll_fn(|cx| Self::poll_read_chunk(&mut self, cx)).await?;
        self.finish()
    }

    /// Poll for the next chunk.
    pub fn poll_read_chunk(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        loop {
            match &mut self.state {
                ChunkHeadReaderState::ReadingSize => {
                    if let Err(e) = ready!(poll_ensure(cx, &mut self.reader)) {
                        return Poll::Ready(Err(e));
                    }

                    match httparse::parse_chunk_size(self.reader.as_bytes())
                        .map_err(BodyError::InvalidChunkHeader)?
                    {
                        httparse::Status::Partial => {
                            if let Err(e) = ready!(poll_extend(cx, &mut self.reader)) {
                                return Poll::Ready(Err(e));
                            }
                            continue;
                        }
                        httparse::Status::Complete((header_len, chunk_size)) => {
                            let chunk_header_bytes = self.reader.split_to(header_len);
                            if chunk_size == 0 {
                                self.state =
                                    ChunkHeadReaderState::ReadingTrailers { chunk_header_bytes };
                            } else {
                                self.state = ChunkHeadReaderState::Ready(PollNextChunk::Data {
                                    size: chunk_size,
                                    chunk_header: chunk_header_bytes,
                                });
                            }
                        }
                    }
                }
                ChunkHeadReaderState::ReadingTrailers { chunk_header_bytes } => {
                    // terminal chunk: drain the trailer section
                    loop {
                        if let Err(e) = ready!(poll_ensure(cx, &mut self.reader)) {
                            return Poll::Ready(Err(e));
                        }
                        match self
                            .parse_slots
                            .parse_headers(&mut self.reader)
                            .map_err(map_trailer_error)?
                        {
                            None => {
                                if let Err(e) = ready!(poll_extend(cx, &mut self.reader)) {
                                    return Poll::Ready(Err(e));
                                }
                            }
                            Some(trailers) => {
                                self.state = ChunkHeadReaderState::Ready(PollNextChunk::Final {
                                    chunk_header: std::mem::take(chunk_header_bytes),
                                    trailers,
                                });
                                return Poll::Ready(Ok(()));
                            }
                        }
                    }
                }
                ChunkHeadReaderState::Ready(_next) => return Poll::Ready(Ok(())),
            }
        }
    }

    /// Finish the head reader.
    ///
    /// # Panics
    ///
    /// Panics if the head reader has not successfully polled
    /// [`Self::poll_read_chunk`] to completion.
    pub fn finish(self) -> BodyResult<NextChunk<I>> {
        let Self {
            reader,
            parse_slots,
            state,
        } = self;

        match state {
            ChunkHeadReaderState::ReadingSize => panic!("tried to finish while reading size"),
            ChunkHeadReaderState::ReadingTrailers { .. } => {
                panic!("tried to finish while reading trailers")
            }
            ChunkHeadReaderState::Ready(next) => match next {
                PollNextChunk::Data { size, chunk_header } => {
                    Ok(NextChunk::Data(ChunkBodyReader {
                        reader,
                        parse_slots,
                        size,
                        extensions: ChunkExtensions::new(chunk_header),
                        remaining: size,
                        state: ChunkBodyState::InBody,
                    }))
                }
                PollNextChunk::Final {
                    chunk_header,
                    trailers,
                } => Ok(NextChunk::Final(FinalChunkReader {
                    reader,
                    parse_slots,
                    extensions: ChunkExtensions::new(chunk_header),
                    trailers,
                })),
            },
        }
    }
}

pub struct FinalChunkReader<I> {
    reader: BytesReader<I>,
    parse_slots: ParseSlots,
    extensions: ChunkExtensions,
    trailers: Headers,
}

impl<I> FinalChunkReader<I> {
    pub fn trailers(&self) -> &Headers {
        &self.trailers
    }

    pub fn extensions(&self) -> &ChunkExtensions {
        &self.extensions
    }

    pub fn into_parts(self) -> (BytesReader<I>, ParseSlots, Headers) {
        let Self {
            reader,
            parse_slots,
            trailers,
            extensions: _,
        } = self;
        (reader, parse_slots, trailers)
    }
}

#[cfg(test)]
mod test {
    use jrpxy_http_message::message::ParseSlots;
    use jrpxy_util::io_buffer::BytesReader;

    use crate::error::BodyError;

    use super::ChunkedBodyReader;

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

        let mut br = ChunkedBodyReader::new(BytesReader::new(&input[..]), ParseSlots::default());
        let chunk = br.read(10).await.expect("read works").unwrap();
        assert_eq!(b"0123", chunk.as_ref());
        let chunk = br.read(10).await.expect("read works").unwrap();
        assert_eq!(b"012", chunk.as_ref());
        assert!(br.read(10).await.expect("read works").is_none());
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

        let mut br = ChunkedBodyReader::new(BytesReader::new(&input[..]), ParseSlots::default());

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

        let mut br = ChunkedBodyReader::new(BytesReader::new(&input[..]), ParseSlots::default());

        // The body bytes themselves are valid and should be readable.
        let chunk = br.read(4).await.expect("reading body bytes works").unwrap();
        assert_eq!(b"0123", chunk.as_ref());

        // Reading past the body must result in an error.
        assert!(matches!(
            br.read(1).await.unwrap_err(),
            BodyError::InvalidChunkFooter(b'\n', b'X'),
        ));
    }
}

/// Body reader for responses with no framing headers where the body is
/// terminated by connection close (RFC 9112 section 6.3 rule 8).
#[derive(Debug)]
pub struct EofBodyReader<I> {
    reader: BytesReader<I>,
    parse_slots: ParseSlots,
    eof: bool,
}

impl<I> EofBodyReader<I> {
    pub fn new(reader: BytesReader<I>, parse_slots: ParseSlots) -> Self {
        Self {
            reader,
            parse_slots,
            eof: false,
        }
    }
}

impl<I: AsyncReadExt + Unpin> EofBodyReader<I> {
    pub async fn read(&mut self, max_len: usize) -> BodyResult<Option<Bytes>> {
        if self.eof {
            return Ok(None);
        }
        // Return already-buffered data first.
        if !self.reader.is_empty() {
            let at = self.reader.len().min(max_len);
            return Ok(Some(self.reader.split_to(at)));
        }
        // Try to read more; a zero return means the connection was closed.
        let n = self
            .reader
            .read(max_len)
            .await
            .map_err(BodyError::BodyReadError)?;
        if n == 0 {
            self.eof = true;
            return Ok(None);
        }
        let at = self.reader.len().min(max_len);
        Ok(Some(self.reader.split_to(at)))
    }

    pub fn drained(&self) -> bool {
        self.eof
    }

    pub async fn drain(mut self) -> BodyResult<ParseSlots> {
        while let Some(_drained) = self.read(DRAIN_SIZE).await? {
            // discard anything we read
        }
        let Self {
            // drop the IO. we can't use it again as it has reached EOF.
            reader: _,
            parse_slots,
            eof: _,
        } = self;
        Ok(parse_slots)
    }
}

#[derive(Debug)]
pub struct BodylessBodyReader<I> {
    reader: BytesReader<I>,
    parse_slots: ParseSlots,
}

impl<I> BodylessBodyReader<I> {
    pub fn new(reader: BytesReader<I>, parse_slots: ParseSlots) -> Self {
        Self {
            reader,
            parse_slots,
        }
    }

    pub fn drain(self) -> (BytesReader<I>, ParseSlots) {
        let Self {
            reader,
            parse_slots,
        } = self;
        (reader, parse_slots)
    }
}
