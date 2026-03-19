use jrpxy_http_message::message::ParseSlots;
use jrpxy_util::io_buffer::BytesReader;

/// A reader for a HTTP message without a body. These are useful when code flows
/// are easier with some sort of body representation (even if empty).
#[derive(Debug)]
pub struct BodylessBodyReader<I> {
    reader: BytesReader<I>,
    parse_slots: ParseSlots,
}

impl<I> BodylessBodyReader<I> {
    /// Create a new bodyless reader. `reader` must be aligned to the first byte
    /// after a HTTP request or response.
    pub fn new(reader: BytesReader<I>, parse_slots: ParseSlots) -> Self {
        Self {
            reader,
            parse_slots,
        }
    }

    /// 'Drain' the bodyless reader and return its inner parts. Since there is
    /// no body, this does not have any side effect other than returning the
    /// inner parts of the body reader.
    pub fn drain(self) -> (BytesReader<I>, ParseSlots) {
        let Self {
            reader,
            parse_slots,
        } = self;
        (reader, parse_slots)
    }
}
