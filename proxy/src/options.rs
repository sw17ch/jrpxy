//! Configuration governing proxy behavior.

use std::{borrow::Cow, fmt::Debug, sync::Arc};

use jrpxy_util::parse::is_valid_tchar;

use crate::error::ProxyOptionsError;

/// Options used to govern the behavior of the proxy.
#[derive(Clone)]
pub struct ProxyOptions {
    inner: Arc<Inner>,
}

impl Debug for ProxyOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyOptions")
            .field(
                "max_backend_head_length",
                &self.inner.max_backend_head_length,
            )
            .field(
                "max_chunk_header_length",
                &self.inner.max_chunk_header_length,
            )
            .field("body_chunk_size", &self.inner.body_chunk_size)
            .field("received_by", &self.inner.received_by)
            .finish()
    }
}

struct Inner {
    max_backend_head_length: usize,
    max_chunk_header_length: usize,
    body_chunk_size: usize,
    received_by: Cow<'static, str>,
}

impl ProxyOptions {
    /// Start building a [`ProxyOptions`] from the defaults. All validation is
    /// deferred to [`ProxyOptionsBuilder::build`].
    pub fn builder() -> ProxyOptionsBuilder {
        ProxyOptionsBuilder::default()
    }

    /// The maximum length, in bytes, that a backend response head can be.
    pub fn max_backend_head_length(&self) -> usize {
        self.inner.max_backend_head_length
    }

    /// The maximum length, in bytes, that a single chunk's size line (the
    /// chunk-size and any chunk extensions) can be when reading a chunked body.
    pub fn max_chunk_header_length(&self) -> usize {
        self.inner.max_chunk_header_length
    }

    /// The read size to be attempted when copying body bytes.
    pub fn body_chunk_size(&self) -> usize {
        self.inner.body_chunk_size
    }

    /// The name of the proxy used in the `Via` header.
    pub fn received_by(&self) -> &str {
        &self.inner.received_by
    }
}

impl Default for ProxyOptions {
    fn default() -> Self {
        ProxyOptions::builder()
            .build()
            .expect("the default ProxyOptions is always valid")
    }
}

/// Builder for [`ProxyOptions`]. Field validation is deferred until
/// [`build`](Self::build) so a whole configuration can be assembled before any
/// of it is rejected.
#[derive(Debug, Clone)]
pub struct ProxyOptionsBuilder {
    max_backend_head_length: usize,
    max_chunk_header_length: usize,
    body_chunk_size: usize,
    received_by: Cow<'static, str>,
}

impl Default for ProxyOptionsBuilder {
    fn default() -> Self {
        Self {
            max_backend_head_length: 8192,
            max_chunk_header_length: 8192,
            body_chunk_size: 8192,
            received_by: Cow::Borrowed("jrpxy"),
        }
    }
}

impl ProxyOptionsBuilder {
    /// Set the maximum length, in bytes, that a backend response head can be.
    pub fn max_backend_head_length(mut self, max_backend_head_length: usize) -> Self {
        self.max_backend_head_length = max_backend_head_length;
        self
    }

    /// Set the maximum length, in bytes, that a single chunk's size line can be
    /// when reading a chunked body.
    pub fn max_chunk_header_length(mut self, max_chunk_header_length: usize) -> Self {
        self.max_chunk_header_length = max_chunk_header_length;
        self
    }

    /// Set the read size to be attempted when copying body bytes.
    pub fn body_chunk_size(mut self, body_chunk_size: usize) -> Self {
        self.body_chunk_size = body_chunk_size;
        self
    }

    /// Set the `Via` pseudonym. The value is validated in [`build`](Self::build).
    pub fn received_by(mut self, received_by: impl Into<Cow<'static, str>>) -> Self {
        self.received_by = received_by.into();
        self
    }

    /// Validate the accumulated options and produce a [`ProxyOptions`].
    pub fn build(self) -> Result<ProxyOptions, ProxyOptionsError> {
        let Self {
            max_backend_head_length,
            max_chunk_header_length,
            body_chunk_size,
            received_by,
        } = self;

        validate_received_by(&received_by)?;

        Ok(ProxyOptions {
            inner: Arc::new(Inner {
                max_backend_head_length,
                max_chunk_header_length,
                body_chunk_size,
                received_by,
            }),
        })
    }
}

/// A `Via` pseudonym must be a non-empty token (`1*tchar`, RFC 9110 sections
/// 5.6.2 and 7.6.3); rejecting anything else keeps an operator typo from
/// corrupting or splitting the emitted `Via` header.
fn validate_received_by(received_by: &str) -> Result<(), ProxyOptionsError> {
    if received_by.is_empty() {
        return Err(ProxyOptionsError::EmptyReceivedBy);
    }

    if let Some(&byte) = received_by.as_bytes().iter().find(|&&b| !is_valid_tchar(b)) {
        return Err(ProxyOptionsError::IllegalReceivedByByte(byte));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ProxyOptions;
    use crate::error::ProxyOptionsError;

    #[test]
    fn builder_applies_fields() {
        let options = ProxyOptions::builder()
            .received_by("proxy.example.com")
            .max_backend_head_length(2048)
            .max_chunk_header_length(256)
            .body_chunk_size(4096)
            .build()
            .expect("all fields are valid");
        assert_eq!(options.received_by(), "proxy.example.com");
        assert_eq!(options.max_backend_head_length(), 2048);
        assert_eq!(options.max_chunk_header_length(), 256);
        assert_eq!(options.body_chunk_size(), 4096);
    }

    #[test]
    fn builder_rejects_empty_received_by() {
        assert_eq!(
            ProxyOptions::builder()
                .received_by("")
                .build()
                .expect_err("an empty pseudonym must be rejected"),
            ProxyOptionsError::EmptyReceivedBy
        );
    }

    #[test]
    fn builder_rejects_crlf_received_by() {
        assert_eq!(
            ProxyOptions::builder()
                .received_by("evil\r\nX-Injected: 1")
                .build()
                .expect_err("a CRLF pseudonym must be rejected"),
            ProxyOptionsError::IllegalReceivedByByte(b'\r')
        );
    }

    #[test]
    fn builder_rejects_interior_space_received_by() {
        assert_eq!(
            ProxyOptions::builder()
                .received_by("two words")
                .build()
                .expect_err("a pseudonym with an interior space must be rejected"),
            ProxyOptionsError::IllegalReceivedByByte(b' ')
        );
    }
}
