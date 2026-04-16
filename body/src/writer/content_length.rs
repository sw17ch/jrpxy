use std::{
    future::poll_fn,
    task::{Context, Poll},
};

use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::error::{BodyError, BodyResult};

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
        super::poll_write_bounded(cx, &mut self.writer, self.length, &mut self.offset, buffer)
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

#[cfg(test)]
mod test {
    use crate::error::BodyError;

    use super::ContentLengthBodyWriter;

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
}
