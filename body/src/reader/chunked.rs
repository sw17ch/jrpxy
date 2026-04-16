use std::future::poll_fn;
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use tokio::io::AsyncRead;

use jrpxy_http_message::header::Headers;
use jrpxy_http_message::message::{MessageError, ParseSlots};
use jrpxy_util::io_buffer::BytesReader;

use crate::error::{BodyError, BodyResult, TrailerError};

fn map_trailer_error(e: MessageError) -> BodyError {
    let trailer_error = match e {
        MessageError::Parse(httparse::Error::TooManyHeaders) => TrailerError::TooManyFields,
        MessageError::Parse(e) => TrailerError::InvalidField(e),
        _ => unreachable!("parse_headers only produces Parse errors"),
    };
    BodyError::TrailerError(trailer_error)
}

/// The extensions attached to a chunk header. Currently opaque; full parsing
/// is a TODO.
#[derive(Debug)]
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
#[derive(Debug)]
pub struct ChunkedBodyReader<I> {
    inner: Option<ChunkedBodyChunkStream<I>>,
}

#[derive(Debug)]
enum ChunkedBodyChunkStream<I> {
    InChunk(ChunkDataReader<I>),
    BetweenChunk(ChunkHeadReader<I>),
    Done(FinalChunkReader<I>),
}

impl<I> From<ChunkDataReader<I>> for ChunkedBodyChunkStream<I> {
    fn from(value: ChunkDataReader<I>) -> Self {
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

    /// Finish the chunked body reader and return the inner parts.
    ///
    /// # Panics
    ///
    /// Panics if the reader has not been fully drained or if an error occurred
    /// while reading or draining.
    pub fn finish(self) -> (BytesReader<I>, ParseSlots, Headers) {
        let Self { inner } = self;
        match inner {
            Some(ChunkedBodyChunkStream::Done(done_chunk_reader)) => done_chunk_reader.into_parts(),
            Some(_) => {
                panic!("attempted to finish the chunked body reader before it was fully drained")
            }
            None => panic!("attempted to finish the chunked body reader after an error"),
        }
    }
}

impl<I: AsyncRead + Unpin> ChunkedBodyReader<I> {
    pub async fn read(&mut self, max_len: usize) -> BodyResult<Option<Bytes>> {
        poll_fn(|cx| self.poll_read(cx, max_len)).await
    }

    pub fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        max_len: usize,
    ) -> Poll<BodyResult<Option<Bytes>>> {
        loop {
            let Some(current) = self.inner.take() else {
                // We get left with a None after an error, which does not
                // replace inner.
                return Poll::Ready(Err(BodyError::ReadAfterError));
            };

            match current {
                ChunkedBodyChunkStream::InChunk(mut reader) => {
                    match reader.poll_read(cx, max_len) {
                        Poll::Pending => {
                            self.inner = Some(reader.into());
                            return Poll::Pending;
                        }
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Ready(Ok(Some(bytes))) => {
                            self.inner = Some(reader.into());
                            return Poll::Ready(Ok(Some(bytes)));
                        }
                        // poll_read returning None means the chunk body and
                        // footer are fully consumed, so we finish directly
                        // without going through poll_drain.
                        Poll::Ready(Ok(None)) => {
                            self.inner = Some(reader.finish().into());
                        }
                    }
                }
                ChunkedBodyChunkStream::BetweenChunk(mut reader) => {
                    match reader.poll_read_chunk(cx) {
                        Poll::Pending => {
                            self.inner = Some(reader.into());
                            return Poll::Pending;
                        }
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Ready(Ok(())) => match reader.finish() {
                            NextChunk::Data(r) => self.inner = Some(r.into()),
                            NextChunk::Final(r) => self.inner = Some(r.into()),
                        },
                    }
                }
                ChunkedBodyChunkStream::Done(reader) => {
                    self.inner = Some(reader.into());
                    return Poll::Ready(Ok(None));
                }
            }
        }
    }

    pub async fn drain(mut self) -> BodyResult<(BytesReader<I>, ParseSlots, Headers)> {
        poll_fn(|cx| self.poll_drain(cx)).await?;
        Ok(self.finish())
    }

    pub fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        loop {
            match ready!(self.poll_read(cx, super::DRAIN_SIZE)) {
                Err(e) => return Poll::Ready(Err(e)),
                Ok(None) => return Poll::Ready(Ok(())),
                Ok(Some(_)) => continue,
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
pub struct ChunkDataReader<I> {
    reader: BytesReader<I>,
    parse_slots: ParseSlots,
    size: u64,
    extensions: ChunkExtensions,
    remaining: u64,
    state: ChunkBodyState,
}

impl<I> std::fmt::Debug for ChunkDataReader<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChunkReader")
            .field("size", &self.size)
            .field("remaining", &self.remaining)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl<I> ChunkDataReader<I> {
    /// The declared byte length of this chunk.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The extensions attached to this chunk header, if any.
    pub fn extensions(&self) -> &ChunkExtensions {
        &self.extensions
    }
}

impl<I: AsyncRead + Unpin> ChunkDataReader<I> {
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
                    if let Err(e) = ready!(super::poll_ensure(cx, &mut self.reader)) {
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
                    if let Err(e) = ready!(super::poll_ensure(cx, &mut self.reader)) {
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
                    if let Err(e) = ready!(super::poll_ensure(cx, &mut self.reader)) {
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

    pub async fn drain(mut self) -> BodyResult<ChunkHeadReader<I>> {
        poll_fn(|cx| Self::poll_drain(&mut self, cx)).await?;
        Ok(self.finish())
    }

    pub fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        loop {
            match ready!(self.poll_read(cx, super::DRAIN_SIZE)) {
                Err(e) => return Poll::Ready(Err(e)),
                Ok(None) => return Poll::Ready(Ok(())),
                Ok(Some(_)) => continue,
            }
        }
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
    Data(ChunkDataReader<I>),
    Final(FinalChunkReader<I>),
}

#[derive(Debug)]
enum ChunkHeadReaderState {
    ReadingSize,
    ReadingTrailers { chunk_header_bytes: Bytes },
    Ready(PollNextChunk),
}

#[derive(Debug)]
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

#[derive(Debug)]
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
        Ok(self.finish())
    }

    /// Poll for the next chunk.
    pub fn poll_read_chunk(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        loop {
            match &mut self.state {
                ChunkHeadReaderState::ReadingSize => {
                    if let Err(e) = ready!(super::poll_ensure(cx, &mut self.reader)) {
                        return Poll::Ready(Err(e));
                    }

                    match httparse::parse_chunk_size(self.reader.as_bytes())
                        .map_err(BodyError::InvalidChunkHeader)?
                    {
                        httparse::Status::Partial => {
                            if let Err(e) = ready!(super::poll_extend(cx, &mut self.reader)) {
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
                        if let Err(e) = ready!(super::poll_ensure(cx, &mut self.reader)) {
                            return Poll::Ready(Err(e));
                        }
                        match self
                            .parse_slots
                            .parse_headers(&mut self.reader)
                            .map_err(map_trailer_error)?
                        {
                            None => {
                                if let Err(e) = ready!(super::poll_extend(cx, &mut self.reader)) {
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
    pub fn finish(self) -> NextChunk<I> {
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
                PollNextChunk::Data { size, chunk_header } => NextChunk::Data(ChunkDataReader {
                    reader,
                    parse_slots,
                    size,
                    extensions: ChunkExtensions::new(chunk_header),
                    remaining: size,
                    state: ChunkBodyState::InBody,
                }),
                PollNextChunk::Final {
                    chunk_header,
                    trailers,
                } => NextChunk::Final(FinalChunkReader {
                    reader,
                    parse_slots,
                    extensions: ChunkExtensions::new(chunk_header),
                    trailers,
                }),
            },
        }
    }
}

#[derive(Debug)]
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
