use std::{
    future::poll_fn,
    task::{Context, Poll, ready},
};

use bytes::Bytes;
use jrpxy_body::{
    error::BodyError,
    reader::{
        BodylessBodyReader, ChunkDataReader, ChunkHeadReader, ContentLengthBodyReader,
        FinalChunkReader, NextChunk,
    },
};
use jrpxy_http_message::{
    framing::ParsedFraming,
    header::Headers,
    message::{ParseSlots, Request},
};
use jrpxy_util::io_buffer::BytesReader;
use tokio::io::AsyncReadExt;

use crate::error::{ProxyFrontendReaderError, ProxyFrontendReaderResult};

const DRAIN_SIZE: usize = 4096;

/// Store `$state` and return `Poll::Pending` atomically, preventing the
/// common mistake of storing state without returning.
macro_rules! park {
    ($self:expr, $state:expr) => {{
        $self.state = Some($state);
        return Poll::Pending;
    }};
}

/// Wrap `$err` in `Poll::Ready(Err(_))` and return.
macro_rules! bail {
    ($err:expr) => {{
        return Poll::Ready(Err($err));
    }};
}

/// Reads a stream of HTTP/1.x requests from a frontend connection.
pub struct ProxyFrontendReader<I> {
    reader: BytesReader<I>,
    parse_slots: ParseSlots,
}

impl<I> ProxyFrontendReader<I> {
    pub fn new(reader: I, max_headers: usize) -> Self {
        Self::new_with_buffer(BytesReader::new(reader), max_headers)
    }

    pub fn new_with_buffer(reader: BytesReader<I>, max_headers: usize) -> Self {
        Self {
            reader,
            parse_slots: ParseSlots::new(max_headers),
        }
    }

    pub fn into_inner(self) -> BytesReader<I> {
        let Self {
            reader,
            parse_slots: _,
        } = self;
        reader
    }

    pub fn as_buf_slice(&self) -> &[u8] {
        self.reader.as_bytes()
    }
}

impl<I: AsyncReadExt + Unpin> ProxyFrontendReader<I> {
    async fn head(&mut self, max_head_length: usize) -> ProxyFrontendReaderResult<Request> {
        loop {
            if let Some(req) = self
                .parse_slots
                .parse_request(&mut self.reader)
                .map_err(ProxyFrontendReaderError::ParseError)?
            {
                return Ok(req);
            } else if self.reader.len() >= max_head_length {
                return Err(ProxyFrontendReaderError::MaxHeadLenExceeded(
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
                .map_err(ProxyFrontendReaderError::ReadError)?;
            if len == 0 {
                return if first_read {
                    // if the first attempt to read data into the buffer for a
                    // request finds EOF, the frontend has gone away and won't be
                    // sending us more data in the pipeline.
                    Err(ProxyFrontendReaderError::FirstReadEOF)
                } else {
                    Err(ProxyFrontendReaderError::UnexpectedEOF)
                };
            }
        }
    }

    /// Read the next request from the frontend.
    pub async fn read(
        mut self,
        max_head_length: usize,
    ) -> ProxyFrontendReaderResult<ProxyFrontendRequest<I>> {
        let req = self.head(max_head_length).await?;
        let Self {
            reader,
            parse_slots,
        } = self;

        let body_reader = match req.framing()? {
            ParsedFraming::Length(cl) => {
                ProxyFrontendBodyReader::CL(ProxyFrontendContentLengthBodyReader {
                    inner: ContentLengthBodyReader::new(cl, reader, parse_slots),
                })
            }
            ParsedFraming::Chunked => ProxyFrontendBodyReader::TE(
                ProxyFrontendChunkedBodyReader::new(reader, parse_slots),
            ),
            ParsedFraming::NoFraming => {
                ProxyFrontendBodyReader::Bodyless(ProxyFrontendBodylessBodyReader {
                    inner: BodylessBodyReader::new(reader, parse_slots),
                })
            }
        };

        Ok(ProxyFrontendRequest { req, body_reader })
    }
}

/// A parsed request with its associated body reader.
pub struct ProxyFrontendRequest<I> {
    req: Request,
    body_reader: ProxyFrontendBodyReader<I>,
}

impl<I> ProxyFrontendRequest<I> {
    pub fn req(&self) -> &Request {
        &self.req
    }

    pub fn req_mut(&mut self) -> &mut Request {
        &mut self.req
    }

    pub fn into_parts(self) -> (Request, ProxyFrontendBodyReader<I>) {
        let Self { req, body_reader } = self;
        (req, body_reader)
    }
}

pub struct ProxyFrontendBodylessBodyReader<I> {
    pub(crate) inner: BodylessBodyReader<I>,
}

impl<I> ProxyFrontendBodylessBodyReader<I> {
    pub fn drain(self) -> ProxyFrontendReader<I> {
        let (reader, parse_slots) = self.inner.drain();
        ProxyFrontendReader {
            reader,
            parse_slots,
        }
    }
}

pub struct ProxyFrontendContentLengthBodyReader<I> {
    pub(crate) inner: ContentLengthBodyReader<I>,
}

impl<I> ProxyFrontendContentLengthBodyReader<I> {
    pub fn content_length(&self) -> u64 {
        self.inner.content_length()
    }

    /// Recover the [`ProxyFrontendReader`] after the body has been fully
    /// drained. Panics if called before draining.
    pub fn finish(self) -> ProxyFrontendReader<I> {
        let (reader, parse_slots) = self.inner.finish();
        ProxyFrontendReader {
            reader,
            parse_slots,
        }
    }
}

impl<I: AsyncReadExt + Unpin> ProxyFrontendContentLengthBodyReader<I> {
    pub async fn read(&mut self, max_len: usize) -> ProxyFrontendReaderResult<Option<Bytes>> {
        poll_fn(|cx| self.poll_read(cx, max_len)).await
    }

    pub fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        max_len: usize,
    ) -> Poll<ProxyFrontendReaderResult<Option<Bytes>>> {
        Poll::Ready(
            ready!(self.inner.poll_read(cx, max_len))
                .map_err(ProxyFrontendReaderError::BodyReadError),
        )
    }

    pub async fn drain(mut self) -> ProxyFrontendReaderResult<ProxyFrontendReader<I>> {
        poll_fn(|cx| self.poll_drain(cx)).await?;
        Ok(self.finish())
    }

    pub fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<ProxyFrontendReaderResult<()>> {
        Poll::Ready(
            ready!(self.inner.poll_drain(cx)).map_err(ProxyFrontendReaderError::BodyReadError),
        )
    }
}

pub(crate) enum FrontendChunkedState<I> {
    Between(ChunkHeadReader<I>),
    InChunk(ChunkDataReader<I>),
    Done(FinalChunkReader<I>),
}

pub struct ProxyFrontendChunkedBodyReader<I> {
    pub(crate) state: Option<FrontendChunkedState<I>>,
}

impl<I> ProxyFrontendChunkedBodyReader<I> {
    pub(crate) fn new(reader: BytesReader<I>, parse_slots: ParseSlots) -> Self {
        Self {
            state: Some(FrontendChunkedState::Between(ChunkHeadReader::new(
                reader,
                parse_slots,
            ))),
        }
    }

    pub fn drained(&self) -> bool {
        matches!(self.state, None | Some(FrontendChunkedState::Done(_)))
    }

    /// Recover the [`ProxyFrontendReader`] and trailers after the body has been
    /// fully drained. Panics if called before draining.
    pub fn finish(self) -> (ProxyFrontendReader<I>, Headers) {
        let Self { state } = self;
        match state {
            Some(FrontendChunkedState::Done(r)) => {
                let (reader, parse_slots, trailers) = r.into_parts();
                (
                    ProxyFrontendReader {
                        reader,
                        parse_slots,
                    },
                    trailers,
                )
            }
            // TODO: this should not panic. return an error if in the wrong state.
            Some(_) => {
                panic!("attempted to finish the chunked body reader before it was fully drained")
            }
            None => panic!("attempted to finish the chunked body reader after an error"),
        }
    }
}

impl<I: AsyncReadExt + Unpin> ProxyFrontendChunkedBodyReader<I> {
    pub async fn read(&mut self, max_len: usize) -> ProxyFrontendReaderResult<Option<Bytes>> {
        poll_fn(|cx| self.poll_read(cx, max_len)).await
    }

    pub fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        max_len: usize,
    ) -> Poll<ProxyFrontendReaderResult<Option<Bytes>>> {
        loop {
            let Some(current) = self.state.take() else {
                bail!(ProxyFrontendReaderError::BodyReadError(
                    BodyError::ReadAfterError,
                ));
            };
            match current {
                FrontendChunkedState::InChunk(mut r) => match r.poll_read(cx, max_len) {
                    Poll::Pending => park!(self, FrontendChunkedState::InChunk(r)),
                    Poll::Ready(Err(e)) => bail!(ProxyFrontendReaderError::BodyReadError(e)),
                    Poll::Ready(Ok(Some(b))) => {
                        self.state = Some(FrontendChunkedState::InChunk(r));
                        return Poll::Ready(Ok(Some(b)));
                    }
                    Poll::Ready(Ok(None)) => {
                        self.state = Some(FrontendChunkedState::Between(r.finish()));
                    }
                },
                FrontendChunkedState::Between(mut r) => match r.poll_read_chunk(cx) {
                    Poll::Pending => park!(self, FrontendChunkedState::Between(r)),
                    Poll::Ready(Err(e)) => bail!(ProxyFrontendReaderError::BodyReadError(e)),
                    Poll::Ready(Ok(())) => match r.finish() {
                        NextChunk::Data(r) => {
                            self.state = Some(FrontendChunkedState::InChunk(r));
                        }
                        NextChunk::Final(r) => {
                            self.state = Some(FrontendChunkedState::Done(r));
                        }
                    },
                },
                FrontendChunkedState::Done(r) => {
                    self.state = Some(FrontendChunkedState::Done(r));
                    return Poll::Ready(Ok(None));
                }
            }
        }
    }

    pub async fn drain(mut self) -> ProxyFrontendReaderResult<(ProxyFrontendReader<I>, Headers)> {
        poll_fn(|cx| self.poll_drain(cx)).await?;
        Ok(self.finish())
    }

    pub fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<ProxyFrontendReaderResult<()>> {
        loop {
            match ready!(self.poll_read(cx, DRAIN_SIZE)) {
                Err(e) => bail!(e),
                Ok(None) => return Poll::Ready(Ok(())),
                Ok(Some(_)) => continue,
            }
        }
    }
}

/// A frontend request body reader. Each variant corresponds to a different
/// framing mode.
pub enum ProxyFrontendBodyReader<I> {
    Bodyless(ProxyFrontendBodylessBodyReader<I>),
    CL(ProxyFrontendContentLengthBodyReader<I>),
    TE(ProxyFrontendChunkedBodyReader<I>),
}

impl<I: AsyncReadExt + Unpin> ProxyFrontendBodyReader<I> {
    /// Read up to `max_len` bytes from the body. Returns `None` when the body
    /// is complete.
    pub async fn read(&mut self, max_len: usize) -> ProxyFrontendReaderResult<Option<Bytes>> {
        match self {
            ProxyFrontendBodyReader::Bodyless(_) => Ok(None),
            ProxyFrontendBodyReader::CL(r) => r.read(max_len).await,
            ProxyFrontendBodyReader::TE(r) => r.read(max_len).await,
        }
    }

    pub fn drained(&self) -> bool {
        match self {
            ProxyFrontendBodyReader::Bodyless(_) => true,
            ProxyFrontendBodyReader::CL(r) => r.inner.drained(),
            ProxyFrontendBodyReader::TE(r) => r.drained(),
        }
    }

    /// Drain all remaining body bytes and return a [`ProxyFrontendReader`]
    /// ready for the next pipelined request.
    pub async fn drain(self) -> ProxyFrontendReaderResult<ProxyFrontendReader<I>> {
        match self {
            ProxyFrontendBodyReader::Bodyless(r) => Ok(r.drain()),
            ProxyFrontendBodyReader::CL(r) => r.drain().await,
            ProxyFrontendBodyReader::TE(r) => {
                let (reader, _trailers) = r.drain().await?;
                Ok(reader)
            }
        }
    }
}
