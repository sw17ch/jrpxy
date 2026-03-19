//! Body forwarders for proxied requests and responses.
//!
//! [`RequestBodyForwarder`] pumps the frontend request body into the
//! backend; [`ResponseBodyForwarder`] pumps the backend response body
//! into the frontend. Each is built around a cancel-safe writer state
//! machine held in `&mut self` so a cancellation between phases leaves
//! the state machine and the wire consistent.

pub mod request;
pub mod response;

pub use request::RequestBodyForwarder;
pub use response::ResponseBodyForwarder;

/// Outcome of one step on a writer state machine.
///
/// Shared by [`request::RequestBodyForwarder`] and
/// [`response::ResponseBodyForwarder`]; their step functions report
/// progress through this enum.
enum StepOutcome {
    /// The writer is at `Ready` with no pending bytes; the caller must
    /// either refill the pending buffer or transition out of `Ready`
    /// (e.g. by extracting the writer and calling `finish` on it).
    NeedInput,
    /// Another call to `step_once` would make further progress.
    Working,
}
