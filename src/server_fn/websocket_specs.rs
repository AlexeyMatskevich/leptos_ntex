use super::*;
use lets_expect::*;
use ntex::web::test::TestRequest;

fn protocol_request(fields: &[&[u8]]) -> HttpRequest {
    let mut request = TestRequest::default()
        .header(header::UPGRADE, "websocket")
        .header(header::CONNECTION, "upgrade")
        .header(header::SEC_WEBSOCKET_VERSION, "13")
        .header(header::SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ==");
    for field in fields {
        request = request.header(
            header::SEC_WEBSOCKET_PROTOCOL,
            header::HeaderValue::from_bytes(field).unwrap(),
        );
    }
    request.to_http_request()
}

lets_expect! {
    expect(verify_handshake(&protocol_request(fields)).map(|offered| offered.len())) as offered_subprotocol_list {
        let fields: &[&[u8]] = &[b"chat"];
        to accepts_unique_protocols { be_ok }
        when offers_are_absent {
            let fields: &[&[u8]] = &[];
            to accepts_without_a_subprotocol { be_ok }
        }
        when an_empty_item_precedes_the_protocol {
            let fields: &[&[u8]] = &[b",chat"];
            to ignores_empty_items { be_ok }
        }
        when an_empty_item_follows_the_protocol {
            let fields: &[&[u8]] = &[b"chat,"];
            to ignores_empty_items { be_ok }
        }
        when empty_items_separate_protocols {
            let fields: &[&[u8]] = &[b"chat, \t, superchat"];
            to ignores_empty_items { be_ok }
        }
        when only_empty_items_are_offered {
            let fields: &[&[u8]] = &[b", \t,"];
            to rejects_a_list_without_protocols { be_err }
        }
        when the_field_value_is_empty {
            let fields: &[&[u8]] = &[b""];
            to rejects_a_list_without_protocols { be_err }
        }
        when an_item_contains_whitespace {
            let fields: &[&[u8]] = &[b"chat, bad token"];
            to rejects_an_invalid_token { be_err }
        }
        when an_item_contains_non_ascii_whitespace {
            let fields: &[&[u8]] = &["chat, \u{a0}superchat".as_bytes()];
            to rejects_an_invalid_token { be_err }
        }
        when a_field_contains_opaque_bytes {
            let fields: &[&[u8]] = &[b"chat,\xff"];
            to rejects_an_invalid_field { be_err }
        }
        when a_protocol_is_repeated {
            let fields: &[&[u8]] = &[b"chat, chat"];
            to rejects_duplicate_protocols { be_err }
        }
        when protocol_names_differ_only_in_case {
            let fields: &[&[u8]] = &[b"chat, Chat"];
            to accepts_distinct_protocols { be_ok }
        }
        when fields_are_repeated {
            let fields: &[&[u8]] = &[b"chat", b"superchat"];
            to accepts_combined_protocols { be_ok }
            when an_empty_field_precedes_a_protocol {
                let fields: &[&[u8]] = &[b", \t", b"superchat"];
                to ignores_empty_items { be_ok }
            }
            when a_protocol_is_repeated {
                let fields: &[&[u8]] = &[b"chat", b"superchat, chat"];
                to rejects_duplicate_protocols { be_err }
            }
            when an_invalid_field_follows_a_protocol {
                let fields: &[&[u8]] = &[b"chat", b"bad token"];
                to rejects_an_invalid_token { be_err }
            }
        }
    }
}

#[derive(Debug)]
struct PayloadStorage {
    payload: Bytes,
    shares_transport_storage: bool,
}

fn owns_payload(expected: Vec<u8>) -> impl Fn(&PayloadStorage) -> AssertionResult {
    move |actual| {
        if actual.payload.as_ref() == expected && !actual.shares_transport_storage {
            Ok(())
        } else {
            Err(AssertionError::new(vec![format!(
                "expected intact payload {expected:?} without transport storage; got {actual:?}"
            )]))
        }
    }
}

fn complete_frame(text: bool, payload: &[u8]) -> Frame {
    let bytes = NBytes::copy_from_slice(payload);
    if text {
        Frame::Text(bytes)
    } else {
        Frame::Binary(bytes)
    }
}

fn message_storage(text: bool, payload_len: usize) -> PayloadStorage {
    let mut input = NBytesMut::with_capacity(256 * 1024);
    // Masked client frame; lengths here remain in the one-byte length domain.
    input.extend_from_slice(&[
        if text { 0x81 } else { 0x82 },
        0x80 | payload_len as u8,
        0,
        0,
        0,
        0,
    ]);
    input.extend_from_slice(&vec![b'x'; payload_len]);
    let allocation = input.as_ptr() as usize;
    let capacity = input.capacity();
    let codec = CheckedCodec::new(1024);
    let frame = codec.decode(&mut input).unwrap().unwrap();
    let payload = Assembly::default().accept(frame, 1024).unwrap().unwrap();
    let shares_transport_storage =
        (allocation..allocation + capacity).contains(&(payload.as_ptr() as usize));
    // Keep the input alive until after comparing addresses, so a copy cannot reuse it.
    drop(input);
    PayloadStorage {
        payload,
        shares_transport_storage,
    }
}

lets_expect! {
    expect(message_storage(text, payload_len)) as complete_message_storage {
        let text = false;
        let payload_len = 24;
        to owns_the_payload_without_transport_storage { owns_payload(vec![b'x'; payload_len]) }
        when the_message_is_empty {
            let payload_len = 0;
            to owns_the_payload_without_transport_storage { owns_payload(Vec::new()) }
        }
        when the_payload_fits_inline {
            let payload_len = 23;
            to owns_the_payload_without_transport_storage { owns_payload(vec![b'x'; payload_len]) }
        }
        when the_payload_exceeds_the_inline_boundary {
            let payload_len = 32;
            to owns_the_payload_without_transport_storage { owns_payload(vec![b'x'; payload_len]) }
        }
        when the_message_is_text {
            let text = true;
            to owns_the_payload_without_transport_storage { owns_payload(vec![b'x'; payload_len]) }
            when the_message_is_empty {
                let payload_len = 0;
                to owns_the_payload_without_transport_storage { owns_payload(Vec::new()) }
            }
            when the_payload_fits_inline {
                let payload_len = 23;
                to owns_the_payload_without_transport_storage { owns_payload(vec![b'x'; payload_len]) }
            }
            when the_payload_exceeds_the_inline_boundary {
                let payload_len = 32;
                to owns_the_payload_without_transport_storage { owns_payload(vec![b'x'; payload_len]) }
            }
        }
    }
    expect(Assembly::default().data(complete_frame(text, &payload), limit)) as complete_message_validation {
        let text = false;
        let payload = vec![b'x'; 24];
        let limit = 24;
        to accepts_the_inclusive_limit { be_ok_and be_some_and equal(Bytes::from(payload)) }
        when the_message_exceeds_the_limit {
            let limit = 23;
            to rejects_an_oversized_message { match_pattern!(Err(FrameError::TooBig)) }
        }
        when the_message_is_text {
            let text = true;
            to accepts_the_inclusive_limit { be_ok_and be_some_and equal(Bytes::from(payload)) }
            when the_message_exceeds_the_limit {
                let limit = 23;
                to rejects_an_oversized_message { match_pattern!(Err(FrameError::TooBig)) }
            }
            when the_text_is_invalid {
                let payload = vec![0xff; 24];
                to rejects_invalid_text { match_pattern!(Err(FrameError::InvalidText)) }
                when the_message_exceeds_the_limit {
                    let limit = 23;
                    to rejects_size_before_text { match_pattern!(Err(FrameError::TooBig)) }
                }
            }
        }
    }
}

struct RecordingCodec {
    inner: CheckedCodec,
    capacities: std::cell::RefCell<Vec<usize>>,
}

impl Decoder for RecordingCodec {
    type Item = Frame;
    type Error = FrameError;

    fn decode(&self, input: &mut NBytesMut) -> Result<Option<Frame>, FrameError> {
        let capacity = input.capacity();
        let frame = self.inner.decode(input)?;
        if frame.is_some() {
            self.capacities.borrow_mut().push(capacity);
        }
        Ok(frame)
    }
}

#[derive(Debug)]
struct IncomingStorage {
    payloads: Vec<Bytes>,
    shares_transport_storage: bool,
}

async fn incoming_storage(text: bool) -> IncomingStorage {
    use ntex::{
        codec::Encoder,
        io::{Io, testing::IoTest},
    };
    let (client, server) = IoTest::create();
    let io = Io::new(server, ntex::SharedCfg::default()).boxed();
    let codec = RecordingCodec {
        inner: CheckedCodec::new(256 * 1024),
        capacities: Default::default(),
    };
    let outgoing = ws::Codec::new().client_mode();
    let small_message = || {
        if text {
            Message::Text("x".repeat(24).into())
        } else {
            Message::Binary(NBytes::from(vec![b'x'; 24]))
        }
    };
    let mut frames = ntex::util::BytePages::default();
    outgoing
        .encodev(
            Message::Binary(NBytes::from(vec![b'L'; 128 * 1024])),
            &mut frames,
        )
        .unwrap();
    outgoing.encodev(small_message(), &mut frames).unwrap();
    client.write(frames.freeze());
    let receive = || {
        ntex::time::timeout(
            ntex::time::Millis(1000),
            poll_fn(|cx| io.poll_recv(&codec, cx)),
        )
    };
    let large = receive()
        .await
        .expect("large message arrives")
        .expect("large message decodes");
    assert!(matches!(&large, Frame::Binary(bytes) if bytes.len() == 128 * 1024));
    assert!(codec.capacities.borrow()[0] >= 128 * 1024);
    drop(large);
    let mut assembly = Assembly::default();
    let mut payloads = Vec::new();
    let mut transport_leases = Vec::new();
    let mut shares_transport_storage = false;
    for index in 0..4 {
        if index > 0 {
            let mut frame = ntex::util::BytePages::default();
            outgoing.encodev(small_message(), &mut frame).unwrap();
            client.write(frame.freeze());
        }
        let frame = receive()
            .await
            .expect("small message arrives")
            .expect("small message decodes");
        let lease = match &frame {
            Frame::Binary(bytes) | Frame::Text(bytes) => bytes.clone(),
            frame => panic!("expected a data frame, got {frame:?}"),
        };
        let payload = assembly.accept(frame, 256 * 1024).unwrap().unwrap();
        shares_transport_storage |= payload.as_ptr() == lease.as_ptr();
        // Live leases prevent allocator address reuse from looking like shared ownership.
        transport_leases.push(lease);
        payloads.push(payload);
    }
    io.terminate();
    IncomingStorage {
        payloads,
        shares_transport_storage,
    }
}

fn owns_incoming_payloads(actual: &IncomingStorage) -> AssertionResult {
    let expected = vec![Bytes::from(vec![b'x'; 24]); 4];
    if actual.payloads == expected && !actual.shares_transport_storage {
        Ok(())
    } else {
        Err(AssertionError::new(vec![format!(
            "expected four intact 24-byte payloads without transport storage; got {actual:?}"
        )]))
    }
}

lets_expect! {
    expect(crate::tests::run_ntex(incoming_storage(text))) as incoming_message_storage {
        let text = false;
        to releases_transport_storage_for_retained_payloads { owns_incoming_payloads }
        when the_message_is_text {
            let text = true;
            to releases_transport_storage_for_retained_payloads { owns_incoming_payloads }
        }
    }
}
