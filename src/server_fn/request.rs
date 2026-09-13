//! The [`NtexRequest`] newtype and its `server_fn::request::Req` impl —
//! covering body collection, streaming, and the WebSocket upgrade bridge.

use bytes::Bytes as SfBytes;
use futures::{Sink, Stream};
use ntex::http::Payload;
use ntex::web::HttpRequest;
use send_wrapper::SendWrapper;
use server_fn::{
    error::{FromServerFnError, IntoAppError},
    request::Req,
};
use std::{borrow::Cow, future::Future};

use crate::config::{PayloadTooLarge, collect_payload, server_fn_config};
use crate::server_fn::response::NtexServerResponse;

/// Wraps an ntex request + payload pair for use as a server function input.
///
/// Implements [`server_fn::request::Req`] so the generic server-function
/// runtime can pull bytes, strings, streams, and websockets out of the
/// request. ntex's types are not [`Send`], so the pair is wrapped in
/// [`SendWrapper`] to satisfy the `Send` bound the generic runtime requires.
///
/// # Panics
///
/// [`SendWrapper`] panics if the wrapped value is accessed or dropped on a
/// thread other than the one that created it. ntex pins request handling to a
/// single worker thread, so in normal request handling this never fires; the
/// hazard only appears if the request is deliberately moved onto another
/// thread (e.g. `spawn_blocking`) and touched there — the same cross-thread
/// invariant documented on [`Request`](crate::request::Request).
///
/// Construct with [`NtexRequest::from`] and consume with [`NtexRequest::take`].
pub struct NtexRequest(pub SendWrapper<(HttpRequest, Payload)>);

impl NtexRequest {
    /// Consumes the wrapper and returns the original ntex request/payload.
    pub fn take(self) -> (HttpRequest, Payload) {
        self.0.take()
    }

    fn header(&self, name: &str) -> Option<Cow<'_, str>> {
        self.0
            .0
            .headers()
            .get(name)
            .map(|h| String::from_utf8_lossy(h.as_bytes()))
    }
}

impl From<(HttpRequest, Payload)> for NtexRequest {
    fn from(value: (HttpRequest, Payload)) -> Self {
        Self(SendWrapper::new(value))
    }
}

impl<Error, InputStreamError, OutputStreamError> Req<Error, InputStreamError, OutputStreamError>
    for NtexRequest
where
    Error: FromServerFnError + Send,
    InputStreamError: FromServerFnError + Send,
    OutputStreamError: FromServerFnError + Send,
{
    type WebsocketResponse = NtexServerResponse;

    fn as_query(&self) -> Option<&str> {
        self.0.0.uri().query()
    }

    fn to_content_type(&self) -> Option<Cow<'_, str>> {
        self.header("Content-Type")
    }

    fn accepts(&self) -> Option<Cow<'_, str>> {
        self.header("Accept")
    }

    fn referer(&self) -> Option<Cow<'_, str>> {
        self.header("Referer")
    }

    fn try_into_bytes(self) -> impl Future<Output = Result<SfBytes, Error>> + Send {
        SendWrapper::new(async move {
            let (req, payload) = self.0.take();
            let limit = server_fn_config(&req).payload_limit;
            collect_payload(&req, payload, limit).await.map_err(|e| {
                // `Args` maps semantically to "error reading arguments
                // from the request", closer to payload-overflow than
                // `Deserialization` (which is defined as a client-side
                // result-parsing error). The outer ntex handler
                // translates the extension marker into 413.
                server_fn::error::ServerFnErrorErr::Args(e.to_string()).into_app_error()
            })
        })
    }

    fn try_into_string(self) -> impl Future<Output = Result<String, Error>> + Send {
        SendWrapper::new(async move {
            let (req, payload) = self.0.take();
            let limit = server_fn_config(&req).payload_limit;
            let bytes = collect_payload(&req, payload, limit).await.map_err(|e| {
                Error::from_server_fn_error(server_fn::error::ServerFnErrorErr::Args(e.to_string()))
            })?;
            String::from_utf8(Vec::from(bytes)).map_err(|e| {
                Error::from_server_fn_error(server_fn::error::ServerFnErrorErr::Args(e.to_string()))
            })
        })
    }

    fn try_into_stream(self) -> Result<impl Stream<Item = Result<SfBytes, SfBytes>> + Send, Error> {
        let (req, payload) = self.0.take();
        let limit = server_fn_config(&req).payload_limit;
        // State is `Option<..>`: `None` terminates the stream on the next
        // poll, so a single error frame (limit exceeded or payload error)
        // is emitted and then the stream closes. On overflow we also
        // stash the `PayloadTooLarge` marker on `req.extensions_mut()`
        // so the outer ntex handler can return 413 if it observes the marker
        // before returning the response. If a lazy response body consumes the
        // input later, this remains a stream error; there is no second marker
        // check, even if the original response has not reached the wire yet.
        let stream =
            futures::stream::unfold(Some((req, payload, 0usize, limit)), |state| async move {
                let (req, mut payload, so_far, limit) = state?;
                let item = payload.recv().await?;
                match item {
                    Ok(b) => {
                        let next = so_far.saturating_add(b.len());
                        if next > limit {
                            req.extensions_mut().insert(PayloadTooLarge);
                            let err = Error::from_server_fn_error(
                                server_fn::error::ServerFnErrorErr::Args(format!(
                                    "payload exceeds limit of {limit} bytes"
                                )),
                            )
                            .ser();
                            Some((Err(err), None))
                        } else {
                            Some((
                                // Zero-copy hand-off of the ntex chunk: the
                                // ntex `Bytes` owner is moved into the
                                // `bytes::Bytes` shared box instead of being
                                // memcpy'd. The owner is exactly this chunk,
                                // so nothing extra is kept alive.
                                Ok(SfBytes::from_owner(b)),
                                Some((req, payload, next, limit)),
                            ))
                        }
                    }
                    Err(e) => {
                        let err = Error::from_server_fn_error(
                            server_fn::error::ServerFnErrorErr::Args(e.to_string()),
                        )
                        .ser();
                        Some((Err(err), None))
                    }
                }
            });
        Ok(SendWrapper::new(stream))
    }

    /// Upgrades the request and returns the incoming stream, outgoing sink and response.
    ///
    /// One worker-local task owns the connection. Its single inbound sender and
    /// outbound receiver apply backpressure before reading another message or
    /// encoding more output. Each futures mpsc channel has the configured buffer
    /// plus one sender reservation. ntex IO adds its configured watermarks and at
    /// most one encoded outgoing message; applications control outgoing sizes.
    ///
    /// Text and fragmented messages are validated and bounded by
    /// [`LeptosServerFnConfig::payload_limit`](crate::LeptosServerFnConfig::payload_limit).
    /// Invalid protocol, UTF-8 and size failures close with 1002, 1007 and 1009.
    /// Delivery of the terminal input error is best effort if the receiver is full.
    ///
    /// Authentication and Origin checks must run in middleware before this call:
    /// successful upgrade commits 101 before the server-function body executes.
    fn try_into_websocket(
        self,
    ) -> impl Future<
        Output = Result<
            (
                impl Stream<Item = Result<SfBytes, SfBytes>> + Send + 'static,
                impl Sink<SfBytes> + Send + 'static,
                Self::WebsocketResponse,
            ),
            Error,
        >,
    > + Send {
        SendWrapper::new(async move {
            let (request, _payload) = self.0.take();
            super::websocket::upgrade::<Error, InputStreamError>(request).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lets_expect::lets_expect;
    use ntex::util::Bytes as NBytes;
    use ntex::web::test;
    use server_fn::error::ServerFnError;

    // ----- NtexRequest accessors: header / query / content-type / accept -
    // The request-side getters the server-fn runtime reads to pick a codec
    // and decode the body. Each must report the EXACT value present on the
    // wire and `None` when the field is absent — a collapse to a constant
    // (`""`, `"xyzzy"`, `None`, `Some`) would feed the runtime a wrong or
    // missing value. A concrete error type pins the generic `Req` impl.
    type E = ServerFnError;

    fn request_with(uri: &str, headers: &[(&str, &str)]) -> NtexRequest {
        let mut builder = test::TestRequest::with_uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        NtexRequest::from(builder.to_http_parts())
    }

    fn header_value(headers: &[(&str, &str)]) -> Option<String> {
        request_with("/", headers)
            .header("x-probe")
            .map(|value| value.into_owned())
    }

    fn query(uri: &str) -> Option<String> {
        let req = request_with(uri, &[]);
        <NtexRequest as Req<E, E, E>>::as_query(&req).map(str::to_owned)
    }

    fn content_type(headers: &[(&str, &str)]) -> Option<String> {
        let req = request_with("/", headers);
        <NtexRequest as Req<E, E, E>>::to_content_type(&req).map(|value| value.into_owned())
    }

    fn accept(headers: &[(&str, &str)]) -> Option<String> {
        let req = request_with("/", headers);
        <NtexRequest as Req<E, E, E>>::accepts(&req).map(|value| value.into_owned())
    }

    fn referer(headers: &[(&str, &str)]) -> Option<String> {
        let req = request_with("/", headers);
        <NtexRequest as Req<E, E, E>>::referer(&req).map(|value| value.into_owned())
    }

    lets_expect! {
        expect(header_value(headers)) as the_request_header {
            let headers: &[(&str, &str)] = &[("x-probe", "probe-value")];

            to returns_the_exact_header_value { equal(Some("probe-value".to_string())) }

            // A SECOND present value with different content: pins that the
            // shared `header()` primitive returns the ACTUAL value, not a
            // fixture constant (a `header()` hardcoded to return "probe-value"
            // for any present key would still pass the first leaf alone).
            when a_different_value_is_present {
                let headers: &[(&str, &str)] = &[("x-probe", "other-value")];
                to returns_that_value { equal(Some("other-value".to_string())) }
            }

            when the_header_is_present_but_empty {
                // An explicitly-present, empty-string header value is
                // distinct from an ABSENT header — `Some("")`, not `None`.
                // A regression collapsing an empty value to "absent" would
                // slip through if this boundary were never exercised.
                let headers: &[(&str, &str)] = &[("x-probe", "")];
                to returns_an_empty_string { equal(Some("".to_string())) }
            }

            when the_header_is_absent {
                let headers: &[(&str, &str)] = &[];
                to returns_none { be_none }
            }
        }
    }

    // `header()`'s shared primitive uses `String::from_utf8_lossy`, silently
    // replacing malformed bytes with U+FFFD rather than failing. No existing
    // leaf supplies a non-UTF-8 header value, so this axis was unpinned.
    fn header_value_from_bytes(value: &'static [u8]) -> Option<String> {
        let req = test::TestRequest::with_uri("/")
            .header("x-probe", value)
            .to_http_parts();
        NtexRequest::from(req)
            .header("x-probe")
            .map(|value| value.into_owned())
    }

    lets_expect! {
        expect(header_value_from_bytes(value)) as the_request_header_lossy_decode {
            // `0xFF` is never a valid UTF-8 byte anywhere in a sequence, so
            // `from_utf8_lossy` replaces it with U+FFFD rather than erroring.
            let value: &'static [u8] = b"hello\xffworld";

            to replaces_the_invalid_byte_with_u_fffd {
                equal(Some("hello\u{fffd}world".to_string()))
            }
        }
    }

    lets_expect! {
        expect(query(uri)) as the_request_query {
            let uri = "/path?foo=bar&baz=1";

            to returns_the_raw_query_string { equal(Some("foo=bar&baz=1".to_string())) }

            when a_different_query_is_present {
                let uri = "/path?x=9";
                to returns_that_query { equal(Some("x=9".to_string())) }
            }

            when the_uri_ends_with_a_bare_question_mark {
                // A trailing `?` is an EMPTY query — `Some("")`, distinct from
                // no `?` at all (`None`). This boundary the `&str` type hides.
                let uri = "/path?";
                to returns_an_empty_query { equal(Some("".to_string())) }
            }

            when there_is_no_query {
                let uri = "/path";
                to returns_none { be_none }
            }
        }
    }

    lets_expect! {
        expect(content_type(headers)) as the_request_content_type {
            let headers: &[(&str, &str)] = &[("Content-Type", "application/json")];

            to reads_the_content_type_header { equal(Some("application/json".to_string())) }

            when a_different_content_type_is_present {
                let headers: &[(&str, &str)] = &[("Content-Type", "application/cbor")];
                to returns_that_content_type { equal(Some("application/cbor".to_string())) }
            }

            when the_content_type_is_absent {
                let headers: &[(&str, &str)] = &[];
                to returns_none { be_none }
            }
        }
    }

    lets_expect! {
        expect(accept(headers)) as the_request_accept {
            let headers: &[(&str, &str)] = &[("Accept", "text/html")];

            to reads_the_accept_header { equal(Some("text/html".to_string())) }

            when a_different_accept_is_present {
                let headers: &[(&str, &str)] = &[("Accept", "application/json")];
                to returns_that_accept { equal(Some("application/json".to_string())) }
            }

            when the_accept_header_is_absent {
                let headers: &[(&str, &str)] = &[];
                to returns_none { be_none }
            }
        }
    }

    lets_expect! {
        expect(referer(headers)) as the_request_referer {
            let headers: &[(&str, &str)] = &[("Referer", "http://example.test/form")];

            to reads_the_referer_header {
                equal(Some("http://example.test/form".to_string()))
            }

            // A SECOND present value with different content, mirroring the
            // sibling pattern used by `the_request_header`/`_content_type`/
            // `_accept`: pins that `referer()` returns the ACTUAL header
            // value, not a fixture constant the first leaf alone couldn't
            // distinguish from a hardcoded return.
            when a_different_value_is_present {
                let headers: &[(&str, &str)] = &[("Referer", "http://example.test/other")];
                to returns_that_value {
                    equal(Some("http://example.test/other".to_string()))
                }
            }

            when the_referer_is_absent {
                let headers: &[(&str, &str)] = &[];
                to returns_none { be_none }
            }
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum BodyError {
        Args(String),
        Other(String),
    }

    fn body_error(error: E) -> BodyError {
        match error {
            ServerFnError::Args(message) => BodyError::Args(message),
            other => BodyError::Other(format!("{other:?}")),
        }
    }

    fn string_body(bytes: &'static [u8]) -> Result<String, BodyError> {
        crate::tests::run_ntex(async move {
            let http_req = test::TestRequest::default().to_http_request();
            let chunks = futures::stream::iter([Ok::<_, ntex::http::error::PayloadError>(
                NBytes::from_static(bytes),
            )]);
            let req = NtexRequest::from((http_req, Payload::from_stream(chunks)));
            let result = <NtexRequest as Req<E, E, E>>::try_into_string(req).await;
            result.map_err(body_error)
        })
    }

    #[derive(Debug)]
    struct StreamObservation {
        items: Vec<Result<Vec<u8>, BodyError>>,
        overflow_marked: bool,
    }

    // A real trailing chunk distinguishes a terminal error from a stream that
    // merely happens to end immediately after its error-producing input.
    fn stream_body(second: Result<NBytes, ntex::http::error::PayloadError>) -> StreamObservation {
        use futures::StreamExt;
        crate::tests::run_ntex(async move {
            let http_req = test::TestRequest::default()
                .state(crate::LeptosServerFnConfig::new().with_payload_limit(16))
                .to_http_request();
            let chunks = futures::stream::iter([
                Ok(NBytes::from_static(b"AAAAAA")),
                second,
                Ok(NBytes::from_static(b"CCC")),
            ]);
            let req = NtexRequest::from((http_req.clone(), Payload::from_stream(chunks)));
            let stream = <NtexRequest as Req<E, E, E>>::try_into_stream(req)
                .expect("request stream construction must succeed");
            let items: Vec<Result<Vec<u8>, BodyError>> = stream
                .map(|item| {
                    item.map(|bytes| bytes.to_vec())
                        .map_err(|bytes| body_error(E::de(bytes)))
                })
                .collect()
                .await;
            let overflow_marked = http_req.extensions().get::<PayloadTooLarge>().is_some();
            StreamObservation {
                items,
                overflow_marked,
            }
        })
    }

    lets_expect! {
        expect(string_body(bytes)) as request_body_as_string {
            let bytes: &'static [u8] = "Привет".as_bytes();
            to preserves_the_utf8_text { equal(Ok("Привет".to_string())) }
            when the_bytes_are_invalid_utf8 {
                let bytes: &'static [u8] = b"ok\xff";
                to reports_the_structured_utf8_error {
                    equal(Err(BodyError::Args("invalid utf-8 sequence of 1 bytes from index 2".to_string())))
                }
            }
        }
    }

    lets_expect! {
        expect(stream_body(Ok(NBytes::from_static(second)))) as cumulative_request_payload {
            let second: &'static [u8] = b"BBBBBB";
            to forwards_every_chunk_without_marking_overflow {
                have(items) equal(vec![Ok(b"AAAAAA".to_vec()), Ok(b"BBBBBB".to_vec()), Ok(b"CCC".to_vec())]),
                have(overflow_marked) equal(false),
            }
            when the_cumulative_size_equals_the_limit {
                let second: &'static [u8] = b"BBBBBBB";
                to forwards_every_chunk_at_the_limit {
                    have(items) equal(vec![Ok(b"AAAAAA".to_vec()), Ok(b"BBBBBBB".to_vec()), Ok(b"CCC".to_vec())]),
                    have(overflow_marked) equal(false),
                }
            }
            when the_second_chunk_crosses_the_limit {
                let second: &'static [u8] = b"BBBBBBBBBBB";
                to reports_overflow_and_stops_before_the_trailing_chunk {
                    have(items) equal(vec![Ok(b"AAAAAA".to_vec()), Err(BodyError::Args("payload exceeds limit of 16 bytes".to_string()))]),
                    have(overflow_marked) equal(true),
                }
            }
        }
    }

    lets_expect! {
        expect(stream_body(Err(ntex::http::error::PayloadError::Incomplete(None)))) as payload_transport_failure {
            to preserves_the_cause_and_stops_before_the_trailing_chunk {
                have(items) equal(vec![Ok(b"AAAAAA".to_vec()), Err(BodyError::Args("A payload reached EOF, but is not complete. With error: None".to_string()))]),
                have(overflow_marked) equal(false),
            }
        }
    }
}
