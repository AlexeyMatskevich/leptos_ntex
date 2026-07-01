use super::*;
use crate::register_leptos_routes;
use ntex::http::{StatusCode, header};
use ntex::web::{App as NtexApp, test};

#[ntex::test]
async fn renders_root_route() {
    // No server-fn setup: these render tests only request rendered routes
    // ('/', '/about'), never an `/api` server fn, so the registration + catchall
    // were dead scaffolding.
    let routes = gen_route_list(UnitApp);
    let app = test::init_service(NtexApp::new().configure(|cfg| {
        register_leptos_routes(cfg, routes.clone(), unit_shell);
    }))
    .await;

    let req = test::TestRequest::with_uri("/").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = test::read_body(resp).await;
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("Leptos over ntex"));
}

#[ntex::test]
async fn renders_about_route() {
    // No server-fn setup: these render tests only request rendered routes
    // ('/', '/about'), never an `/api` server fn, so the registration + catchall
    // were dead scaffolding.
    let routes = gen_route_list(UnitApp);
    let app = test::init_service(NtexApp::new().configure(|cfg| {
        register_leptos_routes(cfg, routes.clone(), unit_shell);
    }))
    .await;

    let req = test::TestRequest::with_uri("/about").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = test::read_body(resp).await;
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("This route is generated from the Leptos router"));
}

/// Regression for the `*splat` route conversion (`to_ntex_path`): ntex
/// must receive the tail match `{any}*`, not actix's single-segment regex
/// `{any:.*}`. The two states of the URL-depth axis:
///   * single segment under the splat — matched under BOTH syntaxes;
///   * nested path under the splat — used to fall through to the router
///     fallback ("Not Found") under `{any:.*}`.
#[ntex::test]
async fn splat_route_serves_nested_paths() {
    use crate::LeptosRoutes;
    let routes = gen_route_list(SplatApp);
    let app = test::init_service(NtexApp::new().leptos_routes(routes, splat_shell)).await;

    for uri in ["/files/report.txt", "/files/a/b/report.txt"] {
        let req = test::TestRequest::with_uri(uri).to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK, "splat must match {uri}");

        let body = test::read_body(resp).await;
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            html.contains("Splat Files"),
            "{uri} must render the splat view, not the fallback: {html}"
        );
    }
}

/// The `:param` sibling of the splat conversion: a brace segment matches
/// exactly one segment — and keeps matching it after the splat fix.
#[ntex::test]
async fn param_route_serves_single_segment() {
    use crate::LeptosRoutes;
    let routes = gen_route_list(SplatApp);
    let app = test::init_service(NtexApp::new().leptos_routes(routes, splat_shell)).await;

    let req = test::TestRequest::with_uri("/users/42").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = test::read_body(resp).await;
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        html.contains("User Param"),
        "param route must render: {html}"
    );

    // The boundary that makes "exactly one segment" meaningful: a TWO-segment
    // URL must NOT match the single-segment `:param` route (it falls through to
    // the fallback). Without this, the positive case alone would also pass a
    // `{id}*`-style multi-segment match.
    let req = test::TestRequest::with_uri("/users/42/extra").to_request();
    let resp = test::call_service(&app, req).await;
    let html = String::from_utf8(test::read_body(resp).await.to_vec()).unwrap();
    assert!(
        !html.contains("User Param"),
        "a 2-segment URL must not match the single-segment :param route: {html}"
    );
}

/// `ServiceConfig` counterpart of `splat_route_serves_nested_paths`: the
/// `&mut ServiceConfig` impl of `LeptosRoutes` (`register_leptos_routes`)
/// registers paths through its own, separately-written match arms
/// (`leptos_routes.rs` lines 184-277), so the splat/nested-path regression
/// must be pinned on THIS registration path too, not just the `App` impl.
#[ntex::test]
async fn splat_route_serves_nested_paths_via_service_config() {
    let routes = gen_route_list(SplatApp);
    let app = test::init_service(NtexApp::new().configure(|cfg| {
        register_leptos_routes(cfg, routes.clone(), splat_shell);
    }))
    .await;

    for uri in ["/files/report.txt", "/files/a/b/report.txt"] {
        let req = test::TestRequest::with_uri(uri).to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK, "splat must match {uri}");

        let body = test::read_body(resp).await;
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            html.contains("Splat Files"),
            "{uri} must render the splat view, not the fallback: {html}"
        );
    }
}

/// `ServiceConfig` counterpart of `param_route_serves_single_segment`: see
/// `splat_route_serves_nested_paths_via_service_config` for why the
/// ServiceConfig registration path needs its own pin.
#[ntex::test]
async fn param_route_serves_single_segment_via_service_config() {
    let routes = gen_route_list(SplatApp);
    let app = test::init_service(NtexApp::new().configure(|cfg| {
        register_leptos_routes(cfg, routes.clone(), splat_shell);
    }))
    .await;

    let req = test::TestRequest::with_uri("/users/42").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = test::read_body(resp).await;
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        html.contains("User Param"),
        "param route must render: {html}"
    );

    let req = test::TestRequest::with_uri("/users/42/extra").to_request();
    let resp = test::call_service(&app, req).await;
    let html = String::from_utf8(test::read_body(resp).await.to_vec()).unwrap();
    assert!(
        !html.contains("User Param"),
        "a 2-segment URL must not match the single-segment :param route: {html}"
    );
}

#[ntex::test]
async fn renders_in_order_ssr_route() {
    let routes = gen_route_list(MixedApp);
    let app = test::init_service(NtexApp::new().configure(|cfg| {
        register_leptos_routes(cfg, routes.clone(), mixed_shell);
    }))
    .await;

    let req = test::TestRequest::with_uri("/in").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = test::read_body(resp).await;
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("InOrder"));
}

#[ntex::test]
async fn renders_async_ssr_route() {
    let routes = gen_route_list(MixedApp);
    let app = test::init_service(NtexApp::new().configure(|cfg| {
        register_leptos_routes(cfg, routes.clone(), mixed_shell);
    }))
    .await;

    let req = test::TestRequest::with_uri("/async").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = test::read_body(resp).await;
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("Async"));
}

// MixedApp's `/out` route (the default/no-suspense `SsrMode::OutOfOrder`
// case) was previously only exercised by the HEAD-parity tests, which check
// status/Content-Type equality but never read the body. Pin the body content
// too, mirroring `renders_in_order_ssr_route` / `renders_async_ssr_route`
// above — a regression specific to the OutOfOrder match arm's own body
// wouldn't otherwise be caught for this fixture.
#[ntex::test]
async fn renders_out_of_order_ssr_route() {
    let routes = gen_route_list(MixedApp);
    let app = test::init_service(NtexApp::new().configure(|cfg| {
        register_leptos_routes(cfg, routes.clone(), mixed_shell);
    }))
    .await;

    let req = test::TestRequest::with_uri("/out").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = test::read_body(resp).await;
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("OutOfOrder"));
}

// `SsrMode::PartiallyBlocked` has its own dispatch arm in `leptos_routes.rs`
// (both the `App` impl and the `&mut ServiceConfig` impl), calling
// `render_app_to_stream_with_context_and_replace_blocks`. Per that function's
// doc, `replace_blocks` is currently a no-op upstream, so PartiallyBlocked
// renders identically to OutOfOrder — but the dispatch arm itself must still
// be reached, not silently fall through to `unsupported_ssr_mode_route`
// (which would 500 instead of rendering the view).
#[component]
fn PartiallyBlockedApp() -> impl IntoView {
    provide_meta_context();
    view! {
        <Router>
            <main>
                <Routes fallback=|| view! { <h1>"Not Found"</h1> }>
                    <Route
                        path=path!("/partial")
                        ssr=SsrMode::PartiallyBlocked
                        view=|| view! { <h1>"PartiallyBlocked"</h1> }
                    />
                </Routes>
            </main>
        </Router>
    }
}

fn partially_blocked_shell() -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <MetaTags/>
            </head>
            <body>
                <PartiallyBlockedApp/>
            </body>
        </html>
    }
}

#[ntex::test]
async fn renders_partially_blocked_ssr_route() {
    let routes = gen_route_list(PartiallyBlockedApp);
    let app = test::init_service(NtexApp::new().configure(|cfg| {
        register_leptos_routes(cfg, routes.clone(), partially_blocked_shell);
    }))
    .await;

    let req = test::TestRequest::with_uri("/partial").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = test::read_body(resp).await;
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        html.contains("PartiallyBlocked"),
        "PartiallyBlocked route must render its view, not fall through to the 500 catch-all: {html}"
    );
}

// ----- SSR streaming mode is observable in the body ------------------
// A `<Suspense>` over a still-pending resource renders differently per
// mode: OutOfOrder streams the shell + fallback first (the body carries
// BOTH the fallback marker and an out-of-order replacement template);
// InOrder/Async block for the resolved value (the body has it in place
// and NO fallback marker). Deleting the InOrder/Async match arm falls
// back to the OutOfOrder renderer, which these assertions catch — across
// both the `App` and the `&mut ServiceConfig` implementations.
// The read guard is deliberately held across the render `.await`s: that is
// the whole point of the workaround (serialize this resource-bearing render
// against any route-generation window). It cannot deadlock — the render path
// never takes the write side — and the `#[ntex::test]` future is single-
// threaded, so the `!Send` guard is fine.
#[allow(clippy::await_holding_lock)]
async fn suspense_body_via_app_impl(path: &str) -> String {
    use crate::LeptosRoutes;
    let routes = gen_route_list(SuspenseApp);
    let app = test::init_service(NtexApp::new().leptos_routes(routes, suspense_shell)).await;
    let req = test::TestRequest::with_uri(path).to_request();
    // Hold the READ side across the render: `SuspendedView` first-polls its
    // `Resource` here, and it must not land inside another test's
    // route-generation window (see `ROUTE_GEN_VS_RENDER`).
    let _render_guard = ROUTE_GEN_VS_RENDER
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let resp = test::call_service(&app, req).await;
    let body = test::read_body(resp).await;
    String::from_utf8(body.to_vec()).unwrap()
}

#[allow(clippy::await_holding_lock)]
async fn suspense_body_via_service_config(path: &str) -> String {
    let routes = gen_route_list(SuspenseApp);
    let app = test::init_service(NtexApp::new().configure(|cfg| {
        register_leptos_routes(cfg, routes.clone(), suspense_shell);
    }))
    .await;
    let req = test::TestRequest::with_uri(path).to_request();
    let _render_guard = ROUTE_GEN_VS_RENDER
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let resp = test::call_service(&app, req).await;
    let body = test::read_body(resp).await;
    String::from_utf8(body.to_vec()).unwrap()
}

#[ntex::test]
async fn in_order_mode_blocks_for_the_resource_via_app_impl() {
    let html = suspense_body_via_app_impl("/in").await;
    assert!(
        html.contains("RESOLVED-CONTENT"),
        "resolved value must be present"
    );
    assert!(
        !html.contains("FALLBACK-MARKER"),
        "InOrder must block for the resource, not stream the OOO fallback"
    );
}

#[ntex::test]
async fn in_order_mode_blocks_for_the_resource_via_service_config() {
    let html = suspense_body_via_service_config("/in").await;
    assert!(html.contains("RESOLVED-CONTENT"));
    assert!(
        !html.contains("FALLBACK-MARKER"),
        "InOrder (ServiceConfig) must block for the resource"
    );
}

#[ntex::test]
async fn async_mode_blocks_for_the_resource_via_app_impl() {
    let html = suspense_body_via_app_impl("/async").await;
    assert!(html.contains("RESOLVED-CONTENT"));
    assert!(
        !html.contains("FALLBACK-MARKER"),
        "Async must resolve everything before sending, not stream the fallback"
    );
}

#[ntex::test]
async fn async_mode_blocks_for_the_resource_via_service_config() {
    let html = suspense_body_via_service_config("/async").await;
    assert!(html.contains("RESOLVED-CONTENT"));
    assert!(
        !html.contains("FALLBACK-MARKER"),
        "Async (ServiceConfig) must resolve everything before sending"
    );
}

/// OutOfOrder is observable in the FINAL buffered body by the presence of BOTH
/// the fallback marker AND the resolved content (InOrder/Async drop the
/// fallback marker entirely). `read_body` buffers the whole response, so this
/// pins the OOO body SHAPE — not the temporal streaming ORDER, which the
/// TCP-based integration tests cover. The name reflects that.
#[ntex::test]
async fn out_of_order_body_contains_the_fallback_marker() {
    let html = suspense_body_via_app_impl("/out").await;
    assert!(
        html.contains("FALLBACK-MARKER"),
        "OutOfOrder's body must carry the streamed fallback marker"
    );
    assert!(html.contains("RESOLVED-CONTENT"));
}

/// RFC 9110 §9.3.2: HEAD must mirror GET's status and headers.
///
/// Note: `test::call_service` bypasses the h1 wire encoder, so the
/// empty-body requirement is asserted in the TCP-based integration
/// tests; here we verify that the handler runs and produces the
/// same status and Content-Type as GET.
#[ntex::test]
async fn head_request_mirrors_get_status_and_content_type() {
    use crate::LeptosRoutes;

    let routes = gen_route_list(MixedApp);
    let app = test::init_service(NtexApp::new().leptos_routes(routes, mixed_shell)).await;

    let get_resp = test::call_service(&app, test::TestRequest::with_uri("/out").to_request()).await;
    assert_eq!(get_resp.status(), StatusCode::OK);
    let get_headers = get_resp.headers().clone();

    let head_resp = test::call_service(
        &app,
        test::TestRequest::default()
            .method(ntex::http::Method::HEAD)
            .uri("/out")
            .to_request(),
    )
    .await;
    assert_eq!(head_resp.status(), StatusCode::OK);
    assert_eq!(
        head_resp.headers().get(header::CONTENT_TYPE),
        get_headers.get(header::CONTENT_TYPE),
        "HEAD must advertise the same Content-Type as GET"
    );
}

/// Same parity assertions via `register_leptos_routes` on a
/// `ServiceConfig`.
#[ntex::test]
async fn head_request_via_service_config_mirrors_get() {
    let routes = gen_route_list(MixedApp);
    let app = test::init_service(NtexApp::new().configure(|cfg| {
        register_leptos_routes(cfg, routes.clone(), mixed_shell);
    }))
    .await;

    // Actually compare against GET (the name claims parity): same status AND
    // Content-Type, mirroring the App-impl variant — not just "HEAD is 200".
    let get = test::call_service(&app, test::TestRequest::with_uri("/out").to_request()).await;
    assert_eq!(get.status(), StatusCode::OK);
    let get_ct = get.headers().get(header::CONTENT_TYPE).cloned();

    let head = test::call_service(
        &app,
        test::TestRequest::default()
            .method(ntex::http::Method::HEAD)
            .uri("/out")
            .to_request(),
    )
    .await;
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(
        head.headers().get(header::CONTENT_TYPE),
        get_ct.as_ref(),
        "HEAD Content-Type must mirror GET through the ServiceConfig path"
    );
}

/// HEAD on an unregistered path must not return 200 — the old
/// synthetic HEAD handler did exactly that, hiding real 404s from
/// monitoring. Now HEAD on a missing route falls through to the
/// default 404 (or the app's configured fallback).
#[ntex::test]
async fn head_request_on_missing_route_not_200() {
    use crate::LeptosRoutes;

    let routes = gen_route_list(MixedApp);
    let app = test::init_service(NtexApp::new().leptos_routes(routes, mixed_shell)).await;

    let resp = test::call_service(
        &app,
        test::TestRequest::default()
            .method(ntex::http::Method::HEAD)
            .uri("/totally-bogus-path")
            .to_request(),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "HEAD on a missing route must exactly 404, not falsely report 200 (or some other non-OK status)"
    );
}

#[ntex::test]
async fn app_leptos_routes_impl_renders_route() {
    use crate::LeptosRoutes;

    // No `/api` catchall: this test only renders '/', never a server fn.
    let routes = gen_route_list(UnitApp);
    let app = test::init_service(NtexApp::new().leptos_routes(routes, unit_shell)).await;

    let req = test::TestRequest::with_uri("/").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = test::read_body(resp).await;
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("Leptos over ntex"));
}

#[ntex::test]
async fn generate_request_and_parts_returns_cloned_head() {
    use crate::generate_request_and_parts;

    let req = test::TestRequest::default()
        .uri("/some/path?q=1")
        .header("x-custom", "yes")
        .to_http_request();
    let payload = ntex::http::Payload::None;

    let (server_fn_req, head) = generate_request_and_parts(req.clone(), payload);
    assert_eq!(head.uri().path(), "/some/path");
    assert_eq!(head.uri().query(), Some("q=1"));
    assert_eq!(
        head.headers().get("x-custom").and_then(|v| v.to_str().ok()),
        Some("yes")
    );

    // Also inspect the `NtexRequest` half of the tuple, not just `head`: a
    // mutation that built it from a fresh/default `HttpRequest` instead of
    // `req.clone()` (while still returning the correct `head`) would
    // otherwise slip through untested.
    let (server_fn_http_req, _payload) = server_fn_req.take();
    assert_eq!(server_fn_http_req.uri().path(), "/some/path");
    assert_eq!(server_fn_http_req.uri().query(), Some("q=1"));
    assert_eq!(
        server_fn_http_req
            .headers()
            .get("x-custom")
            .and_then(|v| v.to_str().ok()),
        Some("yes")
    );
}

#[ntex::test]
async fn handle_response_inner_renders_shell() {
    use crate::handle_response_inner;
    use futures::StreamExt;
    use futures::stream::once as stream_once;
    use leptos_integration_utils::{BoxedFnOnce, PinnedStream};

    let app = test::init_service(NtexApp::new().route(
        "/hrinner",
        ntex::web::get().to(|req: ntex::web::HttpRequest| async move {
            handle_response_inner(
                || {},
                || view! { <!DOCTYPE html><html><body><h1>"HriHello"</h1></body></html> },
                req,
                |app, chunks: BoxedFnOnce<PinnedStream<String>>, _supports_ooo| {
                    Box::pin(async move {
                        let app = app.to_html_stream_in_order().collect::<String>().await;
                        Box::pin(stream_once(async move { app }).chain(chunks()))
                            as PinnedStream<String>
                    })
                },
            )
            .await
        }),
    ))
    .await;

    let req = test::TestRequest::with_uri("/hrinner").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("HriHello"));
}

// ----- public no-context render helpers ------------------------------
// render_app_to_stream / _in_order / _async are thin wrappers over their
// _with_context forms, exported for callers who mount a route by hand. The
// crate's own routing always goes through the _with_context variants, so these
// wrappers were untested — a mutation returning an empty Route survived.
// Mounting each directly and asserting the rendered body pins the wrapper.

#[ntex::test]
async fn render_app_to_stream_helper_renders_the_app() {
    let app = test::init_service(NtexApp::new().route(
        "/",
        crate::render_app_to_stream(unit_shell, leptos_router::Method::Get),
    ))
    .await;
    let resp = test::call_service(&app, test::TestRequest::with_uri("/").to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = String::from_utf8(test::read_body(resp).await.to_vec()).unwrap();
    assert!(
        html.contains("Leptos over ntex"),
        "render_app_to_stream produced no app body: {html}"
    );
}

#[ntex::test]
async fn render_app_to_stream_in_order_helper_renders_the_app() {
    let app = test::init_service(NtexApp::new().route(
        "/",
        crate::render_app_to_stream_in_order(unit_shell, leptos_router::Method::Get),
    ))
    .await;
    let resp = test::call_service(&app, test::TestRequest::with_uri("/").to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = String::from_utf8(test::read_body(resp).await.to_vec()).unwrap();
    assert!(
        html.contains("Leptos over ntex"),
        "render_app_to_stream_in_order produced no app body: {html}"
    );
}

#[ntex::test]
async fn render_app_async_helper_renders_the_app() {
    let app = test::init_service(NtexApp::new().route(
        "/",
        crate::render_app_async(unit_shell, leptos_router::Method::Get),
    ))
    .await;
    let resp = test::call_service(&app, test::TestRequest::with_uri("/").to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = String::from_utf8(test::read_body(resp).await.to_vec()).unwrap();
    assert!(
        html.contains("Leptos over ntex"),
        "render_app_async produced no app body: {html}"
    );
}
