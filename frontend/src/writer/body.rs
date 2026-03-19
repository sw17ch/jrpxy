//! Encoding-generic body-writer states. Each variant wraps the
//! corresponding per-encoding state in [`bodyless`], [`content_length`],
//! [`chunked`], or [`eof`]. The unifier exposes only the surface common
//! to all four encodings; chunked-only operations such as trailer
//! support remain on [`chunked::Ready`].
//!
//! [`Finish::into_writer`] returns `Option<FrontendWriter<I>>` because
//! the EOF encoding cannot be recycled - the connection must be closed
//! once the body is sent. The other encodings always yield `Some`.

use std::{
    future::poll_fn,
    task::{Context, Poll},
};

use tokio::io::AsyncWrite;

use crate::{
    error::FrontendResult,
    writer::{FrontendWriter, bodyless, chunked, content_length, eof},
};

/// Idle state.
#[derive(Debug)]
pub enum Ready<I> {
    Bodyless(bodyless::Ready<I>),
    CL(content_length::Ready<I>),
    Chunked(chunked::Ready<I>),
    Eof(eof::Ready<I>),
}

impl<I> Ready<I> {
    /// Begin a new chunk of `length` bytes. Errors when the underlying
    /// encoding rejects the requested length.
    pub fn open(self, length: u64) -> FrontendResult<Open<I>> {
        Ok(match self {
            Self::Bodyless(r) => Open::Bodyless(r.open(length)?),
            Self::CL(r) => Open::CL(r.open(length)?),
            Self::Chunked(r) => Open::Chunked(r.open(length)),
            Self::Eof(r) => Open::Eof(r.open(length)),
        })
    }

    /// Terminate the body cleanly. Errors when the underlying encoding
    /// rejects the transition (e.g. content-length with bytes still
    /// owed).
    pub fn finish(self) -> FrontendResult<Finish<I>> {
        Ok(match self {
            Self::Bodyless(r) => Finish::Bodyless(r.finish()),
            Self::CL(r) => Finish::CL(r.finish()?),
            Self::Chunked(r) => Finish::Chunked(r.finish()),
            Self::Eof(r) => Finish::Eof(r.finish()),
        })
    }

    /// Terminate the body abnormally.
    pub fn abort(self) -> Abort<I> {
        match self {
            Self::Bodyless(r) => Abort::Bodyless(r.abort()),
            Self::CL(r) => Abort::CL(r.abort()),
            Self::Chunked(r) => Abort::Chunked(r.abort()),
            Self::Eof(r) => Abort::Eof(r.abort()),
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

impl<I> From<eof::Ready<I>> for Ready<I> {
    fn from(r: eof::Ready<I>) -> Self {
        Self::Eof(r)
    }
}

/// Open state.
#[derive(Debug)]
pub enum Open<I> {
    Bodyless(bodyless::Open<I>),
    CL(content_length::Open<I>),
    Chunked(chunked::Open<I>),
    Eof(eof::Open<I>),
}

impl<I> Open<I> {
    /// Move to the [`Write`] state. Will return an error if not polled to
    /// completion.
    pub fn into_write(self) -> FrontendResult<Write<I>> {
        Ok(match self {
            Self::Bodyless(o) => Write::Bodyless(o.into_write()?),
            Self::CL(o) => Write::CL(o.into_write()?),
            Self::Chunked(o) => Write::Chunked(o.into_write()?),
            Self::Eof(o) => Write::Eof(o.into_write()?),
        })
    }
}

impl<I> Open<I>
where
    I: AsyncWrite + Unpin,
{
    /// Poll for completion.
    pub fn poll_open(&mut self, cx: &mut Context<'_>) -> Poll<FrontendResult<()>> {
        match self {
            Self::Bodyless(o) => o.poll_open(cx).map_err(Into::into),
            Self::CL(o) => o.poll_open(cx).map_err(Into::into),
            Self::Chunked(o) => o.poll_open(cx).map_err(Into::into),
            Self::Eof(o) => o.poll_open(cx).map_err(Into::into),
        }
    }

    /// Writes the necessary preamble, and moves to the next state.
    pub async fn open(mut self) -> FrontendResult<Write<I>> {
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
    Eof(eof::Write<I>),
}

impl<I> Write<I> {
    /// Move to the state needed to close out the written bytes. Returns an
    /// error if the right conditions have not been met for closing, or if there
    /// is a write error.
    pub fn into_close(self) -> FrontendResult<Close<I>> {
        Ok(match self {
            Self::Bodyless(w) => Close::Bodyless(w.into_close()?),
            Self::CL(w) => Close::CL(w.into_close()?),
            Self::Chunked(w) => Close::Chunked(w.into_close()?),
            Self::Eof(w) => Close::Eof(w.into_close()?),
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
    ) -> Poll<FrontendResult<usize>> {
        match self {
            Self::Bodyless(w) => w.poll_write(cx, buffer).map_err(Into::into),
            Self::CL(w) => w.poll_write(cx, buffer).map_err(Into::into),
            Self::Chunked(w) => w.poll_write(cx, buffer).map_err(Into::into),
            Self::Eof(w) => w.poll_write(cx, buffer).map_err(Into::into),
        }
    }

    /// Write the entire `buffer`.
    pub async fn write_all(&mut self, buffer: &[u8]) -> FrontendResult<()> {
        match self {
            Self::Bodyless(w) => Ok(w.write_all(buffer).await?),
            Self::CL(w) => Ok(w.write_all(buffer).await?),
            Self::Chunked(w) => Ok(w.write_all(buffer).await?),
            Self::Eof(w) => Ok(w.write_all(buffer).await?),
        }
    }
}

/// Close state.
#[derive(Debug)]
pub enum Close<I> {
    Bodyless(bodyless::Close<I>),
    CL(content_length::Close<I>),
    Chunked(chunked::Close<I>),
    Eof(eof::Close<I>),
}

impl<I> Close<I> {
    /// Move back to the [`Ready`] state. Returns an error if not polled to
    /// completion.
    pub fn into_ready(self) -> FrontendResult<Ready<I>> {
        Ok(match self {
            Self::Bodyless(c) => Ready::Bodyless(c.into_ready()?),
            Self::CL(c) => Ready::CL(c.into_ready()?),
            Self::Chunked(c) => Ready::Chunked(c.into_ready()?),
            Self::Eof(c) => Ready::Eof(c.into_ready()?),
        })
    }
}

impl<I> Close<I>
where
    I: AsyncWrite + Unpin,
{
    /// Poll for completion.
    pub fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<FrontendResult<()>> {
        match self {
            Self::Bodyless(c) => c.poll_close(cx).map_err(Into::into),
            Self::CL(c) => c.poll_close(cx).map_err(Into::into),
            Self::Chunked(c) => c.poll_close(cx).map_err(Into::into),
            Self::Eof(c) => c.poll_close(cx).map_err(Into::into),
        }
    }

    /// Write the needed bytes to commit the previously written bytes to the
    /// stream and return the next [`Ready`] state.
    pub async fn close(mut self) -> FrontendResult<Ready<I>> {
        poll_fn(|cx| self.poll_close(cx)).await?;
        self.into_ready()
    }
}

/// Finish (terminal) state. Yields `Some(FrontendWriter)` for encodings
/// that can be recycled, and `None` for the EOF encoding (where the
/// connection must be closed after the body is sent).
#[derive(Debug)]
pub enum Finish<I> {
    Bodyless(bodyless::Finish<I>),
    CL(content_length::Finish<I>),
    Chunked(chunked::Finish<I>),
    Eof(eof::Finish<I>),
}

impl<I> Finish<I> {
    /// Consume the [`Finish`] state and, if possible, return the original
    /// [`FrontendWriter`].
    pub fn into_writer(self) -> FrontendResult<Option<FrontendWriter<I>>> {
        Ok(match self {
            Self::Bodyless(f) => Some(f.into_writer()?),
            Self::CL(f) => Some(f.into_writer()?),
            Self::Chunked(f) => Some(f.into_writer()?),
            Self::Eof(f) => {
                f.discard()?;
                None
            }
        })
    }
}

impl<I> Finish<I>
where
    I: AsyncWrite + Unpin,
{
    /// Poll for completion.
    pub fn poll_finish(&mut self, cx: &mut Context<'_>) -> Poll<FrontendResult<()>> {
        match self {
            Self::Bodyless(f) => f.poll_finish(cx).map_err(Into::into),
            Self::CL(f) => f.poll_finish(cx).map_err(Into::into),
            Self::Chunked(f) => f.poll_finish(cx).map_err(Into::into),
            Self::Eof(f) => f.poll_finish(cx).map_err(Into::into),
        }
    }

    /// Write the bytes needed to finish the body, and, if possible, return the
    /// original [`FrontendWriter`].
    pub async fn finish(mut self) -> FrontendResult<Option<FrontendWriter<I>>> {
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
    Eof(eof::Abort<I>),
}

impl<I> Abort<I> {
    /// Discard the [`Abort`]. An error is returned if not already polled to
    /// completion, or if an underlying error happened when writing.
    pub fn discard(self) -> FrontendResult<()> {
        let _: () = match self {
            Self::Bodyless(a) => a.discard()?,
            Self::CL(a) => a.discard()?,
            Self::Chunked(a) => a.discard()?,
            Self::Eof(a) => a.discard()?,
        };
        Ok(())
    }
}

impl<I> Abort<I>
where
    I: AsyncWrite + Unpin,
{
    /// Poll for completion.
    pub fn poll_abort(&mut self, cx: &mut Context<'_>) -> Poll<FrontendResult<()>> {
        match self {
            Self::Bodyless(a) => a.poll_abort(cx).map_err(Into::into),
            Self::CL(a) => a.poll_abort(cx).map_err(Into::into),
            Self::Chunked(a) => a.poll_abort(cx).map_err(Into::into),
            Self::Eof(a) => a.poll_abort(cx).map_err(Into::into),
        }
    }

    /// Write the necessary bytes to abort, and then discard the writer.
    pub async fn abort(mut self) -> FrontendResult<()> {
        poll_fn(|cx| self.poll_abort(cx)).await?;
        self.discard()
    }
}
