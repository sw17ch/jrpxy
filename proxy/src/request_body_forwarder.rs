use std::{
    future::poll_fn,
    task::{Context, Poll},
};

use jrpxy_body::{reader::ChunkHeadReader, writer::chunked::IdleWriter};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    backend_writer::{ProxyBackendBodyWriter, ProxyBackendBodyWriterKind},
    body_forwarder::{
        BodyForwarder, ChunkToChunkBodyForwarder, ChunkToOtherBodyForwarder, OtherBodyReader,
        OtherBodyWriter, OtherToChunkedBodyForwarder, OtherToOtherBodyForwarder,
    },
    error::{ProxyRequestBodyForwarderError, ProxyRequestBodyForwarderResult},
    frontend_reader::{
        FrontendChunkedState, ProxyFrontendBodyReader, ProxyFrontendChunkedBodyReader,
    },
};

/// Reader normalized to either a raw chunked head reader or the generic
/// non-chunked reader, collapsing the three frontend framing variants into two
/// cases.
enum NormalizedReader<R> {
    Chunked(ChunkHeadReader<R>),
    Other(OtherBodyReader<R>),
}

/// Writer normalized to either a chunked idle writer or the generic
/// non-chunked writer, collapsing the three backend framing variants into two
/// cases.
enum NormalizedWriter<W> {
    Chunked(IdleWriter<W>),
    Other(OtherBodyWriter<W>),
}

/// Forwards a request body from a [`ProxyFrontendBodyReader`] to a
/// [`ProxyBackendBodyWriter`] by dispatching to the appropriate
/// [`BodyForwarder`] variant.
pub struct ProxyRequestBodyForwarder<R, W> {
    inner: BodyForwarder<R, W>,
}

impl<R, W> ProxyRequestBodyForwarder<R, W> {
    pub(crate) fn new(
        reader: ProxyFrontendBodyReader<R>,
        writer: ProxyBackendBodyWriter<W>,
    ) -> ProxyRequestBodyForwarderResult<Self> {
        let reader = normalize_reader(reader)?;
        let writer = normalize_writer(writer);

        let inner = match (reader, writer) {
            (NormalizedReader::Chunked(r), NormalizedWriter::Chunked(w)) => {
                ChunkToChunkBodyForwarder::new(r, w).into()
            }
            (NormalizedReader::Chunked(r), NormalizedWriter::Other(w)) => {
                ChunkToOtherBodyForwarder::new(r, w).into()
            }
            (NormalizedReader::Other(r), NormalizedWriter::Chunked(w)) => {
                OtherToChunkedBodyForwarder::new(r, w).into()
            }
            (NormalizedReader::Other(r), NormalizedWriter::Other(w)) => {
                OtherToOtherBodyForwarder::new(r, w).into()
            }
        };

        Ok(Self { inner })
    }

    /// Recover the underlying backend writer after [`poll_forward`](Self::poll_forward)
    /// returns `Ready(Ok(()))`. Panics if forwarding has not completed.
    pub fn into_writer(self) -> Option<W> {
        let Self { inner } = self;
        inner.into_writer()
    }
}

impl<R, W> ProxyRequestBodyForwarder<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub fn poll_forward(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<ProxyRequestBodyForwarderResult<()>> {
        match self.inner.poll_forward(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(ProxyRequestBodyForwarderError::BodyError(e))),
        }
    }

    pub async fn forward(mut self) -> ProxyRequestBodyForwarderResult<Option<W>> {
        poll_fn(|cx| self.poll_forward(cx)).await?;
        Ok(self.into_writer())
    }
}

fn normalize_reader<R>(
    reader: ProxyFrontendBodyReader<R>,
) -> ProxyRequestBodyForwarderResult<NormalizedReader<R>> {
    Ok(match reader {
        ProxyFrontendBodyReader::TE(r) => NormalizedReader::Chunked(extract_chunk_head(r)?),
        ProxyFrontendBodyReader::CL(r) => {
            NormalizedReader::Other(OtherBodyReader::ContentLength(r.inner))
        }
        ProxyFrontendBodyReader::Bodyless(r) => {
            NormalizedReader::Other(OtherBodyReader::Bodyless(r.inner))
        }
    })
}

fn normalize_writer<W>(writer: ProxyBackendBodyWriter<W>) -> NormalizedWriter<W> {
    match writer.kind {
        ProxyBackendBodyWriterKind::TE(w) => NormalizedWriter::Chunked(w),
        ProxyBackendBodyWriterKind::CL(w) => {
            NormalizedWriter::Other(OtherBodyWriter::ContentLength(w))
        }
        ProxyBackendBodyWriterKind::Bodyless(w) => {
            NormalizedWriter::Other(OtherBodyWriter::Bodyless(w))
        }
    }
}

// TODO: i really don't like how this can be in the wrong state when we try to
// start. not the end of the world, but it really doesn't play nice with the
// no-invalid-states stuff i'm trying to do.
fn extract_chunk_head<R>(
    reader: ProxyFrontendChunkedBodyReader<R>,
) -> ProxyRequestBodyForwarderResult<ChunkHeadReader<R>> {
    let ProxyFrontendChunkedBodyReader { state } = reader;
    match state {
        Some(FrontendChunkedState::Between(r)) => Ok(r),
        Some(_) => Err(ProxyRequestBodyForwarderError::ChunkedNotAtBoundary),
        None => Err(ProxyRequestBodyForwarderError::ChunkedInErrorState),
    }
}
