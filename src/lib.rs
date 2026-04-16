pub mod leptos_ntex;

use leptos::prelude::*;
use leptos_meta::{MetaTags, Title, provide_meta_context};
use leptos_router::{
    components::{Route, Router, Routes},
    path,
};
#[cfg(test)]
use leptos_router::{SsrMode, static_routes::StaticRoute};

#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();

    view! {
        <Router>
            <main>
                <Routes fallback=|| view! { <h1>"Not Found"</h1> }>
                    <Route
                        path=path!("/")
                        view=|| {
                            view! {
                                <>
                                    <Title text="Leptos + ntex"/>
                                    <h1>"Leptos over ntex"</h1>
                                    <p>"SSR is rendered through an adapter derived from leptos_actix."</p>
                                </>
                            }
                        }
                    />
                    <Route
                        path=path!("/about")
                        view=|| {
                            view! {
                                <>
                                    <Title text="About"/>
                                    <h1>"About"</h1>
                                    <p>"This route is generated from the Leptos router and served by ntex."</p>
                                </>
                            }
                        }
                    />
                </Routes>
            </main>
        </Router>
    }
}

pub fn shell() -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width, initial-scale=1"/>
                <MetaTags/>
            </head>
            <body>
                <App/>
            </body>
        </html>
    }
}

#[cfg(test)]
#[component]
fn StaticApp() -> impl IntoView {
    provide_meta_context();

    view! {
        <Router>
            <main>
                <Routes fallback=|| view! { <h1>"Not Found"</h1> }>
                    <Route
                        path=path!("/")
                        ssr=SsrMode::Static(StaticRoute::new())
                        view=|| view! { <h1>"Static Home"</h1> }
                    />
                    <Route
                        path=path!("/about")
                        ssr=SsrMode::Static(StaticRoute::new())
                        view=|| view! { <h1>"Static About"</h1> }
                    />
                </Routes>
            </main>
        </Router>
    }
}

#[cfg(test)]
#[component]
fn MixedApp() -> impl IntoView {
    provide_meta_context();

    view! {
        <Router>
            <main>
                <Routes fallback=|| view! { <h1>"Not Found"</h1> }>
                    <Route
                        path=path!("/out")
                        view=|| view! { <h1>"OutOfOrder"</h1> }
                    />
                    <Route
                        path=path!("/in")
                        ssr=SsrMode::InOrder
                        view=|| view! { <h1>"InOrder"</h1> }
                    />
                    <Route
                        path=path!("/async")
                        ssr=SsrMode::Async
                        view=|| view! { <h1>"Async"</h1> }
                    />
                </Routes>
            </main>
        </Router>
    }
}

#[cfg(test)]
fn mixed_shell() -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <MetaTags/>
            </head>
            <body>
                <MixedApp/>
            </body>
        </html>
    }
}

#[cfg(test)]
#[server(
    name = EchoName,
    prefix = "/api",
    endpoint = "echo_name",
    server = crate::leptos_ntex::NtexServerFnBackend
)]
pub async fn echo_name(name: String) -> Result<String, ServerFnError> {
    Ok(format!("Hello, {name}"))
}

#[cfg(test)]
#[server(
    name = RedirectToAbout,
    prefix = "/api",
    endpoint = "redirect_to_about",
    server = crate::leptos_ntex::NtexServerFnBackend
)]
pub async fn redirect_to_about() -> Result<(), ServerFnError> {
    crate::leptos_ntex::redirect("/about");
    Ok(())
}

#[cfg(test)]
#[server(
    name = EchoWebsocket,
    prefix = "/api",
    endpoint = "echo_websocket",
    protocol = server_fn::Websocket<server_fn::codec::JsonEncoding, server_fn::codec::JsonEncoding>,
    server = crate::leptos_ntex::NtexServerFnBackend
)]
pub async fn echo_websocket(
    input: server_fn::BoxedStream<String, ServerFnError>,
) -> Result<server_fn::BoxedStream<String, ServerFnError>, ServerFnError> {
    Ok(input)
}

#[cfg(test)]
#[server(
    name = ProbePath,
    prefix = "/api",
    endpoint = "probe_path",
    server = crate::leptos_ntex::NtexServerFnBackend
)]
pub async fn probe_path() -> Result<String, ServerFnError> {
    let req: ntex::web::HttpRequest = crate::leptos_ntex::extract()
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?;
    Ok(req.path().to_string())
}

#[cfg(test)]
#[derive(Clone)]
pub struct AppConfig {
    pub greeting: String,
}

#[cfg(test)]
#[server(
    name = ReadConfig,
    prefix = "/api",
    endpoint = "read_config",
    server = crate::leptos_ntex::NtexServerFnBackend
)]
pub async fn read_config() -> Result<String, ServerFnError> {
    let cfg = crate::leptos_ntex::use_app_state::<AppConfig>()
        .ok_or_else(|| ServerFnError::new("AppConfig not registered".to_string()))?;
    Ok(cfg.greeting)
}

#[cfg(test)]
#[server(
    name = MultiLocation,
    prefix = "/api",
    endpoint = "multi_location",
    server = crate::leptos_ntex::NtexServerFnBackend
)]
pub async fn multi_location() -> Result<(), ServerFnError> {
    let res = leptos::prelude::use_context::<crate::leptos_ntex::ResponseOptions>()
        .ok_or_else(|| ServerFnError::new("no ResponseOptions in context".to_string()))?;
    res.append_header(
        ntex::http::header::LOCATION,
        ntex::http::header::HeaderValue::from_static("/one"),
    );
    res.append_header(
        ntex::http::header::LOCATION,
        ntex::http::header::HeaderValue::from_static("/two"),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::leptos_ntex::{
        generate_route_list, generate_route_list_with_ssg, handle_server_fns, register_explicit,
        register_leptos_routes,
    };
    use leptos::config::LeptosOptions;
    use ntex::http::StatusCode;
    use ntex::web::{App as NtexApp, test};
    use ntex::web::ws;
    use server_fn::serde;
    use server_fn::{ServerFn, redirect::REDIRECT_HEADER};
    use std::{path::PathBuf, time::{SystemTime, UNIX_EPOCH}};

    fn serialize_ws_ok<T: serde::Serialize>(value: &T) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(1);
        bytes.push(0);
        bytes.extend(serde_json::to_vec(value).unwrap());
        bytes
    }

    fn deserialize_ws_ok<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> T {
        assert!(!bytes.is_empty());
        assert_eq!(bytes[0], 0);
        serde_json::from_slice(&bytes[1..]).unwrap()
    }

    fn temp_site_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("leptos_ntex_{name}_{nonce}"))
    }

    #[ntex::test]
    async fn renders_root_route() {
        register_explicit::<EchoName>();
        register_explicit::<RedirectToAbout>();
        let routes = generate_route_list(App);
        let app = test::init_service(
            NtexApp::new()
                .route("/api/{tail:.*}", handle_server_fns())
                .configure(|cfg| {
                    register_leptos_routes(cfg, routes.clone(), shell);
                }),
        )
        .await;

        let req = test::TestRequest::with_uri("/").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = test::read_body(resp).await;
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("Leptos over ntex"));
        assert!(html.contains("<title>Leptos + ntex</title>"));
    }

    #[ntex::test]
    async fn renders_about_route() {
        register_explicit::<EchoName>();
        register_explicit::<RedirectToAbout>();
        let routes = generate_route_list(App);
        let app = test::init_service(
            NtexApp::new()
                .route("/api/{tail:.*}", handle_server_fns())
                .configure(|cfg| {
                    register_leptos_routes(cfg, routes.clone(), shell);
                }),
        )
        .await;

        let req = test::TestRequest::with_uri("/about").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = test::read_body(resp).await;
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("This route is generated from the Leptos router"));
    }

    #[ntex::test]
    async fn handles_server_fn_post() {
        register_explicit::<EchoName>();
        register_explicit::<RedirectToAbout>();
        let routes = generate_route_list(App);
        let app = test::init_service(
            NtexApp::new()
                .route("/api/{tail:.*}", handle_server_fns())
                .configure(|cfg| {
                    register_leptos_routes(cfg, routes.clone(), shell);
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

    #[ntex::test]
    async fn server_fn_redirect_sets_http_redirect_for_html_form() {
        register_explicit::<EchoName>();
        register_explicit::<RedirectToAbout>();
        let routes = generate_route_list(App);
        let app = test::init_service(
            NtexApp::new()
                .route("/api/{tail:.*}", handle_server_fns())
                .configure(|cfg| {
                    register_leptos_routes(cfg, routes.clone(), shell);
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
        let routes = generate_route_list(App);
        let app = test::init_service(
            NtexApp::new()
                .route("/api/{tail:.*}", handle_server_fns())
                .configure(|cfg| {
                    register_leptos_routes(cfg, routes.clone(), shell);
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

    #[ntex::test]
    async fn websocket_server_fn_echoes_messages() {
        register_explicit::<EchoName>();
        register_explicit::<RedirectToAbout>();
        register_explicit::<EchoWebsocket>();

        let srv = test::server(async || {
            NtexApp::new().route("/api/{tail:.*}", handle_server_fns())
        })
        .await;

        let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
        let sink = conn.sink();
        let rx = conn.receiver();

        sink.send(ws::Message::Binary(serialize_ws_ok(&"hello").into()))
            .await
            .unwrap();

        let frame = rx.recv().await.unwrap().unwrap();
        match frame {
            ws::Frame::Binary(bytes) => {
                let echoed: String = deserialize_ws_ok(&bytes);
                assert_eq!(echoed, "hello");
            }
            other => panic!("unexpected websocket frame: {other:?}"),
        }

        sink.send(ws::Message::Close(None)).await.unwrap();
    }

    #[ntex::test]
    async fn static_route_generator_writes_html() {
        let site_root = temp_site_root("static");
        let (_routes, generator) = generate_route_list_with_ssg(StaticApp);
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

    #[ntex::test]
    async fn renders_in_order_ssr_route() {
        let routes = generate_route_list(MixedApp);
        let app = test::init_service(
            NtexApp::new().configure(|cfg| {
                register_leptos_routes(cfg, routes.clone(), mixed_shell);
            }),
        )
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
        let routes = generate_route_list(MixedApp);
        let app = test::init_service(
            NtexApp::new().configure(|cfg| {
                register_leptos_routes(cfg, routes.clone(), mixed_shell);
            }),
        )
        .await;

        let req = test::TestRequest::with_uri("/async").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = test::read_body(resp).await;
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("Async"));
    }

    #[ntex::test]
    async fn head_request_returns_ok() {
        use crate::leptos_ntex::LeptosRoutes;

        let routes = generate_route_list(MixedApp);
        let app = test::init_service(
            NtexApp::new().leptos_routes(routes, mixed_shell),
        )
        .await;

        let req = test::TestRequest::default()
            .method(ntex::http::Method::HEAD)
            .uri("/out")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[ntex::test]
    async fn head_request_via_service_config_returns_ok() {
        let routes = generate_route_list(MixedApp);
        let app = test::init_service(
            NtexApp::new().configure(|cfg| {
                register_leptos_routes(cfg, routes.clone(), mixed_shell);
            }),
        )
        .await;

        let req = test::TestRequest::default()
            .method(ntex::http::Method::HEAD)
            .uri("/out")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[ntex::test]
    async fn app_leptos_routes_impl_renders_route() {
        use crate::leptos_ntex::LeptosRoutes;

        let routes = generate_route_list(App);
        let app = test::init_service(
            NtexApp::new()
                .route("/api/{tail:.*}", handle_server_fns())
                .leptos_routes(routes, shell),
        )
        .await;

        let req = test::TestRequest::with_uri("/").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = test::read_body(resp).await;
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("Leptos over ntex"));
    }

    #[ntex::test]
    async fn use_app_state_reads_registered_ntex_state() {
        register_explicit::<ReadConfig>();
        let config = AppConfig {
            greeting: "Privet".to_string(),
        };
        let app = test::init_service(
            NtexApp::new()
                .state(config)
                .route("/api/{tail:.*}", handle_server_fns()),
        )
        .await;

        let req = test::TestRequest::post()
            .uri(ReadConfig::PATH)
            .header("Accept", "application/json")
            .set_payload("")
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = test::read_body(resp).await;
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("Privet"));
    }

    #[ntex::test]
    async fn payload_limit_rejects_oversized_body() {
        use crate::leptos_ntex::LeptosServerFnConfig;
        register_explicit::<EchoName>();
        let app = test::init_service(
            NtexApp::new()
                // 10-byte limit; `name=AAA...` will blow past it.
                .state(LeptosServerFnConfig {
                    payload_limit: 10,
                    ws_channel_buffer: 32,
                    ..Default::default()
                })
                .route("/api/{tail:.*}", handle_server_fns()),
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
        // server_fn surfaces the deserialization error as a non-2xx
        // response; we just need to see it didn't succeed.
        assert_ne!(resp.status(), StatusCode::OK);
    }

    #[ntex::test]
    async fn payload_limit_accepts_small_body() {
        use crate::leptos_ntex::LeptosServerFnConfig;
        register_explicit::<EchoName>();
        let app = test::init_service(
            NtexApp::new()
                .state(LeptosServerFnConfig {
                    payload_limit: 1024,
                    ws_channel_buffer: 32,
                    ..Default::default()
                })
                .route("/api/{tail:.*}", handle_server_fns()),
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
    async fn multi_location_header_is_preserved_through_res_options() {
        // Regression: `get(LOCATION).cloned()` returned only the first
        // value, silently dropping the second. Now we use `get_all`.
        register_explicit::<MultiLocation>();
        let app = test::init_service(
            NtexApp::new().route("/api/{tail:.*}", handle_server_fns()),
        )
        .await;

        let req = test::TestRequest::post()
            .uri(MultiLocation::PATH)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .set_payload("")
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let locations: Vec<String> = resp
            .headers()
            .get_all(ntex::http::header::LOCATION)
            .filter_map(|v| v.to_str().ok().map(str::to_string))
            .collect();
        assert_eq!(locations, vec!["/one".to_string(), "/two".to_string()]);
    }

    #[ntex::test]
    async fn extract_helper_reads_request_path() {
        register_explicit::<ProbePath>();
        let app = test::init_service(
            NtexApp::new().route("/api/{tail:.*}", handle_server_fns()),
        )
        .await;

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

    #[ntex::test]
    async fn file_and_error_handler_serves_file_then_falls_back() {
        use crate::leptos_ntex::file_and_error_handler;

        let site_root = temp_site_root("file_handler");
        std::fs::create_dir_all(&site_root).unwrap();
        std::fs::write(site_root.join("hello.txt"), "world!").unwrap();

        let options = LeptosOptions::builder()
            .output_name("leptos_ntex_file_handler")
            .site_root(site_root.to_string_lossy().to_string())
            .site_pkg_dir("pkg")
            .build();

        let app = test::init_service(
            NtexApp::new()
                .state(options.clone())
                .route(
                    "/{tail:.*}",
                    file_and_error_handler(|_opts: LeptosOptions| {
                        view! { <h1>"Not Found Shell"</h1> }
                    }),
                ),
        )
        .await;

        let req = test::TestRequest::with_uri("/hello.txt").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = test::read_body(resp).await;
        assert_eq!(String::from_utf8(body.to_vec()).unwrap(), "world!");

        let req = test::TestRequest::with_uri("/missing").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = test::read_body(resp).await;
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("Not Found Shell"));

        let _ = std::fs::remove_dir_all(&site_root);
    }

    #[ntex::test]
    async fn static_route_served_over_http() {
        let site_root = temp_site_root("http_static");
        let (routes, generator) = generate_route_list_with_ssg(StaticApp);
        let options = LeptosOptions::builder()
            .output_name("leptos_ntex_test_http")
            .site_root(site_root.to_string_lossy().to_string())
            .site_pkg_dir("pkg")
            .build();

        generator.generate(&options).await;

        let app = test::init_service(
            NtexApp::new()
                .state(options.clone())
                .configure(|cfg| {
                    register_leptos_routes(cfg, routes.clone(), StaticApp);
                }),
        )
        .await;

        let req = test::TestRequest::with_uri("/").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = test::read_body(resp).await;
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("Static Home"));

        let _ = std::fs::remove_dir_all(&site_root);
    }

    #[ntex::test]
    async fn server_fn_paths_and_get_service_roundtrip() {
        use crate::leptos_ntex::{get_server_fn_service, server_fn_paths};

        register_explicit::<EchoName>();
        register_explicit::<RedirectToAbout>();

        let paths: Vec<_> = server_fn_paths().collect();
        assert!(paths.iter().any(|(p, _)| *p == EchoName::PATH));
        assert!(paths.iter().any(|(p, _)| *p == RedirectToAbout::PATH));

        // Lookup by registered (path, method) returns a service.
        let found = get_server_fn_service(EchoName::PATH, &ntex::http::Method::POST);
        assert!(found.is_some());

        // Lookup with wrong method returns None.
        let not_found =
            get_server_fn_service(EchoName::PATH, &ntex::http::Method::GET);
        assert!(not_found.is_none());

        // Lookup with unknown path returns None.
        let missing = get_server_fn_service("/api/does_not_exist", &ntex::http::Method::POST);
        assert!(missing.is_none());
    }

    #[ntex::test]
    async fn generate_request_and_parts_returns_cloned_head() {
        use crate::leptos_ntex::generate_request_and_parts;

        let req = test::TestRequest::default()
            .uri("/some/path?q=1")
            .header("x-custom", "yes")
            .to_http_request();
        let payload = ntex::http::Payload::None;

        let (_server_fn_req, head) = generate_request_and_parts(req.clone(), payload);
        assert_eq!(head.uri().path(), "/some/path");
        assert_eq!(head.uri().query(), Some("q=1"));
        assert_eq!(
            head.headers().get("x-custom").and_then(|v| v.to_str().ok()),
            Some("yes")
        );
    }

    #[ntex::test]
    async fn handle_response_inner_renders_shell() {
        use crate::leptos_ntex::handle_response_inner;
        use leptos_integration_utils::{BoxedFnOnce, PinnedStream};
        use futures::StreamExt;
        use futures::stream::once as stream_once;

        // Wrap `handle_response_inner` in an ntex route so we can exercise
        // it through `test::call_service` and use `test::read_body` to
        // drain the streaming body uniformly.
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
}
