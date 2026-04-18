use std::task::{Context, Poll};

use bytes::Bytes;
use jrpxy_http_message::{header::Headers, message::ParseSlots};
use jrpxy_util::io_buffer::BytesReader;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    error::{BodyError, BodyResult},
    reader::{ChunkDataReader, ChunkHeadReader, FinalChunkReader, NextChunk},
    writer::chunked::{DataCompleter, DataWriter, FinalWriter, HeadWriter, IdleWriter},
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
                    Poll::Pending => {
                        self.state = Some(C2CState::ReadChunk { reader, writer });
                        return Poll::Pending;
                    }
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
                    Poll::Pending => {
                        self.state = Some(C2CState::WriteChunk { reader, writer });
                        return Poll::Pending;
                    }
                    Poll::Ready(Ok(())) => {
                        let writer = writer.finish();
                        self.state = Some(C2CState::ReadData { reader, writer });
                    }
                },
                C2CState::WriteFinal { reader, mut writer } => match writer.poll_write(cx) {
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => {
                        self.state = Some(C2CState::WriteFinal { reader, writer });
                        return Poll::Pending;
                    }
                    Poll::Ready(Ok(())) => {
                        let reader = reader.into_parts();
                        let writer = writer.finish();
                        self.state = Some(C2CState::Done { reader, writer });
                    }
                },
                C2CState::ReadData { mut reader, writer } => {
                    match reader.poll_read(cx, READ_SIZE) {
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Pending => {
                            self.state = Some(C2CState::ReadData { reader, writer });
                            return Poll::Pending;
                        }
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
                    Poll::Pending => {
                        self.state = Some(C2CState::WriteData {
                            reader,
                            writer,
                            data,
                        });
                        return Poll::Pending;
                    }
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
                    Poll::Pending => {
                        self.state = Some(C2CState::CloseChunk { reader, writer });
                        return Poll::Pending;
                    }
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
