use std::{
    future::poll_fn,
    task::{Context, Poll, ready},
};

use bytes::Bytes;
use jrpxy_body::{
    error::BodyError,
    reader::{
        BodylessBodyReader, ChunkDataReader, ChunkHeadReader, ContentLengthBodyReader,
        EofBodyReader, FinalChunkReader, NextChunk,
    },
};
use jrpxy_http_message::{
    framing::ParsedFraming,
    header::Headers,
    message::{ParseSlots, Response},
};
use jrpxy_util::io_buffer::BytesReader;
use tokio::io::AsyncReadExt;

use crate::error::{ProxyBackendReaderError, ProxyBackendReaderResult};

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

/// Reads a stream of HTTP/1.x responses from a backend connection.
pub struct ProxyBackendReader<I> {
    reader: BytesReader<I>,
    parse_slots: ParseSlots,
}

impl<I> ProxyBackendReader<I> {
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
        self.reader
    }
}

impl<I: AsyncReadExt + Unpin> ProxyBackendReader<I> {
    async fn head(&mut self, max_head_length: usize) -> ProxyBackendReaderResult<Response> {
        loop {
            if let Some(res) = self
                .parse_slots
                .parse_response(&mut self.reader)
                .map_err(ProxyBackendReaderError::ParseError)?
            {
                return Ok(res);
            } else if self.reader.len() >= max_head_length {
                return Err(ProxyBackendReaderError::MaxHeadLenExceeded(
                    self.reader.len(),
                    max_head_length,
                ));
            }

            let first_read = self.reader.is_empty();
            let target_read_len = max_head_length.saturating_sub(self.reader.len());
            let len = self
                .reader
                .read(target_read_len)
                .await
                .map_err(ProxyBackendReaderError::ReadError)?;
            if len == 0 {
                return if first_read {
                    Err(ProxyBackendReaderError::FirstReadEOF)
                } else {
                    Err(ProxyBackendReaderError::UnexpectedEOF)
                };
            }
        }
    }

    /// Read the next response from the backend.
    ///
    /// `allow_body` should be `false` when no body is expected even if the
    /// response headers indicate one (e.g. replies to HEAD or CONNECT).
    pub async fn read(
        mut self,
        allow_body: bool,
        max_head_length: usize,
    ) -> ProxyBackendReaderResult<ProxyResponseStream<I>> {
        let res = self.head(max_head_length).await?;
        let Self {
            reader,
            parse_slots,
        } = self;

        let framing = res.framing()?;
        let is_informational = res.is_informational();

        if res.code() == 204 && !framing.is_no_framing() {
            return Err(ProxyBackendReaderError::FramingHeadersOn204NoContentResponse);
        }
        if is_informational.is_some() && !framing.is_no_framing() {
            return Err(ProxyBackendReaderError::FramingHeadersOnInformationalResponse);
        }

        if let Some(info_code) = is_informational {
            let next_reader = Self {
                reader,
                parse_slots,
            };
            return match info_code {
                100 | 103 => Ok(ProxyResponseStream::Informational(
                    res,
                    ProxyBackendStreamReader {
                        allow_body,
                        max_head_length,
                        reader: next_reader,
                    },
                )),
                101 => Err(ProxyBackendReaderError::SwitchingProtocolsUnsupported),
                102 => Err(ProxyBackendReaderError::ProcessingUnsupported),
                unk => Err(ProxyBackendReaderError::UnsupportedInformational(unk)),
            };
        }

        let expect_body = allow_body && !matches!(res.code(), 100..=199 | 204 | 304);

        let body_reader = if !expect_body {
            ProxyBackendBodyReader::Bodyless(ProxyBackendBodylessBodyReader {
                inner: BodylessBodyReader::new(reader, parse_slots),
            })
        } else if matches!(framing, ParsedFraming::NoFraming) {
            ProxyBackendBodyReader::Eof(ProxyBackendEofBodyReader {
                inner: EofBodyReader::new(reader, parse_slots),
            })
        } else {
            match framing {
                ParsedFraming::Length(cl) => {
                    ProxyBackendBodyReader::CL(ProxyBackendContentLengthBodyReader {
                        inner: ContentLengthBodyReader::new(cl, reader, parse_slots),
                    })
                }
                ParsedFraming::Chunked => ProxyBackendBodyReader::TE(
                    ProxyBackendChunkedBodyReader::new(reader, parse_slots),
                ),
                ParsedFraming::NoFraming => unreachable!(),
            }
        };

        Ok(ProxyResponseStream::Response(ProxyBackendResponse {
            res,
            body_reader,
        }))
    }
}

pub enum ProxyResponseStream<I> {
    Response(ProxyBackendResponse<I>),
    Informational(Response, ProxyBackendStreamReader<I>),
}

impl<I> ProxyResponseStream<I> {
    pub fn try_into_response(self) -> Result<ProxyBackendResponse<I>, Box<Self>> {
        match self {
            ProxyResponseStream::Response(r) => Ok(r),
            s => Err(Box::new(s)),
        }
    }

    pub fn try_into_informational(
        self,
    ) -> Result<(Response, ProxyBackendStreamReader<I>), Box<Self>> {
        match self {
            ProxyResponseStream::Informational(res, reader) => Ok((res, reader)),
            s => Err(Box::new(s)),
        }
    }
}

pub struct ProxyBackendStreamReader<I> {
    allow_body: bool,
    max_head_length: usize,
    reader: ProxyBackendReader<I>,
}

impl<I: AsyncReadExt + Unpin> ProxyBackendStreamReader<I> {
    pub async fn read(self) -> ProxyBackendReaderResult<ProxyResponseStream<I>> {
        let Self {
            allow_body,
            max_head_length,
            reader,
        } = self;
        reader.read(allow_body, max_head_length).await
    }
}

pub struct ProxyBackendResponse<I> {
    res: Response,
    body_reader: ProxyBackendBodyReader<I>,
}

impl<I> ProxyBackendResponse<I> {
    pub fn res(&self) -> &Response {
        &self.res
    }

    pub fn res_mut(&mut self) -> &mut Response {
        &mut self.res
    }

    pub fn into_parts(self) -> (Response, ProxyBackendBodyReader<I>) {
        let Self { res, body_reader } = self;
        (res, body_reader)
    }
}

pub struct ProxyBackendBodylessBodyReader<I> {
    pub(crate) inner: BodylessBodyReader<I>,
}

impl<I> ProxyBackendBodylessBodyReader<I> {
    pub fn drain(self) -> ProxyBackendReader<I> {
        let (reader, parse_slots) = self.inner.drain();
        ProxyBackendReader {
            reader,
            parse_slots,
        }
    }
}

pub struct ProxyBackendContentLengthBodyReader<I> {
    pub(crate) inner: ContentLengthBodyReader<I>,
}

impl<I> ProxyBackendContentLengthBodyReader<I> {
    pub fn content_length(&self) -> u64 {
        self.inner.content_length()
    }

    /// Recover the [`ProxyBackendReader`] after the body has been fully
    /// drained. Panics if called before draining.
    pub fn finish(self) -> ProxyBackendReader<I> {
        let (reader, parse_slots) = self.inner.finish();
        ProxyBackendReader {
            reader,
            parse_slots,
        }
    }
}

impl<I: AsyncReadExt + Unpin> ProxyBackendContentLengthBodyReader<I> {
    pub async fn read(&mut self, max_len: usize) -> ProxyBackendReaderResult<Option<Bytes>> {
        poll_fn(|cx| self.poll_read(cx, max_len)).await
    }

    pub fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        max_len: usize,
    ) -> Poll<ProxyBackendReaderResult<Option<Bytes>>> {
        Poll::Ready(
            ready!(self.inner.poll_read(cx, max_len))
                .map_err(ProxyBackendReaderError::BodyReadError),
        )
    }

    pub async fn drain(mut self) -> ProxyBackendReaderResult<ProxyBackendReader<I>> {
        poll_fn(|cx| self.poll_drain(cx)).await?;
        Ok(self.finish())
    }

    pub fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<ProxyBackendReaderResult<()>> {
        Poll::Ready(
            ready!(self.inner.poll_drain(cx)).map_err(ProxyBackendReaderError::BodyReadError),
        )
    }
}

pub(crate) enum BackendChunkedState<I> {
    Between(ChunkHeadReader<I>),
    InChunk(ChunkDataReader<I>),
    Done(FinalChunkReader<I>),
}

pub struct ProxyBackendChunkedBodyReader<I> {
    pub(crate) state: Option<BackendChunkedState<I>>,
}

impl<I> ProxyBackendChunkedBodyReader<I> {
    pub(crate) fn new(reader: BytesReader<I>, parse_slots: ParseSlots) -> Self {
        Self {
            state: Some(BackendChunkedState::Between(ChunkHeadReader::new(
                reader,
                parse_slots,
            ))),
        }
    }

    pub fn drained(&self) -> bool {
        matches!(self.state, None | Some(BackendChunkedState::Done(_)))
    }

    /// Recover the [`ProxyBackendReader`] and trailers after the body has been
    /// fully drained. Panics if called before draining.
    pub fn finish(self) -> (ProxyBackendReader<I>, Headers) {
        let Self { state } = self;
        match state {
            Some(BackendChunkedState::Done(r)) => {
                let (reader, parse_slots, trailers) = r.into_parts();
                (
                    ProxyBackendReader {
                        reader,
                        parse_slots,
                    },
                    trailers,
                )
            }
            Some(_) => {
                panic!("attempted to finish the chunked body reader before it was fully drained")
            }
            None => panic!("attempted to finish the chunked body reader after an error"),
        }
    }
}

impl<I: AsyncReadExt + Unpin> ProxyBackendChunkedBodyReader<I> {
    pub async fn read(&mut self, max_len: usize) -> ProxyBackendReaderResult<Option<Bytes>> {
        poll_fn(|cx| self.poll_read(cx, max_len)).await
    }

    pub fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        max_len: usize,
    ) -> Poll<ProxyBackendReaderResult<Option<Bytes>>> {
        loop {
            let Some(current) = self.state.take() else {
                bail!(ProxyBackendReaderError::BodyReadError(
                    BodyError::ReadAfterError,
                ));
            };
            match current {
                BackendChunkedState::InChunk(mut r) => match r.poll_read(cx, max_len) {
                    Poll::Pending => park!(self, BackendChunkedState::InChunk(r)),
                    Poll::Ready(Err(e)) => bail!(ProxyBackendReaderError::BodyReadError(e)),
                    Poll::Ready(Ok(Some(b))) => {
                        self.state = Some(BackendChunkedState::InChunk(r));
                        return Poll::Ready(Ok(Some(b)));
                    }
                    Poll::Ready(Ok(None)) => {
                        self.state = Some(BackendChunkedState::Between(r.finish()));
                    }
                },
                BackendChunkedState::Between(mut r) => match r.poll_read_chunk(cx) {
                    Poll::Pending => park!(self, BackendChunkedState::Between(r)),
                    Poll::Ready(Err(e)) => bail!(ProxyBackendReaderError::BodyReadError(e)),
                    Poll::Ready(Ok(())) => match r.finish() {
                        NextChunk::Data(r) => {
                            self.state = Some(BackendChunkedState::InChunk(r));
                        }
                        NextChunk::Final(r) => {
                            self.state = Some(BackendChunkedState::Done(r));
                        }
                    },
                },
                BackendChunkedState::Done(r) => {
                    self.state = Some(BackendChunkedState::Done(r));
                    return Poll::Ready(Ok(None));
                }
            }
        }
    }

    pub async fn drain(mut self) -> ProxyBackendReaderResult<(ProxyBackendReader<I>, Headers)> {
        poll_fn(|cx| self.poll_drain(cx)).await?;
        Ok(self.finish())
    }

    pub fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<ProxyBackendReaderResult<()>> {
        loop {
            match ready!(self.poll_read(cx, DRAIN_SIZE)) {
                Err(e) => bail!(e),
                Ok(None) => return Poll::Ready(Ok(())),
                Ok(Some(_)) => continue,
            }
        }
    }
}

/// Body reader for close-delimited response bodies (RFC 9112 section 6.3 rule
/// 8). Draining discards the connection; there is no `finish`.
pub struct ProxyBackendEofBodyReader<I> {
    pub(crate) inner: EofBodyReader<I>,
}

impl<I> ProxyBackendEofBodyReader<I> {
    pub fn drained(&self) -> bool {
        self.inner.drained()
    }

    /// Discard the connection and return the parse slots for reuse. The
    /// connection cannot be pipelined after an EOF-framed body.
    pub fn drain(self) -> ParseSlots {
        self.inner.drain()
    }
}

impl<I: AsyncReadExt + Unpin> ProxyBackendEofBodyReader<I> {
    pub async fn read(&mut self, max_len: usize) -> ProxyBackendReaderResult<Option<Bytes>> {
        poll_fn(|cx| self.poll_read(cx, max_len)).await
    }

    pub fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        max_len: usize,
    ) -> Poll<ProxyBackendReaderResult<Option<Bytes>>> {
        Poll::Ready(
            ready!(self.inner.poll_read(cx, max_len))
                .map_err(ProxyBackendReaderError::BodyReadError),
        )
    }
}

pub enum ProxyBackendBodyReader<I> {
    Bodyless(ProxyBackendBodylessBodyReader<I>),
    CL(ProxyBackendContentLengthBodyReader<I>),
    TE(ProxyBackendChunkedBodyReader<I>),
    Eof(ProxyBackendEofBodyReader<I>),
}

impl<I> ProxyBackendBodyReader<I> {
    pub fn drained(&self) -> bool {
        match self {
            ProxyBackendBodyReader::Bodyless(_) => true,
            ProxyBackendBodyReader::CL(r) => r.inner.drained(),
            ProxyBackendBodyReader::TE(r) => r.drained(),
            ProxyBackendBodyReader::Eof(r) => r.inner.drained(),
        }
    }

    /// Recover the [`ProxyBackendReader`] after draining. Returns `None` for
    /// EOF-framed bodies where the connection is consumed.
    pub fn finish(self) -> Option<ProxyBackendReader<I>> {
        match self {
            ProxyBackendBodyReader::Bodyless(r) => Some(r.drain()),
            ProxyBackendBodyReader::CL(r) => Some(r.finish()),
            ProxyBackendBodyReader::TE(r) => {
                let (reader, _trailers) = r.finish();
                Some(reader)
            }
            ProxyBackendBodyReader::Eof(_) => None,
        }
    }
}

impl<I: AsyncReadExt + Unpin> ProxyBackendBodyReader<I> {
    pub async fn read(&mut self, max_len: usize) -> ProxyBackendReaderResult<Option<Bytes>> {
        poll_fn(|cx| self.poll_read(cx, max_len)).await
    }

    pub fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        max_len: usize,
    ) -> Poll<ProxyBackendReaderResult<Option<Bytes>>> {
        match self {
            ProxyBackendBodyReader::Bodyless(_) => Poll::Ready(Ok(None)),
            ProxyBackendBodyReader::CL(r) => r.poll_read(cx, max_len),
            ProxyBackendBodyReader::TE(r) => r.poll_read(cx, max_len),
            ProxyBackendBodyReader::Eof(r) => r.poll_read(cx, max_len),
        }
    }

    /// Drain all remaining body bytes and return the next
    /// [`ProxyBackendReader`], or `None` for EOF-framed bodies.
    pub async fn drain(mut self) -> ProxyBackendReaderResult<Option<ProxyBackendReader<I>>> {
        poll_fn(|cx| self.poll_drain(cx)).await?;
        Ok(self.finish())
    }

    pub fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<ProxyBackendReaderResult<()>> {
        match self {
            ProxyBackendBodyReader::Bodyless(_) => Poll::Ready(Ok(())),
            ProxyBackendBodyReader::CL(r) => r.poll_drain(cx),
            ProxyBackendBodyReader::TE(r) => r.poll_drain(cx),
            ProxyBackendBodyReader::Eof(_) => Poll::Ready(Ok(())),
        }
    }
}
