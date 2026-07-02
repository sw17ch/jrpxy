//! State-machine writer for HTTP bodies framed by
//! `Transfer-Encoding: chunked` (RFC 9112 section 7).
//!
//! Each Open/Write/Close cycle produces one chunk on the wire:
//! `{size:x}\r\n{data}\r\n`. [`Ready::finish`] writes the terminal chunk
//! `0\r\n{trailers}\r\n` and flushes. [`Ready::abort`] writes an invalid
//! chunk header byte to poison the stream so a downstream reader cannot
//! mistake a truncated body for a complete one.

use std::{
    future::poll_fn,
    pin::Pin,
    task::{Context, Poll, ready},
};

use jrpxy_http_message::header::Headers;
use tokio::io::AsyncWrite;

use crate::error::{BodyError, BodyResult};

/// Maximum length of a chunk header: 16 hex digits + `\r\n`.
const MAX_CHUNK_HEAD_LEN: usize = 18;

/// The trailing `\r\n` of every non-empty chunk.
const CHUNK_FOOTER: &[u8] = b"\r\n";

/// Idle state.
#[derive(Debug)]
pub struct Ready<I> {
    writer: I,
}

impl<I> Ready<I> {
    pub fn new(writer: I) -> Self {
        Self { writer }
    }

    /// Begin a new chunk of `length` bytes. Chunked encoding has no upper
    /// bound on body size, so this is infallible. A length of zero
    /// produces a no-op cycle that emits nothing on the wire (the
    /// end-of-body marker `0\r\n\r\n` is reserved for [`Self::finish`]).
    pub fn open(self, length: u64) -> Open<I> {
        let Self { writer } = self;
        Open::new(writer, length)
    }

    /// Terminate the body cleanly. Emits `0\r\n\r\n` and flushes.
    pub fn finish(self) -> Finish<I> {
        self.finish_with_trailers(&Default::default())
    }

    /// Terminate the body cleanly with trailer fields. Emits
    /// `0\r\n{trailers}\r\n` and flushes.
    pub fn finish_with_trailers(self, trailers: &Headers) -> Finish<I> {
        let Self { writer } = self;
        Finish::new(writer, trailers)
    }

    /// Terminate the body abnormally. Writes an invalid chunk header byte
    /// (`x`) and flushes; the writer is then dropped along with `Abort`.
    pub fn abort(self) -> Abort<I> {
        let Self { writer } = self;
        Abort::new(writer)
    }
}

/// Open state. Writes the chunk-size header bytes (`{N:x}\r\n`).
#[derive(Debug)]
pub struct Open<I> {
    writer: I,
    /// Pre-formatted header bytes, e.g. `b"5\r\n"`.
    header: [u8; MAX_CHUNK_HEAD_LEN],
    /// Number of valid bytes in `header` (zero for empty chunks, which
    /// emit nothing on the wire).
    header_len: u8,
    /// Number of header bytes already written.
    written: u8,
    /// Declared chunk length, carried into [`Write`] and [`Close`].
    chunk_length: u64,
}

impl<I> Open<I> {
    fn new(writer: I, chunk_length: u64) -> Self {
        use std::io::Write;
        let mut header = [0u8; MAX_CHUNK_HEAD_LEN];
        let header_len = if chunk_length == 0 {
            0
        } else {
            let mut cursor = std::io::Cursor::new(&mut header[..]);
            // 16 hex digits + \r\n always fits in MAX_CHUNK_HEAD_LEN.
            write!(cursor, "{chunk_length:x}\r\n").expect("write to static buffer failed");
            cursor.position() as u8
        };
        Self {
            writer,
            header,
            header_len,
            written: 0,
            chunk_length,
        }
    }

    /// Transition to [`Write`]. Errors with
    /// [`BodyError::IncompleteChunkHeader`] if `poll_open` has not driven
    /// the chunk-size header bytes to completion.
    pub fn into_write(self) -> BodyResult<Write<I>> {
        let Self {
            writer,
            header: _,
            header_len,
            written,
            chunk_length,
        } = self;
        if written < header_len {
            return Err(BodyError::IncompleteChunkHeader {
                expected: header_len as u64,
                actual: written as u64,
            });
        }
        Ok(Write {
            writer,
            chunk_length,
            chunk_offset: 0,
        })
    }
}

impl<I> Open<I>
where
    I: AsyncWrite + Unpin,
{
    pub fn poll_open(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
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

    pub async fn open(mut self) -> BodyResult<Write<I>> {
        poll_fn(|cx| self.poll_open(cx)).await?;
        self.into_write()
    }
}

/// Write state. Tracks the chunk-local offset against the declared chunk
/// length.
#[derive(Debug)]
pub struct Write<I> {
    writer: I,
    chunk_length: u64,
    chunk_offset: u64,
}

impl<I> Write<I> {
    pub fn chunk_remaining(&self) -> u64 {
        self.chunk_length - self.chunk_offset
    }

    pub fn into_close(self) -> BodyResult<Close<I>> {
        let Self {
            writer,
            chunk_length,
            chunk_offset,
        } = self;
        if chunk_offset < chunk_length {
            return Err(BodyError::IncompleteBody {
                expected: chunk_length,
                actual: chunk_offset,
            });
        }
        Ok(Close::new(writer, chunk_length))
    }
}

impl<I> Write<I>
where
    I: AsyncWrite + Unpin,
{
    pub fn poll_write(&mut self, cx: &mut Context<'_>, buffer: &[u8]) -> Poll<BodyResult<usize>> {
        super::poll_write_bounded(
            cx,
            &mut self.writer,
            self.chunk_length,
            &mut self.chunk_offset,
            buffer,
        )
    }

    pub async fn write_all(&mut self, mut buffer: &[u8]) -> BodyResult<()> {
        while !buffer.is_empty() {
            let written = poll_fn(|cx| self.poll_write(cx, buffer)).await?;
            if written == 0 {
                return Err(BodyError::BodyWriteError(
                    std::io::ErrorKind::WriteZero.into(),
                ));
            }
            buffer = &buffer[written..];
        }
        Ok(())
    }
}

/// Close state. Writes the trailing `\r\n` for non-empty chunks; for
/// empty chunks (which emit no header) the close is a no-op.
#[derive(Debug)]
pub struct Close<I> {
    writer: I,
    /// 2 for non-empty chunks (write `\r\n`); 0 for empty chunks.
    footer_len: u8,
    /// Number of footer bytes already written.
    footer_written: u8,
}

impl<I> Close<I> {
    fn new(writer: I, chunk_length: u64) -> Self {
        let footer_len = if chunk_length == 0 {
            0
        } else {
            CHUNK_FOOTER.len() as u8
        };
        Self {
            writer,
            footer_len,
            footer_written: 0,
        }
    }

    /// Transition back to [`Ready`]. Errors with
    /// [`BodyError::IncompleteChunkFooter`] if `poll_close` has not driven
    /// the trailing `\r\n` to completion.
    pub fn into_ready(self) -> BodyResult<Ready<I>> {
        let Self {
            writer,
            footer_len,
            footer_written,
        } = self;
        if footer_written < footer_len {
            return Err(BodyError::IncompleteChunkFooter {
                expected: footer_len as u64,
                actual: footer_written as u64,
            });
        }
        Ok(Ready { writer })
    }
}

impl<I> Close<I>
where
    I: AsyncWrite + Unpin,
{
    pub fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        while self.footer_written < self.footer_len {
            let buf = &CHUNK_FOOTER[self.footer_written as usize..self.footer_len as usize];
            match ready!(Pin::new(&mut self.writer).poll_write(cx, buf)) {
                Ok(0) => {
                    return Poll::Ready(Err(BodyError::BodyWriteError(
                        std::io::ErrorKind::WriteZero.into(),
                    )));
                }
                Ok(w) => self.footer_written += w as u8,
                Err(e) => return Poll::Ready(Err(BodyError::BodyWriteError(e))),
            }
        }
        Poll::Ready(Ok(()))
    }

    pub async fn close(mut self) -> BodyResult<Ready<I>> {
        poll_fn(|cx| self.poll_close(cx)).await?;
        self.into_ready()
    }
}

/// Finish (terminal) state. Writes `0\r\n`, any trailers, and the closing
/// `\r\n`; then flushes.
#[derive(Debug)]
pub struct Finish<I> {
    writer: I,
    /// Pre-serialized terminal bytes: `"0\r\n{trailers}\r\n"`.
    data: Vec<u8>,
    written: usize,
    flushed: bool,
}

impl<I> Finish<I> {
    fn new(writer: I, trailers: &Headers) -> Self {
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
            writer,
            data,
            written: 0,
            flushed: false,
        }
    }

    /// Yield the underlying writer. Errors with
    /// [`BodyError::IncompleteFinish`] if the terminal bytes have not been
    /// fully written, or [`BodyError::UnflushedFinish`] if they have been
    /// written but the writer has not yet been flushed.
    pub fn into_writer(self) -> BodyResult<I> {
        let Self {
            writer,
            data,
            written,
            flushed,
        } = self;
        if written < data.len() {
            return Err(BodyError::IncompleteFinish {
                expected: data.len() as u64,
                actual: written as u64,
            });
        }
        if !flushed {
            return Err(BodyError::UnflushedFinish);
        }
        Ok(writer)
    }
}

impl<I> Finish<I>
where
    I: AsyncWrite + Unpin,
{
    pub fn poll_finish(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
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

    pub async fn finish(mut self) -> BodyResult<I> {
        poll_fn(|cx| self.poll_finish(cx)).await?;
        self.into_writer()
    }
}

/// Abort (terminal) state. Writes an invalid chunk header byte (`x`) and
/// flushes. The writer is consumed and dropped along with `Abort`.
#[derive(Debug)]
pub struct Abort<I> {
    writer: I,
    written: bool,
    flushed: bool,
}

impl<I> Abort<I> {
    fn new(writer: I) -> Self {
        Self {
            writer,
            written: false,
            flushed: false,
        }
    }

    /// Consume the abort, dropping the underlying writer. Errors with
    /// [`BodyError::UnflushedAbort`] if `poll_abort` has not driven the
    /// flush to completion.
    pub fn discard(self) -> BodyResult<()> {
        let Self {
            writer: _,
            written: _,
            flushed,
        } = self;
        if !flushed {
            return Err(BodyError::UnflushedAbort);
        }
        Ok(())
    }
}

impl<I> Abort<I>
where
    I: AsyncWrite + Unpin,
{
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

    pub async fn abort(mut self) -> BodyResult<()> {
        poll_fn(|cx| self.poll_abort(cx)).await?;
        self.discard()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// Drive a full Open/Write/Close cycle for `data`.
    async fn drive_chunk<I>(ready: Ready<I>, data: &[u8]) -> Ready<I>
    where
        I: AsyncWrite + Unpin,
    {
        let open = ready.open(data.len() as u64);
        let mut write = open.open().await.expect("open");
        write.write_all(data).await.expect("write_all");
        let close = write.into_close().expect("into_close");
        close.close().await.expect("close")
    }

    #[tokio::test]
    async fn chunked_two_chunks_finish() {
        let mut buf = Vec::new();
        {
            let ready = Ready::new(&mut buf);
            let ready = drive_chunk(ready, b"hello").await;
            let ready = drive_chunk(ready, b"there").await;
            ready.finish().finish().await.unwrap();
        }
        assert_eq!(
            "5\r\nhello\r\n5\r\nthere\r\n0\r\n\r\n",
            std::str::from_utf8(&buf).unwrap()
        );
    }

    #[tokio::test]
    async fn chunked_two_chunks_abort() {
        let mut buf = Vec::new();
        {
            let ready = Ready::new(&mut buf);
            let ready = drive_chunk(ready, b"hello").await;
            let ready = drive_chunk(ready, b"there").await;
            ready.abort().abort().await.unwrap();
        }
        assert_eq!(
            "5\r\nhello\r\n5\r\nthere\r\nx",
            std::str::from_utf8(&buf).unwrap()
        );
    }

    #[tokio::test]
    async fn chunked_open_zero_is_noop_cycle() {
        let mut buf = Vec::new();
        {
            let ready = Ready::new(&mut buf);
            let mut write = ready.open(0).open().await.unwrap();
            write.write_all(b"").await.unwrap();
            let ready = write.into_close().unwrap().close().await.unwrap();
            ready.finish().finish().await.unwrap();
        }
        assert_eq!("0\r\n\r\n", std::str::from_utf8(&buf).unwrap());
    }

    #[tokio::test]
    async fn chunked_open_zero_rejects_data() {
        let ready = Ready::new(Vec::<u8>::new());
        let mut write = ready.open(0).open().await.unwrap();
        let err = write
            .write_all(b"x")
            .await
            .expect_err("write_all on empty chunk should overflow");
        assert!(matches!(err, BodyError::BodyOverflow(0)));
    }

    #[tokio::test]
    async fn chunked_into_close_incomplete_chunk() {
        let ready = Ready::new(Vec::<u8>::new());
        let mut write = ready.open(5).open().await.unwrap();
        write.write_all(b"abc").await.unwrap();
        let err = write
            .into_close()
            .expect_err("into_close should error on partial chunk");
        assert!(matches!(
            err,
            BodyError::IncompleteBody {
                expected: 5,
                actual: 3
            }
        ));
    }

    #[tokio::test]
    async fn chunked_finish_with_trailers() {
        use jrpxy_http_message::header::Headers;
        let mut buf = Vec::new();
        {
            let ready = Ready::new(&mut buf);
            let ready = drive_chunk(ready, b"hi").await;
            let mut trailers = Headers::default();
            trailers.push_raw("x-foo", &b"bar"[..]);
            ready
                .finish_with_trailers(&trailers)
                .finish()
                .await
                .unwrap();
        }
        assert_eq!(
            "2\r\nhi\r\n0\r\nx-foo: bar\r\n\r\n",
            std::str::from_utf8(&buf).unwrap()
        );
    }
}
