use super::*;
use crate::register_leptos_routes;
use leptos::config::LeptosOptions;
use leptos_meta::provide_meta_context;
use leptos_router::{
    SsrMode,
    components::{Route, Router, Routes},
    path,
    static_routes::StaticRoute,
};
use ntex::http::{StatusCode, header};
use ntex::web::{App as NtexApp, test};

/// A `SsrMode::Static` route whose render sets ALL EIGHT keys in
/// `handle_static_route`'s body-framing strip list (`Content-Length`,
/// `Content-Type`, `Content-Encoding`, `Transfer-Encoding`, `Content-Range`,
/// `Accept-Ranges`, `ETag`, `Last-Modified`) to deliberately WRONG sentinel
/// values, plus one non-framing header (`x-custom-marker`) that must survive
/// the strip untouched. Drives `static_route_strips_every_framing_header_
/// from_the_cached_snapshot`, which the existing `StaticHeaderApp`-based test
/// cannot: that fixture only ever sets a stale `Content-Length`, so the other
/// 7 removals in the strip list were previously unexercised by any assertion.
#[component]
fn StaticFramingHeaderApp() -> impl IntoView {
    provide_meta_context();

    view! {
        <Router>
            <main>
                <Routes fallback=|| view! { <h1>"Not Found"</h1> }>
                    <Route
                        path=path!("/framing")
                        ssr=SsrMode::Static(StaticRoute::new())
                        view=|| {
                            if let Some(res) = use_context::<crate::ResponseOptions>() {
                                for (name, value) in [
                                    (ntex::http::header::CONTENT_LENGTH, "5"),
                                    (ntex::http::header::CONTENT_TYPE, "text/x-bogus"),
                                    (ntex::http::header::CONTENT_ENCODING, "bogus-encoding"),
                                    (ntex::http::header::TRANSFER_ENCODING, "chunked"),
                                    (ntex::http::header::CONTENT_RANGE, "bytes 0-0/0"),
                                    (ntex::http::header::ACCEPT_RANGES, "none"),
                                    (ntex::http::header::ETAG, "\"bogus-etag\""),
                                    (ntex::http::header::LAST_MODIFIED, "Thu, 01 Jan 1970 00:00:00 GMT"),
                                ] {
                                    res.insert_header(
                                        name,
                                        ntex::http::header::HeaderValue::from_static(value),
                                    );
                                }
                                res.insert_header(
                                    ntex::http::header::HeaderName::from_static("x-custom-marker"),
                                    ntex::http::header::HeaderValue::from_static("keep-me"),
                                );
                            }
                            view! { <h1>"Static Framing"</h1> }
                        }
                    />
                </Routes>
            </main>
        </Router>
    }
}

/// A `SsrMode::Static` route registered with a terminal `*splat` segment —
/// matched by every distinct URL under `/files/` (the "wildcard static
/// route" scenario documented on `STATIC_HEADERS`), unlike every other fixture
/// in this file, which registers a literal path. Needed to drive a
/// traversal/dotfile URL past ntex's own router (which only ever matches a
/// literal-path route with that exact string) and into `static_path`'s own
/// rejection guard inside `handle_static_route`.
#[component]
fn StaticSplatApp() -> impl IntoView {
    provide_meta_context();

    view! {
        <Router>
            <main>
                <Routes fallback=|| view! { <h1>"Not Found"</h1> }>
                    <Route
                        path=path!("/files/*any")
                        ssr=SsrMode::Static(StaticRoute::new())
                        view=|| view! { <h1>"Static Splat File"</h1> }
                    />
                </Routes>
            </main>
        </Router>
    }
}

#[ntex::test]
async fn static_route_generator_writes_html() {
    let site_root = temp_site_root("static");
    let (_routes, generator) = gen_route_list_with_ssg(StaticApp);
    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_test")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    generator.generate(&options).await;

    let index_path = site_root.join("index.html");
    let about_path = site_root.join("about.html");

    let index_html = std::fs::read_to_string(&index_path).unwrap();
    let about_html = std::fs::read_to_string(&about_path).unwrap();

    assert!(index_html.contains("Static Home"));
    assert!(about_html.contains("Static About"));

    let _ = std::fs::remove_dir_all(&site_root);
}

/// `StaticRouteGenerator::generate` must skip writing any route whose render
/// resolves to an ERROR status — EVERY 4xx/5xx, not only 404: leptos's
/// static-file builder calls `was_error_status` after each render and, when it
/// returns `true`, sends the HTML back instead of invoking the writer, so the
/// dynamic handler can re-render the real error on demand. Caching an error
/// render as a bare `200 OK` on disk would otherwise serve it indefinitely.
/// A normal route (status left at the default) IS written.
///
/// Pins `was_error_status` end-to-end through the public `generate()` path:
/// `/ok` → file present; `/gone` (404) → absent; `/server-error` (500) →
/// absent. The 500 case is what makes this broader than `leptos_axum` /
/// `leptos_actix` (whose `was_404` checks only `== NOT_FOUND`): narrowing
/// `was_error_status` back to `== NOT_FOUND` would write `server-error.html`,
/// and replacing its body with `true`/`false` breaks the `ok`/error halves.
#[ntex::test]
async fn static_generator_skips_writing_error_routes() {
    let site_root = temp_site_root("static_404");
    let (_routes, generator) = gen_route_list_with_ssg(StaticStatusApp);
    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_static_404")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    generator.generate(&options).await;

    let ok_path = site_root.join("ok.html");
    let gone_path = site_root.join("gone.html");
    let server_error_path = site_root.join("server-error.html");

    assert!(
        ok_path.exists(),
        "a 200 static route must be pre-rendered to disk"
    );
    assert!(
        !gone_path.exists(),
        "a 404 static route must be skipped by `was_error_status`, not cached to disk as a bare 200"
    );
    assert!(
        !server_error_path.exists(),
        "a 500 static route must ALSO be skipped (was_error_status covers every error, not just 404)"
    );

    let ok_html = std::fs::read_to_string(&ok_path).unwrap();
    assert!(ok_html.contains("Static OK"));

    let _ = std::fs::remove_dir_all(&site_root);
}

/// The 404/error-render branch of `handle_static_route` on a COLD cache (the
/// `.html` was never written to disk, matching `static_generator_skips_
/// writing_error_routes` above) must serve the route's OWN captured status —
/// `404` for `/gone`, `500` for `/server-error` — not a hardcoded `404`
/// literal. `ResolvedStaticPath::build` returns `Some(html)` on an error
/// render and the handler seeds the response with a literal `HttpResponse::
/// NotFound()`, but the captured `ResponseParts` snapshot is applied
/// afterwards via `extend_response_parts`, which overwrites the status
/// whenever one was captured — so the real status served must track what the
/// app actually set, not the literal. Deliberately skips `generate()` so the
/// cold-cache regeneration branch (not a pre-warmed cache hit) is what's
/// exercised.
#[ntex::test]
async fn static_route_error_render_on_cold_cache_reports_its_own_captured_status() {
    let site_root = temp_site_root("static_404_cold");
    let (routes, _generator) = gen_route_list_with_ssg(StaticStatusApp);
    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_static_404_cold")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    // Deliberately no `generate()`: both `.html` files are absent, so each
    // request below takes the on-demand regeneration branch, and `/gone` and
    // `/server-error` render an error `build` never caches to disk.
    let app = test::init_service(NtexApp::new().state(options.clone()).configure(|cfg| {
        register_leptos_routes(cfg, routes.clone(), StaticStatusApp);
    }))
    .await;

    let gone_resp =
        test::call_service(&app, test::TestRequest::with_uri("/gone").to_request()).await;
    assert_eq!(
        gone_resp.status(),
        StatusCode::NOT_FOUND,
        "a cold-cache request to a route that captured 404 must report 404"
    );

    let server_error_resp = test::call_service(
        &app,
        test::TestRequest::with_uri("/server-error").to_request(),
    )
    .await;
    assert_eq!(
        server_error_resp.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "a cold-cache request to a route that captured 500 must report 500, not the hardcoded 404 literal"
    );

    let _ = std::fs::remove_dir_all(&site_root);
}

/// HEAD on a statically pre-rendered route must mirror GET's status
/// and Content-Type. (Wire-level body elision is covered by the
/// TCP-based integration tests.)
#[ntex::test]
async fn head_request_on_static_route_mirrors_get() {
    let site_root = temp_site_root("head_static");
    let (routes, generator) = gen_route_list_with_ssg(StaticApp);
    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_head_static")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    generator.generate(&options).await;

    let app = test::init_service(NtexApp::new().state(options.clone()).configure(|cfg| {
        register_leptos_routes(cfg, routes.clone(), StaticApp);
    }))
    .await;

    let get_resp = test::call_service(&app, test::TestRequest::with_uri("/").to_request()).await;
    assert_eq!(get_resp.status(), StatusCode::OK);
    let get_headers = get_resp.headers().clone();

    let head_resp = test::call_service(
        &app,
        test::TestRequest::default()
            .method(ntex::http::Method::HEAD)
            .uri("/")
            .to_request(),
    )
    .await;
    assert_eq!(head_resp.status(), StatusCode::OK);
    assert_eq!(
        head_resp.headers().get(header::CONTENT_TYPE),
        get_headers.get(header::CONTENT_TYPE)
    );

    let _ = std::fs::remove_dir_all(&site_root);
}

#[ntex::test]
async fn static_route_served_over_http() {
    let site_root = temp_site_root("http_static");
    let (routes, generator) = gen_route_list_with_ssg(StaticApp);
    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_test_http")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    generator.generate(&options).await;

    let app = test::init_service(NtexApp::new().state(options.clone()).configure(|cfg| {
        register_leptos_routes(cfg, routes.clone(), StaticApp);
    }))
    .await;

    let req = test::TestRequest::with_uri("/").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = test::read_body(resp).await;
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("Static Home"));

    let _ = std::fs::remove_dir_all(&site_root);
}

/// On-demand regeneration of a `SsrMode::Static` route that was NOT
/// pre-generated by `StaticRouteGenerator::generate` must serve the freshly
/// rendered HTML, not a 500. `ResolvedStaticPath::build` writes the page to
/// disk and returns `None` on success (the body lives on disk, not in
/// memory), so the handler must re-read the just-written file — mirroring
/// leptos_axum (`ServeDir`) and leptos_actix (`NamedFile::open`). Regression
/// probe for the regeneration branch that the existing static-route tests
/// never exercise (they all call `generate()` first).
#[ntex::test]
async fn static_route_on_demand_regeneration_serves_html() {
    let site_root = temp_site_root("static_regen");
    std::fs::create_dir_all(&site_root).unwrap();
    let (routes, _generator) = gen_route_list_with_ssg(StaticApp);
    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_test_regen")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    // Deliberately DO NOT call `_generator.generate(&options).await`: the
    // file is absent on disk, so the first request takes the on-demand
    // regeneration branch of `handle_static_route`.
    let app = test::init_service(NtexApp::new().state(options.clone()).configure(|cfg| {
        register_leptos_routes(cfg, routes.clone(), StaticApp);
    }))
    .await;

    let resp = test::call_service(&app, test::TestRequest::with_uri("/").to_request()).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "first (regenerating) request to an un-pregenerated static route must serve 200, not 500"
    );
    let body = test::read_body(resp).await;
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        html.contains("Static Home"),
        "must serve the rendered static HTML, got: {html}"
    );

    let _ = std::fs::remove_dir_all(&site_root);
}

/// Concurrent on-demand regenerations of the SAME static path must never
/// serve a body from one render under headers captured by another. Every
/// render of `StaticEpochApp` stamps a fresh epoch into both the body and
/// the `x-render-epoch` header; with the file initially absent, N parallel
/// first requests all race down the regeneration branch (the documented
/// stampede), each writing the file+header snapshot under the write stripe
/// and then re-opening the file. A re-open that pairs the on-disk body with
/// the REQUEST-LOCAL header snapshot (instead of reading both under the
/// stripe) lets a neighbour's freshly written body ship under this
/// request's older headers — the regression this pins down. The pairing
/// invariant must hold for every interleaving, so the test is
/// deterministic-green under the fix and only the broken pairing can flake
/// it red.
///
/// One-sided by nature: a red PROVES the pairing bug, while a green is
/// meaningful only while the stampede design lets renders overlap (no
/// barrier forces all 16 requests past the missing-file check before the
/// first write lands). Manual-Red evidence at introduction: against the
/// pre-fix unpaired re-open this failed 17/20 runs; with the fix, 10/10
/// green. If in-flight render dedup (single-flight) ever lands, only one
/// epoch will exist and this test stays trivially green — correctly so,
/// because the bug class is then designed away.
#[ntex::test]
async fn static_route_concurrent_regeneration_pairs_body_with_headers() {
    let site_root = temp_site_root("static_epoch_race");
    std::fs::create_dir_all(&site_root).unwrap();
    let (routes, _generator) = gen_route_list_with_ssg(StaticEpochApp);
    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_static_epoch")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    // Deliberately no `generate()`: the file must be missing so the initial
    // requests take the regeneration branch concurrently.
    let app = test::init_service(NtexApp::new().state(options.clone()).configure(|cfg| {
        register_leptos_routes(cfg, routes.clone(), StaticEpochApp);
    }))
    .await;

    let responses = futures::future::join_all(
        (0..16)
            .map(|_| test::call_service(&app, test::TestRequest::with_uri("/epoch").to_request())),
    )
    .await;

    for resp in responses {
        assert_eq!(resp.status(), StatusCode::OK);
        let header_epoch = resp
            .headers()
            .get("x-render-epoch")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .expect("every served static response must carry the captured header snapshot");
        let body = test::read_body(resp).await;
        let html = String::from_utf8(body.to_vec()).unwrap();
        let body_epoch = html
            .split("epoch-")
            .nth(1)
            .and_then(|tail| tail.split("-marker").next())
            .expect("rendered body must contain the epoch marker");
        assert_eq!(
            body_epoch, header_epoch,
            "body epoch and x-render-epoch header must come from one render"
        );
    }

    let _ = std::fs::remove_dir_all(&site_root);
}

#[ntex::test]
async fn static_route_cached_headers_are_replayed_more_than_once() {
    let site_root = temp_site_root("static_headers");
    let (routes, generator) = gen_route_list_with_ssg(StaticHeaderApp);
    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_static_headers")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    generator.generate(&options).await;

    // Overwrite the pre-rendered file with a sentinel the renderer would never
    // produce. A genuine cache HIT serves the on-disk file (so the body carries
    // the sentinel) AND replays the captured headers; a regression that
    // re-rendered instead would emit "Static Headers" and lose the sentinel.
    // This is what isolates the cache-hit replay path from a re-render.
    std::fs::write(
        site_root.join("headers.html"),
        "<html><body>SENTINEL_CACHE_HIT</body></html>",
    )
    .unwrap();

    let app = test::init_service(NtexApp::new().state(options.clone()).configure(|cfg| {
        register_leptos_routes(cfg, routes.clone(), StaticHeaderApp);
    }))
    .await;

    for _ in 0..3 {
        let resp =
            test::call_service(&app, test::TestRequest::with_uri("/headers").to_request()).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        assert_eq!(
            resp.headers()
                .get("x-static-cache")
                .and_then(|v| v.to_str().ok()),
            Some("preserved")
        );
        // Captured BEFORE the body is consumed. CB-07: the stale `Content-Length`
        // the component put on `ResponseOptions` (a deliberately wrong "5") must
        // NOT survive onto a cache-hit response — the framing-strip drops it so
        // the on-disk file (`NamedFile`) stays authoritative. (`NamedFile`'s own
        // length header is applied by the h1 encoder, which `test::call_service`
        // bypasses, so the observable here is the ABSENCE of the bogus value,
        // not the presence of the real one.)
        let content_length = resp
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = test::read_body(resp).await;
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            html.contains("SENTINEL_CACHE_HIT"),
            "a cache hit must serve the on-disk file (sentinel), not re-render: {html}"
        );
        assert_ne!(
            content_length.as_deref(),
            Some("5"),
            "the stale snapshot Content-Length (5) must be stripped on a cache hit, not served over the file's real size"
        );
    }

    let _ = std::fs::remove_dir_all(&site_root);
}

/// All 8 keys in `handle_static_route`'s framing-header strip list —
/// `Content-Length`, `Content-Type`, `Content-Encoding`, `Transfer-Encoding`,
/// `Content-Range`, `Accept-Ranges`, `ETag`, `Last-Modified` — must be
/// removed from a captured `ResponseParts` snapshot on a file-served
/// response, not just `Content-Length` (the only one the pre-existing
/// `StaticHeaderApp`-based test exercises). A non-framing header the app set
/// (`x-custom-marker`) must still survive, proving the strip is scoped to
/// exactly those 8 keys and not a wholesale header wipe.
#[ntex::test]
async fn static_route_strips_every_framing_header_from_the_cached_snapshot() {
    let site_root = temp_site_root("static_framing_headers");
    let (routes, generator) = gen_route_list_with_ssg(StaticFramingHeaderApp);
    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_static_framing_headers")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    generator.generate(&options).await;

    let app = test::init_service(NtexApp::new().state(options.clone()).configure(|cfg| {
        register_leptos_routes(cfg, routes.clone(), StaticFramingHeaderApp);
    }))
    .await;

    let resp = test::call_service(&app, test::TestRequest::with_uri("/framing").to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let headers = resp.headers().clone();
    // None of the 8 bogus sentinel values the component set must survive —
    // each is either absent or overwritten by NamedFile's own authoritative
    // value, which is never the sentinel this test planted.
    assert_ne!(
        headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()),
        Some("5"),
        "the stale Content-Length sentinel must be stripped"
    );
    assert_ne!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/x-bogus"),
        "the stale Content-Type sentinel must be stripped; NamedFile derives its own MIME"
    );
    assert!(
        headers.get(header::CONTENT_ENCODING).is_none(),
        "the stale Content-Encoding sentinel must be stripped"
    );
    assert!(
        headers.get(header::TRANSFER_ENCODING).is_none(),
        "the stale Transfer-Encoding sentinel must be stripped"
    );
    assert!(
        headers.get(header::CONTENT_RANGE).is_none(),
        "the stale Content-Range sentinel must be stripped on a full (non-range) serve"
    );
    assert_ne!(
        headers
            .get(header::ACCEPT_RANGES)
            .and_then(|v| v.to_str().ok()),
        Some("none"),
        "the stale Accept-Ranges sentinel must be stripped"
    );
    assert_ne!(
        headers.get(header::ETAG).and_then(|v| v.to_str().ok()),
        Some("\"bogus-etag\""),
        "the stale ETag sentinel must be stripped; NamedFile derives its own"
    );
    assert_ne!(
        headers
            .get(header::LAST_MODIFIED)
            .and_then(|v| v.to_str().ok()),
        Some("Thu, 01 Jan 1970 00:00:00 GMT"),
        "the stale Last-Modified sentinel must be stripped; NamedFile derives its own"
    );

    // The non-framing header the app set is NOT in the strip list, so it must
    // survive exactly as captured.
    assert_eq!(
        headers.get("x-custom-marker").and_then(|v| v.to_str().ok()),
        Some("keep-me"),
        "a non-framing header must survive the strip untouched"
    );

    let _ = std::fs::remove_dir_all(&site_root);
}

/// A captured `ResponseOptions` status must not overwrite the status
/// `NamedFile` computes from the request's validators / `Range` on the
/// file-serving branch (RFC 9110 §13, §14.4, §15.4.5). The `/headers` route
/// sets `201` during its static render; a full serve keeps that `201`, but a
/// conditional (`If-None-Match`) or range request must report `NamedFile`'s
/// `304` / `206` instead — otherwise a client sending `If-None-Match` would
/// receive `201` with an empty body and drop its valid cache. Upstream
/// `leptos_axum` / `leptos_actix` overwrite unconditionally; this is a
/// deliberate divergence.
#[ntex::test]
async fn static_route_captured_status_yields_to_namedfile_conditional_and_range() {
    let site_root = temp_site_root("static_conditional_status");
    let (routes, generator) = gen_route_list_with_ssg(StaticHeaderApp);
    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_static_conditional_status")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    generator.generate(&options).await;

    let app = test::init_service(NtexApp::new().state(options.clone()).configure(|cfg| {
        register_leptos_routes(cfg, routes.clone(), StaticHeaderApp);
    }))
    .await;

    // A full serve keeps the app's captured status and exposes NamedFile's ETag.
    let full = test::call_service(&app, test::TestRequest::with_uri("/headers").to_request()).await;
    assert_eq!(
        full.status(),
        StatusCode::CREATED,
        "a full serve must keep the app's captured 201"
    );
    let etag = full
        .headers()
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .expect("NamedFile must set an ETag on the served static file");

    // If-None-Match matches the file's ETag: NamedFile computes 304, and the
    // captured 201 must NOT overwrite it.
    let conditional = test::call_service(
        &app,
        test::TestRequest::with_uri("/headers")
            .header(header::IF_NONE_MATCH, etag.as_str())
            .to_request(),
    )
    .await;
    assert_eq!(
        conditional.status(),
        StatusCode::NOT_MODIFIED,
        "If-None-Match match must report 304, not the captured 201"
    );

    // A satisfiable Range: NamedFile computes 206, and the captured 201 must
    // NOT overwrite it.
    let ranged = test::call_service(
        &app,
        test::TestRequest::with_uri("/headers")
            .header(header::RANGE, "bytes=0-3")
            .to_request(),
    )
    .await;
    assert_eq!(
        ranged.status(),
        StatusCode::PARTIAL_CONTENT,
        "a satisfiable Range must report 206, not the captured 201"
    );

    let _ = std::fs::remove_dir_all(&site_root);
}

/// The THIRD precedence arm — a route with NO captured status at all (`/`,
/// whose `ResponseParts::status` is always `None`) — must let `NamedFile`'s
/// conditional/range status through completely unopposed: there is no
/// captured status to yield OR to fight over. Complements the two tests above,
/// which only ever drive the `Some(status) if is_success()` and `Some(status)`
/// (redirect) arms of the match in `handle_static_route`; this one exercises
/// the `None => {}` arm, otherwise never reached by any conditional/range
/// request in this file.
#[ntex::test]
async fn static_route_with_no_captured_status_still_yields_to_namedfile_conditional_and_range() {
    let site_root = temp_site_root("static_conditional_no_status");
    let (routes, generator) = gen_route_list_with_ssg(StaticApp);
    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_static_conditional_no_status")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    generator.generate(&options).await;

    let app = test::init_service(NtexApp::new().state(options.clone()).configure(|cfg| {
        register_leptos_routes(cfg, routes.clone(), StaticApp);
    }))
    .await;

    // A full serve reports NamedFile's plain 200 (there is no captured status
    // to apply on this route at all).
    let full = test::call_service(&app, test::TestRequest::with_uri("/").to_request()).await;
    assert_eq!(full.status(), StatusCode::OK);
    let etag = full
        .headers()
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .expect("NamedFile must set an ETag on the served static file");

    // If-None-Match matches the file's ETag: NamedFile computes 304. The
    // `None => {}` arm must leave this untouched.
    let conditional = test::call_service(
        &app,
        test::TestRequest::with_uri("/")
            .header(header::IF_NONE_MATCH, etag.as_str())
            .to_request(),
    )
    .await;
    assert_eq!(
        conditional.status(),
        StatusCode::NOT_MODIFIED,
        "a plain static route with no captured status must still report 304 on a conditional hit"
    );

    // A satisfiable Range: NamedFile computes 206, again untouched by the
    // (absent) captured status.
    let ranged = test::call_service(
        &app,
        test::TestRequest::with_uri("/")
            .header(header::RANGE, "bytes=0-3")
            .to_request(),
    )
    .await;
    assert_eq!(
        ranged.status(),
        StatusCode::PARTIAL_CONTENT,
        "a plain static route with no captured status must still report 206 on a satisfiable range request"
    );

    let _ = std::fs::remove_dir_all(&site_root);
}

/// A `SsrMode::Static` route that captured a *redirect* (`302` + `Location`)
/// must keep redirecting on conditional / range hits: the captured status is
/// not a success representation, so `NamedFile`'s `304` / `206` must NOT
/// replace it — otherwise the client is stranded on a `304` / `206` carrying a
/// `Location` instead of following the redirect. Only a captured `2xx` yields
/// to `NamedFile`'s conditional/range status.
#[ntex::test]
async fn static_route_captured_redirect_survives_conditional_and_range() {
    let site_root = temp_site_root("static_redirect_status");
    let (routes, generator) = gen_route_list_with_ssg(StaticRedirectApp);
    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_static_redirect_status")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    generator.generate(&options).await;

    let app = test::init_service(NtexApp::new().state(options.clone()).configure(|cfg| {
        register_leptos_routes(cfg, routes.clone(), StaticRedirectApp);
    }))
    .await;

    // A full serve keeps the captured redirect and exposes NamedFile's ETag.
    let full = test::call_service(&app, test::TestRequest::with_uri("/go").to_request()).await;
    assert_eq!(
        full.status(),
        StatusCode::FOUND,
        "a full serve must keep the captured 302 redirect"
    );
    let etag = full
        .headers()
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .expect("NamedFile must set an ETag on the served static file");

    // A conditional hit must STILL redirect (302), not collapse to NamedFile's 304.
    let conditional = test::call_service(
        &app,
        test::TestRequest::with_uri("/go")
            .header(header::IF_NONE_MATCH, etag.as_str())
            .to_request(),
    )
    .await;
    assert_eq!(
        conditional.status(),
        StatusCode::FOUND,
        "a conditional hit to a static redirect must keep 302, not become 304"
    );

    // A range request must STILL redirect (302), not collapse to NamedFile's
    // 206 — and the redirect must not carry NamedFile's range artifacts
    // (`Content-Range` header + partial file body).
    let ranged = test::call_service(
        &app,
        test::TestRequest::with_uri("/go")
            .header(header::RANGE, "bytes=0-3")
            .to_request(),
    )
    .await;
    assert_eq!(
        ranged.status(),
        StatusCode::FOUND,
        "a range request to a static redirect must keep 302, not become 206"
    );
    let ranged_has_content_range = ranged.headers().get(header::CONTENT_RANGE).is_some();
    let ranged_body = test::read_body(ranged).await;
    assert!(
        !ranged_has_content_range,
        "a redirect must not carry NamedFile's `Content-Range` range artifact"
    );
    assert!(
        ranged_body.is_empty(),
        "a redirect must not carry NamedFile's partial file body, got {} bytes",
        ranged_body.len()
    );

    let _ = std::fs::remove_dir_all(&site_root);
}

/// The very FIRST request to a not-yet-pregenerated `SsrMode::Static` route
/// that captures a non-default status/redirect must already serve it
/// correctly — not just on a subsequent cache hit. Every other
/// status/redirect-capturing test in this file calls `generate()` first
/// (warm-cache only); this one deliberately skips it so the on-demand
/// write→cache→re-open round trip (the SAME `STATIC_HEADERS` mechanism a
/// cache hit reads, per `open_paired_static_file`) is proven end-to-end on a
/// cold cache too, for both a captured status (`StaticHeaderApp`, 201) and a
/// captured redirect (`StaticRedirectApp`, 302 + Location).
#[ntex::test]
async fn static_route_cold_regeneration_preserves_captured_status_and_redirect() {
    let status_site_root = temp_site_root("static_cold_status");
    let (status_routes, _status_generator) = gen_route_list_with_ssg(StaticHeaderApp);
    let status_options = LeptosOptions::builder()
        .output_name("leptos_ntex_static_cold_status")
        .site_root(status_site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    // Deliberately no `generate()`: `headers.html` is absent, so this is the
    // route's first-ever request.
    let status_app = test::init_service(NtexApp::new().state(status_options.clone()).configure(
        |cfg| {
            register_leptos_routes(cfg, status_routes.clone(), StaticHeaderApp);
        },
    ))
    .await;

    let status_resp = test::call_service(
        &status_app,
        test::TestRequest::with_uri("/headers").to_request(),
    )
    .await;
    assert_eq!(
        status_resp.status(),
        StatusCode::CREATED,
        "the first (regenerating) request to a route that captures a custom status must already report it"
    );

    let _ = std::fs::remove_dir_all(&status_site_root);

    let redirect_site_root = temp_site_root("static_cold_redirect");
    let (redirect_routes, _redirect_generator) = gen_route_list_with_ssg(StaticRedirectApp);
    let redirect_options = LeptosOptions::builder()
        .output_name("leptos_ntex_static_cold_redirect")
        .site_root(redirect_site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    // Deliberately no `generate()`: `go.html` is absent, so this is the
    // route's first-ever request.
    let redirect_app = test::init_service(
        NtexApp::new()
            .state(redirect_options.clone())
            .configure(|cfg| {
                register_leptos_routes(cfg, redirect_routes.clone(), StaticRedirectApp);
            }),
    )
    .await;

    let redirect_resp = test::call_service(
        &redirect_app,
        test::TestRequest::with_uri("/go").to_request(),
    )
    .await;
    assert_eq!(
        redirect_resp.status(),
        StatusCode::FOUND,
        "the first (regenerating) request to a route that captures a redirect must already redirect"
    );
    assert_eq!(
        redirect_resp
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("/elsewhere"),
        "the first (regenerating) request must already carry the captured Location header"
    );

    let _ = std::fs::remove_dir_all(&redirect_site_root);
}

/// If the just-written `.html` vanishes between `write_static_route`'s atomic
/// rename and `handle_static_route`'s post-regeneration re-open (e.g. deleted
/// or moved out from under the server), the handler must report `500`, not
/// panic, hang, or silently serve an empty `200`.
///
/// Deterministic by construction: `REGEN_REOPEN_TEST_HOOK` fires exactly once,
/// synchronously, at the precise point between the write completing and the
/// re-open starting — no background thread racing OS scheduling, so this
/// cannot flake on a loaded/fast CI runner the way a timing-based watcher
/// could (an earlier version of this test used one; see git history).
#[ntex::test]
async fn static_route_reopen_failure_after_regeneration_reports_500() {
    let site_root = temp_site_root("static_regen_vanish");
    std::fs::create_dir_all(&site_root).unwrap();
    let (routes, _generator) = gen_route_list_with_ssg(StaticApp);
    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_static_regen_vanish")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    {
        // Remove the whole directory (not just the file): a regenerating
        // re-open re-canonicalizes `root` too, so this also covers a root
        // that disappears mid-race. Keyed by `site_root` — see
        // `REGEN_REOPEN_TEST_HOOK`'s doc comment — so a concurrently running
        // on-demand-regeneration test (e.g.
        // `static_route_on_demand_regeneration_serves_html`) cannot consume
        // this hook for its own, different, request.
        let hook_root = site_root.clone();
        let site_root_to_remove = site_root.clone();
        *crate::static_routes::REGEN_REOPEN_TEST_HOOK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((
            hook_root,
            Box::new(move || {
                std::fs::remove_dir_all(&site_root_to_remove)
                    .expect("site_root must still be removable at the reopen seam");
            }),
        ));
    }

    let app = test::init_service(NtexApp::new().state(options.clone()).configure(|cfg| {
        register_leptos_routes(cfg, routes.clone(), StaticApp);
    }))
    .await;

    let resp = test::call_service(&app, test::TestRequest::with_uri("/").to_request()).await;

    let _ = std::fs::remove_dir_all(&site_root);

    assert_eq!(
        resp.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "a re-open failure right after a successful regeneration must report 500"
    );
}

/// A URL that `static_path` rejects (path traversal, a dotfile, a smuggled
/// separator) must make `handle_static_route`'s registered service answer
/// `404`, not `500` and not panic — the handler's early-return guard
/// (`let Some(path_buf) = static_path(...) else { return HttpResponse::
/// NotFound().finish(); }`) is otherwise only unit-tested as the pure
/// `static_path` function returning `None`, never driven through an actual
/// HTTP request against a registered route in this file. A *splat* static
/// route (`/files/*any`) is required to reach this guard over HTTP at all: a
/// route registered at a literal path (`/about`) only ever receives that exact
/// path from ntex's own router, which never contains a traversal/dotfile
/// segment to reject; a splat is matched by every distinct URL under it
/// (documented on `STATIC_HEADERS` as the "wildcard static route" case), so a
/// malicious segment inside the splat's tail DOES reach `static_path`.
#[ntex::test]
async fn static_route_rejects_a_traversal_url_with_404() {
    let site_root = temp_site_root("static_traversal");
    let (routes, generator) = gen_route_list_with_ssg(StaticSplatApp);
    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_static_traversal")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    generator.generate(&options).await;

    let app = test::init_service(NtexApp::new().state(options.clone()).configure(|cfg| {
        register_leptos_routes(cfg, routes.clone(), StaticSplatApp);
    }))
    .await;

    let traversal = test::call_service(
        &app,
        test::TestRequest::with_uri("/files/../secret").to_request(),
    )
    .await;
    assert_eq!(
        traversal.status(),
        StatusCode::NOT_FOUND,
        "a path-traversal URL must be rejected with 404, not 500 or a panic"
    );

    let dotfile = test::call_service(
        &app,
        test::TestRequest::with_uri("/files/.env").to_request(),
    )
    .await;
    assert_eq!(
        dotfile.status(),
        StatusCode::NOT_FOUND,
        "a dotfile URL must be rejected with 404, not 500 or a panic"
    );

    let _ = std::fs::remove_dir_all(&site_root);
}

/// The remaining 3 of the 5 statuses `is_conditional_or_range_status`
/// protects — `412` (failed `If-Match`), `416` (unsatisfiable `Range`), `400`
/// (a `Range` header that is not valid text) — must ALSO win over a route's
/// captured non-error status (`StaticHeaderApp`'s captured `201`), exactly
/// like the `304`/`206` cases the pre-existing conditional/range test covers.
/// Closes the gap where only 2 of the 5 protected statuses were ever driven
/// end-to-end.
#[ntex::test]
async fn static_route_captured_status_yields_to_namedfile_precondition_range_and_malformed_range() {
    let site_root = temp_site_root("static_conditional_status_412_416_400");
    let (routes, generator) = gen_route_list_with_ssg(StaticHeaderApp);
    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_static_conditional_412_416_400")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    generator.generate(&options).await;

    let app = test::init_service(NtexApp::new().state(options.clone()).configure(|cfg| {
        register_leptos_routes(cfg, routes.clone(), StaticHeaderApp);
    }))
    .await;

    // If-Match with a value that can never match a real ETag: NamedFile
    // computes 412, and the captured 201 must NOT overwrite it.
    let precondition_failed = test::call_service(
        &app,
        test::TestRequest::with_uri("/headers")
            .header(header::IF_MATCH, "\"does-not-match\"")
            .to_request(),
    )
    .await;
    assert_eq!(
        precondition_failed.status(),
        StatusCode::PRECONDITION_FAILED,
        "a failed If-Match must report 412, not the captured 201"
    );

    // An unsatisfiable Range (far past the end of the small rendered file):
    // NamedFile computes 416, and the captured 201 must NOT overwrite it.
    let range_not_satisfiable = test::call_service(
        &app,
        test::TestRequest::with_uri("/headers")
            .header(header::RANGE, "bytes=9000-9999")
            .to_request(),
    )
    .await;
    assert_eq!(
        range_not_satisfiable.status(),
        StatusCode::RANGE_NOT_SATISFIABLE,
        "an unsatisfiable Range must report 416, not the captured 201"
    );

    // A Range header that is not valid TEXT (per `is_conditional_or_range_
    // status`'s own doc comment): `ntex_files` reports 400 specifically when
    // the raw header bytes fail `to_str()` (invalid UTF-8), as opposed to a
    // syntactically-invalid-but-valid-UTF-8 range (e.g. "bytes=abc"), which
    // `HttpRange::parse` treats as unsatisfiable (416) instead. The captured
    // 201 must NOT overwrite this 400 either.
    let malformed_range = test::call_service(
        &app,
        test::TestRequest::with_uri("/headers")
            .header(
                header::RANGE,
                ntex::http::header::HeaderValue::from_bytes(b"bytes=\xff\xff").unwrap(),
            )
            .to_request(),
    )
    .await;
    assert_eq!(
        malformed_range.status(),
        StatusCode::BAD_REQUEST,
        "a malformed Range header must report 400, not the captured 201"
    );

    let _ = std::fs::remove_dir_all(&site_root);
}
