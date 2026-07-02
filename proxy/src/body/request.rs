//! Frontend-to-backend request body forwarder.

use std::{
    future::{Future, poll_fn},
    task::{Context, Poll},
};

use bytes::{Buf, Bytes};
use jrpxy_backend::{
    error::BackendError,
    writer::{BackendWriter, body as backend_body},
};
use jrpxy_frontend::reader::{FrontendBodyReader, FrontendReader};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::{ProxyBackendError, ProxyCopyError};

use super::StepOutcome;

#[derive(Default)]
enum BackendBodyWriterState<I> {
    #[default]
    Poisoned,
    Ready(backend_body::Ready<I>),
    Open(backend_body::Open<I>),
    Write(backend_body::Write<I>),
    Close(backend_body::Close<I>),
}

impl<I> BackendBodyWriterState<I> {
    fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

impl<I> BackendBodyWriterState<I>
where
    I: AsyncWriteExt + Unpin,
{
    fn into_ready(self) -> Result<backend_body::Ready<I>, ProxyCopyError> {
        let Self::Ready(ready) = self else {
            return Err(ProxyCopyError::BackendCopyIncomplete);
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
            Poll::Ready(Err(ProxyCopyError::BackendError(
                ProxyBackendError::BackendError(e),
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

/// Forwards a request body from the frontend to the backend.
pub struct RequestBodyForwarder<FR, BW> {
    chunk_size: usize,
    pending: Bytes,
    read_done: bool,
    errored: bool,
    frontend_body_reader: FrontendBodyReader<FR>,
    backend_body_writer_state: BackendBodyWriterState<BW>,
}

impl<FR, BW> RequestBodyForwarder<FR, BW>
where
    FR: AsyncReadExt + Unpin,
    BW: AsyncWriteExt + Unpin,
{
    pub(crate) fn new(
        chunk_size: usize,
        frontend_body_reader: FrontendBodyReader<FR>,
        backend_body_writer: backend_body::Ready<BW>,
    ) -> Self
    where
        FR: AsyncReadExt + Unpin,
        BW: AsyncWriteExt + Unpin,
    {
        Self {
            chunk_size,
            pending: Default::default(),
            read_done: false,
            errored: false,
            frontend_body_reader,
            backend_body_writer_state: BackendBodyWriterState::Ready(backend_body_writer),
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
        // Both the read and the writer step are cancel-safe: `read` does not
        // mutate `pending`/`read_done` until it resolves, and `step_once`
        // restores any in-flight writer phase into `backend_body_writer_state`
        // before returning `Pending`. Cancelling here is therefore safe to
        // retry.
        if self.pending.is_empty() && !self.read_done {
            match self.frontend_body_reader.read(self.chunk_size).await? {
                Some(buf) => self.pending = buf,
                None => self.read_done = true,
            }
            return Ok(false);
        }

        let outcome = poll_fn(|cx| {
            self.backend_body_writer_state
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

    /// Drive the request body forward concurrently with `fut`. Returns `fut`'s
    /// output once it completes, or an error if one occurs first.
    ///
    /// This allows a user to do something like wait for an action to complete
    /// (such as reading a response from the backend) while also continuing to
    /// forward the request body to the backend.
    pub async fn forward_while<F>(&mut self, fut: F) -> Result<F::Output, ProxyCopyError>
    where
        F: Future,
    {
        tokio::pin!(fut);
        let mut body_finished = false;
        loop {
            tokio::select! {
                res = &mut fut => return Ok(res),
                res = self.forward(), if !body_finished => {
                    match res {
                        Ok(false) => {}
                        Ok(true) => body_finished = true,
                        Err(e) => return Err(e),
                    }
                }
            }
        }
    }

    /// Finish the body forwarder. This is used to perform any closing
    /// operations needed by the body (such as writing the empty trailer chunk
    /// as required by chunk-encoded bodies).
    pub async fn finish(
        mut self,
    ) -> Result<(Option<FrontendReader<FR>>, BackendWriter<BW>), ProxyCopyError> {
        while !self.forward().await? {}
        let Self {
            chunk_size: _,
            pending: _,
            read_done: _,
            errored: _,
            frontend_body_reader,
            backend_body_writer_state,
        } = self;
        let backend_body_writer = backend_body_writer_state.into_ready()?;
        match (frontend_body_reader, backend_body_writer) {
            (FrontendBodyReader::TE(fr), backend_body::Ready::Chunked(bw)) => {
                let (next_reader, mut trailers) = fr.drain().await?;
                // RFC 9110 6.5.1: an origin that folds trailers into headers
                // must not be handed framing/routing/auth fields this way.
                crate::sanitize_trailers(&mut trailers);
                let finisher = bw.finish_with_trailers(&trailers);
                let next_backend = finisher.finish().await.map_err(BackendError::from)?;
                Ok((next_reader, next_backend))
            }
            (fr, bw) => {
                let next_reader = fr.drain().await?;
                let finisher = bw.finish()?;
                let next_backend = finisher.finish().await?;
                Ok((next_reader, next_backend))
            }
        }
    }
}
