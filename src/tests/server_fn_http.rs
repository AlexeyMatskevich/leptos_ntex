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
    assert_eq!(
        text.trim().parse::<usize>().ok(),
        Some(8),
        "must report EXACTLY 8 drained bytes (a substring check passes for 18/80/108), got: {text:?}"
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
    assert_eq!(
        text.trim().parse::<usize>().ok(),
        Some(16),
        "must report EXACTLY 16 drained bytes (a substring check passes for 116/160), got: {text:?}"
    );
}

/// The OVER-limit side of the streaming-input boundary: a body past the limit,
/// sent WITHOUT a `Content-Length` (so the up-front preflight can't see it),
/// must trip `try_into_stream`'s per-chunk guard (`next > limit`), stash the
/// `PayloadTooLarge` marker, and surface as a `413`. This is the streaming-INPUT
/// overflow path — distinct from the buffered `collect_payload` path the other
/// 413 tests exercise — and the boundary test above never actually crosses the
/// limit, so without this a `> -> always-false` regression on `request.rs:122`
/// would ship undetected.
#[ntex::test]
async fn streaming_input_over_limit_returns_413() {
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

    let req = test::TestRequest::post()
        .uri(DrainStreamingInput::PATH)
        .header("Content-Type", "application/octet-stream")
        .header("Accept", "application/json")
        .set_payload("A".repeat(100))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "an over-limit streaming body must 413 via the per-chunk guard, not drain"
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
/// status change, and crucially no panic in the request handler.
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
}

/// Contrast with the invalid-path degrade case above: a VALID redirect path
/// still sets Location + 302 for an HTML form. Kept as its own test (rather
/// than a second assertion block bundled into the degrade test) so a CI
/// failure here is correctly attributed to the success path, not misread as
/// a regression in the invalid-path handling.
#[ntex::test]
async fn redirect_with_valid_path_still_sets_location_and_302() {
    use leptos::context::provide_context;
    use leptos::reactive::owner::Owner;

    let mock_req = test::TestRequest::with_uri("/")
        .header("Accept", "text/html")
        .to_http_request();

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

// NOTE: this is ALSO the success-path exercise of `dispatch_server_fn`'s
// referer-echo branch (handlers.rs:85-88, `location_is_referrer == true`
// with `referrer.clone()` present): server_fn's OWN `run_on_server` always
// calls `res.redirect(referer)` when `accepts_html` — for both `Ok` and
// `Err` outcomes — so `EchoName` (an Ok-returning fn with no explicit
// redirect of its own) already has server_fn set `Location` to the exact
// Referer before `dispatch_server_fn` ever runs its post-processing. That
// makes `location_is_referrer` true and drives the SUCCESS sibling of the
// branch pinned on the error side by
// `server_fn_html_form_same_origin_error_redirect_is_preserved` — replacing
// `referrer.clone()` with the raw, un-normalized `Referer` header (e.g.
// dropping the `same_origin_location` normalization) fails this test's
// exact-location assertion below.
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

/// The genuine "nothing to fall back to" case: an HTML-accepting client that
/// sends NO `Referer` header at all. Every other referer-fallback test in
/// this file sends a Referer (matching-origin or wrong-origin); none omits
/// it entirely while still requesting `Accept: text/html` against an `Ok`,
/// no-redirect server fn.
///
/// With no Referer, server_fn's OWN `run_on_server` still calls
/// `res.redirect(referer.as_deref().unwrap_or("/"))` for any HTML-accepting
/// request — Ok or Err — so the response is a `302` to the bare root `/`,
/// NOT a plain `200`. `raw_referrer` is `None` on our side, so
/// `location_is_referrer` is false and the bare-200 same-origin fallback
/// (handlers.rs:93-99) never fires either; the `/` root IS same-origin, so
/// the wholesale same-origin guard leaves it untouched. This pins that
/// exact shape: a regression that suppressed this fallback (e.g. treating
/// `referer.unwrap_or("/")`'s default target as somehow un-routable) would
/// change the observed status/Location here.
#[ntex::test]
async fn server_fn_html_form_with_no_referer_redirects_to_bare_root() {
    register_explicit::<EchoName>();
    let app = test::init_service(NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let req = test::TestRequest::post()
        .uri(EchoName::PATH)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "text/html")
        .set_payload("name=Alice")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::FOUND,
        "server_fn's own form-redirect fallback defaults to the bare root when no Referer is present"
    );
    assert_eq!(
        resp.headers().get("location").and_then(|v| v.to_str().ok()),
        Some("/"),
        "with no Referer present, the fallback target is the bare root '/'"
    );
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
    // Positive anchor first: the server fn must actually have dispatched and
    // its (stripped) form redirect must have been reset to `200 OK`. Without
    // this, the Location-safety assertions pass VACUOUSLY for an undispatched
    // `400` (no route) or a degraded `500`, which carry no cross-origin
    // Location either.
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the server fn must dispatch; the stripped cross-origin redirect resets to 200"
    );
    let location = resp.headers().get("location").and_then(|v| v.to_str().ok());
    assert!(
        location.is_none_or(|l| l.starts_with('/')),
        "Location must never carry a cross-origin target, got {location:?}"
    );
}

/// Same-origin sibling of the cross-origin q=0 leak test above. A SAME-origin
/// form redirect for a `text/html;q=0` client is deliberately LEFT INTACT:
/// server_fn's form-redirect fallback fires on a loose `contains("text/html")`,
/// and it is driven solely by `req.accepts()` — which user middleware shares —
/// so there is no way to suppress only the fallback without corrupting a
/// middleware's own redirect. This integration therefore matches `leptos_axum` /
/// `leptos_actix` and lets the same-origin `302` stand; only the cross-origin
/// leak (test above) is stripped, by the same-origin guard. The real fix belongs
/// upstream (tighten server_fn's loose `Accept` check).
#[ntex::test]
async fn server_fn_html_form_with_html_q_zero_accept_still_redirects_to_same_origin_referrer() {
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
        StatusCode::FOUND,
        "a same-origin form redirect is preserved (upstream-aligned), got {}",
        resp.status()
    );
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        location.starts_with("http://example.test:8080/form"),
        "the redirect must target the same-origin referer, got {location:?}"
    );
}

/// Error sibling of the q=0 same-origin test above. On the ERROR path server_fn
/// builds `Location = Referer + ?__path=…&__err=…` and a `302`. For a SAME-origin
/// referer this is left intact, exactly like the success case — the error rides
/// in the redirect URL, as `leptos_axum` does. (The CROSS-origin error redirect
/// IS stripped, and preserves the `500` — see
/// `server_fn_html_form_error_redirect_strip_preserves_error_status`.)
#[ntex::test]
async fn server_fn_html_form_with_html_q_zero_accept_still_redirects_on_error() {
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
        StatusCode::FOUND,
        "a same-origin form-error redirect is preserved (upstream-aligned), got {}",
        resp.status()
    );
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        location.starts_with("http://example.test:8080/form"),
        "the error redirect must target the same-origin referer (carrying the error query), got {location:?}"
    );
    // ...and the error must actually RIDE in the redirect URL: a Location that
    // dropped the encoded query (bare referer) would pass the prefix check
    // alone but lose the failure payload the form page needs.
    assert!(
        location.contains("__path=") && location.contains("__err="),
        "the error redirect must carry server_fn's encoded error query, got {location:?}"
    );
}

/// A user middleware attached to a server fn can short-circuit the request with
/// its OWN response — e.g. an auth guard issuing a `302` to a login page —
/// before the inner server fn ever runs. `dispatch_server_fn` must leave that
/// `3xx` alone. `dispatch_server_fn` has NO non-HTML redirect-stripping branch:
/// the only post-run intervention is the cross-origin same-origin guard, and a
/// same-origin `Location` (here `/login`) passes it untouched. Regression for a
/// chain of three Codex P2 findings — the original `is_redirection()` guard
/// corrupted any non-HTML `3xx`, its referer-derived replacement still caught a
/// middleware redirect back to the Referer, and the `accepts()`-source variant
/// hid non-HTML media types from middleware. Removing the strip entirely (and
/// matching upstream on the `q=0` form redirect) is what finally keeps this
/// green without collateral.
#[ntex::test]
async fn middleware_redirect_survives_for_non_html_client() {
    register_explicit::<GuardedByRedirect>();
    let app = test::init_service(NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    // The Codex example verbatim: a JSON (non-HTML) client. The middleware
    // short-circuits, so the only 3xx is its own same-origin `/login` — and with
    // no strip branch it reaches the client unchanged.
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

    // The case the second Codex finding called out: a loose `text/html` Accept
    // (`q=0`) WITH a Referer present — exactly when server_fn's fallback WOULD
    // fire had the inner fn run. The middleware short-circuits so the fallback
    // never runs, and its same-origin `/login` (not the Referer) passes the
    // cross-origin guard untouched.
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
    let location = location.unwrap_or_default();
    assert!(
        location.contains("__path=") && location.contains("__err="),
        "the same-origin error redirect must preserve server_fn's encoded error query, got {location:?}"
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
    // Exact match, not `.contains("10")`: the configured limit (10) is a
    // fixed, known value here, so the full production format string
    // (`oversize_response`'s `"payload exceeds limit of {limit} bytes"`) is
    // fully determined. A substring check would also pass for a regression
    // that reported the observed body size instead of the configured limit
    // (e.g. "...limit of 105 bytes", which still contains "10").
    assert_eq!(text, "payload exceeds limit of 10 bytes");
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

/// `handle_server_fns_with_context`'s else-branch has two HTTP-level
/// outcomes when `get_server_fn_service` returns `None`: a 400 with a
/// diagnostic body when NO path matches at all (`allowed.is_empty()`), or a
/// 405 with an `Allow` header when the path matches but the method doesn't
/// (`catchall_handle_server_fns_returns_405_on_method_mismatch` in
/// `tests/integration.rs`). Only the 405 sibling was ever driven through the
/// actual route; this pins the negative direction: a wholly unregistered
/// path must 400 and name the requested route in the body.
#[ntex::test]
async fn catchall_returns_400_for_a_wholly_unregistered_path() {
    let app = test::init_service(NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let req = test::TestRequest::post()
        .uri("/api/does_not_exist")
        .set_payload("")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body = test::read_body(resp).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
    // Exact body from `handle_server_fns_with_context`'s `allowed.is_empty()` branch.
    assert_eq!(
        text,
        "Could not find a server function at the route /api/does_not_exist. \n\nIt's likely that either\n1. The API prefix you specify in the `#[server]` macro doesn't match the prefix at which your server function handler is mounted, or\n2. You are on a platform that doesn't support automatic server function registration and you need to call register_explicit() on the server function type, somewhere in your `main` function."
    );
}

/// Sibling of the 400 test above: a GET to a POST-only registered server fn
/// must 405 with an EXACT `Allow: POST` header — the direct HTTP-level
/// analog of `catchall_handle_server_fns_returns_405_on_method_mismatch` in
/// `tests/integration.rs`, driven here against `EchoName` through
/// `handle_server_fns()` in this file's own app-building style.
#[ntex::test]
async fn catchall_returns_405_with_allow_header_on_method_mismatch() {
    register_explicit::<EchoName>();
    let app = test::init_service(NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let req = test::TestRequest::get().uri(EchoName::PATH).to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        resp.headers()
            .get(header::ALLOW)
            .and_then(|v| v.to_str().ok()),
        Some("POST"),
        "the Allow header must name exactly the registered method"
    );
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

// -- GET-method server fn dispatch ----------------------------------

/// A GET-method server fn (`input = GetUrl`): every other fixture driven
/// through `handle_server_fns()`/`register_leptos_routes` in this file uses
/// the `#[server]` macro's default POST method, so no test here ever drove
/// an actual GET request through either handler's method-dispatch logic —
/// the only GET-related assertion elsewhere is a pure registry-level lookup
/// (`server_fn_paths_and_get_service_roundtrip`), which never touches HTTP
/// dispatch at all.
#[server(
    name = EchoNameGet,
    prefix = "/api",
    endpoint = "echo_name_get",
    input = server_fn::codec::GetUrl,
    server = crate::NtexServerFnBackend
)]
async fn echo_name_get(name: String) -> Result<String, ServerFnError> {
    Ok(format!("Hello, {name}"))
}

/// Closes the method-dispatch gap: a GET request to a GET-registered server
/// fn must dispatch through `handle_server_fns()` and return the expected
/// body — pinning that `.method(server_fn.method())` (and the equivalent
/// generic `req.method()` branch in the catchall) is not hardcoded/defaulted
/// to POST.
#[ntex::test]
async fn handles_server_fn_get() {
    register_explicit::<EchoNameGet>();
    let app = test::init_service(NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

    let req = test::TestRequest::get()
        .uri(&format!("{}?name=Carol", EchoNameGet::PATH))
        .header("Accept", "application/json")
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = test::read_body(resp).await;
    let text = String::from_utf8(body.to_vec()).unwrap();
    // Exact JSON-encoded server_fn output, not a substring.
    assert_eq!(text, "\"Hello, Carol\"");
}
