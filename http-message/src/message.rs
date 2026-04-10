use std::mem::MaybeUninit;

use bytes::{Bytes, BytesMut};
use httparse::{Request as HttparseRequest, Response as HttparseResponse};
use jrpxy_util::{io_buffer::BytesReader, parse::is_valid_tchar};

pub use httparse::Error as HttpParseError;

use crate::{
    framing::ParsedFraming,
    header::{HeaderError, Headers},
    version::HttpVersion,
};

#[derive(thiserror::Error, Debug)]
pub enum MessageError {
    #[error("Error parsing HTTP: {0}")]
    Parse(#[from] HttpParseError),
    #[error("Unsupported http version: {0}")]
    HttpVersion(u8),
    #[error("Cannot build: {0}")]
    BuildError(#[from] BuildError),
}

pub type MessageResult<T> = Result<T, MessageError>;

#[derive(thiserror::Error, Debug)]
pub enum BuildError {
    #[error("Method not specified")]
    MissingMethod,
    #[error("Path not specified")]
    MissingPath,
    #[error("Version not specified")]
    MissingVersion,
    #[error("Code is not specified")]
    MissingCode,
    #[error("Result is not specified")]
    MissingResult,
    #[error("Method contains invalid characters (RFC9110, 9.1)")]
    InvalidMethod(String),
}

/// A buffer type that carries no type or lifetime information that has the same
/// size and alignment as [`httparse::Header`]. We use this type to avoid
/// lifetime problems that usually arise from trying to reuse header slot
/// allocations across parse attempts.
mod header_buf {
    use std::mem::MaybeUninit;

    use httparse::Header;

    /// A struct used for allocation only. Its internal representation should
    /// never be used.
    #[derive(Copy, Clone)]
    pub(crate) struct HeaderBuf {
        _do_not_use_name: &'static str,
        _do_not_use_value: &'static [u8],
    }
    // These cases must be true for this to be safe.
    const _HEADER_BUF_SIZE_MATCH: () = const {
        assert!(std::mem::size_of::<HeaderBuf>() == std::mem::size_of::<httparse::Header<'_>>());
        assert!(std::mem::align_of::<HeaderBuf>() == std::mem::align_of::<httparse::Header<'_>>());
    };

    /// Parse `buf` into `req` using `header_slots` as the space into which we
    /// will record headers. The lifetime of `req` ensures that `header_slots`
    /// and `buf` are alive long enough for it to be safe to access the headers
    /// from `req` once parsed.
    pub(crate) fn parse_request<'b, 'h>(
        buf: &'b [u8],
        header_slots: &'h mut [MaybeUninit<HeaderBuf>],
        req: &mut httparse::Request<'h, 'b>,
    ) -> Result<httparse::Status<usize>, httparse::Error> {
        // SAFETY: HeaderBuf and Header<'b> have the same size and alignment
        // (verified by the const assertion above). The slice resulting from the
        // cast does not escape this function; it is consumed immediately by
        // httparse, which writes Header<'b> values whose 'b is tied to `buf`.
        let headers = unsafe {
            let ptr = header_slots.as_mut_ptr() as *mut MaybeUninit<Header<'b>>;
            std::slice::from_raw_parts_mut(ptr, header_slots.len())
        };
        httparse::ParserConfig::default().parse_request_with_uninit_headers(req, buf, headers)
    }

    /// See docs for [`parse_request`].
    pub(crate) fn parse_response<'b>(
        buf: &'b [u8],
        header_slots: &mut [MaybeUninit<HeaderBuf>],
        res: &mut httparse::Response<'_, 'b>,
    ) -> Result<httparse::Status<usize>, httparse::Error> {
        // SAFETY: same as parse_request.
        let headers = unsafe {
            let ptr = header_slots.as_mut_ptr() as *mut MaybeUninit<Header<'b>>;
            std::slice::from_raw_parts_mut(ptr, header_slots.len())
        };
        httparse::ParserConfig::default().parse_response_with_uninit_headers(res, buf, headers)
    }

    /// Same as [`parse_request`], but for just a buffer of headers as might
    /// appear in HTTP trailers.
    pub(crate) fn parse_headers<'b, 's>(
        buf: &'b [u8],
        header_slots: &'s mut [MaybeUninit<HeaderBuf>],
    ) -> Result<httparse::Status<(usize, &'s [Header<'b>])>, httparse::Error> {
        // SAFETY: same as parse_request.
        let headers = unsafe {
            let ptr = header_slots.as_mut_ptr() as *mut MaybeUninit<Header<'b>>;
            std::slice::from_raw_parts_mut(ptr, header_slots.len())
        };
        let headers = unsafe { headers.assume_init_mut() };

        // TODO: Sure would be nice if this was a method available on ParserConfig.
        httparse::parse_headers(buf, headers)
    }
}
use header_buf::HeaderBuf;

pub struct ParseSlots {
    parse_headers: Vec<MaybeUninit<HeaderBuf>>,
    out_headers: Vec<MaybeUninit<HeaderOffset>>,
}

impl Default for ParseSlots {
    fn default() -> Self {
        Self::new(16)
    }
}

impl std::fmt::Debug for ParseSlots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let len = self.parse_headers.len();
        f.debug_tuple("ParseSlots").field(&len).finish()
    }
}

impl ParseSlots {
    pub fn new(slot_count: usize) -> Self {
        Self {
            parse_headers: vec![MaybeUninit::uninit(); slot_count],
            out_headers: vec![MaybeUninit::uninit(); slot_count],
        }
    }

    pub fn parse_request<I>(&mut self, buf: &mut BytesReader<I>) -> MessageResult<Option<Request>> {
        let Self {
            parse_headers,
            out_headers,
        } = self;
        match RequestOffset::parse(buf.as_bytes(), parse_headers, out_headers)? {
            None => Ok(None),
            Some((req, head_len)) => {
                let head_buf = buf.split_to(head_len);
                let method = req.method.slice_from(&head_buf);
                let path = req.path.slice_from(&head_buf);
                let version = req.version;
                let headers = populate_headers(req.headers, &head_buf);
                Ok(Some(Request {
                    inner: Box::new(RequestInner {
                        method,
                        path,
                        version,
                        headers,
                    }),
                }))
            }
        }
    }

    pub fn parse_response<I>(
        &mut self,
        buf: &mut BytesReader<I>,
    ) -> MessageResult<Option<Response>> {
        let Self {
            parse_headers,
            out_headers,
        } = self;
        match ResponseOffset::parse(buf.as_bytes(), parse_headers, out_headers)? {
            None => Ok(None),
            Some((res, head_len)) => {
                let head_buf = buf.split_to(head_len);
                let version = res.version;
                let code = res.code;
                let reason = res.reason.slice_from(&head_buf);
                let headers = populate_headers(res.headers, &head_buf);
                Ok(Some(Response {
                    inner: Box::new(ResponseInner {
                        version,
                        code,
                        reason,
                        headers,
                    }),
                }))
            }
        }
    }

    pub fn parse_headers<I>(&mut self, buf: &mut BytesReader<I>) -> MessageResult<Option<Headers>> {
        let Self {
            parse_headers,
            out_headers,
        } = self;
        match header_buf::parse_headers(buf.as_bytes(), parse_headers)? {
            httparse::Status::Partial => Ok(None),
            httparse::Status::Complete((len, headers)) => {
                let out_headers = &mut out_headers[..headers.len()];
                let header_offsets =
                    populate_header_offsets(&buf.as_bytes()[..len], out_headers, headers);
                let header_buf = buf.split_to(len);
                let headers = populate_headers(header_offsets, &header_buf);
                Ok(Some(headers))
            }
        }
    }
}

#[derive(Copy, Clone)]
struct Span {
    offset: usize,
    len: usize,
}

impl Span {
    /// Create a span from a sub-slice's position within a buffer. `sub` *must*
    /// be fully contained by `buf`.
    fn from_subslice(buf: &[u8], sub: &[u8]) -> Self {
        let buf_start = buf.as_ptr() as usize;
        let sub_start = sub.as_ptr() as usize;
        assert!(sub_start >= buf_start);
        assert!(sub_start + sub.len() <= buf_start + buf.len());
        Self {
            offset: sub_start - buf_start,
            len: sub.len(),
        }
    }

    /// Slice the span from a `Bytes` buffer. This should only be used with the
    /// original buffer from which the [`Span`] was derived (or an identical
    /// copy of that buffer).
    ///
    /// # Panics
    ///
    /// Panics if this [`Span`] does not validly slice `buf`.
    fn slice_from(&self, buf: &Bytes) -> Bytes {
        buf.slice(self.offset..self.offset + self.len)
    }
}

#[derive(Copy, Clone)]
struct HeaderOffset {
    name: Span,
    value: Span,
}

struct RequestOffset<'h> {
    method: Span,
    path: Span,
    version: HttpVersion,
    headers: &'h [HeaderOffset],
}

impl<'h> RequestOffset<'h> {
    fn parse(
        buf: &[u8],
        parse_headers: &mut [MaybeUninit<HeaderBuf>],
        out_headers: &'h mut [MaybeUninit<HeaderOffset>],
    ) -> MessageResult<Option<(Self, usize)>> {
        debug_assert_eq!(parse_headers.len(), out_headers.len());

        let mut req = HttparseRequest::new(&mut []);
        match header_buf::parse_request(buf, parse_headers, &mut req) {
            Ok(httparse::Status::Partial) => Ok(None),
            Err(e) => Err(e.into()),
            Ok(httparse::Status::Complete(head_len)) => {
                let method = req.method.unwrap_or_default();
                let path = req.path.unwrap_or_default();
                let version = HttpVersion::try_from(req.version.unwrap_or_default())
                    .map_err(MessageError::HttpVersion)?;

                let out_headers = &mut out_headers[0..req.headers.len()];
                let headers = populate_header_offsets(buf, out_headers, req.headers);

                let p = RequestOffset {
                    method: Span::from_subslice(buf, method.as_bytes()),
                    path: Span::from_subslice(buf, path.as_bytes()),
                    version,
                    headers,
                };

                Ok(Some((p, head_len)))
            }
        }
    }
}

struct ResponseOffset<'h> {
    version: HttpVersion,
    code: u16,
    reason: Span,
    headers: &'h [HeaderOffset],
}

impl<'h> ResponseOffset<'h> {
    fn parse(
        buf: &[u8],
        parse_headers: &mut [MaybeUninit<HeaderBuf>],
        out_headers: &'h mut [MaybeUninit<HeaderOffset>],
    ) -> MessageResult<Option<(Self, usize)>> {
        debug_assert_eq!(parse_headers.len(), out_headers.len());

        let mut res = HttparseResponse::new(&mut []);
        match header_buf::parse_response(buf, parse_headers, &mut res) {
            Ok(httparse::Status::Partial) => Ok(None),
            Err(e) => Err(e.into()),
            Ok(httparse::Status::Complete(head_len)) => {
                let version = HttpVersion::try_from(res.version.unwrap_or_default())
                    .map_err(MessageError::HttpVersion)?;
                let code = res.code.unwrap_or_default();
                let reason = res.reason.unwrap_or_default();

                let out_headers = &mut out_headers[0..res.headers.len()];
                let headers = populate_header_offsets(buf, out_headers, res.headers);

                let p = ResponseOffset {
                    version,
                    code,
                    reason: Span::from_subslice(buf, reason.as_bytes()),
                    headers,
                };

                Ok(Some((p, head_len)))
            }
        }
    }
}

/// Populate [`HeaderOffset`] from parsed headers.
fn populate_header_offsets<'o, 'b>(
    buf: &'b [u8],
    out_headers: &'o mut [MaybeUninit<HeaderOffset>],
    in_headers: &[httparse::Header<'b>],
) -> &'o mut [HeaderOffset] {
    assert_eq!(in_headers.len(), out_headers.len());
    for (o, i) in out_headers.iter_mut().zip(in_headers) {
        o.write(HeaderOffset {
            name: Span::from_subslice(buf, i.name.as_bytes()),
            value: Span::from_subslice(buf, i.value),
        });
    }
    // SAFETY: We just initialized all elements in the loop above.
    unsafe { out_headers.assume_init_mut() }
}

struct RequestInner {
    method: Bytes,
    path: Bytes,
    version: HttpVersion,

    headers: Headers,
}

pub struct Request {
    inner: Box<RequestInner>,
}

impl Request {
    pub fn method(&self) -> &Bytes {
        &self.inner.method
    }

    pub fn path(&self) -> &Bytes {
        &self.inner.path
    }

    pub fn version(&self) -> HttpVersion {
        self.inner.version
    }

    pub fn headers(&self) -> &Headers {
        &self.inner.headers
    }

    pub fn headers_mut(&mut self) -> &mut Headers {
        &mut self.inner.headers
    }

    pub fn framing(&self) -> Result<ParsedFraming, HeaderError> {
        self.inner.headers.framing()
    }

    /// Updates self to remove headers matching the predicate. Returns the
    /// removed headers.
    pub fn remove_headers<P>(&mut self, pred: P) -> Headers
    where
        P: FnMut(&(Bytes, Bytes)) -> bool,
    {
        self.inner.headers.remove(pred)
    }
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut f = f.debug_struct("Request");
        f.field(":method", &self.inner.method)
            .field(":path", &self.inner.path)
            .field(":version", &self.inner.version);
        for (n, v) in self.inner.headers.iter() {
            let Ok(n) = std::str::from_utf8(n) else {
                continue;
            };
            f.field(n, v);
        }
        f.finish()
    }
}

struct ResponseInner {
    version: HttpVersion,
    code: u16,
    reason: Bytes,

    headers: Headers,
}

pub struct Response {
    inner: Box<ResponseInner>,
}

impl Response {
    pub fn version(&self) -> HttpVersion {
        self.inner.version
    }

    pub fn code(&self) -> u16 {
        self.inner.code
    }

    pub fn reason(&self) -> &Bytes {
        &self.inner.reason
    }

    pub fn headers(&self) -> &Headers {
        &self.inner.headers
    }

    pub fn headers_mut(&mut self) -> &mut Headers {
        &mut self.inner.headers
    }

    pub fn framing(&self) -> Result<ParsedFraming, HeaderError> {
        self.inner.headers.framing()
    }

    pub fn is_informational(&self) -> Option<u16> {
        let c = self.code();
        (100..200).contains(&c).then_some(c)
    }
}

impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut f = f.debug_struct("Response");
        f.field(":version", &self.inner.version)
            .field(":code", &self.inner.code)
            .field(":reason", &self.inner.reason);
        for (n, v) in self.inner.headers.iter() {
            let Ok(n) = std::str::from_utf8(n) else {
                continue;
            };
            f.field(n, v);
        }
        f.finish()
    }
}

fn populate_headers(header_offsets: &[HeaderOffset], head_buf: &Bytes) -> Headers {
    let mut headers = Headers::with_capacity(header_offsets.len());
    for h in header_offsets.iter() {
        let name = h.name.slice_from(head_buf);
        let value = h.value.slice_from(head_buf);
        headers.push(name, value);
    }
    headers
}

pub struct InformationalResponse(Response);

impl InformationalResponse {
    pub fn into_inner(self) -> Response {
        let Self(r) = self;
        r
    }
}

impl From<Response> for InformationalResponse {
    fn from(value: Response) -> Self {
        Self(value)
    }
}

#[derive(Debug, Default)]
pub struct RequestBuilder<'s> {
    method: Option<&'s str>,
    path: Option<&'s str>,
    version: Option<HttpVersion>,
    headers: Vec<(&'s str, &'s [u8])>,
}

impl<'s> RequestBuilder<'s> {
    pub fn new(initial_header_count: usize) -> Self {
        Self {
            method: None,
            path: None,
            version: None,
            headers: Vec::with_capacity(initial_header_count),
        }
    }

    pub fn with_method(&mut self, method: &'s str) -> &mut Self {
        self.method = Some(method);
        self
    }

    pub fn with_path(&mut self, path: &'s str) -> &mut Self {
        self.path = Some(path);
        self
    }

    pub fn with_version(&mut self, version: HttpVersion) -> &mut Self {
        self.version = Some(version);
        self
    }

    pub fn with_header(&mut self, name: &'s str, value: &'s [u8]) -> &mut Self {
        self.headers.push((name, value));
        self
    }

    pub fn build(&self) -> Result<Request, BuildError> {
        let Self {
            method,
            path,
            version,
            headers: built_headers,
        } = self;

        let Some(method) = method else {
            return Err(BuildError::MissingMethod);
        };
        let Some(path) = path else {
            return Err(BuildError::MissingPath);
        };
        let Some(version) = version else {
            return Err(BuildError::MissingVersion);
        };

        if !method.bytes().all(is_valid_tchar) {
            return Err(BuildError::InvalidMethod(method.to_string()));
        }

        // TODO: validate path, and headers

        let mut buf = BytesMut::with_capacity(128);

        buf.extend_from_slice(method.as_bytes());
        let method = buf.split().freeze();
        buf.extend_from_slice(path.as_bytes());
        let path = buf.split().freeze();

        let mut headers = Headers::with_capacity(built_headers.len());
        for (name, value) in built_headers {
            buf.extend_from_slice(name.as_bytes());
            let name = buf.split().freeze();
            buf.extend_from_slice(value.as_ref());
            let value = buf.split().freeze();
            headers.push(name, value);
        }

        Ok(Request {
            inner: Box::new(RequestInner {
                method,
                path,
                version: *version,
                headers,
            }),
        })
    }
}

#[derive(Debug, Default)]
pub struct ResponseBuilder<'s> {
    version: Option<HttpVersion>,
    code: Option<u16>,
    reason: Option<&'s str>,
    headers: Vec<(&'s str, &'s [u8])>,
}

impl<'s> ResponseBuilder<'s> {
    pub fn new(initial_header_capacity: usize) -> Self {
        Self {
            version: None,
            code: None,
            reason: None,
            headers: Vec::with_capacity(initial_header_capacity),
        }
    }

    pub fn with_version(&mut self, version: HttpVersion) -> &mut Self {
        self.version = Some(version);
        self
    }

    pub fn with_code(&mut self, code: u16) -> &mut Self {
        self.code = Some(code);
        self
    }

    pub fn with_reason(&mut self, reason: &'s str) -> &mut Self {
        self.reason = Some(reason);
        self
    }

    pub fn with_header(&mut self, name: &'s str, value: &'s [u8]) -> &mut Self {
        self.headers.push((name, value));
        self
    }

    pub fn build(&self) -> Result<Response, BuildError> {
        let Self {
            version,
            code,
            reason,
            headers: built_headers,
        } = self;

        let Some(version) = version else {
            return Err(BuildError::MissingVersion);
        };
        let Some(code) = code else {
            return Err(BuildError::MissingCode);
        };
        let Some(reason) = reason else {
            return Err(BuildError::MissingResult);
        };

        // TODO: validate code, reason, and headers
        // TODO: verify that a 1xx response follows content-length and transfer-encoding rules

        let mut buf = BytesMut::with_capacity(128);

        let code = code.to_owned();
        buf.extend_from_slice(reason.as_bytes());
        let reason = buf.split().freeze();

        let mut headers = Headers::with_capacity(built_headers.len());
        for (name, value) in built_headers {
            buf.extend_from_slice(name.as_bytes());
            let name = buf.split().freeze();
            buf.extend_from_slice(value.as_ref());
            let value = buf.split().freeze();
            headers.push(name, value);
        }

        Ok(Response {
            inner: Box::new(ResponseInner {
                version: *version,
                code,
                reason,
                headers,
            }),
        })
    }
}

#[cfg(test)]
mod test {
    use crate::message::BuildError;

    use super::RequestBuilder;

    #[test]
    fn build_with_invalid_method() {
        let mut b = RequestBuilder::new(0);
        b.with_method("GET OHNOSPACE")
            .with_path("/")
            .with_version(crate::version::HttpVersion::Http11);
        let req = b.build().unwrap_err();
        if let BuildError::InvalidMethod(v) = &req
            && v == "GET OHNOSPACE"
        {
            // good
        } else {
            panic!("unexpected error: {req:?}")
        };
    }
}
