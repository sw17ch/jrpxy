use bytes::Bytes;

use crate::framing::HeadFraming;

#[derive(Debug, thiserror::Error)]
pub enum HeaderError {
    #[error("Invalid content length: {0}")]
    InvalidContentLength(String),
    #[error("Found at least two content-length headers with values: {0}, {0}")]
    MultipleContentLength(u64, u64),
    #[error("Multiple transfer-encoding headers")]
    MultipleTransferEncodingHeaders,
    #[error("Unsupported transfer-encoding value: {0}")]
    UnsupportedTransferEncoding(String),
    #[error("Both transfer-encoding and content-length headers present")]
    BothTeAndCl,
}

pub struct HeaderIter<'s> {
    position: usize,
    spans: &'s [(Bytes, Bytes)],
}

impl<'s> HeaderIter<'s> {
    /// An iterator over headers inside of `buf` identified by `spans`.
    ///
    /// # Panics
    ///
    /// Panics if any span falls outside of the buffer.
    pub fn new(spans: &'s [(Bytes, Bytes)]) -> Self {
        Self { position: 0, spans }
    }
}

impl<'s> Iterator for HeaderIter<'s> {
    type Item = &'s (Bytes, Bytes);

    fn next(&mut self) -> Option<Self::Item> {
        if self.position >= self.spans.len() {
            return None;
        }
        let p = self.position;
        self.position += 1;
        Some(&self.spans[p])
    }
}

pub struct Headers {
    headers: Vec<(Bytes, Bytes)>,
}
impl Headers {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            headers: Vec::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, name: impl Into<Bytes>, value: impl Into<Bytes>) {
        self.headers.push((name.into(), value.into()));
    }

    pub fn iter(&self) -> HeaderIter<'_> {
        HeaderIter::new(&self.headers)
    }

    pub fn get_header(&self, needle: &str) -> Option<&Bytes> {
        let needle = needle.as_bytes();
        for (name, value) in self.iter() {
            if needle.eq_ignore_ascii_case(name) {
                return Some(value);
            }
        }

        None
    }

    pub fn framing(&self) -> Result<HeadFraming, HeaderError> {
        let mut cl = None;
        let mut te = false;

        for (name, value) in self.iter() {
            if name.eq_ignore_ascii_case(b"content-length") {
                let value = value.trim_ascii();
                let cl_str = std::str::from_utf8(value).map_err(|_| {
                    HeaderError::InvalidContentLength(String::from_utf8_lossy(value).to_string())
                })?;
                let cl_val = cl_str
                    .parse::<u64>()
                    .map_err(|_| HeaderError::InvalidContentLength(cl_str.to_string()))?;
                if let Some(old_cl_val) = cl.replace(cl_val) {
                    return Err(HeaderError::MultipleContentLength(old_cl_val, cl_val));
                }
                cl = Some(cl_val);
            } else if name.eq_ignore_ascii_case(b"transfer-encoding") {
                let te_value = value.trim_ascii();
                if te_value != b"chunked" {
                    return Err(HeaderError::UnsupportedTransferEncoding(
                        String::from_utf8_lossy(te_value).to_string(),
                    ));
                }
                if te {
                    return Err(HeaderError::MultipleTransferEncodingHeaders);
                }
                te = true;
            } else {
                continue;
            }
        }

        match (cl, te) {
            (None, true) => Ok(HeadFraming::Chunked),
            (None, false) => Ok(HeadFraming::NoFraming),
            (Some(_), true) => Err(HeaderError::BothTeAndCl),
            (Some(cl), false) => Ok(HeadFraming::Length(cl)),
        }
    }

    pub fn remove<P>(&mut self, mut pred: P) -> Self
    where
        P: FnMut(&(Bytes, Bytes)) -> bool,
    {
        // If we had something like `Vec::filter_drain`, we could use that.
        // There's also certainly a way to avoid needing to re-allocate the
        // headers, but let's just get this shipped and let a benchmark prove
        // this is a slow path.
        let mut keep = Vec::with_capacity(self.headers.len());
        let mut remove = Vec::new();
        for p in self.headers.drain(..) {
            if pred(&p) {
                remove.push(p);
            } else {
                keep.push(p);
            }
        }
        self.headers = keep;
        Self { headers: remove }
    }

    pub fn into_inner(self) -> Vec<(Bytes, Bytes)> {
        let Self { headers } = self;
        headers
    }
}

#[cfg(test)]
mod test {
    use crate::header::Headers;

    /// Requests with both CL and TE headers must be rejected to prevent HTTP
    /// Request Smuggling attacks.
    ///
    /// From RFC 7230 3.3.3: "If a message is received with both a
    /// Transfer-Encoding and a Content-Length header field, the
    /// Transfer-Encoding overrides the Content-Length. Such a message might
    /// indicate an attempt to perform request smuggling or response splitting
    /// and ought to be handled as an error."
    #[test]
    fn reject_message_with_both_cl_and_te_headers() {
        let mut headers = Headers::with_capacity(2);
        headers.push(b"content-length".as_slice(), b"6".as_slice());
        headers.push(b"transfer-encoding".as_slice(), b"chunked".as_slice());

        assert!(
            headers.framing().is_err(),
            "Requests with both content-length and transfer-encoding must be rejected"
        );
    }

    #[test]
    fn reject_message_with_multiple_cl_headers() {
        let mut headers = Headers::with_capacity(2);
        headers.push(b"content-length".as_slice(), b"6".as_slice());
        headers.push(b"content-length".as_slice(), b"5".as_slice());

        assert!(
            headers.framing().is_err(),
            "Requests with multiple Content-Length headers must be rejected"
        );
    }

    #[test]
    fn reject_message_with_multiple_te_headers() {
        let mut headers = Headers::with_capacity(2);
        headers.push(b"transfer-encoding".as_slice(), b"chunked".as_slice());
        headers.push(b"transfer-encoding".as_slice(), b"chunked".as_slice());

        assert!(
            headers.framing().is_err(),
            "Requests with multiple Content-Length headers must be rejected"
        );
    }
}
