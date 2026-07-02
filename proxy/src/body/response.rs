//! Backend-to-frontend response body forwarder.

use std::{
    future::poll_fn,
    task::{Context, Poll},
};

use bytes::{Buf, Bytes};
use jrpxy_backend::reader::{BackendBodyReader, BackendReader};
use jrpxy_frontend::{
    error::FrontendError,
    writer::{FrontendWriter, body as frontend_body},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::{ProxyCopyError, ProxyFrontendError};

use super::StepOutcome;

#[derive(Default)]
enum FrontendBodyWriterState<I> {
    #[default]
    Poisoned,
    Ready(frontend_body::Ready<I>),
    Open(frontend_body::Open<I>),
    Write(frontend_body::Write<I>),
    Close(frontend_body::Close<I>),
}

impl<I> FrontendBodyWriterState<I> {
    fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

impl<I> FrontendBodyWriterState<I>
where
    I: AsyncWriteExt + Unpin,
{
    fn into_ready(self) -> Result<frontend_body::Ready<I>, ProxyCopyError> {
        let Self::Ready(ready) = self else {
            return Err(ProxyCopyError::FrontendCopyIncomplete);
        };
        Ok(ready)
    }

    /// Returns [`StepOutcome::NeedInput`] when the writer is at `Ready`
    /// and `pending` is empty - no more progress can be made until the
    /// caller refills `pending` or transitions out of `Ready` (e.g. by
    /// calling `finish`/`finish_with_trailers` on the extracted `Ready`
    /// writer). Returns [`StepOutcome::Working`] when another call would
    /// make progress.
    fn step_once(
        &mut self,
        cx: &mut Context<'_>,
        pending: &mut Bytes,
    ) -> Poll<Result<StepOutcome, ProxyCopyError>> {
        let ready_err = |e| {
            Poll::Ready(Err(ProxyCopyError::FrontendError(
                ProxyFrontendError::FrontendError(e),
            )))
        };

        if matches!(self, Self::Ready(_)) && pending.is_empty() {
            return Poll::Ready(Ok(StepOutcome::NeedInput));
        }

        let me = self.take();
        match me {
            Self::Poisoned => Poll::Ready(Err(ProxyCopyError::ForwardAfterError)),
            Self::Ready(ready) => {
                let open = match ready.open(pending.len() as u64) {
                    Err(e) => return ready_err(e),
                    Ok(open) => open,
                };
                *self = Self::Open(open);
                Poll::Ready(Ok(StepOutcome::Working))
            }
            Self::Open(mut open) => {
                let write = match open.poll_open(cx) {
                    Poll::Ready(Err(e)) => return ready_err(e),
                    Poll::Pending => {
                        *self = Self::Open(open);
                        return Poll::Pending;
                    }
                    Poll::Ready(Ok(())) => match open.into_write() {
                        Ok(w) => w,
                        Err(e) => return ready_err(e),
                    },
                };
                *self = Self::Write(write);
                Poll::Ready(Ok(StepOutcome::Working))
            }
            Self::Write(mut write) => match write.poll_write(cx, pending) {
                Poll::Ready(Err(e)) => ready_err(e),
                Poll::Ready(Ok(written)) => {
                    pending.advance(written);
                    if pending.is_empty() {
                        let close = match write.into_close() {
                            Err(e) => return ready_err(e),
                            Ok(close) => close,
                        };
                        *self = Self::Close(close);
                    } else {
                        *self = Self::Write(write);
                    }
                    Poll::Ready(Ok(StepOutcome::Working))
                }
                Poll::Pending => {
                    *self = Self::Write(write);
                    Poll::Pending
                }
            },
            Self::Close(mut close) => {
                let ready = match close.poll_close(cx) {
                    Poll::Ready(Err(e)) => return ready_err(e),
                    Poll::Pending => {
                        *self = Self::Close(close);
                        return Poll::Pending;
                    }
                    Poll::Ready(Ok(())) => match close.into_ready() {
                        Err(e) => return ready_err(e),
                        Ok(r) => r,
                    },
                };
                *self = Self::Ready(ready);
                Poll::Ready(Ok(StepOutcome::Working))
            }
        }
    }
}

/// Forward the response body from the backend to the frontend.
pub struct ResponseBodyForwarder<BR, FW> {
    chunk_size: usize,
    pending: Bytes,
    read_done: bool,
    errored: bool,
    backend_body_reader: BackendBodyReader<BR>,
    frontend_body_writer_state: FrontendBodyWriterState<FW>,
}

impl<BR, FW> ResponseBodyForwarder<BR, FW>
where
    BR: AsyncReadExt + Unpin,
    FW: AsyncWriteExt + Unpin,
{
    pub(crate) fn new(
        chunk_size: usize,
        backend_body_reader: BackendBodyReader<BR>,
        frontend_body_writer: frontend_body::Ready<FW>,
    ) -> Self
    where
        FW: AsyncWriteExt + Unpin,
        BR: AsyncReadExt + Unpin,
    {
        Self {
            chunk_size,
            pending: Default::default(),
            read_done: false,
            errored: false,
            backend_body_reader,
            frontend_body_writer_state: FrontendBodyWriterState::Ready(frontend_body_writer),
        }
    }

    /// Perform one forward operation. This is either a buffer read or a buffer
    /// write if a read had already been completed. Returns `true` when the
    /// forwarding is complete.
    ///
    /// Once any call to `forward` returns an error, the forwarder is latched:
    /// every subsequent call returns [`ProxyCopyError::ForwardAfterError`].
    /// Continuing to drive a forwarder past an error would risk writing
    /// partial chunk framing onto a possibly-poisoned connection.
    pub async fn forward(&mut self) -> Result<bool, ProxyCopyError> {
        if self.errored {
            return Err(ProxyCopyError::ForwardAfterError);
        }
        let result = self.forward_inner().await;
        if result.is_err() {
            self.errored = true;
        }
        result
    }

    async fn forward_inner(&mut self) -> Result<bool, ProxyCopyError> {
        // If pending is empty and the body reader still has more, refill.
        // The reader's `read` is cancel-safe and we don't touch the writer
        // state across this await, so a cancel here just leaves
        // `pending`/`read_done` unchanged for the next call.
        if self.pending.is_empty() && !self.read_done {
            match self.backend_body_reader.read(self.chunk_size).await? {
                Some(buf) => self.pending = buf,
                None => self.read_done = true,
            }
            return Ok(false);
        }

        // Drive one writer step. `step_once` is cancel-safe: it restores
        // the in-flight phase into `self.frontend_body_writer_state` before
        // returning `Pending`, and any synchronous `pending.advance` runs
        // after `poll_write` returned `Ready`.
        //
        // Reaching this point means either:
        //   - pending is non-empty (drive `Ready -> Open`, `Open -> Write`,
        //     `Write -> Close|Write`, or `Close -> Ready`); or
        //   - the read side is exhausted and we still need to drain the
        //     writer past an in-flight `Open`/`Write`/`Close` cycle so
        //     the state lands back at `Ready`.
        let outcome = poll_fn(|cx| {
            self.frontend_body_writer_state
                .step_once(cx, &mut self.pending)
        })
        .await?;

        match outcome {
            // Writer is at `Ready` and pending is empty. If the reader is
            // also exhausted the body has been fully forwarded; the caller
            // can extract the `Ready` writer and call `finish` (or
            // `finish_with_trailers`) on it. Otherwise we need another
            // call (which will hit the refill branch above).
            StepOutcome::NeedInput => Ok(self.read_done),
            StepOutcome::Working => Ok(false),
        }
    }

    /// Finish the body forwarder. This is used to perform any closing
    /// operations needed by the body (such as writing the empty trailer chunk
    /// as required by chunk-encoded bodies).
    pub async fn finish(
        mut self,
    ) -> Result<(Option<BackendReader<BR>>, Option<FrontendWriter<FW>>), ProxyCopyError> {
        while !self.forward().await? {}
        let Self {
            chunk_size: _,
            pending: _,
            read_done: _,
            errored: _,
            backend_body_reader,
            frontend_body_writer_state,
        } = self;
        let frontend_body_writer = frontend_body_writer_state.into_ready()?;
        match (backend_body_reader, frontend_body_writer) {
            (BackendBodyReader::TE(fr), frontend_body::Ready::Chunked(bw)) => {
                let (next_reader, mut trailers) = fr.drain().await?;
                // RFC 9110 6.5.1: a client that folds trailers into headers
                // must not be handed framing/routing/auth fields this way.
                crate::sanitize_trailers(&mut trailers);
                let finisher = bw.finish_with_trailers(&trailers);
                let next_frontend = finisher.finish().await.map_err(FrontendError::from)?;
                Ok((next_reader, Some(next_frontend)))
            }
            (fr, bw) => {
                let next_reader = fr.drain().await?;
                let finisher = bw.finish()?;
                let next_frontend = finisher.finish().await?;
                Ok((next_reader, next_frontend))
            }
        }
    }
}
