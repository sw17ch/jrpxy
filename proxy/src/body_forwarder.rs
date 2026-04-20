use bytes::{Buf, Bytes};
use jrpxy_backend::error::BackendError;
use jrpxy_body::{
    error::BodyError,
    writer::{
        bodyless::BodylessBodyWriter,
        chunked::{DataCompleter, DataWriter, FinalWriter, HeadWriter, IdleWriter},
        content_length::ContentLengthBodyWriter,
    },
};
use jrpxy_frontend::{
    error::FrontendResult,
    reader::{FrontendBodyReader, FrontendChunkedBodyReader, FrontendContentLengthBodyReader},
};
use jrpxy_http_message::header::Headers;
use std::{
    future::poll_fn,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    backend_writer::{ProxyBackendBodyWriter, ProxyBackendWriter},
    error::ProxyCopyError,
};

macro_rules! park {
    ($self:expr, $state:expr) => {{
        $self.state = Some($state);
        return Poll::Pending;
    }};
}

macro_rules! bail {
    ($e:expr) => {{
        return Poll::Ready(Err($e));
    }};
}

/// Forwards a frontend request body to a backend body writer, driven by
/// polling. Handles all framing combinations:
///
/// - **TE->TE**: body is re-chunked (chunk boundaries are not preserved).
/// - **CL->TE**: content-length body re-framed as chunked.
/// - **TE->CL**: chunked body re-framed as content-length.
/// - **CL->CL**: content-length body forwarded verbatim.
/// - **Bodyless->any**: writer is immediately finished.
pub struct ProxyBodyForwarder<FR, BW> {
    state: Option<State<FR, BW>>,
    chunk_size: usize,
}

impl<FR, BW> ProxyBodyForwarder<FR, BW> {
    /// Create a new forwarder from a frontend body reader and a backend body
    /// writer.
    ///
    /// Returns an error for invalid framing combinations (a body-carrying
    /// reader paired with a bodyless writer).
    pub fn new(
        reader: FrontendBodyReader<FR>,
        body_writer: ProxyBackendBodyWriter<BW>,
        chunk_size: usize,
    ) -> Result<Self, ProxyCopyError> {
        let state = match (reader, body_writer) {
            // Bodyless reader: immediately finish the writer, regardless of
            // which framing the backend expects.
            (FrontendBodyReader::Bodyless(_), ProxyBackendBodyWriter::Bodyless(w)) => {
                State::BodylessFinish(w)
            }
            (FrontendBodyReader::Bodyless(_), ProxyBackendBodyWriter::CL(w)) => State::FlushCL(w),
            (FrontendBodyReader::Bodyless(_), ProxyBackendBodyWriter::TE(idle)) => {
                State::WriteFinal(idle.into_final(&Headers::default()))
            }

            // Body-carrying reader with bodyless writer: the proxy should not
            // create this combination; return an error.
            (FrontendBodyReader::CL(_), ProxyBackendBodyWriter::Bodyless(_))
            | (FrontendBodyReader::TE(_), ProxyBackendBodyWriter::Bodyless(_)) => {
                return Err(backend_write_error(BodyError::BodyOverflow(0)));
            }

            // Any body reader -> TE backend: rechunk whatever bytes arrive.
            (FrontendBodyReader::CL(r), ProxyBackendBodyWriter::TE(idle)) => State::ToChunkRead {
                reader: BodyReader::CL(r),
                writer: idle,
            },
            (FrontendBodyReader::TE(r), ProxyBackendBodyWriter::TE(idle)) => State::ToChunkRead {
                reader: BodyReader::TE(r),
                writer: idle,
            },

            // Any body reader -> CL backend: stream bytes verbatim.
            (FrontendBodyReader::CL(r), ProxyBackendBodyWriter::CL(w)) => State::ToCLRead {
                reader: BodyReader::CL(r),
                writer: w,
            },
            (FrontendBodyReader::TE(r), ProxyBackendBodyWriter::CL(w)) => State::ToCLRead {
                reader: BodyReader::TE(r),
                writer: w,
            },
        };

        Ok(Self {
            state: Some(state),
            chunk_size,
        })
    }
}

impl<FR, BW> ProxyBodyForwarder<FR, BW>
where
    FR: AsyncRead + Unpin,
    BW: AsyncWrite + Unpin,
{
    /// Poll the forwarder, advancing the body copy. Returns `Ready(Ok(()))` when
    /// the body has been fully forwarded; call [`finish`](Self::finish) after
    /// that to recover the backend writer.
    pub fn poll_forward(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), ProxyCopyError>> {
        loop {
            let Some(state) = self.state.take() else {
                return Poll::Ready(Err(ProxyCopyError::BackendWriterGone));
            };

            match state {
                // Read the next buffer; create a same-sized chunk on the backend.
                State::ToChunkRead { mut reader, writer } => {
                    match reader.poll_read(cx, self.chunk_size) {
                        Poll::Pending => park!(self, State::ToChunkRead { reader, writer }),
                        Poll::Ready(Err(e)) => bail!(ProxyCopyError::FrontendError(e)),
                        Poll::Ready(Ok(None)) => {
                            // Body fully read; write the terminal chunk.
                            self.state =
                                Some(State::WriteFinal(writer.into_final(&Headers::default())));
                        }
                        Poll::Ready(Ok(Some(data))) => {
                            let head = writer.start(data.len() as u64);
                            self.state = Some(State::ToChunkWriteHead {
                                reader,
                                writer: head,
                                data,
                            });
                        }
                    }
                }

                // Write the backend chunk header for the current data buffer.
                State::ToChunkWriteHead {
                    reader,
                    mut writer,
                    data,
                } => match writer.poll_write(cx) {
                    Poll::Pending => park!(
                        self,
                        State::ToChunkWriteHead {
                            reader,
                            writer,
                            data
                        }
                    ),
                    Poll::Ready(Err(e)) => bail!(backend_write_error(e)),
                    Poll::Ready(Ok(())) => {
                        self.state = Some(State::ToChunkWriteData {
                            reader,
                            writer: writer.finish(),
                            data,
                        });
                    }
                },

                // Write the data bytes of the current chunk.
                State::ToChunkWriteData {
                    reader,
                    mut writer,
                    mut data,
                } => match writer.poll_write(cx, &data) {
                    Poll::Pending => park!(
                        self,
                        State::ToChunkWriteData {
                            reader,
                            writer,
                            data
                        }
                    ),
                    Poll::Ready(Err(e)) => bail!(backend_write_error(e)),
                    Poll::Ready(Ok(n)) => {
                        data.advance(n);
                        self.state = Some(if data.is_empty() {
                            State::ToChunkCloseChunk {
                                reader,
                                writer: writer.finish(),
                            }
                        } else {
                            State::ToChunkWriteData {
                                reader,
                                writer,
                                data,
                            }
                        });
                    }
                },

                // Write the chunk footer, then loop back to read more data.
                State::ToChunkCloseChunk { reader, mut writer } => match writer.poll_complete(cx) {
                    Poll::Pending => park!(self, State::ToChunkCloseChunk { reader, writer }),
                    Poll::Ready(Err(e)) => bail!(backend_write_error(e)),
                    Poll::Ready(Ok(())) => {
                        self.state = Some(State::ToChunkRead {
                            reader,
                            writer: writer.finish(),
                        });
                    }
                },

                // Read bytes and write them directly to the CL backend writer.
                State::ToCLRead { mut reader, writer } => {
                    match reader.poll_read(cx, self.chunk_size) {
                        Poll::Pending => park!(self, State::ToCLRead { reader, writer }),
                        Poll::Ready(Err(e)) => bail!(ProxyCopyError::FrontendError(e)),
                        Poll::Ready(Ok(None)) => {
                            self.state = Some(State::FlushCL(writer));
                        }
                        Poll::Ready(Ok(Some(data))) => {
                            self.state = Some(State::ToCLWrite {
                                reader,
                                writer,
                                data,
                            });
                        }
                    }
                }

                State::ToCLWrite {
                    reader,
                    mut writer,
                    mut data,
                } => match writer.poll_write(cx, &data) {
                    Poll::Pending => park!(
                        self,
                        State::ToCLWrite {
                            reader,
                            writer,
                            data
                        }
                    ),
                    Poll::Ready(Err(e)) => bail!(backend_write_error(e)),
                    Poll::Ready(Ok(n)) => {
                        data.advance(n);
                        self.state = Some(if data.is_empty() {
                            State::ToCLRead { reader, writer }
                        } else {
                            State::ToCLWrite {
                                reader,
                                writer,
                                data,
                            }
                        });
                    }
                },

                // Write the terminal chunk and flush.
                State::WriteFinal(mut writer) => match writer.poll_write(cx) {
                    Poll::Pending => park!(self, State::WriteFinal(writer)),
                    Poll::Ready(Err(e)) => bail!(backend_write_error(e)),
                    Poll::Ready(Ok(())) => {
                        self.state =
                            Some(State::Finished(ProxyBackendWriter::new(writer.finish())));
                    }
                },

                // Flush the CL writer and extract the inner writer.
                State::FlushCL(mut writer) => match writer.poll_flush(cx) {
                    Poll::Pending => park!(self, State::FlushCL(writer)),
                    Poll::Ready(Err(e)) => bail!(backend_write_error(e)),
                    Poll::Ready(Ok(())) => {
                        let w = writer.into_writer().map_err(backend_write_error)?;
                        self.state = Some(State::Finished(ProxyBackendWriter::new(w)));
                    }
                },

                // Bodyless writer: no framing to write, just unwrap.
                State::BodylessFinish(writer) => {
                    self.state = Some(State::Finished(ProxyBackendWriter::new(writer.finish())));
                }

                State::Finished(_) => {
                    self.state = Some(state);
                    return Poll::Ready(Ok(()));
                }
            }
        }
    }

    /// Recover the backend writer after [`poll_forward`](Self::poll_forward)
    /// has returned `Ready(Ok(()))`. Panics if called before forwarding
    /// completes.
    pub fn finish(self) -> ProxyBackendWriter<BW> {
        match self.state {
            Some(State::Finished(w)) => w,
            _ => panic!("finish() called before poll_forward completed"),
        }
    }

    /// Drive the forwarder to completion, returning the recovered backend
    /// writer when done.
    pub async fn forward(mut self) -> Result<ProxyBackendWriter<BW>, ProxyCopyError> {
        poll_fn(|cx| self.poll_forward(cx)).await?;
        Ok(self.finish())
    }
}

enum BodyReader<FR> {
    CL(FrontendContentLengthBodyReader<FR>),
    TE(FrontendChunkedBodyReader<FR>),
}

impl<FR: AsyncRead + Unpin> BodyReader<FR> {
    fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        max_len: usize,
    ) -> Poll<FrontendResult<Option<Bytes>>> {
        match self {
            BodyReader::CL(r) => r.poll_read(cx, max_len),
            BodyReader::TE(r) => r.poll_read(cx, max_len),
        }
    }
}

enum State<FR, BW> {
    // Any reader -> TE backend (re-chunks)
    /// Read a buffer from the frontend; size it to a new backend chunk.
    ToChunkRead {
        reader: BodyReader<FR>,
        writer: IdleWriter<BW>,
    },
    /// Write the backend chunk header for the pending data buffer.
    ToChunkWriteHead {
        reader: BodyReader<FR>,
        writer: HeadWriter<BW>,
        data: Bytes,
    },
    /// Write the data bytes of the current chunk.
    ToChunkWriteData {
        reader: BodyReader<FR>,
        writer: DataWriter<BW>,
        data: Bytes,
    },
    /// Write the chunk footer, then loop back to `ToChunkRead`.
    ToChunkCloseChunk {
        reader: BodyReader<FR>,
        writer: DataCompleter<BW>,
    },

    // Any reader -> CL backend
    /// Read bytes from the frontend body.
    ToCLRead {
        reader: BodyReader<FR>,
        writer: ContentLengthBodyWriter<BW>,
    },
    /// Write a pending buffer to the CL backend writer.
    ToCLWrite {
        reader: BodyReader<FR>,
        writer: ContentLengthBodyWriter<BW>,
        data: Bytes,
    },

    // Shared terminal states
    /// Write the terminal chunk (`0\r\n[trailers]\r\n`) for a TE backend.
    WriteFinal(FinalWriter<BW>),
    /// Flush a CL backend writer before extracting the inner writer.
    FlushCL(ContentLengthBodyWriter<BW>),
    /// Finish a bodyless backend writer (no I/O needed).
    BodylessFinish(BodylessBodyWriter<BW>),
    /// Forwarding complete; holds the recovered backend writer until
    /// [`ProxyBodyForwarder::finish`] is called.
    Finished(ProxyBackendWriter<BW>),
}

fn backend_write_error(e: BodyError) -> ProxyCopyError {
    ProxyCopyError::BackendError(BackendError::BodyWriteError(e))
}
