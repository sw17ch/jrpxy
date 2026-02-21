use bytes::BytesMut;

#[derive(Debug)]
pub struct Span {
    offset: usize,
    len: usize,
}

impl Span {
    pub fn new(outer: &[u8], inner: &[u8]) -> Option<Self> {
        let len = inner.len();
        let outer = outer.as_ptr_range();
        let inner = inner.as_ptr_range();
        if outer.start <= inner.start && inner.end <= outer.end {
            let outer_start = outer.start as usize;
            let inner_start = inner.start as usize;
            let offset = inner_start - outer_start;
            Some(Self { offset, len })
        } else {
            None
        }
    }

    pub const fn empty() -> Self {
        Self { offset: 0, len: 0 }
    }

    pub fn range(&self) -> std::ops::Range<usize> {
        self.offset..self.offset + self.len
    }

    pub fn try_slice<'b>(&self, buf: &'b [u8]) -> Option<&'b [u8]> {
        buf.get(self.range())
    }

    pub fn slice<'b>(&self, buf: &'b [u8]) -> &'b [u8] {
        &buf[self.range()]
    }
}

impl Default for Span {
    fn default() -> Self {
        Self::empty()
    }
}

pub struct BufBuilder {
    buf: BytesMut,
}

impl BufBuilder {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: BytesMut::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, slice: &[u8]) -> Span {
        let offset = self.buf.len();
        let len = slice.len();
        self.buf.extend_from_slice(slice);
        Span { offset, len }
    }

    pub fn push_space(&mut self) -> Span {
        self.push(b" ")
    }

    pub fn push_field_sep(&mut self) -> Span {
        self.push(b": ")
    }

    pub fn push_crlf(&mut self) -> Span {
        self.push(b"\r\n")
    }

    pub fn finish(self) -> BytesMut {
        let Self { buf } = self;
        buf
    }
}

#[cfg(test)]
mod test {
    use super::Span;

    #[test]
    fn span_from_slices() {
        let outer = &[0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9];
        assert!(Span::new(outer, &outer[1..4]).is_some());
        assert!(Span::new(&[], outer).is_none());
        assert!(Span::new(&outer[2..], &outer[1..]).is_none());
    }
}
