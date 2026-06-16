use super::*;
use crate::{handle_server_fns, register_explicit, register_leptos_routes};
use ntex::http::{StatusCode, header};
use ntex::web::{App as NtexApp, test};
use server_fn::{ServerFn, redirect::REDIRECT_HEADER};

#[ntex::test]
async fn handles_server_fn_post() {
    register_explicit::<EchoName>();
    register_explicit::<RedirectToAbout>();
    let routes = gen_route_list(UnitApp);
    let app = test::init_service(
        NtexApp::new()
            .route("/api/{tail}*", handle_server_fns())
            .configure(|cfg| {
                register_leptos_routes(cfg, routes.clone(), unit_shell);
            }),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(EchoName::PATH)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .set_payload("name=Alice")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = test::read_body(resp).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("Hello, Alice"));
}

/// `NtexRequest::try_into_stream` enforces the payload limit incrementally as
/// chunks arrive (`cumulative > limit`) — the only overflow guard for a chunked
/// streaming body, whose total size the up-front `Content-Length` preflight
/// cannot see. Pin that boundary from the passing side: a body exactly AT the
/// limit and one strictly UNDER it must both stream through to the server fn
/// and succeed with the drained byte count.
///
/// Kill matrix for the `>` on `request.rs:122` (`if next > limit`):
/// - at-limit (16 of 16 bytes, `16 > 16` is false → success) kills `> -> ==`
///   and `> -> >=` (both would treat the equal case as overflow → 413);
/// - under-limit (8 of 16 bytes) kills `> -> <` (which would reject every
///   non-empty body below the limit → 413).
///
/// The at-limit body clears the `declared > limit` Content-Length preflight
/// (`16 > 16` is false), so it genuinely reaches and exercises line 122.
#[ntex::test]
async fn streaming_input_payload_limit_boundary() {
    use crate::LeptosServerFnConfig;

    register_explicit::<DrainStreamingInput>();
    let app = test::init_service(
        NtexApp::new()
            .state(LeptosServerFnConfig {
                payload_limit: 16,
                ..Default::default()
            })
            .route("/api/{tail}*", handle_server_fns()),
    )
    .await;

    // UNDER the limit: 8 of 16 bytes.
    let under = test::TestRequest::post()
        .uri(DrainStreamingInput::PATH)
        .header("Content-Type", "application/octet-stream")
        .header("Accept", "application/json")
        .set_payload("AAAAAAAA")
        .to_request();
    let resp = test::call_service(&app, under).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "an under-limit streaming body must drain successfully, not 413"
    );
    let body = test::read_body(resp).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        text.contains('8'),
        "must report the 8 drained bytes, got: {text}"
    );

    // AT the limit: exactly 16 of 16 bytes.
    let at_limit = test::TestRequest::post()
        .uri(DrainStreamingInput::PATH)
        .header("Content-Type", "application/octet-stream")
        .header("Accept", "application/json")
        .set_payload("AAAAAAAAAAAAAAAA")
        .to_request();
    let resp = test::call_service(&app, at_limit).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a body whose size equals the limit must succeed (limit is inclusive), not 413"
    );
    let body = test::read_body(resp).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        text.contains("16"),
        "must report the 16 drained bytes, got: {text}"
    );
}

/// The `&mut ServiceConfig` impl registers server functions ITSELF (its
/// `server_fn_paths()` loop, guarded by `!excluded.contains(path)`), so a
/// server fn must resolve through `register_leptos_routes` ALONE — WITHOUT
/// a separate `handle_server_fns()` catch-all. `handles_server_fn_post`
/// above mounts that catch-all, which masks this registration path; here we
/// drop it. Inverting the guard (`delete !`) would register only excluded
/// paths, leaving this non-excluded server fn unrouted → 404.
#[ntex::test]
async fn service_config_registers_server_fns_without_a_catch_all() {
    register_explicit::<EchoName>();
    let routes = gen_route_list(UnitApp);
    let app = test::init_service(NtexApp::new().configure(|cfg| {
        register_leptos_routes(cfg, routes.clone(), unit_shell);
    }))
    .await;

    let req = test::TestRequest::post()
        .uri(EchoName::PATH)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .set_payload("name=Bob")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = test::read_body(resp).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("Hello, Bob"));
}

#[ntex::test]
async fn server_fn_redirect_sets_http_redirect_for_html_form() {
    register_explicit::<EchoName>();
    register_explicit::<RedirectToAbout>();
    let routes = gen_route_list(UnitApp);
    let app = test::init_service(
        NtexApp::new()
            .route("/api/{tail}*", handle_server_fns())
            .configure(|cfg| {
                register_leptos_routes(cfg, routes.clone(), unit_shell);
            }),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(RedirectToAbout::PATH)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "text/html")
        .set_payload("")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert_eq!(
        resp.headers().get("location").and_then(|v| v.to_str().ok()),
        Some("/about")
    );
}

#[ntex::test]
async fn server_fn_redirect_sets_client_redirect_header_for_xhr() {
    register_explicit::<EchoName>();
    register_explicit::<RedirectToAbout>();
    register_explicit::<EchoWebsocket>();
    let routes = gen_route_list(UnitApp);
    let app = test::init_service(
        NtexApp::new()
            .route("/api/{tail}*", handle_server_fns())
            .configure(|cfg| {
                register_leptos_routes(cfg, routes.clone(), unit_shell);
            }),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(RedirectToAbout::PATH)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .set_payload("")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("location").and_then(|v| v.to_str().ok()),
        Some("/about")
    );
    assert!(resp.headers().contains_key(REDIRECT_HEADER));
}

/// A redirect target that is not a valid HTTP header value (e.g. one
/// carrying a CR/LF/control byte, as an attacker-influenced `?next=` or
/// `<Redirect>` path might) must degrade gracefully: no Location, no
/// status change, and crucially no panic in the request handler. Contrast
/// with a valid path, which still sets Location + 302 for an HTML form.
#[ntex::test]
async fn redirect_with_invalid_path_degrades_without_panic() {
    use leptos::context::provide_context;
    use leptos::reactive::owner::Owner;

    let mock_req = test::TestRequest::with_uri("/")
        .header("Accept", "text/html")
        .to_http_request();

    // Invalid path: a newline is rejected by `HeaderValue::from_str`.
    let owner = Owner::new();
    owner.with(|| {
        provide_context(crate::request::Request::new(&mock_req));
        let res_options = crate::ResponseOptions::default();
        provide_context(res_options.clone());

        crate::redirect("/about\r\nX-Injected: 1");

        let parts = res_options.0.read().unwrap();
        assert!(
            parts.headers.get(ntex::http::header::LOCATION).is_none(),
            "invalid redirect path must not set a Location header"
        );
        assert!(
            parts.status.is_none(),
            "invalid redirect path must not change the status"
        );
    });

    // Valid path: still behaves correctly (Location + 302 for HTML).
    let owner = Owner::new();
    owner.with(|| {
        provide_context(crate::request::Request::new(&mock_req));
        let res_options = crate::ResponseOptions::default();
        provide_context(res_options.clone());

        crate::redirect("/about");

        let parts = res_options.0.read().unwrap();
        assert_eq!(
            parts
                .headers
                .get(ntex::http::header::LOCATION)
                .and_then(|v| v.to_str().ok()),
            Some("/about")
        );
        assert_eq!(parts.status, Some(StatusCode::FOUND));
    });
}

#[ntex::test]
async fn server_fn_html_form_falls_back_to_same_origin_referrer() {
    register_explicit::<EchoName>();
    let app = test::init_service(NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let req = test::TestRequest::post()
        .uri(EchoName::PATH)
        .header(header::HOST, "example.test:8080")
        .header(header::REFERER, "http://example.test:8080/form")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "text/html")
        .set_payload("name=Alice")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert_eq!(
        resp.headers().get("location").and_then(|v| v.to_str().ok()),
        Some("/form")
    );
}

#[ntex::test]
async fn server_fn_html_form_does_not_fallback_to_different_port_referrer() {
    register_explicit::<EchoName>();
    let app = test::init_service(NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let req = test::TestRequest::post()
        .uri(EchoName::PATH)
        .header(header::HOST, "example.test:8080")
        .header(header::REFERER, "http://example.test:9090/form")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "text/html")
        .set_payload("name=Alice")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("location").is_none());
}

#[ntex::test]
async fn server_fn_html_form_does_not_fallback_to_protocol_relative_referrer() {
    register_explicit::<EchoName>();
    let app = test::init_service(NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let req = test::TestRequest::post()
        .uri(EchoName::PATH)
        .header(header::HOST, "example.test")
        .header(header::REFERER, "//attacker.test/form")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "text/html")
        .set_payload("name=Alice")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("location").is_none());
}

#[ntex::test]
async fn server_fn_html_form_does_not_fallback_to_different_scheme_referrer() {
    register_explicit::<EchoName>();
    let app = test::init_service(NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let req = test::TestRequest::post()
        .uri(EchoName::PATH)
        .header(header::HOST, "example.test")
        .header("X-Forwarded-Proto", "https")
        .header(header::REFERER, "http://example.test/form")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "text/html")
        .set_payload("name=Alice")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("location").is_none());
}

/// The `Accept` axis must not open a hole in the same-origin invariant.
/// The integration's strict `Accept` parser rejects `text/html;q=0`, so
/// its own referer-repair block is skipped — but server_fn's form-redirect
/// fallback gates on a loose `contains("text/html")` and would inject the
/// raw cross-origin referer as `Location`. The wholesale same-origin guard
/// must still strip it. (Sibling of the `Accept: text/html` cross-port
/// case above, varying only the `Accept` value.)
#[ntex::test]
async fn server_fn_html_form_with_html_q_zero_accept_does_not_leak_cross_origin_referrer() {
    register_explicit::<EchoName>();
    let app = test::init_service(NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let req = test::TestRequest::post()
        .uri(EchoName::PATH)
        .header(header::HOST, "example.test:8080")
        .header(header::REFERER, "http://example.test:9090/form")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "text/html;q=0")
        .set_payload("name=Alice")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(
        !resp.status().is_redirection(),
        "must not redirect, got {}",
        resp.status()
    );
    let location = resp.headers().get("location").and_then(|v| v.to_str().ok());
    assert!(
        location.is_none_or(|l| l.starts_with('/')),
        "Location must never carry a cross-origin target, got {location:?}"
    );
}

/// Same-origin sibling of the cross-origin q=0 leak test above, varying only
/// the Referer's origin. `text/html;q=0` explicitly REFUSES HTML, so even
/// though server_fn's loose `contains("text/html")` fallback emits a
/// *same-origin* form redirect, the integration must drop it and return the
/// structured response — a programmatic client that refuses HTML must not be
/// bounced through a browser redirect. The `Accept: text/html` counterpart
/// still redirects (`server_fn_html_form_falls_back_to_same_origin_referrer`),
/// which is what pins this as the q=0-specific behavior.
#[ntex::test]
async fn server_fn_html_form_with_html_q_zero_accept_does_not_redirect_to_same_origin_referrer() {
    register_explicit::<EchoName>();
    let app = test::init_service(NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let req = test::TestRequest::post()
        .uri(EchoName::PATH)
        .header(header::HOST, "example.test:8080")
        .header(header::REFERER, "http://example.test:8080/form")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "text/html;q=0")
        .set_payload("name=Alice")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "q=0 refuses HTML; the same-origin form redirect must not survive, got {}",
        resp.status()
    );
    assert!(
        resp.headers().get("location").is_none(),
        "a client refusing HTML must not receive a form Location"
    );
}

/// Error sibling of the q=0 same-origin test above. On the ERROR path server_fn
/// builds `Location = Referer + ?__path=…&__err=…`, which is NOT an exact
/// referer echo — a `location_matches_referrer`-based strip misses it and the
/// same-origin guard would otherwise preserve the `302`. A client that refuses
/// HTML must still get the structured `500`, never a browser redirect. (Pins
/// the Codex-found gap: the q=0 strip keys on "is the response a form redirect
/// at all", not on an exact referer match.)
#[ntex::test]
async fn server_fn_html_form_with_html_q_zero_accept_does_not_redirect_on_error() {
    register_explicit::<AlwaysErr>();
    let app = test::init_service(NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let req = test::TestRequest::post()
        .uri(AlwaysErr::PATH)
        .header(header::HOST, "example.test:8080")
        .header(header::REFERER, "http://example.test:8080/form")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "text/html;q=0")
        .set_payload("")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "q=0 refuses HTML; an erroring form post must return the structured error, not a 302, got {}",
        resp.status()
    );
    assert!(
        resp.headers().get("location").is_none(),
        "a client refusing HTML must not receive a form Location, even on the error path"
    );
}

/// A user middleware attached to a server fn can short-circuit the request with
/// its OWN response — e.g. an auth guard issuing a `302` to a login page —
/// before the inner server fn (and server_fn's form-redirect fallback) ever
/// runs. `dispatch_server_fn`'s referer-redirect sanitization must not mistake
/// that 3xx for the form fallback and strip it: the fallback's `Location` is
/// derived from the Referer and only fires on a loose `text/html` `Accept`,
/// whereas this redirect targets `/login` regardless. Regression for the Codex
/// P2 finding — the prior `is_redirection()` guard corrupted any non-HTML 3xx
/// from middleware into a `200`/`500`.
#[ntex::test]
async fn middleware_redirect_survives_for_non_html_client() {
    register_explicit::<GuardedByRedirect>();
    let app = test::init_service(NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    // The Codex example verbatim: a JSON (non-HTML) client. server_fn's form
    // fallback cannot fire (no `text/html` in `Accept`), so the only 3xx is the
    // middleware's — it must reach the client unchanged.
    let json = test::TestRequest::post()
        .uri(GuardedByRedirect::PATH)
        .header("Accept", "application/json")
        .set_payload("")
        .to_request();
    let resp = test::call_service(&app, json).await;
    assert_eq!(
        resp.status(),
        StatusCode::FOUND,
        "a middleware auth redirect must survive for a JSON client, not collapse to 200/500"
    );
    assert_eq!(
        resp.headers().get("location").and_then(|v| v.to_str().ok()),
        Some("/login"),
        "the middleware's own Location must be preserved"
    );

    // Even with a loose `text/html` Accept AND a Referer present, a redirect
    // whose Location is NOT referer-derived (here `/login`, not the Referer) is
    // the middleware's, not the form fallback — so it must still survive. This
    // pins the referer-derived guard, not merely the Accept guard.
    let q_zero = test::TestRequest::post()
        .uri(GuardedByRedirect::PATH)
        .header(header::HOST, "example.test:8080")
        .header(header::REFERER, "http://example.test:8080/dashboard")
        .header("Accept", "text/html;q=0")
        .set_payload("")
        .to_request();
    let resp = test::call_service(&app, q_zero).await;
    assert_eq!(
        resp.status(),
        StatusCode::FOUND,
        "a middleware redirect to a non-referer target must survive even under text/html;q=0"
    );
    assert_eq!(
        resp.headers().get("location").and_then(|v| v.to_str().ok()),
        Some("/login")
    );

    // The other 3xx Codex called out: a conditional `304 Not Modified` from a
    // caching middleware. It carries no `Location`, so it can never match the
    // referer-derived form-redirect shape — it must pass through untouched
    // rather than being reset to `200`.
    register_explicit::<GuardedByNotModified>();
    let not_modified = test::TestRequest::post()
        .uri(GuardedByNotModified::PATH)
        .header("Accept", "application/json")
        .set_payload("")
        .to_request();
    let resp = test::call_service(&app, not_modified).await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_MODIFIED,
        "a middleware 304 must survive for a JSON client, not collapse to 200"
    );
}

/// When a server function **errors** on an HTML form post, server_fn rewrites
/// the response into a `302` back to the Referer with the error encoded in the
/// query. With a cross-origin Referer the same-origin guard must strip that
/// unsafe `Location` WITHOUT promoting the failure to `200 OK` — the caller
/// still has to observe a server error. Sibling of the successful cross-origin
/// cases above, varying the server-fn OUTCOME (Err vs Ok).
#[ntex::test]
async fn server_fn_html_form_error_redirect_strip_preserves_error_status() {
    register_explicit::<AlwaysErr>();
    let app = test::init_service(NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let req = test::TestRequest::post()
        .uri(AlwaysErr::PATH)
        .header(header::HOST, "example.test:8080")
        .header(header::REFERER, "http://evil.test/form")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "text/html")
        .set_payload("")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "stripping an unsafe error redirect must preserve the server-fn failure status, got {}",
        resp.status()
    );
    assert!(
        resp.headers().get("location").is_none(),
        "the unsafe cross-origin error Location must be stripped"
    );
}

/// Same-origin sibling of the cross-origin error case above (C3), varying the
/// Referer's origin. A SAME-origin form-error redirect is legitimate — the form
/// page shows the error — so the `302` back to the full referer must be
/// PRESERVED, not stripped and not normalized to a bare path. Pins both halves
/// of the redirect machinery: `NtexServerResponse::redirect` must actually set
/// the `302`+`Location` (a no-op there drops it to a non-redirect), and
/// `location_matches_referrer` must NOT treat the upstream error-query URL as a
/// plain referer echo (a blanket match would rewrite it to bare "/form").
#[ntex::test]
async fn server_fn_html_form_same_origin_error_redirect_is_preserved() {
    register_explicit::<AlwaysErr>();
    let app = test::init_service(NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let req = test::TestRequest::post()
        .uri(AlwaysErr::PATH)
        .header(header::HOST, "example.test:8080")
        .header(header::REFERER, "http://example.test:8080/form")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "text/html")
        .set_payload("")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::FOUND,
        "a same-origin form error must stay a 302 back to the form, got {}",
        resp.status()
    );
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    assert!(
        location
            .as_deref()
            .is_some_and(|l| l.starts_with("http://example.test:8080/form")),
        "the same-origin error redirect must preserve the full referer URL, got {location:?}"
    );
}

/// `NtexServerResponse::content_type` (the `Res::content_type` setter) is only
/// reached on the server-fn ERROR path, where server_fn sets the error
/// encoding's media type after `error_response`. A no-op there would leave the
/// error body with no `Content-Type`, so pin it: an erroring server fn must
/// report `text/plain` (the `ServerFnError` encoder's `CONTENT_TYPE`). No
/// Referer/Accept here, so no form redirect intervenes — we observe the raw
/// error response.
#[ntex::test]
async fn server_fn_error_response_sets_error_encoder_content_type() {
    register_explicit::<AlwaysErr>();
    let app = test::init_service(NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let req = test::TestRequest::post()
        .uri(AlwaysErr::PATH)
        .set_payload("")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    assert_eq!(
        content_type,
        Some("text/plain"),
        "the server-fn error encoder's Content-Type must be set on the error response"
    );
}

/// Oversize request detected via streaming (no Content-Length
/// preflight, because `set_payload` doesn't set the header — the
/// limit trips mid-read in `collect_payload` and is promoted to 413
/// by the `PayloadTooLarge` extension marker.
#[ntex::test]
async fn payload_limit_streaming_overflow_returns_413() {
    use crate::LeptosServerFnConfig;
    register_explicit::<EchoName>();
    let app = test::init_service(
        NtexApp::new()
            .state(LeptosServerFnConfig {
                payload_limit: 10,
                ws_channel_buffer: 32,
                ..Default::default()
            })
            .route("/api/{tail}*", handle_server_fns()),
    )
    .await;

    let oversized = format!("name={}", "A".repeat(100));
    let req = test::TestRequest::post()
        .uri(EchoName::PATH)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .set_payload(oversized)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = test::read_body(resp).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("10"), "body should mention limit: {text:?}");
}

/// Oversize request declared via `Content-Length` is rejected by the
/// preflight, *before* any body bytes are read. The test uses a
/// deliberately impossible CL value and a tiny body; if the preflight
/// did not run, `collect_payload` would still succeed (body is small)
/// and we would incorrectly return 200.
#[ntex::test]
async fn payload_limit_content_length_preflight_returns_413() {
    use crate::LeptosServerFnConfig;
    register_explicit::<EchoName>();
    let app = test::init_service(
        NtexApp::new()
            .state(LeptosServerFnConfig {
                payload_limit: 32,
                ws_channel_buffer: 32,
                ..Default::default()
            })
            .route("/api/{tail}*", handle_server_fns()),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(EchoName::PATH)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .header("Content-Length", "999999")
        .set_payload("name=Bob")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[ntex::test]
async fn payload_limit_accepts_small_body() {
    use crate::LeptosServerFnConfig;
    register_explicit::<EchoName>();
    let app = test::init_service(
        NtexApp::new()
            .state(LeptosServerFnConfig {
                payload_limit: 1024,
                ws_channel_buffer: 32,
                ..Default::default()
            })
            .route("/api/{tail}*", handle_server_fns()),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(EchoName::PATH)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .set_payload("name=Bob")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("Hello, Bob"));
}

#[ntex::test]
async fn singleton_location_header_is_replaced_through_res_options() {
    register_explicit::<MultiLocation>();
    let app = test::init_service(NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let req = test::TestRequest::post()
        .uri(MultiLocation::PATH)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .set_payload("")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let locations: Vec<_> = resp
        .headers()
        .get_all(ntex::http::header::LOCATION)
        .filter_map(|v| v.to_str().ok())
        .collect();
    assert_eq!(locations, vec!["/two"]);
}

#[ntex::test]
async fn extract_helper_reads_request_path() {
    register_explicit::<ProbePath>();
    let app = test::init_service(NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let req = test::TestRequest::post()
        .uri(ProbePath::PATH)
        .header("Accept", "application/json")
        .set_payload("")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = test::read_body(resp).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("/api/probe_path"));
}

/// `register_explicit` is idempotent per `(path, method)`: registering the
/// same server function any number of times — on top of the `inventory`
/// entry already installed at startup — leaves exactly ONE registration.
/// Without dedup, native targets accumulate duplicates (and
/// `server_fn_paths()` emits one row per call).
#[test]
fn registering_a_server_fn_twice_keeps_a_single_registration() {
    use crate::server_fn_paths;

    register_explicit::<EchoName>();
    register_explicit::<EchoName>();
    register_explicit::<EchoName>();

    let echo_post = server_fn_paths()
        .filter(|(p, m)| *p == EchoName::PATH && *m == ntex::http::Method::POST)
        .count();
    assert_eq!(
        echo_post, 1,
        "a repeated register_explicit must not accumulate duplicate entries"
    );
}

#[ntex::test]
async fn server_fn_paths_and_get_service_roundtrip() {
    use crate::{get_server_fn_service, server_fn_paths};

    register_explicit::<EchoName>();
    register_explicit::<RedirectToAbout>();

    let paths: Vec<_> = server_fn_paths().collect();
    assert!(paths.iter().any(|(p, _)| *p == EchoName::PATH));
    assert!(paths.iter().any(|(p, _)| *p == RedirectToAbout::PATH));

    let found = get_server_fn_service(EchoName::PATH, &ntex::http::Method::POST);
    assert!(found.is_some());

    let not_found = get_server_fn_service(EchoName::PATH, &ntex::http::Method::GET);
    assert!(not_found.is_none());

    let missing = get_server_fn_service("/api/does_not_exist", &ntex::http::Method::POST);
    assert!(missing.is_none());
}

// -- Payload boundary tests ----------------------------------------

/// Payload exactly at the limit must be accepted (the check is `>`,
/// not `>=`).
#[ntex::test]
async fn payload_limit_exactly_at_limit_succeeds() {
    use crate::LeptosServerFnConfig;
    register_explicit::<EchoName>();
    let limit = 32;
    let app = test::init_service(
        NtexApp::new()
            .state(LeptosServerFnConfig {
                payload_limit: limit,
                ws_channel_buffer: 32,
                ..Default::default()
            })
            .route("/api/{tail}*", handle_server_fns()),
    )
    .await;

    // "name=X" is 6 bytes overhead; pad the value so total == limit.
    let value = "A".repeat(limit - "name=".len());
    let body = format!("name={value}");
    assert_eq!(body.len(), limit);

    let req = test::TestRequest::post()
        .uri(EchoName::PATH)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .set_payload(body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// Payload one byte over the limit must be rejected with 413.
#[ntex::test]
async fn payload_limit_one_over_limit_returns_413() {
    use crate::LeptosServerFnConfig;
    register_explicit::<EchoName>();
    let limit = 32;
    let app = test::init_service(
        NtexApp::new()
            .state(LeptosServerFnConfig {
                payload_limit: limit,
                ws_channel_buffer: 32,
                ..Default::default()
            })
            .route("/api/{tail}*", handle_server_fns()),
    )
    .await;

    let value = "A".repeat(limit - "name=".len() + 1);
    let body = format!("name={value}");
    assert_eq!(body.len(), limit + 1);

    let req = test::TestRequest::post()
        .uri(EchoName::PATH)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .set_payload(body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}
