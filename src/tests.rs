// ---------------------------------------------------------------------------
// In-crate tests. Kept in the crate (rather than in tests/integration.rs)
// because they touch crate-private helpers — `handle_response_inner`, the
// `ensure_executor_initialized` regression probe, direct access to the
// registration table — that are not exported from the public API.
// ---------------------------------------------------------------------------

use leptos::prelude::*;
use leptos_meta::{MetaTags, provide_meta_context};
use leptos_router::{
    SsrMode,
    components::{Route, Router, Routes},
    path,
    static_routes::StaticRoute,
};

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

#[component]
fn StaticHeaderApp() -> impl IntoView {
    provide_meta_context();

    view! {
        <Router>
            <main>
                <Routes fallback=|| view! { <h1>"Not Found"</h1> }>
                    <Route
                        path=path!("/headers")
                        ssr=SsrMode::Static(StaticRoute::new())
                        view=|| {
                            if let Some(res) = use_context::<crate::ResponseOptions>() {
                                res.set_status(ntex::http::StatusCode::CREATED);
                                res.insert_header(
                                    ntex::http::header::HeaderName::from_static("x-static-cache"),
                                    ntex::http::header::HeaderValue::from_static("preserved"),
                                );
                            }
                            view! { <h1>"Static Headers"</h1> }
                        }
                    />
                </Routes>
            </main>
        </Router>
    }
}

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

#[component]
fn UnitApp() -> impl IntoView {
    provide_meta_context();
    view! {
        <Router>
            <main>
                <Routes fallback=|| view! { <h1>"Not Found"</h1> }>
                    <Route
                        path=path!("/")
                        view=|| view! {
                            <>
                                <h1>"Leptos over ntex"</h1>
                                <p>"SSR is rendered through an adapter derived from leptos_actix."</p>
                            </>
                        }
                    />
                    <Route
                        path=path!("/about")
                        view=|| view! {
                            <>
                                <h1>"About"</h1>
                                <p>"This route is generated from the Leptos router and served by ntex."</p>
                            </>
                        }
                    />
                </Routes>
            </main>
        </Router>
    }
}

fn unit_shell() -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width, initial-scale=1"/>
                <MetaTags/>
            </head>
            <body>
                <UnitApp/>
            </body>
        </html>
    }
}

#[server(
    name = EchoName,
    prefix = "/api",
    endpoint = "echo_name",
    server = crate::NtexServerFnBackend
)]
async fn echo_name(name: String) -> Result<String, ServerFnError> {
    Ok(format!("Hello, {name}"))
}

#[server(
    name = RedirectToAbout,
    prefix = "/api",
    endpoint = "redirect_to_about",
    server = crate::NtexServerFnBackend
)]
async fn redirect_to_about() -> Result<(), ServerFnError> {
    crate::redirect("/about");
    Ok(())
}

#[server(
    name = EchoWebsocket,
    prefix = "/api",
    endpoint = "echo_websocket",
    protocol = server_fn::Websocket<server_fn::codec::JsonEncoding, server_fn::codec::JsonEncoding>,
    server = crate::NtexServerFnBackend
)]
async fn echo_websocket(
    input: server_fn::BoxedStream<String, ServerFnError>,
) -> Result<server_fn::BoxedStream<String, ServerFnError>, ServerFnError> {
    Ok(input)
}

#[server(
    name = ProbePath,
    prefix = "/api",
    endpoint = "probe_path",
    server = crate::NtexServerFnBackend
)]
async fn probe_path() -> Result<String, ServerFnError> {
    let req: ntex::web::HttpRequest = crate::extract()
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?;
    Ok(req.path().to_string())
}

#[server(
    name = MultiLocation,
    prefix = "/api",
    endpoint = "multi_location",
    server = crate::NtexServerFnBackend
)]
async fn multi_location() -> Result<(), ServerFnError> {
    let res = leptos::prelude::use_context::<crate::ResponseOptions>()
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

#[allow(clippy::module_inception)]
mod tests {
    use super::*;
    use crate::{
        generate_route_list, generate_route_list_with_ssg, handle_server_fns, register_explicit,
        register_leptos_routes,
    };
    use leptos::config::LeptosOptions;
    use ntex::http::{StatusCode, header};
    use ntex::web::ws;
    use ntex::web::{App as NtexApp, test};
    use server_fn::serde;
    use server_fn::{ServerFn, redirect::REDIRECT_HEADER};
    use std::{
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

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

    async fn recv_ws_frame<E: std::fmt::Debug>(
        rx: &ntex::channel::mpsc::Receiver<Result<ws::Frame, E>>,
    ) -> ws::Frame {
        ntex::time::timeout(ntex::time::Millis(1_000), rx.recv())
            .await
            .expect("timed out waiting for websocket frame")
            .expect("websocket receiver closed")
            .expect("websocket frame error")
    }

    fn temp_site_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("leptos_ntex_{name}_{nonce}"))
    }

    #[test]
    fn server_fn_config_builder_sets_expected_values() {
        let config = crate::LeptosServerFnConfig::new()
            .with_payload_limit(4096)
            .with_ws_channel_buffer(32)
            .with_ws_subprotocol("graphql-ws");

        assert_eq!(config.payload_limit, 4096);
        assert_eq!(config.ws_channel_buffer, 32);
        assert_eq!(config.ws_subprotocol, Some("graphql-ws"));
    }

    #[test]
    fn singleton_response_headers_replace_existing_values() {
        let mut response = crate::response::NtexResponse(
            ntex::web::HttpResponse::Ok()
                .header(header::CACHE_CONTROL, "public, max-age=60")
                .header(header::EXPIRES, "Wed, 21 Oct 2015 07:28:00 GMT")
                .header(header::CONTENT_DISPOSITION, "inline")
                .finish(),
        );
        let mut parts = crate::ResponseParts::default();
        parts.append_header(
            header::CACHE_CONTROL,
            ntex::http::header::HeaderValue::from_static("no-store"),
        );
        parts.append_header(
            header::CONTENT_DISPOSITION,
            ntex::http::header::HeaderValue::from_static("attachment"),
        );
        parts.append_header(
            header::EXPIRES,
            ntex::http::header::HeaderValue::from_static("Thu, 01 Jan 1970 00:00:00 GMT"),
        );

        response.extend_response_parts(parts);
        let response = response.take();

        assert_eq!(
            response
                .headers()
                .get_all(header::CACHE_CONTROL)
                .filter_map(|value| value.to_str().ok())
                .collect::<Vec<_>>(),
            vec!["no-store"]
        );
        assert_eq!(
            response
                .headers()
                .get_all(header::CONTENT_DISPOSITION)
                .filter_map(|value| value.to_str().ok())
                .collect::<Vec<_>>(),
            vec!["attachment"]
        );
        assert_eq!(
            response
                .headers()
                .get_all(header::EXPIRES)
                .filter_map(|value| value.to_str().ok())
                .collect::<Vec<_>>(),
            vec!["Thu, 01 Jan 1970 00:00:00 GMT"]
        );
    }

    #[ntex::test]
    async fn renders_root_route() {
        register_explicit::<EchoName>();
        register_explicit::<RedirectToAbout>();
        let routes = generate_route_list(UnitApp);
        let app = test::init_service(
            NtexApp::new()
                .route("/api/{tail:.*}", handle_server_fns())
                .configure(|cfg| {
                    register_leptos_routes(cfg, routes.clone(), unit_shell);
                }),
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
    async fn renders_about_route() {
        register_explicit::<EchoName>();
        register_explicit::<RedirectToAbout>();
        let routes = generate_route_list(UnitApp);
        let app = test::init_service(
            NtexApp::new()
                .route("/api/{tail:.*}", handle_server_fns())
                .configure(|cfg| {
                    register_leptos_routes(cfg, routes.clone(), unit_shell);
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
        let routes = generate_route_list(UnitApp);
        let app = test::init_service(
            NtexApp::new()
                .route("/api/{tail:.*}", handle_server_fns())
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

    #[ntex::test]
    async fn server_fn_redirect_sets_http_redirect_for_html_form() {
        register_explicit::<EchoName>();
        register_explicit::<RedirectToAbout>();
        let routes = generate_route_list(UnitApp);
        let app = test::init_service(
            NtexApp::new()
                .route("/api/{tail:.*}", handle_server_fns())
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
        let routes = generate_route_list(UnitApp);
        let app = test::init_service(
            NtexApp::new()
                .route("/api/{tail:.*}", handle_server_fns())
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

    #[ntex::test]
    async fn server_fn_html_form_falls_back_to_same_origin_referrer() {
        register_explicit::<EchoName>();
        let app =
            test::init_service(NtexApp::new().route("/api/{tail:.*}", handle_server_fns())).await;

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
        let app =
            test::init_service(NtexApp::new().route("/api/{tail:.*}", handle_server_fns())).await;

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
        let app =
            test::init_service(NtexApp::new().route("/api/{tail:.*}", handle_server_fns())).await;

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
        let app =
            test::init_service(NtexApp::new().route("/api/{tail:.*}", handle_server_fns())).await;

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

    #[ntex::test]
    async fn websocket_server_fn_echoes_messages() {
        register_explicit::<EchoName>();
        register_explicit::<RedirectToAbout>();
        register_explicit::<EchoWebsocket>();

        let srv =
            test::server(async || NtexApp::new().route("/api/{tail:.*}", handle_server_fns()))
                .await;

        let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
        let sink = conn.sink();
        let rx = conn.receiver();

        sink.send(ws::Message::Binary(serialize_ws_ok(&"hello").into()))
            .await
            .unwrap();

        let frame = recv_ws_frame(&rx).await;
        match frame {
            ws::Frame::Binary(bytes) => {
                let echoed: String = deserialize_ws_ok(&bytes);
                assert_eq!(echoed, "hello");
            }
            other => panic!("unexpected websocket frame: {other:?}"),
        }

        sink.send(ws::Message::Close(None)).await.unwrap();
    }

    /// RFC 6455 §5.4: fragmented binary messages must be reassembled
    /// before delivery to the server-fn.
    #[ntex::test]
    async fn websocket_server_fn_reassembles_fragmented_binary() {
        use ntex::ws::Item;

        register_explicit::<EchoWebsocket>();

        let srv =
            test::server(async || NtexApp::new().route("/api/{tail:.*}", handle_server_fns()))
                .await;

        let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
        let sink = conn.sink();
        let rx = conn.receiver();

        // Build the full payload and split into three chunks.
        let full = serialize_ws_ok(&"fragmented-hello");
        let (head, tail) = full.split_at(4);
        let (mid, last) = tail.split_at(tail.len() / 2);

        sink.send(ws::Message::Continuation(Item::FirstBinary(
            head.to_vec().into(),
        )))
        .await
        .unwrap();
        sink.send(ws::Message::Continuation(Item::Continue(
            mid.to_vec().into(),
        )))
        .await
        .unwrap();
        sink.send(ws::Message::Continuation(Item::Last(last.to_vec().into())))
            .await
            .unwrap();

        let frame = recv_ws_frame(&rx).await;
        match frame {
            ws::Frame::Binary(bytes) => {
                let echoed: String = deserialize_ws_ok(&bytes);
                assert_eq!(echoed, "fragmented-hello");
            }
            other => panic!("unexpected websocket frame: {other:?}"),
        }

        sink.send(ws::Message::Close(None)).await.unwrap();
    }

    /// An oversized opening fragment (`FirstBinary`/`FirstText`) must
    /// be rejected with `CloseCode::Size` *before* the buffer is
    /// established. A prior bug simply pushed the bytes into the buffer
    /// without a size check, so a client could already exceed the limit
    /// on the first frame.
    #[ntex::test]
    async fn websocket_server_fn_first_fragment_over_limit_closes_with_size() {
        use crate::LeptosServerFnConfig;
        use ntex::ws::{CloseCode, Item};

        register_explicit::<EchoWebsocket>();

        let srv = test::server(async || {
            NtexApp::new()
                .state(LeptosServerFnConfig {
                    payload_limit: 16,
                    ws_channel_buffer: 16,
                    ..Default::default()
                })
                .route("/api/{tail:.*}", handle_server_fns())
        })
        .await;

        let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
        let sink = conn.sink();
        let rx = conn.receiver();

        let oversize = [b'A'; 64];
        sink.send(ws::Message::Continuation(Item::FirstBinary(
            oversize.to_vec().into(),
        )))
        .await
        .unwrap();

        let frame = recv_ws_frame(&rx).await;
        match frame {
            ws::Frame::Close(Some(reason)) => {
                assert_eq!(reason.code, CloseCode::Size);
            }
            other => panic!("expected Close(Size) on oversized First* frame, got {other:?}"),
        }
    }

    // Note: the interleaved-First* protocol check in the bridge is
    // defense-in-depth. ntex's own WS codec enforces the same
    // invariant at the frame decoder/encoder layer
    // (`ProtocolError::ContinuationStarted`), so a well-behaved client
    // cannot even send such a frame sequence through `ntex::ws`. The
    // server-side guard catches the case where a custom framer or
    // future codec change would forward the bad sequence to us.

    /// When a reassembled fragmented message exceeds `payload_limit`,
    /// the bridge must close the connection with `CloseCode::Size`
    /// (1009, "Message Too Big" per RFC 6455 §7.4.1) and not forward
    /// the partial payload to the server-fn.
    #[ntex::test]
    async fn websocket_server_fn_oversize_fragmented_closes_with_size() {
        use crate::LeptosServerFnConfig;
        use ntex::ws::{CloseCode, Item};

        register_explicit::<EchoWebsocket>();

        let srv = test::server(async || {
            NtexApp::new()
                .state(LeptosServerFnConfig {
                    payload_limit: 16,
                    ws_channel_buffer: 16,
                    ..Default::default()
                })
                .route("/api/{tail:.*}", handle_server_fns())
        })
        .await;

        let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
        let sink = conn.sink();
        let rx = conn.receiver();

        let blob = [b'A'; 100];
        sink.send(ws::Message::Continuation(Item::FirstBinary(
            blob[..40].to_vec().into(),
        )))
        .await
        .unwrap();
        sink.send(ws::Message::Continuation(Item::Continue(
            blob[40..80].to_vec().into(),
        )))
        .await
        .unwrap();

        // The next frame received must be a Close with CloseCode::Size.
        let frame = recv_ws_frame(&rx).await;
        match frame {
            ws::Frame::Close(Some(reason)) => {
                assert_eq!(reason.code, CloseCode::Size);
            }
            other => panic!("expected Close(Size), got {other:?} (limit enforcement regressed)"),
        }
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
        let routes = generate_route_list(MixedApp);
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

    /// RFC 9110 §9.3.2: HEAD must mirror GET's status and headers.
    ///
    /// Note: `test::call_service` bypasses the h1 wire encoder, so the
    /// empty-body requirement is asserted in the TCP-based integration
    /// tests; here we verify that the handler runs and produces the
    /// same status and Content-Type as GET.
    #[ntex::test]
    async fn head_request_mirrors_get_status_and_content_type() {
        use crate::LeptosRoutes;

        let routes = generate_route_list(MixedApp);
        let app = test::init_service(NtexApp::new().leptos_routes(routes, mixed_shell)).await;

        let get_resp =
            test::call_service(&app, test::TestRequest::with_uri("/out").to_request()).await;
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
        let routes = generate_route_list(MixedApp);
        let app = test::init_service(NtexApp::new().configure(|cfg| {
            register_leptos_routes(cfg, routes.clone(), mixed_shell);
        }))
        .await;

        let head = test::call_service(
            &app,
            test::TestRequest::default()
                .method(ntex::http::Method::HEAD)
                .uri("/out")
                .to_request(),
        )
        .await;
        assert_eq!(head.status(), StatusCode::OK);
    }

    /// HEAD on an unregistered path must not return 200 — the old
    /// synthetic HEAD handler did exactly that, hiding real 404s from
    /// monitoring. Now HEAD on a missing route falls through to the
    /// default 404 (or the app's configured fallback).
    #[ntex::test]
    async fn head_request_on_missing_route_not_200() {
        use crate::LeptosRoutes;

        let routes = generate_route_list(MixedApp);
        let app = test::init_service(NtexApp::new().leptos_routes(routes, mixed_shell)).await;

        let resp = test::call_service(
            &app,
            test::TestRequest::default()
                .method(ntex::http::Method::HEAD)
                .uri("/totally-bogus-path")
                .to_request(),
        )
        .await;
        assert_ne!(resp.status(), StatusCode::OK);
    }

    #[ntex::test]
    async fn app_leptos_routes_impl_renders_route() {
        use crate::LeptosRoutes;

        let routes = generate_route_list(UnitApp);
        let app = test::init_service(
            NtexApp::new()
                .route("/api/{tail:.*}", handle_server_fns())
                .leptos_routes(routes, unit_shell),
        )
        .await;

        let req = test::TestRequest::with_uri("/").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = test::read_body(resp).await;
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("Leptos over ntex"));
    }

    // Invariant: `ensure_executor_initialized()` must not depend on a
    // running ntex arbiter — SSG and library-mode callers invoke public
    // entry points (like `generate_route_list`) without booting a ntex
    // runtime. A plain `#[test]` (no `#[ntex::test]`) enforces this.
    #[test]
    fn executor_init_is_safe_without_ntex_system() {
        #[component]
        fn Empty() -> impl IntoView {
            provide_meta_context();
            view! { <h1>"empty"</h1> }
        }
        let _routes = crate::generate_route_list(Empty);
        let _routes2 = crate::generate_route_list(Empty);
    }

    #[test]
    fn try_init_executor_is_idempotent_and_reports_conflicts() {
        let first = crate::try_init_executor();
        let second = crate::try_init_executor();
        match (first, second) {
            (Ok(()), Ok(())) => {}
            (
                Err(any_spawner::ExecutorError::AlreadySet),
                Err(any_spawner::ExecutorError::AlreadySet),
            ) => {}
            other => panic!("executor init result should be stable, got {other:?}"),
        }
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
                .route("/api/{tail:.*}", handle_server_fns()),
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
    async fn singleton_location_header_is_replaced_through_res_options() {
        register_explicit::<MultiLocation>();
        let app =
            test::init_service(NtexApp::new().route("/api/{tail:.*}", handle_server_fns())).await;

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
        let app =
            test::init_service(NtexApp::new().route("/api/{tail:.*}", handle_server_fns())).await;

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
        use crate::file_and_error_handler;

        let site_root = temp_site_root("file_handler");
        std::fs::create_dir_all(&site_root).unwrap();
        std::fs::write(site_root.join("hello.txt"), "world!").unwrap();

        let options = LeptosOptions::builder()
            .output_name("leptos_ntex_file_handler")
            .site_root(site_root.to_string_lossy().to_string())
            .site_pkg_dir("pkg")
            .build();

        let app = test::init_service(NtexApp::new().state(options.clone()).route(
            "/{tail:.*}",
            file_and_error_handler(|_opts: LeptosOptions| {
                view! { <h1>"Not Found Shell"</h1> }
            }),
        ))
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
    async fn file_and_error_handler_file_hit_applies_context_response_options() {
        use crate::file_and_error_handler_with_context;

        let site_root = temp_site_root("file_handler_context");
        std::fs::create_dir_all(&site_root).unwrap();
        std::fs::write(site_root.join("hello.txt"), "world!").unwrap();

        let options = LeptosOptions::builder()
            .output_name("leptos_ntex_file_handler_context")
            .site_root(site_root.to_string_lossy().to_string())
            .site_pkg_dir("pkg")
            .build();

        let app = test::init_service(NtexApp::new().state(options.clone()).route(
            "/{tail:.*}",
            file_and_error_handler_with_context(
                || {
                    let res = use_context::<crate::ResponseOptions>()
                        .expect("ResponseOptions should be provided on file hits");
                    res.insert_header(
                        ntex::http::header::HeaderName::from_static("x-file-hit"),
                        ntex::http::header::HeaderValue::from_static("yes"),
                    );
                },
                |_opts: LeptosOptions| view! { <h1>"Not Found Shell"</h1> },
            ),
        ))
        .await;

        let resp =
            test::call_service(&app, test::TestRequest::with_uri("/hello.txt").to_request()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("x-file-hit")
                .and_then(|v| v.to_str().ok()),
            Some("yes")
        );

        let _ = std::fs::remove_dir_all(&site_root);
    }

    #[ntex::test]
    async fn file_and_error_handler_serves_precompressed_br_with_original_mime() {
        use crate::file_and_error_handler;

        let site_root = temp_site_root("file_handler_br");
        std::fs::create_dir_all(&site_root).unwrap();
        std::fs::write(site_root.join("app.js"), "console.log('plain');").unwrap();
        std::fs::write(site_root.join("app.js.br"), "br-bytes").unwrap();
        std::fs::write(site_root.join("app.js.gz"), "gzip-bytes").unwrap();

        let options = LeptosOptions::builder()
            .output_name("leptos_ntex_file_handler_br")
            .site_root(site_root.to_string_lossy().to_string())
            .site_pkg_dir("pkg")
            .build();

        let app = test::init_service(NtexApp::new().state(options.clone()).route(
            "/{tail:.*}",
            file_and_error_handler(|_opts: LeptosOptions| {
                view! { <h1>"Not Found Shell"</h1> }
            }),
        ))
        .await;

        let resp = test::call_service(
            &app,
            test::TestRequest::with_uri("/app.js")
                .header(ntex::http::header::ACCEPT_ENCODING, "br")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(ntex::http::header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some("br")
        );
        assert_eq!(
            resp.headers()
                .get(ntex::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/javascript")
        );
        let vary = resp
            .headers()
            .get(ntex::http::header::VARY)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(vary.contains("Accept-Encoding"));
        let body = test::read_body(resp).await;
        assert_eq!(String::from_utf8(body.to_vec()).unwrap(), "br-bytes");

        let resp = test::call_service(
            &app,
            test::TestRequest::with_uri("/app.js")
                .header(
                    ntex::http::header::ACCEPT_ENCODING,
                    "br;q=0, gzip;q=0, *;q=1",
                )
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers()
                .get(ntex::http::header::CONTENT_ENCODING)
                .is_none()
        );
        let body = test::read_body(resp).await;
        assert_eq!(
            String::from_utf8(body.to_vec()).unwrap(),
            "console.log('plain');"
        );

        let resp = test::call_service(
            &app,
            test::TestRequest::with_uri("/app.js")
                .header(ntex::http::header::ACCEPT_ENCODING, "gzip;q=0.1")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(ntex::http::header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some("gzip")
        );
        let body = test::read_body(resp).await;
        assert_eq!(String::from_utf8(body.to_vec()).unwrap(), "gzip-bytes");

        let _ = std::fs::remove_dir_all(&site_root);
    }

    /// Builds a shell-only app with `file_and_error_handler` rooted at
    /// `site_root`, suitable for traversal assertions.
    macro_rules! traversal_app {
        ($site_root:expr) => {{
            use crate::file_and_error_handler;
            let options = LeptosOptions::builder()
                .output_name("leptos_ntex_traversal")
                .site_root($site_root.to_string_lossy().to_string())
                .site_pkg_dir("pkg")
                .build();
            test::init_service(NtexApp::new().state(options).route(
                "/{tail:.*}",
                file_and_error_handler(|_opts: LeptosOptions| {
                    view! { <h1>"Shell"</h1> }
                }),
            ))
            .await
        }};
    }

    /// Verifies that relative-parent traversal does not escape `site_root`.
    /// Writes a "secret" file *outside* the root but inside its parent, then
    /// checks that `/../secret.txt` returns the shell rather than file
    /// contents.
    #[ntex::test]
    async fn traversal_relative_parent_rejected() {
        let parent = temp_site_root("traversal_parent");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::write(parent.join("secret.txt"), "SECRET").unwrap();
        let site_root = parent.join("public");
        std::fs::create_dir_all(&site_root).unwrap();

        let app = traversal_app!(&site_root);
        let req = test::TestRequest::with_uri("/../secret.txt").to_request();
        let resp = test::call_service(&app, req).await;
        let status = resp.status();
        let body = test::read_body(resp).await;
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert_ne!(
            status,
            StatusCode::OK,
            "traversal must not return 200, got body = {text:?}"
        );
        assert!(
            !text.contains("SECRET"),
            "traversal leaked: body = {text:?}"
        );

        let _ = std::fs::remove_dir_all(&parent);
    }

    /// Percent-encoded `..` (`%2e%2e`) must not bypass the traversal filter.
    #[ntex::test]
    async fn traversal_percent_encoded_parent_rejected() {
        let parent = temp_site_root("traversal_pct");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::write(parent.join("secret.txt"), "PCT_SECRET").unwrap();
        let site_root = parent.join("public");
        std::fs::create_dir_all(&site_root).unwrap();

        let app = traversal_app!(&site_root);
        let req = test::TestRequest::with_uri("/%2e%2e/secret.txt").to_request();
        let resp = test::call_service(&app, req).await;
        let body = test::read_body(resp).await;
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(!text.contains("PCT_SECRET"));

        let _ = std::fs::remove_dir_all(&parent);
    }

    /// A root-style URI (leading `/etc/…`) must not pull files from the
    /// real `/etc` — `Path::join` replacement of the root is the classic
    /// exploit vector. Our `safe_subpath` reconstructs the path from split
    /// segments so a bare `/etc/passwd` resolves under `<site_root>/etc/…`.
    #[ntex::test]
    async fn traversal_absolute_path_rejected() {
        let site_root = temp_site_root("traversal_abs");
        std::fs::create_dir_all(&site_root).unwrap();

        let app = traversal_app!(&site_root);
        let req = test::TestRequest::with_uri("/etc/passwd").to_request();
        let resp = test::call_service(&app, req).await;
        let status = resp.status();
        let body = test::read_body(resp).await;
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert_ne!(status, StatusCode::OK);
        assert!(!text.contains("root:"), "leaked /etc/passwd: {text:?}");

        let _ = std::fs::remove_dir_all(&site_root);
    }

    /// Dotfiles (`.env`, `.htaccess`) must not be served by the fallback
    /// handler — matches the convention established by `ntex_files::Files`.
    #[ntex::test]
    async fn traversal_dotfile_rejected() {
        let site_root = temp_site_root("traversal_dot");
        std::fs::create_dir_all(&site_root).unwrap();
        std::fs::write(site_root.join(".env"), "API_KEY=secret").unwrap();

        let app = traversal_app!(&site_root);
        let req = test::TestRequest::with_uri("/.env").to_request();
        let resp = test::call_service(&app, req).await;
        let body = test::read_body(resp).await;
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(!text.contains("API_KEY"));

        let _ = std::fs::remove_dir_all(&site_root);
    }

    /// A NUL byte in a path segment must be rejected outright — NUL is
    /// illegal in POSIX paths and typically signals a smuggling attempt.
    #[ntex::test]
    async fn traversal_null_byte_rejected() {
        let site_root = temp_site_root("traversal_nul");
        std::fs::create_dir_all(&site_root).unwrap();
        std::fs::write(site_root.join("ok.txt"), "ok").unwrap();

        let app = traversal_app!(&site_root);
        let req = test::TestRequest::with_uri("/ok%00hidden.txt").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let _ = std::fs::remove_dir_all(&site_root);
    }

    /// Symlink escape: a symlink inside `site_root` pointing outside must
    /// not leak external files. `canonicalize()` + `starts_with(canon_root)`
    /// catches this.
    #[cfg(unix)]
    #[ntex::test]
    async fn traversal_symlink_escape_rejected() {
        let parent = temp_site_root("traversal_symlink");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::write(parent.join("outside.txt"), "OUTSIDE").unwrap();
        let site_root = parent.join("public");
        std::fs::create_dir_all(&site_root).unwrap();
        let _ =
            std::os::unix::fs::symlink(parent.join("outside.txt"), site_root.join("escape.txt"));

        let app = traversal_app!(&site_root);
        let req = test::TestRequest::with_uri("/escape.txt").to_request();
        let resp = test::call_service(&app, req).await;
        let body = test::read_body(resp).await;
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(!text.contains("OUTSIDE"), "symlink escape leaked: {text:?}");

        let _ = std::fs::remove_dir_all(&parent);
    }

    /// HEAD on a statically pre-rendered route must mirror GET's status
    /// and Content-Type. (Wire-level body elision is covered by the
    /// TCP-based integration tests.)
    #[ntex::test]
    async fn head_request_on_static_route_mirrors_get() {
        let site_root = temp_site_root("head_static");
        let (routes, generator) = generate_route_list_with_ssg(StaticApp);
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

        let get_resp =
            test::call_service(&app, test::TestRequest::with_uri("/").to_request()).await;
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
        let (routes, generator) = generate_route_list_with_ssg(StaticApp);
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

    #[ntex::test]
    async fn static_route_cached_headers_are_replayed_more_than_once() {
        let site_root = temp_site_root("static_headers");
        let (routes, generator) = generate_route_list_with_ssg(StaticHeaderApp);
        let options = LeptosOptions::builder()
            .output_name("leptos_ntex_static_headers")
            .site_root(site_root.to_string_lossy().to_string())
            .site_pkg_dir("pkg")
            .build();

        generator.generate(&options).await;

        let app = test::init_service(NtexApp::new().state(options.clone()).configure(|cfg| {
            register_leptos_routes(cfg, routes.clone(), StaticHeaderApp);
        }))
        .await;

        for _ in 0..3 {
            let resp =
                test::call_service(&app, test::TestRequest::with_uri("/headers").to_request())
                    .await;
            assert_eq!(resp.status(), StatusCode::CREATED);
            assert_eq!(
                resp.headers()
                    .get("x-static-cache")
                    .and_then(|v| v.to_str().ok()),
                Some("preserved")
            );
            let body = test::read_body(resp).await;
            let html = String::from_utf8(body.to_vec()).unwrap();
            assert!(html.contains("Static Headers"));
        }

        let _ = std::fs::remove_dir_all(&site_root);
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
                .route("/api/{tail:.*}", handle_server_fns()),
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
                .route("/api/{tail:.*}", handle_server_fns()),
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

    // -- WebSocket frame-type coverage ----------------------------------

    /// A single unfragmented Binary frame exceeding `payload_limit` must
    /// close with `CloseCode::Size`.
    #[ntex::test]
    async fn websocket_unfragmented_binary_oversize_closes_with_size() {
        use crate::LeptosServerFnConfig;
        use ntex::ws::CloseCode;

        register_explicit::<EchoWebsocket>();

        let srv = test::server(async || {
            NtexApp::new()
                .state(LeptosServerFnConfig {
                    payload_limit: 16,
                    ws_channel_buffer: 16,
                    ..Default::default()
                })
                .route("/api/{tail:.*}", handle_server_fns())
        })
        .await;

        let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
        let sink = conn.sink();
        let rx = conn.receiver();

        let oversize = vec![b'X'; 64];
        sink.send(ws::Message::Binary(oversize.into()))
            .await
            .unwrap();

        let frame = recv_ws_frame(&rx).await;
        match frame {
            ws::Frame::Close(Some(reason)) => {
                assert_eq!(reason.code, CloseCode::Size);
            }
            other => panic!("expected Close(Size) on oversized Binary frame, got {other:?}"),
        }
    }

    /// A single unfragmented Text frame must be forwarded to the
    /// server-fn and echoed back.
    #[ntex::test]
    async fn websocket_text_frame_echoed() {
        register_explicit::<EchoWebsocket>();

        let srv =
            test::server(async || NtexApp::new().route("/api/{tail:.*}", handle_server_fns()))
                .await;

        let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
        let sink = conn.sink();
        let rx = conn.receiver();

        let payload = serialize_ws_ok(&"text-hello");
        sink.send(ws::Message::Text(
            String::from_utf8(payload).unwrap().into(),
        ))
        .await
        .unwrap();

        let frame = recv_ws_frame(&rx).await;
        match frame {
            ws::Frame::Binary(bytes) => {
                let echoed: String = deserialize_ws_ok(&bytes);
                assert_eq!(echoed, "text-hello");
            }
            other => panic!("expected Binary echo response, got {other:?}"),
        }

        sink.send(ws::Message::Close(None)).await.unwrap();
    }

    /// An oversized Text frame must close with `CloseCode::Size`.
    #[ntex::test]
    async fn websocket_text_oversize_closes_with_size() {
        use crate::LeptosServerFnConfig;
        use ntex::ws::CloseCode;

        register_explicit::<EchoWebsocket>();

        let srv = test::server(async || {
            NtexApp::new()
                .state(LeptosServerFnConfig {
                    payload_limit: 16,
                    ws_channel_buffer: 16,
                    ..Default::default()
                })
                .route("/api/{tail:.*}", handle_server_fns())
        })
        .await;

        let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
        let sink = conn.sink();
        let rx = conn.receiver();

        let oversize_text = "X".repeat(64);
        sink.send(ws::Message::Text(oversize_text.into()))
            .await
            .unwrap();

        let frame = recv_ws_frame(&rx).await;
        match frame {
            ws::Frame::Close(Some(reason)) => {
                assert_eq!(reason.code, CloseCode::Size);
            }
            other => panic!("expected Close(Size) on oversized Text frame, got {other:?}"),
        }
    }

    /// A Ping frame must be answered with Pong.
    #[ntex::test]
    async fn websocket_ping_receives_pong() {
        register_explicit::<EchoWebsocket>();

        let srv =
            test::server(async || NtexApp::new().route("/api/{tail:.*}", handle_server_fns()))
                .await;

        let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
        let sink = conn.sink();
        let rx = conn.receiver();

        sink.send(ws::Message::Ping(ntex::util::Bytes::from_static(
            b"ping-data",
        )))
        .await
        .unwrap();

        let frame = recv_ws_frame(&rx).await;
        match frame {
            ws::Frame::Pong(data) => {
                assert_eq!(&data[..], b"ping-data");
            }
            other => panic!("expected Pong, got {other:?}"),
        }

        sink.send(ws::Message::Close(None)).await.unwrap();
    }

    #[ntex::test]
    async fn websocket_configured_subprotocol_is_not_echoed_unless_offered() {
        use crate::LeptosServerFnConfig;

        register_explicit::<EchoWebsocket>();

        let srv = test::server(async || {
            NtexApp::new()
                .state(LeptosServerFnConfig {
                    payload_limit: 1024,
                    ws_channel_buffer: 16,
                    ws_subprotocol: Some("graphql-ws"),
                })
                .route("/api/{tail:.*}", handle_server_fns())
        })
        .await;

        let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
        assert!(
            conn.response()
                .headers()
                .get(ntex::http::header::SEC_WEBSOCKET_PROTOCOL)
                .is_none(),
            "server must not echo a subprotocol that the client did not offer"
        );

        conn.sink().send(ws::Message::Close(None)).await.unwrap();
    }

    /// A Close frame must be echoed back.
    #[ntex::test]
    async fn websocket_close_is_echoed() {
        use ntex::ws::CloseCode;

        register_explicit::<EchoWebsocket>();

        let srv =
            test::server(async || NtexApp::new().route("/api/{tail:.*}", handle_server_fns()))
                .await;

        let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
        let sink = conn.sink();
        let rx = conn.receiver();

        sink.send(ws::Message::Close(Some(ws::CloseReason {
            code: CloseCode::Normal,
            description: Some("bye".into()),
        })))
        .await
        .unwrap();

        let frame = recv_ws_frame(&rx).await;
        match frame {
            ws::Frame::Close(Some(reason)) => {
                assert_eq!(reason.code, CloseCode::Normal);
            }
            other => panic!("expected Close echo, got {other:?}"),
        }
    }

    /// A dotfile nested inside a subdirectory must be rejected.
    #[ntex::test]
    async fn traversal_dotfile_in_subdirectory_rejected() {
        let site_root = temp_site_root("dotfile_subdir");
        std::fs::create_dir_all(site_root.join("subdir")).unwrap();
        std::fs::write(site_root.join("subdir/.env"), "SECRET=abc").unwrap();

        let app = traversal_app!(&site_root);

        let req = test::TestRequest::with_uri("/subdir/.env").to_request();
        let resp = test::call_service(&app, req).await;
        let body = test::read_body(resp).await;
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            !text.contains("SECRET"),
            "dotfile in subdirectory must not be served"
        );

        let _ = std::fs::remove_dir_all(&site_root);
    }

    #[ntex::test]
    async fn traversal_encoded_slash_dotfile_rejected() {
        let site_root = temp_site_root("dotfile_encoded_slash");
        std::fs::create_dir_all(site_root.join("subdir")).unwrap();
        std::fs::write(site_root.join("subdir/.env"), "SECRET=encoded").unwrap();

        let app = traversal_app!(&site_root);

        let req = test::TestRequest::with_uri("/subdir%2F.env").to_request();
        let resp = test::call_service(&app, req).await;
        let body = test::read_body(resp).await;
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            !text.contains("SECRET=encoded"),
            "encoded slash must not bypass dotfile filtering"
        );

        let _ = std::fs::remove_dir_all(&site_root);
    }

    #[ntex::test]
    async fn generate_request_and_parts_returns_cloned_head() {
        use crate::generate_request_and_parts;

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
}
