//! Frontend wrappers around [`jrpxy_body::writer::eof`]. EOF framing has
//! no on-the-wire terminator; both [`Finish`] and [`Abort`] consume and
//! drop the underlying writer so the caller cannot accidentally hold the
//! connection open past the body.

use std::{
    future::poll_fn,
    task::{Context, Poll},
};

use jrpxy_body::{error::BodyResult, writer::eof};
use tokio::io::AsyncWrite;

/// Idle state.
#[derive(Debug)]
pub struct Ready<I> {
    inner: eof::Ready<I>,
}

impl<I> Ready<I> {
    pub(crate) fn new(inner: eof::Ready<I>) -> Self {
        Self { inner }
    }

    /// Begin a new chunk of `length` bytes. EOF framing has no upper
    /// bound on body size, so this is infallible.
    pub fn open(self, length: u64) -> Open<I> {
        let Self { inner } = self;
        Open {
            inner: inner.open(length),
        }
    }

    /// Terminate the body cleanly.
    pub fn finish(self) -> Finish<I> {
        let Self { inner } = self;
        Finish {
            inner: inner.finish(),
        }
    }

    /// Terminate the body abnormally.
    pub fn abort(self) -> Abort<I> {
        let Self { inner } = self;
        Abort {
            inner: inner.abort(),
        }
    }
}

/// Open state.
#[derive(Debug)]
pub struct Open<I> {
    inner: eof::Open<I>,
}

impl<I> Open<I> {
    pub fn into_write(self) -> BodyResult<Write<I>> {
        let Self { inner } = self;
        Ok(Write {
            inner: inner.into_write()?,
        })
    }
}

impl<I> Open<I>
where
    I: AsyncWrite + Unpin,
{
    pub fn poll_open(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        self.inner.poll_open(cx)
    }

    pub async fn open(mut self) -> BodyResult<Write<I>> {
        poll_fn(|cx| self.poll_open(cx)).await?;
        self.into_write()
    }
}

/// Write state.
#[derive(Debug)]
pub struct Write<I> {
    inner: eof::Write<I>,
}

impl<I> Write<I> {
    pub fn chunk_remaining(&self) -> u64 {
        self.inner.chunk_remaining()
    }

    pub fn into_close(self) -> BodyResult<Close<I>> {
        let Self { inner } = self;
        Ok(Close {
            inner: inner.into_close()?,
        })
    }
}

impl<I> Write<I>
where
    I: AsyncWrite + Unpin,
{
    pub fn poll_write(&mut self, cx: &mut Context<'_>, buffer: &[u8]) -> Poll<BodyResult<usize>> {
        self.inner.poll_write(cx, buffer)
    }

    pub async fn write_all(&mut self, buffer: &[u8]) -> BodyResult<()> {
        self.inner.write_all(buffer).await
    }
}

/// Close state.
#[derive(Debug)]
pub struct Close<I> {
    inner: eof::Close<I>,
}

impl<I> Close<I> {
    pub fn into_ready(self) -> BodyResult<Ready<I>> {
        let Self { inner } = self;
        Ok(Ready {
            inner: inner.into_ready()?,
        })
    }
}

impl<I> Close<I>
where
    I: AsyncWrite + Unpin,
{
    pub fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        self.inner.poll_close(cx)
    }

    pub async fn close(self) -> BodyResult<Ready<I>> {
        let Self { inner } = self;
        Ok(Ready {
            inner: inner.close().await?,
        })
    }
}

/// Finish (terminal) state. Consumes and drops the underlying writer;
/// EOF framing has no on-the-wire terminator and the receiver detects
/// end-of-body by the connection closing.
#[derive(Debug)]
pub struct Finish<I> {
    inner: eof::Finish<I>,
}

impl<I> Finish<I> {
    pub fn discard(self) -> BodyResult<()> {
        let Self { inner } = self;
        inner.discard()
    }
}

impl<I> Finish<I>
where
    I: AsyncWrite + Unpin,
{
    pub fn poll_finish(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        self.inner.poll_finish(cx)
    }

    pub async fn finish(self) -> BodyResult<()> {
        let Self { inner } = self;
        inner.finish().await
    }
}

/// Abort (terminal) state. Consumes and drops the underlying writer
/// after flushing any buffered bytes; EOF framing has no in-band poison.
#[derive(Debug)]
pub struct Abort<I> {
    inner: eof::Abort<I>,
}

impl<I> Abort<I> {
    pub fn discard(self) -> BodyResult<()> {
        let Self { inner } = self;
        inner.discard()
    }
}

impl<I> Abort<I>
where
    I: AsyncWrite + Unpin,
{
    pub fn poll_abort(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        self.inner.poll_abort(cx)
    }

    pub async fn abort(self) -> BodyResult<()> {
        let Self { inner } = self;
        inner.abort().await
    }
}
