/// Represents the three different ways an HTTP message can be framed.
pub enum HeadFraming {
    /// The message body has a defined length specified by the `content-length:
    /// <len>` header
    Length(u64),
    /// The message body is of indeterminate length specified by the
    /// `transfer-encoding: chunked` header
    Chunked,
    /// The message does not contain either of `content-length` or
    /// `transfer-encoding: chunked`
    NoFraming,
}
impl HeadFraming {
    pub fn is_no_framing(&self) -> bool {
        matches!(self, Self::NoFraming)
    }
}
