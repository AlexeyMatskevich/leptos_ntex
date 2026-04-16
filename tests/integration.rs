//! End-to-end integration tests that boot a real ntex server over TCP and
//! issue HTTP requests through the ntex test client. These exercise the
//! public `leptos_ntex_unofficial` API from outside the crate (no `cfg(test)` access),
//! so they catch visibility and API-shape regressions that the in-crate
//! tests cannot.

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
use ntex::web::{App as NtexApp, test};
use server_fn::ServerFn;
use server_fn::codec::Cbor;

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

fn temp_site_root(name: &str) -> std::path::PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("leptos_ntex_integration_{name}_{nonce}"))
}

#[ntex::test]
async fn real_server_renders_ssr_and_serves_server_fn() {
    register_explicit::<SumTwo>();

    let srv = test::server(move || {
        let routes = generate_route_list(App);
        async move {
            NtexApp::new()
                .route("/api/{tail:.*}", handle_server_fns())
                .configure(move |cfg| {
                    register_leptos_routes(cfg, routes.clone(), shell);
                })
        }
    })
    .await;

    let resp = srv.request(ntex::http::Method::GET, srv.url("/")).send().await.unwrap();
    assert_eq!(resp.status(), ntex::http::StatusCode::OK);
    let body = resp.body().await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("Integration Home"));
    assert!(html.contains("<title>Home</title>"));

    let resp = srv
        .request(ntex::http::Method::POST, srv.url(SumTwo::PATH))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .send_body("a=3&b=4")
        .await
        .unwrap();
    assert_eq!(resp.status(), ntex::http::StatusCode::OK);
    let body = resp.body().await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("7"));
}

#[ntex::test]
async fn catchall_handle_server_fns_returns_400_on_method_mismatch() {
    register_explicit::<SumTwo>();

    let srv = test::server(|| async {
        NtexApp::new().route("/api/{tail:.*}", handle_server_fns())
    })
    .await;

    let resp = srv
        .request(ntex::http::Method::GET, srv.url(SumTwo::PATH))
        .send()
        .await
        .unwrap();
    // `SumTwo` is POST-only. The catchall `.route("/api/{tail:.*}", ...)`
    // accepts every method, so the request reaches the handler body, which
    // looks up the (path, method) pair and — finding no match — returns
    // 400. This is the catchall-mode behavior; use `leptos_routes` for
    // router-level method filtering (see next test).
    assert_eq!(resp.status(), ntex::http::StatusCode::BAD_REQUEST);
}

#[ntex::test]
async fn real_server_method_specific_routing_rejects_wrong_method() {
    register_explicit::<SumTwo>();

    let srv = test::server(|| {
        let routes = generate_route_list(App);
        async move {
            NtexApp::new().leptos_routes(routes, shell)
        }
    })
    .await;

    // SumTwo is registered as POST via leptos_routes. GET on it should be
    // rejected at the router level (405 Method Not Allowed or 404 depending
    // on ntex's router — either way, NOT the 400 from the fall-through).
    let resp = srv
        .request(ntex::http::Method::GET, srv.url(SumTwo::PATH))
        .send()
        .await
        .unwrap();
    assert_ne!(resp.status(), ntex::http::StatusCode::BAD_REQUEST);
    assert_ne!(resp.status(), ntex::http::StatusCode::OK);
}

#[ntex::test]
async fn real_server_file_and_error_handler_serves_file_and_falls_back() {
    let site_root = temp_site_root("file_fallback");
    std::fs::create_dir_all(&site_root).unwrap();
    std::fs::write(site_root.join("robots.txt"), "User-agent: *\n").unwrap();

    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_integration_file_fallback")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    let srv = test::server(move || {
        let options = options.clone();
        async move {
            NtexApp::new()
                .state(options.clone())
                .route("/{tail:.*}", file_and_error_handler(shell_with_options))
        }
    })
    .await;

    let resp = srv
        .request(ntex::http::Method::GET, srv.url("/robots.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), ntex::http::StatusCode::OK);
    let body = resp.body().await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("User-agent:"));

    let resp = srv
        .request(ntex::http::Method::GET, srv.url("/does-not-exist"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), ntex::http::StatusCode::NOT_FOUND);
    let body = resp.body().await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("Not Found"));

    let _ = std::fs::remove_dir_all(&site_root);
}

#[ntex::test]
async fn real_server_site_pkg_dir_service_registers() {
    let site_root = temp_site_root("pkg_service");
    let pkg_dir = site_root.join("pkg");
    std::fs::create_dir_all(&pkg_dir).unwrap();
    std::fs::write(pkg_dir.join("app.js"), "console.log('hi');").unwrap();

    let options = LeptosOptions::builder()
        .output_name("leptos_ntex_integration_pkg_service")
        .site_root(site_root.to_string_lossy().to_string())
        .site_pkg_dir("pkg")
        .build();

    let srv = test::server(move || {
        let options = options.clone();
        async move {
            NtexApp::new()
                .state(options.clone())
                .service(site_pkg_dir_service::<ntex::web::DefaultError>(&options))
        }
    })
    .await;

    let resp = srv
        .request(ntex::http::Method::GET, srv.url("/pkg/app.js"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), ntex::http::StatusCode::OK);
    let body = resp.body().await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("console.log"));

    let _ = std::fs::remove_dir_all(&site_root);
}

// ---------------------------------------------------------------------------
// Inventory auto-registration probe.
//
// `server_fn::initialize_server_fn_map!` iterates `inventory::iter::<
// ServerFnTraitObj<NtexRequest, NtexServerResponse>>`, and the `#[server]`
// macro emits an `inventory::submit!` keyed by the backend's associated
// Request/Response types. On native Linux the `inventory` crate uses ctor
// hooks, so every `#[server(server = NtexServerFnBackend)]` function should
// appear in `REGISTERED_SERVER_FUNCTIONS` without any `register_explicit::<T>`
// call. This test deliberately skips `register_explicit` to verify that.
#[server(
    name = MulTwo,
    prefix = "/api",
    endpoint = "mul_two_auto_inventory",
    server = leptos_ntex_unofficial::NtexServerFnBackend
)]
pub async fn mul_two_auto_inventory(a: i32, b: i32) -> Result<i32, ServerFnError> {
    Ok(a * b)
}

#[ntex::test]
async fn server_fn_auto_registers_via_inventory_without_register_explicit() {
    // NOTE: no `register_explicit::<MulTwo>()` here.
    let srv = test::server(|| {
        let routes = generate_route_list(App);
        async move {
            NtexApp::new()
                .route("/api/{tail:.*}", handle_server_fns())
                .configure(move |cfg| {
                    register_leptos_routes(cfg, routes.clone(), shell);
                })
        }
    })
    .await;

    let resp = srv
        .request(ntex::http::Method::POST, srv.url(MulTwo::PATH))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .send_body("a=3&b=4")
        .await
        .unwrap();
    assert_eq!(resp.status(), ntex::http::StatusCode::OK);
    let body = resp.body().await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("12"));
}

// ---------------------------------------------------------------------------
// CBOR end-to-end probe.
//
// Proves the adapter is codec-agnostic: `NtexRequest::try_into_bytes` and
// `NtexServerResponse::try_from_bytes` handle arbitrary binary bodies, so any
// `server_fn` codec that plugs into the byte pipeline works. Here we wire up
// a `#[server(input = Cbor, output = Cbor)]` function, encode the arguments
// with `ciborium` into `application/cbor`, POST through the ntex test server,
// and decode the response.
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

#[ntex::test]
async fn real_server_roundtrips_cbor_server_fn() {
    register_explicit::<SumCbor>();

    let srv = test::server(|| async {
        NtexApp::new().route("/api/{tail:.*}", handle_server_fns())
    })
    .await;

    // Encode `{a: 5, b: 9}` as a CBOR map with text keys. The `#[server]`
    // macro synthesises a struct `SumCbor { a: i32, b: i32 }` on the server
    // side; ciborium deserialises our map into that struct by field name.
    let mut args = std::collections::BTreeMap::new();
    args.insert("a", 5i32);
    args.insert("b", 9i32);
    let mut buf: Vec<u8> = Vec::new();
    ciborium::ser::into_writer(&args, &mut buf).unwrap();

    let resp = srv
        .request(ntex::http::Method::POST, srv.url(SumCbor::PATH))
        .header("Content-Type", "application/cbor")
        .header("Accept", "application/cbor")
        .send_body(buf)
        .await
        .unwrap();
    assert_eq!(resp.status(), ntex::http::StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/cbor"),
    );

    let body = resp.body().await.unwrap();
    let value: i32 = ciborium::de::from_reader(body.as_ref()).unwrap();
    assert_eq!(value, 14);
}
