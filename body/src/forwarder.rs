use std::{
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use jrpxy_http_message::{header::Headers, message::ParseSlots};
use jrpxy_util::io_buffer::BytesReader;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    error::{BodyError, BodyResult},
    reader::{
        ChunkDataReader, ChunkHeadReader, ContentLengthBodyReader, EofBodyReader, FinalChunkReader,
        NextChunk,
    },
    writer::{
        chunked::{DataCompleter, DataWriter, FinalWriter, HeadWriter, IdleWriter},
        content_length::ContentLengthBodyWriter,
    },
};

// Assume we can glob length and eof into the same operations, we need the
// following writers. ChunkToChunkBodyForwarder lets us preserve what we can
// from chunk encoding.
//
// We will also make OtherToChunkedBodyForwarder and
// ChunkedToOtherBodyForwarder.

pub enum C2CState<R, W> {
    /// On final chunk, transitions to `WriteFinal`. On normal chunk,
    /// transitions to `WriteChunk`.
    ReadChunk {
        reader: ChunkHeadReader<R>,
        writer: IdleWriter<W>,
    },
    // Once the chunk header has been written, transition to `ReadData`.
    WriteChunk {
        reader: ChunkDataReader<R>,
        writer: HeadWriter<W>,
    },
    // Once the final chunk has been written, transition to `Done`.
    WriteFinal {
        reader: FinalChunkReader<R>,
        writer: FinalWriter<W>,
    },
    // When more data is read, transition to `WriteData`. If no more data is read,
    // transition to `CloseChunk`. Data may arrive in multiple reads.
    ReadData {
        reader: ChunkDataReader<R>,
        writer: DataWriter<W>,
    },
    // When there is no more data to write, transition to `ReadData`.
    WriteData {
        reader: ChunkDataReader<R>,
        writer: DataWriter<W>,
        data: Bytes,
    },
    // After the chunk is closed, transition to `ReadChunk`.
    CloseChunk {
        reader: ChunkDataReader<R>,
        writer: DataCompleter<W>,
    },
    // When we reach the terminal `Done` state, we no longer transition.
    Done {
        reader: (BytesReader<R>, ParseSlots, Headers),
        writer: W,
    },
}

const READ_SIZE: usize = 8192;

/// Store `$state` and return `Poll::Pending` atomically, preventing the
/// common mistake of storing state without returning.
macro_rules! park {
    ($self:expr, $state:expr) => {{
        $self.state = Some($state);
        return Poll::Pending;
    }};
}

pub struct ChunkToChunkBodyForwarder<R, W> {
    state: Option<C2CState<R, W>>,
}

impl<R, W> ChunkToChunkBodyForwarder<R, W> {
    pub fn new(reader: ChunkHeadReader<R>, writer: IdleWriter<W>) -> Self {
        Self {
            state: Some(C2CState::ReadChunk { reader, writer }),
        }
    }
}

impl<R, W> ChunkToChunkBodyForwarder<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub fn poll_forward(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        loop {
            let Some(state) = self.state.take() else {
                return Poll::Ready(Err(BodyError::ForwardAfterError));
            };
            match state {
                C2CState::ReadChunk { mut reader, writer } => match reader.poll_read_chunk(cx) {
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => park!(self, C2CState::ReadChunk { reader, writer }),
                    Poll::Ready(Ok(())) => match reader.finish() {
                        NextChunk::Data(reader) => {
                            let writer = writer.start(reader.size());
                            self.state = Some(C2CState::WriteChunk { reader, writer })
                        }
                        NextChunk::Final(reader) => {
                            let writer = writer.into_final(reader.trailers());
                            self.state = Some(C2CState::WriteFinal { reader, writer })
                        }
                    },
                },
                C2CState::WriteChunk { reader, mut writer } => match writer.poll_write(cx) {
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => park!(self, C2CState::WriteChunk { reader, writer }),
                    Poll::Ready(Ok(())) => {
                        let writer = writer.finish();
                        self.state = Some(C2CState::ReadData { reader, writer });
                    }
                },
                C2CState::WriteFinal { reader, mut writer } => match writer.poll_write(cx) {
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => park!(self, C2CState::WriteFinal { reader, writer }),
                    Poll::Ready(Ok(())) => {
                        let reader = reader.into_parts();
                        let writer = writer.finish();
                        self.state = Some(C2CState::Done { reader, writer });
                    }
                },
                C2CState::ReadData { mut reader, writer } => {
                    match reader.poll_read(cx, READ_SIZE) {
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Pending => park!(self, C2CState::ReadData { reader, writer }),
                        Poll::Ready(Ok(None)) => {
                            let writer = writer.finish();
                            self.state = Some(C2CState::CloseChunk { reader, writer });
                        }
                        Poll::Ready(Ok(Some(data))) => {
                            self.state = Some(C2CState::WriteData {
                                reader,
                                writer,
                                data,
                            });
                        }
                    }
                }
                C2CState::WriteData {
                    reader,
                    mut writer,
                    mut data,
                } => match writer.poll_write(cx, &data) {
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => park!(
                        self,
                        C2CState::WriteData {
                            reader,
                            writer,
                            data
                        }
                    ),
                    Poll::Ready(Ok(len)) => {
                        let _written = data.split_to(len);
                        self.state = if !data.is_empty() {
                            Some(C2CState::WriteData {
                                reader,
                                writer,
                                data,
                            })
                        } else {
                            Some(C2CState::ReadData { reader, writer })
                        };
                    }
                },
                C2CState::CloseChunk { reader, mut writer } => match writer.poll_complete(cx) {
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => park!(self, C2CState::CloseChunk { reader, writer }),
                    Poll::Ready(Ok(())) => {
                        let reader = reader.finish();
                        let writer = writer.finish();
                        self.state = Some(C2CState::ReadChunk { reader, writer })
                    }
                },
                C2CState::Done { .. } => return Poll::Ready(Ok(())),
            }
        }
    }
}

// i could build a ChunkForwarder. Would that be nuts?

/// A writer for a body that is not chunk-encoded: either content-length
/// framed, or EOF-framed (a raw `W`).
pub enum OtherBodyWriter<W> {
    ContentLength(ContentLengthBodyWriter<W>),
    /// The inner writer is the body stream; callers signal end-of-body by
    /// finishing the forwarder rather than by writing any framing byte.
    Eof(W),
}

impl<W: AsyncWrite + Unpin> OtherBodyWriter<W> {
    fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<BodyResult<usize>> {
        match self {
            OtherBodyWriter::ContentLength(w) => w.poll_write(cx, buf),
            OtherBodyWriter::Eof(w) => match Pin::new(w).poll_write(cx, buf) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Ok(n)) => Poll::Ready(Ok(n)),
                Poll::Ready(Err(e)) => Poll::Ready(Err(BodyError::BodyWriteError(e))),
            },
        }
    }

    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        match self {
            OtherBodyWriter::ContentLength(w) => w.poll_flush(cx),
            OtherBodyWriter::Eof(w) => match Pin::new(w).poll_flush(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
                Poll::Ready(Err(e)) => Poll::Ready(Err(BodyError::BodyWriteError(e))),
            },
        }
    }

    fn into_writer(self) -> BodyResult<Option<W>> {
        match self {
            OtherBodyWriter::ContentLength(w) => w.into_writer().map(Some),
            // Do not allow extracting out the EOF writer. We drop it to
            // indicate that things are finished. There's no way to indicate
            // that something has gone wrong. Use non-EOF writers and readers if
            // you want to avoid this ambiguous error condition.
            OtherBodyWriter::Eof(_w) => Ok(None),
        }
    }
}

pub enum C2OState<R, W> {
    /// Reading the next chunk header. On a data chunk, transitions to
    /// `ReadData`. On the final chunk, transitions to `FlushWriter`.
    ReadChunk {
        reader: ChunkHeadReader<R>,
        writer: OtherBodyWriter<W>,
    },
    /// Reading chunk body bytes. On data, transitions to `WriteData`. When
    /// exhausted, transitions back to `ReadChunk`.
    ReadData {
        reader: ChunkDataReader<R>,
        writer: OtherBodyWriter<W>,
    },
    /// Writing chunk body bytes to the output. When the buffer is empty,
    /// transitions back to `ReadData`.
    WriteData {
        reader: ChunkDataReader<R>,
        writer: OtherBodyWriter<W>,
        data: Bytes,
    },
    /// All chunk data has been forwarded. Flush the writer, then validate
    /// completeness (relevant for content-length) and transition to `Done`.
    FlushWriter {
        reader: FinalChunkReader<R>,
        writer: OtherBodyWriter<W>,
    },
    Done {
        reader: (BytesReader<R>, ParseSlots, Headers),
        writer: Option<W>,
    },
}

pub struct ChunkToOtherBodyForwarder<R, W> {
    state: Option<C2OState<R, W>>,
}

impl<R, W> ChunkToOtherBodyForwarder<R, W> {
    pub fn new(reader: ChunkHeadReader<R>, writer: OtherBodyWriter<W>) -> Self {
        Self {
            state: Some(C2OState::ReadChunk { reader, writer }),
        }
    }
}

impl<R, W> ChunkToOtherBodyForwarder<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub fn poll_forward(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        loop {
            let Some(state) = self.state.take() else {
                return Poll::Ready(Err(BodyError::ForwardAfterError));
            };
            match state {
                C2OState::ReadChunk { mut reader, writer } => match reader.poll_read_chunk(cx) {
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => park!(self, C2OState::ReadChunk { reader, writer }),
                    Poll::Ready(Ok(())) => match reader.finish() {
                        NextChunk::Data(reader) => {
                            self.state = Some(C2OState::ReadData { reader, writer });
                        }
                        NextChunk::Final(reader) => {
                            self.state = Some(C2OState::FlushWriter { reader, writer });
                        }
                    },
                },
                C2OState::ReadData { mut reader, writer } => {
                    match reader.poll_read(cx, READ_SIZE) {
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Pending => park!(self, C2OState::ReadData { reader, writer }),
                        Poll::Ready(Ok(None)) => {
                            let reader = reader.finish();
                            self.state = Some(C2OState::ReadChunk { reader, writer });
                        }
                        Poll::Ready(Ok(Some(data))) => {
                            self.state = Some(C2OState::WriteData {
                                reader,
                                writer,
                                data,
                            });
                        }
                    }
                }
                C2OState::WriteData {
                    reader,
                    mut writer,
                    mut data,
                } => match writer.poll_write(cx, &data) {
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => park!(
                        self,
                        C2OState::WriteData {
                            reader,
                            writer,
                            data
                        }
                    ),
                    Poll::Ready(Ok(len)) => {
                        let _written = data.split_to(len);
                        self.state = if !data.is_empty() {
                            Some(C2OState::WriteData {
                                reader,
                                writer,
                                data,
                            })
                        } else {
                            Some(C2OState::ReadData { reader, writer })
                        };
                    }
                },
                C2OState::FlushWriter { reader, mut writer } => match writer.poll_flush(cx) {
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => park!(self, C2OState::FlushWriter { reader, writer }),
                    Poll::Ready(Ok(())) => {
                        let writer = match writer.into_writer() {
                            Ok(w) => w,
                            Err(e) => return Poll::Ready(Err(e)),
                        };
                        let reader = reader.into_parts();
                        self.state = Some(C2OState::Done { reader, writer });
                    }
                },
                C2OState::Done { .. } => return Poll::Ready(Ok(())),
            }
        }
    }
}

/// A reader for a body that is not chunk-encoded: either content-length
/// framed, or EOF-framed.
pub enum OtherBodyReader<R> {
    ContentLength(ContentLengthBodyReader<R>),
    /// The body ends when the underlying connection is closed.
    Eof(EofBodyReader<R>),
}

impl<R: AsyncRead + Unpin> OtherBodyReader<R> {
    fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        max_len: usize,
    ) -> Poll<BodyResult<Option<Bytes>>> {
        match self {
            OtherBodyReader::ContentLength(r) => r.poll_read(cx, max_len),
            OtherBodyReader::Eof(r) => r.poll_read(cx, max_len),
        }
    }

    /// Consume the reader after `poll_read` has returned `Ok(None)`.
    /// Returns `(parse_slots, Some(reader))` for content-length (IO is still
    /// viable for pipelining) and `(parse_slots, None)` for EOF (connection
    /// is consumed).
    fn into_done(self) -> (ParseSlots, Option<BytesReader<R>>) {
        match self {
            OtherBodyReader::ContentLength(r) => {
                let (reader, parse_slots) = r.finish();
                (parse_slots, Some(reader))
            }
            OtherBodyReader::Eof(r) => (r.drain(), None),
        }
    }
}

pub enum O2CState<R, W> {
    /// Reading body bytes from the source. On data, transitions to
    /// `WriteHead`. On EOF/end, transitions to `WriteFinal`.
    ReadData {
        reader: OtherBodyReader<R>,
        writer: IdleWriter<W>,
    },
    /// Writing the chunk header for the next data chunk. Once complete,
    /// transitions to `WriteData`.
    WriteHead {
        reader: OtherBodyReader<R>,
        writer: HeadWriter<W>,
        data: Bytes,
    },
    /// Writing the chunk body bytes. When the buffer is empty, transitions to
    /// `CloseChunk`.
    WriteData {
        reader: OtherBodyReader<R>,
        writer: DataWriter<W>,
        data: Bytes,
    },
    /// Writing the chunk footer (`\r\n`). Once complete, transitions back to
    /// `ReadData` for the next chunk.
    CloseChunk {
        reader: OtherBodyReader<R>,
        writer: DataCompleter<W>,
    },
    /// Writing the terminal chunk (`0\r\n\r\n`) and flushing. Once complete,
    /// transitions to `Done`.
    WriteFinal {
        reader: OtherBodyReader<R>,
        writer: FinalWriter<W>,
    },
    Done {
        reader: (ParseSlots, Option<BytesReader<R>>),
        writer: W,
    },
}

pub struct OtherToChunkedBodyForwarder<R, W> {
    state: Option<O2CState<R, W>>,
}

impl<R, W> OtherToChunkedBodyForwarder<R, W> {
    pub fn new(reader: OtherBodyReader<R>, writer: IdleWriter<W>) -> Self {
        Self {
            state: Some(O2CState::ReadData { reader, writer }),
        }
    }
}

impl<R, W> OtherToChunkedBodyForwarder<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub fn poll_forward(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        loop {
            let Some(state) = self.state.take() else {
                return Poll::Ready(Err(BodyError::ForwardAfterError));
            };
            match state {
                O2CState::ReadData { mut reader, writer } => {
                    match reader.poll_read(cx, READ_SIZE) {
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Pending => park!(self, O2CState::ReadData { reader, writer }),
                        Poll::Ready(Ok(None)) => {
                            let writer = writer.into_final(&Default::default());
                            self.state = Some(O2CState::WriteFinal { reader, writer });
                        }
                        Poll::Ready(Ok(Some(data))) => {
                            let writer = writer.start(data.len() as u64);
                            self.state = Some(O2CState::WriteHead {
                                reader,
                                writer,
                                data,
                            });
                        }
                    }
                }
                O2CState::WriteHead {
                    reader,
                    mut writer,
                    data,
                } => match writer.poll_write(cx) {
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => park!(
                        self,
                        O2CState::WriteHead {
                            reader,
                            writer,
                            data
                        }
                    ),
                    Poll::Ready(Ok(())) => {
                        let writer = writer.finish();
                        self.state = Some(O2CState::WriteData {
                            reader,
                            writer,
                            data,
                        });
                    }
                },
                O2CState::WriteData {
                    reader,
                    mut writer,
                    mut data,
                } => match writer.poll_write(cx, &data) {
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => park!(
                        self,
                        O2CState::WriteData {
                            reader,
                            writer,
                            data
                        }
                    ),
                    Poll::Ready(Ok(len)) => {
                        let _written = data.split_to(len);
                        self.state = if !data.is_empty() {
                            Some(O2CState::WriteData {
                                reader,
                                writer,
                                data,
                            })
                        } else {
                            let writer = writer.finish();
                            Some(O2CState::CloseChunk { reader, writer })
                        };
                    }
                },
                O2CState::CloseChunk { reader, mut writer } => match writer.poll_complete(cx) {
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => park!(self, O2CState::CloseChunk { reader, writer }),
                    Poll::Ready(Ok(())) => {
                        let writer = writer.finish();
                        self.state = Some(O2CState::ReadData { reader, writer });
                    }
                },
                O2CState::WriteFinal { reader, mut writer } => match writer.poll_write(cx) {
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => park!(self, O2CState::WriteFinal { reader, writer }),
                    Poll::Ready(Ok(())) => {
                        let writer = writer.finish();
                        let reader = reader.into_done();
                        self.state = Some(O2CState::Done { reader, writer });
                    }
                },
                O2CState::Done { .. } => return Poll::Ready(Ok(())),
            }
        }
    }
}

pub enum O2OState<R, W> {
    /// Reading body bytes from the source. On data, transitions to
    /// `WriteData`. On EOF/end, transitions to `FlushWriter`.
    ReadData {
        reader: OtherBodyReader<R>,
        writer: OtherBodyWriter<W>,
    },
    /// Writing body bytes to the destination. When the buffer is empty,
    /// transitions back to `ReadData`.
    WriteData {
        reader: OtherBodyReader<R>,
        writer: OtherBodyWriter<W>,
        data: Bytes,
    },
    /// All body bytes have been forwarded. Flush and validate the writer
    /// (relevant for content-length), then transition to `Done`.
    FlushWriter {
        reader: OtherBodyReader<R>,
        writer: OtherBodyWriter<W>,
    },
    Done {
        reader: (ParseSlots, Option<BytesReader<R>>),
        writer: Option<W>,
    },
}

pub struct OtherToOtherBodyForwarder<R, W> {
    state: Option<O2OState<R, W>>,
}

impl<R, W> OtherToOtherBodyForwarder<R, W> {
    pub fn new(reader: OtherBodyReader<R>, writer: OtherBodyWriter<W>) -> Self {
        Self {
            state: Some(O2OState::ReadData { reader, writer }),
        }
    }
}

impl<R, W> OtherToOtherBodyForwarder<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub fn poll_forward(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        loop {
            let Some(state) = self.state.take() else {
                return Poll::Ready(Err(BodyError::ForwardAfterError));
            };
            match state {
                O2OState::ReadData { mut reader, writer } => {
                    match reader.poll_read(cx, READ_SIZE) {
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Pending => park!(self, O2OState::ReadData { reader, writer }),
                        Poll::Ready(Ok(None)) => {
                            self.state = Some(O2OState::FlushWriter { reader, writer });
                        }
                        Poll::Ready(Ok(Some(data))) => {
                            self.state = Some(O2OState::WriteData {
                                reader,
                                writer,
                                data,
                            });
                        }
                    }
                }
                O2OState::WriteData {
                    reader,
                    mut writer,
                    mut data,
                } => match writer.poll_write(cx, &data) {
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => park!(
                        self,
                        O2OState::WriteData {
                            reader,
                            writer,
                            data
                        }
                    ),
                    Poll::Ready(Ok(len)) => {
                        let _written = data.split_to(len);
                        self.state = if !data.is_empty() {
                            Some(O2OState::WriteData {
                                reader,
                                writer,
                                data,
                            })
                        } else {
                            Some(O2OState::ReadData { reader, writer })
                        };
                    }
                },
                O2OState::FlushWriter { reader, mut writer } => match writer.poll_flush(cx) {
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => park!(self, O2OState::FlushWriter { reader, writer }),
                    Poll::Ready(Ok(())) => {
                        let writer = match writer.into_writer() {
                            Ok(w) => w,
                            Err(e) => return Poll::Ready(Err(e)),
                        };
                        let reader = reader.into_done();
                        self.state = Some(O2OState::Done { reader, writer });
                    }
                },
                O2OState::Done { .. } => return Poll::Ready(Ok(())),
            }
        }
    }
}

pub enum BodyForwarder<R, W> {
    C2C(ChunkToChunkBodyForwarder<R, W>),
    C2O(ChunkToOtherBodyForwarder<R, W>),
    O2C(OtherToChunkedBodyForwarder<R, W>),
    O2O(OtherToOtherBodyForwarder<R, W>),
}

impl<R, W> From<ChunkToChunkBodyForwarder<R, W>> for BodyForwarder<R, W> {
    fn from(f: ChunkToChunkBodyForwarder<R, W>) -> Self {
        BodyForwarder::C2C(f)
    }
}

impl<R, W> From<ChunkToOtherBodyForwarder<R, W>> for BodyForwarder<R, W> {
    fn from(f: ChunkToOtherBodyForwarder<R, W>) -> Self {
        BodyForwarder::C2O(f)
    }
}

impl<R, W> From<OtherToChunkedBodyForwarder<R, W>> for BodyForwarder<R, W> {
    fn from(f: OtherToChunkedBodyForwarder<R, W>) -> Self {
        BodyForwarder::O2C(f)
    }
}

impl<R, W> From<OtherToOtherBodyForwarder<R, W>> for BodyForwarder<R, W> {
    fn from(f: OtherToOtherBodyForwarder<R, W>) -> Self {
        BodyForwarder::O2O(f)
    }
}

impl<R, W> BodyForwarder<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub fn poll_forward(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        match self {
            BodyForwarder::C2C(f) => f.poll_forward(cx),
            BodyForwarder::C2O(f) => f.poll_forward(cx),
            BodyForwarder::O2C(f) => f.poll_forward(cx),
            BodyForwarder::O2O(f) => f.poll_forward(cx),
        }
    }
}
