use super::*;
use crate::{handle_server_fns, register_explicit};
use lets_expect::*;
use ntex::web::{App as NtexApp, test, ws};
use ntex::ws::Item;
use server_fn::ServerFn;

fn encoded(value: &str) -> Vec<u8> {
    let mut bytes = vec![0];
    bytes.extend(serde_json::to_vec(value).unwrap());
    bytes
}
fn binary(value: &str) -> ws::Message {
    ws::Message::Binary(encoded(value).into())
}
fn text(value: &str) -> ws::Message {
    ws::Message::Text(String::from_utf8(encoded(value)).unwrap().into())
}
fn first(bytes: Vec<u8>, is_text: bool) -> ws::Message {
    ws::Message::Continuation(if is_text {
        Item::FirstText(bytes.into())
    } else {
        Item::FirstBinary(bytes.into())
    })
}
fn last(bytes: Vec<u8>) -> ws::Message {
    ws::Message::Continuation(Item::Last(bytes.into()))
}
fn middle(bytes: Vec<u8>) -> ws::Message {
    ws::Message::Continuation(Item::Continue(bytes.into()))
}

fn one(a: ws::Message) -> Vec<ws::Message> {
    vec![a]
}
fn two(a: ws::Message, b: ws::Message) -> Vec<ws::Message> {
    vec![a, b]
}
fn three(a: ws::Message, b: ws::Message, c: ws::Message) -> Vec<ws::Message> {
    vec![a, b, c]
}

#[derive(Debug, PartialEq)]
enum Observation {
    Data(Result<String, String>),
    Pong(Vec<u8>),
    Close(Option<(u16, Option<String>)>),
    Other(String),
}
fn observe(frame: ws::Frame) -> Observation {
    match frame {
        ws::Frame::Binary(bytes) => Observation::Data(if bytes.first() == Some(&0) {
            serde_json::from_slice(&bytes[1..]).map_err(|error| error.to_string())
        } else {
            Err(format!("missing success marker: {bytes:?}"))
        }),
        ws::Frame::Pong(bytes) => Observation::Pong(bytes.to_vec()),
        ws::Frame::Close(reason) => {
            Observation::Close(reason.map(|reason| (reason.code.into(), reason.description)))
        }
        other => Observation::Other(format!("{other:?}")),
    }
}
async fn receive<E: std::fmt::Debug>(
    rx: &ntex::channel::mpsc::Receiver<Result<ws::Frame, E>>,
) -> Observation {
    let frame = ntex::time::timeout(ntex::time::Millis(1000), rx.recv())
        .await
        .expect("local peer makes progress")
        .expect("local peer remains connected")
        .expect("valid server frame");
    observe(frame)
}
async fn exchange(frames: Vec<ws::Message>, limit: usize) -> Observation {
    register_explicit::<EchoWebsocket>();
    let server = test::server(move || async move {
        NtexApp::new()
            .state(crate::LeptosServerFnConfig::new().with_payload_limit(limit))
            .route("/api/{tail}*", handle_server_fns())
    })
    .await;
    let connection = server.ws_at(EchoWebsocket::PATH).await.unwrap();
    let sink = connection.sink();
    let receiver = connection.receiver();
    for frame in frames {
        sink.send(frame).await.unwrap();
    }
    let observed = receive(&receiver).await;
    if !matches!(observed, Observation::Close(_)) {
        let _ = sink.send(ws::Message::Close(None)).await;
    }
    observed
}
fn echo(expected: impl Into<String>) -> impl Fn(&Observation) -> AssertionResult {
    equal(Observation::Data(Ok(expected.into())))
}
fn close_with(expected: u16) -> impl Fn(&Observation) -> AssertionResult {
    move |observed| {
        if matches!(observed, Observation::Close(Some((code, _))) if *code == expected) {
            Ok(())
        } else {
            Err(AssertionError::new(vec![format!(
                "expected Close({expected}), got {observed:?}"
            )]))
        }
    }
}
async fn selected_protocol(
    configured: Option<&'static str>,
    offers: Vec<&'static str>,
) -> Option<String> {
    register_explicit::<EchoWebsocket>();
    let server = test::server(move || async move {
        NtexApp::new()
            .state(crate::LeptosServerFnConfig {
                ws_subprotocol: configured,
                ..Default::default()
            })
            .route("/api/{tail}*", handle_server_fns())
    })
    .await;
    let mut builder = ntex::ws::WsClient::builder(server.url(EchoWebsocket::PATH));
    builder
        .address(server.addr())
        .timeout(ntex::time::Seconds(2));
    if !offers.is_empty() {
        builder.protocols(offers);
    }
    let client = builder
        .build(ntex::SharedCfg::new("protocol-spec"))
        .await
        .unwrap();
    let connection = client.connect().await.unwrap();
    let selected = connection
        .response()
        .headers()
        .get(ntex::http::header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    connection
        .sink()
        .send(ws::Message::Close(None))
        .await
        .unwrap();
    selected
}

lets_expect! {
    expect(run_ntex(exchange(frames, 16))) as complete_message {
        let frames = one(binary("hello"));
        to deliver_binary { echo("hello") }
        when size_equals_limit { let frames = one(binary("aaaaaaaaaaaaa")); to deliver_inclusive_limit { echo("aaaaaaaaaaaaa") } }
        when size_exceeds_limit { let frames = one(ws::Message::Binary(vec![b'X';64].into())); to close_message_too_big { close_with(1009) } }
        when message_is_text {
            let frames = one(text("hello"));
            to deliver_text { echo("hello") }
            when size_equals_limit { let frames = one(text("aaaaaaaaaaaaa")); to deliver_inclusive_limit { echo("aaaaaaaaaaaaa") } }
            when size_exceeds_limit { let frames = one(ws::Message::Text("X".repeat(64).into())); to close_message_too_big { close_with(1009) } }
        }
    }
    expect(run_ntex(exchange(frames, 16))) as fragmented_message {
        let is_text = false;
        let frames = three(first(encoded("hello")[..3].to_vec(), is_text), middle(encoded("hello")[3..5].to_vec()), last(encoded("hello")[5..].to_vec()));
        to reassemble_binary { echo("hello") }
        when opening_fragment_equals_limit {
            let frames = two(first(encoded("aaaaaaaaaaaaa"), is_text), last(Vec::new()));
            to accept_empty_terminal_fragment { echo("aaaaaaaaaaaaa") }
        }
        when continuation_reaches_limit {
            let frames = three(first(encoded("aaaaaaaaaaaaa")[..8].to_vec(), is_text), middle(encoded("aaaaaaaaaaaaa")[8..].to_vec()), last(Vec::new()));
            to accept_inclusive_total { echo("aaaaaaaaaaaaa") }
        }
        when opening_fragment_exceeds_limit {
            let frames = one(first(vec![b'A';64], is_text));
            to close_message_too_big { close_with(1009) }
        }
        when continuation_exceeds_limit {
            let frames = two(first(vec![b'A';8], is_text), middle(vec![b'B';12]));
            to close_message_too_big { close_with(1009) }
        }
        when terminal_fragment_exceeds_limit {
            let frames = three(first(vec![b'A';8], is_text), middle(vec![b'B';4]), last(vec![b'C';8]));
            to close_message_too_big { close_with(1009) }
        }
        when message_is_text {
            let is_text = true;
            to reassemble_text { echo("hello") }
            when opening_fragment_equals_limit {
                let frames = two(first(encoded("aaaaaaaaaaaaa"), is_text), last(Vec::new()));
                to accept_empty_terminal_fragment { echo("aaaaaaaaaaaaa") }
            }
            when opening_fragment_exceeds_limit {
                let frames = one(first(vec![b'A';64], is_text));
                to close_message_too_big { close_with(1009) }
            }
            when continuation_reaches_limit {
                    let frames = three(first(encoded("aaaaaaaaaaaaa")[..8].to_vec(), is_text), middle(encoded("aaaaaaaaaaaaa")[8..].to_vec()), last(Vec::new()));
                to accept_inclusive_total { echo("aaaaaaaaaaaaa") }
            }
            when continuation_exceeds_limit {
                let frames = two(first(vec![b'A';8], is_text), middle(vec![b'B';12]));
                to close_message_too_big { close_with(1009) }
            }
            when terminal_fragment_exceeds_limit {
                let frames = three(first(vec![b'A';8], is_text), middle(vec![b'B';4]), last(vec![b'C';8]));
                to close_message_too_big { close_with(1009) }
            }
        }
    }
    expect(run_ntex(exchange(frames, 4))) as assembled_text_validity {
        let frames = two(first(vec![0, 34], true), last(vec![34]));
        to deliver_valid_text { echo("") }
        when text_is_invalid {
            let frames = two(first(vec![0xff], true), last(vec![0xfe]));
            to close_invalid_text { close_with(1007) }
            when total_also_exceeds_limit {
                let frames = two(first(vec![97,98], true), last(vec![0xff,0xfe,99,100]));
                to enforce_size_before_text_decoding { close_with(1009) }
            }
        }
    }
    expect(run_ntex(exchange(vec![ws::Message::Ping(ntex::util::Bytes::from_static("ping-data".as_bytes()))], 1024))) as ping_response {
        to preserve_pong_payload { equal(Observation::Pong("ping-data".as_bytes().to_vec())) }
    }
    expect(run_ntex(exchange(vec![ws::Message::Close(Some(ws::CloseReason { code: ws::CloseCode::Normal, description: Some("bye".into()) }))], 1024))) as client_close {
        to echo_code_and_description { equal(Observation::Close(Some((1000,Some("bye".into()))))) }
    }
    expect(run_ntex(selected_protocol(configured, offers))) as subprotocol_selection {
        let configured = Some("graphql-ws");
        let offers = vec!["graphql-ws"];
        to select_matching_protocol { equal(Some("graphql-ws".to_owned())) }
        when matching_offer_is_not_first { let offers = vec!["other-ws","graphql-ws"]; to select_matching_protocol { equal(Some("graphql-ws".to_owned())) } }
        when offers_are_missing { let offers = Vec::new(); to select_no_protocol { equal(None) } }
        when offers_differ { let offers = vec!["other-ws"]; to select_no_protocol { equal(None) } }
        when configured_protocol_is_absent {
            let configured = None;
            to select_no_protocol { equal(None) }
            when offers_are_missing { let offers = Vec::new(); to select_no_protocol { equal(None) } }
        }
    }
}
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
// These exercise delivery with a small channel and a paced producer. The client
// drains network frames internally, so whole-chain bounds belong to flow_tests.
fn expected_sequence(count: usize) -> Vec<Observation> {
    (0..count)
        .map(|i| Observation::Data(Ok(format!("message-{i}"))))
        .collect()
}
async fn ordered_delivery(paced: bool) -> Vec<Observation> {
    register_explicit::<EchoWebsocket>();
    register_explicit::<SlowEchoWebsocket>();
    let server = test::server(async || {
        NtexApp::new()
            .state(crate::LeptosServerFnConfig::new().with_ws_channel_buffer(1))
            .route("/api/{tail}*", handle_server_fns())
    })
    .await;
    let path = if paced {
        SlowEchoWebsocket::PATH
    } else {
        EchoWebsocket::PATH
    };
    let connection = server.ws_at(path).await.unwrap();
    let sink = connection.sink();
    let receiver = connection.receiver();
    let count = if paced { 5 } else { 20 };
    for index in 0..count {
        sink.send(binary(&format!("message-{index}")))
            .await
            .unwrap();
    }
    let mut received = Vec::new();
    for _ in 0..count {
        received.push(receive(&receiver).await);
    }
    sink.send(ws::Message::Close(None)).await.unwrap();
    received
}
async fn finite_output() -> Vec<Observation> {
    register_explicit::<FiniteEchoWebsocket>();
    let server =
        test::server(async || NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;
    let connection = server.ws_at(FiniteEchoWebsocket::PATH).await.unwrap();
    let sink = connection.sink();
    let receiver = connection.receiver();
    let mut received = Vec::new();
    for value in ["one", "two"] {
        sink.send(binary(value)).await.unwrap();
        received.push(receive(&receiver).await);
    }
    received.push(receive(&receiver).await);
    received
}
lets_expect! {
    expect(run_ntex(ordered_delivery(paced))) as ordered_delivery {
        let paced = false;
        to deliver_entire_burst { equal(expected_sequence(20)) }
        when producer_is_paced {
            let paced = true;
            to deliver_entire_sequence { equal(expected_sequence(5)) }
        }
    }
    expect(run_ntex(finite_output())) as output_completion {
        to close_after_final_message { equal(vec![Observation::Data(Ok("one".into())),Observation::Data(Ok("two".into())),Observation::Close(None)]) }
    }
}
