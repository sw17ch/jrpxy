use jrpxy_body::writer::{
    bodyless::BodylessBodyWriter, chunked::IdleWriter, content_length::ContentLengthBodyWriter,
};
use jrpxy_http_message::{
    framing::{WriteFraming, is_framing_header},
    message::Request,
};
use tokio::io::{self, AsyncWriteExt};

use crate::error::{ProxyBackendWriterError, ProxyBackendWriterResult};

/// Proxy-owned backend writer. Mirrors [`jrpxy_backend::writer::BackendWriter`]
/// but produces a [`ProxyBackendWriter`] on completion rather than a
/// [`jrpxy_backend::writer::BackendWriter`], keeping the proxy's type system
/// sealed.
pub struct ProxyBackendWriter<I> {
    writer: I,
}

impl<I: AsyncWriteExt + Unpin> ProxyBackendWriter<I> {
    pub fn new(writer: I) -> Self {
        Self { writer }
    }

    pub async fn send_as_chunked(
        self,
        request: &Request,
    ) -> ProxyBackendWriterResult<ProxyBackendBodyWriter<I>> {
        let Self { mut writer } = self;
        write_request_to(request, WriteFraming::Chunked, &mut writer)
            .await
            .map_err(ProxyBackendWriterError::WriteError)?;
        Ok(ProxyBackendBodyWriter {
            kind: ProxyBackendBodyWriterKind::TE(IdleWriter::new(writer)),
        })
    }

    pub async fn send_as_content_length(
        self,
        request: &Request,
        body_len: u64,
    ) -> ProxyBackendWriterResult<ProxyBackendBodyWriter<I>> {
        let Self { mut writer } = self;
        write_request_to(request, WriteFraming::Length(body_len), &mut writer)
            .await
            .map_err(ProxyBackendWriterError::WriteError)?;
        Ok(ProxyBackendBodyWriter {
            kind: ProxyBackendBodyWriterKind::CL(ContentLengthBodyWriter::new(body_len, writer)),
        })
    }

    pub async fn send_as_bodyless(
        self,
        request: &Request,
    ) -> ProxyBackendWriterResult<ProxyBackendBodyWriter<I>> {
        let Self { mut writer } = self;
        write_request_to(request, WriteFraming::PreserveFraming, &mut writer)
            .await
            .map_err(ProxyBackendWriterError::WriteError)?;
        Ok(ProxyBackendBodyWriter {
            kind: ProxyBackendBodyWriterKind::Bodyless(BodylessBodyWriter::new(writer)),
        })
    }

    pub fn into_inner(self) -> I {
        self.writer
    }
}

async fn write_request_to<W: AsyncWriteExt + Unpin>(
    req: &Request,
    framing: WriteFraming,
    mut w: W,
) -> io::Result<()> {
    let method = req.method();
    let path = req.path();

    w.write_all(method).await?;
    w.write_all(b" ").await?;
    w.write_all(path).await?;
    w.write_all(b" ").await?;
    w.write_all(b"HTTP/1.1").await?;
    w.write_all(b"\r\n").await?;

    let headers = req.headers().iter().filter(|(n, _)| !is_framing_header(n));

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

/// The body writer for a proxy-owned backend connection. Finishing always
/// produces a [`ProxyBackendWriter`].
///
/// The inner writer kind is crate-private so that raw body-crate writers
/// cannot be extracted and reused on a frontend connection.
pub struct ProxyBackendBodyWriter<I> {
    pub(crate) kind: ProxyBackendBodyWriterKind<I>,
}

pub(crate) enum ProxyBackendBodyWriterKind<I> {
    Bodyless(BodylessBodyWriter<I>),
    CL(ContentLengthBodyWriter<I>),
    TE(IdleWriter<I>),
}
