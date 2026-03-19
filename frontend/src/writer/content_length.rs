//! Frontend wrappers around [`jrpxy_body::writer::content_length`].
//! Each state is a thin newtype over the body-crate state

use std::{
    future::poll_fn,
    task::{Context, Poll},
};

use jrpxy_body::{error::BodyResult, writer::content_length};
use tokio::io::AsyncWrite;

use crate::writer::FrontendWriter;

/// Idle state.
#[derive(Debug)]
pub struct Ready<I> {
    inner: content_length::Ready<I>,
}

impl<I> Ready<I> {
    pub(crate) fn new(inner: content_length::Ready<I>) -> Self {
        Self { inner }
    }

    /// Bytes still owed against the declared content length.
    pub fn remaining(&self) -> u64 {
        self.inner.remaining()
    }

    /// Begin a new chunk of `length` bytes. Errors when the requested
    /// length would push the running offset past the declared total.
    pub fn open(self, length: u64) -> BodyResult<Open<I>> {
        let Self { inner } = self;
        Ok(Open {
            inner: inner.open(length)?,
        })
    }

    /// Terminate the body cleanly. Errors if the declared length has not
    /// been fully written.
    pub fn finish(self) -> BodyResult<Finish<I>> {
        let Self { inner } = self;
        Ok(Finish {
            inner: inner.finish()?,
        })
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
    inner: content_length::Open<I>,
}

impl<I> Open<I> {
    pub fn poll_open(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        self.inner.poll_open(cx)
    }

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
    pub async fn open(mut self) -> BodyResult<Write<I>> {
        poll_fn(|cx| self.poll_open(cx)).await?;
        self.into_write()
    }
}

/// Write state.
#[derive(Debug)]
pub struct Write<I> {
    inner: content_length::Write<I>,
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
    inner: content_length::Close<I>,
}

impl<I> Close<I> {
    pub fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        self.inner.poll_close(cx)
    }

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
    pub async fn close(self) -> BodyResult<Ready<I>> {
        let Self { inner } = self;
        Ok(Ready {
            inner: inner.close().await?,
        })
    }
}

/// Finish (terminal) state. Yields a [`FrontendWriter`] ready for the
/// next response once the underlying writer is flushed.
#[derive(Debug)]
pub struct Finish<I> {
    inner: content_length::Finish<I>,
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

/// Abort (terminal) state. Consumes the writer; the receiver will detect
/// a short body via byte count.
#[derive(Debug)]
pub struct Abort<I> {
    inner: content_length::Abort<I>,
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
