//! Encoding-generic body-writer states. Each variant wraps the
//! corresponding per-encoding state in [`bodyless`], [`content_length`],
//! or [`chunked`]. The unifier exposes only the surface common to all
//! three encodings; chunked-only operations such as trailer support
//! remain on [`chunked::Ready`].

use std::{
    future::poll_fn,
    task::{Context, Poll},
};

use tokio::io::AsyncWrite;

use crate::{
    error::BackendResult,
    writer::{BackendWriter, bodyless, chunked, content_length},
};

/// Idle state.
#[derive(Debug)]
pub enum Ready<I> {
    Bodyless(bodyless::Ready<I>),
    CL(content_length::Ready<I>),
    Chunked(chunked::Ready<I>),
}

impl<I> Ready<I> {
    /// Begin a new chunk of `length` bytes. Errors when the underlying
    /// encoding rejects the requested length.
    pub fn open(self, length: u64) -> BackendResult<Open<I>> {
        Ok(match self {
            Self::Bodyless(r) => Open::Bodyless(r.open(length)?),
            Self::CL(r) => Open::CL(r.open(length)?),
            Self::Chunked(r) => Open::Chunked(r.open(length)),
        })
    }

    /// Terminate the body cleanly. Errors when the underlying encoding
    /// rejects the transition (e.g. content-length with bytes still
    /// owed).
    pub fn finish(self) -> BackendResult<Finish<I>> {
        Ok(match self {
            Self::Bodyless(r) => Finish::Bodyless(r.finish()),
            Self::CL(r) => Finish::CL(r.finish()?),
            Self::Chunked(r) => Finish::Chunked(r.finish()),
        })
    }

    /// Terminate the body abnormally.
    pub fn abort(self) -> Abort<I> {
        match self {
            Self::Bodyless(r) => Abort::Bodyless(r.abort()),
            Self::CL(r) => Abort::CL(r.abort()),
            Self::Chunked(r) => Abort::Chunked(r.abort()),
        }
    }
}

impl<I> From<bodyless::Ready<I>> for Ready<I> {
    fn from(r: bodyless::Ready<I>) -> Self {
        Self::Bodyless(r)
    }
}

impl<I> From<content_length::Ready<I>> for Ready<I> {
    fn from(r: content_length::Ready<I>) -> Self {
        Self::CL(r)
    }
}

impl<I> From<chunked::Ready<I>> for Ready<I> {
    fn from(r: chunked::Ready<I>) -> Self {
        Self::Chunked(r)
    }
}

/// Open state.
#[derive(Debug)]
pub enum Open<I> {
    Bodyless(bodyless::Open<I>),
    CL(content_length::Open<I>),
    Chunked(chunked::Open<I>),
}

impl<I> Open<I> {
    /// Move to the [`Write`] state. Will return an error if not polled to
    /// completion.
    pub fn into_write(self) -> BackendResult<Write<I>> {
        Ok(match self {
            Self::Bodyless(o) => Write::Bodyless(o.into_write()?),
            Self::CL(o) => Write::CL(o.into_write()?),
            Self::Chunked(o) => Write::Chunked(o.into_write()?),
        })
    }
}

impl<I> Open<I>
where
    I: AsyncWrite + Unpin,
{
    /// Poll for completion.
    pub fn poll_open(&mut self, cx: &mut Context<'_>) -> Poll<BackendResult<()>> {
        match self {
            Self::Bodyless(o) => o.poll_open(cx).map_err(Into::into),
            Self::CL(o) => o.poll_open(cx).map_err(Into::into),
            Self::Chunked(o) => o.poll_open(cx).map_err(Into::into),
        }
    }

    /// Writes the necessary preamble, and moves to the next state.
    pub async fn open(mut self) -> BackendResult<Write<I>> {
        poll_fn(|cx| self.poll_open(cx)).await?;
        self.into_write()
    }
}

/// Write state.
#[derive(Debug)]
pub enum Write<I> {
    Bodyless(bodyless::Write<I>),
    CL(content_length::Write<I>),
    Chunked(chunked::Write<I>),
}

impl<I> Write<I> {
    /// Move to the state needed to close out the written bytes. Returns an
    /// error if the right conditions have not been met for closing, or if there
    /// is a write error.
    pub fn into_close(self) -> BackendResult<Close<I>> {
        Ok(match self {
            Self::Bodyless(w) => Close::Bodyless(w.into_close()?),
            Self::CL(w) => Close::CL(w.into_close()?),
            Self::Chunked(w) => Close::Chunked(w.into_close()?),
        })
    }
}

impl<I> Write<I>
where
    I: AsyncWrite + Unpin,
{
    /// Write `buffer`. Returns the number of bytes successfully written.
    pub fn poll_write(
        &mut self,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<BackendResult<usize>> {
        match self {
            Self::Bodyless(w) => w.poll_write(cx, buffer).map_err(Into::into),
            Self::CL(w) => w.poll_write(cx, buffer).map_err(Into::into),
            Self::Chunked(w) => w.poll_write(cx, buffer).map_err(Into::into),
        }
    }

    /// Write the entire `buffer`.
    pub async fn write_all(&mut self, buffer: &[u8]) -> BackendResult<()> {
        match self {
            Self::Bodyless(w) => Ok(w.write_all(buffer).await?),
            Self::CL(w) => Ok(w.write_all(buffer).await?),
            Self::Chunked(w) => Ok(w.write_all(buffer).await?),
        }
    }
}

/// Close state.
#[derive(Debug)]
pub enum Close<I> {
    Bodyless(bodyless::Close<I>),
    CL(content_length::Close<I>),
    Chunked(chunked::Close<I>),
}

impl<I> Close<I> {
    /// Move back to the [`Ready`] state. Returns an error if not polled to
    /// completion.
    pub fn into_ready(self) -> BackendResult<Ready<I>> {
        Ok(match self {
            Self::Bodyless(c) => Ready::Bodyless(c.into_ready()?),
            Self::CL(c) => Ready::CL(c.into_ready()?),
            Self::Chunked(c) => Ready::Chunked(c.into_ready()?),
        })
    }
}

impl<I> Close<I>
where
    I: AsyncWrite + Unpin,
{
    /// Poll for completion.
    pub fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<BackendResult<()>> {
        match self {
            Self::Bodyless(c) => c.poll_close(cx).map_err(Into::into),
            Self::CL(c) => c.poll_close(cx).map_err(Into::into),
            Self::Chunked(c) => c.poll_close(cx).map_err(Into::into),
        }
    }

    /// Write the needed bytes to commit the previously written bytes to the
    /// stream and return the next [`Ready`] state.
    pub async fn close(mut self) -> BackendResult<Ready<I>> {
        poll_fn(|cx| self.poll_close(cx)).await?;
        self.into_ready()
    }
}

/// Finish (terminal) state. Yields a [`BackendWriter`] once the
/// underlying writer is flushed.
#[derive(Debug)]
pub enum Finish<I> {
    Bodyless(bodyless::Finish<I>),
    CL(content_length::Finish<I>),
    Chunked(chunked::Finish<I>),
}

impl<I> Finish<I> {
    /// Consume the [`Finish`] state and return the original [`BackendWriter`].
    pub fn into_writer(self) -> BackendResult<BackendWriter<I>> {
        Ok(match self {
            Self::Bodyless(f) => f.into_writer()?,
            Self::CL(f) => f.into_writer()?,
            Self::Chunked(f) => f.into_writer()?,
        })
    }
}

impl<I> Finish<I>
where
    I: AsyncWrite + Unpin,
{
    /// Poll for completion.
    pub fn poll_finish(&mut self, cx: &mut Context<'_>) -> Poll<BackendResult<()>> {
        match self {
            Self::Bodyless(f) => f.poll_finish(cx).map_err(Into::into),
            Self::CL(f) => f.poll_finish(cx).map_err(Into::into),
            Self::Chunked(f) => f.poll_finish(cx).map_err(Into::into),
        }
    }

    /// Write the bytes needed to finish the body, and return the original
    /// [`BackendWriter`].
    pub async fn finish(mut self) -> BackendResult<BackendWriter<I>> {
        poll_fn(|cx| self.poll_finish(cx)).await?;
        self.into_writer()
    }
}

/// Abort (terminal) state.
#[derive(Debug)]
pub enum Abort<I> {
    Bodyless(bodyless::Abort<I>),
    CL(content_length::Abort<I>),
    Chunked(chunked::Abort<I>),
}

impl<I> Abort<I> {
    /// Discard the [`Abort`]. An error is returned if not already polled to
    /// completion, or if an underlying error happened when writing.
    pub fn discard(self) -> BackendResult<()> {
        let _: () = match self {
            Self::Bodyless(a) => a.discard()?,
            Self::CL(a) => a.discard()?,
            Self::Chunked(a) => a.discard()?,
        };
        Ok(())
    }
}

impl<I> Abort<I>
where
    I: AsyncWrite + Unpin,
{
    /// Poll for completion.
    pub fn poll_abort(&mut self, cx: &mut Context<'_>) -> Poll<BackendResult<()>> {
        match self {
            Self::Bodyless(a) => a.poll_abort(cx).map_err(Into::into),
            Self::CL(a) => a.poll_abort(cx).map_err(Into::into),
            Self::Chunked(a) => a.poll_abort(cx).map_err(Into::into),
        }
    }

    /// Write the necessary bytes to abort, and then discard the writer.
    pub async fn abort(mut self) -> BackendResult<()> {
        poll_fn(|cx| self.poll_abort(cx)).await?;
        self.discard()
    }
}
