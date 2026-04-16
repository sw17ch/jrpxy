use jrpxy_http_message::message::ParseSlots;
use jrpxy_util::io_buffer::BytesReader;

#[derive(Debug)]
pub struct BodylessBodyReader<I> {
    reader: BytesReader<I>,
    parse_slots: ParseSlots,
}

impl<I> BodylessBodyReader<I> {
    pub fn new(reader: BytesReader<I>, parse_slots: ParseSlots) -> Self {
        Self {
            reader,
            parse_slots,
        }
    }

    pub fn drain(self) -> (BytesReader<I>, ParseSlots) {
        let Self {
            reader,
            parse_slots,
        } = self;
        (reader, parse_slots)
    }
}
