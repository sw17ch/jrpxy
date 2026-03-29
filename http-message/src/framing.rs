/// The framing of an HTTP message as parsed from its headers. Returned by
/// [`crate::header::Headers::framing`] and the `framing()` methods on
/// [`crate::message::Request`] and [`crate::message::Response`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsedFraming {
    /// Body length is specified by a `content-length` header.
    Length(u64),
    /// Body is delimited by `transfer-encoding: chunked`.
    Chunked,
    /// No framing header is present in the message.
    NoFraming,
}

impl ParsedFraming {
    /// Returns `true` if no framing header is present in the message.
    pub fn is_no_framing(&self) -> bool {
        matches!(self, Self::NoFraming)
    }
}

/// The framing instruction used when writing an HTTP message head to a
/// connection. Passed to the internal `write_request_to` / `write_response_to`
/// helpers and to the public `send_as_*` methods on [`FrontendWriter`] and
/// [`BackendWriter`].
///
/// [`FrontendWriter`]: jrpxy_frontend::writer::FrontendWriter
/// [`BackendWriter`]: jrpxy_backend::writer::BackendWriter
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteFraming {
    /// Write a `content-length: <len>` framing header.
    Length(u64),
    /// Write a `transfer-encoding: chunked` framing header.
    Chunked,
    /// Write no framing header; forward any existing framing headers from the
    /// message as-is. Use for responses to `HEAD` requests and `304 Not
    /// Modified` responses, where `content-length` / `transfer-encoding`
    /// describe the representation rather than an actual body.
    PreserveFraming,
    /// Write no framing header; strip any existing framing headers from the
    /// message before writing. Use for responses that must never carry a body
    /// by definition: `1xx` informational responses and `204 No Content`.
    StripFraming,
}

impl WriteFraming {
    /// Returns `true` if this is [`WriteFraming::PreserveFraming`], meaning
    /// any framing headers already present in the message should be forwarded
    /// as-is.
    pub fn preserves_framing(&self) -> bool {
        matches!(self, Self::PreserveFraming)
    }
}

impl From<ParsedFraming> for WriteFraming {
    fn from(f: ParsedFraming) -> Self {
        match f {
            ParsedFraming::Length(l) => WriteFraming::Length(l),
            ParsedFraming::Chunked => WriteFraming::Chunked,
            ParsedFraming::NoFraming => WriteFraming::PreserveFraming,
        }
    }
}
