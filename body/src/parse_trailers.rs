/// RFC 9110 Field-Name Character Lookup Table
/// Index = Byte Value (0-255)
/// Value = 1 if valid tchar, 0 if invalid
#[rustfmt::skip]
const FIELD_NAME_LOOKUP: [u8; 256] = [
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

const fn is_valid_field_name_char(byte: u8) -> bool {
    if false {
        FIELD_NAME_LOOKUP[byte as usize] == 1
    } else {
        match byte {
            0x00..=0x1F => false,
            b' ' => false,
            b'!' => true,
            b'"' => false,
            b'#' => true,
            b'$' => true,
            b'%' => true,
            b'&' => true,
            b'\'' => true,
            b'(' => false,
            b')' => false,
            b'*' => true,
            b'+' => true,
            b',' => false,
            b'-' => true,
            b'.' => true,
            b'/' => false,
            b'0'..=b'9' => true,
            b':' => false,
            b';' => false,
            b'<' => false,
            b'=' => false,
            b'>' => false,
            b'?' => false,
            b'@' => false,
            b'A'..=b'Z' => true,
            b'[' => false,
            b'\\' => false,
            b']' => false,
            b'^' => true,
            b'_' => true,
            b'`' => true,
            b'a'..=b'z' => true,
            b'{' => false,
            b'|' => true,
            b'}' => false,
            b'~' => true,
            0x7F..=0xFF => false,
        }
    }
}

/// Lookup table for RFC 9110 `field-vchar`.
/// 1 for VCHAR (0x21-0x7E) and obs-text (0x80-0xFF).
/// 0 for CTLs, SP (0x20), and DEL (0x7F).
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

const fn is_valid_field_vchar(byte: u8) -> bool {
    if false {
        FIELD_VCHAR_LOOKUP[byte as usize] == 1
    } else {
        match byte {
            // 0x00 - 0x0F (Control characters)
            0x00..=0x1F => false,
            // 0x20 (SP)
            0x20 => false,
            // 0x2F VCHAR starts at 0x21
            0x21..=0x7E => true,
            // 0x7F is DEL
            0x7F => false,
            // 0x2F VCHAR runs through 0xFF
            0x80..=0xFF => true,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TrailerError {
    #[error("Invalid field name")]
    InvalidFieldName,
    #[error("Invalid field value")]
    InvalidFieldValue,
    #[error("Invalid trailer footer")]
    InvalidTrailerFooter,
}

#[derive(Default)]
enum TrailerParser {
    #[default]
    Start,
    InToken,
    SkipFieldWhitespace,
    InFieldValue {
        can_end: bool,
    },
    NeedFieldLineNL,
    NeedFinalNL,
}

impl TrailerParser {
    fn give(&mut self, b: u8) -> Result<bool, TrailerError> {
        match self {
            Self::Start => {
                if b == b'\r' {
                    *self = Self::NeedFinalNL;
                } else if is_valid_field_name_char(b) {
                    *self = Self::InToken;
                } else {
                    return Err(TrailerError::InvalidFieldName);
                }
            }
            Self::InToken => {
                if b == b':' {
                    *self = Self::SkipFieldWhitespace;
                } else if is_valid_field_name_char(b) {
                    // no change to state
                } else {
                    return Err(TrailerError::InvalidFieldName);
                }
            }
            Self::SkipFieldWhitespace => {
                if b == b'\t' || b == b' ' {
                    // no change to state
                } else if is_valid_field_vchar(b) {
                    *self = Self::InFieldValue { can_end: true };
                } else {
                    return Err(TrailerError::InvalidFieldValue);
                }
            }
            Self::InFieldValue { can_end: true } => {
                if b == b'\r' {
                    *self = Self::NeedFieldLineNL;
                } else if b == b'\t' || b == b' ' {
                    *self = Self::InFieldValue { can_end: false }
                } else if is_valid_field_vchar(b) {
                    *self = Self::InFieldValue { can_end: true }
                } else {
                    return Err(TrailerError::InvalidFieldValue);
                }
            }
            Self::InFieldValue { can_end: false } => {
                if b == b'\t' || b == b' ' {
                    *self = Self::InFieldValue { can_end: false }
                } else if is_valid_field_vchar(b) {
                    *self = Self::InFieldValue { can_end: true }
                } else {
                    return Err(TrailerError::InvalidFieldValue);
                }
            }
            Self::NeedFieldLineNL => {
                if b == b'\n' {
                    *self = Self::Start;
                } else {
                    return Err(TrailerError::InvalidFieldValue);
                }
            }
            Self::NeedFinalNL => {
                if b == b'\n' {
                    return Ok(true);
                } else {
                    return Err(TrailerError::InvalidTrailerFooter);
                }
            }
        }
        Ok(false)
    }
}

pub fn parse_trailers(buf: &[u8]) -> Result<httparse::Status<usize>, TrailerError> {
    let mut tp = TrailerParser::default();
    for (ix, &b) in buf.iter().enumerate() {
        if tp.give(b)? {
            return Ok(httparse::Status::Complete(ix + 1));
        }
    }
    Ok(httparse::Status::Partial)
}

#[cfg(test)]
mod test {
    use crate::parse_trailers::TrailerError;

    use super::{is_valid_field_name_char, is_valid_field_vchar, parse_trailers};

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
                assert!(is_valid_field_name_char(b));
            } else {
                assert!(!is_valid_field_name_char(b));
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

    #[test]
    fn one_trailer() {
        let buf = b"\
            hello:world\r\n\
            \r\n\
            rest";
        let buf = &buf[..];

        let parsed = parse_trailers(buf).expect("failed to parse");
        assert_eq!(httparse::Status::Complete(15), parsed);
    }

    #[test]
    fn several_trailer() {
        let buf = b"\
            hello: world\r\n\
            look: there\r\n\
            \r\n\
            rest";
        let buf = &buf[..];

        let parsed = parse_trailers(buf).expect("failed to parse");
        assert_eq!(httparse::Status::Complete(29), parsed);
    }

    #[test]
    fn partial_trailer() {
        // stops part way through field-name
        let buf = b"\
            hello: world\r\n\
            loo\
            ";
        let buf = &buf[..];

        let parsed = parse_trailers(buf).expect("failed to parse");
        assert_eq!(httparse::Status::Partial, parsed);

        // stops part way through a field-value
        let buf = b"\
            hello: world\r\n\
            look: the";
        let buf = &buf[..];

        let parsed = parse_trailers(buf).expect("failed to parse");
        assert_eq!(httparse::Status::Partial, parsed);

        // missing final \n
        let buf = b"\
            hello: world\r\n\
            look: there\r\n\
            \r\
            ";
        let buf = &buf[..];

        let parsed = parse_trailers(buf).expect("failed to parse");
        assert_eq!(httparse::Status::Partial, parsed);
    }

    #[test]
    fn invalid_trailer() {
        // illegal name byte (DEL)
        let buf = b"\
            he/llo: world\r\n\
            \r\n\
            rest";
        let buf = &buf[..];
        assert!(matches!(
            parse_trailers(buf).unwrap_err(),
            TrailerError::InvalidFieldName
        ));

        // trailer line with space before ':'
        let buf = b"\
            hello : world\r\n\
            \r\n\
            rest";
        let buf = &buf[..];
        assert!(matches!(
            parse_trailers(buf).unwrap_err(),
            TrailerError::InvalidFieldName
        ));

        // value with trailing space
        let buf = b"\
            hello: world \r\n\
            \r\n\
            rest";
        let buf = &buf[..];
        assert!(matches!(
            parse_trailers(buf).unwrap_err(),
            TrailerError::InvalidFieldValue
        ));

        // value with illegal byte (DEL)
        let buf = b"\
            hello: wor\x7Fld\r\n\
            \r\n\
            rest";
        let buf = &buf[..];
        assert!(matches!(
            parse_trailers(buf).unwrap_err(),
            TrailerError::InvalidFieldValue
        ));
    }
}
