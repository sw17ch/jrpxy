//! Request-target classification and normalization per RFC 9112 section 3.2.
//!
//! [`normalize`] is the entry point used by [`crate::ProxyClient::start`]: it
//! inspects the request-target on the incoming [`Request`], decides which of
//! the four forms it is in, and either passes it through (origin-form,
//! asterisk-form) or rewrites it (absolute-form -> origin-form with Host
//! substitution per RFC 9112 section 3.2.2). Anything outside the four forms,
//! or in a form not allowed for the request's method, is rejected with a
//! [`ProxyFrontendError`] variant.

use bytes::Bytes;
use jrpxy_http_message::message::Request;

use crate::error::ProxyFrontendError;

/// The four request-target forms defined by RFC 9112 section 3.2, plus the
/// out-of-band cases the classifier needs to distinguish.
#[derive(Debug)]
pub(crate) enum RequestTargetForm {
    /// `/path?query` - the normal case.
    Origin,
    /// `*` - server-wide. Valid only for OPTIONS (RFC 9112 section 3.2.4).
    Asterisk,
    /// `scheme://authority/path?query` - clients send this to proxies (RFC
    /// 9112 section 3.2.2).
    Absolute {
        authority: Bytes,
        path_and_query: Bytes,
    },
    /// Alpha-led bytes with no `://` separator. This *may* be authority-form
    /// (`host:port`, valid only for CONNECT per RFC 9112 section 3.2.3), but
    /// the classifier only checks the absence of `://`, not that the bytes
    /// actually parse as a valid authority. Callers should treat this as a
    /// candidate, not a proof.
    MaybeAuthority,
    /// The request-target is zero bytes. Surfaced separately from
    /// `Malformed` so callers can distinguish "no target at all" from
    /// "syntactically present but not in any of the four forms".
    Empty,
    /// Anything else.
    Malformed,
}

/// Classify a request-target by inspecting its first byte and overall shape.
pub(crate) fn classify(target: &Bytes) -> RequestTargetForm {
    if target.as_ref() == b"*" {
        return RequestTargetForm::Asterisk;
    }
    let Some(&first) = target.first() else {
        return RequestTargetForm::Empty;
    };
    if first == b'/' {
        return RequestTargetForm::Origin;
    }
    if first.is_ascii_alphabetic() {
        return parse_absolute_form(target);
    }
    RequestTargetForm::Malformed
}

/// Parse `scheme "://" authority path-abempty [ "?" query ]` from `target`.
/// Returns `MaybeAuthority` if the bytes look like they could be authority-
/// form (no `://` separator after the alpha-led prefix) without actually
/// verifying the `host:port` shape, and `Malformed` if anything is
/// structurally wrong.
fn parse_absolute_form(target: &Bytes) -> RequestTargetForm {
    // scheme = ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ). The first byte
    // is checked explicitly because the rest of this function treats any
    // alpha-led prefix as a candidate scheme.
    let bytes = target.as_ref();
    match bytes.first() {
        None => return RequestTargetForm::Empty,
        Some(b) if !b.is_ascii_alphabetic() => return RequestTargetForm::Malformed,
        Some(_) => {}
    }
    let scheme_end = bytes
        .iter()
        .position(|&b| !(b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')))
        .unwrap_or(bytes.len());

    if bytes.len() < scheme_end + 3 || &bytes[scheme_end..scheme_end + 3] != b"://" {
        // No `://` separator after the alpha-led prefix: the bytes might be
        // `host:port` (authority-form). CONNECT is the only method for which
        // authority-form is valid, and the proxy rejects CONNECT upstream of
        // this, so this is always an error - but flag it specifically so the
        // caller can return a precise error. We deliberately don't validate
        // the host:port shape here; MaybeAuthority signals "candidate".
        return RequestTargetForm::MaybeAuthority;
    }

    let after_scheme = &bytes[scheme_end + 3..];
    let authority_end = after_scheme
        .iter()
        .position(|&b| matches!(b, b'/' | b'?' | b'#'))
        .unwrap_or(after_scheme.len());
    let authority_bytes = &after_scheme[..authority_end];

    if authority_bytes.is_empty() {
        return RequestTargetForm::Malformed;
    }
    // Reject deprecated `userinfo@host` (RFC 9110 section 4.2.4). Forwarding
    // credentials through to the origin without policy is a footgun.
    if authority_bytes.contains(&b'@') {
        return RequestTargetForm::Malformed;
    }

    // Strip any fragment. A fragment is a client-side reference and MUST NOT
    // be forwarded to the origin (RFC 9110 section 7.1).
    let path_query_bytes = &after_scheme[authority_end..];
    let fragment_start = path_query_bytes
        .iter()
        .position(|&b| b == b'#')
        .unwrap_or(path_query_bytes.len());
    let path_query_bytes = &path_query_bytes[..fragment_start];

    let path_and_query = if path_query_bytes.is_empty() {
        // RFC 9112 section 3.2.1 explicitly: "If the target URI's path
        // component is empty, the client MUST send '/' as the path within
        // the origin-form of request-target." Section 3.2.2 extends this
        // to proxies/gateways by requiring a "properly generated
        // origin-form" when forwarding absolute-form. The RFC 3986 grammar
        // reinforces it: absolute-path = 1*( "/" segment ), so an empty
        // path isn't lexically expressible in origin-form anyway.
        Bytes::from_static(b"/")
    } else if path_query_bytes[0] == b'?' {
        // Same rule: the target URI's path is empty, so the origin-form
        // path MUST be `/`. The query is a separate `[ "?" query ]`
        // production after the path and is preserved verbatim, yielding
        // `/` + query.
        let mut buf = Vec::with_capacity(1 + path_query_bytes.len());
        buf.extend_from_slice(b"/");
        buf.extend_from_slice(path_query_bytes);
        Bytes::from(buf)
    } else {
        target.slice_ref(path_query_bytes)
    };

    RequestTargetForm::Absolute {
        authority: target.slice_ref(authority_bytes),
        path_and_query,
    }
}

/// Validate and normalize the request-target on `req` per RFC 9112 section
/// 3.2. On success `req` is mutated into a forward-safe form: origin-form and
/// asterisk-form pass through unchanged; absolute-form is rewritten to
/// origin-form, with all existing `Host` headers replaced by a single one
/// derived from the authority (RFC 9112 section 3.2.2).
pub(crate) fn normalize(req: &mut Request) -> Result<(), ProxyFrontendError> {
    match classify(req.path()) {
        RequestTargetForm::Origin => Ok(()),
        RequestTargetForm::Asterisk => {
            if req.method().as_ref() == b"OPTIONS" {
                Ok(())
            } else {
                Err(ProxyFrontendError::AsteriskFormNotAllowed)
            }
        }
        RequestTargetForm::Absolute {
            authority,
            path_and_query,
        } => {
            // Set the path first so a malformed request-target is rejected
            // before any header mutation. set_origin_form_path validates
            // before assigning, so on failure the request is untouched -
            // keeping normalize atomic for any future retry or recovery
            // path.
            req.set_origin_form_path(path_and_query)
                .map_err(|_| ProxyFrontendError::MalformedRequestTarget)?;
            req.headers_mut()
                .remove(|(name, _)| name.eq_ignore_ascii_case(b"host"));
            req.headers_mut()
                .push(Bytes::from_static(b"host"), authority);
            Ok(())
        }
        RequestTargetForm::MaybeAuthority => Err(ProxyFrontendError::AuthorityFormNotAllowed),
        // An empty request-target is not one of the four RFC 9112 forms and
        // is reported as malformed.
        RequestTargetForm::Empty | RequestTargetForm::Malformed => {
            Err(ProxyFrontendError::MalformedRequestTarget)
        }
    }
}

#[cfg(test)]
mod test {
    use bytes::Bytes;

    use super::{RequestTargetForm, classify, parse_absolute_form};

    #[test]
    fn origin_form() {
        let r = classify(&Bytes::from_static(b"/foo"));
        assert!(matches!(r, RequestTargetForm::Origin));
    }

    #[test]
    fn origin_form_root() {
        let r = classify(&Bytes::from_static(b"/"));
        assert!(matches!(r, RequestTargetForm::Origin));
    }

    #[test]
    fn asterisk_form() {
        let r = classify(&Bytes::from_static(b"*"));
        assert!(matches!(r, RequestTargetForm::Asterisk));
    }

    #[test]
    fn empty_is_empty() {
        let r = classify(&Bytes::from_static(b""));
        assert!(matches!(r, RequestTargetForm::Empty));
    }

    #[test]
    fn absolute_form_on_empty_is_empty() {
        let r = parse_absolute_form(&Bytes::from_static(b""));
        assert!(matches!(r, RequestTargetForm::Empty));
    }

    #[test]
    fn maybe_authority_form() {
        // No `://` after the alpha prefix - could be authority-form, but the
        // classifier doesn't verify the host:port shape.
        let r = classify(&Bytes::from_static(b"example.com:8080"));
        assert!(matches!(r, RequestTargetForm::MaybeAuthority));
    }

    #[test]
    fn absolute_form_simple() {
        let r = classify(&Bytes::from_static(b"http://example.com/foo"));
        let RequestTargetForm::Absolute {
            authority,
            path_and_query,
        } = r
        else {
            panic!("expected Absolute, got {r:?}");
        };
        assert_eq!(authority.as_ref(), b"example.com");
        assert_eq!(path_and_query.as_ref(), b"/foo");
    }

    #[test]
    fn absolute_form_with_query() {
        let r = classify(&Bytes::from_static(b"https://example.com/foo?bar=baz"));
        let RequestTargetForm::Absolute {
            authority,
            path_and_query,
        } = r
        else {
            panic!("expected Absolute, got {r:?}");
        };
        assert_eq!(authority.as_ref(), b"example.com");
        assert_eq!(path_and_query.as_ref(), b"/foo?bar=baz");
    }

    #[test]
    fn absolute_form_with_port() {
        let r = classify(&Bytes::from_static(b"http://example.com:8080/foo"));
        let RequestTargetForm::Absolute {
            authority,
            path_and_query,
        } = r
        else {
            panic!("expected Absolute, got {r:?}");
        };
        assert_eq!(authority.as_ref(), b"example.com:8080");
        assert_eq!(path_and_query.as_ref(), b"/foo");
    }

    #[test]
    fn absolute_form_empty_path_becomes_slash() {
        let r = classify(&Bytes::from_static(b"http://example.com"));
        let RequestTargetForm::Absolute {
            authority,
            path_and_query,
        } = r
        else {
            panic!("expected Absolute, got {r:?}");
        };
        assert_eq!(authority.as_ref(), b"example.com");
        assert_eq!(path_and_query.as_ref(), b"/");
    }

    #[test]
    fn absolute_form_empty_path_with_query_becomes_slash_query() {
        let r = classify(&Bytes::from_static(b"http://example.com?q=1"));
        let RequestTargetForm::Absolute {
            authority,
            path_and_query,
        } = r
        else {
            panic!("expected Absolute, got {r:?}");
        };
        assert_eq!(authority.as_ref(), b"example.com");
        assert_eq!(path_and_query.as_ref(), b"/?q=1");
    }

    #[test]
    fn absolute_form_strips_fragment() {
        let r = classify(&Bytes::from_static(b"http://example.com/foo#frag"));
        let RequestTargetForm::Absolute {
            authority,
            path_and_query,
        } = r
        else {
            panic!("expected Absolute, got {r:?}");
        };
        assert_eq!(authority.as_ref(), b"example.com");
        assert_eq!(path_and_query.as_ref(), b"/foo");
    }

    #[test]
    fn absolute_form_rejects_userinfo() {
        let r = classify(&Bytes::from_static(b"http://user:pass@example.com/foo"));
        assert!(matches!(r, RequestTargetForm::Malformed));
    }

    #[test]
    fn absolute_form_rejects_scheme_starting_with_digit() {
        // RFC 3986: scheme = ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ).
        // A scheme MUST begin with ALPHA, so `1http` is not a valid scheme.
        // classify() filters this case out via its first-byte alpha check
        // before parse_absolute_form is reached, but parse_absolute_form
        // must also reject it on its own so the invariant is enforced in
        // one place and a future caller cannot accidentally accept it.
        let r = parse_absolute_form(&Bytes::from_static(b"1http://www.example.com"));
        assert!(matches!(r, RequestTargetForm::Malformed));
    }

    #[test]
    fn absolute_form_rejects_empty_authority() {
        let r = classify(&Bytes::from_static(b"http:///foo"));
        assert!(matches!(r, RequestTargetForm::Malformed));
    }

    #[test]
    fn leading_non_alpha_non_slash_is_malformed() {
        let r = classify(&Bytes::from_static(b"@foo"));
        assert!(matches!(r, RequestTargetForm::Malformed));
    }

    mod normalize {
        use jrpxy_http_message::{message::RequestBuilder, version::HttpVersion};

        use super::super::normalize;

        #[test]
        fn origin_form_passes_through_unchanged() {
            let mut req = RequestBuilder::new(0)
                .with_method("GET")
                .with_path("/foo")
                .with_version(HttpVersion::Http11)
                .build()
                .unwrap();

            normalize(&mut req).expect("origin-form must normalize");
            assert_eq!(req.path().as_ref(), b"/foo");
        }

        #[test]
        fn absolute_form_rewrites_to_origin_form_and_substitutes_host() {
            let mut req = RequestBuilder::new(4)
                .with_method("GET")
                .with_path("http://origin.example.com/x")
                .with_version(HttpVersion::Http11)
                .with_header("Host", b"spoofed.example.com")
                .build()
                .unwrap();

            normalize(&mut req).expect("absolute-form must normalize");

            // The request now bears the authority-derived Host and origin-form path.
            assert_eq!(req.path().as_ref(), b"/x");
            let hosts: Vec<_> = req.headers().get_header("host").collect();
            assert_eq!(hosts.len(), 1, "exactly one Host after rewrite");
            assert_eq!(hosts[0].1.as_ref(), b"origin.example.com");
        }

        #[test]
        fn options_asterisk_passes_through_unchanged() {
            let mut req = RequestBuilder::new(0)
                .with_method("OPTIONS")
                .with_path("*")
                .with_version(HttpVersion::Http11)
                .build()
                .unwrap();

            normalize(&mut req).expect("OPTIONS * must normalize");
            assert_eq!(req.path().as_ref(), b"*");
        }

        #[test]
        fn absolute_form_empty_path_with_query_normalizes_to_slash_query() {
            // RFC 9112 section 3.2.1: `http://example.com?q=1` has an empty
            // path; the origin-form path MUST be `/` with the query preserved,
            // giving `/?q=1`. The authority replaces any spoofed Host header.
            let mut req = RequestBuilder::new(4)
                .with_method("GET")
                .with_path("http://example.com?q=1")
                .with_version(HttpVersion::Http11)
                .with_header("Host", b"spoofed.example.com")
                .build()
                .unwrap();

            normalize(&mut req).expect("empty path + query must normalize");

            assert_eq!(req.path().as_ref(), b"/?q=1");
            let hosts: Vec<_> = req.headers().get_header("host").collect();
            assert_eq!(hosts.len(), 1);
            assert_eq!(hosts[0].1.as_ref(), b"example.com");
        }
    }
}
