/// A representation of the supported HTTP versions.
#[derive(Debug, Default, Copy, Clone, PartialEq, PartialOrd)]
pub enum HttpVersion {
    // HTTP/1.0
    Http10 = 0,
    // HTTP/1.1
    #[default]
    Http11 = 1,
}
impl HttpVersion {
    /// True when the version supports an informational (1xx) response.
    pub fn supports_informational_response(&self) -> bool {
        match self {
            HttpVersion::Http10 => false,
            HttpVersion::Http11 => true,
        }
    }

    /// A `&'static str` representation of the version.
    pub fn to_static(&self) -> &'static str {
        match self {
            HttpVersion::Http10 => "HTTP/1.0",
            HttpVersion::Http11 => "HTTP/1.1",
        }
    }
}

impl TryFrom<u8> for HttpVersion {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Http10),
            1 => Ok(Self::Http11),
            _ => Err(value),
        }
    }
}

impl TryFrom<&u8> for HttpVersion {
    type Error = u8;

    fn try_from(value: &u8) -> Result<Self, Self::Error> {
        Self::try_from(*value)
    }
}
