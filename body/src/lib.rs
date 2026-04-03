pub mod error;
pub mod reader;
pub mod writer;

const FRAMING_HEADERS: &[&[u8]] = &[b"content-length", b"transfer-encoding"];

pub fn is_framing_header(name: &[u8]) -> bool {
    FRAMING_HEADERS.iter().any(|h| h.eq_ignore_ascii_case(name))
}
