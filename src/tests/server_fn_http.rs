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
