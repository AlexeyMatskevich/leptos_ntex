//! End-to-end integration tests that boot a real ntex server over TCP and
//! issue HTTP requests through the ntex test client. These exercise the
//! public `leptos_ntex_unofficial` API from outside the crate (no `cfg(test)` access),
//! so they catch visibility and API-shape regressions that the in-crate
//! tests cannot.
//!
//! Note: these call `generate_route_list` directly, without the in-crate test
//! suite's `ROUTE_GEN_VS_RENDER` serialization. That workaround guards the
//! upstream `IS_SUPPRESSING_RESOURCE_LOAD` race, which only bites a render that
//! first-polls a `Resource` during a concurrent generation window. This binary
//! is a separate process (the lib static would not be shared anyway) and its
//! `App` has no `<Suspense>`/`Resource`, so there is no victim render to
//! protect here.

use leptos::config::LeptosOptions;
use leptos::prelude::*;
use leptos_meta::{MetaTags, Title, provide_meta_context};
use leptos_ntex_unofficial::{
    LeptosRoutes, file_and_error_handler, generate_route_list, handle_server_fns,
    register_explicit, register_leptos_routes, site_pkg_dir_service,
};
use leptos_router::{
    SsrMode,
    components::{Route, Router, Routes},
    path,
    static_routes::StaticRoute,
};
use lets_expect::*;
use ntex::http::{
    Method, StatusCode,
    header::{CONTENT_TYPE, HeaderMap},
};
use ntex::web::{App as NtexApp, test};
use server_fn::ServerFn;
use server_fn::codec::Cbor;
use std::{
    future::Future,
    path::{Path, PathBuf},
};

#[component]
fn App() -> impl IntoView {
    provide_meta_context();
    view! {
        <Router>
            <main>
                <Routes fallback=|| view! { <h1>"Not Found"</h1> }>
                    <Route path=path!("/")
                        view=|| view! {
                            <>
                                <Title text="Home"/>
                                <h1>"Integration Home"</h1>
                            </>
                        }
                    />
                    <Route path=path!("/about")
                        view=|| view! { <h1>"Integration About"</h1> }
                    />
                    <Route path=path!("/static")
                        ssr=SsrMode::Static(StaticRoute::new())
                        view=|| view! { <h1>"Integration Static"</h1> }
                    />
                </Routes>
            </main>
        </Router>
    }
}

fn shell() -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head><meta charset="utf-8"/><MetaTags/></head>
            <body><App/></body>
        </html>
    }
}

fn shell_with_options(_: LeptosOptions) -> impl IntoView {
    shell()
}

#[server(
    name = SumTwo,
    prefix = "/api",
    endpoint = "sum_two",
    server = leptos_ntex_unofficial::NtexServerFnBackend
)]
pub async fn sum_two(a: i32, b: i32) -> Result<i32, ServerFnError> {
    Ok(a + b)
}

#[server(
    name = MulTwo,
    prefix = "/api",
    endpoint = "mul_two_auto_inventory",
    server = leptos_ntex_unofficial::NtexServerFnBackend
)]
pub async fn mul_two_auto_inventory(a: i32, b: i32) -> Result<i32, ServerFnError> {
    Ok(a * b)
}

#[server(
    name = SumCbor,
    prefix = "/api",
    endpoint = "sum_cbor",
    input = Cbor,
    output = Cbor,
    server = leptos_ntex_unofficial::NtexServerFnBackend
)]
pub async fn sum_cbor(a: i32, b: i32) -> Result<i32, ServerFnError> {
    Ok(a + b)
}

// Every subject owns a new ntex runtime and real TCP server. The macro stays
// synchronous: enabling its Tokio runtime would change the runtime being tested.
fn run_ntex<F: Future + 'static>(future: F) -> F::Output
where
    F::Output: 'static,
{
    ntex::rt::System::new(
        "wire-spec",
        leptos_ntex_unofficial::RequestRuntime::new(ntex::rt::DefaultRuntime),
    )
    .block_on(future)
}

#[derive(Debug)]
struct SiteRoot(PathBuf);

impl SiteRoot {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "leptos_ntex_wire_{}_{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create test root: {error}"),
            }
        }
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for SiteRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Copy)]
enum Registration {
    Config,
    App,
    Catchall,
    Files,
    Package,
}

struct WireServer {
    // Drop the server before removing its files, including on assertion panic.
    server: test::TestServer,
    _root: SiteRoot,
}

impl WireServer {
    async fn new(registration: Registration) -> Self {
        let root = SiteRoot::new();
        std::fs::write(root.path().join("robots.txt"), "User-agent: *\n").unwrap();
        let pkg = root.path().join("pkg");
        std::fs::create_dir_all(pkg.join("snippets/crate")).unwrap();
        std::fs::write(pkg.join("app.js"), "console.log('hi');").unwrap();
        // Raw fixture bytes deliberately exercise selection, not compression.
        std::fs::write(pkg.join("app.js.br"), "br-js").unwrap();
        std::fs::write(pkg.join("snippets/crate/inline.js"), "export const x = 1;").unwrap();
        let options = LeptosOptions::builder()
            .output_name("leptos_ntex_wire")
            .site_root(root.path().to_string_lossy().to_string())
            .site_pkg_dir("pkg")
            .build();
        let server = test::server(move || {
            let options = options.clone();
            async move {
                let app = NtexApp::new().state(options.clone());
                match registration {
                    Registration::Config => app.configure(|cfg| {
                        register_leptos_routes(cfg, generate_route_list(App), shell);
                    }),
                    Registration::App => app.leptos_routes(generate_route_list(App), shell),
                    Registration::Catchall => app
                        .route("/api/{tail}*", handle_server_fns())
                        .configure(|cfg| {
                            register_leptos_routes(cfg, generate_route_list(App), shell);
                        }),
                    Registration::Files => {
                        app.route("/{tail}*", file_and_error_handler(shell_with_options))
                    }
                    Registration::Package => {
                        app.service(site_pkg_dir_service::<ntex::web::DefaultError>(&options))
                    }
                }
            }
        })
        .await;
        Self {
            server,
            _root: root,
        }
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> WireResponse {
        let url = format!("http://{}{}", self.server.addr(), path);
        let mut request = self.server.request(method, url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.send_body(body).await.expect("send local request");
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .body()
            .await
            .expect("read local response body")
            .to_vec();
        WireResponse {
            status,
            headers,
            body,
        }
    }
}

#[derive(Debug)]
struct WireResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

fn have_status(expected: StatusCode) -> impl Fn(&WireResponse) -> AssertionResult {
    move |response| equal(expected)(&response.status)
}
fn have_header(
    name: &'static str,
    expected: &'static str,
) -> impl Fn(&WireResponse) -> AssertionResult {
    move |response| {
        let actual = response.headers.get(name).and_then(|v| v.to_str().ok());
        if actual == Some(expected) {
            Ok(())
        } else {
            Err(AssertionError::new(vec![format!(
                "{name}: expected {expected:?}, got {actual:?}"
            )]))
        }
    }
}
fn have_body(expected: &'static str) -> impl Fn(&WireResponse) -> AssertionResult {
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
fn contain_html(expected: &'static str) -> impl Fn(&WireResponse) -> AssertionResult {
    move |response| {
        let body = String::from_utf8_lossy(&response.body);
        if body.contains(expected) {
            Ok(())
        } else {
            Err(AssertionError::new(vec![format!(
                "HTML missing {expected:?}: {body}"
            )]))
        }
    }
}
fn decode_cbor_as(expected: i32) -> impl Fn(&WireResponse) -> AssertionResult {
    move |response| match ciborium::de::from_reader::<i32, _>(response.body.as_slice()) {
        Ok(actual) if actual == expected => Ok(()),
        actual => Err(AssertionError::new(vec![format!(
            "expected CBOR integer {expected}, got {actual:?}"
        )])),
    }
}

async fn get_page(registration: Registration, path: &str, encoding: Option<&str>) -> WireResponse {
    let srv = WireServer::new(registration).await;
    let headers: Vec<_> = encoding
        .map(|value| vec![("Accept-Encoding", value)])
        .unwrap_or_default();
    srv.request(Method::GET, path, &headers, Vec::new()).await
}

async fn function_response(registration: Registration, method: Method, path: &str) -> WireResponse {
    register_explicit::<SumTwo>();
    let srv = WireServer::new(registration).await;
    srv.request(
        method,
        path,
        &[
            ("Content-Type", "application/x-www-form-urlencoded"),
            ("Accept", "application/json"),
        ],
        b"a=3&b=4".to_vec(),
    )
    .await
}

#[derive(Clone, Copy)]
enum Codec {
    Json,
    Cbor,
}

async fn codec_response(codec: Codec) -> WireResponse {
    register_explicit::<SumTwo>();
    register_explicit::<SumCbor>();
    let srv = WireServer::new(Registration::Catchall).await;
    match codec {
        Codec::Json => {
            srv.request(
                Method::POST,
                SumTwo::PATH,
                &[
                    ("Content-Type", "application/x-www-form-urlencoded"),
                    ("Accept", "application/json"),
                ],
                b"a=3&b=4".to_vec(),
            )
            .await
        }
        Codec::Cbor => {
            let args = std::collections::BTreeMap::from([("a", 5i32), ("b", 9i32)]);
            let mut body = Vec::new();
            ciborium::ser::into_writer(&args, &mut body).unwrap();
            srv.request(
                Method::POST,
                SumCbor::PATH,
                &[
                    ("Content-Type", "application/cbor"),
                    ("Accept", "application/cbor"),
                ],
                body,
            )
            .await
        }
    }
}

#[derive(Debug)]
struct HeadPair {
    get: WireResponse,
    head: WireResponse,
}

async fn head_pair(registration: Registration, path: &str) -> HeadPair {
    let srv = WireServer::new(registration).await;
    let get = srv.request(Method::GET, path, &[], Vec::new()).await;
    let head = srv.request(Method::HEAD, path, &[], Vec::new()).await;
    HeadPair { get, head }
}

fn mirror_get(expected_status: StatusCode) -> impl Fn(&HeadPair) -> AssertionResult {
    move |pair| {
        let mut errors = Vec::new();
        if pair.get.status != expected_status || pair.head.status != expected_status {
            errors.push(format!(
                "GET/HEAD expected {expected_status}, got {}/{}",
                pair.get.status, pair.head.status
            ));
        }
        if pair.get.headers.get(CONTENT_TYPE) != pair.head.headers.get(CONTENT_TYPE) {
            errors.push(format!(
                "GET/HEAD Content-Type differs: {:?}/{:?}",
                pair.get.headers.get(CONTENT_TYPE),
                pair.head.headers.get(CONTENT_TYPE)
            ));
        }
        if !pair.head.body.is_empty() {
            errors.push(format!("HEAD returned {} body bytes", pair.head.body.len()));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(AssertionError::new(errors))
        }
    }
}

lets_expect! {
    expect(run_ntex(get_page(Registration::Config, path, None))) as wire_ssr {
        let path = "/";
        to render_home { have_status(StatusCode::OK), contain_html("<h1>Integration Home</h1>"), contain_html("<title>Home</title>") }
        when route_is_about {
            let path = "/about";
            to render_about { have_status(StatusCode::OK), contain_html("<h1>Integration About</h1>") }
        }
        when route_is_static {
            let path = "/static";
            to serve_generated_html { have_status(StatusCode::OK), contain_html("<h1>Integration Static</h1>") }
        }
        when route_is_missing {
            let path = "/missing";
            to reject_unknown_route { have_status(StatusCode::NOT_FOUND) }
        }
    }
    expect(run_ntex(get_page(Registration::App, "/", None))) as wire_app_registration {
        to render_home { have_status(StatusCode::OK), contain_html("<h1>Integration Home</h1>"), contain_html("<title>Home</title>") }
    }
    expect(run_ntex(head_pair(registration, path))) as wire_head_parity {
        let registration = Registration::Config;
        let path = "/";
        to omit_body_and_preserve_metadata { mirror_get(StatusCode::OK) }
        when endpoint_is_package_asset {
            let registration = Registration::Package;
            let path = "/pkg/app.js";
            to omit_body_and_preserve_metadata { mirror_get(StatusCode::OK) }
        }
        when endpoint_is_missing {
            let path = "/missing";
            to preserve_not_found { mirror_get(StatusCode::NOT_FOUND) }
        }
    }
    expect(run_ntex(function_response(Registration::Catchall, method, path))) as wire_catchall_function {
        let method = Method::POST;
        let path = SumTwo::PATH;
        to return_sum { have_status(StatusCode::OK), have_body("7") }
        when method_is_wrong {
            let method = Method::GET;
            to report_allowed_method { have_status(StatusCode::METHOD_NOT_ALLOWED), have_header("allow", "POST") }
        }
        when endpoint_is_unknown {
            let path = "/api/no-such-endpoint";
            to reject_unknown_function { have_status(StatusCode::BAD_REQUEST) }
        }
    }
    expect(run_ntex(function_response(Registration::App, method, SumTwo::PATH))) as wire_method_specific_function {
        let method = Method::POST;
        to return_sum { have_status(StatusCode::OK), have_body("7") }
        when method_is_wrong {
            let method = Method::GET;
            to fall_through_router { have_status(StatusCode::NOT_FOUND) }
        }
    }
    // No code in this binary calls register_explicit::<MulTwo>().
    expect(run_ntex(function_response(Registration::Catchall, Method::POST, MulTwo::PATH))) as wire_inventory_function {
        to find_inventory_entry { have_status(StatusCode::OK), have_body("12") }
    }
    expect(run_ntex(codec_response(codec))) as wire_function_codec {
        let codec = Codec::Json;
        to roundtrip_json { have_status(StatusCode::OK), have_body("7") }
        when codec_is_cbor {
            let codec = Codec::Cbor;
            to roundtrip_cbor { have_status(StatusCode::OK), have_header("content-type", "application/cbor"), decode_cbor_as(14) }
        }
    }
    expect(run_ntex(get_page(Registration::Files, path, None))) as wire_file_fallback {
        let path = "/robots.txt";
        to serve_file { have_status(StatusCode::OK), have_body("User-agent: *\n") }
        when file_is_missing {
            let path = "/does-not-exist";
            to render_not_found { have_status(StatusCode::NOT_FOUND), contain_html("Not Found") }
        }
    }
    expect(run_ntex(get_page(Registration::Package, path, None))) as wire_package_lookup {
        let path = "/pkg/app.js";
        to serve_asset { have_status(StatusCode::OK), have_body("console.log('hi');") }
        when asset_is_nested {
            let path = "/pkg/snippets/crate/inline.js";
            to serve_snippet { have_status(StatusCode::OK), have_body("export const x = 1;") }
        }
        when asset_is_missing {
            let path = "/pkg/missing.js";
            to return_not_found { have_status(StatusCode::NOT_FOUND) }
        }
    }
    expect(run_ntex(get_page(Registration::Package, "/pkg/app.js", encoding))) as wire_package_representation {
        let encoding = None;
        to serve_identity { have_status(StatusCode::OK), have_body("console.log('hi');") }
        when representation_is_br {
            let encoding = Some("br");
            to serve_brotli_sibling { have_status(StatusCode::OK), have_header("content-encoding", "br"), have_body("br-js") }
        }
    }
}
