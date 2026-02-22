/// Lookup table for RFC9110 `tchar`
/// 1 if valid tchar, 0 if invalid
#[rustfmt::skip]
const TCHAR_LOOKUP: [u8; 256] = [
    // 0x00 - 0x0F (Control Chars)
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    // 0x10 - 0x1F (Control Chars)
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    // 0x20 - 0x2F ( ! " # $ % & ' ( ) * + , - . /)
    0, 1, 0, 1, 1, 1, 1, 1, 0, 0, 1, 1, 0, 1, 1, 0,
    // 0x30 - 0x3F (0-9 : ; < = > ?)
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0,
    // 0x40 - 0x4F (@ A-O)
    0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    // 0x50 - 0x5F (P-Z [ \ ] ^ _)
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 1, 1,
    // 0x60 - 0x6F (` a-o)
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    // 0x70 - 0x7F (p-z { | } ~ DEL)
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 1, 0, 1, 0,
    // 0x80 - 0x8F (Extended ASCII - Invalid)
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    // 0x90 - 0x9F
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    // 0xA0 - 0xAF
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    // 0xB0 - 0xBF
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    // 0xC0 - 0xCF
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    // 0xD0 - 0xDF
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    // 0xE0 - 0xEF
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    // 0xF0 - 0xFF
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

pub const fn is_valid_tchar(byte: u8) -> bool {
    TCHAR_LOOKUP[byte as usize] == 1
}

/// Lookup table for RFC 9110 `field-vchar`.
/// 1 for valid field-vchar, 0 if invalid
#[rustfmt::skip]
const FIELD_VCHAR_LOOKUP: [u8; 256] = [
    // 0x00 - 0x0F (Control characters)
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    // 0x10 - 0x1F (Control characters)
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    // 0x20 (SP) - 0x2F (VCHAR starts at 0x21)
    0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    // 0x30 - 0x3F
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    // 0x40 - 0x4F
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    // 0x50 - 0x5F
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    // 0x60 - 0x6F
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    // 0x70 - 0x7F (0x7F is DEL)
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0,
    // 0x80 - 0x8F (obs-text starts)
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    // 0x90 - 0x9F
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    // 0xA0 - 0xAF
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    // 0xB0 - 0xBF
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    // 0xC0 - 0xCF
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    // 0xD0 - 0xDF
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    // 0xE0 - 0xEF
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    // 0xF0 - 0xFF
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
];

pub const fn is_valid_field_vchar(byte: u8) -> bool {
    FIELD_VCHAR_LOOKUP[byte as usize] == 1
}

#[cfg(test)]
mod test {
    use super::{is_valid_field_vchar, is_valid_tchar};

    #[test]
    fn field_name_char() {
        let punct = b"!#$%&'*+-.^_`|~".to_vec();
        let digits = (b'0'..=b'9').collect::<Vec<u8>>();
        let lalpha = (b'a'..=b'z').collect::<Vec<u8>>();
        let ualpha = (b'A'..=b'Z').collect::<Vec<u8>>();
        let all = punct
            .iter()
            .chain(&digits)
            .chain(&lalpha)
            .chain(&ualpha)
            .map(|v| v.to_owned())
            .collect::<Vec<u8>>();

        for b in 0u8..=255 {
            if all.contains(&b) {
                assert!(is_valid_tchar(b));
            } else {
                assert!(!is_valid_tchar(b));
            }
        }
    }

    #[test]
    fn field_vchar() {
        #[rustfmt::skip]
        let bad = [
            0x00, 0x01, 0x02, 0x03,
            0x04, 0x05, 0x06, 0x07,
            0x08, 0x09, 0x0A, 0x0B,
            0x0C, 0x0D, 0x0E, 0x0F,

            0x10, 0x11, 0x12, 0x13,
            0x14, 0x15, 0x16, 0x17,
            0x18, 0x19, 0x1A, 0x1B,
            0x1C, 0x1D, 0x1E, 0x1F,
            0x20,

            0x7F,
        ];

        for b in 0u8..=255 {
            if bad.contains(&b) {
                assert!(!is_valid_field_vchar(b));
            } else {
                assert!(is_valid_field_vchar(b));
            }
        }
    }
}
