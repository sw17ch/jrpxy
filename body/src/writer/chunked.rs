use std::{
    future::poll_fn,
    pin::Pin,
    task::{Context, Poll, ready},
};

use jrpxy_http_message::header::Headers;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::error::{BodyError, BodyResult};

#[derive(Debug)]
pub struct ChunkedBodyWriter<I> {
    writer: I,
}

impl<I> ChunkedBodyWriter<I> {
    pub fn new(writer: I) -> Self {
        Self { writer }
    }
}

impl<I: AsyncWrite + Unpin> ChunkedBodyWriter<I> {
    // TODO: add a flush method so that we can force out writes

    pub async fn write(&mut self, buffer: &[u8]) -> BodyResult<()> {
        if buffer.is_empty() {
            // if there's nothing in the buffer, don't do anything else.
            // attempting to write a zero-length chunk will break framing.
            return Ok(());
        }
        let mut w = ManuallyChunkedBodyWriter::new(&mut self.writer, buffer);
        poll_fn(|cx| w.poll_write(cx)).await
    }

    pub async fn finish_with_trailers(self, trailers: &Headers) -> BodyResult<I> {
        let mut f = ManuallyChunkedBodyFinalizer::new(trailers, self.writer);
        poll_fn(|cx| f.poll_final(cx)).await?;
        Ok(f.finish())
    }

    pub async fn finish(self) -> BodyResult<I> {
        self.finish_with_trailers(&Default::default()).await
    }

    pub async fn abort(self) -> BodyResult<I> {
        let Self { mut writer } = self;
        // we allow users to explicitly abandon a transfer-encoding:chunked
        // write by emitting a bad chunk header. this should be a little more
        // durable when badly-configured body readers don't wait for the
        // terminating empty chunk to consider a body complete.
        //
        // we write an 'x' because it is not a valid hexadecimal character
        writer
            .write_all(b"x")
            .await
            .map_err(BodyError::BodyWriteError)?;
        writer.flush().await.map_err(BodyError::BodyWriteError)?;
        Ok(writer)
    }
}

pub struct ManuallyChunkedBodyWriter<'b, I> {
    buffer: &'b [u8],
    mode: Option<BodyWriterMode<I>>,
}

impl<'b, I> ManuallyChunkedBodyWriter<'b, I> {
    pub fn finish(mut self, trailers: &Headers) -> ManuallyChunkedBodyFinalizer<I> {
        let Some(mode) = self.mode.take() else {
            panic!("attempted to finish after error");
        };
        let BodyWriterMode::Idle(writer) = mode else {
            panic!("attempted to finish without polling poll_write to completion");
        };
        let IdleChunkWriter { writer } = writer;

        ManuallyChunkedBodyFinalizer {
            mode: Some(BodyFinalizerMode::Final(FinalChunkWriter::new(
                trailers, writer,
            ))),
        }
    }
}

impl<'b, I> ManuallyChunkedBodyWriter<'b, I>
where
    I: AsyncWrite + Unpin,
{
    pub fn new(writer: I, buffer: &'b [u8]) -> Self {
        Self {
            buffer,
            mode: Some(BodyWriterMode::Idle(IdleChunkWriter { writer })),
        }
    }

    pub fn poll_write(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        loop {
            let Some(mode) = self.mode.take() else {
                return Poll::Ready(Err(BodyError::WriteAfterError));
            };
            match mode {
                BodyWriterMode::Idle(w) => {
                    if self.buffer.is_empty() {
                        // if the buffer we were given is empty, don't try to
                        // write it out. the user will have to call finish to
                        // close out the body.
                        self.mode = Some(BodyWriterMode::Idle(w));
                        return Poll::Ready(Ok(()));
                    } else {
                        self.mode = Some(BodyWriterMode::Head(w.start(self.buffer.len() as u64)));
                    }
                }
                BodyWriterMode::Head(mut w) => match w.poll_write(cx) {
                    Poll::Pending => {
                        self.mode = Some(BodyWriterMode::Head(w));
                        return Poll::Pending;
                    }
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Ready(Ok(())) => {
                        self.mode = Some(BodyWriterMode::Data(w.finish()));
                    }
                },
                BodyWriterMode::Data(mut w) => {
                    if !self.buffer.is_empty() {
                        match w.poll_write(cx, self.buffer) {
                            Poll::Pending => {
                                self.mode = Some(BodyWriterMode::Data(w));
                                return Poll::Pending;
                            }
                            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                            Poll::Ready(Ok(len)) => {
                                let rest = &self.buffer[len..];
                                self.buffer = rest;
                                self.mode = Some(BodyWriterMode::Data(w));
                            }
                        }
                    } else {
                        match w.poll_complete(cx) {
                            Poll::Pending => {
                                self.mode = Some(BodyWriterMode::Data(w));
                                return Poll::Pending;
                            }
                            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                            Poll::Ready(Ok(())) => {
                                self.mode = Some(BodyWriterMode::Idle(w.finish()));
                                return Poll::Ready(Ok(()));
                            }
                        }
                    }
                }
            }
            debug_assert!(self.mode.is_some(), "failed to replace mode");
        }
    }
}

pub struct ManuallyChunkedBodyFinalizer<I> {
    mode: Option<BodyFinalizerMode<I>>,
}

impl<I> ManuallyChunkedBodyFinalizer<I> {
    pub(super) fn new(trailers: &Headers, writer: I) -> Self {
        Self {
            mode: Some(BodyFinalizerMode::Final(FinalChunkWriter::new(
                trailers, writer,
            ))),
        }
    }

    pub fn finish(mut self) -> I {
        let Some(mode) = self.mode.take() else {
            panic!("attempted to finish after an error");
        };
        let BodyFinalizerMode::Done(w) = mode else {
            panic!("attempted to finish without polling poll_final to completion");
        };
        w
    }
}

impl<I> ManuallyChunkedBodyFinalizer<I>
where
    I: AsyncWrite + Unpin,
{
    pub fn poll_final(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        loop {
            let Some(mode) = self.mode.take() else {
                return Poll::Ready(Err(BodyError::WriteAfterError));
            };
            match mode {
                BodyFinalizerMode::Final(mut w) => match w.poll_write(cx) {
                    Poll::Pending => {
                        self.mode = Some(BodyFinalizerMode::Final(w));
                        return Poll::Pending;
                    }
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Ready(Ok(())) => {
                        self.mode = Some(BodyFinalizerMode::Done(w.finish()));
                    }
                },
                BodyFinalizerMode::Done(w) => {
                    self.mode = Some(BodyFinalizerMode::Done(w));
                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}

#[derive(Debug)]
enum BodyWriterMode<I> {
    Idle(IdleChunkWriter<I>),
    Head(ChunkHeadWriter<I>),
    Data(ChunkDataWriter<I>),
}

#[derive(Debug)]
enum BodyFinalizerMode<I> {
    Final(FinalChunkWriter<I>),
    Done(I),
}

#[derive(Debug)]
pub struct IdleChunkWriter<I> {
    writer: I,
}

impl<I> IdleChunkWriter<I> {
    pub fn start(self, chunk_length: u64) -> ChunkHeadWriter<I> {
        let Self { writer } = self;
        ChunkHeadWriter::new(chunk_length, writer)
    }
}

/// Maximum length of a chunk header: 16 hex digits + `\r\n`.
const MAX_CHUNK_HEAD_LEN: usize = 18;

/// Writes the header line for a single chunk (`{size:x}\r\n`). Once fully
/// written, call [`finish`](ChunkHeadWriter::finish) to obtain a
/// [`ChunkDataWriter`] for the chunk body.
#[derive(Debug)]
pub struct ChunkHeadWriter<I> {
    // TODO: avoid the heap allocation from format! by implementing a
    // no-alloc hex formatter.
    /// Pre-formatted header bytes, e.g. `"1a\r\n"`.
    header: [u8; MAX_CHUNK_HEAD_LEN],
    /// Number of valid bytes in `header`.
    header_len: u8,
    /// Number of header bytes already written.
    written: u8,
    /// The chunk body length, forwarded to [`ChunkDataWriter`] on finish.
    chunk_length: u64,
    writer: I,
}

impl<I> ChunkHeadWriter<I> {
    fn new(chunk_length: u64, writer: I) -> Self {
        use std::io::Write;

        let mut header = [0u8; MAX_CHUNK_HEAD_LEN];
        let mut cursor = std::io::Cursor::new(&mut header[..]);
        let r = write!(cursor, "{chunk_length:x}\r\n");
        debug_assert!(r.is_ok());
        let header_len = cursor.position() as u8;

        Self {
            header,
            header_len,
            written: 0,
            chunk_length,
            writer,
        }
    }

    /// Finish the head writer and return a [`ChunkDataWriter`] for the body.
    ///
    /// # Panics
    ///
    /// Panics if the chunk header has not been fully written via
    /// [`Self::poll_write`].
    pub fn finish(self) -> ChunkDataWriter<I> {
        assert_eq!(
            self.written, self.header_len,
            "attempted to finish chunk head writer before header was fully written"
        );
        ChunkDataWriter::new(self.chunk_length, self.writer)
    }
}

impl<I: AsyncWrite + Unpin> ChunkHeadWriter<I> {
    /// Write the entire chunk header, returning a [`ChunkDataWriter`] on
    /// success.
    pub async fn write(mut self) -> BodyResult<ChunkDataWriter<I>> {
        poll_fn(|cx| self.poll_write(cx)).await?;
        Ok(self.finish())
    }

    pub fn poll_write(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        while self.written < self.header_len {
            let buf = &self.header[self.written as usize..self.header_len as usize];
            match ready!(Pin::new(&mut self.writer).poll_write(cx, buf)) {
                Ok(0) => {
                    return Poll::Ready(Err(BodyError::BodyWriteError(
                        std::io::ErrorKind::WriteZero.into(),
                    )));
                }
                Ok(w) => self.written += w as u8,
                Err(e) => return Poll::Ready(Err(BodyError::BodyWriteError(e))),
            }
        }
        Poll::Ready(Ok(()))
    }
}

/// A writer for a single chunk-encoded data chunk. It accepts a fixed number of
/// bytes before returning [`BodyError::BodyOverflow`].
#[derive(Debug)]
pub struct ChunkDataWriter<I> {
    /// the declared size of this chunk
    length: u64,
    /// the amount of the chunk already written
    offset: u64,
    /// the number of footer characters written; a footer is \r\n, so once this
    /// is 2, the footer is fully written
    footer_written: u8,
    /// the writer into which data will be placed
    writer: I,
}

const CHUNK_FOOTER: &[u8] = b"\r\n";

impl<I> ChunkDataWriter<I> {
    fn new(length: u64, writer: I) -> Self {
        Self {
            length,
            offset: 0,
            footer_written: 0,
            writer,
        }
    }

    /// Finish the chunk writer and return a [`ChunkedBodyWriter`].
    ///
    /// # Panics
    ///
    /// Panics if the chunk data has not been fully written or if the chunk
    /// footer has not been written via [`Self::poll_complete`].
    pub fn finish(self) -> IdleChunkWriter<I> {
        assert_eq!(
            self.offset, self.length,
            "attempted to finish chunk writer before chunk data was fully written"
        );
        assert_eq!(
            CHUNK_FOOTER.len(),
            self.footer_written as usize,
            "attempted to finish chunk writer before chunk footer was written"
        );
        IdleChunkWriter {
            writer: self.writer,
        }
    }
}

impl<I: AsyncWrite + Unpin> ChunkDataWriter<I> {
    /// Write the entire buffer and the chunk footer, returning a
    /// [`ChunkedBodyWriter`] on success.
    pub async fn write(mut self, mut buffer: &[u8]) -> BodyResult<ChunkedBodyWriter<I>> {
        while !buffer.is_empty() {
            let written = poll_fn(|cx| self.poll_write(cx, buffer)).await?;
            if written == 0 {
                return Err(BodyError::BodyWriteError(
                    std::io::ErrorKind::WriteZero.into(),
                ));
            }
            buffer = &buffer[written..];
        }
        poll_fn(|cx| self.poll_complete(cx)).await?;
        let IdleChunkWriter { writer } = self.finish();
        Ok(ChunkedBodyWriter { writer })
    }

    pub fn poll_write(&mut self, cx: &mut Context<'_>, buffer: &[u8]) -> Poll<BodyResult<usize>> {
        super::poll_write_bounded(cx, &mut self.writer, self.length, &mut self.offset, buffer)
    }

    pub fn poll_complete(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        while self.footer_written < CHUNK_FOOTER.len() as u8 {
            let buf = &CHUNK_FOOTER[self.footer_written as usize..];
            match ready!(Pin::new(&mut self.writer).poll_write(cx, buf)) {
                Ok(0) => {
                    return Poll::Ready(Err(BodyError::BodyWriteError(
                        std::io::ErrorKind::WriteZero.into(),
                    )));
                }
                Ok(w) => {
                    self.footer_written += w as u8;
                    debug_assert!(
                        self.footer_written <= CHUNK_FOOTER.len() as u8,
                        "BUG: wrote more than 2 bytes of footer"
                    );
                }
                Err(e) => return Poll::Ready(Err(BodyError::BodyWriteError(e))),
            }
        }

        Poll::Ready(Ok(()))
    }
}

/// Writes the terminal chunk (`0\r\n`), any trailers, and the final `\r\n`,
/// then flushes. Call [`finish`](FinalChunkWriter::finish) to retrieve the
/// inner writer once [`poll_write`](FinalChunkWriter::poll_write) completes.
#[derive(Debug)]
pub struct FinalChunkWriter<I> {
    /// Pre-serialized bytes: `"0\r\n{trailers}\r\n"`.
    data: Vec<u8>,
    /// Number of bytes already written.
    written: usize,
    /// Whether the writer has been flushed.
    flushed: bool,
    writer: I,
}

impl<I> FinalChunkWriter<I> {
    pub(super) fn new(trailers: &Headers, writer: I) -> Self {
        // TODO: It seems we could avoid having to copy all the headers by being
        // a little clever, but I'm not 100% sure how to go about it, so we'll
        // go with this copy method for now.

        let mut data = Vec::new();
        data.extend_from_slice(b"0\r\n");
        for (name, value) in trailers.iter() {
            data.extend_from_slice(name);
            data.extend_from_slice(b": ");
            data.extend_from_slice(value);
            data.extend_from_slice(b"\r\n");
        }
        data.extend_from_slice(b"\r\n");
        Self {
            data,
            written: 0,
            flushed: false,
            writer,
        }
    }

    /// Finish and return the inner writer.
    ///
    /// # Panics
    ///
    /// Panics if the terminal chunk has not been fully written and flushed
    /// via [`Self::poll_write`].
    pub fn finish(self) -> I {
        assert_eq!(
            self.written,
            self.data.len(),
            "attempted to finish final chunk writer before data was fully written"
        );
        assert!(
            self.flushed,
            "attempted to finish final chunk writer before flushing"
        );
        self.writer
    }
}

impl<I: AsyncWrite + Unpin> FinalChunkWriter<I> {
    /// Write the terminal chunk and flush, returning the inner writer on
    /// success.
    pub async fn write(mut self) -> BodyResult<I> {
        poll_fn(|cx| self.poll_write(cx)).await?;
        Ok(self.finish())
    }

    pub fn poll_write(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        while self.written < self.data.len() {
            let buf = &self.data[self.written..];
            match ready!(Pin::new(&mut self.writer).poll_write(cx, buf)) {
                Ok(0) => {
                    return Poll::Ready(Err(BodyError::BodyWriteError(
                        std::io::ErrorKind::WriteZero.into(),
                    )));
                }
                Ok(w) => self.written += w,
                Err(e) => return Poll::Ready(Err(BodyError::BodyWriteError(e))),
            }
        }
        match ready!(Pin::new(&mut self.writer).poll_flush(cx)) {
            Ok(()) => {
                self.flushed = true;
                Poll::Ready(Ok(()))
            }
            Err(e) => Poll::Ready(Err(BodyError::BodyWriteError(e))),
        }
    }
}

#[cfg(test)]
mod test {
    use super::ChunkedBodyWriter;

    #[tokio::test]
    async fn chunked_write_abort() {
        let mut bw = ChunkedBodyWriter::new(Vec::new());
        bw.write(b"hello").await.unwrap();
        bw.write(b"there").await.unwrap();
        let write_buf = bw.abort().await.unwrap();

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
        let mut bw = ChunkedBodyWriter::new(Vec::new());
        bw.write(b"hello").await.unwrap();
        bw.write(b"there").await.unwrap();
        let write_buf = bw.finish().await.unwrap();

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
