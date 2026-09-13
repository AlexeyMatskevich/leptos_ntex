use super::*;
use crate::{handle_server_fns, register_explicit, register_leptos_routes};
use lets_expect::*;
use ntex::http::{
    Method, StatusCode,
    header::{self, HeaderMap},
};
use ntex::web::{App as NtexApp, test};
use server_fn::{ServerFn, redirect::REDIRECT_HEADER};

#[server(name = EchoNameGet, prefix = "/api", endpoint = "echo_name_get", input = server_fn::codec::GetUrl, server = crate::NtexServerFnBackend)]
async fn echo_name_get(name: String) -> Result<String, ServerFnError> {
    Ok(format!("Hello, {name}"))
}

#[derive(Clone)]
struct RequestSpec {
    uri: String,
    method: Method,
    body: String,
    accept: Option<&'static str>,
    referer: Option<&'static str>,
    scheme: Option<&'static str>,
    limit: usize,
    declared: Option<&'static str>,
}
impl Default for RequestSpec {
    fn default() -> Self {
        Self {
            uri: EchoName::PATH.into(),
            method: Method::POST,
            body: "name=Alice".into(),
            accept: Some("application/json"),
            referer: None,
            scheme: None,
            limit: crate::DEFAULT_PAYLOAD_LIMIT,
            declared: None,
        }
    }
}
#[derive(Debug)]
struct ResponseSnapshot {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

async fn response(spec: RequestSpec, config_registration: bool) -> ResponseSnapshot {
    register_explicit::<EchoName>();
    register_explicit::<EchoNameGet>();
    register_explicit::<DrainStreamingInput>();
    register_explicit::<RedirectToAbout>();
    register_explicit::<AlwaysErr>();
    register_explicit::<GuardedByRedirect>();
    register_explicit::<GuardedByNotModified>();
    register_explicit::<MultiLocation>();
    register_explicit::<ProbePath>();
    let mut app =
        NtexApp::new().state(crate::LeptosServerFnConfig::new().with_payload_limit(spec.limit));
    if config_registration {
        let routes = gen_route_list(UnitApp);
        app = app.configure(|cfg| {
            register_leptos_routes(cfg, routes.clone(), unit_shell);
        });
    } else {
        app = app.route("/api/{tail}*", handle_server_fns());
    }
    let app = test::init_service(app).await;
    let mut request = test::TestRequest::with_uri(&spec.uri)
        .method(spec.method)
        .header(header::HOST, "example.test:8080")
        .header(
            header::CONTENT_TYPE,
            if spec.uri == DrainStreamingInput::PATH {
                "application/octet-stream"
            } else {
                "application/x-www-form-urlencoded"
            },
        )
        .set_payload(spec.body);
    if let Some(value) = spec.accept {
        request = request.header(header::ACCEPT, value);
    }
    if let Some(value) = spec.referer {
        request = request.header(header::REFERER, value);
    }
    if let Some(value) = spec.scheme {
        request = request.header("X-Forwarded-Proto", value);
    }
    if let Some(value) = spec.declared {
        request = request.header(header::CONTENT_LENGTH, value);
    }
    let response = test::call_service(&app, request.to_request()).await;
    let status = response.status();
    let headers = response.headers().clone();
    let body = test::read_body(response).await.to_vec();
    ResponseSnapshot {
        status,
        headers,
        body,
    }
}
fn have_status(expected: StatusCode) -> impl Fn(&ResponseSnapshot) -> AssertionResult {
    move |response| equal(expected)(&response.status)
}
fn have_header(
    name: &'static str,
    expected: Option<&'static str>,
) -> impl Fn(&ResponseSnapshot) -> AssertionResult {
    move |response| {
        let actual = response.headers.get(name).and_then(|v| v.to_str().ok());
        if actual == expected {
            Ok(())
        } else {
            Err(AssertionError::new(vec![format!(
                "{name}: expected {expected:?}, got {actual:?}"
            )]))
        }
    }
}
fn have_body(expected: impl Into<String>) -> impl Fn(&ResponseSnapshot) -> AssertionResult {
    let expected = expected.into();
    move |response| {
        if response.body == expected.as_bytes() {
            Ok(())
        } else {
            Err(AssertionError::new(vec![format!(
                "expected body {expected:?}, got {:?}",
                String::from_utf8_lossy(&response.body)
            )]))
        }
    }
}
fn have_client_redirect(response: &ResponseSnapshot) -> AssertionResult {
    equal(true)(&response.headers.contains_key(REDIRECT_HEADER))
}
fn target_same_origin_form(response: &ResponseSnapshot) -> AssertionResult {
    let location = response
        .headers
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok());
    let valid = location
        .and_then(|s| s.parse::<ntex::http::Uri>().ok())
        .is_some_and(|uri| {
            uri.scheme_str() == Some("http")
                && uri.authority().map(|v| v.as_str()) == Some("example.test:8080")
                && uri.path() == "/form"
                && uri.query().is_none_or(str::is_empty)
        });
    if valid {
        Ok(())
    } else {
        Err(AssertionError::new(vec![format!(
            "expected same-origin form target without query data, got {location:?}"
        )]))
    }
}
fn carry_form_error(response: &ResponseSnapshot) -> AssertionResult {
    let location = response
        .headers
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok());
    if location.is_some_and(|s| {
        s.starts_with("http://example.test:8080/form?")
            && s.contains("__path=")
            && s.contains("__err=")
    }) {
        Ok(())
    } else {
        Err(AssertionError::new(vec![format!(
            "missing form error query in Location: {location:?}"
        )]))
    }
}
fn have_single_location(response: &ResponseSnapshot) -> AssertionResult {
    let values: Vec<_> = response
        .headers
        .get_all(header::LOCATION)
        .map(|v| v.to_str().unwrap_or("<non-text>"))
        .collect();
    equal(vec!["/two"])(&values)
}
fn form_spec(accept: &'static str, referer: Option<&'static str>, error: bool) -> RequestSpec {
    RequestSpec {
        uri: if error {
            AlwaysErr::PATH
        } else {
            EchoName::PATH
        }
        .into(),
        body: if error { "" } else { "name=Alice" }.into(),
        accept: Some(accept),
        referer,
        ..Default::default()
    }
}
fn payload_spec(streaming: bool, length: usize, limit: usize) -> RequestSpec {
    RequestSpec {
        uri: if streaming {
            DrainStreamingInput::PATH
        } else {
            EchoName::PATH
        }
        .into(),
        body: if streaming {
            "A".repeat(length)
        } else {
            format!("name={}", "A".repeat(length - 5))
        },
        limit,
        ..Default::default()
    }
}
fn redirect_parts(path: &'static str) -> (Option<StatusCode>, Option<String>) {
    let _scope = crate::RequestScope::new();
    let request = test::TestRequest::with_uri("/")
        .header(header::ACCEPT, "text/html")
        .to_http_request();
    let owner = leptos::reactive::owner::Owner::new();
    owner.with(|| {
        provide_context(crate::Request::new(&request));
        let options = crate::ResponseOptions::default();
        provide_context(options.clone());
        crate::redirect(path);
        let parts = options.0.read().unwrap();
        (
            parts.status,
            parts
                .headers
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
        )
    })
}
fn registration_count(repeat: bool) -> usize {
    register_explicit::<EchoName>();
    if repeat {
        register_explicit::<EchoName>();
        register_explicit::<EchoName>();
    }
    crate::server_fn_paths()
        .filter(|(p, m)| *p == EchoName::PATH && *m == Method::POST)
        .count()
}
fn lookup(path: &'static str, method: Method) -> bool {
    register_explicit::<EchoName>();
    crate::get_server_fn_service(path, &method).is_some()
}
fn listed_paths() -> (bool, bool) {
    register_explicit::<EchoName>();
    register_explicit::<RedirectToAbout>();
    let paths: Vec<_> = crate::server_fn_paths().collect();
    (
        paths
            .iter()
            .any(|(p, m)| *p == EchoName::PATH && *m == Method::POST),
        paths
            .iter()
            .any(|(p, m)| *p == RedirectToAbout::PATH && *m == Method::POST),
    )
}

/// A request body the catch-all cannot hand to any function: the handler must
/// still consume it (within the limit) so that its error response leaves on an
/// orderly close rather than a reset racing the response.
#[derive(Clone, Copy)]
enum UnusedBody {
    UnknownEndpoint,
    WrongMethod,
    BeyondLimit,
}
async fn unused_body(case: UnusedBody) -> (StatusCode, bool) {
    use futures::StreamExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    register_explicit::<EchoName>();
    let limit = 64;
    let app = test::init_service(
        NtexApp::new()
            .state(crate::LeptosServerFnConfig::new().with_payload_limit(limit))
            .route("/api/{tail}*", handle_server_fns()),
    )
    .await;
    let (uri, method) = match case {
        UnusedBody::UnknownEndpoint | UnusedBody::BeyondLimit => {
            ("/api/does_not_exist", Method::POST)
        }
        UnusedBody::WrongMethod => (EchoName::PATH, Method::GET),
    };
    let chunks = if matches!(case, UnusedBody::BeyondLimit) {
        8
    } else {
        2
    };
    let drained = std::sync::Arc::new(AtomicBool::new(false));
    let observed = drained.clone();
    let body = futures::stream::iter((0..chunks).map(|_| {
        Ok::<_, ntex::http::error::PayloadError>(ntex::util::Bytes::from(vec![b'a'; 16]))
    }))
    .chain(futures::stream::poll_fn(move |_| {
        observed.store(true, Ordering::SeqCst);
        std::task::Poll::Ready(None)
    }));
    let mut request = test::TestRequest::with_uri(uri)
        .method(method)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .to_request();
    request.replace_payload(ntex::http::Payload::Stream(Box::pin(body)));
    let response = test::call_service(&app, request).await;
    let status = response.status();
    let _ = test::read_body(response).await;
    (status, drained.load(Ordering::SeqCst))
}

const MISSING_DIAGNOSTIC: &str = "Could not find a server function at the route /api/does_not_exist. \n\nIt's likely that either\n1. The API prefix you specify in the `#[server]` macro doesn't match the prefix at which your server function handler is mounted, or\n2. You are on a platform that doesn't support automatic server function registration and you need to call register_explicit() on the server function type, somewhere in your `main` function.";

lets_expect! {
    expect(run_ntex(response(spec, false))) as server_function_dispatch {
        let spec = RequestSpec::default();
        to dispatch_post { have_status(StatusCode::OK), have_body("\"Hello, Alice\"") }
        when registered_function_uses_get {
            let spec = RequestSpec { uri: format!("{}?name=Carol", EchoNameGet::PATH), method: Method::GET, body: String::new(), ..Default::default() };
            to dispatch_query { have_status(StatusCode::OK), have_body("\"Hello, Carol\"") }
        }
        when method_is_wrong {
            let spec = RequestSpec { method: Method::GET, ..Default::default() };
            to report_allowed_method { have_status(StatusCode::METHOD_NOT_ALLOWED), have_header("allow", Some("POST")) }
        }
        when function_is_missing {
            let spec = RequestSpec { uri: "/api/does_not_exist".into(), ..Default::default() };
            to explain_missing_registration { have_status(StatusCode::BAD_REQUEST), have_body(MISSING_DIAGNOSTIC) }
        }
    }
    expect(run_ntex(unused_body(case))) as unused_request_body {
        let case = UnusedBody::UnknownEndpoint;
        to is_consumed_before_the_rejection { equal((StatusCode::BAD_REQUEST, true)) }
        when the_method_is_wrong {
            let case = UnusedBody::WrongMethod;
            to is_consumed_before_the_rejection { equal((StatusCode::METHOD_NOT_ALLOWED, true)) }
        }
        when the_body_exceeds_the_limit {
            let case = UnusedBody::BeyondLimit;
            to is_abandoned_at_the_limit { equal((StatusCode::BAD_REQUEST, false)) }
        }
    }
    expect(run_ntex(response(RequestSpec { body: "name=Bob".into(), ..Default::default() }, true))) as service_config_registration {
        to dispatch_without_catchall { have_status(StatusCode::OK), have_body("\"Hello, Bob\"") }
    }
    expect(run_ntex(response(payload_spec(false, length, limit), false))) as buffered_payload {
        let length = 8usize;
        let limit = 32usize;
        to accept_small_body { have_status(StatusCode::OK), have_body("\"Hello, AAA\"") }
        when size_equals_limit {
            let length = 32usize;
            to accept_inclusive_limit { have_status(StatusCode::OK), have_body(format!("\"Hello, {}\"", "A".repeat(27))) }
        }
        when size_is_one_above_limit {
            let length = 33usize;
            to reject_body { have_status(StatusCode::PAYLOAD_TOO_LARGE), have_body("payload exceeds limit of 32 bytes") }
        }
        when size_is_far_above_limit {
            let length = 105usize;
            to report_configured_limit { have_status(StatusCode::PAYLOAD_TOO_LARGE), have_body("payload exceeds limit of 32 bytes") }
        }
    }
    expect(run_ntex(response(payload_spec(true, length, 16), false))) as drained_stream_payload {
        let length = 8usize;
        to drain_all_bytes { have_status(StatusCode::OK), have_body("8") }
        when size_equals_limit { let length = 16usize; to accept_inclusive_limit { have_status(StatusCode::OK), have_body("16") } }
        when size_is_one_above_limit { let length = 17usize; to reject_before_response { have_status(StatusCode::PAYLOAD_TOO_LARGE) } }
        when size_is_far_above_limit { let length = 100usize; to reject_before_response { have_status(StatusCode::PAYLOAD_TOO_LARGE) } }
    }
    expect(run_ntex(response(RequestSpec { body: "name=Bob".into(), limit: 32, declared, ..Default::default() }, false))) as declared_payload {
        let declared = None;
        to accept_small_actual_body { have_status(StatusCode::OK), have_body("\"Hello, Bob\"") }
        when declared_size_exceeds_limit { let declared = Some("999999"); to reject_before_reading { have_status(StatusCode::PAYLOAD_TOO_LARGE) } }
    }
    expect(run_ntex(response(RequestSpec { uri: RedirectToAbout::PATH.into(), body: String::new(), accept: Some(accept), ..Default::default() }, false))) as explicit_redirect {
        let accept = "text/html";
        to redirect_html_client { have_status(StatusCode::FOUND), have_header("location", Some("/about")) }
        when client_accepts_json { let accept = "application/json"; to signal_client_navigation { have_status(StatusCode::OK), have_header("location", Some("/about")), have_client_redirect } }
    }
    expect(run_ntex(response(form_spec(accept, referer, false), false))) as successful_form_fallback {
        let accept = "text/html";
        let referer = Some("http://example.test:8080/form");
        to normalize_same_origin { have_status(StatusCode::FOUND), have_header("location", Some("/form")) }
        when referer_is_missing { let referer = None; to use_root { have_status(StatusCode::FOUND), have_header("location", Some("/")) } }
        when referer_uses_other_port { let referer = Some("http://example.test:9090/form"); to omit_location { have_status(StatusCode::OK), have_header("location", None) } }
        when referer_is_protocol_relative { let referer = Some("//other.test/form"); to omit_location { have_status(StatusCode::OK), have_header("location", None) } }
        // server_fn currently uses a loose HTML check. Preserve its same-origin
        // q=0 fallback until that dependency contract changes; keep the origin guard.
        when html_has_zero_quality {
            let accept = "text/html;q=0";
            to preserve_dependency_redirect { have_status(StatusCode::FOUND), target_same_origin_form }
            when referer_uses_other_port { let referer = Some("http://example.test:9090/form"); to omit_location { have_status(StatusCode::OK), have_header("location", None) } }
        }
    }
    expect(run_ntex(response(RequestSpec { scheme: Some("https"), ..form_spec("text/html", Some("http://example.test:8080/form"), false) }, false))) as different_scheme_form {
        to omit_location { have_status(StatusCode::OK), have_header("location", None) }
    }
    expect(run_ntex(response(form_spec(accept, referer, true), false))) as failed_form_fallback {
        let accept = "text/html";
        let referer = Some("http://example.test:8080/form");
        to preserve_form_error { have_status(StatusCode::FOUND), carry_form_error }
        when referer_has_other_origin { let referer = Some("http://other.test/form"); to preserve_error_status { have_status(StatusCode::INTERNAL_SERVER_ERROR), have_header("location", None) } }
        when html_has_zero_quality {
            let accept = "text/html;q=0";
            to preserve_form_error { have_status(StatusCode::FOUND), carry_form_error }
            when referer_has_other_origin { let referer = Some("http://other.test/form"); to preserve_error_status { have_status(StatusCode::INTERNAL_SERVER_ERROR), have_header("location", None) } }
        }
    }
    expect(run_ntex(response(RequestSpec { uri: GuardedByRedirect::PATH.into(), body: String::new(), accept: Some(accept), referer, ..Default::default() }, false))) as middleware_redirect {
        let accept = "application/json";
        let referer = None;
        to preserve_middleware_response { have_status(StatusCode::FOUND), have_header("location", Some("/login")) }
        when client_is_html_q_zero_form {
            let accept = "text/html;q=0";
            let referer = Some("http://example.test:8080/dashboard");
            to preserve_middleware_response { have_status(StatusCode::FOUND), have_header("location", Some("/login")) }
        }
    }
    expect(run_ntex(response(RequestSpec { uri: GuardedByNotModified::PATH.into(), body: String::new(), ..Default::default() }, false))) as middleware_not_modified {
        to preserve_middleware_response { have_status(StatusCode::NOT_MODIFIED), have_header("location", None) }
    }
    expect(run_ntex(response(RequestSpec { uri: AlwaysErr::PATH.into(), body: String::new(), accept: None, ..Default::default() }, false))) as raw_function_error {
        to report_error_encoding { have_status(StatusCode::INTERNAL_SERVER_ERROR), have_header("content-type", Some("text/plain")) }
    }
    expect(run_ntex(response(RequestSpec { uri: MultiLocation::PATH.into(), body: String::new(), ..Default::default() }, false))) as response_options_location {
        to replace_singleton { have_status(StatusCode::OK), have_single_location }
    }
    expect(run_ntex(response(RequestSpec { uri: ProbePath::PATH.into(), body: String::new(), ..Default::default() }, false))) as request_path_extraction {
        to return_request_path { have_status(StatusCode::OK), have_body("\"/api/probe_path\"") }
    }
    expect(run_ntex(async move { redirect_parts(path) })) as redirect_options {
        let path = "/about";
        to set_redirect { equal((Some(StatusCode::FOUND), Some("/about".to_string()))) }
        when target_has_invalid_header_bytes { let path = "/about\r\ninvalid"; to leave_options_unchanged { equal((None, None)) } }
    }
    expect(registration_count(repeat)) as explicit_registration {
        let repeat = false;
        to contain_one_entry { equal(1) }
        when registration_is_repeated { let repeat = true; to contain_one_entry { equal(1) } }
    }
    expect(lookup(path, method)) as registered_service_lookup {
        let path = EchoName::PATH;
        let method = Method::POST;
        to find_registered_service { be_true }
        when method_is_wrong { let method = Method::GET; to find_no_service { be_false } }
        when path_is_missing {
            let path = "/api/does_not_exist";
            to find_no_service { be_false }
            when method_is_get { let method = Method::GET; to find_no_service { be_false } }
        }
    }
    expect(listed_paths()) as server_function_listing { to include_both_registered_methods { equal((true, true)) } }
}

#[derive(Clone, Copy)]
enum Rejection {
    MissingUpgrade,
    MissingVersion,
    UnsupportedVersion,
    PayloadOverflow,
}

// The rejected response owns its body metadata; the application has already
// supplied independent headers and metadata for a response that never completed.
fn rejected_response_context() {
    let options = use_context::<crate::ResponseOptions>().unwrap();
    options.set_status(StatusCode::ACCEPTED);
    options.append_header(
        header::SET_COOKIE,
        header::HeaderValue::from_static("session=one; HttpOnly"),
    );
    options.append_header(
        header::SET_COOKIE,
        header::HeaderValue::from_static("csrf=two; SameSite=Lax"),
    );
    options.append_header(header::VARY, header::HeaderValue::from_static("Accept"));
    options.append_header(header::VARY, header::HeaderValue::from_static("Origin"));
    options.insert_header(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("private, no-store"),
    );
    options.insert_header(
        header::LOCATION,
        header::HeaderValue::from_static("/account"),
    );
    options.append_header(
        header::HeaderName::from_static("x-application"),
        header::HeaderValue::from_static("first"),
    );
    options.append_header(
        header::HeaderName::from_static("x-application"),
        header::HeaderValue::from_static("second"),
    );
    for (name, value) in [
        (header::CONTENT_TYPE, "application/json"),
        (header::CONTENT_LENGTH, "999"),
        (header::CONTENT_ENCODING, "gzip"),
        (header::CONTENT_RANGE, "bytes 0-998/999"),
        (header::CONTENT_LANGUAGE, "fr"),
        (header::CONTENT_LOCATION, "/old-representation"),
        (header::TRANSFER_ENCODING, "chunked"),
        (header::ETAG, "\"old\""),
        (header::LAST_MODIFIED, "Wed, 21 Oct 2015 07:28:00 GMT"),
        (header::ACCEPT_RANGES, "bytes"),
        (header::SEC_WEBSOCKET_VERSION, "999"),
    ] {
        options.insert_header(name, header::HeaderValue::from_static(value));
    }
}

async fn rejected_response(rejection: Rejection, per_path: bool) -> ResponseSnapshot {
    use crate::LeptosRoutes;
    register_explicit::<EchoWebsocket>();
    register_explicit::<EchoName>();
    let app = NtexApp::new().state(crate::LeptosServerFnConfig::new().with_payload_limit(1));
    let app = if per_path {
        app.leptos_routes_with_context(Vec::new(), rejected_response_context, || ())
    } else {
        app.route(
            "/api/{tail}*",
            crate::handle_server_fns_with_context(rejected_response_context),
        )
    };
    let app = test::init_service(app).await;
    let request = match rejection {
        Rejection::PayloadOverflow => test::TestRequest::with_uri(EchoName::PATH)
            .method(Method::POST)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .set_payload("name=Alice"),
        Rejection::MissingUpgrade => {
            test::TestRequest::with_uri(EchoWebsocket::PATH).method(Method::GET)
        }
        Rejection::MissingVersion => test::TestRequest::with_uri(EchoWebsocket::PATH)
            .method(Method::GET)
            .header(header::UPGRADE, "websocket")
            .header(header::CONNECTION, "upgrade"),
        Rejection::UnsupportedVersion => test::TestRequest::with_uri(EchoWebsocket::PATH)
            .method(Method::GET)
            .header(header::UPGRADE, "websocket")
            .header(header::CONNECTION, "upgrade")
            .header(header::SEC_WEBSOCKET_VERSION, "999"),
    };
    let response = test::call_service(&app, request.to_request()).await;
    ResponseSnapshot {
        status: response.status(),
        headers: response.headers().clone(),
        body: test::read_body(response).await.to_vec(),
    }
}

fn completed_rejection(rejection: Rejection) -> impl Fn(&ResponseSnapshot) -> AssertionResult {
    move |response| {
        let (status, body, content_type) = match rejection {
            Rejection::PayloadOverflow => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload exceeds limit of 1 bytes",
                Some("text/plain; charset=utf-8"),
            ),
            _ => (StatusCode::BAD_REQUEST, "", None),
        };
        have_status(status)(response)?;
        have_body(body)(response)?;
        have_header("content-type", content_type)(response)?;
        for name in [
            "content-length",
            "content-encoding",
            "content-range",
            "content-language",
            "content-location",
            "transfer-encoding",
            "etag",
            "last-modified",
            "accept-ranges",
        ] {
            have_header(name, None)(response)?;
        }
        have_header("cache-control", Some("private, no-store"))(response)?;
        have_header("location", Some("/account"))(response)?;
        let values = |name| {
            response
                .headers
                .get_all(name)
                .map(|v| v.to_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        equal(vec![
            "session=one; HttpOnly".to_owned(),
            "csrf=two; SameSite=Lax".to_owned(),
        ])(&values(header::SET_COOKIE))?;
        equal(vec!["Accept".to_owned(), "Origin".to_owned()])(&values(header::VARY))?;
        equal(vec!["first".to_owned(), "second".to_owned()])(&values(
            header::HeaderName::from_static("x-application"),
        ))?;
        let versions = if matches!(rejection, Rejection::UnsupportedVersion) {
            vec!["13, 8, 7".to_owned()]
        } else {
            Vec::new()
        };
        equal(versions)(&values(header::SEC_WEBSOCKET_VERSION))
    }
}

lets_expect! {
    expect(run_ntex(rejected_response(rejection, per_path))) as rejected_server_response_completion {
        let rejection = Rejection::MissingUpgrade;
        let per_path = false;
        to preserves_application_headers_and_the_handshake_error { completed_rejection(rejection) }
        when registration_is_per_path {
            let per_path = true;
            to preserves_application_headers_and_the_handshake_error { completed_rejection(rejection) }
        }
        when the_version_is_missing {
            let rejection = Rejection::MissingVersion;
            to preserves_application_headers_and_the_version_error { completed_rejection(rejection) }
            when registration_is_per_path {
                let per_path = true;
                to preserves_application_headers_and_the_version_error { completed_rejection(rejection) }
            }
        }
        when the_version_is_unsupported {
            let rejection = Rejection::UnsupportedVersion;
            to preserves_application_headers_and_supported_versions { completed_rejection(rejection) }
            when registration_is_per_path {
                let per_path = true;
                to preserves_application_headers_and_supported_versions { completed_rejection(rejection) }
            }
        }
        when the_payload_exceeds_the_budget {
            let rejection = Rejection::PayloadOverflow;
            to preserves_application_headers_and_the_payload_error { completed_rejection(rejection) }
            when registration_is_per_path {
                let per_path = true;
                to preserves_application_headers_and_the_payload_error { completed_rejection(rejection) }
            }
        }
    }
}
