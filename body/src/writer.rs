use jrpxy_http_message::header::Headers;
use tokio::io::AsyncWriteExt;

use crate::error::{BodyError, BodyResult};

#[derive(Debug)]
pub struct BodylessBodyWriter<I>(I);

impl<I> BodylessBodyWriter<I> {
    pub fn new(io: I) -> Self {
        Self(io)
    }

    pub fn finish(self) -> I {
        let Self(io) = self;
        io
    }
}

#[derive(Debug)]
pub struct ContentLengthBodyWriter<I> {
    /// the total body length specified by the content-length header.
    length: u64,
    /// the amount of the body already written
    offset: u64,
    io: I,
}

impl<I> ContentLengthBodyWriter<I> {
    pub fn new(length: u64, io: I) -> Self {
        Self {
            length,
            offset: 0,
            io,
        }
    }
}

impl<I: AsyncWriteExt + Unpin> ContentLengthBodyWriter<I> {
    pub async fn write(&mut self, buffer: &[u8]) -> BodyResult<()> {
        let Self { length, offset, io } = self;

        let Some(next_offset) = offset.checked_add(buffer.len() as u64) else {
            return Err(BodyError::BodyOverflow(*length));
        };

        if next_offset > *length {
            return Err(BodyError::BodyOverflow(*length));
        }

        io.write_all(buffer)
            .await
            .map_err(BodyError::BodyWriteError)?;

        *offset = next_offset;

        Ok(())
    }

    pub async fn finish(self) -> BodyResult<I> {
        let Self {
            length,
            offset,
            mut io,
        } = self;
        debug_assert!(offset <= length, "offset exceeds length");
        if offset < length {
            return Err(BodyError::IncompleteBody {
                expected: length,
                actual: offset,
            });
        }
        io.flush().await.map_err(BodyError::BodyWriteError)?;
        Ok(io)
    }
}

#[cfg(test)]
mod test {
    use super::ChunkedBodyWriter;

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
#[derive(Debug)]
pub struct ChunkedBodyWriter<I> {
    io: I,
}

impl<I> ChunkedBodyWriter<I> {
    pub fn new(io: I) -> Self {
        Self { io }
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

        self.io
            .write_all(chunk_header.as_bytes())
            .await
            .map_err(BodyError::BodyWriteError)?;
        self.io
            .write_all(buffer)
            .await
            .map_err(BodyError::BodyWriteError)?;
        self.io
            .write_all(b"\r\n")
            .await
            .map_err(BodyError::BodyWriteError)?;

        Ok(())
    }

    pub async fn finish_with_trailers(self, trailers: &Headers) -> BodyResult<I> {
        let Self { mut io } = self;

        io.write_all(b"0\r\n")
            .await
            .map_err(BodyError::BodyWriteError)?;
        for (name, value) in trailers.iter() {
            io.write_all(name)
                .await
                .map_err(BodyError::BodyWriteError)?;
            io.write_all(b": ")
                .await
                .map_err(BodyError::BodyWriteError)?;
            io.write_all(value)
                .await
                .map_err(BodyError::BodyWriteError)?;
            io.write_all(b"\r\n")
                .await
                .map_err(BodyError::BodyWriteError)?;
        }
        io.write_all(b"\r\n")
            .await
            .map_err(BodyError::BodyWriteError)?;
        io.flush().await.map_err(BodyError::BodyWriteError)?;
        Ok(io)
    }

    pub async fn finish(self) -> BodyResult<I> {
        self.finish_with_trailers(&Default::default()).await
    }

    pub async fn abort(self) -> BodyResult<I> {
        let Self { mut io } = self;
        // we allow users to explicitly abandon a transfer-encoding:chunked
        // write by emitting a bad chunk header. this should be a little more
        // durable when badly-configured body readers don't wait for the
        // terminating empty chunk to consider a body complete.
        //
        // we write an 'x' because it is not a valid hexadecimal character
        io.write_all(b"x")
            .await
            .map_err(BodyError::BodyWriteError)?;
        io.flush().await.map_err(BodyError::BodyWriteError)?;
        Ok(io)
    }
}
