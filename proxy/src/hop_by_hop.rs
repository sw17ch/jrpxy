use bytes::{Bytes, BytesMut};
use jrpxy_http_message::header::Headers;

const PROXY_REMOVE_HEADERS: &[&[u8]] = &[
    // https://datatracker.ietf.org/doc/html/rfc9110#section-7.6.1
    b"proxy-connection",
    b"keep-alive",
    b"te",
    b"transfer-encoding",
    b"upgrade",
    b"connection",
    // https://datatracker.ietf.org/doc/html/rfc9110#section-11.7.1
    b"proxy-authenticate",
    // https://datatracker.ietf.org/doc/html/rfc9110#section-11.7.2
    b"proxy-authorization",
    // i remove this as we'll always re-insert it (or use chunked encoding)
    b"content-length",
];

pub struct HopByHopInfo {
    opts: Option<ConnectionOptions>,
    removed: Headers,
}

impl HopByHopInfo {
    pub fn new(headers: &mut Headers) -> Self {
        let opts = ConnectionOptions::new(headers);
        let removed = headers.remove(|(n, _v)| {
            for c in PROXY_REMOVE_HEADERS.iter() {
                if n.eq_ignore_ascii_case(c) {
                    return true;
                }
            }
            if let Some(opts) = opts.as_ref() {
                for c in opts.iter() {
                    if n.eq_ignore_ascii_case(c) {
                        return true;
                    }
                }
            }

            false
        });
        Self { opts, removed }
    }

    pub fn headers(&self) -> &Headers {
        &self.removed
    }

    pub fn connection_options(&self) -> Option<&ConnectionOptions> {
        self.opts.as_ref()
    }
}

#[derive(Copy, Clone)]
pub enum KeepAlive {
    KeepAlive,
    Close,
    Absent,
}

impl KeepAlive {
    fn new(keep_alive: bool, close: bool) -> Self {
        match (keep_alive, close) {
            // TODO: maybe flag that we saw keep-alive and close in the same
            // connection header? are there other suspicious combinations?
            (true, true) => KeepAlive::Close,
            (true, false) => KeepAlive::KeepAlive,
            (false, true) => KeepAlive::Close,
            (false, false) => KeepAlive::Absent,
        }
    }
}

pub struct ConnectionOptions {
    opts: Vec<Bytes>,
    keep_alive: KeepAlive,
}

impl ConnectionOptions {
    pub fn new(headers: &Headers) -> Option<Self> {
        let v = headers.get_header("connection")?;

        let mut v = BytesMut::from(&v[..]);
        for i in v.iter_mut() {
            i.make_ascii_lowercase();
        }
        let v = v.freeze();

        let mut keep_alive = false;
        let mut close = false;

        let mut opts = v
            .split(|i| *i == b',')
            .map(|o| {
                let o = o.trim_ascii();
                if o == b"keep-alive" {
                    keep_alive = true;
                } else if o == b"close" {
                    close = true;
                }
                v.slice_ref(o)
            })
            .collect::<Vec<_>>();
        opts.sort();
        opts.dedup();

        Some(Self {
            opts,
            keep_alive: KeepAlive::new(keep_alive, close),
        })
    }

    pub fn iter(&self) -> ConnectionOptionIter<'_> {
        ConnectionOptionIter {
            iter: self.opts.iter(),
        }
    }

    pub fn keep_alive(&self) -> KeepAlive {
        self.keep_alive
    }
}

pub struct ConnectionOptionIter<'i> {
    iter: std::slice::Iter<'i, Bytes>,
}

impl<'i> Iterator for ConnectionOptionIter<'i> {
    type Item = &'i Bytes;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next()
    }
}
