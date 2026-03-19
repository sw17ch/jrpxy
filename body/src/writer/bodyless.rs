//! State-machine writer for HTTP messages that have no body. A bodyless
//! writer never emits any body bytes to the wire; the only valid call to
//! [`Ready::open`] is for length zero. The full Ready/Open/Write/Close set
//! exists to preserve a uniform shape across all body encodings.

use std::{
    future::poll_fn,
    pin::Pin,
    task::{Context, Poll, ready},
};

use tokio::io::AsyncWrite;

use crate::error::{BodyError, BodyResult};

/// Idle state. Bodyless writers permit zero-length opens, finish, and
/// abort.
#[derive(Debug)]
pub struct Ready<I> {
    writer: I,
}

impl<I> Ready<I> {
    pub fn new(writer: I) -> Self {
        Self { writer }
    }

    /// Begin a new chunk of `length` bytes. Bodyless writers only permit
    /// `length == 0`; any positive length returns
    /// [`BodyError::BodyOverflow`].
    pub fn open(self, length: u64) -> Result<Open<I>, BodyError> {
        if length > 0 {
            return Err(BodyError::BodyOverflow(0));
        }
        let Self { writer } = self;
        Ok(Open { writer })
    }

    /// Terminate the body cleanly. Bodyless `Finish` emits no body bytes
    /// but flushes the underlying writer so any buffered head bytes reach
    /// the wire.
    pub fn finish(self) -> Finish<I> {
        let Self { writer } = self;
        Finish {
            writer,
            flushed: false,
        }
    }

    /// Terminate the body abnormally. For bodyless writers there is no
    /// stream to poison; [`Abort::poll_abort`] flushes the underlying
    /// writer so any buffered head bytes reach the wire, then the writer
    /// is consumed and dropped along with `Abort`.
    pub fn abort(self) -> Abort<I> {
        let Self { writer } = self;
        Abort {
            writer,
            flushed: false,
        }
    }
}

/// Open state. Bodyless `Open` has nothing to emit; polling completes
/// immediately.
#[derive(Debug)]
pub struct Open<I> {
    writer: I,
}

impl<I> Open<I> {
    pub fn poll_open(&mut self, _cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        Poll::Ready(Ok(()))
    }

    /// Transition to [`Write`]. Bodyless `Open` has no header bytes to
    /// drive, so this is infallible; the `Result` shape is preserved to
    /// match the chunked encoding.
    pub fn into_write(self) -> BodyResult<Write<I>> {
        let Self { writer } = self;
        Ok(Write { writer })
    }

    pub async fn open(mut self) -> BodyResult<Write<I>> {
        poll_fn(|cx| self.poll_open(cx)).await?;
        self.into_write()
    }
}

/// Write state. Bodyless writers reject any non-empty buffer.
#[derive(Debug)]
pub struct Write<I> {
    writer: I,
}

impl<I> Write<I> {
    pub fn poll_write(&mut self, _cx: &mut Context<'_>, buffer: &[u8]) -> Poll<BodyResult<usize>> {
        if buffer.is_empty() {
            Poll::Ready(Ok(0))
        } else {
            Poll::Ready(Err(BodyError::BodyOverflow(0)))
        }
    }

    pub fn into_close(self) -> BodyResult<Close<I>> {
        let Self { writer } = self;
        Ok(Close { writer })
    }

    pub async fn write_all(&mut self, buffer: &[u8]) -> BodyResult<()> {
        if !buffer.is_empty() {
            return Err(BodyError::BodyOverflow(0));
        }
        Ok(())
    }
}

/// Close state. Bodyless `Close` has nothing to emit; polling completes
/// immediately.
#[derive(Debug)]
pub struct Close<I> {
    writer: I,
}

impl<I> Close<I> {
    pub fn poll_close(&mut self, _cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        Poll::Ready(Ok(()))
    }

    /// Transition back to [`Ready`]. Bodyless `Close` has no footer bytes
    /// to drive, so this is infallible; the `Result` shape is preserved to
    /// match the chunked encoding.
    pub fn into_ready(self) -> BodyResult<Ready<I>> {
        let Self { writer } = self;
        Ok(Ready { writer })
    }

    pub async fn close(mut self) -> BodyResult<Ready<I>> {
        poll_fn(|cx| self.poll_close(cx)).await?;
        self.into_ready()
    }
}

/// Finish (terminal) state. Bodyless `Finish` emits no body bytes but
/// flushes the underlying writer so any buffered head bytes reach the
/// wire.
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

/// Abort (terminal) state. Bodyless writers have no stream to poison;
/// `poll_abort` flushes the underlying writer so any buffered head bytes
/// reach the wire, then the writer is dropped along with `Abort`.
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

    #[tokio::test]
    async fn bodyless_open_zero_then_close() {
        let ready = Ready::new(Vec::<u8>::new());
        let open = ready.open(0).expect("open(0) should succeed");
        let mut write = open.open().await.expect("open");
        write.write_all(b"").await.expect("empty write");
        let close = write.into_close().expect("into_close");
        let _ready = close.close().await.expect("close");
    }

    #[tokio::test]
    async fn bodyless_open_nonzero_overflows() {
        let ready = Ready::new(Vec::<u8>::new());
        let err = ready.open(1).expect_err("open(1) should overflow");
        assert!(matches!(err, BodyError::BodyOverflow(0)));
    }

    #[tokio::test]
    async fn bodyless_write_nonempty_overflows() {
        let ready = Ready::new(Vec::<u8>::new());
        let open = ready.open(0).unwrap();
        let mut write = open.into_write().unwrap();
        let err = write
            .write_all(b"hi")
            .await
            .expect_err("non-empty write should overflow");
        assert!(matches!(err, BodyError::BodyOverflow(0)));
    }

    #[tokio::test]
    async fn bodyless_finish_yields_writer() {
        let ready = Ready::new(Vec::<u8>::new());
        let buf = ready.finish().finish().await.unwrap();
        assert!(buf.is_empty());
    }

    #[tokio::test]
    async fn bodyless_abort_drops_silently() {
        let ready = Ready::new(Vec::<u8>::new());
        ready.abort().abort().await.unwrap();
    }
}
