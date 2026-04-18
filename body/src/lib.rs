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
    reader::{ChunkDataReader, ChunkHeadReader, FinalChunkReader, NextChunk},
    writer::{
        chunked::{DataCompleter, DataWriter, FinalWriter, HeadWriter, IdleWriter},
        content_length::ContentLengthBodyWriter,
    },
};

pub mod error;
pub mod reader;
pub mod writer;

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

    fn into_writer(self) -> BodyResult<W> {
        match self {
            OtherBodyWriter::ContentLength(w) => w.into_writer(),
            OtherBodyWriter::Eof(w) => Ok(w),
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
        writer: W,
    },
}

pub struct ChunkToOtherBodyForwarder<R, W> {
    state: Option<C2OState<R, W>>,
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
