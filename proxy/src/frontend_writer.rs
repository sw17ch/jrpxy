use jrpxy_body::{
    error::BodyError,
    writer::{
        bodyless::BodylessBodyWriter, chunked::IdleWriter, content_length::ContentLengthBodyWriter,
    },
};
use jrpxy_http_message::{
    framing::{WriteFraming, is_framing_header},
    message::Response,
};
use tokio::io::{self, AsyncWriteExt};

use crate::error::{ProxyFrontendWriterError, ProxyFrontendWriterResult};

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
    ) -> ProxyFrontendWriterResult<ProxyFrontendBodyWriter<I>> {
        let Self { mut writer } = self;
        write_response_to(response, WriteFraming::Chunked, &mut writer)
            .await
            .map_err(ProxyFrontendWriterError::WriteError)?;
        Ok(ProxyFrontendBodyWriter {
            kind: ProxyFrontendBodyWriterKind::TE(Some(IdleWriter::new(writer))),
        })
    }

    /// Send the response with a content-length-delimited body.
    pub async fn send_as_content_length(
        self,
        response: &Response,
        body_len: u64,
    ) -> ProxyFrontendWriterResult<ProxyFrontendBodyWriter<I>> {
        let Self { mut writer } = self;
        write_response_to(response, WriteFraming::Length(body_len), &mut writer)
            .await
            .map_err(ProxyFrontendWriterError::WriteError)?;
        Ok(ProxyFrontendBodyWriter {
            kind: ProxyFrontendBodyWriterKind::CL(ContentLengthBodyWriter::new(body_len, writer)),
        })
    }

    /// Send the response with no body, preserving any framing headers from the
    /// origin. Use this for HEAD responses and 304 Not Modified.
    pub async fn send_as_bodyless_keep_framing(
        self,
        response: &Response,
    ) -> ProxyFrontendWriterResult<ProxyFrontendBodyWriter<I>> {
        let Self { mut writer } = self;
        write_response_to(response, WriteFraming::PreserveFraming, &mut writer)
            .await
            .map_err(ProxyFrontendWriterError::WriteError)?;
        Ok(ProxyFrontendBodyWriter {
            kind: ProxyFrontendBodyWriterKind::Bodyless(ProxyFrontendBodylessBodyWriter {
                inner: BodylessBodyWriter::new(writer),
            }),
        })
    }

    /// Send the response with no body and no framing headers. Use this for 1xx
    /// informational responses and 204 No Content.
    ///
    /// Returns [`ProxyFrontendBodylessBodyWriter`] directly so the caller can
    /// finish back into a [`ProxyFrontendWriter`] without the enum dispatch
    /// required by [`ProxyFrontendBodyWriter`].
    ///
    /// # Panics (debug)
    /// Asserts in debug builds that the response carries no framing headers.
    pub async fn send_as_no_content(
        self,
        response: &Response,
    ) -> ProxyFrontendWriterResult<ProxyFrontendBodylessBodyWriter<I>> {
        debug_assert!(
            !response.headers().iter().any(|(n, _)| is_framing_header(n)),
            "send_as_no_content called on a response that contains framing headers \
             (content-length or transfer-encoding); the origin is misbehaving"
        );
        let Self { mut writer } = self;
        write_response_to(response, WriteFraming::StripFraming, &mut writer)
            .await
            .map_err(ProxyFrontendWriterError::WriteError)?;
        Ok(ProxyFrontendBodylessBodyWriter {
            inner: BodylessBodyWriter::new(writer),
        })
    }

    /// Send the response with a body terminated by connection close (RFC 9112
    /// section 6.3). Framing headers from the origin are stripped. Use this
    /// when the client cannot decode chunked transfer encoding (e.g. HTTP/1.0).
    pub async fn send_as_eof(
        self,
        response: &Response,
    ) -> ProxyFrontendWriterResult<ProxyFrontendBodyWriter<I>> {
        let Self { mut writer } = self;
        write_response_to(response, WriteFraming::StripFraming, &mut writer)
            .await
            .map_err(ProxyFrontendWriterError::WriteError)?;
        Ok(ProxyFrontendBodyWriter {
            kind: ProxyFrontendBodyWriterKind::Eof(writer),
        })
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

/// Proxy wrapper for [`BodylessBodyWriter`]. Produced directly by
/// [`ProxyFrontendWriter::send_as_no_content`] and carried inside the
/// `Bodyless` variant of [`ProxyFrontendBodyWriter`].
pub struct ProxyFrontendBodylessBodyWriter<I> {
    inner: BodylessBodyWriter<I>,
}

impl<I: AsyncWriteExt + Unpin> ProxyFrontendBodylessBodyWriter<I> {
    pub fn finish(self) -> ProxyFrontendWriterResult<ProxyFrontendWriter<I>> {
        let Self { inner } = self;
        Ok(ProxyFrontendWriter {
            writer: inner.finish(),
        })
    }
}

/// The body writer for a proxy-owned frontend connection. Finishing produces
/// a [`ProxyFrontendWriter`] when the framing allows the connection to be
/// reused; the `Eof` variant returns `None` since the connection must be
/// closed after the body.
///
/// The inner writer kind is crate-private so that raw body-crate writers
/// cannot be extracted and reused on a backend connection.
pub struct ProxyFrontendBodyWriter<I> {
    pub(crate) kind: ProxyFrontendBodyWriterKind<I>,
}

pub(crate) enum ProxyFrontendBodyWriterKind<I> {
    Bodyless(ProxyFrontendBodylessBodyWriter<I>),
    CL(ContentLengthBodyWriter<I>),
    /// `None` once a prior write failed mid-chunk; any further operation on
    /// this variant returns [`BodyError::WriteAfterError`].
    TE(Option<IdleWriter<I>>),
    /// Connection-close body: the raw writer is held directly; no framing
    /// is written. The connection must be closed after the body is sent.
    Eof(I),
}

impl<I: AsyncWriteExt + Unpin> ProxyFrontendBodyWriter<I> {
    pub async fn write(&mut self, buf: &[u8]) -> ProxyFrontendWriterResult<()> {
        match &mut self.kind {
            ProxyFrontendBodyWriterKind::Bodyless(_) => {
                Err(BodyError::BodyOverflow(buf.len() as u64).into())
            }
            ProxyFrontendBodyWriterKind::CL(w) => Ok(w.write(buf).await?),
            ProxyFrontendBodyWriterKind::TE(opt) => {
                let idle = opt.take().ok_or(BodyError::WriteAfterError)?;
                if buf.is_empty() {
                    *opt = Some(idle);
                    return Ok(());
                }
                let next = idle
                    .start(buf.len() as u64)
                    .write()
                    .await?
                    .write(buf)
                    .await?
                    .write()
                    .await?;
                *opt = Some(next);
                Ok(())
            }
            ProxyFrontendBodyWriterKind::Eof(w) => w
                .write_all(buf)
                .await
                .map_err(ProxyFrontendWriterError::WriteError),
        }
    }

    /// Finish the body. Returns `Some(ProxyFrontendWriter)` when the connection
    /// can be reused, and `None` for the `Eof` variant where the connection
    /// must be closed after the body is sent.
    pub async fn finish(self) -> ProxyFrontendWriterResult<Option<ProxyFrontendWriter<I>>> {
        let Self { kind } = self;
        match kind {
            ProxyFrontendBodyWriterKind::Bodyless(w) => w.finish().map(Some),
            ProxyFrontendBodyWriterKind::CL(mut inner) => {
                inner.flush().await?;
                let writer = inner.into_writer()?;
                Ok(Some(ProxyFrontendWriter { writer }))
            }
            ProxyFrontendBodyWriterKind::TE(opt) => {
                let idle = opt.ok_or(BodyError::WriteAfterError)?;
                let writer = idle.into_final(&Default::default()).write().await?;
                Ok(Some(ProxyFrontendWriter { writer }))
            }
            ProxyFrontendBodyWriterKind::Eof(mut writer) => {
                writer
                    .flush()
                    .await
                    .map_err(ProxyFrontendWriterError::WriteError)?;
                Ok(None)
            }
        }
    }
}
