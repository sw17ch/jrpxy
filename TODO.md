There are some things that need doing. Tracking them here for now.

# TODO List

- add proxy types
- vectored writes where appropriate
  - end-to-end proxy benchmarks first
- add abort that explicitly tries to break framing for chunk-encoding
  - something like putting `X` in the chunk length -- an invalid chunk length character
- validate method, path, header, reason for requests and responses
- coalesce chunks and body reads as much as possible so that we don't
  necessarily match writes chunk-for-chunk
- benchmarks for `parse_trailers` to decide between lookup table and the more direct lookup mechanisms
- add ability for the chunk-body reader to expose chunk extensions and trailers
- reject unsupported methods until support added
  - CONNECT
  - OPTIONS
  - TRACE
- probably need types that allow accepting HTTP/1.0 from clients, HTTP/1.0 from
  servers, and being able to translate between HTTP/1.1 to either.
  - frontend HTTP/1.0, backend HTTP/1.1. we could pool backend connection and reuse.
  - frontend HTTP/1.1, backend HTTP/1.0. we could pool the frontend, and reuse,
    but cannot pass `100 Continue` or other informational responses from the
    frontend to the backend.
  - this implies a mechanism for translating requests and responses, but also
    forwarders for going from one to the other.
  - this also implies a need to translate all requests and responses to an
    internal value that expresses HTTP semantics (RFC 9110).
- do not forward 1xx-informational responses to HTTP/1.0 frontends