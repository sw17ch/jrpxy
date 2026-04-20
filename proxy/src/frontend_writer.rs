use jrpxy_body::writer::{
    bodyless::BodylessBodyWriter, chunked::IdleWriter, content_length::ContentLengthBodyWriter,
};
use jrpxy_frontend::error::{FrontendError, FrontendResult};
use jrpxy_http_message::{
    framing::{WriteFraming, is_framing_header},
    message::Response,
};
use tokio::io::{self, AsyncWriteExt};

/// Proxy-owned frontend writer. Mirrors [`jrpxy_frontend::writer::FrontendWriter`]
/// but produces a [`ProxyFrontendWriter`] on completion rather than a
/// [`jrpxy_frontend::writer::FrontendWriter`], keeping the proxy's type system
/// sealed.
pub struct ProxyFrontendWriter<I> {
    writer: I,
}

impl<I: AsyncWriteExt + Unpin> ProxyFrontendWriter<I> {
    pub fn new(writer: I) -> Self {
        Self { writer }
    }

    /// Send the response with a chunked body.
    pub async fn send_as_chunked(
        self,
        response: &Response,
    ) -> FrontendResult<ProxyFrontendBodyWriter<I>> {
        let Self { mut writer } = self;
        write_response_to(response, WriteFraming::Chunked, &mut writer)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(ProxyFrontendBodyWriter::TE(IdleWriter::new(writer)))
    }

    /// Send the response with a content-length-delimited body.
    pub async fn send_as_content_length(
        self,
        response: &Response,
        body_len: u64,
    ) -> FrontendResult<ProxyFrontendBodyWriter<I>> {
        let Self { mut writer } = self;
        write_response_to(response, WriteFraming::Length(body_len), &mut writer)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(ProxyFrontendBodyWriter::CL(ContentLengthBodyWriter::new(
            body_len, writer,
        )))
    }

    /// Send the response with no body, preserving any framing headers from the
    /// origin. Use this for HEAD responses and 304 Not Modified.
    pub async fn send_as_bodyless(
        self,
        response: &Response,
    ) -> FrontendResult<ProxyFrontendBodyWriter<I>> {
        let Self { mut writer } = self;
        write_response_to(response, WriteFraming::PreserveFraming, &mut writer)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(ProxyFrontendBodyWriter::Bodyless(BodylessBodyWriter::new(
            writer,
        )))
    }

    /// Send the response with a body terminated by connection close (RFC 9112
    /// section 6.3). Framing headers from the origin are stripped. Use this
    /// when the client cannot decode chunked transfer encoding (e.g. HTTP/1.0).
    pub async fn send_as_eof(
        self,
        response: &Response,
    ) -> FrontendResult<ProxyFrontendBodyWriter<I>> {
        let Self { mut writer } = self;
        write_response_to(response, WriteFraming::StripFraming, &mut writer)
            .await
            .map_err(FrontendError::WriteError)?;
        Ok(ProxyFrontendBodyWriter::Eof(writer))
    }

    pub fn into_inner(self) -> I {
        self.writer
    }
}

async fn write_response_to<W: AsyncWriteExt + Unpin>(
    res: &Response,
    framing: WriteFraming,
    mut w: W,
) -> io::Result<()> {
    let code = res.code();
    let reason = res.reason();

    let code = format!("{code}");

    w.write_all(b"HTTP/1.1").await?;
    w.write_all(b" ").await?;
    w.write_all(code.as_bytes()).await?;
    w.write_all(b" ").await?;
    w.write_all(reason).await?;
    w.write_all(b"\r\n").await?;

    let headers = res
        .headers()
        .iter()
        .filter(|(n, _)| framing.preserves_framing() || !is_framing_header(n));

    for (n, v) in headers {
        w.write_all(n).await?;
        w.write_all(b": ").await?;
        w.write_all(v).await?;
        w.write_all(b"\r\n").await?;
    }

    match framing {
        WriteFraming::PreserveFraming | WriteFraming::StripFraming => {}
        WriteFraming::Length(l) => {
            let cl = format!("content-length: {l}\r\n");
            w.write_all(cl.as_bytes()).await?;
        }
        WriteFraming::Chunked => {
            w.write_all(b"transfer-encoding: chunked\r\n").await?;
        }
    }

    w.write_all(b"\r\n").await?;
    w.flush().await?;

    Ok(())
}

/// The body writer for a proxy-owned frontend connection, holding raw body-crate
/// writer types. Pass to [`crate::response_forwarder::ProxyResponseBodyForwarder`]
/// to forward the body. Finishing always produces a [`ProxyFrontendWriter`].
pub enum ProxyFrontendBodyWriter<I> {
    Bodyless(BodylessBodyWriter<I>),
    CL(ContentLengthBodyWriter<I>),
    TE(IdleWriter<I>),
    /// Connection-close body: the raw writer is held directly; no framing
    /// is written. The connection must be closed after the body is sent.
    Eof(I),
}
