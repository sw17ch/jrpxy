use jrpxy_util::io_buffer::ReaderBufferError;

#[derive(Debug, thiserror::Error)]
pub enum TrailerError {
    #[error("Invalid trailer field: {0}")]
    InvalidField(httparse::Error),
    #[error("Too many trailer fields")]
    TooManyFields,
}

#[derive(Debug, thiserror::Error)]
pub enum BodyError {
    #[error("Attempted to write more than the maximum allowed bytes: {0}")]
    BodyOverflow(u64),
    #[error("Could not write body: {0}")]
    BodyWriteError(std::io::Error),
    #[error("Could not read body: {0}")]
    BodyReadError(std::io::Error),
    #[error("Body expected {expected} bytes, but only has {actual} bytes")]
    IncompleteBody { expected: u64, actual: u64 },
    #[error("Invalid chunk header: {0}")]
    InvalidChunkHeader(httparse::InvalidChunkSize),
    #[error("Invalid HTTP trailer: {0}")]
    TrailerError(#[from] TrailerError),
    #[error("Unexpected EOF")]
    UnexpectedEOF,
    #[error("Invalid chunk footer: expected 0x{0:x} got 0x{1:x}")]
    InvalidChunkFooter(u8, u8),
    #[error("Attempted to read after previous failure")]
    ReadAfterError,
    #[error("Attempted to write after previous failure")]
    WriteAfterError,
    #[error("Attempted to forward after previous failure")]
    ForwardAfterError,
}

impl From<ReaderBufferError> for BodyError {
    fn from(e: ReaderBufferError) -> Self {
        match e {
            ReaderBufferError::Io(e) => BodyError::BodyReadError(e),
            ReaderBufferError::UnexpectedEOF => BodyError::UnexpectedEOF,
        }
    }
}

pub type BodyResult<T> = Result<T, BodyError>;
