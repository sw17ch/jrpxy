use bytes::{Bytes, BytesMut};
use jrpxy_http_message::message::ParseSlots;
use jrpxy_util::io_buffer::IoBuffer;
use tokio::io::AsyncReadExt;

use super::{
    BodyResult, BodylessBodyReader, ChunkedBodyReader, ContentLengthBodyReader, EofBodyReader,
};

enum PeekableInner<I> {
    Bodyless(BodylessBodyReader<I>),
    CL(ContentLengthBodyReader<I>),
    TE(ChunkedBodyReader<I>),
    Eof(EofBodyReader<I>),
}

impl<I: AsyncReadExt + Unpin> PeekableInner<I> {
    async fn read(&mut self, max_len: usize) -> BodyResult<Option<Bytes>> {
        match self {
            PeekableInner::Bodyless(_) => Ok(None),
            PeekableInner::CL(r) => r.read(max_len).await,
            PeekableInner::TE(r) => r.read(max_len).await,
            PeekableInner::Eof(r) => r.read(max_len).await,
        }
    }

    fn drained(&self) -> bool {
        match self {
            PeekableInner::Bodyless(_) => true,
            PeekableInner::CL(r) => r.drained(),
            PeekableInner::TE(r) => r.drained(),
            PeekableInner::Eof(r) => r.drained(),
        }
    }

    async fn drain(self) -> BodyResult<(IoBuffer<I>, ParseSlots)> {
        match self {
            PeekableInner::Bodyless(r) => Ok(r.drain()),
            PeekableInner::CL(r) => r.drain().await,
            PeekableInner::TE(r) => {
                let (io, parse_slots, _trailers) = r.drain().await?;
                Ok((io, parse_slots))
            }
            PeekableInner::Eof(r) => r.drain().await,
        }
    }
}

pub struct PeekableBodyReader<I> {
    /// a buffer in which we store data peeked off the socket, but not yet read
    peeked: BytesMut,
    inner: PeekableInner<I>,
}

impl<I> From<BodylessBodyReader<I>> for PeekableBodyReader<I> {
    fn from(r: BodylessBodyReader<I>) -> Self {
        Self {
            peeked: BytesMut::new(),
            inner: PeekableInner::Bodyless(r),
        }
    }
}

impl<I> From<ContentLengthBodyReader<I>> for PeekableBodyReader<I> {
    fn from(r: ContentLengthBodyReader<I>) -> Self {
        Self {
            peeked: BytesMut::new(),
            inner: PeekableInner::CL(r),
        }
    }
}

impl<I> From<ChunkedBodyReader<I>> for PeekableBodyReader<I> {
    fn from(r: ChunkedBodyReader<I>) -> Self {
        Self {
            peeked: BytesMut::new(),
            inner: PeekableInner::TE(r),
        }
    }
}

impl<I> From<EofBodyReader<I>> for PeekableBodyReader<I> {
    fn from(r: EofBodyReader<I>) -> Self {
        Self {
            peeked: BytesMut::new(),
            inner: PeekableInner::Eof(r),
        }
    }
}

impl<I: AsyncReadExt + Unpin> PeekableBodyReader<I> {
    /// Peek data from the body, and allow it to be observed, but do not remove
    /// it from the read buffer. Always returns data starting from the first
    /// byte not yet read. That is, repeated calls to this method with the same
    /// length will always yield the same result unless read calls are
    /// interspersed with peek calls.
    pub async fn peek(&mut self, target_len: usize) -> BodyResult<(bool, &[u8])> {
        let mut remaining_len = target_len.saturating_sub(self.peeked.len());
        loop {
            let complete = match self.inner.read(remaining_len).await? {
                None => true,
                Some(bytes) => {
                    self.peeked.extend_from_slice(&bytes);
                    self.inner.drained()
                }
            };

            remaining_len = target_len.saturating_sub(self.peeked.len());
            if !complete && remaining_len > 0 {
                continue;
            }

            let len = self.peeked.len().min(target_len);
            return Ok((complete, &self.peeked[0..len]));
        }
    }

    pub async fn read(&mut self, max_len: usize) -> BodyResult<Option<Bytes>> {
        if !self.peeked.is_empty() {
            // If there's already data in the peek buffer, return that first.
            let at = self.peeked.len().min(max_len);
            return Ok(Some(self.peeked.split_to(at).freeze()));
        }
        self.inner.read(max_len).await
    }

    pub fn drained(&self) -> bool {
        self.inner.drained()
    }

    /// Drain the body and return the underlying IO buffer along with the
    /// [`ParseSlots`] so the caller can reuse the allocation.
    pub async fn drain(self) -> BodyResult<(IoBuffer<I>, ParseSlots)> {
        // peeked bytes have already been consumed from the underlying IO,
        // so we just need to drain the inner reader
        self.inner.drain().await
    }
}
