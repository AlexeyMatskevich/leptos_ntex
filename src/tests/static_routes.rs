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

/// A static render with incorrect framing/validator overrides, a valid
/// application-selected UTF-8 HTML MIME, and an unrelated marker. NamedFile
/// must derive byte framing/validators while preserving the explicit MIME.
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
                                    (ntex::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
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
/// route" scenario), unlike every other fixture
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

#[derive(Clone, Copy)]
enum StaticAppKind {
    Home,
    Headers,
    Framing,
    Status,
    Redirect,
    Splat,
}

fn static_view(kind: StaticAppKind) -> AnyView {
    match kind {
        StaticAppKind::Home => StaticApp().into_any(),
        StaticAppKind::Headers => StaticHeaderApp().into_any(),
        StaticAppKind::Framing => StaticFramingHeaderApp().into_any(),
        StaticAppKind::Status => StaticStatusApp().into_any(),
        StaticAppKind::Redirect => StaticRedirectApp().into_any(),
        StaticAppKind::Splat => StaticSplatApp().into_any(),
    }
}

fn static_options(root: &std::path::Path) -> LeptosOptions {
    LeptosOptions::builder()
        .output_name("static_contract")
        .site_root(root.to_string_lossy().into_owned())
        .site_pkg_dir("pkg")
        .build()
}

#[derive(Debug)]
struct GenerationObservation {
    index: Option<String>,
    about: Option<String>,
    ok: Option<String>,
    gone: bool,
    server_error: bool,
}

async fn generation_observation(kind: StaticAppKind) -> GenerationObservation {
    let root = temp_site_root("static_generation_contract");
    let app_fn = move || static_view(kind);
    let (_routes, generator) = gen_route_list_with_ssg(app_fn);
    generator.generate(&static_options(&root)).await;
    GenerationObservation {
        index: std::fs::read_to_string(root.join("index.html")).ok(),
        about: std::fs::read_to_string(root.join("about.html")).ok(),
        ok: std::fs::read_to_string(root.join("ok.html")).ok(),
        gone: root.join("gone.html").exists(),
        server_error: root.join("server-error.html").exists(),
    }
}

#[derive(Clone)]
enum Constraint {
    None,
    MatchCurrent,
    FailedMatch,
    Range(&'static str),
    MalformedRange,
}
#[derive(Clone, Copy)]
enum ArtifactState {
    Paired,
    ChangedBody,
    Legacy,
    RemovedBeforeReopen,
}

#[derive(Clone)]
struct StaticRequest {
    kind: StaticAppKind,
    uri: &'static str,
    pregenerate: bool,
    method: ntex::http::Method,
    constraint: Constraint,
    artifact: ArtifactState,
}
impl Default for StaticRequest {
    fn default() -> Self {
        Self {
            kind: StaticAppKind::Home,
            uri: "/",
            pregenerate: true,
            method: ntex::http::Method::GET,
            constraint: Constraint::None,
            artifact: ArtifactState::Paired,
        }
    }
}

#[derive(Debug)]
struct StaticObservation {
    status: StatusCode,
    body: String,
    content_type: Option<String>,
    content_length: Option<String>,
    encoding: Option<String>,
    transfer: Option<String>,
    content_range: Option<String>,
    ranges: Option<String>,
    etag: Option<String>,
    modified: Option<String>,
    marker: Option<String>,
    location: Option<String>,
    rendered_on_request: usize,
    disk_exists: bool,
}

async fn static_request(request: StaticRequest) -> StaticObservation {
    let root = temp_site_root("static_http_contract");
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let app_fn = {
        let count = count.clone();
        let kind = request.kind;
        move || {
            count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            static_view(kind)
        }
    };
    let (routes, generator) = gen_route_list_with_ssg(app_fn.clone());
    let options = static_options(&root);
    if request.pregenerate {
        generator.generate(&options).await;
    }
    let file = root.join(if request.uri == "/" {
        "index.html".to_owned()
    } else {
        format!("{}.html", request.uri.trim_start_matches('/'))
    });
    let mut _reopen_hook = None;
    match request.artifact {
        ArtifactState::Paired => {}
        ArtifactState::ChangedBody => std::fs::write(&file, "inconsistent-old-body").unwrap(),
        ArtifactState::Legacy => {
            let metadata = file
                .parent()
                .unwrap()
                .join(".leptos-static-metadata")
                .join(file.file_name().unwrap());
            let _ = std::fs::remove_file(metadata);
            std::fs::write(&file, "external-legacy-html").unwrap();
        }
        ArtifactState::RemovedBeforeReopen => {
            let path = root.to_path_buf();
            let owned = root.clone();
            _reopen_hook = Some(crate::static_routes::test_hooks::on_reopen(
                path,
                Box::new(move || std::fs::remove_dir_all(&owned)),
            ));
        }
    }
    let app = test::init_service(
        NtexApp::new()
            .state(options)
            .configure(|cfg| register_leptos_routes(cfg, routes.clone(), app_fn.clone())),
    )
    .await;
    let mut req = test::TestRequest::with_uri(request.uri).method(request.method);
    match request.constraint {
        Constraint::None => {}
        Constraint::MatchCurrent => {
            let full =
                test::call_service(&app, test::TestRequest::with_uri(request.uri).to_request())
                    .await;
            req = req.header(
                header::IF_NONE_MATCH,
                full.headers()
                    .get(header::ETAG)
                    .expect("full representation has validator")
                    .clone(),
            );
        }
        Constraint::FailedMatch => {
            req = req.header(header::IF_MATCH, "\"not-current\"");
        }
        Constraint::Range(range) => {
            req = req.header(header::RANGE, range);
        }
        Constraint::MalformedRange => {
            req = req.header(
                header::RANGE,
                header::HeaderValue::from_bytes(b"bytes=\xff").unwrap(),
            );
        }
    }
    let before = count.load(std::sync::atomic::Ordering::Relaxed);
    let response = test::call_service(&app, req.to_request()).await;
    let rendered_on_request = count.load(std::sync::atomic::Ordering::Relaxed) - before;
    let value = |name| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let status = response.status();
    let content_type = value(header::CONTENT_TYPE);
    let content_length = value(header::CONTENT_LENGTH);
    let encoding = value(header::CONTENT_ENCODING);
    let transfer = value(header::TRANSFER_ENCODING);
    let content_range = value(header::CONTENT_RANGE);
    let ranges = value(header::ACCEPT_RANGES);
    let etag = value(header::ETAG);
    let modified = value(header::LAST_MODIFIED);
    let marker = value(header::HeaderName::from_static(
        if matches!(request.kind, StaticAppKind::Framing) {
            "x-custom-marker"
        } else {
            "x-static-cache"
        },
    ));
    let location = value(header::LOCATION);
    let body = String::from_utf8(test::read_body(response).await.to_vec()).unwrap();
    StaticObservation {
        status,
        body,
        content_type,
        content_length,
        encoding,
        transfer,
        content_range,
        ranges,
        etag,
        modified,
        marker,
        location,
        rendered_on_request,
        disk_exists: file.exists(),
    }
}

fn contains_html(text: &'static str) -> impl Fn(&String) -> lets_expect::AssertionResult {
    move |actual| {
        if actual.contains(text) {
            Ok(())
        } else {
            Err(lets_expect::AssertionError {
                message: vec![format!(
                    "Expected HTML containing {text:?}; received {actual:?}"
                )],
            })
        }
    }
}

lets_expect::lets_expect! {
    expect(run_ntex(generation_observation(kind))) as the_static_generation {
        let kind = StaticAppKind::Home;
        to writes_the_home_page { have(index) be_some_and contains_html("Static Home") }
        to writes_the_other_declared_page { have(about) be_some_and contains_html("Static About") }
        when the_routes_include_errors {
            let kind = StaticAppKind::Status;
            to writes_the_success { have(ok) be_some_and contains_html("Static OK") }
            to does_not_persist_client_errors { have(gone) be_false }
            to does_not_persist_server_errors { have(server_error) be_false }
        }
    }
}

lets_expect::lets_expect! {
    expect(run_ntex(static_request(request))) as the_static_http_response {
        let request = StaticRequest::default();
        to serves_the_pregenerated_page { have(status) equal(StatusCode::OK), have(body) contains_html("Static Home"), have(rendered_on_request) equal(0_usize) }
        when the_method_is_head {
            let request = StaticRequest { method: ntex::http::Method::HEAD, ..Default::default() };
            to preserves_the_representation_metadata { have(status) equal(StatusCode::OK), have(content_type) equal(Some("text/html".to_owned())) }
        }
        when the_file_has_not_been_generated {
            let request = StaticRequest { pregenerate: false, ..Default::default() };
            to generates_and_serves_the_page { have(status) equal(StatusCode::OK), have(body) contains_html("Static Home"), have(disk_exists) be_true }
        }
        when the_body_no_longer_matches_its_metadata {
            let request = StaticRequest { artifact: ArtifactState::ChangedBody, ..Default::default() };
            to regenerates_a_valid_representation { have(status) equal(StatusCode::OK), have(body) contains_html("Static Home"), have(rendered_on_request) equal(1_usize) }
        }
        when the_artifact_is_legacy_html {
            let request = StaticRequest { artifact: ArtifactState::Legacy, ..Default::default() };
            to serves_the_existing_legacy_format { have(status) equal(StatusCode::OK), have(body) equal("external-legacy-html".to_owned()), have(rendered_on_request) equal(0_usize) }
        }
        when the_artifact_disappears_before_reopen {
            let request = StaticRequest { pregenerate: false, artifact: ArtifactState::RemovedBeforeReopen, ..Default::default() };
            to reports_a_controlled_failure { have(status) equal(StatusCode::INTERNAL_SERVER_ERROR), have(body) equal(String::new()) }
        }
        when a_current_validator_is_sent {
            let request = StaticRequest { constraint: Constraint::MatchCurrent, ..Default::default() };
            to reports_not_modified { have(status) equal(StatusCode::NOT_MODIFIED), have(body) equal(String::new()) }
        }
        when a_partial_range_is_requested {
            let request = StaticRequest { constraint: Constraint::Range("bytes=0-3"), ..Default::default() };
            to serves_the_selected_bytes { have(status) equal(StatusCode::PARTIAL_CONTENT), have(body.len()) equal(4_usize), have(content_range) be_some }
        }
        when a_range_is_unsatisfiable {
            let request = StaticRequest { constraint: Constraint::Range("bytes=9000-9999"), ..Default::default() };
            to reports_range_not_satisfiable { have(status) equal(StatusCode::RANGE_NOT_SATISFIABLE), have(body) equal(String::new()) }
        }
    }
}

lets_expect::lets_expect! {
    expect(run_ntex(static_request(request))) as the_captured_static_status {
        let constraint = Constraint::None;
        let pregenerate = true;
        let request = StaticRequest { kind: StaticAppKind::Headers, uri: "/headers", pregenerate, constraint, ..Default::default() };
        to replays_status_and_headers { have(status) equal(StatusCode::CREATED), have(marker) equal(Some("preserved".to_owned())), have(body) contains_html("Static Headers") }
        to does_not_rerender_a_paired_hit { have(rendered_on_request) equal(0_usize) }
        to uses_the_file_framing { have(content_length) not_equal(Some("5".to_owned())) }
        when generation_is_on_demand {
            let pregenerate = false;
            to preserves_status_on_the_first_request { have(status) equal(StatusCode::CREATED), have(marker) equal(Some("preserved".to_owned())), have(disk_exists) be_true }
        }
        when a_current_validator_is_sent {
            let constraint = Constraint::MatchCurrent;
            to reports_not_modified { have(status) equal(StatusCode::NOT_MODIFIED), have(body) equal(String::new()) }
        }
        when if_match_fails {
            let constraint = Constraint::FailedMatch;
            to reports_the_failed_precondition { have(status) equal(StatusCode::PRECONDITION_FAILED), have(body) equal(String::new()) }
        }
        when a_partial_range_is_requested {
            let constraint = Constraint::Range("bytes=0-3");
            to ignores_range_for_a_non_200_representation { have(status) equal(StatusCode::CREATED), have(body) contains_html("Static Headers"), have(content_range) be_none }
        }
        when the_range_is_unsatisfiable {
            let constraint = Constraint::Range("bytes=9000-9999");
            to ignores_range_for_a_non_200_representation { have(status) equal(StatusCode::CREATED), have(body) contains_html("Static Headers"), have(content_range) be_none }
        }
        when the_range_is_non_text {
            let constraint = Constraint::MalformedRange;
            to ignores_range_for_a_non_200_representation { have(status) equal(StatusCode::CREATED), have(body) contains_html("Static Headers"), have(content_range) be_none }
        }
    }
}

lets_expect::lets_expect! {
    expect(run_ntex(static_request(request))) as the_static_redirect {
        let constraint = Constraint::None;
        let pregenerate = true;
        let request = StaticRequest { kind: StaticAppKind::Redirect, uri: "/go", pregenerate, constraint, ..Default::default() };
        to preserves_the_redirect { have(status) equal(StatusCode::FOUND), have(location) equal(Some("/elsewhere".to_owned())) }
        when generation_is_on_demand {
            let pregenerate = false;
            to redirects_on_the_first_request { have(status) equal(StatusCode::FOUND), have(location) equal(Some("/elsewhere".to_owned())) }
        }
        when a_current_validator_is_sent {
            let constraint = Constraint::MatchCurrent;
            to keeps_redirecting { have(status) equal(StatusCode::FOUND), have(location) equal(Some("/elsewhere".to_owned())) }
        }
        when a_range_is_sent {
            let constraint = Constraint::Range("bytes=0-3");
            to does_not_emit_range_artifacts { have(status) equal(StatusCode::FOUND), have(location) equal(Some("/elsewhere".to_owned())), have(content_range) be_none, have(body) equal(String::new()) }
        }
    }
}

lets_expect::lets_expect! {
    expect(run_ntex(static_request(request))) as the_static_error_response {
        let uri = "/gone";
        let request = StaticRequest { kind: StaticAppKind::Status, uri, pregenerate: false, ..Default::default() };
        to preserves_the_client_error_without_caching { have(status) equal(StatusCode::NOT_FOUND), have(disk_exists) be_false }
        when the_render_has_a_server_error {
            let uri = "/server-error";
            to preserves_the_server_error_without_caching { have(status) equal(StatusCode::INTERNAL_SERVER_ERROR), have(disk_exists) be_false }
        }
    }
}

lets_expect::lets_expect! {
    expect(run_ntex(static_request(StaticRequest { kind: StaticAppKind::Framing, uri: "/framing", ..Default::default() }))) as the_static_file_framing {
        to preserves_the_application_marker { have(marker) equal(Some("keep-me".to_owned())), have(body) contains_html("Static Framing") }
        to preserves_the_explicit_html_media_type { have(content_type) equal(Some("text/html; charset=utf-8".to_owned())) }
        to uses_the_file_length { have(content_length) not_equal(Some("5".to_owned())) }
        to does_not_use_a_fake_encoding { have(encoding) be_none }
        to does_not_use_a_fake_transfer_encoding { have(transfer) be_none }
        to does_not_use_a_fake_content_range { have(content_range) be_none }
        to advertises_actual_range_support { have(ranges) equal(Some("bytes".to_owned())) }
        to uses_the_file_validator { have(etag.clone()) be_some, have(etag) not_equal(Some("\"bogus-etag\"".to_owned())) }
        to uses_the_file_modification_time { have(modified.clone()) be_some, have(modified) not_equal(Some("Thu, 01 Jan 1970 00:00:00 GMT".to_owned())) }
    }
}

lets_expect::lets_expect! {
    expect(run_ntex(static_request(request))) as the_static_path_rejection {
        let uri = "/files/../secret";
        let request = StaticRequest { kind: StaticAppKind::Splat, uri, pregenerate: false, ..Default::default() };
        to rejects_traversal { have(status) equal(StatusCode::NOT_FOUND), have(disk_exists) be_false }
        when the_path_contains_a_dotfile {
            let uri = "/files/.env";
            to rejects_the_dotfile { have(status) equal(StatusCode::NOT_FOUND), have(disk_exists) be_false }
        }
    }
}

#[derive(Debug)]
struct RegenerationObservation {
    listeners: usize,
    statuses: Vec<StatusCode>,
    bodies: Vec<String>,
}

async fn concurrent_regeneration(delete_after_first: bool) -> RegenerationObservation {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let root = temp_site_root("controlled_regeneration");
    let listeners = Arc::new(AtomicUsize::new(0));
    let app_fn = {
        let listeners = listeners.clone();
        move || {
            let listeners = listeners.clone();
            view! {
                <Router><Routes fallback=|| "missing">
                    <Route path=path!("/regen") ssr=SsrMode::Static(StaticRoute::new().regenerate(move |_| {
                        listeners.fetch_add(1, Ordering::SeqCst);
                        futures::stream::pending::<()>()
                    })) view=|| "regenerated"/>
                </Routes></Router>
            }
        }
    };
    let routes = crate::generate_route_list(app_fn.clone());
    let app = test::init_service(
        NtexApp::new()
            .state(static_options(&root))
            .configure(|cfg| register_leptos_routes(cfg, routes.clone(), app_fn.clone())),
    )
    .await;
    let barrier = crate::static_routes::test_hooks::two_misses(root.to_path_buf());
    let (first, second) = futures::join!(
        test::call_service(&app, test::TestRequest::with_uri("/regen").to_request()),
        test::call_service(&app, test::TestRequest::with_uri("/regen").to_request())
    );
    drop(barrier);
    let mut statuses = vec![first.status(), second.status()];
    let mut bodies = vec![
        String::from_utf8(test::read_body(first).await.to_vec()).unwrap(),
        String::from_utf8(test::read_body(second).await.to_vec()).unwrap(),
    ];
    if delete_after_first {
        std::fs::remove_file(root.join("regen.html")).unwrap();
        let barrier = crate::static_routes::test_hooks::two_misses(root.to_path_buf());
        let (first, second) = futures::join!(
            test::call_service(&app, test::TestRequest::with_uri("/regen").to_request()),
            test::call_service(&app, test::TestRequest::with_uri("/regen").to_request())
        );
        drop(barrier);
        statuses.extend([first.status(), second.status()]);
        bodies.extend([
            String::from_utf8(test::read_body(first).await.to_vec()).unwrap(),
            String::from_utf8(test::read_body(second).await.to_vec()).unwrap(),
        ]);
    }
    RegenerationObservation {
        listeners: listeners.load(Ordering::SeqCst),
        statuses,
        bodies,
    }
}

fn all_regenerated(bodies: &[String]) -> lets_expect::AssertionResult {
    if bodies.iter().all(|body| body.contains("regenerated")) {
        Ok(())
    } else {
        Err(lets_expect::AssertionError {
            message: vec![format!(
                "Expected regenerated HTML in every response, received {bodies:?}"
            )],
        })
    }
}

lets_expect::lets_expect! {
    expect(run_ntex(concurrent_regeneration(delete_after_first))) as the_controlled_static_regeneration {
        let delete_after_first = false;
        to serves_both_cold_callers { have(statuses) equal(vec![StatusCode::OK; 2]), have(bodies) all_regenerated }
        to installs_one_listener { have(listeners) equal(1_usize) }
        when the_persisted_file_is_deleted {
            let delete_after_first = true;
            to serves_the_next_cold_callers { have(statuses) equal(vec![StatusCode::OK; 4]), have(bodies) all_regenerated }
            to reuses_the_existing_listener { have(listeners) equal(1_usize) }
        }
    }
}

#[derive(Debug)]
struct RepeatedStaticObservation {
    statuses: Vec<StatusCode>,
    headers: Vec<Option<String>>,
    renders: usize,
}
async fn repeated_static_hits() -> RepeatedStaticObservation {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let root = temp_site_root("repeated_static_hits");
    let count = Arc::new(AtomicUsize::new(0));
    let app_fn = {
        let count = count.clone();
        move || {
            count.fetch_add(1, Ordering::Relaxed);
            StaticHeaderApp()
        }
    };
    let (routes, generator) = gen_route_list_with_ssg(app_fn.clone());
    generator.generate(&static_options(&root)).await;
    let app = test::init_service(
        NtexApp::new()
            .state(static_options(&root))
            .configure(|cfg| register_leptos_routes(cfg, routes.clone(), app_fn.clone())),
    )
    .await;
    let before = count.load(Ordering::Relaxed);
    let first =
        test::call_service(&app, test::TestRequest::with_uri("/headers").to_request()).await;
    let second =
        test::call_service(&app, test::TestRequest::with_uri("/headers").to_request()).await;
    let third =
        test::call_service(&app, test::TestRequest::with_uri("/headers").to_request()).await;
    let statuses = vec![first.status(), second.status(), third.status()];
    let headers = [&first, &second, &third]
        .into_iter()
        .map(|response| {
            response
                .headers()
                .get("x-static-cache")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        })
        .collect();
    RepeatedStaticObservation {
        statuses,
        headers,
        renders: count.load(Ordering::Relaxed) - before,
    }
}
lets_expect::lets_expect! {
    expect(run_ntex(repeated_static_hits())) as repeated_static_hits {
        to replays_the_status_every_time { have(statuses) equal(vec![StatusCode::CREATED; 3]) }
        to replays_the_headers_every_time { have(headers) equal(vec![Some("preserved".to_owned()); 3]) }
        to reuses_the_persisted_representation { have(renders) equal(0_usize) }
    }
}

struct PollUnwindWitness(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl Drop for PollUnwindWitness {
    fn drop(&mut self) {
        self.0.store(
            std::thread::panicking(),
            std::sync::atomic::Ordering::SeqCst,
        );
    }
}

struct OwnerReadingStream {
    stop: futures::channel::oneshot::Receiver<()>,
    value: StoredValue<usize>,
    panic_on_stop: bool,
    unwound: std::sync::Arc<std::sync::atomic::AtomicBool>,
    events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}
impl futures::Stream for OwnerReadingStream {
    type Item = ();
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<()>> {
        use std::future::Future;
        match std::pin::Pin::new(&mut self.stop).poll(cx) {
            std::task::Poll::Pending => std::task::Poll::Pending,
            std::task::Poll::Ready(_) if self.panic_on_stop => {
                let _witness = PollUnwindWitness(self.unwound.clone());
                panic!("regeneration poll probe")
            }
            std::task::Poll::Ready(_) => std::task::Poll::Ready(None),
        }
    }
}
impl Drop for OwnerReadingStream {
    fn drop(&mut self) {
        self.events
            .lock()
            .unwrap()
            .push(format!("stream:{:?}", self.value.try_get_value()));
    }
}
async fn terminated_regeneration_inner(
    panic_on_stop: bool,
    unwound: std::sync::Arc<std::sync::atomic::AtomicBool>,
    events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) -> Vec<String> {
    use std::sync::{Arc, Mutex};
    let root = temp_site_root("regeneration_eof");
    let (stop, receiver) = futures::channel::oneshot::channel();
    let receiver = Arc::new(Mutex::new(Some(receiver)));
    let app_fn = {
        let events = events.clone();
        move || {
            let events = events.clone();
            let receiver = receiver.clone();
            let unwound = unwound.clone();
            view! { <Router><Routes fallback=|| "missing">
                <Route path=path!("/terminal") ssr=SsrMode::Static(StaticRoute::new().regenerate(move |_| {
                    let events_for_cleanup = events.clone();
                    on_cleanup(move || events_for_cleanup.lock().unwrap().push("owner".to_owned()));
                    OwnerReadingStream { stop: receiver.lock().unwrap().take().unwrap(), value: StoredValue::new(7), panic_on_stop, unwound: unwound.clone(), events: events.clone() }
                })) view=|| "terminal"/>
            </Routes></Router> }
        }
    };
    let routes = crate::generate_route_list(app_fn.clone());
    let runtime = routes[0].runtime.clone();
    let app = test::init_service(
        NtexApp::new()
            .state(static_options(&root))
            .configure(|cfg| register_leptos_routes(cfg, routes.clone(), app_fn.clone())),
    )
    .await;
    let response =
        test::call_service(&app, test::TestRequest::with_uri("/terminal").to_request()).await;
    assert_eq!(response.status(), StatusCode::OK);
    stop.send(()).unwrap();
    ntex::time::timeout(
        ntex::time::Millis(1000),
        futures::future::poll_fn(|cx| {
            if events.lock().unwrap().len() == 2 {
                std::task::Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        }),
    )
    .await
    .expect("regeneration EOF did not finish its cleanup sequence");
    drop(runtime);
    events.lock().unwrap().clone()
}
fn terminated_regeneration(panic_on_stop: bool) -> Vec<String> {
    let unwound = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = events.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_ntex(terminated_regeneration_inner(
            panic_on_stop,
            unwound.clone(),
            observed,
        ))
    }));
    if let Err(payload) = result {
        assert_eq!(
            payload.downcast_ref::<&str>().copied(),
            Some("regeneration poll probe"),
            "unrelated panic must fail the scenario"
        );
    }
    assert_eq!(
        unwound.load(std::sync::atomic::Ordering::SeqCst),
        panic_on_stop,
        "the stream must actually unwind only for the panic scenario"
    );
    events.lock().unwrap().clone()
}
lets_expect::lets_expect! {
    expect(terminated_regeneration(panic_on_stop)) as the_terminated_regeneration_stream {
        let panic_on_stop = false;
        to drops_before_its_reactive_values_are_cleaned { equal(vec!["stream:Some(7)".to_owned(), "owner".to_owned()]) }
        when polling_the_stream_panics {
            let panic_on_stop = true;
            to preserves_the_same_cleanup_order { equal(vec!["stream:Some(7)".to_owned(), "owner".to_owned()]) }
        }
    }
}
async fn static_method_response(
    configuration: bool,
    allow_get: bool,
    method: ntex::http::Method,
) -> StatusCode {
    use crate::{LeptosRoutes, NtexRouteListing};
    let root = temp_site_root("static_method_contract");
    std::fs::create_dir_all(&*root).unwrap();
    std::fs::write(root.join("method.html"), "legacy-method").unwrap();
    let mut methods = vec![leptos_router::Method::Post];
    if allow_get {
        methods.push(leptos_router::Method::Get);
    }
    let routes = vec![NtexRouteListing::new(
        "/method".to_owned(),
        SsrMode::Static(StaticRoute::new()),
        methods,
        Vec::new(),
    )];
    let app = NtexApp::new().state(
        LeptosOptions::builder()
            .output_name("static_method_contract")
            .site_root(root.to_string_lossy().into_owned())
            .build(),
    );
    let app = if configuration {
        app.configure(|cfg| {
            cfg.leptos_routes(routes.clone(), || "rendered");
        })
    } else {
        app.leptos_routes(routes, || "rendered")
    };
    let app = test::init_service(app).await;
    test::call_service(
        &app,
        test::TestRequest::with_uri("/method")
            .method(method)
            .to_request(),
    )
    .await
    .status()
}
lets_expect::lets_expect! {
    expect(run_ntex(static_method_response(configuration, allow_get, method))) as the_static_method_dispatch {
        let configuration = false;
        let allow_get = false;
        let method = ntex::http::Method::POST;
        when registered_on_the_app {
            when only_post_is_listed {
                to accepts_the_listed_post { equal(StatusCode::OK) }
                when the_request_is_get { let method = ntex::http::Method::GET; to obeys_the_method_listing { equal(StatusCode::NOT_FOUND) } }
                when the_request_is_head { let method = ntex::http::Method::HEAD; to obeys_the_method_listing { equal(StatusCode::NOT_FOUND) } }
            }
            when get_is_also_listed {
                let allow_get = true;
                to accepts_the_listed_post { equal(StatusCode::OK) }
                when the_request_is_get { let method = ntex::http::Method::GET; to obeys_the_method_listing { equal(StatusCode::OK) } }
                when the_request_is_head { let method = ntex::http::Method::HEAD; to obeys_the_method_listing { equal(StatusCode::OK) } }
            }
        }
        when registered_on_service_configuration {
            let configuration = true;
            when only_post_is_listed {
                to accepts_the_listed_post { equal(StatusCode::OK) }
                when the_request_is_get { let method = ntex::http::Method::GET; to obeys_the_method_listing { equal(StatusCode::NOT_FOUND) } }
                when the_request_is_head { let method = ntex::http::Method::HEAD; to obeys_the_method_listing { equal(StatusCode::NOT_FOUND) } }
            }
            when get_is_also_listed {
                let allow_get = true;
                to accepts_the_listed_post { equal(StatusCode::OK) }
                when the_request_is_get { let method = ntex::http::Method::GET; to obeys_the_method_listing { equal(StatusCode::OK) } }
                when the_request_is_head { let method = ntex::http::Method::HEAD; to obeys_the_method_listing { equal(StatusCode::OK) } }
            }
        }
    }
}

#[derive(Clone)]
struct InheritedRegenerationContext(u8);
#[derive(Clone)]
struct AdditionalRegenerationContext(u8);
#[derive(Clone)]
struct RenderRegenerationContext(u8);

type RegenerationContext = (Option<u8>, Option<u8>, Option<u8>);
fn regeneration_context() -> RegenerationContext {
    (
        use_context::<InheritedRegenerationContext>().map(|value| value.0),
        use_context::<AdditionalRegenerationContext>().map(|value| value.0),
        use_context::<RenderRegenerationContext>().map(|value| value.0),
    )
}
#[derive(Clone, Debug, PartialEq)]
enum RegenerationScopeEvent {
    Context(&'static str, RegenerationContext),
    Drop(RegenerationContext, Option<usize>),
    Cleanup,
}
#[derive(Clone, Copy, PartialEq)]
enum RegenerationEnd {
    Eof,
    Panic,
}
#[derive(Clone, Copy)]
enum RegenerationCommand {
    Trigger,
    Finish,
}
struct ScopedSubscriptionProbe {
    commands: futures::channel::mpsc::Receiver<RegenerationCommand>,
    pending_before: Option<futures::channel::oneshot::Sender<()>>,
    pending_after: Option<futures::channel::oneshot::Sender<()>>,
    triggered: bool,
    end: RegenerationEnd,
    unwound: std::sync::Arc<std::sync::atomic::AtomicBool>,
    value: StoredValue<usize>,
    events: std::sync::Arc<std::sync::Mutex<Vec<RegenerationScopeEvent>>>,
}
impl futures::Stream for ScopedSubscriptionProbe {
    type Item = ();
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<()>> {
        match futures::Stream::poll_next(std::pin::Pin::new(&mut self.commands), cx) {
            std::task::Poll::Pending => {
                let (stage, signal) = if self.triggered {
                    ("pending_after", self.pending_after.take())
                } else {
                    ("pending_before", self.pending_before.take())
                };
                if let Some(signal) = signal {
                    self.events
                        .lock()
                        .unwrap()
                        .push(RegenerationScopeEvent::Context(
                            stage,
                            regeneration_context(),
                        ));
                    let _ = signal.send(());
                }
                std::task::Poll::Pending
            }
            std::task::Poll::Ready(Some(RegenerationCommand::Trigger)) => {
                self.events
                    .lock()
                    .unwrap()
                    .push(RegenerationScopeEvent::Context(
                        "trigger",
                        regeneration_context(),
                    ));
                self.triggered = true;
                std::task::Poll::Ready(Some(()))
            }
            std::task::Poll::Ready(Some(RegenerationCommand::Finish)) => {
                self.events
                    .lock()
                    .unwrap()
                    .push(RegenerationScopeEvent::Context(
                        "terminal",
                        regeneration_context(),
                    ));
                if self.end == RegenerationEnd::Panic {
                    let _witness = PollUnwindWitness(self.unwound.clone());
                    panic!("controlled regeneration context panic");
                }
                std::task::Poll::Ready(None)
            }
            std::task::Poll::Ready(None) => panic!("fixture sender must outlive the subscription"),
        }
    }
}
impl Drop for ScopedSubscriptionProbe {
    fn drop(&mut self) {
        self.events
            .lock()
            .unwrap()
            .push(RegenerationScopeEvent::Drop(
                regeneration_context(),
                self.value.try_get_value(),
            ));
    }
}

async fn regeneration_scope_inner(
    startup: bool,
    end: RegenerationEnd,
    unwound: std::sync::Arc<std::sync::atomic::AtomicBool>,
    events: std::sync::Arc<std::sync::Mutex<Vec<RegenerationScopeEvent>>>,
    keep_sender: std::sync::Arc<
        std::sync::Mutex<Option<futures::channel::mpsc::Sender<RegenerationCommand>>>,
    >,
) {
    use crate::LeptosRoutes;
    use futures::SinkExt;
    use std::sync::{Arc, Mutex};
    let body = async move {
        let root = temp_site_root("regeneration_scope");
        let options = static_options(&root);
        let (mut commands, receiver) = futures::channel::mpsc::channel(1);
        *keep_sender.lock().unwrap() = Some(commands.clone());
        let (pending_before, first_pending) = futures::channel::oneshot::channel();
        let (pending_after, next_pending) = futures::channel::oneshot::channel();
        let (cleaned, cleanup_done) = futures::channel::oneshot::channel();
        let controls = Arc::new(Mutex::new(Some((
            receiver,
            pending_before,
            pending_after,
            cleaned,
        ))));
        let app_fn = {
            let events = events.clone();
            move || {
                provide_context(RenderRegenerationContext(79));
                let events = events.clone();
                let controls = controls.clone();
                let unwound = unwound.clone();
                view! { <Router><Routes fallback=|| "missing">
                    <Route path=path!("/scope") ssr=SsrMode::Static(StaticRoute::new().regenerate(move |_| {
                        events.lock().unwrap().push(RegenerationScopeEvent::Context("factory", regeneration_context()));
                        let (receiver, pending_before, pending_after, cleaned) = controls.lock().unwrap().take().unwrap();
                        let cleanup_events = events.clone();
                        on_cleanup(move || {
                            cleanup_events.lock().unwrap().push(RegenerationScopeEvent::Cleanup);
                            let _ = cleaned.send(());
                        });
                        ScopedSubscriptionProbe {
                            commands: receiver, pending_before: Some(pending_before), pending_after: Some(pending_after),
                            triggered: false, end, unwound: unwound.clone(), value: StoredValue::new(7), events: events.clone(),
                        }
                    })) view=|| "contextual generation"/>
                </Routes></Router> }
            }
        };
        let additional = || provide_context(AdditionalRegenerationContext(31));
        let (routes, generator) =
            gen_route_list_with_exclusions_and_ssg_and_context(app_fn.clone(), None, additional);
        if startup {
            generator.generate(&options).await;
        } else {
            drop(generator);
            let app = test::init_service(
                NtexApp::new()
                    .state(options)
                    .leptos_routes_with_context(routes, additional, app_fn),
            )
            .await;
            let response =
                test::call_service(&app, test::TestRequest::with_uri("/scope").to_request()).await;
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "on-demand initial render"
            );
            let _ = test::read_body(response).await;
        }
        first_pending
            .await
            .expect("subscription reaches initial Pending");
        commands.send(RegenerationCommand::Trigger).await.unwrap();
        next_pending
            .await
            .expect("regeneration completes before next Pending");
        commands.send(RegenerationCommand::Finish).await.unwrap();
        cleanup_done
            .await
            .expect("subscription owner cleanup completes");
    };
    let caller = Owner::new();
    caller.with(|| provide_context(InheritedRegenerationContext(53)));
    let body = caller.with(|| leptos::reactive::computed::ScopedFuture::new(body));
    crate::owner::OwnerContextFuture::new(body).await;
}

fn regeneration_scope(startup: bool, end: RegenerationEnd) -> Vec<RegenerationScopeEvent> {
    let unwound = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let panic_witness = unwound.clone();
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = events.clone();
    let keep_sender = std::sync::Arc::new(std::sync::Mutex::new(None));
    let kept = keep_sender.clone();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_ntex(async move {
            ntex::time::timeout(
                ntex::time::Millis(2000),
                regeneration_scope_inner(startup, end, panic_witness, observed, kept),
            )
            .await
            .expect("controlled subscription lifecycle completes");
        })
    }));
    assert_eq!(
        unwound.load(std::sync::atomic::Ordering::SeqCst),
        end == RegenerationEnd::Panic,
        "the subscription must actually unwind only for the panic scenario"
    );
    if let Err(payload) = outcome {
        assert_eq!(
            payload.downcast_ref::<&str>().copied(),
            Some("controlled regeneration context panic"),
            "a timeout or unrelated panic must not satisfy the terminal scenario"
        );
    }
    drop(keep_sender);
    events.lock().unwrap().clone()
}
fn preserves_regeneration_scope(
    actual: &Vec<RegenerationScopeEvent>,
) -> lets_expect::AssertionResult {
    // Each SSR render gets a new root Owner: unrelated caller context must not
    // cross that boundary, while additional and render context must survive.
    let context = (None, Some(31), Some(79));
    lets_expect::equal(vec![
        RegenerationScopeEvent::Context("factory", context),
        RegenerationScopeEvent::Context("pending_before", context),
        RegenerationScopeEvent::Context("trigger", context),
        RegenerationScopeEvent::Context("pending_after", context),
        RegenerationScopeEvent::Context("terminal", context),
        RegenerationScopeEvent::Drop(context, Some(7)),
        RegenerationScopeEvent::Cleanup,
    ])(actual)
}
lets_expect::lets_expect! {
    expect(regeneration_scope(startup, end)) as reactive_scope_of_static_regeneration {
        let startup = true;
        let end = RegenerationEnd::Eof;
        to preserves_its_scope_through_completion { preserves_regeneration_scope }
        when subscription_poll_panics {
            let end = RegenerationEnd::Panic;
            to preserves_its_scope_during_unwind { preserves_regeneration_scope }
        }
        when first_render_is_on_demand {
            let startup = false;
            to preserves_its_scope_through_completion { preserves_regeneration_scope }
            when subscription_poll_panics {
                let end = RegenerationEnd::Panic;
                to preserves_its_scope_during_unwind { preserves_regeneration_scope }
            }
        }
    }
}
