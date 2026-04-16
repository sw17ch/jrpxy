use std::future::poll_fn;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use tokio::io::AsyncRead;

use jrpxy_http_message::message::ParseSlots;
use jrpxy_util::io_buffer::BytesReader;

use crate::error::{BodyError, BodyResult};

/// Body reader for responses with no framing headers where the body is
/// terminated by connection close (RFC 9112 section 6.3 rule 8).
#[derive(Debug)]
pub struct EofBodyReader<I> {
    reader: BytesReader<I>,
    parse_slots: ParseSlots,
    eof: bool,
}

impl<I> EofBodyReader<I> {
    pub fn new(reader: BytesReader<I>, parse_slots: ParseSlots) -> Self {
        Self {
            reader,
            parse_slots,
            eof: false,
        }
    }
}

impl<I: AsyncRead + Unpin> EofBodyReader<I> {
    pub async fn read(&mut self, max_len: usize) -> BodyResult<Option<Bytes>> {
        poll_fn(|cx| self.poll_read(cx, max_len)).await
    }

    pub fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        max_len: usize,
    ) -> Poll<BodyResult<Option<Bytes>>> {
        if self.eof {
            return Poll::Ready(Ok(None));
        }
        // Return already-buffered data first.
        if !self.reader.is_empty() {
            let at = self.reader.len().min(max_len);
            return Poll::Ready(Ok(Some(self.reader.split_to(at))));
        }
        // Try to read more; a zero return means the connection was closed.
        let mut read = self.reader.read(max_len);
        let n = match ready!(Pin::new(&mut read).poll(cx)) {
            Ok(n) => n,
            Err(e) => return Poll::Ready(Err(BodyError::BodyReadError(e))),
        };
        if n == 0 {
            self.eof = true;
            return Poll::Ready(Ok(None));
        }
        let at = self.reader.len().min(max_len);
        Poll::Ready(Ok(Some(self.reader.split_to(at))))
    }

    pub fn drained(&self) -> bool {
        self.eof
    }

    /// There isn't any reason to drain an EOF reader because draining discards
    /// the body, and we only find the end of the body when the remote end
    /// closes the connection. For this reason, draining discards the
    /// connection, and returns the internal [`ParseSlots`] so they can be
    /// reused.
    pub fn drain(self) -> ParseSlots {
        let Self {
            reader: _,
            parse_slots,
            eof: _,
        } = self;
        parse_slots
    }
}
