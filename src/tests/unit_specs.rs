use super::*;
use lets_expect::lets_expect;
use ntex::http::{StatusCode, header};
use ntex::web::test;

// ----- LeptosServerFnConfig builder: exhaustive spec ----------------
// Object-interface spec for the const builder. The behavioural content
// the old single-case test missed: each setter overrides ONLY its own
// field (leaving the siblings at the crate defaults), and `Default`
// agrees with `new()`. `LeptosServerFnConfig` derives no `PartialEq`,
// so each field is asserted individually via `have(...)`.
lets_expect! {
    expect(config) as the_server_fn_config {
        let config = crate::LeptosServerFnConfig::new();

        to starts_from_the_crate_defaults {
            have(payload_limit) equal(crate::DEFAULT_PAYLOAD_LIMIT),
            have(ws_channel_buffer) equal(crate::DEFAULT_WS_CHANNEL_BUFFER),
            have(ws_subprotocol) be_none,
        }

        when built_through_the_default_trait {
            let config = crate::LeptosServerFnConfig::default();
            to matches_new {
                have(payload_limit) equal(crate::DEFAULT_PAYLOAD_LIMIT),
                have(ws_channel_buffer) equal(crate::DEFAULT_WS_CHANNEL_BUFFER),
                have(ws_subprotocol) be_none,
            }
        }

        when only_the_payload_limit_is_overridden {
            let config = crate::LeptosServerFnConfig::new().with_payload_limit(4096);
            to changes_the_payload_limit_alone {
                have(payload_limit) equal(4096),
                have(ws_channel_buffer) equal(crate::DEFAULT_WS_CHANNEL_BUFFER),
                have(ws_subprotocol) be_none,
            }
        }

        when only_the_ws_channel_buffer_is_overridden {
            let config = crate::LeptosServerFnConfig::new().with_ws_channel_buffer(32);
            to changes_the_channel_buffer_alone {
                have(payload_limit) equal(crate::DEFAULT_PAYLOAD_LIMIT),
                have(ws_channel_buffer) equal(32),
                have(ws_subprotocol) be_none,
            }
        }

        when only_the_ws_subprotocol_is_overridden {
            let config = crate::LeptosServerFnConfig::new().with_ws_subprotocol("graphql-ws");
            to changes_the_subprotocol_alone {
                have(payload_limit) equal(crate::DEFAULT_PAYLOAD_LIMIT),
                have(ws_channel_buffer) equal(crate::DEFAULT_WS_CHANNEL_BUFFER),
                have(ws_subprotocol) equal(Some("graphql-ws")),
            }
        }

        when every_field_is_overridden {
            let config = crate::LeptosServerFnConfig::new()
                .with_payload_limit(4096)
                .with_ws_channel_buffer(32)
                .with_ws_subprotocol("graphql-ws");
            to applies_all_three_overrides {
                have(payload_limit) equal(4096),
                have(ws_channel_buffer) equal(32),
                have(ws_subprotocol) equal(Some("graphql-ws")),
            }
        }
    }
}

// ----- NtexResponse::extend_response_parts header merge: exhaustive -
// When merging captured `ResponseParts` into the response, a *singleton*
// header (the `should_replace_header` set — Cache-Control, Location,
// ETag, …, here represented by Cache-Control) must REPLACE any existing
// value, while a multi-value header (e.g. Set-Cookie) must APPEND,
// keeping both. The old test covered only the replace direction; the
// append direction was the missing negative. The assertion pins the
// exact resulting value vector, not mere presence.
fn reconcile_header(key: header::HeaderName, existing: &str, incoming: &str) -> Vec<String> {
    let mut response = crate::response::NtexResponse(
        ntex::web::HttpResponse::Ok()
            .header(key.clone(), existing)
            .finish(),
    );
    let mut parts = crate::ResponseParts::default();
    parts.append_header(
        key.clone(),
        header::HeaderValue::from_str(incoming).unwrap(),
    );
    response.extend_response_parts(parts);
    response
        .take()
        .headers()
        .get_all(key)
        .filter_map(|value| value.to_str().ok())
        .map(str::to_string)
        .collect()
}

lets_expect! {
    expect(reconcile_header(key, existing, incoming)) as the_reconciled_header {
        let key = header::CACHE_CONTROL;
        let existing = "public, max-age=60";
        let incoming = "no-store";

        to replaces_the_previous_singleton_value { equal(vec!["no-store".to_string()]) }

        // Pin the other singleton match arms the original test covered, so
        // a regression deleting only `EXPIRES` or `CONTENT_DISPOSITION`
        // from `should_replace_header` is still caught (each is a distinct
        // arm — not subsumed by the Cache-Control representative).
        when the_singleton_header_is_expires {
            let key = header::EXPIRES;
            let existing = "Wed, 21 Oct 2015 07:28:00 GMT";
            let incoming = "Thu, 01 Jan 1970 00:00:00 GMT";
            to replaces_the_previous_value {
                equal(vec!["Thu, 01 Jan 1970 00:00:00 GMT".to_string()])
            }
        }

        when the_singleton_header_is_content_disposition {
            let key = header::CONTENT_DISPOSITION;
            let existing = "inline";
            let incoming = "attachment";
            to replaces_the_previous_value { equal(vec!["attachment".to_string()]) }
        }

        when the_header_permits_multiple_values {
            let key = header::SET_COOKIE;
            let existing = "session=abc";
            let incoming = "theme=dark";
            to appends_and_keeps_both_values {
                equal(vec!["session=abc".to_string(), "theme=dark".to_string()])
            }
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
// and above are rejected. A missing or unparseable header is not a size
// declaration, so the preflight stays out of the way and returns false.
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
            to stays_out_of_the_way { be_false }
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
// `overwrite` swaps the entire inner `ResponseParts`, so a previously set
// status is replaced by the incoming one (including back to `None`).
fn status_after_overwrite(replacement: Option<StatusCode>) -> Option<StatusCode> {
    let options = crate::ResponseOptions::default();
    options.set_status(StatusCode::OK);
    options.overwrite(crate::ResponseParts {
        status: replacement,
        ..Default::default()
    });
    options.0.read().unwrap().status
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
fn sample_listing() -> crate::NtexRouteListing {
    crate::NtexRouteListing::new(
        "/sample".to_string(),
        leptos_router::SsrMode::Async,
        [leptos_router::Method::Post],
        Vec::new(),
    )
}

lets_expect! {
    expect(sample_listing().mode()) as the_listing_mode {
        to reports_the_configured_mode { equal(leptos_router::SsrMode::Async) }
    }
}

lets_expect! {
    expect(sample_listing().methods().collect::<Vec<_>>()) as the_listing_methods {
        to reports_the_configured_methods { equal(vec![leptos_router::Method::Post]) }
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

/// A multi-segment route path keeps a `/` separator BETWEEN segments.
/// Multi-segment paths arrive as separate `Static` segments WITHOUT
/// leading slashes (a lone `/about` is stored whole, but `/outer/inner`
/// splits), so the separator-insertion in `to_ntex_path` is load-bearing:
/// dropping the `!raw.is_empty()` or `!raw.starts_with('/')` guard
/// collapses `/outer/inner` into `outerinner`.
#[test]
fn nested_route_path_keeps_segment_separators() {
    let paths = gen_route_list(NestedApp)
        .into_iter()
        .map(|r| r.path().to_string())
        .collect::<Vec<_>>();
    assert_eq!(paths, vec!["/outer/inner".to_string()]);
}
