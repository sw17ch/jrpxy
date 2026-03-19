mod bodyless;
mod chunked;
mod content_length;
mod eof;

pub use bodyless::BodylessBodyReader;
pub use chunked::{
    ChunkDataReader, ChunkExtensions, ChunkHeadReader, ChunkedBodyReader, FinalChunkReader,
    NextChunk,
};
pub use content_length::ContentLengthBodyReader;
pub use eof::EofBodyReader;

use std::{
    pin::Pin,
    task::{Context, Poll, ready},
};

use tokio::io::AsyncRead;

use jrpxy_util::io_buffer::BytesReader;

use crate::error::{BodyError, BodyResult};

const DRAIN_SIZE: usize = 4096;
const IO_FILL_LEN: usize = 4096;

fn poll_ensure<I>(cx: &mut Context<'_>, reader: &mut BytesReader<I>) -> Poll<BodyResult<()>>
where
    I: AsyncRead + Unpin,
{
    let mut ensure = reader.ensure(IO_FILL_LEN);
    Poll::Ready(ready!(Pin::new(&mut ensure).poll(cx)).map_err(BodyError::from))
}

fn poll_extend<I>(cx: &mut Context<'_>, reader: &mut BytesReader<I>) -> Poll<BodyResult<()>>
where
    I: AsyncRead + Unpin,
{
    let mut extend = reader.extend(IO_FILL_LEN);
    Poll::Ready(ready!(Pin::new(&mut extend).poll(cx)).map_err(BodyError::from))
}
