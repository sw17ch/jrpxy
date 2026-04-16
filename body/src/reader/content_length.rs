use std::future::poll_fn;
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use tokio::io::AsyncRead;

use jrpxy_http_message::message::ParseSlots;
use jrpxy_util::io_buffer::BytesReader;

use crate::error::BodyResult;

#[derive(Debug)]
pub struct ContentLengthBodyReader<I> {
    /// the total body length specified by the content-length header.
    length: u64,
    /// the amount of the body already read
    offset: u64,
    /// the io associated with this body read
    reader: BytesReader<I>,
    /// These are not ever used while processing a content-length body. They
    /// exist here in order to allow higher level calls to reuse the slot
    /// allocation.
    parse_slots: ParseSlots,
}

impl<I> ContentLengthBodyReader<I> {
    pub fn new(length: u64, reader: BytesReader<I>, parse_slots: ParseSlots) -> Self {
        Self {
            length,
            offset: 0,
            reader,
            parse_slots,
        }
    }

    pub fn content_length(&self) -> u64 {
        self.length
    }

    pub fn drained(&self) -> bool {
        self.remaining() == 0
    }

    fn remaining(&self) -> u64 {
        debug_assert!(self.offset <= self.length);
        self.length - self.offset
    }

    /// # Panics
    ///
    /// Panics if the reader has not been fully drained via [`Self::poll_drain`].
    pub fn finish(self) -> (BytesReader<I>, ParseSlots) {
        let Self {
            length,
            offset,
            reader,
            parse_slots,
        } = self;
        assert_eq!(
            offset, length,
            "attempted to finish content length body reader before fully drained"
        );
        (reader, parse_slots)
    }
}

impl<I> ContentLengthBodyReader<I>
where
    I: AsyncRead + Unpin,
{
    pub async fn read(&mut self, max_len: usize) -> BodyResult<Option<Bytes>> {
        poll_fn(|cx| Self::poll_read(self, cx, max_len)).await
    }

    pub fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        max_len: usize,
    ) -> Poll<BodyResult<Option<Bytes>>> {
        let remaining = self.remaining();
        if remaining == 0 {
            return Poll::Ready(Ok(None));
        }

        if let Err(e) = ready!(super::poll_ensure(cx, &mut self.reader)) {
            return Poll::Ready(Err(e));
        }

        let at = remaining
            .try_into()
            .unwrap_or(usize::MAX)
            .min(self.reader.len())
            .min(max_len);

        self.offset += at as u64;

        Poll::Ready(Ok(Some(self.reader.split_to(at))))
    }

    /// Drain the [`ContentLengthBodyReader`] and return the inner
    /// [`BytesReader`] and [`ParseSlots`].
    pub async fn drain(mut self) -> BodyResult<(BytesReader<I>, ParseSlots)> {
        let () = poll_fn(|cx| self.poll_drain(cx)).await?;
        Ok(self.finish())
    }

    pub fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<BodyResult<()>> {
        loop {
            match ready!(self.poll_read(cx, super::DRAIN_SIZE)) {
                Err(e) => return Poll::Ready(Err(e)),
                Ok(None) => return Poll::Ready(Ok(())),
                Ok(Some(_)) => continue,
            }
        }
    }
}
