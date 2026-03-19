//! State-machine writer for HTTP bodies framed by connection close
//! (RFC 9112 section 6.3).
//!
//! There is no on-the-wire framing per chunk: every byte handed to
//! [`Write::poll_write`] reaches the wire unframed, and the receiver
//! detects end-of-body by the connection closing. Because the encoding
//! has no terminator, both [`Finish`] and [`Abort`] consume and drop
//! the underlying writer - holding it open past the body would leave
//! the receiver waiting indefinitely.
//!
//! The state shape mirrors the other body-crate writers so the encoding
//! can flow through the frontend's encoding-generic unifier alongside
//! the others.

use std::{
    future::poll_fn,
    pin::Pin,
    task::{Context, Poll, ready},
};

use tokio::io::AsyncWrite;

use crate::error::{BodyError, BodyResult};

/// Idle state.
#[derive(Debug)]
pub struct Ready<I> {
    writer: I,
}

impl<I> Ready<I> {
    pub fn new(writer: I) -> Self {
        Self { writer }
    }

    /// Begin a new chunk of `length` bytes. EOF framing has no upper
    /// bound on body size, so this is infallible.
    pub fn open(self, length: u64) -> Open<I> {
        let Self { writer } = self;
        Open {
            writer,
            chunk_length: length,
        }
    }

    /// Terminate the body cleanly. EOF framing has no terminator on the
    /// wire; [`Finish::poll_finish`] only flushes any buffered head or
    /// body bytes.
    pub fn finish(self) -> Finish<I> {
        let Self { writer } = self;
        Finish {
            writer,
            flushed: false,
        }
    }

    /// Terminate the body abnormally. [`Abort::poll_abort`] flushes any
    /// buffered bytes; the receiver cannot distinguish a clean EOF body
    /// from an aborted one without out-of-band signalling.
    pub fn abort(self) -> Abort<I> {
        let Self { writer } = self;
        Abort {
            writer,
            flushed: false,
        }
    }
}

/// Open state. EOF encoding emits no bytes for Open; polling completes
/// immediately.
#[derive(Debug)]
pub struct Open<I> {
    writer: I,
    chunk_length: u64,
}

impl<I> Open<I> {
    pub fn poll_open(&mut self, _cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        Poll::Ready(Ok(()))
    }

    /// Transition to [`Write`]. EOF `Open` has no header bytes to drive,
    /// so this is infallible; the `Result` shape is preserved to match
    /// the chunked encoding.
    pub fn into_write(self) -> BodyResult<Write<I>> {
        let Self {
            writer,
            chunk_length,
        } = self;
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
    pub async fn open(mut self) -> BodyResult<Write<I>> {
        poll_fn(|cx| self.poll_open(cx)).await?;
        self.into_write()
    }
}

/// Write state. Tracks the chunk-local offset against the per-chunk
/// length declared at [`Ready::open`].
#[derive(Debug)]
pub struct Write<I> {
    writer: I,
    chunk_length: u64,
    chunk_offset: u64,
}

impl<I> Write<I> {
    /// Bytes still owed for the current chunk.
    pub fn chunk_remaining(&self) -> u64 {
        self.chunk_length - self.chunk_offset
    }

    /// Transition to [`Close`]. Errors with
    /// [`BodyError::IncompleteBody`] when the caller has not written the
    /// full per-chunk budget declared at [`Ready::open`].
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
        Ok(Close { writer })
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

/// Close state. EOF encoding emits no bytes for Close; polling completes
/// immediately.
#[derive(Debug)]
pub struct Close<I> {
    writer: I,
}

impl<I> Close<I> {
    pub fn poll_close(&mut self, _cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        Poll::Ready(Ok(()))
    }

    /// Transition back to [`Ready`]. EOF `Close` has no footer bytes to
    /// drive, so this is infallible; the `Result` shape is preserved to
    /// match the chunked encoding.
    pub fn into_ready(self) -> BodyResult<Ready<I>> {
        let Self { writer } = self;
        Ok(Ready { writer })
    }
}

impl<I> Close<I>
where
    I: AsyncWrite + Unpin,
{
    pub async fn close(mut self) -> BodyResult<Ready<I>> {
        poll_fn(|cx| self.poll_close(cx)).await?;
        self.into_ready()
    }
}

/// Finish (terminal) state. EOF framing has no on-the-wire terminator;
/// the receiver detects end-of-body by the connection closing. The
/// writer is therefore consumed and dropped along with `Finish` so the
/// caller cannot accidentally hold the connection open past the body.
/// `poll_finish` only drives the final flush.
#[derive(Debug)]
pub struct Finish<I> {
    writer: I,
    flushed: bool,
}

impl<I> Finish<I> {
    /// Consume the finish, dropping the underlying writer and signalling
    /// end-of-body to the receiver via connection close. Errors with
    /// [`BodyError::UnflushedFinish`] if `poll_finish` has not driven the
    /// flush to completion.
    pub fn discard(self) -> BodyResult<()> {
        let Self { writer: _, flushed } = self;
        if !flushed {
            return Err(BodyError::UnflushedFinish);
        }
        Ok(())
    }
}

impl<I> Finish<I>
where
    I: AsyncWrite + Unpin,
{
    pub fn poll_finish(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        match ready!(Pin::new(&mut self.writer).poll_flush(cx)) {
            Ok(()) => {
                self.flushed = true;
                Poll::Ready(Ok(()))
            }
            Err(e) => Poll::Ready(Err(BodyError::BodyWriteError(e))),
        }
    }

    pub async fn finish(mut self) -> BodyResult<()> {
        poll_fn(|cx| self.poll_finish(cx)).await?;
        self.discard()
    }
}

/// Abort (terminal) state. EOF framing has no in-band poison;
/// `poll_abort` flushes any buffered bytes and the writer is dropped
/// along with `Abort`.
#[derive(Debug)]
pub struct Abort<I> {
    writer: I,
    flushed: bool,
}

impl<I> Abort<I> {
    /// Consume the abort, dropping the underlying writer. Errors with
    /// [`BodyError::UnflushedAbort`] if `poll_abort` has not driven the
    /// flush to completion.
    pub fn discard(self) -> BodyResult<()> {
        let Self { writer: _, flushed } = self;
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

    /// Drive a full Open/Write/Close cycle, writing `data` of declared
    /// chunk length `data.len()`.
    async fn drive_chunk<I>(ready: Ready<I>, data: &[u8]) -> Ready<I>
    where
        I: AsyncWrite + Unpin,
    {
        let open = ready.open(data.len() as u64);
        let mut write = open.open().await.expect("open");
        write.write_all(data).await.expect("write");
        let close = write.into_close().expect("into_close");
        close.close().await.expect("close")
    }

    #[tokio::test]
    async fn eof_full_body_single_chunk() {
        let ready = Ready::new(Vec::<u8>::new());
        let ready = drive_chunk(ready, b"hello").await;
        ready.finish().finish().await.unwrap();
    }

    #[tokio::test]
    async fn eof_full_body_two_chunks() {
        let ready = Ready::new(Vec::<u8>::new());
        let ready = drive_chunk(ready, b"abc").await;
        let _ready = drive_chunk(ready, b"defg").await;
    }

    #[tokio::test]
    async fn eof_write_overflow_in_chunk() {
        let ready = Ready::new(Vec::<u8>::new());
        let open = ready.open(3);
        let mut write = open.open().await.unwrap();
        let err = write
            .write_all(b"abcde")
            .await
            .expect_err("write should overflow");
        assert!(matches!(err, BodyError::BodyOverflow(3)));
    }

    #[tokio::test]
    async fn eof_into_close_incomplete_chunk() {
        let ready = Ready::new(Vec::<u8>::new());
        let open = ready.open(5);
        let mut write = open.open().await.unwrap();
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
    async fn eof_finish_unflushed_errors() {
        let ready = Ready::new(Vec::<u8>::new());
        let finish = ready.finish();
        let err = finish.discard().expect_err("discard should error");
        assert!(matches!(err, BodyError::UnflushedFinish));
    }

    #[tokio::test]
    async fn eof_abort_unflushed_errors() {
        let ready = Ready::new(Vec::<u8>::new());
        let abort = ready.abort();
        let err = abort.discard().expect_err("discard should error");
        assert!(matches!(err, BodyError::UnflushedAbort));
    }
}
