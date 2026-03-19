/// The framing of an HTTP message as parsed from its headers.
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
/// connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteFraming {
    /// Write a `content-length: <len>` framing header.
    Length(u64),
    /// Write a `transfer-encoding: chunked` framing header.
    Chunked,
    /// Do not write any framing header; forward any existing framing headers
    /// from the message as-is. This can be used for responses to `HEAD`
    /// requests and `304 Not Modified` responses, where `content-length` /
    /// `transfer-encoding` describe the representation rather than an actual
    /// body.
    PreserveFraming,
    /// Do not write any framing header; strip any existing framing headers from
    /// the message before writing. Use for responses that must never carry a
    /// body by definition: `1xx` informational responses and `204 No Content`.
    StripFraming,
}

impl WriteFraming {
    /// Returns `true` when this is [`WriteFraming::PreserveFraming`].
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

const FRAMING_HEADERS: &[&[u8]] = &[b"content-length", b"transfer-encoding"];

/// True when `name` is one of `content-length` or `transfer-encoding`.
pub fn is_framing_header(name: &[u8]) -> bool {
    FRAMING_HEADERS.iter().any(|h| h.eq_ignore_ascii_case(name))
}
