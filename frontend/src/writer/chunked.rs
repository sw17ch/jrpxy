//! Frontend wrappers around [`jrpxy_body::writer::chunked`]. Each state
//! is a thin newtype over the body-crate state of the same name.

use std::{
    future::poll_fn,
    task::{Context, Poll},
};

use jrpxy_body::{error::BodyResult, writer::chunked};
use jrpxy_http_message::header::Headers;
use tokio::io::AsyncWrite;

use crate::writer::FrontendWriter;

/// Idle state.
#[derive(Debug)]
pub struct Ready<I> {
    inner: chunked::Ready<I>,
}

impl<I> Ready<I> {
    pub(crate) fn new(inner: chunked::Ready<I>) -> Self {
        Self { inner }
    }

    /// Begin a new chunk of `length` bytes. Chunked encoding has no upper
    /// bound on body size, so this is infallible.
    pub fn open(self, length: u64) -> Open<I> {
        let Self { inner } = self;
        Open {
            inner: inner.open(length),
        }
    }

    /// Terminate the body cleanly with no trailers.
    pub fn finish(self) -> Finish<I> {
        let Self { inner } = self;
        Finish {
            inner: inner.finish(),
        }
    }

    /// Terminate the body cleanly with trailer fields.
    pub fn finish_with_trailers(self, trailers: &Headers) -> Finish<I> {
        let Self { inner } = self;
        Finish {
            inner: inner.finish_with_trailers(trailers),
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

/// Open state. Drives the chunk-size header (`{N:x}\r\n`).
#[derive(Debug)]
pub struct Open<I> {
    inner: chunked::Open<I>,
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
    inner: chunked::Write<I>,
}

impl<I> Write<I> {
    /// The number of bytes yet to be written into this chunk.
    pub fn chunk_remaining(&self) -> u64 {
        self.inner.chunk_remaining()
    }

    /// Finish the chunk and move to the next state. Errors if not all bytes
    /// have been written.
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
    /// Poll for completion.
    pub fn poll_write(&mut self, cx: &mut Context<'_>, buffer: &[u8]) -> Poll<BodyResult<usize>> {
        self.inner.poll_write(cx, buffer)
    }

    /// Write the entire `buffer`.
    pub async fn write_all(&mut self, buffer: &[u8]) -> BodyResult<()> {
        self.inner.write_all(buffer).await
    }
}

/// Close state. Drives the trailing `\r\n` for non-empty chunks.
#[derive(Debug)]
pub struct Close<I> {
    inner: chunked::Close<I>,
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

/// Finish (terminal) state. Drives `0\r\n{trailers}\r\n` and the final
/// flush, yielding a [`FrontendWriter`] ready for the next response.
#[derive(Debug)]
pub struct Finish<I> {
    inner: chunked::Finish<I>,
}

impl<I> Finish<I> {
    pub fn into_writer(self) -> BodyResult<FrontendWriter<I>> {
        let Self { inner } = self;
        Ok(FrontendWriter::new(inner.into_writer()?))
    }
}

impl<I> Finish<I>
where
    I: AsyncWrite + Unpin,
{
    pub fn poll_finish(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        self.inner.poll_finish(cx)
    }

    pub async fn finish(self) -> BodyResult<FrontendWriter<I>> {
        let Self { inner } = self;
        Ok(FrontendWriter::new(inner.finish().await?))
    }
}

/// Abort (terminal) state. Writes an invalid chunk header byte and
/// flushes; the writer is consumed and dropped.
#[derive(Debug)]
pub struct Abort<I> {
    inner: chunked::Abort<I>,
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
