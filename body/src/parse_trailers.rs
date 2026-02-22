use jrpxy_util::parse::{is_valid_field_vchar, is_valid_tchar};

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
                } else if is_valid_tchar(b) {
                    *self = Self::InToken;
                } else {
                    return Err(TrailerError::InvalidFieldName);
                }
            }
            Self::InToken => {
                if b == b':' {
                    *self = Self::SkipFieldWhitespace;
                } else if is_valid_tchar(b) {
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
    use super::{TrailerError, parse_trailers};

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
