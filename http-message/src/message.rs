use std::mem::MaybeUninit;

use bytes::{Bytes, BytesMut};
use httparse::{Request as HttparseRequest, Response as HttparseResponse};
use jrpxy_util::buffer::Buffer;

pub use httparse::Error as HttpParseError;

use crate::{
    framing::HeadFraming,
    header::{HeaderError, Headers},
    version::HttpVersion,
};

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

impl<'p, 'b: 'p, 'h: 'b> RequestOffset<'h> {
    fn parse(
        buf: &'b [u8],
        parse_headers: &mut [MaybeUninit<httparse::Header<'p>>],
        out_headers: &'h mut [MaybeUninit<HeaderOffset>],
    ) -> Result<Option<(Self, usize)>, httparse::Error> {
        debug_assert_eq!(parse_headers.len(), out_headers.len());

        let mut req = HttparseRequest::new(&mut []);
        match httparse::ParserConfig::default().parse_request_with_uninit_headers(
            &mut req,
            buf,
            parse_headers,
        ) {
            Ok(httparse::Status::Partial) => Ok(None),
            Err(e) => Err(e),
            Ok(httparse::Status::Complete(head_len)) => {
                let method = req.method.unwrap_or_default();
                let path = req.path.unwrap_or_default();
                let version =
                    HttpVersion::try_from(req.version.unwrap_or_default()).expect("FIX ME");

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

impl<'p, 'b: 'p, 'h: 'b> ResponseOffset<'h> {
    fn parse(
        buf: &'b [u8],
        parse_headers: &mut [MaybeUninit<httparse::Header<'p>>],
        out_headers: &'h mut [MaybeUninit<HeaderOffset>],
    ) -> Result<Option<(Self, usize)>, httparse::Error> {
        debug_assert_eq!(parse_headers.len(), out_headers.len());

        let mut res = HttparseResponse::new(&mut []);
        match httparse::ParserConfig::default().parse_response_with_uninit_headers(
            &mut res,
            buf,
            parse_headers,
        ) {
            Ok(httparse::Status::Partial) => Ok(None),
            Err(e) => Err(e),
            Ok(httparse::Status::Complete(head_len)) => {
                let version =
                    HttpVersion::try_from(res.version.unwrap_or_default()).expect("FIX ME");
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
    pub fn parse(buf: &mut Buffer, max_headers: usize) -> Result<Option<Self>, httparse::Error> {
        let mut parse_headers = vec![MaybeUninit::uninit(); max_headers];
        let mut out_headers = vec![MaybeUninit::uninit(); max_headers];
        match RequestOffset::parse(&*buf, &mut parse_headers, &mut out_headers)? {
            None => Ok(None),
            Some((req, head_len)) => {
                let head_buf = buf.split_to(head_len).freeze();
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

    pub fn get_header(&self, needle: &str) -> Option<&Bytes> {
        self.inner.headers.get_header(needle)
    }

    pub fn framing(&self) -> Result<HeadFraming, HeaderError> {
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
    pub fn parse(buf: &mut Buffer, max_headers: usize) -> Result<Option<Self>, httparse::Error> {
        let mut parse_headers = vec![MaybeUninit::uninit(); max_headers];
        let mut out_headers = vec![MaybeUninit::uninit(); max_headers];
        match ResponseOffset::parse(&*buf, &mut parse_headers, &mut out_headers)? {
            None => Ok(None),
            Some((res, head_len)) => {
                let head_buf = buf.split_to(head_len).freeze();
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

    pub fn get_header(&self, needle: &str) -> Option<&Bytes> {
        self.inner.headers.get_header(needle)
    }

    pub fn framing(&self) -> Result<HeadFraming, HeaderError> {
        self.inner.headers.framing()
    }

    pub fn is_informational(&self) -> Option<u16> {
        let c = self.code();
        (100..200).contains(&c).then_some(c)
    }

    pub fn into_version(mut self, frontend_version: HttpVersion) -> Self {
        self.inner.version = frontend_version;

        // TODO: strip out or convert response headers not supported by the
        // target version

        self
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

    pub fn build(&self) -> Request {
        let Self {
            method,
            path,
            version,
            headers: built_headers,
        } = self;

        // TODO: validate method, path, and headers
        // TODO: also make sure none of them are none

        let mut buf = BytesMut::with_capacity(128);

        buf.extend_from_slice(method.unwrap_or("GET").as_bytes());
        let method = buf.split().freeze();
        buf.extend_from_slice(path.unwrap_or("/").as_bytes());
        let path = buf.split().freeze();
        let version = version.unwrap_or(HttpVersion::Http10);

        let mut headers = Headers::with_capacity(built_headers.len());
        for (name, value) in built_headers {
            buf.extend_from_slice(name.as_bytes());
            let name = buf.split().freeze();
            buf.extend_from_slice(value.as_ref());
            let value = buf.split().freeze();
            headers.push(name, value);
        }

        Request {
            inner: Box::new(RequestInner {
                method,
                path,
                version,
                headers,
            }),
        }
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

    pub fn build(&self) -> Response {
        let Self {
            version,
            code,
            reason,
            headers: built_headers,
        } = self;

        // TODO: validate code, reason, and headers
        // TODO: also make sure none of them are none.
        // TODO: verify that a 1xx response follows content-length and transfer-encoding rules

        let mut buf = BytesMut::with_capacity(128);

        let version = version.to_owned().unwrap_or(HttpVersion::Http10);
        let code = code.to_owned().unwrap_or(200);
        buf.extend_from_slice(reason.unwrap_or("Unknown").as_bytes());
        let reason = buf.split().freeze();

        let mut headers = Headers::with_capacity(built_headers.len());
        for (name, value) in built_headers {
            buf.extend_from_slice(name.as_bytes());
            let name = buf.split().freeze();
            buf.extend_from_slice(value.as_ref());
            let value = buf.split().freeze();
            headers.push(name, value);
        }

        Response {
            inner: Box::new(ResponseInner {
                version,
                code,
                reason,
                headers,
            }),
        }
    }
}
