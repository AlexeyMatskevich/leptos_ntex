use super::*;
use lets_expect::lets_expect;
use ntex::http::{StatusCode, header};
use ntex::web::test;

// Builder fields are independent axes; each context overrides one field.
fn configured_limits(
    payload: Option<usize>,
    buffer: Option<usize>,
    protocol: Option<&'static str>,
) -> crate::LeptosServerFnConfig {
    let mut config = crate::LeptosServerFnConfig::new();
    if let Some(limit) = payload {
        config = config.with_payload_limit(limit);
    }
    if let Some(limit) = buffer {
        config = config.with_ws_channel_buffer(limit);
    }
    if let Some(protocol) = protocol {
        config = config.with_ws_subprotocol(protocol);
    }
    config
}

lets_expect! {
    expect(configured_limits(payload, buffer, protocol)) as independent_config_fields {
        let payload = None;
        let buffer = None;
        let protocol = None;
        to preserves_every_independent_field {
            have(payload_limit) equal(crate::DEFAULT_PAYLOAD_LIMIT),
            have(ws_channel_buffer) equal(crate::DEFAULT_WS_CHANNEL_BUFFER),
            have(ws_subprotocol) equal(None),
        }
        when the_subprotocol_is_overridden {
            let protocol = Some("graphql-ws");
            to preserves_every_independent_field {
                have(payload_limit) equal(crate::DEFAULT_PAYLOAD_LIMIT),
                have(ws_channel_buffer) equal(crate::DEFAULT_WS_CHANNEL_BUFFER),
                have(ws_subprotocol) equal(Some("graphql-ws")),
            }
        }
        when the_channel_buffer_is_overridden {
            let buffer = Some(32);
            to preserves_every_independent_field {
                have(payload_limit) equal(crate::DEFAULT_PAYLOAD_LIMIT),
                have(ws_channel_buffer) equal(32),
                have(ws_subprotocol) equal(None),
            }
            when the_subprotocol_is_overridden {
                let protocol = Some("graphql-ws");
                to preserves_every_independent_field {
                    have(payload_limit) equal(crate::DEFAULT_PAYLOAD_LIMIT),
                    have(ws_channel_buffer) equal(32),
                    have(ws_subprotocol) equal(Some("graphql-ws")),
                }
            }
        }
        when the_payload_limit_is_overridden {
            let payload = Some(4096);
            to preserves_every_independent_field {
                have(payload_limit) equal(4096),
                have(ws_channel_buffer) equal(crate::DEFAULT_WS_CHANNEL_BUFFER),
                have(ws_subprotocol) equal(None),
            }
            when the_subprotocol_is_overridden {
                let protocol = Some("graphql-ws");
                to preserves_every_independent_field {
                    have(payload_limit) equal(4096),
                    have(ws_channel_buffer) equal(crate::DEFAULT_WS_CHANNEL_BUFFER),
                    have(ws_subprotocol) equal(Some("graphql-ws")),
                }
            }
            when the_channel_buffer_is_overridden {
                let buffer = Some(32);
                to preserves_every_independent_field {
                    have(payload_limit) equal(4096),
                    have(ws_channel_buffer) equal(32),
                    have(ws_subprotocol) equal(None),
                }
                when the_subprotocol_is_overridden {
                    let protocol = Some("graphql-ws");
                    to preserves_every_independent_field {
                        have(payload_limit) equal(4096),
                        have(ws_channel_buffer) equal(32),
                        have(ws_subprotocol) equal(Some("graphql-ws")),
                    }
                }
            }
        }
    }
    expect(crate::LeptosServerFnConfig::default()) as default_config {
        to agrees_with_new {
            have(payload_limit) equal(crate::LeptosServerFnConfig::new().payload_limit),
            have(ws_channel_buffer) equal(crate::LeptosServerFnConfig::new().ws_channel_buffer),
            have(ws_subprotocol) equal(crate::LeptosServerFnConfig::new().ws_subprotocol),
        }
    }
}

// Header field replacement is separate from the cardinality of either set.
// Every spec invokes the merge anew and compares the complete ordered field set.
fn reconcile_header(key: header::HeaderName, existing: &[&str], incoming: &[&str]) -> Vec<String> {
    let mut response = crate::response::NtexResponse(ntex::web::HttpResponse::Ok().finish());
    for value in existing {
        response
            .0
            .headers_mut()
            .append(key.clone(), header::HeaderValue::from_str(value).unwrap());
    }
    let mut parts = crate::ResponseParts::default();
    for value in incoming {
        parts.append_header(key.clone(), header::HeaderValue::from_str(value).unwrap());
    }
    response.extend_response_parts(parts);
    response
        .take()
        .headers()
        .get_all(key)
        .map(|value| value.to_str().unwrap().to_string())
        .collect()
}

lets_expect! {
    expect(reconcile_header(key, existing, incoming)) as list_header_cardinality {
        let key = header::CACHE_CONTROL;
        let existing = &["private"];
        let incoming = &["no-store", "max-age=60"];
        to preserves_the_required_field_set { equal(vec!["no-store".to_string(), "max-age=60".to_string()]) }
        when one_incoming_value {
            let incoming = &["no-store"];
            to preserves_the_required_field_set { equal(vec!["no-store".to_string()]) }
        }
        when no_incoming_value {
            let incoming = &[];
            to preserves_the_required_field_set { equal(vec!["private".to_string()]) }
        }
        when no_existing_value {
            let existing = &[];
            to preserves_the_required_field_set { equal(vec!["no-store".to_string(), "max-age=60".to_string()]) }
            when one_incoming_value {
                let incoming = &["no-store"];
                to preserves_the_required_field_set { equal(vec!["no-store".to_string()]) }
            }
            when no_incoming_value {
                let incoming = &[];
                to preserves_the_required_field_set { equal(Vec::<String>::new()) }
            }
        }
        when multiple_existing_values {
            let existing = &["private", "max-age=5"];
            to preserves_the_required_field_set { equal(vec!["no-store".to_string(), "max-age=60".to_string()]) }
            when one_incoming_value {
                let incoming = &["no-store"];
                to preserves_the_required_field_set { equal(vec!["no-store".to_string()]) }
            }
            when no_incoming_value {
                let incoming = &[];
                to preserves_the_required_field_set { equal(vec!["private".to_string(), "max-age=5".to_string()]) }
            }
        }
    }
}

lets_expect! {
    expect(reconcile_header(key, existing, incoming)) as singleton_header_cardinality {
        let key = header::CONTENT_TYPE;
        let existing = &["text/plain"];
        let incoming = &["application/json", "application/xml"];
        to preserves_the_required_field_set { equal(vec!["application/xml".to_string()]) }
        when one_incoming_value {
            let incoming = &["application/json"];
            to preserves_the_required_field_set { equal(vec!["application/json".to_string()]) }
        }
        when no_incoming_value {
            let incoming = &[];
            to preserves_the_required_field_set { equal(vec!["text/plain".to_string()]) }
        }
        when no_existing_value {
            let existing = &[];
            to preserves_the_required_field_set { equal(vec!["application/xml".to_string()]) }
            when one_incoming_value {
                let incoming = &["application/json"];
                to preserves_the_required_field_set { equal(vec!["application/json".to_string()]) }
            }
            when no_incoming_value {
                let incoming = &[];
                to preserves_the_required_field_set { equal(Vec::<String>::new()) }
            }
        }
        when multiple_existing_values {
            let existing = &["text/plain", "text/html"];
            to preserves_the_required_field_set { equal(vec!["application/xml".to_string()]) }
            when one_incoming_value {
                let incoming = &["application/json"];
                to preserves_the_required_field_set { equal(vec!["application/json".to_string()]) }
            }
            when no_incoming_value {
                let incoming = &[];
                to preserves_the_required_field_set { equal(vec!["text/plain".to_string(), "text/html".to_string()]) }
            }
        }
    }
}

lets_expect! {
    expect(reconcile_header(key, existing, incoming)) as appended_header_cardinality {
        let key = header::SET_COOKIE;
        let existing = &["a=1"];
        let incoming = &["c=3", "d=4"];
        to preserves_the_required_field_set { equal(vec!["a=1".to_string(), "c=3".to_string(), "d=4".to_string()]) }
        when one_incoming_value {
            let incoming = &["c=3"];
            to preserves_the_required_field_set { equal(vec!["a=1".to_string(), "c=3".to_string()]) }
        }
        when no_incoming_value {
            let incoming = &[];
            to preserves_the_required_field_set { equal(vec!["a=1".to_string()]) }
        }
        when no_existing_value {
            let existing = &[];
            to preserves_the_required_field_set { equal(vec!["c=3".to_string(), "d=4".to_string()]) }
            when one_incoming_value {
                let incoming = &["c=3"];
                to preserves_the_required_field_set { equal(vec!["c=3".to_string()]) }
            }
            when no_incoming_value {
                let incoming = &[];
                to preserves_the_required_field_set { equal(Vec::<String>::new()) }
            }
        }
        when multiple_existing_values {
            let existing = &["a=1", "b=2"];
            to preserves_the_required_field_set { equal(vec!["a=1".to_string(), "b=2".to_string(), "c=3".to_string(), "d=4".to_string()]) }
            when one_incoming_value {
                let incoming = &["c=3"];
                to preserves_the_required_field_set { equal(vec!["a=1".to_string(), "b=2".to_string(), "c=3".to_string()]) }
            }
            when no_incoming_value {
                let incoming = &[];
                to preserves_the_required_field_set { equal(vec!["a=1".to_string(), "b=2".to_string()]) }
            }
        }
    }
}

lets_expect! {
    expect(reconcile_header(key, existing, incoming)) as header_field_contract {
        let key = header::CACHE_CONTROL;
        let existing = &["private"];
        let incoming = &["no-store", "max-age=60"];
        to applies_the_complete_override { equal(vec!["no-store".to_string(), "max-age=60".to_string()]) }
        when the_field_is_content_encoding {
            let key = header::CONTENT_ENCODING;
            let existing = &["br"];
            let incoming = &["gzip", "identity"];
            to applies_the_complete_override { equal(vec!["gzip".to_string(), "identity".to_string()]) }
        }
        when the_field_is_transfer_encoding {
            let key = header::TRANSFER_ENCODING;
            let existing = &["identity"];
            let incoming = &["gzip", "chunked"];
            to applies_the_complete_override { equal(vec!["gzip".to_string(), "chunked".to_string()]) }
        }
        when the_field_is_accept_ranges {
            let key = header::ACCEPT_RANGES;
            let existing = &["none"];
            let incoming = &["bytes", "custom"];
            to applies_the_complete_override { equal(vec!["bytes".to_string(), "custom".to_string()]) }
        }
        when the_field_is_content_length {
            let key = header::CONTENT_LENGTH;
            let existing = &["1"];
            let incoming = &["2", "3"];
            to applies_the_complete_override { equal(vec!["3".to_string()]) }
        }
        when the_field_is_content_type {
            let key = header::CONTENT_TYPE;
            let existing = &["text/plain"];
            let incoming = &["text/html", "application/json"];
            to applies_the_complete_override { equal(vec!["application/json".to_string()]) }
        }
        when the_field_is_location {
            let key = header::LOCATION;
            let existing = &["/old"];
            let incoming = &["/one", "/two"];
            to applies_the_complete_override { equal(vec!["/two".to_string()]) }
        }
        when the_field_is_etag {
            let key = header::ETAG;
            let existing = &["old"];
            let incoming = &["one", "two"];
            to applies_the_complete_override { equal(vec!["two".to_string()]) }
        }
        when the_field_is_last_modified {
            let key = header::LAST_MODIFIED;
            let existing = &["Mon, 01 Jan 2024 00:00:00 GMT"];
            let incoming = &["Tue, 02 Jan 2024 00:00:00 GMT", "Wed, 03 Jan 2024 00:00:00 GMT"];
            to applies_the_complete_override { equal(vec!["Wed, 03 Jan 2024 00:00:00 GMT".to_string()]) }
        }
        when the_field_is_expires {
            let key = header::EXPIRES;
            let existing = &["Mon, 01 Jan 2024 00:00:00 GMT"];
            let incoming = &["Tue, 02 Jan 2024 00:00:00 GMT", "Wed, 03 Jan 2024 00:00:00 GMT"];
            to applies_the_complete_override { equal(vec!["Wed, 03 Jan 2024 00:00:00 GMT".to_string()]) }
        }
        when the_field_is_content_disposition {
            let key = header::CONTENT_DISPOSITION;
            let existing = &["inline"];
            let incoming = &["attachment; filename=one", "attachment; filename=two"];
            to applies_the_complete_override { equal(vec!["attachment; filename=two".to_string()]) }
        }
        when the_field_is_content_range {
            let key = header::CONTENT_RANGE;
            let existing = &["bytes 0-1/5"];
            let incoming = &["bytes 1-2/5", "bytes 2-3/5"];
            to applies_the_complete_override { equal(vec!["bytes 2-3/5".to_string()]) }
        }
        when the_field_is_content_location {
            let key = header::CONTENT_LOCATION;
            let existing = &["/framework"];
            let incoming = &["/one", "/two"];
            to applies_the_complete_override { equal(vec!["/two".to_string()]) }
        }
        when the_field_is_retry_after {
            let key = header::RETRY_AFTER;
            let existing = &["1"];
            let incoming = &["2", "3"];
            to applies_the_complete_override { equal(vec!["3".to_string()]) }
        }
        when the_field_is_strict_transport_security {
            let key = header::STRICT_TRANSPORT_SECURITY;
            let existing = &["max-age=1"];
            let incoming = &["max-age=2", "max-age=3"];
            to applies_the_complete_override { equal(vec!["max-age=3".to_string()]) }
        }
        when the_field_is_content_digest {
            let key = header::HeaderName::from_static("content-digest");
            let existing = &["sha-256=:old:"];
            let incoming = &["sha-256=:one:", "sha-256=:two:"];
            to applies_the_complete_override { equal(vec!["sha-256=:two:".to_string()]) }
        }
        when the_field_is_set_cookie {
            let key = header::SET_COOKIE;
            let existing = &["a=1"];
            let incoming = &["b=2", "c=3"];
            to applies_the_complete_override { equal(vec!["a=1".to_string(), "b=2".to_string(), "c=3".to_string()]) }
        }
        when the_field_is_vary {
            let key = header::VARY;
            let existing = &["Accept"];
            let incoming = &["Origin", "Accept-Encoding"];
            to applies_the_complete_override { equal(vec!["Accept".to_string(), "Origin".to_string(), "Accept-Encoding".to_string()]) }
        }
        when the_field_is_custom {
            let key = header::HeaderName::from_static("x-custom");
            let existing = &["old"];
            let incoming = &["one", "two"];
            to applies_the_complete_override { equal(vec!["old".to_string(), "one".to_string(), "two".to_string()]) }
        }
    }
}

// ----- extend_response_parts status override: exhaustive spec -------
// A captured `Some(status)` overrides the response status; `None` leaves
// it untouched. The old test exercised neither.
fn status_after_extend(override_status: Option<StatusCode>) -> StatusCode {
    let mut response = crate::response::NtexResponse(ntex::web::HttpResponse::Ok().finish());
    let parts = crate::ResponseParts {
        status: override_status,
        ..Default::default()
    };
    response.extend_response_parts(parts);
    response.take().status()
}

lets_expect! {
    expect(status_after_extend(override_status)) as the_extended_status {
        let override_status: Option<StatusCode> = None;

        to leaves_the_existing_status_unchanged { equal(StatusCode::OK) }

        when a_status_override_is_present {
            let override_status = Some(StatusCode::CREATED);
            to applies_the_overridden_status { equal(StatusCode::CREATED) }
        }
    }
}

// ----- content_length_exceeds preflight: boundary spec --------------
// The 413 preflight that rejects an oversize body declared up-front via
// `Content-Length`, WITHOUT reading it. The boundary is the crux: a body
// of *exactly* `limit` bytes does NOT "exceed" the limit (the predicate
// is strict `>`), so the preflight must let it through — only `limit + 1`
// and above are rejected. A MISSING header is not a size declaration, so the
// preflight stays out of the way (false); a PRESENT but malformed declaration
// (non-numeric, or a number beyond `usize`) is a pathological/oversize claim
// and is rejected up-front (true).
fn content_length_preflight(content_length: Option<&str>, limit: usize) -> bool {
    let mut req = test::TestRequest::default();
    if let Some(value) = content_length {
        req = req.header(header::CONTENT_LENGTH, value);
    }
    crate::config::content_length_exceeds(&req.to_http_request(), limit)
}

lets_expect! {
    expect(content_length_preflight(content_length, limit)) as the_preflight {
        let limit = 1024usize;
        let content_length: Option<&str> = Some("2048");

        to rejects_an_oversize_declaration { be_true }

        when the_declared_length_is_below_the_limit {
            let content_length = Some("1023");
            to allows_the_body { be_false }
        }

        when the_declared_length_is_exactly_the_limit {
            let content_length = Some("1024");
            to allows_the_body { be_false }
        }

        when the_declared_length_is_one_byte_over_the_limit {
            let content_length = Some("1025");
            to rejects_the_body { be_true }
        }

        when no_content_length_is_declared {
            let content_length: Option<&str> = None;
            to stays_out_of_the_way { be_false }
        }

        when the_content_length_is_not_a_number {
            let content_length = Some("not-a-number");
            to rejects_the_malformed_declaration { be_true }
        }

        when the_declared_length_overflows_usize {
            // A value beyond `usize::MAX` cannot parse, so it must be rejected
            // as a malformed oversize claim rather than slipping past as "no
            // declaration".
            let content_length = Some("99999999999999999999999999");
            to rejects_the_overflowing_declaration { be_true }
        }
    }
}

// ----- initial_payload_capacity: eager-allocation clamp -------------
// The up-front buffer reservation derived from a client-declared
// `Content-Length`. Axes: declaration present/absent; declared vs `limit`
// (an over-limit declaration reserves NOTHING — it is rejected later
// anyway, so pre-sizing for it would only serve an attacker; boundary at
// exactly-limit and one-over); declared vs the 64 KiB cap (a plausible
// declaration passes through below the cap and clamps at/above it —
// boundary at exactly-cap and one-over). The default `limit` sits ABOVE
// the cap so both axes stay observable; the exactly-the-limit leaf drops
// `limit` below the cap to show the declaration passing through unclamped.
lets_expect! {
    expect(crate::config::initial_payload_capacity(content_length, limit)) as the_initial_capacity {
        let limit = 8 * 1024 * 1024usize;
        let content_length: Option<usize> = Some(1024);

        to reserves_the_declared_size { equal(1024usize) }

        when no_content_length_is_declared {
            let content_length: Option<usize> = None;
            to reserves_nothing { equal(0usize) }
        }

        when the_declaration_is_one_byte_over_the_limit {
            let content_length = Some(limit + 1);
            to reserves_nothing { equal(0usize) }
        }

        when the_declaration_is_exactly_the_limit_and_below_the_cap {
            let limit = 4096usize;
            let content_length = Some(4096);
            to reserves_the_declared_size { equal(4096usize) }
        }

        when the_declaration_is_exactly_the_cap {
            let content_length = Some(crate::config::INITIAL_PAYLOAD_CAPACITY_CAP);
            to reserves_the_full_cap {
                equal(crate::config::INITIAL_PAYLOAD_CAPACITY_CAP)
            }
        }

        when the_declaration_is_one_byte_over_the_cap {
            let content_length = Some(crate::config::INITIAL_PAYLOAD_CAPACITY_CAP + 1);
            to clamps_to_the_cap {
                equal(crate::config::INITIAL_PAYLOAD_CAPACITY_CAP)
            }
        }

        when the_declaration_is_far_over_the_cap {
            let content_length = Some(1024 * 1024);
            to clamps_to_the_cap {
                equal(crate::config::INITIAL_PAYLOAD_CAPACITY_CAP)
            }
        }
    }
}

// ----- DEFAULT_PAYLOAD_LIMIT: pins the documented 2 MiB default ------
// Matches ntex's own `PayloadConfig` default. A regression in the
// constant expression (e.g. a dropped factor) changes the limit silently.
lets_expect! {
    expect(crate::DEFAULT_PAYLOAD_LIMIT) as the_default_payload_limit {
        to is_two_mebibytes { equal(2 * 1024 * 1024) }
    }
}

// ----- ResponseParts::insert_header: overwrite semantics ------------
// `insert_header` must REPLACE any previous value for the same key
// (unlike `append_header`). A no-op regression drops the header entirely.
fn response_parts_header_values(inserts: &[&str]) -> Vec<String> {
    let name = header::HeaderName::from_static("x-test");
    let mut parts = crate::ResponseParts::default();
    for value in inserts {
        parts.insert_header(name.clone(), header::HeaderValue::from_str(value).unwrap());
    }
    parts
        .headers
        .get_all(&name)
        .filter_map(|value| value.to_str().ok())
        .map(str::to_string)
        .collect()
}

lets_expect! {
    expect(response_parts_header_values(inserts)) as response_parts_headers {
        let inserts: &[&str] = &["first"];

        to records_the_inserted_header { equal(vec!["first".to_string()]) }

        when the_same_key_is_inserted_twice {
            let inserts: &[&str] = &["first", "second"];
            to keeps_only_the_latest_value { equal(vec!["second".to_string()]) }
        }
    }
}

// ----- ResponseOptions::overwrite: wholesale replacement ------------
// `overwrite` swaps the entire inner `ResponseParts`, so BOTH halves of the
// contract are pinned: a previously set status is replaced by the incoming one
// (including back to `None`), AND a previously set header is dropped, leaving
// only the replacement's headers — a `*writable = parts` regression that kept
// or merged stale headers would otherwise pass the status-only leaves.
fn status_after_overwrite(replacement: Option<StatusCode>) -> Option<StatusCode> {
    let options = crate::ResponseOptions::default();
    options.set_status(StatusCode::OK);
    options.overwrite(crate::ResponseParts {
        status: replacement,
        ..Default::default()
    });
    options.0.read().unwrap().status
}

// (old header present?, new header present?) after a wholesale overwrite.
fn headers_after_overwrite() -> (bool, bool) {
    let options = crate::ResponseOptions::default();
    options.insert_header(
        header::HeaderName::from_static("x-old"),
        header::HeaderValue::from_static("1"),
    );
    let mut replacement = crate::ResponseParts::default();
    replacement.insert_header(
        header::HeaderName::from_static("x-new"),
        header::HeaderValue::from_static("2"),
    );
    options.overwrite(replacement);
    let parts = options.0.read().unwrap();
    (
        parts.headers.contains_key("x-old"),
        parts.headers.contains_key("x-new"),
    )
}

lets_expect! {
    expect(status_after_overwrite(replacement)) as overwriting_response_parts {
        let replacement: Option<StatusCode> = Some(StatusCode::IM_A_TEAPOT);

        to replaces_the_previously_set_status { equal(Some(StatusCode::IM_A_TEAPOT)) }

        when the_replacement_carries_no_status {
            let replacement: Option<StatusCode> = None;
            to clears_the_previously_set_status { equal(None) }
        }
    }
}

lets_expect! {
    expect(headers_after_overwrite()) as overwriting_response_parts_headers {
        to drops_old_headers_and_keeps_only_the_replacement {
            equal((false, true))
        }
    }
}

// ----- ResponseOptions::insert_header / append_header -----------------
// `ResponseOptions` is the `Arc<RwLock<ResponseParts>>` wrapper actually
// injected as context and used by app/component/server-fn code, so its own
// `insert_header`/`append_header` need a direct spec independent of the
// inner `ResponseParts` methods they delegate to: `insert_header` must
// REPLACE any previous value for the same key, while `append_header` must
// APPEND, keeping both (the multi-value guarantee repeated `Set-Cookie`
// calls rely on). A `append_header` regression that called `.insert(...)`
// instead of `.headers.append(...)` would silently drop the earlier value.
fn response_options_header_values(
    second_call: fn(&crate::ResponseOptions, header::HeaderName, header::HeaderValue),
) -> Vec<String> {
    let name = header::HeaderName::from_static("x-test");
    let options = crate::ResponseOptions::default();
    options.insert_header(name.clone(), header::HeaderValue::from_static("first"));
    second_call(
        &options,
        name.clone(),
        header::HeaderValue::from_static("second"),
    );
    options
        .0
        .read()
        .unwrap()
        .headers
        .get_all(&name)
        .filter_map(|value| value.to_str().ok())
        .map(str::to_string)
        .collect()
}

lets_expect! {
    expect(response_options_header_values(second_call)) as the_response_options_header_values {
        let second_call: fn(&crate::ResponseOptions, header::HeaderName, header::HeaderValue) =
            crate::ResponseOptions::insert_header;

        to keeps_only_the_latest_value_when_inserted_twice {
            equal(vec!["second".to_string()])
        }

        when the_same_key_is_appended_instead {
            let second_call: fn(&crate::ResponseOptions, header::HeaderName, header::HeaderValue) =
                crate::ResponseOptions::append_header;
            to keeps_both_values_in_insertion_order {
                equal(vec!["first".to_string(), "second".to_string()])
            }
        }
    }
}

// ----- render::ntex_method: exhaustive leptos→ntex method map -------
// Every `leptos_router::Method` variant maps to its ntex counterpart;
// a collapse to `Method::default()` (GET) would misroute POST/PUT/etc.
lets_expect! {
    expect(crate::render::ntex_method(method)) as the_mapped_ntex_method {
        let method = leptos_router::Method::Get;

        to maps_get { equal(ntex::http::Method::GET) }

        when the_method_is_post {
            let method = leptos_router::Method::Post;
            to maps_post { equal(ntex::http::Method::POST) }
        }

        when the_method_is_put {
            let method = leptos_router::Method::Put;
            to maps_put { equal(ntex::http::Method::PUT) }
        }

        when the_method_is_delete {
            let method = leptos_router::Method::Delete;
            to maps_delete { equal(ntex::http::Method::DELETE) }
        }

        when the_method_is_patch {
            let method = leptos_router::Method::Patch;
            to maps_patch { equal(ntex::http::Method::PATCH) }
        }
    }
}

// ----- NtexRouteListing getters: mode() and methods() ---------------
// The getters must report the values the listing was built with, not the
// type defaults (a collapse to `SsrMode::default()` / `[Method::Get]`
// would silently re-mode and re-method every route).
// Two distinct methods, so `methods()` must report BOTH in order — a single
// method could not catch a `.take(1)` / drop-after-first / reorder regression.
fn listing_with_mode(mode: leptos_router::SsrMode) -> crate::NtexRouteListing {
    crate::NtexRouteListing::new(
        "/sample".to_string(),
        mode,
        [leptos_router::Method::Post],
        Vec::new(),
    )
}

lets_expect! {
    expect(listing_with_mode(mode).mode()) as the_listing_mode {
        let mode = leptos_router::SsrMode::Async;

        to reports_the_configured_mode { equal(leptos_router::SsrMode::Async) }

        // A SECOND, different mode pins that the getter ECHOES the stored
        // value, not a fixed non-default one (a `mode()` hardcoded to `Async`
        // would pass the first leaf alone).
        when the_listing_was_built_in_order {
            let mode = leptos_router::SsrMode::InOrder;
            to echoes_that_mode { equal(leptos_router::SsrMode::InOrder) }
        }

        // The type's own `#[default]` variant, pinned precisely because it IS
        // the default: a `mode()` rewrite that collapses any other variant
        // onto `SsrMode::default()` would otherwise still pass this leaf.
        when the_listing_was_built_out_of_order {
            let mode = leptos_router::SsrMode::OutOfOrder;
            to echoes_that_mode { equal(leptos_router::SsrMode::OutOfOrder) }
        }

        // No other leaf here builds a `PartiallyBlocked` listing.
        when the_listing_was_built_partially_blocked {
            let mode = leptos_router::SsrMode::PartiallyBlocked;
            to echoes_that_mode { equal(leptos_router::SsrMode::PartiallyBlocked) }
        }

        // The `Static(StaticRoute)` payload variant: a getter that clones the
        // wrong inner value (or re-defaults the payload) would still satisfy
        // `matches!(mode, SsrMode::Static(_))` checks elsewhere in the crate,
        // but only an exact-value check like this one catches it.
        when the_listing_was_built_static {
            let static_route = leptos_router::static_routes::StaticRoute::new()
                .prerender_params(|| async { Default::default() })
                .regenerate(|_| futures::stream::pending());
            let mode = leptos_router::SsrMode::Static(static_route.clone());
            to preserves_the_configured_callbacks {
                equal(leptos_router::SsrMode::Static(static_route))
            }
        }
    }
}

lets_expect! {
    expect(crate::NtexRouteListing::new(
        "/methods".to_string(), leptos_router::SsrMode::Async, methods, Vec::new(),
    ).methods().collect::<Vec<_>>()) as listing_method_cardinality {
        let methods = vec![leptos_router::Method::Get, leptos_router::Method::Post];
        to preserves_every_method_in_order {
            equal(vec![leptos_router::Method::Get, leptos_router::Method::Post])
        }
        when one_method_is_configured {
            let methods = vec![leptos_router::Method::Post];
            to preserves_the_single_method { equal(vec![leptos_router::Method::Post]) }
        }
        when no_methods_are_configured {
            let methods = Vec::<leptos_router::Method>::new();
            to does_not_invent_a_get_method { equal(Vec::<leptos_router::Method>::new()) }
        }
    }
}

// ----- generate_route_list exclusion + path rendering ---------------
// Two behaviours in one spec over the public route-list API:
//   * every produced path carries its leading slash (the `to_ntex_path`
//     segment-separator logic), and
//   * an excluded path is dropped from the *active* listings (the
//     `retain(!excluded)` filter).
// `StaticApp` registers exactly `/` and `/about`.
fn active_paths_after_excluding(excluded: &[&str]) -> Vec<String> {
    let excluded = if excluded.is_empty() {
        None
    } else {
        Some(excluded.iter().map(|s| s.to_string()).collect())
    };
    let mut paths = gen_route_list_with_exclusions(StaticApp, excluded)
        .into_iter()
        .filter(|listing| !listing.exclude)
        .map(|listing| listing.path().to_string())
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

lets_expect! {
    expect(active_paths_after_excluding(excluded)) as the_active_route_paths {
        let excluded: &[&str] = &[];

        to lists_every_route_with_a_leading_slash {
            equal(vec!["/".to_string(), "/about".to_string()])
        }

        when a_route_is_excluded {
            let excluded: &[&str] = &["/about"];
            to drops_the_excluded_route_from_the_active_set {
                equal(vec!["/".to_string()])
            }
        }
    }
}

// ----- empty-route-tree exclusion ------------------------------------
// The synthetic-fallback branch: when the app declares no routes,
// `generate_route_list*` injects a single GET `/` so the shell still
// renders. Excluding `/` must drop that synthetic listing from the ACTIVE
// set exactly like a real route — otherwise a custom root handler is
// shadowed by Leptos's GET `/`. `EmptyApp` declares no `<Router>`, so
// `RouteList::generate` yields nothing and the synthetic branch runs.
// The default leaf (count == 1) is load-bearing: it proves the synthetic
// `/` is actually produced, so the exclusion leaf cannot pass vacuously.
fn active_root_listings_from_empty_tree(excluded: &[&str]) -> usize {
    let excluded = if excluded.is_empty() {
        None
    } else {
        Some(excluded.iter().map(|s| s.to_string()).collect())
    };
    gen_route_list_with_exclusions(EmptyApp, excluded)
        .into_iter()
        .filter(|listing| !listing.exclude && listing.path() == "/")
        .count()
}

lets_expect! {
    expect(active_root_listings_from_empty_tree(excluded)) as the_synthetic_root_listing {
        let excluded: &[&str] = &[];

        to is_active_when_nothing_is_excluded { equal(1) }

        when the_root_is_excluded {
            let excluded: &[&str] = &["/"];
            to is_dropped_from_the_active_set { equal(0) }
        }
    }
}

// ----- to_ntex_path segment kinds ------------------------------------
// Characteristic axis: the `PathSegment` variant of each route segment,
// carried by the `SegmentKind` enum below — one `when`-context per kind.
//   * `Static` — pinned by the `/about` (single segment) and `/outer/inner`
//     (multi-segment separators) specs elsewhere in this file.
//   * `Param`, `Splat`, `OptionalParam` — each its own context here.
//   * `Unit` — pruned: contributes no path text and is not constructible as
//     a distinct route through `path!`.
// The load-bearing state is `Splat`: ntex tail syntax is `{name}*`; the
// actix-style `{name:.*}` is a single-segment regex in ntex-router, so
// nested URLs under the splat silently fell through to the fallback.
// `OptionalParam` is rewritten by `expand_optionals()` into two listings
// (with and without the optional segment) BEFORE `to_ntex_path` runs.
enum SegmentKind {
    Param,
    Splat,
    OptionalParam,
}

fn segment_kind_route_paths(kind: SegmentKind) -> Vec<String> {
    let listings = match kind {
        SegmentKind::Param => gen_route_list(ParamRouteApp),
        SegmentKind::Splat => gen_route_list(SplatRouteApp),
        SegmentKind::OptionalParam => gen_route_list(OptionalParamRouteApp),
    };
    let mut paths = listings
        .into_iter()
        .map(|r| r.path().to_string())
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

lets_expect! {
    expect(segment_kind_route_paths(kind)) as the_segment_kind_route_paths {
        let kind = SegmentKind::Param;

        to renders_a_param_as_a_brace_segment { equal(vec!["/users/{id}".to_string()]) }

        when the_segment_is_a_splat {
            let kind = SegmentKind::Splat;
            to renders_a_splat_as_an_ntex_tail_match {
                equal(vec!["/files/{any}*".to_string()])
            }
        }

        when the_segment_is_an_optional_param {
            let kind = SegmentKind::OptionalParam;
            to expands_into_a_listing_with_and_without_the_segment {
                equal(vec!["/users".to_string(), "/users/{id}".to_string()])
            }
        }
    }
}

// A multi-segment route path keeps a `/` separator BETWEEN segments.
// Multi-segment paths arrive as separate `Static` segments WITHOUT
// leading slashes (a lone `/about` is stored whole, but `/outer/inner`
// splits), so the separator-insertion in `to_ntex_path` is load-bearing:
// dropping the `!raw.is_empty()` or `!raw.starts_with('/')` guard
// collapses `/outer/inner` into `outerinner`.
lets_expect! {
    expect(gen_route_list(NestedApp).into_iter().map(|route| route.path().to_string()).collect::<Vec<_>>()) as nested_route_paths {
        to separates_parent_and_child_segments { equal(vec!["/outer/inner".to_string()]) }
    }
}
