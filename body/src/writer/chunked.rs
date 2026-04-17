use std::{
    future::poll_fn,
    pin::Pin,
    task::{Context, Poll, ready},
};

use jrpxy_http_message::header::Headers;
use tokio::io::AsyncWrite;

use crate::error::{BodyError, BodyResult};

#[derive(Debug)]
pub struct ChunkedBodyWriter<I> {
    /// `None` after an error or once the writer has been finished/aborted.
    inner: Option<IdleWriter<I>>,
}

impl<I> ChunkedBodyWriter<I> {
    pub fn new(writer: I) -> Self {
        Self {
            inner: Some(IdleWriter::new(writer)),
        }
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
        let Some(idle) = self.inner.take() else {
            return Err(BodyError::WriteAfterError);
        };
        let idle = idle
            .start(buffer.len() as u64)
            .write()
            .await?
            .write(buffer)
            .await?
            .write()
            .await?;
        self.inner = Some(idle);
        Ok(())
    }

    pub async fn finish_with_trailers(mut self, trailers: &Headers) -> BodyResult<I> {
        let Some(idle) = self.inner.take() else {
            return Err(BodyError::WriteAfterError);
        };
        idle.into_final(trailers).write().await
    }

    pub async fn finish(self) -> BodyResult<I> {
        self.finish_with_trailers(&Default::default()).await
    }

    pub async fn abort(mut self) -> BodyResult<I> {
        let Some(idle) = self.inner.take() else {
            return Err(BodyError::WriteAfterError);
        };
        idle.abort().write().await
    }
}

#[derive(Debug)]
pub struct IdleWriter<I> {
    writer: I,
}

impl<I> IdleWriter<I> {
    pub fn new(writer: I) -> Self {
        Self { writer }
    }

    pub fn start(self, chunk_length: u64) -> HeadWriter<I> {
        let Self { writer } = self;
        HeadWriter::new(chunk_length, writer)
    }

    pub fn into_final(self, trailers: &Headers) -> FinalWriter<I> {
        let Self { writer } = self;
        FinalWriter::new(trailers, writer)
    }

    pub fn abort(self) -> AbortWriter<I> {
        AbortWriter {
            written: false,
            flushed: false,
            writer: self.writer,
        }
    }
}

/// Aborts a chunked transfer by writing an invalid chunk header byte (`x`),
/// then flushing. The invalid hex character signals to the reader that the
/// body is malformed, which is more robust than relying on connection close
/// when readers don't wait for the terminating empty chunk.
#[derive(Debug)]
pub struct AbortWriter<I> {
    written: bool,
    flushed: bool,
    writer: I,
}

impl<I: AsyncWrite + Unpin> AbortWriter<I> {
    /// Write the abort byte and flush, returning the inner writer on success.
    pub async fn write(mut self) -> BodyResult<I> {
        poll_fn(|cx| self.poll_abort(cx)).await?;
        Ok(self.writer)
    }

    pub fn poll_abort(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        if !self.written {
            match ready!(Pin::new(&mut self.writer).poll_write(cx, b"x")) {
                Ok(0) => {
                    return Poll::Ready(Err(BodyError::BodyWriteError(
                        std::io::ErrorKind::WriteZero.into(),
                    )));
                }
                Ok(_) => self.written = true,
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

/// Maximum length of a chunk header: 16 hex digits + `\r\n`.
const MAX_CHUNK_HEAD_LEN: usize = 18;

/// Writes the header line for a single chunk (`{size:x}\r\n`). Once fully
/// written, call [`finish`](HeadWriter::finish) to obtain a
/// [`DataWriter`] for the chunk body.
#[derive(Debug)]
pub struct HeadWriter<I> {
    /// Pre-formatted header bytes, e.g. `"1a\r\n"`.
    header: [u8; MAX_CHUNK_HEAD_LEN],
    /// Number of valid bytes in `header`.
    header_len: u8,
    /// Number of header bytes already written.
    written: u8,
    /// The chunk body length, forwarded to [`DataWriter`] on finish.
    chunk_length: u64,
    writer: I,
}

impl<I> HeadWriter<I> {
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

    /// Finish the head writer and return a [`DataWriter`] for the body.
    ///
    /// # Panics
    ///
    /// Panics if the chunk header has not been fully written via
    /// [`Self::poll_write`].
    pub fn finish(self) -> DataWriter<I> {
        assert_eq!(
            self.written, self.header_len,
            "attempted to finish chunk head writer before header was fully written"
        );
        DataWriter::new(self.chunk_length, self.writer)
    }
}

impl<I: AsyncWrite + Unpin> HeadWriter<I> {
    /// Write the entire chunk header, returning a [`DataWriter`] on success.
    pub async fn write(mut self) -> BodyResult<DataWriter<I>> {
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

/// Writes the data bytes of a single chunk. The declared length is fixed at
/// construction; attempting to write more bytes returns
/// [`BodyError::BodyOverflow`]. Call [`finish`](DataWriter::finish) once
/// [`is_complete`](DataWriter::is_complete) returns `true` to obtain a
/// [`DataCompleter`] for the chunk footer.
#[derive(Debug)]
pub struct DataWriter<I> {
    /// the declared size of this chunk
    length: u64,
    /// the amount of the chunk already written
    offset: u64,
    /// the writer into which data will be placed
    writer: I,
}

impl<I> DataWriter<I> {
    fn new(length: u64, writer: I) -> Self {
        Self {
            length,
            offset: 0,
            writer,
        }
    }

    /// Returns `true` when all declared bytes have been written and the chunk
    /// is ready to be completed via [`finish`](Self::finish).
    pub fn is_complete(&self) -> bool {
        self.offset == self.length
    }

    /// Finish the data writer and return a [`DataCompleter`] for the chunk
    /// footer.
    ///
    /// # Panics
    ///
    /// Panics if the chunk data has not been fully written via
    /// [`Self::poll_write`].
    pub fn finish(self) -> DataCompleter<I> {
        assert_eq!(
            self.offset, self.length,
            "attempted to finish data writer before all chunk data was written"
        );
        DataCompleter {
            footer_written: 0,
            writer: self.writer,
        }
    }
}

impl<I: AsyncWrite + Unpin> DataWriter<I> {
    /// Write the entire buffer, returning a [`DataCompleter`] on success.
    pub async fn write(mut self, mut buffer: &[u8]) -> BodyResult<DataCompleter<I>> {
        while !buffer.is_empty() {
            let written = poll_fn(|cx| self.poll_write(cx, buffer)).await?;
            if written == 0 {
                return Err(BodyError::BodyWriteError(
                    std::io::ErrorKind::WriteZero.into(),
                ));
            }
            buffer = &buffer[written..];
        }
        Ok(self.finish())
    }

    pub fn poll_write(&mut self, cx: &mut Context<'_>, buffer: &[u8]) -> Poll<BodyResult<usize>> {
        super::poll_write_bounded(cx, &mut self.writer, self.length, &mut self.offset, buffer)
    }
}

/// The footer (`\r\n`) terminating a single chunk. Obtained from
/// [`DataWriter::finish`] once all chunk data has been written.
#[derive(Debug)]
pub struct DataCompleter<I> {
    /// the number of footer bytes already written; the footer is `\r\n`
    footer_written: u8,
    writer: I,
}

const CHUNK_FOOTER: &[u8] = b"\r\n";

impl<I> DataCompleter<I> {
    /// Finish the completer and return an [`IdleWriter`] ready for the next
    /// chunk.
    ///
    /// # Panics
    ///
    /// Panics if the chunk footer has not been fully written via
    /// [`Self::poll_complete`].
    pub fn finish(self) -> IdleWriter<I> {
        assert_eq!(
            self.footer_written as usize,
            CHUNK_FOOTER.len(),
            "attempted to finish data completer before chunk footer was written"
        );
        IdleWriter {
            writer: self.writer,
        }
    }
}

impl<I: AsyncWrite + Unpin> DataCompleter<I> {
    /// Write the chunk footer, returning an [`IdleWriter`] on success.
    pub async fn write(mut self) -> BodyResult<IdleWriter<I>> {
        poll_fn(|cx| self.poll_complete(cx)).await?;
        Ok(self.finish())
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
/// then flushes. Call [`finish`](FinalWriter::finish) to retrieve the
/// inner writer once [`poll_write`](FinalWriter::poll_write) completes.
#[derive(Debug)]
pub struct FinalWriter<I> {
    /// Pre-serialized bytes: `"0\r\n{trailers}\r\n"`.
    data: Vec<u8>,
    /// Number of bytes already written.
    written: usize,
    /// Whether the writer has been flushed.
    flushed: bool,
    writer: I,
}

impl<I> FinalWriter<I> {
    fn new(trailers: &Headers, writer: I) -> Self {
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

impl<I: AsyncWrite + Unpin> FinalWriter<I> {
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
