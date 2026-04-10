use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll, ready},
};

use bytes::{Buf, Bytes, BytesMut};
use tokio::io::AsyncRead;

#[derive(Debug, thiserror::Error)]
pub enum ReaderBufferError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Unexpected EOF")]
    UnexpectedEOF,
}

#[derive(Default, Debug)]
struct ReaderBuffer {
    buffer: BytesMut,
    /// Indicates how many bytes of the buffer are valid. This is the maximum
    /// number of bytes we allow to be taken from the buffer.
    position: usize,
}

impl ReaderBuffer {
    fn split_to(&mut self, len: usize) -> BytesMut {
        assert!(len <= self.position);
        let b = self.buffer.split_to(len);
        self.position -= len;
        debug_assert!(self.position <= self.buffer.len());
        b
    }

    fn len(&self) -> usize {
        self.position
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn as_bytes(&self) -> &[u8] {
        &self.buffer[0..self.position]
    }

    /// Ensures there's space for at least `extra` bytes to be read into the
    /// buffer. Returns a mutable slice of the reserved space.
    fn reserve(&mut self, extra: usize) -> &mut [u8] {
        debug_assert!(
            self.position <= self.buffer.len(),
            "position must not exceed the buffer length"
        );
        let needed_len = self.position.saturating_add(extra);
        if needed_len > self.buffer.len() {
            self.buffer.resize(needed_len, 0);
        }
        &mut self.buffer[self.position..needed_len]
    }

    /// Advance the internal position. Must not advance past the buffer's
    /// current length.
    fn advance(&mut self, advance_by: usize) {
        let new_position = self.position.saturating_add(advance_by);
        debug_assert!(
            new_position <= self.buffer.len(),
            "advanced past end of buffer"
        );
        self.position = new_position;
    }

    fn get_u8(&mut self) -> u8 {
        debug_assert!(self.position > 0, "get_u8 without available data");
        let b = self.buffer.get_u8();
        self.position -= 1;
        b
    }
}

#[derive(Debug)]
pub struct BytesReader<I> {
    /// The reader from which to read
    io: I,
    /// The buffer into which we write and read
    buffer: ReaderBuffer,
}

impl<I> BytesReader<I> {
    pub fn new(io: I) -> Self {
        Self {
            io,
            buffer: Default::default(),
        }
    }

    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn split_to(&mut self, len: usize) -> Bytes {
        self.buffer.split_to(len).freeze()
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.buffer.as_bytes()
    }

    pub fn get_u8(&mut self) -> u8 {
        self.buffer.get_u8()
    }

    pub fn into_parts(self) -> (I, BytesMut) {
        let Self { io, buffer } = self;
        // Before returning the inner BytesMut, adjust the length down to only
        // the valid data. Not the allocated buffer size.
        let len = buffer.len();
        let mut buffer = buffer.buffer;
        buffer.resize(len, 0);
        (io, buffer)
    }
}

impl<I> BytesReader<I>
where
    I: AsyncRead + Unpin,
{
    /// On success, returns the additional the number of bytes added to the
    /// buffer. A return value of 0 indicates that either the user passed a
    /// `len` of 0, or the reader has stopped returning bytes.
    pub fn read_with_len(&mut self, len: usize) -> Fill<'_, I> {
        let Self { io, buffer } = self;
        Fill {
            reader: Pin::new(io),
            buffer,
            len,
        }
    }

    /// On success, at least one byte is in the buffer. `len` is the read length
    /// used to refill the buffer if it is empty.
    pub fn ensure(&mut self, len: usize) -> Ensure<'_, I> {
        let Self { io, buffer } = self;
        Ensure {
            reader: Pin::new(io),
            buffer,
            len,
        }
    }

    /// On success, at least one byte has been added to the buffer. `len` is the
    /// read length used to refill the buffer if it is empty.
    pub fn extend(&mut self, len: usize) -> Extend<'_, I> {
        let Self { io, buffer } = self;
        Extend {
            reader: Pin::new(io),
            buffer,
            len,
        }
    }
}

#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Fill<'s, I> {
    reader: Pin<&'s mut I>,
    buffer: &'s mut ReaderBuffer,
    len: usize,
}

impl<'s, I> Future for Fill<'s, I>
where
    I: AsyncRead + Unpin,
{
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Self {
            reader,
            buffer,
            len,
        } = self.get_mut();

        do_fill(cx, reader.as_mut(), *len, buffer)
    }
}

/// This future becomes ready when there is at least one byte available in the
/// buffer.
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Ensure<'s, I> {
    reader: Pin<&'s mut I>,
    buffer: &'s mut ReaderBuffer,
    len: usize,
}

impl<'s, I> Future for Ensure<'s, I>
where
    I: AsyncRead + Unpin,
{
    type Output = Result<(), ReaderBufferError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Self {
            reader,
            buffer,
            len,
        } = self.get_mut();

        if !buffer.is_empty() {
            return Poll::Ready(Ok(()));
        }

        let filled = ready!(do_fill(cx, reader.as_mut(), *len, buffer));
        match filled {
            Err(e) => Poll::Ready(Err(ReaderBufferError::Io(e))),
            Ok(0) => Poll::Ready(Err(ReaderBufferError::UnexpectedEOF)),
            Ok(_) => Poll::Ready(Ok(())),
        }
    }
}

/// This future becomes ready when bytes have been added to the buffer.
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Extend<'s, I> {
    reader: Pin<&'s mut I>,
    buffer: &'s mut ReaderBuffer,
    len: usize,
}

impl<'s, I> Future for Extend<'s, I>
where
    I: AsyncRead + Unpin,
{
    type Output = Result<(), ReaderBufferError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Self {
            reader,
            buffer,
            len,
        } = self.get_mut();

        let filled = ready!(do_fill(cx, reader.as_mut(), *len, buffer));
        match filled {
            Err(e) => Poll::Ready(Err(ReaderBufferError::Io(e))),
            Ok(0) => Poll::Ready(Err(ReaderBufferError::UnexpectedEOF)),
            Ok(_) => Poll::Ready(Ok(())),
        }
    }
}

fn do_fill<'s, I>(
    cx: &mut Context<'_>,
    mut reader: Pin<&'s mut I>,
    len: usize,
    buffer: &'s mut ReaderBuffer,
) -> Poll<Result<usize, io::Error>>
where
    I: AsyncRead + Unpin,
{
    let buf = buffer.reserve(len);
    let mut buf = tokio::io::ReadBuf::new(buf);
    match ready!(reader.as_mut().poll_read(cx, &mut buf)) {
        Err(e) => Poll::Ready(Err(e)),
        Ok(()) => {
            let len = buf.filled().len();
            buffer.advance(len);
            Poll::Ready(Ok(len))
        }
    }
}
