use super::*;
use crate::{handle_server_fns, register_explicit};
use ntex::web::ws;
use ntex::web::{App as NtexApp, test};
use server_fn::ServerFn;
use server_fn::serde;

fn serialize_ws_ok<T: serde::Serialize>(value: &T) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(1);
    bytes.push(0);
    bytes.extend(serde_json::to_vec(value).unwrap());
    bytes
}

fn deserialize_ws_ok<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> T {
    assert!(!bytes.is_empty());
    assert_eq!(bytes[0], 0);
    serde_json::from_slice(&bytes[1..]).unwrap()
}

async fn recv_ws_frame<E: std::fmt::Debug>(
    rx: &ntex::channel::mpsc::Receiver<Result<ws::Frame, E>>,
) -> ws::Frame {
    ntex::time::timeout(ntex::time::Millis(1_000), rx.recv())
        .await
        .expect("timed out waiting for websocket frame")
        .expect("websocket receiver closed")
        .expect("websocket frame error")
}

/// Regression for the bridge-task leak: after the client closes the
/// connection, the bridge must notice the disconnect and release its
/// input-sender clone, so the server fn sees EOF and its OUTPUT stream is
/// dropped. The sibling state of the shutdown axis — the SERVER ending its
/// output first — is pinned by `websocket_close_is_echoed` (the bridge
/// sends `Close` when the output channel ends).
#[ntex::test]
async fn websocket_client_close_releases_the_server_fn_output_stream() {
    use std::sync::atomic::Ordering;

    register_explicit::<LeakProbeWebsocket>();

    let srv =
        test::server(async || NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let conn = srv.ws_at(LeakProbeWebsocket::PATH).await.unwrap();
    let sink = conn.sink();
    let rx = conn.receiver();

    // One echoed message proves the full pipeline (and the probe) is up
    // BEFORE the close — otherwise the assertion below could pass vacuously.
    let payload = serialize_ws_ok(&"probe");
    sink.send(ws::Message::Text(
        String::from_utf8(payload).unwrap().into(),
    ))
    .await
    .unwrap();
    let frame = recv_ws_frame(&rx).await;
    assert!(
        matches!(frame, ws::Frame::Binary(_)),
        "expected echo before close, got {frame:?}"
    );
    assert!(
        !WS_PROBE_OUTPUT_DROPPED.load(Ordering::SeqCst),
        "the output stream must be alive while the connection is open"
    );

    sink.send(ws::Message::Close(None)).await.unwrap();
    drop(rx);
    drop(sink);

    // The bridge reacts to the disconnect asynchronously — poll with a cap.
    let mut released = false;
    for _ in 0..100 {
        if WS_PROBE_OUTPUT_DROPPED.load(Ordering::SeqCst) {
            released = true;
            break;
        }
        ntex::time::sleep(ntex::time::Millis(20)).await;
    }
    assert!(
        released,
        "server-fn output stream was not dropped within 2s of the client \
         close — the websocket bridge is leaking"
    );
}

#[ntex::test]
async fn websocket_server_fn_echoes_messages() {
    register_explicit::<EchoName>();
    register_explicit::<RedirectToAbout>();
    register_explicit::<EchoWebsocket>();

    let srv =
        test::server(async || NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
    let sink = conn.sink();
    let rx = conn.receiver();

    sink.send(ws::Message::Binary(serialize_ws_ok(&"hello").into()))
        .await
        .unwrap();

    let frame = recv_ws_frame(&rx).await;
    match frame {
        ws::Frame::Binary(bytes) => {
            let echoed: String = deserialize_ws_ok(&bytes);
            assert_eq!(echoed, "hello");
        }
        other => panic!("unexpected websocket frame: {other:?}"),
    }

    sink.send(ws::Message::Close(None)).await.unwrap();
}

/// RFC 6455 §5.4: fragmented binary messages must be reassembled
/// before delivery to the server-fn.
#[ntex::test]
async fn websocket_server_fn_reassembles_fragmented_binary() {
    use ntex::ws::Item;

    register_explicit::<EchoWebsocket>();

    let srv =
        test::server(async || NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
    let sink = conn.sink();
    let rx = conn.receiver();

    // Build the full payload and split into three chunks.
    let full = serialize_ws_ok(&"fragmented-hello");
    let (head, tail) = full.split_at(4);
    let (mid, last) = tail.split_at(tail.len() / 2);

    sink.send(ws::Message::Continuation(Item::FirstBinary(
        head.to_vec().into(),
    )))
    .await
    .unwrap();
    sink.send(ws::Message::Continuation(Item::Continue(
        mid.to_vec().into(),
    )))
    .await
    .unwrap();
    sink.send(ws::Message::Continuation(Item::Last(last.to_vec().into())))
        .await
        .unwrap();

    let frame = recv_ws_frame(&rx).await;
    match frame {
        ws::Frame::Binary(bytes) => {
            let echoed: String = deserialize_ws_ok(&bytes);
            assert_eq!(echoed, "fragmented-hello");
        }
        other => panic!("unexpected websocket frame: {other:?}"),
    }

    sink.send(ws::Message::Close(None)).await.unwrap();
}

/// RFC 6455 §5.4: fragmented **text** messages must be reassembled
/// before delivery — the mirror of the binary case above. Mutation
/// testing flagged the `FirstText` size guard (`request.rs`
/// `Item::FirstText`, `b.len() > payload_limit`): the at-limit text
/// test sends exactly `== limit`, where `>` and `<` agree (both
/// install), so it cannot pin the comparison's direction. A completing
/// message whose opening fragment is strictly UNDER the limit does —
/// the correct `>` installs it, while `<` would wrongly reject a normal
/// small fragment as overflow. This also drives the under-limit
/// `Continue`/`Last` accumulation guards.
#[ntex::test]
async fn websocket_server_fn_reassembles_fragmented_text() {
    use ntex::ws::Item;

    register_explicit::<EchoWebsocket>();

    let srv =
        test::server(async || NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
    let sink = conn.sink();
    let rx = conn.receiver();

    // Three chunks, each well under the (default) limit. `Item::FirstText`
    // carries raw bytes; the text kind is tracked by the opcode.
    let full = serialize_ws_ok(&"fragmented-text-hello");
    let (head, tail) = full.split_at(4);
    let (mid, last) = tail.split_at(tail.len() / 2);

    sink.send(ws::Message::Continuation(Item::FirstText(
        head.to_vec().into(),
    )))
    .await
    .unwrap();
    sink.send(ws::Message::Continuation(Item::Continue(
        mid.to_vec().into(),
    )))
    .await
    .unwrap();
    sink.send(ws::Message::Continuation(Item::Last(last.to_vec().into())))
        .await
        .unwrap();

    let frame = recv_ws_frame(&rx).await;
    match frame {
        ws::Frame::Binary(bytes) => {
            let echoed: String = deserialize_ws_ok(&bytes);
            assert_eq!(echoed, "fragmented-text-hello");
        }
        other => panic!("unexpected websocket frame: {other:?}"),
    }

    sink.send(ws::Message::Close(None)).await.unwrap();
}

/// The TEXT mirror of the oversized-opening-fragment test: a `FirstText`
/// fragment already past the limit must be rejected with `CloseCode::Size`
/// before any buffer is established, exactly as `FirstBinary` is. With the
/// at-limit and under-limit text tests this pins all three points of the
/// `FirstText` size axis (the binary axis was already complete).
#[ntex::test]
async fn websocket_server_fn_first_text_fragment_over_limit_closes_with_size() {
    use crate::LeptosServerFnConfig;
    use ntex::ws::{CloseCode, Item};

    register_explicit::<EchoWebsocket>();

    let srv = test::server(async || {
        NtexApp::new()
            .state(LeptosServerFnConfig {
                payload_limit: 16,
                ws_channel_buffer: 16,
                ..Default::default()
            })
            .route("/api/{tail}*", handle_server_fns())
    })
    .await;

    let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
    let sink = conn.sink();
    let rx = conn.receiver();

    let oversize = [b'A'; 64];
    sink.send(ws::Message::Continuation(Item::FirstText(
        oversize.to_vec().into(),
    )))
    .await
    .unwrap();

    let frame = recv_ws_frame(&rx).await;
    match frame {
        ws::Frame::Close(Some(reason)) => {
            assert_eq!(reason.code, CloseCode::Size);
        }
        other => panic!("expected Close(Size) on oversized FirstText frame, got {other:?}"),
    }
}

/// An oversized opening fragment (`FirstBinary`/`FirstText`) must
/// be rejected with `CloseCode::Size` *before* the buffer is
/// established. A prior bug simply pushed the bytes into the buffer
/// without a size check, so a client could already exceed the limit
/// on the first frame.
#[ntex::test]
async fn websocket_server_fn_first_fragment_over_limit_closes_with_size() {
    use crate::LeptosServerFnConfig;
    use ntex::ws::{CloseCode, Item};

    register_explicit::<EchoWebsocket>();

    let srv = test::server(async || {
        NtexApp::new()
            .state(LeptosServerFnConfig {
                payload_limit: 16,
                ws_channel_buffer: 16,
                ..Default::default()
            })
            .route("/api/{tail}*", handle_server_fns())
    })
    .await;

    let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
    let sink = conn.sink();
    let rx = conn.receiver();

    let oversize = [b'A'; 64];
    sink.send(ws::Message::Continuation(Item::FirstBinary(
        oversize.to_vec().into(),
    )))
    .await
    .unwrap();

    let frame = recv_ws_frame(&rx).await;
    match frame {
        ws::Frame::Close(Some(reason)) => {
            assert_eq!(reason.code, CloseCode::Size);
        }
        other => panic!("expected Close(Size) on oversized First* frame, got {other:?}"),
    }
}

// Note: the interleaved-First* protocol check in the bridge is
// defense-in-depth. ntex's own WS codec enforces the same
// invariant at the frame decoder/encoder layer
// (`ProtocolError::ContinuationStarted`), so a well-behaved client
// cannot even send such a frame sequence through `ntex::ws`. The
// server-side guard catches the case where a custom framer or
// future codec change would forward the bad sequence to us.

/// When a reassembled fragmented message exceeds `payload_limit`,
/// the bridge must close the connection with `CloseCode::Size`
/// (1009, "Message Too Big" per RFC 6455 §7.4.1) and not forward
/// the partial payload to the server-fn.
#[ntex::test]
async fn websocket_server_fn_oversize_fragmented_closes_with_size() {
    use crate::LeptosServerFnConfig;
    use ntex::ws::{CloseCode, Item};

    register_explicit::<EchoWebsocket>();

    let srv = test::server(async || {
        NtexApp::new()
            .state(LeptosServerFnConfig {
                payload_limit: 16,
                ws_channel_buffer: 16,
                ..Default::default()
            })
            .route("/api/{tail}*", handle_server_fns())
    })
    .await;

    let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
    let sink = conn.sink();
    let rx = conn.receiver();

    let blob = [b'A'; 100];
    sink.send(ws::Message::Continuation(Item::FirstBinary(
        blob[..40].to_vec().into(),
    )))
    .await
    .unwrap();
    sink.send(ws::Message::Continuation(Item::Continue(
        blob[40..80].to_vec().into(),
    )))
    .await
    .unwrap();

    // The next frame received must be a Close with CloseCode::Size.
    let frame = recv_ws_frame(&rx).await;
    match frame {
        ws::Frame::Close(Some(reason)) => {
            assert_eq!(reason.code, CloseCode::Size);
        }
        other => panic!("expected Close(Size), got {other:?} (limit enforcement regressed)"),
    }
}

// ----- WebSocket payload-limit BOUNDARY -----------------------------
// The existing oversize tests send well past the limit, so they cannot
// distinguish `len > limit` from `len >= limit`. These send a payload of
// *exactly* `payload_limit` bytes: it is within the limit (the check is
// strict `>`), so it must be DELIVERED, not closed with `Size`. A
// 13-char string serializes to a 16-byte server-fn frame (1 ok-marker +
// `"<13 chars>"`), matched to a 16-byte limit.

/// Unfragmented binary frame exactly at the limit is echoed, not closed.
#[ntex::test]
async fn websocket_unfragmented_binary_exactly_at_limit_is_delivered() {
    register_explicit::<EchoWebsocket>();

    let srv = test::server(async || {
        NtexApp::new()
            .state(crate::LeptosServerFnConfig {
                payload_limit: 16,
                ws_channel_buffer: 16,
                ..Default::default()
            })
            .route("/api/{tail}*", handle_server_fns())
    })
    .await;

    let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
    let sink = conn.sink();
    let rx = conn.receiver();

    let message = "a".repeat(13);
    let payload = serialize_ws_ok(&message);
    assert_eq!(payload.len(), 16, "payload must sit exactly on the limit");

    sink.send(ws::Message::Binary(payload.into()))
        .await
        .unwrap();

    let frame = recv_ws_frame(&rx).await;
    match frame {
        ws::Frame::Binary(bytes) => {
            let echoed: String = deserialize_ws_ok(&bytes);
            assert_eq!(echoed, message);
        }
        other => panic!("expected the at-limit message echoed, got {other:?}"),
    }

    sink.send(ws::Message::Close(None)).await.unwrap();
}

/// Unfragmented text frame exactly at the limit is echoed, not closed.
#[ntex::test]
async fn websocket_unfragmented_text_exactly_at_limit_is_delivered() {
    register_explicit::<EchoWebsocket>();

    let srv = test::server(async || {
        NtexApp::new()
            .state(crate::LeptosServerFnConfig {
                payload_limit: 16,
                ws_channel_buffer: 16,
                ..Default::default()
            })
            .route("/api/{tail}*", handle_server_fns())
    })
    .await;

    let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
    let sink = conn.sink();
    let rx = conn.receiver();

    let message = "a".repeat(13);
    let payload = serialize_ws_ok(&message);
    assert_eq!(payload.len(), 16);
    let text = ntex::util::ByteString::try_from(payload).unwrap();

    sink.send(ws::Message::Text(text)).await.unwrap();

    let frame = recv_ws_frame(&rx).await;
    match frame {
        ws::Frame::Binary(bytes) => {
            let echoed: String = deserialize_ws_ok(&bytes);
            assert_eq!(echoed, message);
        }
        other => panic!("expected the at-limit text message echoed, got {other:?}"),
    }

    sink.send(ws::Message::Close(None)).await.unwrap();
}

/// A single opening fragment (`FirstBinary`) exactly at the limit is
/// accepted; the terminal `Last` frame then completes the message at the
/// limit too. Pins the `>` vs `>=` checks on First* and Last.
#[ntex::test]
async fn websocket_first_fragment_exactly_at_limit_is_delivered() {
    use ntex::ws::Item;

    register_explicit::<EchoWebsocket>();

    let srv = test::server(async || {
        NtexApp::new()
            .state(crate::LeptosServerFnConfig {
                payload_limit: 16,
                ws_channel_buffer: 16,
                ..Default::default()
            })
            .route("/api/{tail}*", handle_server_fns())
    })
    .await;

    let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
    let sink = conn.sink();
    let rx = conn.receiver();

    let message = "a".repeat(13);
    let payload = serialize_ws_ok(&message);
    assert_eq!(payload.len(), 16);

    // Whole payload in the opening fragment (len == limit), then an empty
    // terminal frame (cumulative len == limit).
    sink.send(ws::Message::Continuation(Item::FirstBinary(payload.into())))
        .await
        .unwrap();
    sink.send(ws::Message::Continuation(Item::Last(Vec::new().into())))
        .await
        .unwrap();

    let frame = recv_ws_frame(&rx).await;
    match frame {
        ws::Frame::Binary(bytes) => {
            let echoed: String = deserialize_ws_ok(&bytes);
            assert_eq!(echoed, message);
        }
        other => panic!("expected the at-limit fragmented message echoed, got {other:?}"),
    }

    sink.send(ws::Message::Close(None)).await.unwrap();
}

/// A message reassembled across `Continue` to exactly the limit must be
/// delivered. The cumulative size hits the limit on the `Continue` frame.
#[ntex::test]
async fn websocket_reassembled_continuation_exactly_at_limit_is_delivered() {
    use ntex::ws::Item;

    register_explicit::<EchoWebsocket>();

    let srv = test::server(async || {
        NtexApp::new()
            .state(crate::LeptosServerFnConfig {
                payload_limit: 16,
                ws_channel_buffer: 16,
                ..Default::default()
            })
            .route("/api/{tail}*", handle_server_fns())
    })
    .await;

    let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
    let sink = conn.sink();
    let rx = conn.receiver();

    let message = "a".repeat(13);
    let payload = serialize_ws_ok(&message);
    assert_eq!(payload.len(), 16);

    // First(8) + Continue(8): cumulative hits 16 exactly on the Continue.
    sink.send(ws::Message::Continuation(Item::FirstBinary(
        payload[0..8].to_vec().into(),
    )))
    .await
    .unwrap();
    sink.send(ws::Message::Continuation(Item::Continue(
        payload[8..16].to_vec().into(),
    )))
    .await
    .unwrap();
    sink.send(ws::Message::Continuation(Item::Last(Vec::new().into())))
        .await
        .unwrap();

    let frame = recv_ws_frame(&rx).await;
    match frame {
        ws::Frame::Binary(bytes) => {
            let echoed: String = deserialize_ws_ok(&bytes);
            assert_eq!(echoed, message);
        }
        other => panic!("expected the at-limit reassembled message echoed, got {other:?}"),
    }

    sink.send(ws::Message::Close(None)).await.unwrap();
}

/// An opening TEXT fragment (`FirstText`) exactly at the limit is
/// accepted (the `FirstText` size guard mirrors `FirstBinary`).
#[ntex::test]
async fn websocket_first_text_fragment_exactly_at_limit_is_delivered() {
    use ntex::ws::Item;

    register_explicit::<EchoWebsocket>();

    let srv = test::server(async || {
        NtexApp::new()
            .state(crate::LeptosServerFnConfig {
                payload_limit: 16,
                ws_channel_buffer: 16,
                ..Default::default()
            })
            .route("/api/{tail}*", handle_server_fns())
    })
    .await;

    let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
    let sink = conn.sink();
    let rx = conn.receiver();

    let message = "a".repeat(13);
    let payload = serialize_ws_ok(&message);
    assert_eq!(payload.len(), 16);

    // `Item::FirstText` carries raw `Bytes` (the text-vs-binary kind is
    // tracked by the frame opcode, not the payload type).
    sink.send(ws::Message::Continuation(Item::FirstText(payload.into())))
        .await
        .unwrap();
    sink.send(ws::Message::Continuation(Item::Last(Vec::new().into())))
        .await
        .unwrap();

    let frame = recv_ws_frame(&rx).await;
    match frame {
        ws::Frame::Binary(bytes) => {
            let echoed: String = deserialize_ws_ok(&bytes);
            assert_eq!(echoed, message);
        }
        other => panic!("expected the at-limit text fragment echoed, got {other:?}"),
    }

    sink.send(ws::Message::Close(None)).await.unwrap();
}

// -- WebSocket frame-type coverage ----------------------------------

/// A single unfragmented Binary frame exceeding `payload_limit` must
/// close with `CloseCode::Size`.
#[ntex::test]
async fn websocket_unfragmented_binary_oversize_closes_with_size() {
    use crate::LeptosServerFnConfig;
    use ntex::ws::CloseCode;

    register_explicit::<EchoWebsocket>();

    let srv = test::server(async || {
        NtexApp::new()
            .state(LeptosServerFnConfig {
                payload_limit: 16,
                ws_channel_buffer: 16,
                ..Default::default()
            })
            .route("/api/{tail}*", handle_server_fns())
    })
    .await;

    let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
    let sink = conn.sink();
    let rx = conn.receiver();

    let oversize = vec![b'X'; 64];
    sink.send(ws::Message::Binary(oversize.into()))
        .await
        .unwrap();

    let frame = recv_ws_frame(&rx).await;
    match frame {
        ws::Frame::Close(Some(reason)) => {
            assert_eq!(reason.code, CloseCode::Size);
        }
        other => panic!("expected Close(Size) on oversized Binary frame, got {other:?}"),
    }
}

/// A single unfragmented Text frame must be forwarded to the
/// server-fn and echoed back.
#[ntex::test]
async fn websocket_text_frame_echoed() {
    register_explicit::<EchoWebsocket>();

    let srv =
        test::server(async || NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
    let sink = conn.sink();
    let rx = conn.receiver();

    let payload = serialize_ws_ok(&"text-hello");
    sink.send(ws::Message::Text(
        String::from_utf8(payload).unwrap().into(),
    ))
    .await
    .unwrap();

    let frame = recv_ws_frame(&rx).await;
    match frame {
        ws::Frame::Binary(bytes) => {
            let echoed: String = deserialize_ws_ok(&bytes);
            assert_eq!(echoed, "text-hello");
        }
        other => panic!("expected Binary echo response, got {other:?}"),
    }

    sink.send(ws::Message::Close(None)).await.unwrap();
}

/// An oversized Text frame must close with `CloseCode::Size`.
#[ntex::test]
async fn websocket_text_oversize_closes_with_size() {
    use crate::LeptosServerFnConfig;
    use ntex::ws::CloseCode;

    register_explicit::<EchoWebsocket>();

    let srv = test::server(async || {
        NtexApp::new()
            .state(LeptosServerFnConfig {
                payload_limit: 16,
                ws_channel_buffer: 16,
                ..Default::default()
            })
            .route("/api/{tail}*", handle_server_fns())
    })
    .await;

    let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
    let sink = conn.sink();
    let rx = conn.receiver();

    let oversize_text = "X".repeat(64);
    sink.send(ws::Message::Text(oversize_text.into()))
        .await
        .unwrap();

    let frame = recv_ws_frame(&rx).await;
    match frame {
        ws::Frame::Close(Some(reason)) => {
            assert_eq!(reason.code, CloseCode::Size);
        }
        other => panic!("expected Close(Size) on oversized Text frame, got {other:?}"),
    }
}

/// A Ping frame must be answered with Pong.
#[ntex::test]
async fn websocket_ping_receives_pong() {
    register_explicit::<EchoWebsocket>();

    let srv =
        test::server(async || NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
    let sink = conn.sink();
    let rx = conn.receiver();

    sink.send(ws::Message::Ping(ntex::util::Bytes::from_static(
        b"ping-data",
    )))
    .await
    .unwrap();

    let frame = recv_ws_frame(&rx).await;
    match frame {
        ws::Frame::Pong(data) => {
            assert_eq!(&data[..], b"ping-data");
        }
        other => panic!("expected Pong, got {other:?}"),
    }

    sink.send(ws::Message::Close(None)).await.unwrap();
}

/// Positive sibling of the not-offered case below: when the client OFFERS
/// the configured subprotocol, the 101 upgrade response must select it via
/// `Sec-WebSocket-Protocol` (RFC 6455 §4.2.2). Without this leaf, a
/// regression that stops echoing the configured subprotocol entirely would
/// stay green — the negative sibling alone cannot catch it.
#[ntex::test]
async fn websocket_configured_subprotocol_is_echoed_when_offered() {
    use crate::LeptosServerFnConfig;

    register_explicit::<EchoWebsocket>();

    let srv = test::server(async || {
        NtexApp::new()
            .state(LeptosServerFnConfig {
                payload_limit: 1024,
                ws_channel_buffer: 16,
                ws_subprotocol: Some("graphql-ws"),
            })
            .route("/api/{tail}*", handle_server_fns())
    })
    .await;

    // `TestServer::ws_at` offers no subprotocol, so build the client by hand
    // with the configured protocol in its handshake.
    let mut builder = ntex::ws::WsClient::builder(srv.url(EchoWebsocket::PATH));
    builder
        .address(srv.addr())
        .timeout(ntex::time::Seconds(60))
        .protocols(["graphql-ws"]);
    let client = builder
        .build(ntex::SharedCfg::new("ws-subprotocol-test"))
        .await
        .unwrap();
    let conn = client.connect().await.unwrap();

    assert_eq!(
        conn.response()
            .headers()
            .get(ntex::http::header::SEC_WEBSOCKET_PROTOCOL)
            .and_then(|v| v.to_str().ok()),
        Some("graphql-ws"),
        "server must select the configured subprotocol when the client offers it"
    );

    conn.sink().send(ws::Message::Close(None)).await.unwrap();
}

#[ntex::test]
async fn websocket_configured_subprotocol_is_not_echoed_unless_offered() {
    use crate::LeptosServerFnConfig;

    register_explicit::<EchoWebsocket>();

    let srv = test::server(async || {
        NtexApp::new()
            .state(LeptosServerFnConfig {
                payload_limit: 1024,
                ws_channel_buffer: 16,
                ws_subprotocol: Some("graphql-ws"),
            })
            .route("/api/{tail}*", handle_server_fns())
    })
    .await;

    let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
    assert!(
        conn.response()
            .headers()
            .get(ntex::http::header::SEC_WEBSOCKET_PROTOCOL)
            .is_none(),
        "server must not echo a subprotocol that the client did not offer"
    );

    conn.sink().send(ws::Message::Close(None)).await.unwrap();
}

/// A Close frame must be echoed back.
#[ntex::test]
async fn websocket_close_is_echoed() {
    use ntex::ws::CloseCode;

    register_explicit::<EchoWebsocket>();

    let srv =
        test::server(async || NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
    let sink = conn.sink();
    let rx = conn.receiver();

    sink.send(ws::Message::Close(Some(ws::CloseReason {
        code: CloseCode::Normal,
        description: Some("bye".into()),
    })))
    .await
    .unwrap();

    let frame = recv_ws_frame(&rx).await;
    match frame {
        ws::Frame::Close(Some(reason)) => {
            assert_eq!(reason.code, CloseCode::Normal);
        }
        other => panic!("expected Close echo, got {other:?}"),
    }
}
