use std::{
    future::poll_fn,
    pin::Pin,
    task::{Context, Poll, ready},
};

use jrpxy_http_message::header::Headers;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::error::{BodyError, BodyResult};

#[derive(Debug)]
pub struct BodylessBodyWriter<I>(I);

impl<I> BodylessBodyWriter<I> {
    pub fn new(writer: I) -> Self {
        Self(writer)
    }

    pub fn finish(self) -> I {
        let Self(writer) = self;
        writer
    }
}

#[derive(Debug)]
pub struct ContentLengthBodyWriter<I> {
    /// the total body length specified by the content-length header.
    length: u64,
    /// the amount of the body already written
    offset: u64,
    writer: I,
}

impl<I> ContentLengthBodyWriter<I> {
    pub fn new(length: u64, writer: I) -> Self {
        Self {
            length,
            offset: 0,
            writer,
        }
    }
}

impl<I: AsyncWrite + Unpin> ContentLengthBodyWriter<I> {
    pub async fn write(&mut self, mut buffer: &[u8]) -> BodyResult<()> {
        // TODO: should this return a usize instead of ()? Right now, we return
        // a WriteZero error if we cannot fully write.
        while !buffer.is_empty() {
            let written = poll_fn(|cx| self.poll_write(cx, buffer)).await?;
            if written == 0 {
                return Err(BodyError::BodyWriteError(
                    std::io::ErrorKind::WriteZero.into(),
                ));
            }
            buffer = &buffer[written..];
        }
        Ok(())
    }

    pub fn poll_write(&mut self, cx: &mut Context<'_>, buffer: &[u8]) -> Poll<BodyResult<usize>> {
        let Some(target_next_offset) = self.offset.checked_add(buffer.len() as u64) else {
            // a complete write of this buffer would overflow the size of a u64
            return Poll::Ready(Err(BodyError::BodyOverflow(self.length)));
        };

        if target_next_offset > self.length {
            // a complete write of this buffer would overflow the allowed length
            return Poll::Ready(Err(BodyError::BodyOverflow(self.length)));
        }

        match ready!(Pin::new(&mut self.writer).poll_write(cx, buffer)) {
            Ok(len) => {
                // we already check that a full write of the buffer won't
                // overflow, so we are safe to advance the length by the written
                // length.
                self.offset += len as u64;
                Poll::Ready(Ok(len))
            }
            Err(e) => Poll::Ready(Err(BodyError::BodyWriteError(e))),
        }
    }

    pub async fn finish(self) -> BodyResult<I> {
        let Self {
            length,
            offset,
            mut writer,
        } = self;
        debug_assert!(offset <= length, "offset exceeds length");
        if offset < length {
            return Err(BodyError::IncompleteBody {
                expected: length,
                actual: offset,
            });
        }
        writer.flush().await.map_err(BodyError::BodyWriteError)?;
        Ok(writer)
    }
}

#[derive(Debug)]
pub struct ChunkedBodyWriter<I> {
    writer: I,
}

impl<I> ChunkedBodyWriter<I> {
    pub fn new(writer: I) -> Self {
        Self { writer }
    }
}

impl<I: AsyncWriteExt + Unpin> ChunkedBodyWriter<I> {
    // TODO: add a flush method so that we can force out writes

    pub async fn write(&mut self, buffer: &[u8]) -> BodyResult<()> {
        // TODO: use vectored writes

        if buffer.is_empty() {
            // if there's nothing in the buffer, don't do anything else.
            // attempting to write a zero-length chunk will break framing.
            return Ok(());
        }

        // TODO: we can avoid this heap allocation
        let chunk_header = format!("{:x}\r\n", buffer.len());

        self.writer
            .write_all(chunk_header.as_bytes())
            .await
            .map_err(BodyError::BodyWriteError)?;
        self.writer
            .write_all(buffer)
            .await
            .map_err(BodyError::BodyWriteError)?;
        self.writer
            .write_all(b"\r\n")
            .await
            .map_err(BodyError::BodyWriteError)?;

        Ok(())
    }

    pub async fn finish_with_trailers(self, trailers: &Headers) -> BodyResult<I> {
        let Self { mut writer } = self;

        writer
            .write_all(b"0\r\n")
            .await
            .map_err(BodyError::BodyWriteError)?;
        for (name, value) in trailers.iter() {
            writer
                .write_all(name)
                .await
                .map_err(BodyError::BodyWriteError)?;
            writer
                .write_all(b": ")
                .await
                .map_err(BodyError::BodyWriteError)?;
            writer
                .write_all(value)
                .await
                .map_err(BodyError::BodyWriteError)?;
            writer
                .write_all(b"\r\n")
                .await
                .map_err(BodyError::BodyWriteError)?;
        }
        writer
            .write_all(b"\r\n")
            .await
            .map_err(BodyError::BodyWriteError)?;
        writer.flush().await.map_err(BodyError::BodyWriteError)?;
        Ok(writer)
    }

    pub async fn finish(self) -> BodyResult<I> {
        self.finish_with_trailers(&Default::default()).await
    }

    pub async fn abort(self) -> BodyResult<I> {
        let Self { mut writer } = self;
        // we allow users to explicitly abandon a transfer-encoding:chunked
        // write by emitting a bad chunk header. this should be a little more
        // durable when badly-configured body readers don't wait for the
        // terminating empty chunk to consider a body complete.
        //
        // we write an 'x' because it is not a valid hexadecimal character
        writer
            .write_all(b"x")
            .await
            .map_err(BodyError::BodyWriteError)?;
        writer.flush().await.map_err(BodyError::BodyWriteError)?;
        Ok(writer)
    }
}

#[cfg(test)]
mod test {
    use crate::error::BodyError;

    use super::{ChunkedBodyWriter, ContentLengthBodyWriter};

    #[tokio::test]
    async fn content_length_write_overflow() {
        let mut bw = ContentLengthBodyWriter::new(10, Vec::new());
        let () = bw.write(b"01234").await.unwrap();
        let e = bw.write(b"567890").await.unwrap_err();
        assert!(matches!(e, BodyError::BodyOverflow(10)));
    }

    #[tokio::test]
    async fn content_length_zero_write() {
        let buf = &mut [0u8, 0, 0, 0][..];
        let cursor = std::io::Cursor::new(buf);
        let mut bw = ContentLengthBodyWriter::new(10, cursor);
        let e = bw.write(b"01234").await.unwrap_err();
        let BodyError::BodyWriteError(e) = e else {
            panic!("unexpected error: {e:?}");
        };
        assert_eq!(std::io::ErrorKind::WriteZero, e.kind());
    }

    #[tokio::test]
    async fn chunked_write_abort() {
        let mut bw = ChunkedBodyWriter::new(Vec::new());
        bw.write(b"hello").await.unwrap();
        bw.write(b"there").await.unwrap();
        let write_buf = bw.abort().await.unwrap();

        assert_eq!(
            "\
            5\r\n\
            hello\r\n\
            5\r\n\
            there\r\n\
            x",
            std::str::from_utf8(&write_buf).unwrap(),
        );
    }

    #[tokio::test]
    async fn chunked_write() {
        let mut bw = ChunkedBodyWriter::new(Vec::new());
        bw.write(b"hello").await.unwrap();
        bw.write(b"there").await.unwrap();
        let write_buf = bw.finish().await.unwrap();

        assert_eq!(
            "\
            5\r\n\
            hello\r\n\
            5\r\n\
            there\r\n\
            0\r\n\
            \r\n",
            std::str::from_utf8(&write_buf).unwrap(),
        );
    }
}
