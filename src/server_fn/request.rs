//! The [`NtexRequest`] newtype and its `server_fn::request::Req` impl —
//! covering body collection, streaming, and the WebSocket upgrade bridge.

use bytes::{Bytes as SfBytes, BytesMut as SfBytesMut};
use futures::{FutureExt, Sink, SinkExt, Stream, StreamExt, channel::mpsc};
use ntex::http::Payload;
use ntex::util::Bytes as NBytes;
use ntex::web::{self, HttpRequest};
use or_poisoned::OrPoisoned;
use send_wrapper::SendWrapper;
use server_fn::{
    error::{FromServerFnError, IntoAppError},
    request::Req,
};
use std::{
    borrow::Cow,
    future::Future,
    sync::{Arc, Mutex},
};

use crate::config::{PayloadTooLarge, collect_payload, server_fn_config};
use crate::server_fn::response::NtexServerResponse;

/// Wraps an ntex request + payload pair for use as a server function input.
///
/// Implements [`server_fn::request::Req`] so the generic server-function
/// runtime can pull bytes, strings, streams, and websockets out of the
/// request. ntex's types are not [`Send`], so the pair is wrapped in
/// [`SendWrapper`].
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
        // so the outer ntex handler can translate the stream's error
        // frame into a 413 response body.
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

    /// Upgrades the request to a WebSocket connection and returns
    /// `(incoming_stream, outgoing_sink, response)` for the server-fn
    /// runtime.
    ///
    /// The incoming and outgoing mpsc channel capacities default to
    /// [`DEFAULT_WS_CHANNEL_BUFFER`](crate::DEFAULT_WS_CHANNEL_BUFFER)
    /// (2048 messages). Override per-app by registering a
    /// [`LeptosServerFnConfig`](crate::LeptosServerFnConfig) with
    /// [`App::state`](ntex::web::App::state).
    ///
    /// ## Backpressure
    ///
    /// Both channels are bounded (`futures::channel::mpsc`). Producers
    /// (this bridge writing incoming WS frames into the server-fn
    /// receiver; the server-fn writing outgoing frames into the ntex
    /// sink) call `Sink::send().await`, which suspends the task until
    /// the channel has capacity again. A slow consumer therefore stalls
    /// the frame-reader task, which is exactly the backpressure behavior
    /// expected. Smaller buffers apply backpressure sooner; larger
    /// buffers absorb bursts at the cost of memory
    /// (`O(N_connections * buffer * msg_size)` worst case).
    ///
    /// ## Fragmented messages (RFC 6455 §5.4)
    ///
    /// Fragmented WebSocket messages — delivered by ntex as
    /// `Frame::Continuation(Item::{FirstText, FirstBinary, Continue,
    /// Last})` — are reassembled per-connection into a single payload
    /// before being handed to the server-fn. Browser ws-clients and
    /// many library clients fragment large messages automatically, so
    /// dropping continuation frames (as an earlier revision did) would
    /// silently lose data.
    ///
    /// ## Policy-violation close
    ///
    /// When a reassembled fragmented message exceeds
    /// `LeptosServerFnConfig::payload_limit`, this bridge closes the
    /// connection with [`CloseCode::Size`](ntex::ws::CloseCode::Size)
    /// (1009, "Message Too Big" per RFC 6455 §7.4.1) and delivers an
    /// `InputStreamError` to the server-fn receiver. The client gets
    /// a structured reason rather than an abrupt disconnect.
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
        use ntex::ws::{CloseCode, CloseReason};
        use std::{cell::RefCell, rc::Rc};

        #[derive(Copy, Clone)]
        enum FragmentKind {
            Text,
            Binary,
        }

        SendWrapper::new(async move {
            let (request, _payload) = self.0.take();

            let config = server_fn_config(&request);
            let payload_limit = config.payload_limit;
            let ws_subprotocol = config.ws_subprotocol.filter(|protocol| {
                web::ws::subprotocols(&request).any(|offered| offered == *protocol)
            });
            let (response_stream_tx, response_stream_rx) =
                mpsc::channel::<Result<SfBytes, SfBytes>>(config.ws_channel_buffer);
            let (response_sink_tx, response_sink_rx) =
                mpsc::channel::<SfBytes>(config.ws_channel_buffer);
            let response_sink_rx = Arc::new(Mutex::new(Some(response_sink_rx)));

            let response = web::ws::start::<_, _, &str, web::Error>(
                request,
                ws_subprotocol,
                ntex::service::fn_factory_with_config(move |sink: web::ws::WsSink| {
                    let response_stream_tx = response_stream_tx.clone();
                    let response_sink_rx = response_sink_rx.clone();

                    async move {
                        let mut response_sink_rx = response_sink_rx
                            .lock()
                            .or_poisoned()
                            .take()
                            .expect("websocket response sink should only be initialized once");

                        let outbound_sink = sink.clone();
                        let mut outbound_errors = response_stream_tx.clone();
                        ntex::rt::spawn(async move {
                            // The bridge parks on the OUTPUT receiver while
                            // holding a clone of the INPUT sender, so it must
                            // also watch the connection itself: when the peer
                            // disconnects, ntex drops only the frame-service's
                            // sender clone, and without this signal the bridge
                            // and the server-fn forwarder would keep waiting
                            // on each other forever — leaking both channels,
                            // the task, and the WsSink of every closed
                            // connection.
                            let mut disconnect = outbound_sink.on_disconnect().fuse();
                            loop {
                                let incoming = futures::select! {
                                    item = response_sink_rx.next() => match item {
                                        Some(incoming) => incoming,
                                        // Server fn finished its output:
                                        // close the websocket politely.
                                        None => break,
                                    },
                                    // Peer gone: release the INPUT sender so
                                    // the server fn sees EOF and unwinds.
                                    _ = disconnect => return,
                                };
                                if let Err(err) = outbound_sink
                                    .send(web::ws::Message::Binary(NBytes::copy_from_slice(&incoming)))
                                    .await
                                {
                                    // Best-effort notify the server-fn
                                    // receiver, then tear down. NOT an
                                    // awaiting `send`: this is the teardown
                                    // path, and backpressure here can
                                    // deadlock — if the inbound channel is
                                    // full because the server fn never drains
                                    // its input, `send().await` would block
                                    // forever and the bridge would never drop
                                    // its sender clone (the very leak the
                                    // disconnect watch above prevents).
                                    let _ = outbound_errors.try_send(Err(
                                        InputStreamError::from_server_fn_error(
                                            server_fn::error::ServerFnErrorErr::Request(
                                                err.to_string(),
                                            ),
                                        )
                                        .ser(),
                                    ));
                                    let _ = outbound_sink
                                        .send(close_send_failure(&err.to_string()))
                                        .await;
                                    return;
                                }
                            }
                            let _ = outbound_sink.send(web::ws::Message::Close(None)).await;
                        });

                        // Per-connection reassembly buffer for
                        // `Frame::Continuation`. `Rc<RefCell<_>>`
                        // because `fn_factory_with_config` returns a
                        // `!Send` future — one per connection.
                        let fragment: Rc<RefCell<Option<(FragmentKind, SfBytesMut)>>> =
                            Rc::new(RefCell::new(None));

                        Ok::<_, web::Error>(ntex::service::fn_service({
                            let response_stream_tx = response_stream_tx.clone();
                            let fragment = fragment.clone();
                            move |frame: web::ws::Frame| {
                                let mut tx = response_stream_tx.clone();
                                let fragment = fragment.clone();
                                async move {
                                    use web::ws::{Frame, Message};
                                    use ntex::ws::Item;
                                    match frame {
                                        Frame::Ping(bytes) => {
                                            Ok::<Option<Message>, web::Error>(Some(
                                                Message::Pong(bytes),
                                            ))
                                        }
                                        Frame::Pong(_) => Ok(None),
                                        Frame::Close(reason) => {
                                            fragment.borrow_mut().take();
                                            Ok(Some(Message::Close(reason)))
                                        }
                                        Frame::Binary(bytes) => {
                                            // Unfragmented binary; bypass
                                            // the reassembly buffer and
                                            // enforce the limit directly.
                                            if bytes.len() > payload_limit {
                                                let _ = tx
                                                    .send(Err(InputStreamError::from_server_fn_error(
                                                        server_fn::error::ServerFnErrorErr::Args(format!(
                                                            "websocket payload exceeded limit of {payload_limit} bytes (observed {})",
                                                            bytes.len()
                                                        )),
                                                    )
                                                    .ser()))
                                                    .await;
                                                return Ok(Some(close_too_big(payload_limit)));
                                            }
                                            // `send().await` applies
                                            // backpressure when the
                                            // consumer is slow. If the
                                            // receiver is gone, the WS
                                            // is about to close anyway.
                                            let _ = tx
                                                .send(Ok(SfBytes::from_owner(bytes)))
                                                .await;
                                            Ok(None)
                                        }
                                        Frame::Text(text) => {
                                            if text.len() > payload_limit {
                                                let _ = tx
                                                    .send(Err(InputStreamError::from_server_fn_error(
                                                        server_fn::error::ServerFnErrorErr::Args(format!(
                                                            "websocket payload exceeded limit of {payload_limit} bytes (observed {})",
                                                            text.len()
                                                        )),
                                                    )
                                                    .ser()))
                                                    .await;
                                                return Ok(Some(close_too_big(payload_limit)));
                                            }
                                            // RFC 6455 §8.1: a text message MUST
                                            // be valid UTF-8. ntex's `Frame::Text`
                                            // hands over raw bytes without
                                            // checking, so validate here and fail
                                            // the connection (1007) rather than
                                            // forward invalid bytes to the
                                            // server fn.
                                            if std::str::from_utf8(&text).is_err() {
                                                let _ = tx
                                                    .send(Err(InputStreamError::from_server_fn_error(
                                                        server_fn::error::ServerFnErrorErr::Args(
                                                            "websocket text frame is not valid UTF-8 (RFC 6455 §8.1)".to_string(),
                                                        ),
                                                    )
                                                    .ser()))
                                                    .await;
                                                return Ok(Some(close_invalid_utf8()));
                                            }
                                            let _ = tx
                                                .send(Ok(SfBytes::from_owner(text)))
                                                .await;
                                            Ok(None)
                                        }
                                        Frame::Continuation(item) => {
                                            // What to do with a First*
                                            // fragment after state + size
                                            // checks. Computed inside one
                                            // borrow window so no `RefMut`
                                            // is held across `.await`.
                                            enum FirstAction {
                                                Installed,
                                                ProtocolViolation,
                                                Overflow(usize),
                                            }
                                            // For Continue: whether to
                                            // extend the buffer, reject as
                                            // protocol error, or reject
                                            // on overflow.
                                            enum ContinueAction {
                                                Extended,
                                                NoOpener,
                                                Overflow(usize),
                                            }
                                            // For Last: take the buffer
                                            // (if any) along with its message
                                            // kind (so a Text message can be
                                            // UTF-8 validated), or reject.
                                            enum LastAction {
                                                Complete(FragmentKind, SfBytesMut),
                                                NoOpener,
                                                Overflow(usize),
                                            }
                                            // Inline overflow-emit block:
                                            // builds the `InputStreamError`
                                            // (generic captured by the
                                            // enclosing impl), sends it
                                            // through the receiver with
                                            // backpressure, then returns
                                            // the policy-close message.
                                            macro_rules! overflow_close {
                                                ($total:expr) => {{
                                                    let total = $total;
                                                    let _ = tx
                                                        .send(Err(InputStreamError::from_server_fn_error(
                                                            server_fn::error::ServerFnErrorErr::Args(format!(
                                                                "websocket payload exceeded limit of {payload_limit} bytes (observed {total})"
                                                            )),
                                                        )
                                                        .ser()))
                                                        .await;
                                                    Ok(Some(close_too_big(payload_limit)))
                                                }};
                                            }
                                            let protocol_close = |msg: &'static str| {
                                                Ok::<Option<Message>, web::Error>(Some(
                                                    Message::Close(Some(CloseReason {
                                                        code: CloseCode::Protocol,
                                                        description: Some(msg.into()),
                                                    })),
                                                ))
                                            };

                                            match item {
                                                Item::FirstText(b) => {
                                                    let action = {
                                                        let mut guard = fragment.borrow_mut();
                                                        if guard.is_some() {
                                                            *guard = None;
                                                            FirstAction::ProtocolViolation
                                                        } else if b.len() > payload_limit {
                                                            FirstAction::Overflow(b.len())
                                                        } else {
                                                            let mut buf = SfBytesMut::new();
                                                            buf.extend_from_slice(&b);
                                                            *guard = Some((FragmentKind::Text, buf));
                                                            FirstAction::Installed
                                                        }
                                                    };
                                                    match action {
                                                        FirstAction::Installed => Ok(None),
                                                        FirstAction::ProtocolViolation => protocol_close(
                                                            "new fragmented message started before previous one terminated",
                                                        ),
                                                        FirstAction::Overflow(t) => overflow_close!(t),
                                                    }
                                                }
                                                Item::FirstBinary(b) => {
                                                    let action = {
                                                        let mut guard = fragment.borrow_mut();
                                                        if guard.is_some() {
                                                            *guard = None;
                                                            FirstAction::ProtocolViolation
                                                        } else if b.len() > payload_limit {
                                                            FirstAction::Overflow(b.len())
                                                        } else {
                                                            let mut buf = SfBytesMut::new();
                                                            buf.extend_from_slice(&b);
                                                            *guard = Some((FragmentKind::Binary, buf));
                                                            FirstAction::Installed
                                                        }
                                                    };
                                                    match action {
                                                        FirstAction::Installed => Ok(None),
                                                        FirstAction::ProtocolViolation => protocol_close(
                                                            "new fragmented message started before previous one terminated",
                                                        ),
                                                        FirstAction::Overflow(t) => overflow_close!(t),
                                                    }
                                                }
                                                Item::Continue(b) => {
                                                    let action = {
                                                        let mut guard = fragment.borrow_mut();
                                                        match guard.as_mut() {
                                                            None => ContinueAction::NoOpener,
                                                            Some((_, buf)) => {
                                                                if buf
                                                                    .len()
                                                                    .saturating_add(b.len())
                                                                    > payload_limit
                                                                {
                                                                    let total =
                                                                        buf.len() + b.len();
                                                                    *guard = None;
                                                                    ContinueAction::Overflow(total)
                                                                } else {
                                                                    buf.extend_from_slice(&b);
                                                                    ContinueAction::Extended
                                                                }
                                                            }
                                                        }
                                                    };
                                                    match action {
                                                        ContinueAction::Extended => Ok(None),
                                                        ContinueAction::NoOpener => protocol_close(
                                                            "unexpected continuation frame",
                                                        ),
                                                        ContinueAction::Overflow(t) => overflow_close!(t),
                                                    }
                                                }
                                                Item::Last(b) => {
                                                    let action = {
                                                        let taken = fragment.borrow_mut().take();
                                                        match taken {
                                                            None => LastAction::NoOpener,
                                                            Some((kind, mut buf)) => {
                                                                if buf
                                                                    .len()
                                                                    .saturating_add(b.len())
                                                                    > payload_limit
                                                                {
                                                                    LastAction::Overflow(
                                                                        buf.len() + b.len(),
                                                                    )
                                                                } else {
                                                                    buf.extend_from_slice(&b);
                                                                    LastAction::Complete(kind, buf)
                                                                }
                                                            }
                                                        }
                                                    };
                                                    match action {
                                                        LastAction::Complete(kind, buf) => {
                                                            // RFC 6455 §8.1: a
                                                            // reassembled TEXT
                                                            // message must be
                                                            // valid UTF-8. The
                                                            // kind was recorded
                                                            // on the opening
                                                            // fragment; validate
                                                            // the whole buffer
                                                            // before forwarding.
                                                            if matches!(kind, FragmentKind::Text)
                                                                && std::str::from_utf8(&buf).is_err()
                                                            {
                                                                let _ = tx
                                                                    .send(Err(InputStreamError::from_server_fn_error(
                                                                        server_fn::error::ServerFnErrorErr::Args(
                                                                            "reassembled websocket text message is not valid UTF-8 (RFC 6455 §8.1)".to_string(),
                                                                        ),
                                                                    )
                                                                    .ser()))
                                                                    .await;
                                                                return Ok(Some(close_invalid_utf8()));
                                                            }
                                                            let _ = tx.send(Ok(buf.freeze())).await;
                                                            Ok(None)
                                                        }
                                                        LastAction::NoOpener => protocol_close(
                                                            "unexpected terminal continuation frame",
                                                        ),
                                                        LastAction::Overflow(t) => overflow_close!(t),
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }))
                    }
                }),
            )
            .await
            .map_err(|e| {
                Error::from_server_fn_error(server_fn::error::ServerFnErrorErr::Request(
                    e.to_string(),
                ))
            })?;

            Ok((
                response_stream_rx,
                response_sink_tx,
                NtexServerResponse::from(response),
            ))
        })
    }
}

/// Builds a policy-violation `Close` message carrying `CloseCode::Size`
/// (1009, "Message Too Big" per RFC 6455 §7.4.1) with a human-readable
/// description. The corresponding `InputStreamError` is emitted at the
/// call site because its generic type is only in scope inside
/// `impl Req for NtexRequest`.
fn close_too_big(limit: usize) -> web::ws::Message {
    web::ws::Message::Close(Some(ntex::ws::CloseReason {
        code: ntex::ws::CloseCode::Size,
        description: Some(format!("message exceeds limit of {limit} bytes")),
    }))
}

/// Builds the policy-violation `Close` for a text frame whose payload is not
/// valid UTF-8, carrying [`CloseCode::Invalid`](ntex::ws::CloseCode::Invalid)
/// (1007, "Invalid frame payload data" per RFC 6455 §7.4.1; §8.1 requires text
/// messages to be valid UTF-8). ntex's `Frame::Text` exposes raw `Bytes`
/// WITHOUT validating UTF-8, so the bridge must validate before forwarding.
/// The matching `InputStreamError` is emitted at the call site because its
/// generic type is only in scope inside `impl Req for NtexRequest`.
fn close_invalid_utf8() -> web::ws::Message {
    web::ws::Message::Close(Some(ntex::ws::CloseReason {
        code: ntex::ws::CloseCode::Invalid,
        description: Some("text frame payload is not valid UTF-8".to_string()),
    }))
}

/// Builds the `Close` sent when an OUTBOUND server-fn message fails to reach
/// the peer, carrying [`CloseCode::Error`](ntex::ws::CloseCode::Error) (1011,
/// "Internal Error"). RFC 6455 §7.4.1 RESERVES 1006 ("Abnormal Closure") for
/// local reporting only — an endpoint must never place it in a Close control
/// frame, and ntex serializes whatever code it is given onto the wire verbatim.
fn close_send_failure(reason: &str) -> web::ws::Message {
    web::ws::Message::Close(Some(ntex::ws::CloseReason {
        code: ntex::ws::CloseCode::Error,
        description: Some(reason.to_string()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lets_expect::lets_expect;
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

    lets_expect! {
        expect(header_value(headers)) as the_request_header {
            let headers: &[(&str, &str)] = &[("x-probe", "probe-value")];

            to returns_the_exact_header_value { equal(Some("probe-value".to_string())) }

            when the_header_is_absent {
                let headers: &[(&str, &str)] = &[];
                to returns_none { be_none }
            }
        }
    }

    lets_expect! {
        expect(query(uri)) as the_request_query {
            let uri = "/path?foo=bar&baz=1";

            to returns_the_raw_query_string { equal(Some("foo=bar&baz=1".to_string())) }

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

            when the_accept_header_is_absent {
                let headers: &[(&str, &str)] = &[];
                to returns_none { be_none }
            }
        }
    }

    // ----- WebSocket policy-close codes ---------------------------------
    // The bridge builds policy `Close` frames whose CODE is the contract
    // (RFC 6455 §7.4.1). These lock the two added codes: invalid-UTF-8 text
    // closes with 1007, and an outbound send failure closes with 1011 — NOT
    // the reserved 1006, which an endpoint must never serialize onto the
    // wire. (`close_send_failure`'s path is otherwise unreachable in a wire
    // test, so this is its primary regression.)
    fn close_code(msg: &web::ws::Message) -> Option<ntex::ws::CloseCode> {
        match msg {
            web::ws::Message::Close(Some(reason)) => Some(reason.code),
            _ => None,
        }
    }

    lets_expect! {
        expect(close_code(&close_invalid_utf8())) as the_invalid_utf8_close {
            to carries_close_code_1007_invalid {
                equal(Some(ntex::ws::CloseCode::Invalid))
            }
        }
    }

    lets_expect! {
        expect(close_code(&close_send_failure("send failed"))) as the_send_failure_close {
            to carries_1011_internal_error_not_reserved_1006 {
                equal(Some(ntex::ws::CloseCode::Error))
            }
        }
    }
}
