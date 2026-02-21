use jrpxy_backend::BackendResponse;
use jrpxy_http_message::message::Response;

use crate::hop_by_hop::HopByHopInfo;

pub struct ProxyResponse<IO> {
    res: BackendResponse<IO>,
    info: HopByHopInfo,
}

impl<IO> From<BackendResponse<IO>> for ProxyResponse<IO> {
    fn from(res: BackendResponse<IO>) -> Self {
        Self::new(res)
    }
}

impl<IO> ProxyResponse<IO> {
    /// Create a [`ProxyResponse`] from the [`BackendResponse`]. This will
    /// *mutate* the frontend request by stripping out hop-by-hop headers, and
    /// inserting other headers required of proxies.
    pub fn new(mut res: BackendResponse<IO>) -> Self {
        let info = HopByHopInfo::new(res.res_mut().headers_mut());
        Self { res, info }
    }

    pub fn res(&self) -> &Response {
        self.res.res()
    }

    pub fn hop_by_hop_info(&self) -> &HopByHopInfo {
        &self.info
    }
}

#[cfg(test)]
mod test {
    use bytes::BytesMut;
    use jrpxy_backend::BackendReader;

    use crate::response::ProxyResponse;

    #[tokio::test]
    async fn proxy_response_strips_hop_by_hop() {
        let backend_response_buf = b"\
            HTTP/1.1 200 Ok\r\n\
            content-length: 10\r\n\
            connection: keep-alive, listed\r\n\
            keep-alive: param=5\r\n\
            not-hop-by-hop: 1\r\n\
            listed: 2\r\n\
            \r\n\
            0123456789\
            ";

        let backend_reader = BackendReader::new(&backend_response_buf[..], 8192);
        let backend_response = backend_reader
            .read(true)
            .await
            .expect("failed to read request head")
            .try_into_response()
            .expect("got informational");
        let proxy_response = ProxyResponse::new(backend_response);

        let mut proxy_request_headers = proxy_response
            .res()
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
