//! State-machine writer for HTTP bodies framed by `Content-Length`.
//!
//! The total declared length is fixed at [`Ready::new`]. Each
//! Open/Write/Close cycle writes a sub-budget of the body; the running
//! offset is preserved across cycles. [`Ready::finish`] errors if the
//! caller has not written exactly the declared length.

use std::{
    future::poll_fn,
    pin::Pin,
    task::{Context, Poll, ready},
};

use tokio::io::AsyncWrite;

use crate::error::{BodyError, BodyResult};

/// Idle state. Holds the running total offset against the declared length.
#[derive(Debug)]
pub struct Ready<I> {
    writer: I,
    length: u64,
    offset: u64,
}

impl<I> Ready<I> {
    pub fn new(writer: I, length: u64) -> Self {
        Self {
            writer,
            length,
            offset: 0,
        }
    }

    /// Bytes still owed before [`Self::finish`] becomes valid.
    pub fn remaining(&self) -> u64 {
        self.length - self.offset
    }

    /// Begin a new chunk of `length` bytes. Returns
    /// [`BodyError::BodyOverflow`] if the requested length would push the
    /// running offset past the declared total.
    pub fn open(self, length: u64) -> Result<Open<I>, BodyError> {
        let target = self
            .offset
            .checked_add(length)
            .ok_or(BodyError::BodyOverflow(self.length))?;
        if target > self.length {
            return Err(BodyError::BodyOverflow(self.length));
        }
        let Self {
            writer,
            length: total_length,
            offset,
        } = self;
        Ok(Open {
            writer,
            total_length,
            offset,
            chunk_length: length,
        })
    }

    /// Terminate the body. Errors with [`BodyError::IncompleteBody`] when
    /// `offset < length`. On success, [`Finish`] flushes the underlying
    /// writer so any buffered head or body bytes reach the wire.
    pub fn finish(self) -> Result<Finish<I>, BodyError> {
        let Self {
            writer,
            length,
            offset,
        } = self;
        if offset < length {
            return Err(BodyError::IncompleteBody {
                expected: length,
                actual: offset,
            });
        }
        Ok(Finish {
            writer,
            flushed: false,
        })
    }

    /// Terminate the body abnormally. [`Abort::poll_abort`] flushes the
    /// underlying writer so any buffered head or body bytes reach the
    /// wire; the receiver will then detect a short body via byte count,
    /// and the writer is consumed and dropped along with `Abort`.
    pub fn abort(self) -> Abort<I> {
        let Self { writer, .. } = self;
        Abort {
            writer,
            flushed: false,
        }
    }
}

/// Open state. Content-length encoding emits no bytes for Open; polling
/// completes immediately.
#[derive(Debug)]
pub struct Open<I> {
    writer: I,
    total_length: u64,
    offset: u64,
    chunk_length: u64,
}

impl<I> Open<I> {
    pub fn poll_open(&mut self, _cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        Poll::Ready(Ok(()))
    }

    /// Transition to [`Write`]. Content-length `Open` has no header bytes
    /// to drive, so this is infallible; the `Result` shape is preserved to
    /// match the chunked encoding.
    pub fn into_write(self) -> BodyResult<Write<I>> {
        let Self {
            writer,
            total_length,
            offset,
            chunk_length,
        } = self;
        Ok(Write {
            writer,
            total_length,
            offset,
            chunk_length,
            chunk_offset: 0,
        })
    }

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
    total_length: u64,
    offset: u64,
    chunk_length: u64,
    chunk_offset: u64,
}

impl<I> Write<I> {
    /// Bytes still owed for the current chunk.
    pub fn chunk_remaining(&self) -> u64 {
        self.chunk_length - self.chunk_offset
    }

    pub fn into_close(self) -> BodyResult<Close<I>> {
        let Self {
            writer,
            total_length,
            offset,
            chunk_length,
            chunk_offset,
        } = self;
        if chunk_offset < chunk_length {
            return Err(BodyError::IncompleteBody {
                expected: chunk_length,
                actual: chunk_offset,
            });
        }
        Ok(Close {
            writer,
            total_length,
            offset: offset + chunk_length,
        })
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

/// Close state. Content-length encoding emits no bytes for Close; polling
/// completes immediately.
#[derive(Debug)]
pub struct Close<I> {
    writer: I,
    total_length: u64,
    offset: u64,
}

impl<I> Close<I> {
    pub fn poll_close(&mut self, _cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        Poll::Ready(Ok(()))
    }

    /// Transition back to [`Ready`]. Content-length `Close` has no footer
    /// bytes to drive, so this is infallible; the `Result` shape is
    /// preserved to match the chunked encoding.
    pub fn into_ready(self) -> BodyResult<Ready<I>> {
        let Self {
            writer,
            total_length,
            offset,
        } = self;
        Ok(Ready {
            writer,
            length: total_length,
            offset,
        })
    }

    pub async fn close(mut self) -> BodyResult<Ready<I>> {
        poll_fn(|cx| self.poll_close(cx)).await?;
        self.into_ready()
    }
}

/// Finish (terminal) state. Content-length encoding emits no terminal
/// bytes but flushes the underlying writer so any buffered head or body
/// bytes reach the wire.
#[derive(Debug)]
pub struct Finish<I> {
    writer: I,
    flushed: bool,
}

impl<I> Finish<I> {
    /// Yield the underlying writer. Errors with
    /// [`BodyError::UnflushedFinish`] if `poll_finish` has not driven the
    /// flush to completion.
    pub fn into_writer(self) -> BodyResult<I> {
        let Self { writer, flushed } = self;
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

/// Abort (terminal) state. Content-length encoding has no in-band poison;
/// `poll_abort` flushes the underlying writer so any buffered head or
/// body bytes reach the wire (the receiver will detect a short body via
/// byte count), then the writer is dropped along with `Abort`.
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
        let open = ready.open(data.len() as u64).expect("open");
        let mut write = open.open().await.expect("open");
        write.write_all(data).await.expect("write");
        let close = write.into_close().expect("into_close");
        close.close().await.expect("close")
    }

    #[tokio::test]
    async fn cl_full_body_single_chunk() {
        let ready = Ready::new(Vec::<u8>::new(), 5);
        let ready = drive_chunk(ready, b"hello").await;
        let buf = ready.finish().expect("finish").finish().await.unwrap();
        assert_eq!(b"hello"[..], buf[..]);
    }

    #[tokio::test]
    async fn cl_full_body_two_chunks() {
        let ready = Ready::new(Vec::<u8>::new(), 7);
        let ready = drive_chunk(ready, b"abc").await;
        let ready = drive_chunk(ready, b"defg").await;
        let buf = ready.finish().expect("finish").finish().await.unwrap();
        assert_eq!(b"abcdefg"[..], buf[..]);
    }

    #[tokio::test]
    async fn cl_finish_before_complete_errors() {
        let ready = Ready::new(Vec::<u8>::new(), 10);
        let ready = drive_chunk(ready, b"abc").await;
        let err = ready.finish().expect_err("finish should error");
        assert!(matches!(
            err,
            BodyError::IncompleteBody {
                expected: 10,
                actual: 3
            }
        ));
    }

    #[tokio::test]
    async fn cl_open_overflow() {
        let ready = Ready::new(Vec::<u8>::new(), 10);
        let err = ready.open(11).expect_err("open should overflow");
        assert!(matches!(err, BodyError::BodyOverflow(10)));
    }

    #[tokio::test]
    async fn cl_open_overflow_after_partial() {
        let ready = Ready::new(Vec::<u8>::new(), 10);
        let ready = drive_chunk(ready, b"abcde").await;
        let err = ready.open(6).expect_err("open should overflow");
        assert!(matches!(err, BodyError::BodyOverflow(10)));
    }

    #[tokio::test]
    async fn cl_write_overflow_in_chunk() {
        let ready = Ready::new(Vec::<u8>::new(), 10);
        let open = ready.open(3).unwrap();
        let mut write = open.open().await.unwrap();
        let err = write
            .write_all(b"abcde")
            .await
            .expect_err("write should overflow");
        assert!(matches!(err, BodyError::BodyOverflow(3)));
    }

    #[tokio::test]
    async fn cl_into_close_incomplete_chunk() {
        let ready = Ready::new(Vec::<u8>::new(), 10);
        let open = ready.open(5).unwrap();
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
    async fn cl_zero_length_finish_immediately() {
        let ready = Ready::new(Vec::<u8>::new(), 0);
        let buf = ready.finish().expect("finish").finish().await.unwrap();
        assert!(buf.is_empty());
    }
}
