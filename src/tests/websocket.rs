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
/// output first, with the CLIENT never closing — is pinned by
/// `websocket_bridge_sends_unprompted_close_when_output_stream_ends_first`
/// (the bridge sends an unprompted `Close(None)` when the output channel
/// ends on its own). `websocket_close_is_echoed` is a DIFFERENT state: the
/// client sends `Close` first and the bridge echoes it back.
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

/// RFC 6455 §8.1: a TEXT message must be valid UTF-8. ntex's codec hands the
/// bridge raw bytes WITHOUT validating, so a fragmented text message whose
/// reassembled payload is not valid UTF-8 must fail the connection with
/// `CloseCode::Invalid` (1007) — the invalid bytes must never reach the server
/// fn. Sibling of the `_over_limit_closes_with_size` text tests, varying the
/// payload-VALIDITY axis instead of the size axis.
///
/// Coverage note: only the FRAGMENTED text path is wire-reachable here, because
/// ntex's high-level client encodes unfragmented text as
/// `Message::Text(ByteString)` — UTF-8 by construction. The unfragmented
/// `Frame::Text` arm shares the same `from_utf8` guard and the same
/// `close_invalid_utf8()` helper (unit-tested in `request.rs`); only a raw,
/// non-ntex client could drive invalid bytes through it on the wire.
#[ntex::test]
async fn websocket_fragmented_text_with_invalid_utf8_closes_with_invalid() {
    use ntex::ws::{CloseCode, Item};

    register_explicit::<EchoWebsocket>();

    let srv =
        test::server(async || NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
    let sink = conn.sink();
    let rx = conn.receiver();

    // 0xFF is never a valid UTF-8 byte. Split across two fragments so the bytes
    // only form a complete message at `Last` — the point where the bridge
    // reassembles and validates.
    sink.send(ws::Message::Continuation(Item::FirstText(
        vec![0xffu8].into(),
    )))
    .await
    .unwrap();
    sink.send(ws::Message::Continuation(Item::Last(vec![0xfeu8].into())))
        .await
        .unwrap();

    let frame = recv_ws_frame(&rx).await;
    match frame {
        ws::Frame::Close(Some(reason)) => {
            assert_eq!(
                reason.code,
                CloseCode::Invalid,
                "invalid-UTF-8 text must close with 1007, got {:?}",
                reason.code
            );
        }
        other => panic!("expected Close(Invalid) on invalid-UTF-8 text, got {other:?}"),
    }
}

/// A reassembled `Last` frame that is SIMULTANEOUSLY over the payload limit
/// AND invalid UTF-8: production code checks the overflow condition
/// (`LastAction::Overflow`) strictly BEFORE the UTF-8 validation, which only
/// runs inside `LastAction::Complete`. A doubly-violating frame must
/// therefore close with `CloseCode::Size` (1009), not `CloseCode::Invalid`
/// (1007) — pinning this precedence directly, since the existing tests only
/// ever violate one axis at a time.
#[ntex::test]
async fn websocket_last_fragment_over_limit_and_invalid_utf8_closes_with_size() {
    use crate::LeptosServerFnConfig;
    use ntex::ws::{CloseCode, Item};

    register_explicit::<EchoWebsocket>();

    let srv = test::server(async || {
        NtexApp::new()
            .state(LeptosServerFnConfig {
                payload_limit: 4,
                ws_channel_buffer: 16,
                ..Default::default()
            })
            .route("/api/{tail}*", handle_server_fns())
    })
    .await;

    let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
    let sink = conn.sink();
    let rx = conn.receiver();

    // First(2) installs the buffer under the 4-byte limit; Last(4) carries
    // the running total to 6 > 4 (overflow) AND its bytes (0xFF, invalid
    // UTF-8 anywhere) would also fail validation if reached.
    sink.send(ws::Message::Continuation(Item::FirstText(
        vec![b'a', b'b'].into(),
    )))
    .await
    .unwrap();
    sink.send(ws::Message::Continuation(Item::Last(
        vec![0xffu8, 0xfeu8, b'c', b'd'].into(),
    )))
    .await
    .unwrap();

    let frame = recv_ws_frame(&rx).await;
    match frame {
        ws::Frame::Close(Some(reason)) => {
            assert_eq!(
                reason.code,
                CloseCode::Size,
                "overflow must win over the UTF-8 check on a doubly-violating frame, got {:?}",
                reason.code
            );
        }
        other => panic!("expected Close(Size) on a doubly-violating Last frame, got {other:?}"),
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

/// Multi-protocol offer sibling: `web::ws::subprotocols` yields ALL offered
/// values, but every other test here offers a set of size 0 or 1. This
/// offers TWO protocols with the configured one present but NOT first — a
/// regression that only checked the first offered protocol (rather than
/// `.any(...)` over all offered) would pass every other test in this file
/// but silently break a real multi-protocol client.
#[ntex::test]
async fn websocket_configured_subprotocol_is_echoed_when_offered_among_several() {
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

    let mut builder = ntex::ws::WsClient::builder(srv.url(EchoWebsocket::PATH));
    builder
        .address(srv.addr())
        .timeout(ntex::time::Seconds(60))
        .protocols(["other-ws", "graphql-ws"]);
    let client = builder
        .build(ntex::SharedCfg::new("ws-subprotocol-multi-test"))
        .await
        .unwrap();
    let conn = client.connect().await.unwrap();

    assert_eq!(
        conn.response()
            .headers()
            .get(ntex::http::header::SEC_WEBSOCKET_PROTOCOL)
            .and_then(|v| v.to_str().ok()),
        Some("graphql-ws"),
        "server must select the configured subprotocol even when it is not the first offered"
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
            // The bridge echoes the WHOLE close reason, not just the code, so
            // pin the description it was sent with too — otherwise a
            // regression dropping/altering the echoed description survives.
            assert_eq!(reason.description.as_deref(), Some("bye"));
        }
        other => panic!("expected Close echo, got {other:?}"),
    }
}

/// Cumulative overflow across `Continue`: a fragmented message whose OPENING
/// fragment is UNDER the limit but whose running total crosses it on a later
/// `Continue` frame must close with `CloseCode::Size`. The existing
/// `oversize_fragmented` test sends an opening fragment already past the limit,
/// so it only exercises the `FirstBinary` guard — this one pins the distinct
/// `Item::Continue` accumulation guard (`buf.len() + b.len() > limit`).
#[ntex::test]
async fn websocket_continuation_cumulative_overflow_closes_with_size() {
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

    // First(8) is under the limit and installs the buffer; Continue(12) pushes
    // the running total to 20 > 16, tripping the `Continue` accumulation guard.
    sink.send(ws::Message::Continuation(Item::FirstBinary(
        [b'A'; 8].to_vec().into(),
    )))
    .await
    .unwrap();
    sink.send(ws::Message::Continuation(Item::Continue(
        [b'B'; 12].to_vec().into(),
    )))
    .await
    .unwrap();

    let frame = recv_ws_frame(&rx).await;
    match frame {
        ws::Frame::Close(Some(reason)) => assert_eq!(reason.code, CloseCode::Size),
        other => panic!("expected Close(Size) on cumulative Continue overflow, got {other:?}"),
    }
}

/// Cumulative overflow on the TERMINAL `Last` frame: First(8) + Continue(4)
/// stays under the limit, then `Last(8)` carries the running total to 20 > 16.
/// Pins the `Item::Last` accumulation guard separately from the `Continue` one.
#[ntex::test]
async fn websocket_last_fragment_cumulative_overflow_closes_with_size() {
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

    sink.send(ws::Message::Continuation(Item::FirstBinary(
        [b'A'; 8].to_vec().into(),
    )))
    .await
    .unwrap();
    sink.send(ws::Message::Continuation(Item::Continue(
        [b'B'; 4].to_vec().into(),
    )))
    .await
    .unwrap();
    sink.send(ws::Message::Continuation(Item::Last(
        [b'C'; 8].to_vec().into(),
    )))
    .await
    .unwrap();

    let frame = recv_ws_frame(&rx).await;
    match frame {
        ws::Frame::Close(Some(reason)) => assert_eq!(reason.code, CloseCode::Size),
        other => panic!("expected Close(Size) on cumulative Last overflow, got {other:?}"),
    }
}

/// Negative sibling of the offered / not-offered subprotocol tests: when the
/// client offers a DIFFERENT subprotocol than the one configured, the server
/// must NOT select it (the production filter only echoes the configured value
/// when the client actually offers it). Without this case, a weaker predicate
/// that echoes the configured subprotocol whenever ANY is offered would pass
/// both existing tests.
#[ntex::test]
async fn websocket_configured_subprotocol_is_not_echoed_for_a_different_offer() {
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

    // Offer a subprotocol the server does NOT configure.
    let mut builder = ntex::ws::WsClient::builder(srv.url(EchoWebsocket::PATH));
    builder
        .address(srv.addr())
        .timeout(ntex::time::Seconds(60))
        .protocols(["other-ws"]);
    let client = builder
        .build(ntex::SharedCfg::new("ws-subprotocol-mismatch"))
        .await
        .unwrap();
    let conn = client.connect().await.unwrap();

    assert!(
        conn.response()
            .headers()
            .get(ntex::http::header::SEC_WEBSOCKET_PROTOCOL)
            .is_none(),
        "server must not select a subprotocol the client did not offer"
    );

    conn.sink().send(ws::Message::Close(None)).await.unwrap();
}

/// The combination no other test exercises: `ws_subprotocol` left at its
/// default `None` AND the client actually offers a protocol. A regression
/// treating `None` as "accept whatever the client offers" (e.g. echoing the
/// first offered protocol whenever the config has none configured) would
/// pass every existing test in this file, since none of them offers a
/// protocol while the config's `ws_subprotocol` is `None`.
#[ntex::test]
async fn websocket_no_configured_subprotocol_is_not_echoed_even_when_client_offers_one() {
    register_explicit::<EchoWebsocket>();

    // Default app config: `ws_subprotocol` is `None`.
    let srv =
        test::server(async || NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let mut builder = ntex::ws::WsClient::builder(srv.url(EchoWebsocket::PATH));
    builder
        .address(srv.addr())
        .timeout(ntex::time::Seconds(60))
        .protocols(["graphql-ws"]);
    let client = builder
        .build(ntex::SharedCfg::new("ws-subprotocol-none-config"))
        .await
        .unwrap();
    let conn = client.connect().await.unwrap();

    assert!(
        conn.response()
            .headers()
            .get(ntex::http::header::SEC_WEBSOCKET_PROTOCOL)
            .is_none(),
        "with no configured subprotocol, the response must not select one even if the client offers it"
    );

    conn.sink().send(ws::Message::Close(None)).await.unwrap();
}

// ----- ws_channel_buffer: lossless delivery under a minimal buffer -----

/// This test does NOT force the bounded `ws_channel_buffer` channel to be
/// genuinely full at send time (the ntex test client's frame reader drains
/// the wire into an unbounded local queue regardless of whether the test
/// calls `rx.recv()`, so simply deferring reads never backs up the server
/// side). It only pins ordered, lossless echo under a minimal
/// (`ws_channel_buffer: 1`) configuration: sends 20 messages back-to-back
/// against a channel buffer of 1 BEFORE reading any reply, then drains and
/// asserts all 20 arrive, in order, none lost. The sibling test
/// `websocket_slow_producer_without_losing_or_reordering_messages` below
/// pins the same lossless-delivery property from the opposite angle (a
/// naturally slow producer instead of a fast burst) — see that test's doc
/// comment for why this crate's bounded channels turn out to be
/// unobservable as an actual bottleneck through this local-loopback
/// harness in either direction, so neither test can claim to pin genuine
/// full-buffer backpressure.
#[ntex::test]
async fn websocket_bursts_more_messages_than_the_channel_buffer_without_losing_any() {
    use crate::LeptosServerFnConfig;

    register_explicit::<EchoWebsocket>();

    let srv = test::server(async || {
        NtexApp::new()
            .state(LeptosServerFnConfig {
                payload_limit: 1024,
                ws_channel_buffer: 1,
                ws_subprotocol: None,
            })
            .route("/api/{tail}*", handle_server_fns())
    })
    .await;

    let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
    let sink = conn.sink();
    let rx = conn.receiver();

    let messages: Vec<String> = (0..20).map(|i| format!("msg-{i}")).collect();
    for message in &messages {
        sink.send(ws::Message::Binary(serialize_ws_ok(message).into()))
            .await
            .unwrap();
    }

    for expected in &messages {
        let frame = recv_ws_frame(&rx).await;
        match frame {
            ws::Frame::Binary(bytes) => {
                let echoed: String = deserialize_ws_ok(&bytes);
                assert_eq!(
                    &echoed, expected,
                    "messages must arrive in order with none lost or reordered"
                );
            }
            other => panic!("expected a Binary echo, got {other:?}"),
        }
    }

    sink.send(ws::Message::Close(None)).await.unwrap();
}

/// A server-fn that echoes each input item back, but only after an
/// artificial per-item delay — a deliberately slow PRODUCER of the output
/// stream that the server-fn runtime forwards into the bounded
/// `response_sink_tx` channel (`request.rs`). Investigating what this
/// fixture can pin (see the doc comment on
/// `websocket_slow_producer_without_losing_or_reordering_messages` below)
/// established that neither direction's `ws_channel_buffer` capacity is
/// actually the bottleneck in this local-loopback harness: the pacing here
/// comes entirely from this fixture's own delay, not from the channel
/// filling up. It is kept as a real, narrower regression pin for lossless,
/// ordered delivery when the producer is naturally slow.
#[server(
    name = SlowEchoWebsocket,
    prefix = "/api",
    endpoint = "slow_echo_websocket",
    protocol = server_fn::Websocket<server_fn::codec::JsonEncoding, server_fn::codec::JsonEncoding>,
    server = crate::NtexServerFnBackend
)]
async fn slow_echo_websocket(
    input: server_fn::BoxedStream<String, ServerFnError>,
) -> Result<server_fn::BoxedStream<String, ServerFnError>, ServerFnError> {
    use futures::StreamExt;

    let input: std::pin::Pin<
        Box<dyn futures::Stream<Item = Result<String, ServerFnError>> + Send>,
    > = input.into();
    Ok(input
        .then(|item| async move {
            ntex::time::sleep(ntex::time::Millis(200)).await;
            item
        })
        .into())
}

/// NOT a `ws_channel_buffer`-fullness test (see the fixture's doc comment):
/// this crate's bounded channels turn out to be effectively unobservable as
/// a bottleneck through a local-loopback single-client harness in EITHER
/// direction —
///
/// - INBOUND: the WS frame-processing closure (`request.rs`) clones a fresh
///   `mpsc::Sender` per frame, and `futures::channel::mpsc` guarantees every
///   sender clone at least one reserved slot beyond the nominal buffer size,
///   so a burst of sends never measurably fills the channel regardless of
///   `ws_channel_buffer` (confirmed experimentally: swapping the inbound
///   `tx.send(...).await` for a non-blocking `try_send(...)` did not change
///   this test's outcome — every `try_send` succeeded immediately). See the
///   "ADDITIONAL BUG FOUND" note in the accompanying change summary.
/// - OUTBOUND: the bridge task's `outbound_sink.send(...).await` is a local,
///   effectively non-blocking wire write (confirmed experimentally: setting
///   `ws_channel_buffer` to 100 instead of 1 did not change this test's
///   timing at all), so the channel almost never backs up regardless of
///   whether the client is reading.
///
/// What IS pinned here: `SlowEchoWebsocket` is a naturally slow producer
/// (one item every 200ms) with a client that sends a burst up front and
/// only reads replies afterward. Despite the pacing mismatch, every message
/// must still arrive, in order, with none lost.
#[ntex::test]
async fn websocket_slow_producer_without_losing_or_reordering_messages() {
    use crate::LeptosServerFnConfig;

    register_explicit::<SlowEchoWebsocket>();

    let srv = test::server(async || {
        NtexApp::new()
            .state(LeptosServerFnConfig {
                payload_limit: 1024,
                ws_channel_buffer: 1,
                ws_subprotocol: None,
            })
            .route("/api/{tail}*", handle_server_fns())
    })
    .await;

    let conn = srv.ws_at(SlowEchoWebsocket::PATH).await.unwrap();
    let sink = conn.sink();
    let rx = conn.receiver();

    let messages: Vec<String> = (0..5).map(|i| format!("slow-msg-{i}")).collect();

    for message in &messages {
        sink.send(ws::Message::Binary(serialize_ws_ok(message).into()))
            .await
            .unwrap();
    }

    // No message may be lost or reordered despite the producer's pacing.
    for expected in &messages {
        let frame = recv_ws_frame(&rx).await;
        match frame {
            ws::Frame::Binary(bytes) => {
                let echoed: String = deserialize_ws_ok(&bytes);
                assert_eq!(
                    &echoed, expected,
                    "messages must arrive in order with none lost or reordered \
                     even from a naturally slow producer"
                );
            }
            other => panic!("expected a Binary echo, got {other:?}"),
        }
    }

    sink.send(ws::Message::Close(None)).await.unwrap();
}

// ----- unprompted server-side Close (output stream ends first) ---------

/// A server-fn whose OUTPUT stream completes on its own after exactly two
/// items — regardless of whether the client keeps sending or the connection
/// stays open. Mirrors `EchoWebsocket`'s echo shape but bounds the output
/// with `.take(2)`, so the outbound-sink task's `None => break` branch
/// (`request.rs`, the `response_sink_rx.next()` arm) fires without any
/// client-side close.
#[server(
    name = FiniteEchoWebsocket,
    prefix = "/api",
    endpoint = "finite_echo_websocket",
    protocol = server_fn::Websocket<server_fn::codec::JsonEncoding, server_fn::codec::JsonEncoding>,
    server = crate::NtexServerFnBackend
)]
async fn finite_echo_websocket(
    input: server_fn::BoxedStream<String, ServerFnError>,
) -> Result<server_fn::BoxedStream<String, ServerFnError>, ServerFnError> {
    use futures::StreamExt;

    let input: std::pin::Pin<
        Box<dyn futures::Stream<Item = Result<String, ServerFnError>> + Send>,
    > = input.into();
    Ok(input.take(2).into())
}

/// A doc comment on `websocket_client_close_releases_the_server_fn_output_stream`
/// claims the sibling "server ends output first" state is pinned by
/// `websocket_close_is_echoed` — but that test only exercises the CLIENT
/// sending `Close`, never the bridge's OWN unprompted `Close(None)` when the
/// server-fn output stream ends naturally while the client stays connected.
/// This drives that case directly: `FiniteEchoWebsocket` completes its
/// output after two echoed messages, and the client — which never sends a
/// `Close` itself — must still receive an unprompted `Close(None)` once the
/// output stream ends.
#[ntex::test]
async fn websocket_bridge_sends_unprompted_close_when_output_stream_ends_first() {
    register_explicit::<FiniteEchoWebsocket>();

    let srv =
        test::server(async || NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let conn = srv.ws_at(FiniteEchoWebsocket::PATH).await.unwrap();
    let sink = conn.sink();
    let rx = conn.receiver();

    sink.send(ws::Message::Binary(serialize_ws_ok(&"one").into()))
        .await
        .unwrap();
    let frame = recv_ws_frame(&rx).await;
    match frame {
        ws::Frame::Binary(bytes) => assert_eq!(deserialize_ws_ok::<String>(&bytes), "one"),
        other => panic!("expected the first echo, got {other:?}"),
    }

    sink.send(ws::Message::Binary(serialize_ws_ok(&"two").into()))
        .await
        .unwrap();
    let frame = recv_ws_frame(&rx).await;
    match frame {
        ws::Frame::Binary(bytes) => assert_eq!(deserialize_ws_ok::<String>(&bytes), "two"),
        other => panic!("expected the second echo, got {other:?}"),
    }

    // The client never sends Close. The output stream ends on its own after
    // the second item, so the bridge must send an UNPROMPTED Close(None).
    let frame = recv_ws_frame(&rx).await;
    assert!(
        matches!(frame, ws::Frame::Close(None)),
        "expected an unprompted Close(None) once the output stream ended, got {frame:?}"
    );
}
