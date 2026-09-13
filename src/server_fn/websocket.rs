//! Connection ownership and RFC 6455 checks missing from ntex's WS convenience API.

use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::{Bytes, BytesMut};
use futures::{
    FutureExt, Sink, Stream,
    channel::{mpsc, oneshot},
    future::{BoxFuture, Shared, poll_fn},
};
use ntex::{
    codec::Decoder,
    http::{self, StatusCode, header},
    io::{IoBoxed, RecvError},
    util::{Bytes as NBytes, BytesMut as NBytesMut},
    web::{HttpRequest, HttpResponse},
    ws::{self, CloseCode, CloseReason, Frame, Item, Message},
};
use or_poisoned::OrPoisoned;
use server_fn::error::{FromServerFnError, ServerFnErrorErr};
use std::{
    cell::Cell,
    collections::HashSet,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::Poll,
};

use crate::{config::server_fn_config, server_fn::NtexServerResponse};

/// Shared only within one dispatch. The pump takes the sender after upgrade.
#[derive(Clone)]
pub(crate) struct ConnectionScope {
    sender: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    pub(crate) cancelled: Shared<BoxFuture<'static, ()>>,
}

impl ConnectionScope {
    pub(crate) fn new() -> Self {
        let (sender, receiver) = oneshot::channel();
        Self {
            sender: Arc::new(Mutex::new(Some(sender))),
            cancelled: async {
                let _ = receiver.await;
            }
            .boxed()
            .shared(),
        }
    }

    fn take_sender(&self) -> Option<oneshot::Sender<()>> {
        self.sender.lock().or_poisoned().take()
    }
}

#[derive(Clone, Copy)]
pub(crate) struct HandshakeFailure(pub(crate) ws::error::HandshakeError);

fn has_token(req: &HttpRequest, name: header::HeaderName, token: &str) -> bool {
    req.headers().get_all(name).any(|value| {
        value.to_str().is_ok_and(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case(token))
        })
    })
}

fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// Validates the RFC 6455 handshake and returns the client's subprotocol offers.
fn verify_handshake(req: &HttpRequest) -> Result<HashSet<&str>, ws::error::HandshakeError> {
    use ws::error::HandshakeError as E;
    if req.method() != http::Method::GET {
        return Err(E::GetMethodRequired);
    }
    if req.version() != http::Version::HTTP_11 || !has_token(req, header::UPGRADE, "websocket") {
        return Err(E::NoWebsocketUpgrade);
    }
    if !has_token(req, header::CONNECTION, "upgrade") {
        return Err(E::NoConnectionUpgrade);
    }
    let mut versions = req.headers().get_all(header::SEC_WEBSOCKET_VERSION);
    let version = versions.next().ok_or(E::NoVersionHeader)?;
    if versions.next().is_some() || !matches!(version.as_bytes(), b"13" | b"8" | b"7") {
        return Err(E::UnsupportedVersion);
    }
    let mut keys = req.headers().get_all(header::SEC_WEBSOCKET_KEY);
    let key = keys.next().ok_or(E::BadWebsocketKey)?;
    let mut nonce = [0; 16];
    if keys.next().is_some() || STANDARD.decode_slice(key.as_bytes(), &mut nonce) != Ok(16) {
        return Err(E::BadWebsocketKey);
    }
    let mut fields = req
        .headers()
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .peekable();
    let has_protocols = fields.peek().is_some();
    let mut protocols = HashSet::new();
    for value in fields {
        let value = value.to_str().map_err(|_| E::NoWebsocketUpgrade)?;
        for protocol in value.split(',') {
            let protocol = protocol.trim_matches([' ', '\t']);
            // RFC 9110 §5.6.1.2: empty list elements do not count as offers.
            if protocol.is_empty() {
                continue;
            }
            // RFC 6455 §4.1/§11.3.4: unique tokens across the combined list.
            if !is_token(protocol) || !protocols.insert(protocol) {
                return Err(E::NoWebsocketUpgrade);
            }
        }
    }
    // RFC 6455 §4.3: a present offer is 1#token, including across fields.
    if has_protocols && protocols.is_empty() {
        return Err(E::NoWebsocketUpgrade);
    }
    Ok(protocols)
}

pub(crate) fn failure_response(error: ws::error::HandshakeError) -> HttpResponse {
    use ntex::http::error::ResponseError;
    let mut response = error.error_response();
    if matches!(error, ws::error::HandshakeError::UnsupportedVersion) {
        response.headers_mut().insert(
            header::SEC_WEBSOCKET_VERSION,
            header::HeaderValue::from_static("13, 8, 7"),
        );
    }
    response
}

type UpgradedConnection = (
    mpsc::Receiver<Result<Bytes, Bytes>>,
    mpsc::Sender<Bytes>,
    NtexServerResponse,
);

pub(crate) async fn upgrade<E, I>(request: HttpRequest) -> Result<UpgradedConnection, E>
where
    E: FromServerFnError,
    I: FromServerFnError + 'static,
{
    let result = start::<I>(&request);
    result.map_err(|err| {
        request.extensions_mut().insert(HandshakeFailure(err));
        E::from_server_fn_error(ServerFnErrorErr::Request(err.to_string()))
    })
}

fn start<I>(request: &HttpRequest) -> Result<UpgradedConnection, ws::error::HandshakeError>
where
    I: FromServerFnError + 'static,
{
    let offered = verify_handshake(request)?;
    let config = server_fn_config(request);
    // ntex's HTTP/1 encoder treats 101 as a stream regardless of BodySize.
    // Disable chunking explicitly; removing the header alone is insufficient.
    let mut response = ws::handshake_response(request.head())
        .no_chunking()
        .finish();
    response.headers_mut().remove(header::TRANSFER_ENCODING);
    // Only validated offers can be selected; an invalid configured value never
    // matches a token the client offered.
    if let Some(protocol) = config
        .ws_subprotocol
        .filter(|protocol| offered.contains(protocol))
    {
        // The configured value has already passed the token grammar.
        if let Ok(protocol) = header::HeaderValue::from_str(protocol) {
            response
                .headers_mut()
                .insert(header::SEC_WEBSOCKET_PROTOCOL, protocol);
        }
    }
    let (io, http_codec) = request
        .head()
        .take_io()
        .ok_or(ws::error::HandshakeError::NoWebsocketUpgrade)?;
    io.encode(
        http::h1::Message::Item((response.into_parts().0, http::body::BodySize::Empty)),
        &http_codec,
    )
    .map_err(|_| ws::error::HandshakeError::NoWebsocketUpgrade)?;
    io.stop_timer();
    let (input_tx, input_rx) = mpsc::channel(config.ws_channel_buffer);
    let (output_tx, output_rx) = mpsc::channel(config.ws_channel_buffer);
    let cancel =
        leptos::context::use_context::<ConnectionScope>().and_then(|scope| scope.take_sender());
    ntex::rt::spawn(async move {
        // Dropping a oneshot sender also signals cancellation on panic/unwind.
        let cancel = cancel;
        let reason = pump::<I>(&io, config.payload_limit, input_tx, output_rx).await;
        drop(cancel);
        if let Some(reason) = reason {
            let _ = io.encode(Message::Close(reason), &ws::Codec::new());
            // ntex's configured disconnect timeout bounds graceful flushing.
            io.close();
            io.on_disconnect().await;
        }
    });
    Ok((
        input_rx,
        output_tx,
        NtexServerResponse::from(HttpResponse::new(StatusCode::OK)),
    ))
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum FrameError {
    Protocol(&'static str),
    InvalidText,
    TooBig,
    Ntex(ws::error::ProtocolError),
}

impl FrameError {
    fn reason(self, limit: usize) -> CloseReason {
        let (code, description) = match self {
            Self::TooBig | Self::Ntex(ws::error::ProtocolError::Overflow) => (
                CloseCode::Size,
                format!("message exceeds limit of {limit} bytes"),
            ),
            Self::InvalidText => (
                CloseCode::Invalid,
                "text frame payload is not valid UTF-8".to_owned(),
            ),
            Self::Protocol(message) => (CloseCode::Protocol, message.to_owned()),
            Self::Ntex(error) => (CloseCode::Protocol, error.to_string()),
        };
        CloseReason {
            code,
            description: Some(description),
        }
    }
}

/// Validate fields that ntex's decoded Frame no longer represents.
pub(crate) struct CheckedCodec {
    codec: ws::Codec,
    fragmented: Cell<bool>,
}

impl CheckedCodec {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            codec: ws::Codec::new().max_size(limit.max(125)),
            fragmented: Cell::new(false),
        }
    }
}

impl Decoder for CheckedCodec {
    type Item = Frame;
    type Error = FrameError;

    fn decode(&self, bytes: &mut NBytesMut) -> Result<Option<Frame>, FrameError> {
        use FrameError::Protocol;
        if bytes.len() < 2 {
            return Ok(None);
        }
        let first = bytes[0];
        let opcode = first & 15;
        let finished = first & 128 != 0;
        let short_len = bytes[1] & 127;
        if first & 0x70 != 0 {
            return Err(Protocol("reserved frame bits without an extension"));
        }
        if opcode >= 8 && (!finished || short_len > 125) {
            return Err(Protocol("invalid control frame"));
        }
        if matches!(opcode, 1 | 2) && self.fragmented.get() {
            return Err(Protocol("data frame interrupts a fragmented message"));
        }
        let header_len = match short_len {
            126 => 4,
            127 => 10,
            _ => 2,
        };
        if bytes.len() < header_len {
            return Ok(None);
        }
        if short_len == 126 && u16::from_be_bytes([bytes[2], bytes[3]]) < 126 {
            return Err(Protocol("nonminimal frame length"));
        }
        if short_len == 127 {
            let length =
                u64::from_be_bytes(bytes[2..10].try_into().expect("header length checked"));
            if length < 65536 || length >> 63 != 0 {
                return Err(Protocol("invalid extended frame length"));
            }
        }
        if opcode == 8 {
            if short_len == 1 {
                return Err(Protocol("incomplete close code"));
            }
            let masked = bytes[1] & 128 != 0;
            let payload_start = header_len + usize::from(masked) * 4;
            let payload_len = usize::from(short_len);
            if bytes.len() < payload_start + payload_len {
                return Ok(None);
            }
            let mut payload = [0; 125];
            for (index, byte) in bytes[payload_start..payload_start + payload_len]
                .iter()
                .enumerate()
            {
                payload[index] = *byte
                    ^ if masked {
                        bytes[header_len + index % 4]
                    } else {
                        0
                    };
            }
            if payload_len >= 2 {
                let code = u16::from_be_bytes([payload[0], payload[1]]);
                // RFC 6455 / IANA: exclude local-only and unassigned protocol codes.
                if !matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999) {
                    return Err(Protocol("invalid close code"));
                }
                if std::str::from_utf8(&payload[2..payload_len]).is_err() {
                    return Err(FrameError::InvalidText);
                }
            }
        }
        let frame = self.codec.decode(bytes).map_err(FrameError::Ntex)?;
        if frame.is_some() {
            if matches!(opcode, 1 | 2) && !finished {
                self.fragmented.set(true);
            }
            if opcode == 0 && finished {
                self.fragmented.set(false);
            }
        }
        Ok(frame)
    }
}

#[derive(Default)]
struct Assembly(Option<(bool, BytesMut)>);

impl Assembly {
    fn data(&mut self, frame: Frame, limit: usize) -> Result<Option<Bytes>, FrameError> {
        let (text, bytes) = match frame {
            Frame::Binary(b) => (false, b),
            Frame::Text(b) => (true, b),
            _ => return Ok(None),
        };
        if bytes.len() > limit {
            return Err(FrameError::TooBig);
        }
        if text && std::str::from_utf8(&bytes).is_err() {
            return Err(FrameError::InvalidText);
        }
        // A decoded frame can share a much larger transport read allocation.
        // Retained application messages own only their validated payload bytes.
        Ok(Some(Bytes::copy_from_slice(&bytes)))
    }

    fn accept(&mut self, frame: Frame, limit: usize) -> Result<Option<Bytes>, FrameError> {
        match frame {
            Frame::Continuation(Item::FirstText(ref b) | Item::FirstBinary(ref b)) => {
                if self.0.is_some() {
                    return Err(FrameError::Protocol("fragment already started"));
                }
                if b.len() > limit {
                    return Err(FrameError::TooBig);
                }
                let text = matches!(frame, Frame::Continuation(Item::FirstText(_)));
                self.0 = Some((text, BytesMut::from(b.as_ref())));
                Ok(None)
            }
            Frame::Continuation(Item::Continue(ref b) | Item::Last(ref b)) => {
                let Some((_, bytes)) = self.0.as_mut() else {
                    return Err(FrameError::Protocol("fragment not started"));
                };
                if b.len() > limit.saturating_sub(bytes.len()) {
                    return Err(FrameError::TooBig);
                }
                bytes.extend_from_slice(b);
                if matches!(frame, Frame::Continuation(Item::Last(_))) {
                    let (text, bytes) = self.0.take().expect("fragment checked");
                    if text && std::str::from_utf8(&bytes).is_err() {
                        return Err(FrameError::InvalidText);
                    }
                    Ok(Some(bytes.freeze()))
                } else {
                    Ok(None)
                }
            }
            other => self.data(other, limit),
        }
    }
}

async fn pump<I>(
    io: &IoBoxed,
    limit: usize,
    mut input: mpsc::Sender<Result<Bytes, Bytes>>,
    mut output: mpsc::Receiver<Bytes>,
) -> Option<Option<CloseReason>>
where
    I: FromServerFnError,
{
    let codec = CheckedCodec::new(limit);
    let mut assembly = Assembly::default();
    let mut disconnected = io.on_disconnect();
    let mut input_open = true;
    poll_fn(|cx| {
        // One task owns all IO polling: Io uses one dispatch waker for read/flush.
        for _ in 0..32 {
            if Pin::new(&mut disconnected).poll(cx).is_ready() {
                return Poll::Ready(None);
            }
            let mut progress = false;
            match io.poll_flush(cx, false) {
                Poll::Ready(Err(_)) => return Poll::Ready(None),
                Poll::Ready(Ok(())) => match Pin::new(&mut output).poll_next(cx) {
                    Poll::Ready(Some(bytes)) => {
                        if io
                            .encode(
                                Message::Binary(NBytes::copy_from_slice(&bytes)),
                                &codec.codec,
                            )
                            .is_err()
                        {
                            return Poll::Ready(None);
                        }
                        progress = true;
                    }
                    Poll::Ready(None) => return Poll::Ready(Some(None)),
                    Poll::Pending => {}
                },
                Poll::Pending => {}
            }
            // A Pong also writes bytes, so decode only while output can progress.
            if matches!(io.poll_flush(cx, false), Poll::Ready(Ok(()))) {
                let ready = if input_open {
                    match Pin::new(&mut input).poll_ready(cx) {
                        Poll::Ready(Err(_)) => {
                            input_open = false;
                            Poll::Ready(Ok(()))
                        }
                        ready => ready,
                    }
                } else {
                    Poll::Ready(Ok(()))
                };
                match ready {
                    Poll::Ready(Ok(())) => match io.poll_recv(&codec, cx) {
                        Poll::Ready(Ok(Frame::Close(reason))) => return Poll::Ready(Some(reason)),
                        Poll::Ready(Ok(Frame::Ping(bytes))) => {
                            let _ = io.encode(Message::Pong(bytes), &codec.codec);
                            progress = true;
                        }
                        Poll::Ready(Ok(Frame::Pong(_))) => {
                            progress = true;
                        }
                        Poll::Ready(Ok(frame)) => {
                            progress = true;
                            match assembly.accept(frame, limit) {
                                Ok(Some(bytes)) => {
                                    if input_open
                                        && Pin::new(&mut input).start_send(Ok(bytes)).is_err()
                                    {
                                        input_open = false;
                                    }
                                }
                                Ok(None) => {}
                                Err(error) => {
                                    return Poll::Ready(Some(Some(report_error::<I>(
                                        &mut input, error, limit,
                                    ))));
                                }
                            }
                        }
                        Poll::Ready(Err(RecvError::Decoder(error))) => {
                            return Poll::Ready(Some(Some(report_error::<I>(
                                &mut input, error, limit,
                            ))));
                        }
                        Poll::Ready(Err(RecvError::WriteBackpressure)) | Poll::Pending => {}
                        Poll::Ready(Err(_)) => return Poll::Ready(None),
                    },
                    Poll::Ready(Err(_)) => unreachable!("sender failure handled above"),
                    Poll::Pending => {
                        io.pause();
                    }
                }
            }
            if !progress {
                return Poll::Pending;
            }
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    })
    .await
}

fn report_error<I>(
    input: &mut mpsc::Sender<Result<Bytes, Bytes>>,
    error: FrameError,
    limit: usize,
) -> CloseReason
where
    I: FromServerFnError,
{
    let reason = error.reason(limit);
    // Teardown must never wait for an application that stopped reading input.
    let _ = input.try_send(Err(I::from_server_fn_error(ServerFnErrorErr::Args(
        reason.description.clone().unwrap_or_default(),
    ))
    .ser()));
    reason
}

#[cfg(test)]
mod flow_tests {
    use super::*;
    use futures::SinkExt;
    use lets_expect::*;
    use ntex::{
        codec::Encoder,
        io::{Io, testing::IoTest},
    };
    use server_fn::error::ServerFnError;

    fn run<T: 'static>(future: impl Future<Output = T> + 'static) -> T {
        ntex::rt::System::build()
            .testing()
            .build(ntex::rt::DefaultRuntime)
            .block_on(future)
    }

    #[derive(Debug)]
    struct IncomingObservation {
        paused_messages: usize,
        delivered: Vec<u32>,
        cancelled: bool,
    }

    async fn incoming() -> IncomingObservation {
        let (client, server) = IoTest::create();
        let io = Io::new(server, ntex::SharedCfg::default()).boxed();
        let (input_tx, mut input_rx) = mpsc::channel(1);
        let (_output_tx, output_rx) = mpsc::channel(1);
        let mut connection = Box::pin(pump::<ServerFnError>(&io, 1024, input_tx, output_rx));
        let codec = ws::Codec::new().client_mode();
        let mut frames = ntex::util::BytePages::default();
        for index in 0..32u32 {
            codec
                .encodev(
                    Message::Binary(NBytes::copy_from_slice(&index.to_be_bytes())),
                    &mut frames,
                )
                .unwrap();
        }
        client.write(frames.freeze());
        // Readiness is observed after polling the real pump to its blocked state.
        let paused_messages = ntex::time::timeout(
            ntex::time::Millis(1000),
            poll_fn(|cx| {
                assert!(connection.as_mut().poll(cx).is_pending());
                let messages = input_rx.size_hint().0;
                if messages > 0 {
                    Poll::Ready(messages)
                } else {
                    Poll::Pending
                }
            }),
        )
        .await
        .expect("input reaches the paused consumer");
        let mut delivered = Vec::new();
        for _ in 0..32 {
            let bytes = ntex::time::timeout(
                ntex::time::Millis(1000),
                poll_fn(|cx| {
                    assert!(connection.as_mut().poll(cx).is_pending());
                    Pin::new(&mut input_rx).poll_next(cx)
                }),
            )
            .await
            .expect("input resumes")
            .unwrap()
            .unwrap();
            delivered.push(u32::from_be_bytes(bytes.as_ref().try_into().unwrap()));
        }
        io.terminate();
        let cancelled = connection.await.is_none();
        IncomingObservation {
            paused_messages,
            delivered,
            cancelled,
        }
    }

    lets_expect! {
        expect(run(incoming())) as incoming_flow {
            to bounds_a_paused_consumer_to_the_buffer_and_one_reservation { have(paused_messages) equal(2) }
            to delivers_after_resumption { have(delivered) equal((0..32).collect::<Vec<_>>()) }
            to finishes_on_disconnect { have(cancelled) be_true }
        }
    }

    #[derive(Debug)]
    struct OutgoingObservation {
        buffered: usize,
        sent_before_resume: usize,
        received: Vec<u32>,
        payloads_intact: bool,
    }

    async fn outgoing() -> OutgoingObservation {
        let (client, server) = IoTest::create();
        // Zero transport capacity is an explicit dependency outcome, not a delay.
        client.remote_buffer_cap(0);
        let io = Io::new(server, ntex::SharedCfg::default()).boxed();
        let (input_tx, _input_rx) = mpsc::channel(1);
        let (mut output_tx, output_rx) = mpsc::channel(1);
        let mut connection = Box::pin(pump::<ServerFnError>(&io, 1024, input_tx, output_rx));
        let sent = std::cell::Cell::new(0);
        let producer = async {
            for index in 0..512u32 {
                let mut bytes = vec![0; 16384];
                bytes[..4].copy_from_slice(&index.to_be_bytes());
                output_tx.send(Bytes::from(bytes)).await.unwrap();
                sent.set(sent.get() + 1);
            }
        };
        futures::pin_mut!(producer);
        let mut producer_done = false;
        let buffered = ntex::time::timeout(
            ntex::time::Millis(1000),
            poll_fn(|cx| {
                if !producer_done {
                    producer_done = producer.as_mut().poll(cx).is_ready();
                }
                assert!(connection.as_mut().poll(cx).is_pending());
                if io.is_wr_backpressure() {
                    Poll::Ready(io.with_write_buf(|buffer| buffer.len()).unwrap())
                } else {
                    Poll::Pending
                }
            }),
        )
        .await
        .expect("transport backpressure reached");
        let sent_before_resume = sent.get();
        client.remote_buffer_cap(65536);
        let mut bytes = NBytesMut::new();
        let codec = ws::Codec::new().client_mode();
        let mut received = Vec::new();
        let mut payloads_intact = true;
        while received.len() < 512 {
            let chunk = ntex::time::timeout(
                ntex::time::Millis(1000),
                poll_fn(|cx| {
                    if !producer_done {
                        producer_done = producer.as_mut().poll(cx).is_ready();
                    }
                    assert!(connection.as_mut().poll(cx).is_pending());
                    let chunk = client.read_any();
                    if chunk.is_empty() {
                        let read = client.read();
                        futures::pin_mut!(read);
                        read.poll(cx)
                    } else {
                        Poll::Ready(Ok(chunk))
                    }
                }),
            )
            .await
            .expect("output resumes")
            .unwrap();
            bytes.extend_from_slice(&chunk);
            while let Some(frame) = codec.decode(&mut bytes).unwrap() {
                let Frame::Binary(payload) = frame else {
                    panic!("unexpected output: {frame:?}");
                };
                payloads_intact &= payload.len() == 16384
                    && payload
                        .get(4..)
                        .is_some_and(|suffix| suffix.iter().all(|byte| *byte == 0));
                received.push(
                    payload
                        .get(..4)
                        .map(|prefix| u32::from_be_bytes(prefix.try_into().unwrap()))
                        .unwrap_or(u32::MAX),
                );
            }
            client.remote_buffer_cap(65536);
        }
        io.terminate();
        let _ = connection.await;
        OutgoingObservation {
            buffered,
            sent_before_resume,
            received,
            payloads_intact,
        }
    }

    fn bounded_encoded_bytes(result: &OutgoingObservation) -> AssertionResult {
        if result.buffered <= 32768 {
            Ok(())
        } else {
            Err(AssertionError::new(vec![format!(
                "expected at most 32768 encoded bytes, got {}",
                result.buffered
            )]))
        }
    }
    fn bounded_production(result: &OutgoingObservation) -> AssertionResult {
        if result.sent_before_resume < 512 {
            Ok(())
        } else {
            Err(AssertionError::new(vec![format!(
                "producer completed {} messages while transport was paused",
                result.sent_before_resume
            )]))
        }
    }
    lets_expect! {
        expect(run(outgoing())) as outgoing_flow {
            to bounds_encoded_bytes_when_the_peer_stops_reading { bounded_encoded_bytes }
            to stops_the_upstream_producer { bounded_production }
            to preserves_the_sequence_after_reading_resumes { have(received) equal((0..512).collect::<Vec<_>>()) }
            to preserves_payload_bytes_after_reading_resumes { have(payloads_intact) be_true }
        }
    }
}

#[cfg(test)]
#[path = "websocket_specs.rs"]
mod specs;
