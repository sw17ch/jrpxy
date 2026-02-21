use jrpxy_frontend::FrontendRequest;
use jrpxy_http_message::message::Request;

use crate::hop_by_hop::HopByHopInfo;

pub struct ProxyRequest<IO> {
    req: FrontendRequest<IO>,
    info: HopByHopInfo,
}

impl<IO> From<FrontendRequest<IO>> for ProxyRequest<IO> {
    fn from(value: FrontendRequest<IO>) -> Self {
        Self::new(value)
    }
}

impl<IO> ProxyRequest<IO> {
    /// Create a [`ProxyRequest`] from the [`FrontendRequest`]. This will
    /// *mutate* the frontend request by stripping out hop-by-hop headers, and
    /// inserting other headers required of proxies.
    pub fn new(mut req: FrontendRequest<IO>) -> Self {
        let info = HopByHopInfo::new(req.req_mut().headers_mut());
        Self { req, info }
    }

    pub fn req(&self) -> &Request {
        self.req.req()
    }

    pub fn hop_by_hop_info(&self) -> &HopByHopInfo {
        &self.info
    }
}

#[cfg(test)]
mod test {
    use bytes::BytesMut;
    use jrpxy_frontend::FrontendReader;

    use crate::request::ProxyRequest;

    #[tokio::test]
    async fn proxy_request_strips_hop_by_hop() {
        let frontend_request_buf = b"\
            POST / HTTP/1.1\r\n\
            content-length: 5\r\n\
            connection: keep-alive, listed\r\n\
            keep-alive: param=5\r\n\
            not-hop-by-hop: 1\r\n\
            listed: 2\r\n\
            \r\n\
            01234\
            ";

        let frontend_reader = FrontendReader::new(&frontend_request_buf[..], 8192);
        let frontend_request = frontend_reader
            .read()
            .await
            .expect("failed to read request head");
        let proxy_request = ProxyRequest::new(frontend_request);

        let mut proxy_request_headers = proxy_request
            .req()
            .headers()
            .iter()
            .map(|h| h.to_owned())
            .collect::<Vec<_>>();
        proxy_request_headers.sort();
        assert_eq!(
            vec![(
                BytesMut::from(b"not-hop-by-hop".as_ref()).freeze(),
                BytesMut::from(b"1".as_ref()).freeze()
            )],
            proxy_request_headers
        );
    }
}
