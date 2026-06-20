use super::*;
use crate::register_leptos_routes;
use leptos::config::LeptosOptions;
use ntex::http::{StatusCode, header};
use ntex::web::{App as NtexApp, test};

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
