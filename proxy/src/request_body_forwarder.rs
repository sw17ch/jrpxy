//! Proxy-level wrapper around the body-crate forwarders used to pump the
//! request body from the frontend to the backend while the proxy waits for
//! the backend response head.
//!
//! The wrapper dispatches across the four body-crate forwarder shapes
//! (C2C, C2O, O2C, O2O) and exposes a [`ProxyRequestBodyForwarder::into_parts`]
//! method that produces a [`FrontendReader`] and [`BackendWriter`] ready for
//! the next stage of the proxy pipeline, hiding the four distinct `Done`
//! state shapes the body crate exposes.
//!
//! `poll_forward` is idempotent once the forwarder has completed: repolling
//! a `Done` forwarder returns `Poll::Ready(Ok(()))` immediately. This means
//! callers can pass the forwarder into the next stage of the proxy pipeline
//! (e.g. the response-body `BodyExchanger`) and have it be polled a second
//! time without special handling.

use std::task::{Context, Poll};

use jrpxy_body::{
    error::BodyResult,
    forwarder::{
        ChunkToChunkBodyForwarder, ChunkToOtherBodyForwarder, OtherBodyReader, OtherBodyWriter,
        OtherToChunkedBodyForwarder, OtherToOtherBodyForwarder,
    },
    reader::ChunkHeadReader,
    writer::chunked::IdleWriter,
};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::backend::writer::{BackendBodyWriter, BackendWriter};
use crate::frontend::reader::{FrontendBodyReader, FrontendReader};

/// Pumps the frontend request body to the backend while the proxy waits for
/// the backend response head. Completion is idempotent: polling after the
/// pump has finished returns `Poll::Ready(Ok(()))` without advancing state.
pub struct ProxyRequestBodyForwarder<R, W> {
    inner: Inner<R, W>,
    /// Set once the inner forwarder returns `Poll::Ready(Ok(()))`. Subsequent
    /// polls short-circuit here so the wrapper remains idempotent at Done.
    done: bool,
}

enum Inner<R, W> {
    C2C(ChunkToChunkBodyForwarder<R, W>),
    C2O(ChunkToOtherBodyForwarder<R, W>),
    O2C(OtherToChunkedBodyForwarder<R, W>),
    O2O(OtherToOtherBodyForwarder<R, W>),
}

impl<R, W> ProxyRequestBodyForwarder<R, W> {
    /// Construct a forwarder that pumps a chunked frontend body into a
    /// chunked backend body (chunk framing preserved on both sides).
    pub(crate) fn new_c2c(reader: ChunkHeadReader<R>, writer: IdleWriter<W>) -> Self {
        Self {
            inner: Inner::C2C(ChunkToChunkBodyForwarder::new(reader, writer)),
            done: false,
        }
    }

    /// Construct a forwarder that pumps a chunked frontend body into a
    /// non-chunked backend body (content-length or EOF on the backend side).
    pub(crate) fn new_c2o(reader: ChunkHeadReader<R>, writer: OtherBodyWriter<W>) -> Self {
        Self {
            inner: Inner::C2O(ChunkToOtherBodyForwarder::new(reader, writer)),
            done: false,
        }
    }

    /// Construct a forwarder that pumps a non-chunked frontend body into a
    /// chunked backend body.
    pub(crate) fn new_o2c(reader: OtherBodyReader<R>, writer: IdleWriter<W>) -> Self {
        Self {
            inner: Inner::O2C(OtherToChunkedBodyForwarder::new(reader, writer)),
            done: false,
        }
    }

    /// Construct a forwarder that pumps a non-chunked frontend body into a
    /// non-chunked backend body.
    pub(crate) fn new_o2o(reader: OtherBodyReader<R>, writer: OtherBodyWriter<W>) -> Self {
        Self {
            inner: Inner::O2O(OtherToOtherBodyForwarder::new(reader, writer)),
            done: false,
        }
    }

    /// Build a forwarder for a frontend reader and backend writer. Any
    /// combination of content-length, chunked, and bodyless reader is
    /// supported against any content-length, chunked, or bodyless writer. A
    /// bodyless reader is at end-of-body immediately, and a bodyless writer
    /// rejects any non-empty body read with
    /// [`jrpxy_body::error::BodyError::BodyOverflow`].
    ///
    /// # Panics
    ///
    /// Panics if the chunked reader has already consumed chunk bytes,
    /// breaking the forwarder's requirement to start at the initial chunk
    /// head.
    pub(crate) fn build(reader: FrontendBodyReader<R>, writer: BackendBodyWriter<W>) -> Self {
        match (reader, writer) {
            (FrontendBodyReader::Bodyless(r), BackendBodyWriter::Bodyless(w)) => {
                let reader = OtherBodyReader::Bodyless(r.into_inner());
                let writer = OtherBodyWriter::Bodyless(w.into_inner());
                Self::new_o2o(reader, writer)
            }
            (FrontendBodyReader::Bodyless(r), BackendBodyWriter::CL(w)) => {
                let reader = OtherBodyReader::Bodyless(r.into_inner());
                let writer = OtherBodyWriter::ContentLength(w.into_inner());
                Self::new_o2o(reader, writer)
            }
            (FrontendBodyReader::Bodyless(r), BackendBodyWriter::TE(idle)) => {
                let reader = OtherBodyReader::Bodyless(r.into_inner());
                let idle = idle
                    .expect("chunked backend body writer was taken")
                    .into_inner();
                Self::new_o2c(reader, idle)
            }
            (FrontendBodyReader::CL(r), BackendBodyWriter::CL(w)) => {
                let reader = OtherBodyReader::ContentLength(r.into_inner());
                let writer = OtherBodyWriter::ContentLength(w.into_inner());
                Self::new_o2o(reader, writer)
            }
            (FrontendBodyReader::CL(r), BackendBodyWriter::TE(idle)) => {
                let reader = OtherBodyReader::ContentLength(r.into_inner());
                let idle = idle
                    .expect("chunked backend body writer was taken")
                    .into_inner();
                Self::new_o2c(reader, idle)
            }
            (FrontendBodyReader::TE(r), BackendBodyWriter::CL(w)) => {
                let head = r
                    .into_inner()
                    .into_head_reader()
                    .expect("chunked frontend reader must be at the initial chunk boundary");
                let writer = OtherBodyWriter::ContentLength(w.into_inner());
                Self::new_c2o(head, writer)
            }
            (FrontendBodyReader::TE(r), BackendBodyWriter::TE(idle)) => {
                let head = r
                    .into_inner()
                    .into_head_reader()
                    .expect("chunked frontend reader must be at the initial chunk boundary");
                let idle = idle
                    .expect("chunked backend body writer was taken")
                    .into_inner();
                Self::new_c2c(head, idle)
            }
            (FrontendBodyReader::CL(r), BackendBodyWriter::Bodyless(w)) => {
                let reader = OtherBodyReader::ContentLength(r.into_inner());
                let writer = OtherBodyWriter::Bodyless(w.into_inner());
                Self::new_o2o(reader, writer)
            }
            (FrontendBodyReader::TE(r), BackendBodyWriter::Bodyless(w)) => {
                let head = r
                    .into_inner()
                    .into_head_reader()
                    .expect("chunked frontend reader must be at the initial chunk boundary");
                let writer = OtherBodyWriter::Bodyless(w.into_inner());
                Self::new_c2o(head, writer)
            }
        }
    }
}

impl<R, W> ProxyRequestBodyForwarder<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Drive the body pump. Returns `Poll::Ready(Ok(()))` once the full
    /// request body has been forwarded; subsequent calls remain ready.
    pub fn poll_forward(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        if self.done {
            return Poll::Ready(Ok(()));
        }
        let res = match &mut self.inner {
            Inner::C2C(f) => f.poll_forward(cx),
            Inner::C2O(f) => f.poll_forward(cx),
            Inner::O2C(f) => f.poll_forward(cx),
            Inner::O2O(f) => f.poll_forward(cx),
        };
        if matches!(res, Poll::Ready(Ok(()))) {
            self.done = true;
        }
        res
    }

    /// Decompose a fully-completed forwarder into a [`FrontendReader`] ready
    /// to read the next pipelined request and a [`BackendWriter`] ready to
    /// reuse the backend connection.
    ///
    /// Each side is returned as an [`Option`]. A side is `None` when the
    /// corresponding endpoint was EOF-framed: EOF framing consumes the
    /// underlying IO to signal end-of-body, so no reusable reader or writer
    /// remains for that side.
    ///
    /// # Panics
    ///
    /// Panics if [`poll_forward`] has not yet returned `Poll::Ready(Ok(()))`.
    pub fn into_parts(self) -> (Option<FrontendReader<R>>, Option<BackendWriter<W>>) {
        let Self { inner, done: _ } = self;
        match inner {
            Inner::C2C(f) => {
                let ((reader, parse_slots, _trailers), writer) = f.into_parts();
                (
                    Some(FrontendReader::from_parts(reader, parse_slots)),
                    Some(BackendWriter::from_writer(writer)),
                )
            }
            Inner::C2O(f) => {
                let ((reader, parse_slots, _trailers), writer) = f.into_parts();
                (
                    Some(FrontendReader::from_parts(reader, parse_slots)),
                    writer.map(BackendWriter::from_writer),
                )
            }
            Inner::O2C(f) => {
                let ((parse_slots, reader), writer) = f.into_parts();
                (
                    reader.map(|r| FrontendReader::from_parts(r, parse_slots)),
                    Some(BackendWriter::from_writer(writer)),
                )
            }
            Inner::O2O(f) => {
                let ((parse_slots, reader), writer) = f.into_parts();
                (
                    reader.map(|r| FrontendReader::from_parts(r, parse_slots)),
                    writer.map(BackendWriter::from_writer),
                )
            }
        }
    }
}
