use super::*;
use crate::{LeptosServerFnConfig, NtexRequest, handle_server_fns, register_explicit};
use futures::{SinkExt, StreamExt};
use lets_expect::*;
use ntex::{
    http::StatusCode,
    web::{self, App, test},
    ws,
};
use server_fn::{ServerFn, error::ServerFnError};
use std::io::{Read, Write};

#[derive(Debug, PartialEq)]
struct HandshakeObservation {
    status: u16,
    transfer_encoding: bool,
    versions: Option<String>,
    subprotocol: Option<String>,
}

fn request_headers(
    key: &str,
    upgrade: &str,
    connection: &str,
    version: &str,
    extra: &str,
) -> String {
    format!(
        "GET {} HTTP/1.1\r\nHost: localhost\r\nUpgrade: {upgrade}\r\nConnection: {connection}\r\nSec-WebSocket-Version: {version}\r\nSec-WebSocket-Key: {key}\r\n{extra}\r\n",
        EchoWebsocket::PATH
    )
}

async fn handshake(
    key: &str,
    upgrade: &str,
    connection: &str,
    version: &str,
    extra: &str,
) -> HandshakeObservation {
    handshake_with_protocol(key, upgrade, connection, version, extra, None).await
}

async fn configured_handshake(configured: Option<&'static str>) -> HandshakeObservation {
    handshake_with_protocol(
        "dGhlIHNhbXBsZSBub25jZQ==",
        "websocket",
        "upgrade",
        "13",
        "Sec-WebSocket-Protocol: chat,\r\n",
        configured,
    )
    .await
}

async fn handshake_with_protocol(
    key: &str,
    upgrade: &str,
    connection: &str,
    version: &str,
    extra: &str,
    configured: Option<&'static str>,
) -> HandshakeObservation {
    register_explicit::<EchoWebsocket>();
    let server = test::server(move || async move {
        App::new()
            .state(LeptosServerFnConfig {
                ws_subprotocol: configured,
                ..Default::default()
            })
            .route("/api/{tail}*", handle_server_fns())
    })
    .await;
    let address = server.addr();
    let request = request_headers(key, upgrade, connection, version, extra);
    let headers = ntex::rt::spawn_blocking(move || {
        let mut socket = std::net::TcpStream::connect(address).unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        socket.write_all(request.as_bytes()).unwrap();
        let mut response = Vec::new();
        while !response.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            socket
                .read_exact(&mut byte)
                .expect("complete local response headers");
            response.push(byte[0]);
            assert!(response.len() < 8192, "bounded fixture response headers");
        }
        String::from_utf8(response).unwrap()
    })
    .await
    .unwrap();
    let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
    let header = |name: &str| {
        headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim().to_owned())
    };
    HandshakeObservation {
        status,
        transfer_encoding: header("transfer-encoding").is_some(),
        versions: header("sec-websocket-version"),
        subprotocol: header("sec-websocket-protocol"),
    }
}

lets_expect! {
    expect(run_ntex(configured_handshake(configured))) as configured_subprotocol_response {
        let configured = Some("chat");
        to selects_the_matching_valid_protocol {
            have(status) equal(101), have(subprotocol) equal(Some("chat".to_owned()))
        }
        when the_configured_protocol_is_empty {
            let configured = Some("");
            to upgrades_without_a_selected_protocol { have(status) equal(101), have(subprotocol) be_none }
        }
        when the_configured_protocol_is_not_a_token {
            let configured = Some("bad token");
            to upgrades_without_a_selected_protocol { have(status) equal(101), have(subprotocol) be_none }
        }
        when no_protocol_is_configured {
            let configured = None;
            to upgrades_without_a_selected_protocol { have(status) equal(101), have(subprotocol) be_none }
        }
    }
    expect(run_ntex(handshake(key, upgrade, connection, version, extra))) as handshake_contract {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let upgrade = "websocket";
        let connection = "upgrade";
        let version = "13";
        let extra = "";
        to switch_protocol { have(status) equal(101) }
        to omit_body_transfer_encoding { have(transfer_encoding) be_false }
        when key_is_malformed { let key = "x"; to reject_key { have(status) equal(400) } }
        when key_is_repeated { let extra = "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"; to reject_duplicate_key { have(status) equal(400) } }
        when upgrade_is_a_mixed_case_list { let upgrade = "h2c, WebSocket"; to select_websocket { have(status) equal(101) } }
        when upgrade_names_another_protocol { let upgrade = "notwebsocket"; to reject_protocol { have(status) equal(400) } }
        when connection_does_not_request_upgrade { let connection = "keep-alive"; to reject_connection { have(status) equal(400) } }
        when version_is_legacy_seven { let version = "7"; to preserve_legacy_support { have(status) equal(101) } }
        when version_is_legacy_eight { let version = "8"; to preserve_legacy_support { have(status) equal(101) } }
        when version_is_unsupported {
            let version = "99";
            to reject_version { have(status) equal(400) }
            to advertise_supported_versions { have(versions) equal(Some("13, 8, 7".to_owned())) }
        }
        when offered_protocol_is_valid { let extra = "Sec-WebSocket-Protocol: json\r\n"; to accept_offer { have(status) equal(101) } }
        when offered_protocol_contains_whitespace { let extra = "Sec-WebSocket-Protocol: invalid protocol\r\n"; to reject_offer { have(status) equal(400) } }
    }
}

#[derive(Debug, PartialEq)]
enum FrameObservation {
    Binary(Vec<u8>),
    Pong(Vec<u8>),
    Close(Option<u16>),
    Eof,
    Other(String),
}

fn observe(frame: ws::Frame) -> FrameObservation {
    match frame {
        ws::Frame::Binary(bytes) => FrameObservation::Binary(bytes.to_vec()),
        ws::Frame::Pong(bytes) => FrameObservation::Pong(bytes.to_vec()),
        ws::Frame::Close(reason) => FrameObservation::Close(reason.map(|r| r.code.into())),
        other => FrameObservation::Other(format!("{other:?}")),
    }
}

async fn echo_upgrade(req: web::HttpRequest, payload: web::types::Payload) -> web::HttpResponse {
    let req = NtexRequest::from((req, payload.into_inner()));
    let (mut input, mut output, response) = <NtexRequest as server_fn::request::Req<
        ServerFnError,
        ServerFnError,
        ServerFnError,
    >>::try_into_websocket(req)
    .await
    .unwrap();
    ntex::rt::spawn(async move {
        while let Some(Ok(bytes)) = input.next().await {
            if output.send(bytes).await.is_err() {
                break;
            }
        }
    });
    response.take()
}

async fn frame_exchange(frames: Vec<u8>) -> FrameObservation {
    let server = test::server(async || App::new().route("/ws", web::get().to(echo_upgrade))).await;
    let connection = server.ws_at("/ws").await.unwrap();
    let (io, codec, _) = connection.into_inner();
    io.encode_slice(&frames).unwrap();
    io.flush(true).await.unwrap();
    let received = ntex::time::timeout(ntex::time::Millis(1000), io.recv(&codec))
        .await
        .expect("local protocol response")
        .expect("decoded response");
    io.terminate();
    received.map(observe).unwrap_or(FrameObservation::Eof)
}

fn frame(first: u8, payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() <= 125, "short-frame fixture");
    let mut bytes = vec![first, 0x80 | payload.len() as u8, 0, 0, 0, 0];
    bytes.extend_from_slice(payload);
    bytes
}
fn joined(first: Vec<u8>, second: Vec<u8>) -> Vec<u8> {
    [first, second].concat()
}
fn close(code: u16) -> FrameObservation {
    FrameObservation::Close(Some(code))
}

lets_expect! {
    expect(run_ntex(frame_exchange(bytes))) as complete_frame_contract {
        let bytes = frame(0x82, "hello".as_bytes());
        to echo_binary { equal(FrameObservation::Binary("hello".as_bytes().to_vec())) }
        when reserved_bit_is_set { let bytes = frame(0xc2, "hello".as_bytes()); to close_protocol_error { equal(close(1002)) } }
        when client_mask_is_missing { let bytes = vec![0x82, 0]; to close_protocol_error { equal(close(1002)) } }
        when opcode_is_unknown { let bytes = frame(0x83, &[]); to close_protocol_error { equal(close(1002)) } }
        when length_is_not_minimal { let bytes = vec![0x82, 0xfe, 0, 0, 0, 0, 0, 0]; to close_protocol_error { equal(close(1002)) } }
    }
    expect(run_ntex(frame_exchange(bytes))) as control_frame_contract {
        let bytes = frame(0x89, "ping".as_bytes());
        to preserve_pong_payload { equal(FrameObservation::Pong("ping".as_bytes().to_vec())) }
        when ping_is_fragmented { let bytes = frame(0x09, "ping".as_bytes()); to close_protocol_error { equal(close(1002)) } }
        when control_is_close {
            let bytes = frame(0x88, &[3, 232]);
            to echo_normal_close { equal(close(1000)) }
            when code_is_incomplete { let bytes = frame(0x88, &[3]); to close_protocol_error { equal(close(1002)) } }
            when code_is_local_only { let bytes = frame(0x88, &[3, 238]); to close_protocol_error { equal(close(1002)) } }
            when description_is_invalid_utf8 { let bytes = frame(0x88, &[3, 232, 255]); to close_invalid_payload { equal(close(1007)) } }
        }
    }
    expect(run_ntex(frame_exchange(bytes))) as fragment_transition_contract {
        let bytes = joined(frame(0x02, "hel".as_bytes()), frame(0x80, "lo".as_bytes()));
        to echo_reassembled_binary { equal(FrameObservation::Binary("hello".as_bytes().to_vec())) }
        when data_interrupts_the_fragment { let bytes = joined(frame(0x02, "hel".as_bytes()), frame(0x82, "lo".as_bytes())); to close_protocol_error { equal(close(1002)) } }
        when final_has_no_opening_fragment { let bytes = frame(0x80, "lo".as_bytes()); to close_protocol_error { equal(close(1002)) } }
    }
}

async fn large_message(size: usize) -> FrameObservation {
    let server = test::server(async || {
        App::new()
            .state(LeptosServerFnConfig::new().with_payload_limit(131072))
            .route("/ws", web::get().to(echo_upgrade))
    })
    .await;
    let mut builder = ws::WsClient::builder(server.url("/ws"));
    builder.address(server.addr()).max_frame_size(262144);
    let client = builder
        .build(ntex::SharedCfg::new("message-limit-spec"))
        .await
        .unwrap();
    let connection = client.connect().await.unwrap().seal();
    let sink = connection.sink();
    let receiver = connection.receiver();
    sink.send(ws::Message::Binary(vec![65; size].into()))
        .await
        .unwrap();
    let response = ntex::time::timeout(ntex::time::Millis(1000), receiver.recv())
        .await
        .expect("local size response");
    sink.io().terminate();
    match response {
        Some(Ok(frame)) => observe(frame),
        None => FrameObservation::Eof,
        Some(Err(error)) => FrameObservation::Other(format!("{error:?}")),
    }
}

lets_expect! {
    expect(run_ntex(large_message(size))) as configured_frame_size {
        let size = 16;
        to preserve_payload { equal(FrameObservation::Binary(vec![65; 16])) }
        when size_exceeds_old_codec_default { let size = 65537; to preserve_payload { equal(FrameObservation::Binary(vec![65; 65537])) } }
        when size_equals_configured_limit { let size = 131072; to preserve_payload { equal(FrameObservation::Binary(vec![65; 131072])) } }
        when size_exceeds_configured_limit { let size = 131073; to close_message_too_big { equal(close(1009)) } }
    }
}

#[leptos::server(name = MirrorResponse, prefix = "/response-contract", endpoint = "mirror", input = server_fn::codec::StreamingText, output = server_fn::codec::StreamingText, server = crate::NtexServerFnBackend)]
async fn mirror_response(
    input: server_fn::codec::TextStream,
) -> Result<server_fn::codec::TextStream, ServerFnError> {
    Ok(input)
}

// A complete chunked request avoids Content-Length preflight. The draining
// function consumes it before returning; MirrorResponse consumes it only when
// ntex polls the already returned response body.
#[derive(Debug)]
struct InputResponseObservation {
    status_lines: usize,
    status: Option<u16>,
    body: String,
    complete: bool,
}

async fn input_response(path: &'static str, input: &'static str) -> InputResponseObservation {
    register_explicit::<DrainStreamingInput>();
    register_explicit::<MirrorResponse>();
    let server = test::server(async || {
        App::new()
            .state(LeptosServerFnConfig::new().with_payload_limit(4))
            .route("/api/{tail}*", handle_server_fns())
            .route("/response-contract/{tail}*", handle_server_fns())
    })
    .await;
    let address = server.addr();
    let bytes = ntex::rt::spawn_blocking(move || {
        let mut socket = std::net::TcpStream::connect(address).unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{input}\r\n0\r\n\r\n",
            input.len()
        );
        socket.write_all(request.as_bytes()).unwrap();
        let mut bytes = Vec::new();
        // A successful response ends at its HTTP framing boundary. A failed
        // response must end in EOF: a timeout is never accepted as termination.
        loop {
            let mut byte = [0];
            if socket.read(&mut byte).expect("response bytes or peer termination") == 0 {
                break;
            }
            bytes.push(byte[0]);
            assert!(bytes.len() < 8192, "bounded fixture response");
            if framed_response_complete(&bytes) {
                break;
            }
        }
        String::from_utf8(bytes).unwrap()
    })
    .await
    .unwrap();
    let (status, body) = if bytes.is_empty() {
        (None, String::new())
    } else {
        let (head, body) = bytes
            .split_once("\r\n\r\n")
            .expect("complete response headers");
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        (Some(status), body.to_owned())
    };
    InputResponseObservation {
        status_lines: bytes.matches("HTTP/1.1 ").count(),
        status,
        body,
        complete: framed_response_complete(bytes.as_bytes()),
    }
}

fn framed_response_complete(bytes: &[u8]) -> bool {
    let Some(boundary) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
        return false;
    };
    let head = std::str::from_utf8(&bytes[..boundary]).unwrap();
    let body = &bytes[boundary + 4..];
    for line in head.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            return body.len() == value.trim().parse::<usize>().unwrap();
        }
        if name.eq_ignore_ascii_case("transfer-encoding") && value.trim() == "chunked" {
            return body.ends_with(b"0\r\n\r\n");
        }
    }
    false
}

fn terminate_without_replacing_response(actual: &InputResponseObservation) -> AssertionResult {
    // ntex may not flush even the original headers before an immediately ready
    // first body error terminates HTTP/1. If headers did reach the client, they
    // must still describe the original 200; neither case may forward input or
    // publish a successful chunk terminator.
    let expected_status_lines = usize::from(actual.status.is_some());
    if matches!(actual.status, None | Some(200))
        && actual.status_lines == expected_status_lines
        && actual.body.is_empty()
        && !actual.complete
    {
        Ok(())
    } else {
        Err(AssertionError::new(vec![format!(
            "expected EOF with no response or only the original 200 headers, got {actual:?}"
        )]))
    }
}

lets_expect! {
    expect(run_ntex(input_response(DrainStreamingInput::PATH, input))) as streaming_input_before_response {
        let input = "abcd";
        to returns_the_complete_success_response {
            have(status_lines) equal(1),
            have(complete) equal(true),
            have(status) equal(Some(200)),
            have(body) equal("4".to_owned()),
        }
        when input_exceeds_limit {
            let input = "abcde";
            to returns_the_complete_payload_too_large_response {
                have(status_lines) equal(1),
                have(complete) equal(true),
                have(status) equal(Some(413)),
                have(body) equal("payload exceeds limit of 4 bytes".to_owned()),
            }
        }
    }
    expect(run_ntex(input_response(MirrorResponse::PATH, input))) as streaming_input_in_first_response_poll {
        let input = "abcd";
        to returns_the_complete_success_response {
            have(status_lines) equal(1),
            have(complete) equal(true),
            have(status) equal(Some(200)),
            have(body) equal("4\r\nabcd\r\n0\r\n\r\n".to_owned()),
        }
        when input_exceeds_limit {
            let input = "abcde";
            to terminates_the_stream_without_replacing_the_response {
                terminate_without_replacing_response
            }
        }
    }
}

#[derive(Debug, PartialEq)]
struct ResponseObservation {
    status_lines: usize,
    original_status: bool,
    first_chunk: bool,
    successful_end: bool,
}

async fn streamed_response(overflows: bool) -> ResponseObservation {
    register_explicit::<MirrorResponse>();
    let server = test::server(async || {
        App::new()
            .state(LeptosServerFnConfig::new().with_payload_limit(4))
            .route("/response-contract/{tail}*", handle_server_fns())
    })
    .await;
    let address = server.addr();
    let bytes = ntex::rt::spawn_blocking(move || {
        let mut socket = std::net::TcpStream::connect(address).unwrap();
        socket.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        socket.write_all(b"POST /response-contract/mirror HTTP/1.1\r\nHost: localhost\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\nabcd\r\n").unwrap();
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"4\r\nabcd\r\n") {
            let mut byte = [0];
            socket.read_exact(&mut byte).expect("first response chunk received before releasing remaining input");
            bytes.push(byte[0]);
            assert!(bytes.len() < 8192, "bounded first response");
        }
        if overflows { socket.write_all(b"4\r\nefgh\r\n0\r\n\r\n").unwrap(); }
        else { socket.write_all(b"0\r\n\r\n").unwrap(); }
        if overflows {
            socket.read_to_end(&mut bytes).expect("peer closes failed response");
        } else {
            while !bytes.ends_with(b"0\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).expect("successful HTTP terminal chunk");
                bytes.push(byte[0]);
            }
        }
        String::from_utf8(bytes).unwrap()
    }).await.unwrap();
    ResponseObservation {
        status_lines: bytes.matches("HTTP/1.1 ").count(),
        original_status: bytes.starts_with(&format!("HTTP/1.1 {}", StatusCode::OK.as_str())),
        first_chunk: bytes.contains("4\r\nabcd\r\n"),
        successful_end: bytes.ends_with("0\r\n\r\n"),
    }
}

lets_expect! {
    expect(run_ntex(streamed_response(overflows))) as response_stream_contract {
        let overflows = false;
        to complete_the_original_response { equal(ResponseObservation { status_lines: 1, original_status: true, first_chunk: true, successful_end: true }) }
        when input_fails_after_wire_commit {
            let overflows = true;
            to abort_the_original_response { equal(ResponseObservation { status_lines: 1, original_status: true, first_chunk: true, successful_end: false }) }
        }
    }
}
