use std::io;

use tokio::io::AsyncReadExt;

use bytes::{Buf, BytesMut};

#[derive(Debug)]
pub struct Buffer(BytesMut);

impl std::ops::Deref for Buffer {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl Buffer {
    pub fn new(buffer: BytesMut) -> Self {
        Self(buffer)
    }

    #[inline]
    pub fn split_to(&mut self, at: usize) -> BytesMut {
        self.0.split_to(at)
    }

    /// Read some data from `io`. The internal buffer will be expanded to
    /// accomodate at least `target_read_len` new bytes, and a read from `io`
    /// will be attempted.
    pub async fn read_from<I: AsyncReadExt + Unpin>(
        &mut self,
        mut io: I,
        target_read_len: usize,
    ) -> io::Result<usize> {
        let mut r = ReadLimitedBytes::new(&mut self.0, target_read_len);
        io.read_buf(&mut r).await
    }

    #[inline]
    pub fn into_inner(self) -> BytesMut {
        let Self(b) = self;
        b
    }

    #[inline]
    pub fn try_get_u8(&mut self) -> Result<u8, bytes::TryGetError> {
        self.0.try_get_u8()
    }

    #[inline]
    /// See [`bytes::BytesMut::get_u8`].
    pub fn get_u8(&mut self) -> u8 {
        self.0.get_u8()
    }

    #[inline]
    pub fn extend_from_slice(&mut self, buf: &[u8]) {
        self.0.extend_from_slice(buf);
    }
}

/// A wrapper struct that limits how many bytes can be read into a [`BytesMut`]
/// via [`bytes::BufMut`]. The impl for [`BytesMut`] does not have a practical
/// limit on how many bytes it will allow to be read in. This type limits the
/// maximum amount to the specified [`max_read`] size.
struct ReadLimitedBytes<'b> {
    max_read: usize,
    position: usize,
    buf: &'b mut BytesMut,
}

impl<'b> ReadLimitedBytes<'b> {
    fn new(buf: &'b mut BytesMut, max_read: usize) -> Self {
        buf.reserve(max_read);
        Self {
            max_read,
            position: 0,
            buf,
        }
    }
}

unsafe impl<'b> bytes::BufMut for ReadLimitedBytes<'b> {
    fn remaining_mut(&self) -> usize {
        self.max_read.saturating_sub(self.position)
    }

    unsafe fn advance_mut(&mut self, cnt: usize) {
        let new_position = self.position + cnt;
        if new_position > self.max_read {
            panic!("advancing past max_read");
        }
        self.position = new_position;
        unsafe { self.buf.advance_mut(cnt) }
    }

    fn chunk_mut(&mut self) -> &mut bytes::buf::UninitSlice {
        // how many bytes until the limit
        let rem = self.remaining_mut();
        // the inner buf chunk we're going to slice
        let inner_chunk = self.buf.chunk_mut();
        // the length we're going to slice. we use whichever is smallest of the
        // chunk length or our remaining size
        let slice_len = inner_chunk.len().min(rem);

        &mut inner_chunk[..slice_len]
    }
}

#[cfg(test)]
mod test {
    use bytes::{BufMut, BytesMut};

    use crate::buffer::{Buffer, ReadLimitedBytes};

    #[tokio::test]
    async fn buffer() {
        let input = b"01234";
        let mut b = Buffer::new(BytesMut::new());
        let l = b.read_from(&input[..], 128).await.expect("read failed");
        assert_eq!(5, l);
        assert_eq!(128, b.0.capacity());
    }

    #[tokio::test]
    async fn limits() {
        let input = b"0123456789";
        let mut b = Buffer::new(BytesMut::new());
        let l = b.read_from(&input[..], 8).await.expect("read failed");
        assert_eq!(8, l);
        assert_eq!(8, b.0.capacity());
    }

    #[tokio::test]
    async fn close_to_limits() {
        let input = b"0123456789";
        let mut b = Buffer::new(BytesMut::with_capacity(1024));
        let l = b.read_from(&input[..], 7).await.expect("read failed");
        assert_eq!(7, l);
        let l = b.read_from(&input[..], 1).await.expect("read failed");
        assert_eq!(1, l);
    }

    #[test]
    fn chunk_mut_does_not_exceed_buf_spare_capacity() {
        let mut buf = BytesMut::new();
        let mut r = ReadLimitedBytes::new(&mut buf, 4);
        // BytesMut::reserve may allocate more than requested (e.g. rounding
        // to an internal minimum). Read the actual spare capacity after new().
        let actual_spare = r.buf.chunk_mut().len();
        assert!(actual_spare >= 4);
        // adjust our max read to one more byte than buf has. this means
        // remaining_mut() will exceed the actual capacity forcing the check to
        // operate.
        r.max_read = actual_spare + 1;
        assert_eq!(r.remaining_mut(), actual_spare + 1);
        // chunk_mut() attempts &mut buf.chunk_mut()[0..actual_spare+1] on a
        // actual_spare-byte UninitSlice. With the current code this panics
        // with an opaque index-out-of-bounds; with an explicit assertion it
        // should panic with a clear message.
        let chunk = r.chunk_mut();
        assert_eq!(chunk.len(), actual_spare,);
    }

    #[test]
    fn advance_mut_updates_remaining() {
        let mut buf = BytesMut::new();
        let mut r = ReadLimitedBytes::new(&mut buf, 8);
        assert_eq!(r.remaining_mut(), 8);
        unsafe { r.advance_mut(4) };
        assert_eq!(r.remaining_mut(), 4);
    }

    #[test]
    #[should_panic]
    fn advance_mut_panics_when_advancing_too_far() {
        let mut buf = BytesMut::new();
        let mut r = ReadLimitedBytes::new(&mut buf, 8);
        assert_eq!(r.remaining_mut(), 8);
        unsafe { r.advance_mut(4) };
        assert_eq!(r.remaining_mut(), 4);
        unsafe { r.advance_mut(5) };
    }
}
