mod bodyless;
mod chunked;
mod content_length;

pub use bodyless::BodylessBodyWriter;
pub use chunked::{
    ChunkDataWriter, ChunkHeadWriter, ChunkedBodyWriter, FinalChunkWriter, IdleChunkWriter,
    ManuallyChunkedBodyFinalizer, ManuallyChunkedBodyWriter,
};
pub use content_length::ContentLengthBodyWriter;

use std::{
    pin::Pin,
    task::{Context, Poll, ready},
};

use tokio::io::AsyncWrite;

use crate::error::{BodyError, BodyResult};

fn poll_write_bounded<I: AsyncWrite + Unpin>(
    cx: &mut Context<'_>,
    writer: &mut I,
    max_length: u64,
    offset: &mut u64,
    buffer: &[u8],
) -> Poll<BodyResult<usize>> {
    let Some(target_next_offset) = offset.checked_add(buffer.len() as u64) else {
        // a complete write of this buffer would overflow the size of a u64
        return Poll::Ready(Err(BodyError::BodyOverflow(max_length)));
    };
    if target_next_offset > max_length {
        // a complete write of this buffer would overflow the allowed length
        return Poll::Ready(Err(BodyError::BodyOverflow(max_length)));
    }
    match ready!(Pin::new(writer).poll_write(cx, buffer)) {
        Ok(len) => {
            // we already check that a full write of the buffer won't
            // overflow, so we are safe to advance the offset by the
            // written length.
            *offset += len as u64;
            Poll::Ready(Ok(len))
        }
        Err(e) => Poll::Ready(Err(BodyError::BodyWriteError(e))),
    }
}
