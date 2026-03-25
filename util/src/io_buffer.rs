use bytes::{Bytes, BytesMut};
use tokio::io::AsyncReadExt;

use crate::buffer::Buffer;

#[derive(Debug, thiserror::Error)]
pub enum IoBufferError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Unexpected EOF")]
    UnexpectedEOF,
}

/// A buffered IO source. Wraps an IO reader with a read buffer and a preferred
/// chunk size for filling the buffer.
#[derive(Debug)]
pub struct IoBuffer<I> {
    io: I,
    fill_len: usize,
    buffer: Buffer,
}

impl<I> IoBuffer<I> {
    pub fn new(io: I, buffer: BytesMut, fill_len: usize) -> Self {
        Self {
            io,
            fill_len,
            buffer: Buffer::new(buffer),
        }
    }

    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn split_to(&mut self, at: usize) -> Bytes {
        self.buffer.split_to(at).freeze()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buffer
    }

    pub fn get_u8(&mut self) -> u8 {
        self.buffer.get_u8()
    }

    pub fn as_buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffer
    }

    pub fn into_parts(self) -> (I, Buffer) {
        let Self {
            io,
            fill_len: _,
            buffer,
        } = self;
        (io, buffer)
    }
}

impl<I: AsyncReadExt + Unpin> IoBuffer<I> {
    /// Ensure there is at least one byte of data available in the buffer. If
    /// the buffer is empty, reads `fill_len` bytes from IO. Returns an error
    /// if the buffer is empty and no more data is available.
    pub async fn ensure(&mut self) -> Result<(), IoBufferError> {
        if !self.buffer.is_empty() {
            return Ok(());
        }
        self.extend().await?;
        Ok(())
    }

    /// Read more data from IO into the buffer regardless of how much is
    /// already buffered. Returns the new buffer length, or an error if no
    /// bytes were available (unexpected EOF).
    pub async fn extend(&mut self) -> Result<usize, IoBufferError> {
        let r = self.buffer.read_from(&mut self.io, self.fill_len).await?;
        if r == 0 {
            return Err(IoBufferError::UnexpectedEOF);
        }
        Ok(self.buffer.len())
    }
}
