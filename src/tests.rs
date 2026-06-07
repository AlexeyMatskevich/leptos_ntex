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

// A view that reads an async `Resource` under `<Suspense>`. The whole point
// is that the three streaming modes render its `<Suspense>` DIFFERENTLY:
//   * OutOfOrder: emits the FALLBACK in the shell, then streams the resolved
//     fragment out of order (so the body contains BOTH the fallback marker
//     and an out-of-order replacement chunk).
//   * InOrder / Async: block until the resource resolves, so the body has the
//     RESOLVED content in place and NO fallback marker.
// Deleting the InOrder/Async match arm falls back to the OutOfOrder renderer,
// which a fallback-marker assertion then catches.
#[component]
fn SuspendedView() -> impl IntoView {
    let data = Resource::new(
        || (),
        |_| async move {
            // A real delay so the resource is still pending when the shell
            // renders. OutOfOrder then streams the shell + fallback first and
            // the resolved fragment later; InOrder/Async block for the value.
            ntex::time::sleep(ntex::time::Millis(50)).await;
            String::from("RESOLVED-CONTENT")
        },
    );
    view! {
        <Suspense fallback=move || view! { <span>"FALLBACK-MARKER"</span> }>
            {move || Suspend::new(async move {
                let value = data.await;
                view! { <span>{value}</span> }
            })}
        </Suspense>
    }
}

#[component]
fn SuspenseApp() -> impl IntoView {
    provide_meta_context();
    view! {
        <Router>
            <main>
                <Routes fallback=|| view! { <p>"Not Found"</p> }>
                    <Route path=path!("/out") ssr=SsrMode::OutOfOrder view=SuspendedView/>
                    <Route path=path!("/in") ssr=SsrMode::InOrder view=SuspendedView/>
                    <Route path=path!("/async") ssr=SsrMode::Async view=SuspendedView/>
                </Routes>
            </main>
        </Router>
    }
}

#[component]
fn NestedApp() -> impl IntoView {
    provide_meta_context();
    view! {
        <Router>
            <main>
                <Routes fallback=|| view! { <p>"Not Found"</p> }>
                    <Route path=path!("/outer/inner") view=|| view! { <h1>"Nested"</h1> } />
                </Routes>
            </main>
        </Router>
    }
}

fn suspense_shell() -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <MetaTags/>
            </head>
            <body>
                <SuspenseApp/>
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
    use lets_expect::lets_expect;
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

    // ----- LeptosServerFnConfig builder: exhaustive spec ----------------
    // Object-interface spec for the const builder. The behavioural content
    // the old single-case test missed: each setter overrides ONLY its own
    // field (leaving the siblings at the crate defaults), and `Default`
    // agrees with `new()`. `LeptosServerFnConfig` derives no `PartialEq`,
    // so each field is asserted individually via `have(...)`.
    lets_expect! {
        expect(config) as the_server_fn_config {
            let config = crate::LeptosServerFnConfig::new();

            to starts_from_the_crate_defaults {
                have(payload_limit) equal(crate::DEFAULT_PAYLOAD_LIMIT),
                have(ws_channel_buffer) equal(crate::DEFAULT_WS_CHANNEL_BUFFER),
                have(ws_subprotocol) be_none,
            }

            when built_through_the_default_trait {
                let config = crate::LeptosServerFnConfig::default();
                to matches_new {
                    have(payload_limit) equal(crate::DEFAULT_PAYLOAD_LIMIT),
                    have(ws_channel_buffer) equal(crate::DEFAULT_WS_CHANNEL_BUFFER),
                    have(ws_subprotocol) be_none,
                }
            }

            when only_the_payload_limit_is_overridden {
                let config = crate::LeptosServerFnConfig::new().with_payload_limit(4096);
                to changes_the_payload_limit_alone {
                    have(payload_limit) equal(4096),
                    have(ws_channel_buffer) equal(crate::DEFAULT_WS_CHANNEL_BUFFER),
                    have(ws_subprotocol) be_none,
                }
            }

            when only_the_ws_channel_buffer_is_overridden {
                let config = crate::LeptosServerFnConfig::new().with_ws_channel_buffer(32);
                to changes_the_channel_buffer_alone {
                    have(payload_limit) equal(crate::DEFAULT_PAYLOAD_LIMIT),
                    have(ws_channel_buffer) equal(32),
                    have(ws_subprotocol) be_none,
                }
            }

            when only_the_ws_subprotocol_is_overridden {
                let config = crate::LeptosServerFnConfig::new().with_ws_subprotocol("graphql-ws");
                to changes_the_subprotocol_alone {
                    have(payload_limit) equal(crate::DEFAULT_PAYLOAD_LIMIT),
                    have(ws_channel_buffer) equal(crate::DEFAULT_WS_CHANNEL_BUFFER),
                    have(ws_subprotocol) equal(Some("graphql-ws")),
                }
            }

            when every_field_is_overridden {
                let config = crate::LeptosServerFnConfig::new()
                    .with_payload_limit(4096)
                    .with_ws_channel_buffer(32)
                    .with_ws_subprotocol("graphql-ws");
                to applies_all_three_overrides {
                    have(payload_limit) equal(4096),
                    have(ws_channel_buffer) equal(32),
                    have(ws_subprotocol) equal(Some("graphql-ws")),
                }
            }
        }
    }

    // ----- NtexResponse::extend_response_parts header merge: exhaustive -
    // When merging captured `ResponseParts` into the response, a *singleton*
    // header (the `should_replace_header` set — Cache-Control, Location,
    // ETag, …, here represented by Cache-Control) must REPLACE any existing
    // value, while a multi-value header (e.g. Set-Cookie) must APPEND,
    // keeping both. The old test covered only the replace direction; the
    // append direction was the missing negative. The assertion pins the
    // exact resulting value vector, not mere presence.
    fn reconcile_header(key: header::HeaderName, existing: &str, incoming: &str) -> Vec<String> {
        let mut response = crate::response::NtexResponse(
            ntex::web::HttpResponse::Ok()
                .header(key.clone(), existing)
                .finish(),
        );
        let mut parts = crate::ResponseParts::default();
        parts.append_header(
            key.clone(),
            header::HeaderValue::from_str(incoming).unwrap(),
        );
        response.extend_response_parts(parts);
        response
            .take()
            .headers()
            .get_all(key)
            .filter_map(|value| value.to_str().ok())
            .map(str::to_string)
            .collect()
    }

    lets_expect! {
        expect(reconcile_header(key, existing, incoming)) as the_reconciled_header {
            let key = header::CACHE_CONTROL;
            let existing = "public, max-age=60";
            let incoming = "no-store";

            to replaces_the_previous_singleton_value { equal(vec!["no-store".to_string()]) }

            // Pin the other singleton match arms the original test covered, so
            // a regression deleting only `EXPIRES` or `CONTENT_DISPOSITION`
            // from `should_replace_header` is still caught (each is a distinct
            // arm — not subsumed by the Cache-Control representative).
            when the_singleton_header_is_expires {
                let key = header::EXPIRES;
                let existing = "Wed, 21 Oct 2015 07:28:00 GMT";
                let incoming = "Thu, 01 Jan 1970 00:00:00 GMT";
                to replaces_the_previous_value {
                    equal(vec!["Thu, 01 Jan 1970 00:00:00 GMT".to_string()])
                }
            }

            when the_singleton_header_is_content_disposition {
                let key = header::CONTENT_DISPOSITION;
                let existing = "inline";
                let incoming = "attachment";
                to replaces_the_previous_value { equal(vec!["attachment".to_string()]) }
            }

            when the_header_permits_multiple_values {
                let key = header::SET_COOKIE;
                let existing = "session=abc";
                let incoming = "theme=dark";
                to appends_and_keeps_both_values {
                    equal(vec!["session=abc".to_string(), "theme=dark".to_string()])
                }
            }
        }
    }

    // ----- extend_response_parts status override: exhaustive spec -------
    // A captured `Some(status)` overrides the response status; `None` leaves
    // it untouched. The old test exercised neither.
    fn status_after_extend(override_status: Option<StatusCode>) -> StatusCode {
        let mut response = crate::response::NtexResponse(ntex::web::HttpResponse::Ok().finish());
        let parts = crate::ResponseParts {
            status: override_status,
            ..Default::default()
        };
        response.extend_response_parts(parts);
        response.take().status()
    }

    lets_expect! {
        expect(status_after_extend(override_status)) as the_extended_status {
            let override_status: Option<StatusCode> = None;

            to leaves_the_existing_status_unchanged { equal(StatusCode::OK) }

            when a_status_override_is_present {
                let override_status = Some(StatusCode::CREATED);
                to applies_the_overridden_status { equal(StatusCode::CREATED) }
            }
        }
    }

    // ----- content_length_exceeds preflight: boundary spec --------------
    // The 413 preflight that rejects an oversize body declared up-front via
    // `Content-Length`, WITHOUT reading it. The boundary is the crux: a body
    // of *exactly* `limit` bytes does NOT "exceed" the limit (the predicate
    // is strict `>`), so the preflight must let it through — only `limit + 1`
    // and above are rejected. A missing or unparseable header is not a size
    // declaration, so the preflight stays out of the way and returns false.
    fn content_length_preflight(content_length: Option<&str>, limit: usize) -> bool {
        let mut req = test::TestRequest::default();
        if let Some(value) = content_length {
            req = req.header(header::CONTENT_LENGTH, value);
        }
        crate::config::content_length_exceeds(&req.to_http_request(), limit)
    }

    lets_expect! {
        expect(content_length_preflight(content_length, limit)) as the_preflight {
            let limit = 1024usize;
            let content_length: Option<&str> = Some("2048");

            to rejects_an_oversize_declaration { be_true }

            when the_declared_length_is_below_the_limit {
                let content_length = Some("1023");
                to allows_the_body { be_false }
            }

            when the_declared_length_is_exactly_the_limit {
                let content_length = Some("1024");
                to allows_the_body { be_false }
            }

            when the_declared_length_is_one_byte_over_the_limit {
                let content_length = Some("1025");
                to rejects_the_body { be_true }
            }

            when no_content_length_is_declared {
                let content_length: Option<&str> = None;
                to stays_out_of_the_way { be_false }
            }

            when the_content_length_is_not_a_number {
                let content_length = Some("not-a-number");
                to stays_out_of_the_way { be_false }
            }
        }
    }

    // ----- DEFAULT_PAYLOAD_LIMIT: pins the documented 2 MiB default ------
    // Matches ntex's own `PayloadConfig` default. A regression in the
    // constant expression (e.g. a dropped factor) changes the limit silently.
    lets_expect! {
        expect(crate::DEFAULT_PAYLOAD_LIMIT) as the_default_payload_limit {
            to is_two_mebibytes { equal(2 * 1024 * 1024) }
        }
    }

    // ----- ResponseParts::insert_header: overwrite semantics ------------
    // `insert_header` must REPLACE any previous value for the same key
    // (unlike `append_header`). A no-op regression drops the header entirely.
    fn response_parts_header_values(inserts: &[&str]) -> Vec<String> {
        let name = header::HeaderName::from_static("x-test");
        let mut parts = crate::ResponseParts::default();
        for value in inserts {
            parts.insert_header(name.clone(), header::HeaderValue::from_str(value).unwrap());
        }
        parts
            .headers
            .get_all(&name)
            .filter_map(|value| value.to_str().ok())
            .map(str::to_string)
            .collect()
    }

    lets_expect! {
        expect(response_parts_header_values(inserts)) as response_parts_headers {
            let inserts: &[&str] = &["first"];

            to records_the_inserted_header { equal(vec!["first".to_string()]) }

            when the_same_key_is_inserted_twice {
                let inserts: &[&str] = &["first", "second"];
                to keeps_only_the_latest_value { equal(vec!["second".to_string()]) }
            }
        }
    }

    // ----- ResponseOptions::overwrite: wholesale replacement ------------
    // `overwrite` swaps the entire inner `ResponseParts`, so a previously set
    // status is replaced by the incoming one (including back to `None`).
    fn status_after_overwrite(replacement: Option<StatusCode>) -> Option<StatusCode> {
        let options = crate::ResponseOptions::default();
        options.set_status(StatusCode::OK);
        options.overwrite(crate::ResponseParts {
            status: replacement,
            ..Default::default()
        });
        options.0.read().unwrap().status
    }

    lets_expect! {
        expect(status_after_overwrite(replacement)) as overwriting_response_parts {
            let replacement: Option<StatusCode> = Some(StatusCode::IM_A_TEAPOT);

            to replaces_the_previously_set_status { equal(Some(StatusCode::IM_A_TEAPOT)) }

            when the_replacement_carries_no_status {
                let replacement: Option<StatusCode> = None;
                to clears_the_previously_set_status { equal(None) }
            }
        }
    }

    // ----- render::ntex_method: exhaustive leptos→ntex method map -------
    // Every `leptos_router::Method` variant maps to its ntex counterpart;
    // a collapse to `Method::default()` (GET) would misroute POST/PUT/etc.
    lets_expect! {
        expect(crate::render::ntex_method(method)) as the_mapped_ntex_method {
            let method = leptos_router::Method::Get;

            to maps_get { equal(ntex::http::Method::GET) }

            when the_method_is_post {
                let method = leptos_router::Method::Post;
                to maps_post { equal(ntex::http::Method::POST) }
            }

            when the_method_is_put {
                let method = leptos_router::Method::Put;
                to maps_put { equal(ntex::http::Method::PUT) }
            }

            when the_method_is_delete {
                let method = leptos_router::Method::Delete;
                to maps_delete { equal(ntex::http::Method::DELETE) }
            }

            when the_method_is_patch {
                let method = leptos_router::Method::Patch;
                to maps_patch { equal(ntex::http::Method::PATCH) }
            }
        }
    }

    // ----- NtexRouteListing getters: mode() and methods() ---------------
    // The getters must report the values the listing was built with, not the
    // type defaults (a collapse to `SsrMode::default()` / `[Method::Get]`
    // would silently re-mode and re-method every route).
    fn sample_listing() -> crate::NtexRouteListing {
        crate::NtexRouteListing::new(
            "/sample".to_string(),
            leptos_router::SsrMode::Async,
            [leptos_router::Method::Post],
            Vec::new(),
        )
    }

    lets_expect! {
        expect(sample_listing().mode()) as the_listing_mode {
            to reports_the_configured_mode { equal(leptos_router::SsrMode::Async) }
        }
    }

    lets_expect! {
        expect(sample_listing().methods().collect::<Vec<_>>()) as the_listing_methods {
            to reports_the_configured_methods { equal(vec![leptos_router::Method::Post]) }
        }
    }

    // ----- generate_route_list exclusion + path rendering ---------------
    // Two behaviours in one spec over the public route-list API:
    //   * every produced path carries its leading slash (the `to_ntex_path`
    //     segment-separator logic), and
    //   * an excluded path is dropped from the *active* listings (the
    //     `retain(!excluded)` filter).
    // `StaticApp` registers exactly `/` and `/about`.
    fn active_paths_after_excluding(excluded: &[&str]) -> Vec<String> {
        let excluded = if excluded.is_empty() {
            None
        } else {
            Some(excluded.iter().map(|s| s.to_string()).collect())
        };
        let mut paths = crate::generate_route_list_with_exclusions(StaticApp, excluded)
            .into_iter()
            .filter(|listing| !listing.exclude)
            .map(|listing| listing.path().to_string())
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }

    lets_expect! {
        expect(active_paths_after_excluding(excluded)) as the_active_route_paths {
            let excluded: &[&str] = &[];

            to lists_every_route_with_a_leading_slash {
                equal(vec!["/".to_string(), "/about".to_string()])
            }

            when a_route_is_excluded {
                let excluded: &[&str] = &["/about"];
                to drops_the_excluded_route_from_the_active_set {
                    equal(vec!["/".to_string()])
                }
            }
        }
    }

    #[ntex::test]
    async fn renders_root_route() {
        register_explicit::<EchoName>();
        register_explicit::<RedirectToAbout>();
        let routes = generate_route_list(UnitApp);
        let app = test::init_service(
            NtexApp::new()
                .route("/api/{tail}*", handle_server_fns())
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
                .route("/api/{tail}*", handle_server_fns())
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
        let routes = generate_route_list(UnitApp);
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
        let routes = generate_route_list(UnitApp);
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
        let routes = generate_route_list(UnitApp);
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
        let app =
            test::init_service(NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

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
            test::init_service(NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

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
            test::init_service(NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

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
            test::init_service(NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

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
            test::server(async || NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

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
            test::server(async || NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

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

    /// RFC 6455 §5.4: fragmented **text** messages must be reassembled
    /// before delivery — the mirror of the binary case above. Mutation
    /// testing flagged the `FirstText` size guard (`request.rs`
    /// `Item::FirstText`, `b.len() > payload_limit`): the at-limit text
    /// test sends exactly `== limit`, where `>` and `<` agree (both
    /// install), so it cannot pin the comparison's direction. A completing
    /// message whose opening fragment is strictly UNDER the limit does —
    /// the correct `>` installs it, while `<` would wrongly reject a normal
    /// small fragment as overflow. This also drives the under-limit
    /// `Continue`/`Last` accumulation guards.
    #[ntex::test]
    async fn websocket_server_fn_reassembles_fragmented_text() {
        use ntex::ws::Item;

        register_explicit::<EchoWebsocket>();

        let srv =
            test::server(async || NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

        let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
        let sink = conn.sink();
        let rx = conn.receiver();

        // Three chunks, each well under the (default) limit. `Item::FirstText`
        // carries raw bytes; the text kind is tracked by the opcode.
        let full = serialize_ws_ok(&"fragmented-text-hello");
        let (head, tail) = full.split_at(4);
        let (mid, last) = tail.split_at(tail.len() / 2);

        sink.send(ws::Message::Continuation(Item::FirstText(
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
                assert_eq!(echoed, "fragmented-text-hello");
            }
            other => panic!("unexpected websocket frame: {other:?}"),
        }

        sink.send(ws::Message::Close(None)).await.unwrap();
    }

    /// The TEXT mirror of the oversized-opening-fragment test: a `FirstText`
    /// fragment already past the limit must be rejected with `CloseCode::Size`
    /// before any buffer is established, exactly as `FirstBinary` is. With the
    /// at-limit and under-limit text tests this pins all three points of the
    /// `FirstText` size axis (the binary axis was already complete).
    #[ntex::test]
    async fn websocket_server_fn_first_text_fragment_over_limit_closes_with_size() {
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
                .route("/api/{tail}*", handle_server_fns())
        })
        .await;

        let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
        let sink = conn.sink();
        let rx = conn.receiver();

        let oversize = [b'A'; 64];
        sink.send(ws::Message::Continuation(Item::FirstText(
            oversize.to_vec().into(),
        )))
        .await
        .unwrap();

        let frame = recv_ws_frame(&rx).await;
        match frame {
            ws::Frame::Close(Some(reason)) => {
                assert_eq!(reason.code, CloseCode::Size);
            }
            other => panic!("expected Close(Size) on oversized FirstText frame, got {other:?}"),
        }
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
                .route("/api/{tail}*", handle_server_fns())
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
                .route("/api/{tail}*", handle_server_fns())
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

    // ----- WebSocket payload-limit BOUNDARY -----------------------------
    // The existing oversize tests send well past the limit, so they cannot
    // distinguish `len > limit` from `len >= limit`. These send a payload of
    // *exactly* `payload_limit` bytes: it is within the limit (the check is
    // strict `>`), so it must be DELIVERED, not closed with `Size`. A
    // 13-char string serializes to a 16-byte server-fn frame (1 ok-marker +
    // `"<13 chars>"`), matched to a 16-byte limit.

    /// Unfragmented binary frame exactly at the limit is echoed, not closed.
    #[ntex::test]
    async fn websocket_unfragmented_binary_exactly_at_limit_is_delivered() {
        register_explicit::<EchoWebsocket>();

        let srv = test::server(async || {
            NtexApp::new()
                .state(crate::LeptosServerFnConfig {
                    payload_limit: 16,
                    ws_channel_buffer: 16,
                    ..Default::default()
                })
                .route("/api/{tail}*", handle_server_fns())
        })
        .await;

        let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
        let sink = conn.sink();
        let rx = conn.receiver();

        let message = "a".repeat(13);
        let payload = serialize_ws_ok(&message);
        assert_eq!(payload.len(), 16, "payload must sit exactly on the limit");

        sink.send(ws::Message::Binary(payload.into()))
            .await
            .unwrap();

        let frame = recv_ws_frame(&rx).await;
        match frame {
            ws::Frame::Binary(bytes) => {
                let echoed: String = deserialize_ws_ok(&bytes);
                assert_eq!(echoed, message);
            }
            other => panic!("expected the at-limit message echoed, got {other:?}"),
        }

        sink.send(ws::Message::Close(None)).await.unwrap();
    }

    /// Unfragmented text frame exactly at the limit is echoed, not closed.
    #[ntex::test]
    async fn websocket_unfragmented_text_exactly_at_limit_is_delivered() {
        register_explicit::<EchoWebsocket>();

        let srv = test::server(async || {
            NtexApp::new()
                .state(crate::LeptosServerFnConfig {
                    payload_limit: 16,
                    ws_channel_buffer: 16,
                    ..Default::default()
                })
                .route("/api/{tail}*", handle_server_fns())
        })
        .await;

        let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
        let sink = conn.sink();
        let rx = conn.receiver();

        let message = "a".repeat(13);
        let payload = serialize_ws_ok(&message);
        assert_eq!(payload.len(), 16);
        let text = ntex::util::ByteString::try_from(payload).unwrap();

        sink.send(ws::Message::Text(text)).await.unwrap();

        let frame = recv_ws_frame(&rx).await;
        match frame {
            ws::Frame::Binary(bytes) => {
                let echoed: String = deserialize_ws_ok(&bytes);
                assert_eq!(echoed, message);
            }
            other => panic!("expected the at-limit text message echoed, got {other:?}"),
        }

        sink.send(ws::Message::Close(None)).await.unwrap();
    }

    /// A single opening fragment (`FirstBinary`) exactly at the limit is
    /// accepted; the terminal `Last` frame then completes the message at the
    /// limit too. Pins the `>` vs `>=` checks on First* and Last.
    #[ntex::test]
    async fn websocket_first_fragment_exactly_at_limit_is_delivered() {
        use ntex::ws::Item;

        register_explicit::<EchoWebsocket>();

        let srv = test::server(async || {
            NtexApp::new()
                .state(crate::LeptosServerFnConfig {
                    payload_limit: 16,
                    ws_channel_buffer: 16,
                    ..Default::default()
                })
                .route("/api/{tail}*", handle_server_fns())
        })
        .await;

        let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
        let sink = conn.sink();
        let rx = conn.receiver();

        let message = "a".repeat(13);
        let payload = serialize_ws_ok(&message);
        assert_eq!(payload.len(), 16);

        // Whole payload in the opening fragment (len == limit), then an empty
        // terminal frame (cumulative len == limit).
        sink.send(ws::Message::Continuation(Item::FirstBinary(payload.into())))
            .await
            .unwrap();
        sink.send(ws::Message::Continuation(Item::Last(Vec::new().into())))
            .await
            .unwrap();

        let frame = recv_ws_frame(&rx).await;
        match frame {
            ws::Frame::Binary(bytes) => {
                let echoed: String = deserialize_ws_ok(&bytes);
                assert_eq!(echoed, message);
            }
            other => panic!("expected the at-limit fragmented message echoed, got {other:?}"),
        }

        sink.send(ws::Message::Close(None)).await.unwrap();
    }

    /// A message reassembled across `Continue` to exactly the limit must be
    /// delivered. The cumulative size hits the limit on the `Continue` frame.
    #[ntex::test]
    async fn websocket_reassembled_continuation_exactly_at_limit_is_delivered() {
        use ntex::ws::Item;

        register_explicit::<EchoWebsocket>();

        let srv = test::server(async || {
            NtexApp::new()
                .state(crate::LeptosServerFnConfig {
                    payload_limit: 16,
                    ws_channel_buffer: 16,
                    ..Default::default()
                })
                .route("/api/{tail}*", handle_server_fns())
        })
        .await;

        let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
        let sink = conn.sink();
        let rx = conn.receiver();

        let message = "a".repeat(13);
        let payload = serialize_ws_ok(&message);
        assert_eq!(payload.len(), 16);

        // First(8) + Continue(8): cumulative hits 16 exactly on the Continue.
        sink.send(ws::Message::Continuation(Item::FirstBinary(
            payload[0..8].to_vec().into(),
        )))
        .await
        .unwrap();
        sink.send(ws::Message::Continuation(Item::Continue(
            payload[8..16].to_vec().into(),
        )))
        .await
        .unwrap();
        sink.send(ws::Message::Continuation(Item::Last(Vec::new().into())))
            .await
            .unwrap();

        let frame = recv_ws_frame(&rx).await;
        match frame {
            ws::Frame::Binary(bytes) => {
                let echoed: String = deserialize_ws_ok(&bytes);
                assert_eq!(echoed, message);
            }
            other => panic!("expected the at-limit reassembled message echoed, got {other:?}"),
        }

        sink.send(ws::Message::Close(None)).await.unwrap();
    }

    /// An opening TEXT fragment (`FirstText`) exactly at the limit is
    /// accepted (the `FirstText` size guard mirrors `FirstBinary`).
    #[ntex::test]
    async fn websocket_first_text_fragment_exactly_at_limit_is_delivered() {
        use ntex::ws::Item;

        register_explicit::<EchoWebsocket>();

        let srv = test::server(async || {
            NtexApp::new()
                .state(crate::LeptosServerFnConfig {
                    payload_limit: 16,
                    ws_channel_buffer: 16,
                    ..Default::default()
                })
                .route("/api/{tail}*", handle_server_fns())
        })
        .await;

        let conn = srv.ws_at(EchoWebsocket::PATH).await.unwrap();
        let sink = conn.sink();
        let rx = conn.receiver();

        let message = "a".repeat(13);
        let payload = serialize_ws_ok(&message);
        assert_eq!(payload.len(), 16);

        // `Item::FirstText` carries raw `Bytes` (the text-vs-binary kind is
        // tracked by the frame opcode, not the payload type).
        sink.send(ws::Message::Continuation(Item::FirstText(payload.into())))
            .await
            .unwrap();
        sink.send(ws::Message::Continuation(Item::Last(Vec::new().into())))
            .await
            .unwrap();

        let frame = recv_ws_frame(&rx).await;
        match frame {
            ws::Frame::Binary(bytes) => {
                let echoed: String = deserialize_ws_ok(&bytes);
                assert_eq!(echoed, message);
            }
            other => panic!("expected the at-limit text fragment echoed, got {other:?}"),
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

    /// A multi-segment route path keeps a `/` separator BETWEEN segments.
    /// Multi-segment paths arrive as separate `Static` segments WITHOUT
    /// leading slashes (a lone `/about` is stored whole, but `/outer/inner`
    /// splits), so the separator-insertion in `to_ntex_path` is load-bearing:
    /// dropping the `!raw.is_empty()` or `!raw.starts_with('/')` guard
    /// collapses `/outer/inner` into `outerinner`.
    #[test]
    fn nested_route_path_keeps_segment_separators() {
        let paths = generate_route_list(NestedApp)
            .into_iter()
            .map(|r| r.path().to_string())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec!["/outer/inner".to_string()]);
    }

    // ----- SSR streaming mode is observable in the body ------------------
    // A `<Suspense>` over a still-pending resource renders differently per
    // mode: OutOfOrder streams the shell + fallback first (the body carries
    // BOTH the fallback marker and an out-of-order replacement template);
    // InOrder/Async block for the resolved value (the body has it in place
    // and NO fallback marker). Deleting the InOrder/Async match arm falls
    // back to the OutOfOrder renderer, which these assertions catch — across
    // both the `App` and the `&mut ServiceConfig` implementations.
    async fn suspense_body_via_app_impl(path: &str) -> String {
        use crate::LeptosRoutes;
        let routes = generate_route_list(SuspenseApp);
        let app = test::init_service(NtexApp::new().leptos_routes(routes, suspense_shell)).await;
        let req = test::TestRequest::with_uri(path).to_request();
        let resp = test::call_service(&app, req).await;
        let body = test::read_body(resp).await;
        String::from_utf8(body.to_vec()).unwrap()
    }

    async fn suspense_body_via_service_config(path: &str) -> String {
        let routes = generate_route_list(SuspenseApp);
        let app = test::init_service(NtexApp::new().configure(|cfg| {
            register_leptos_routes(cfg, routes.clone(), suspense_shell);
        }))
        .await;
        let req = test::TestRequest::with_uri(path).to_request();
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

    #[ntex::test]
    async fn out_of_order_mode_streams_the_fallback_shell() {
        let html = suspense_body_via_app_impl("/out").await;
        assert!(
            html.contains("FALLBACK-MARKER"),
            "OutOfOrder must stream the shell + fallback before the resolved fragment"
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
                .route("/api/{tail}*", handle_server_fns())
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

    // ----- executor one-shot initialization: regression pins -----------
    // Two process-global, monotonic one-shot invariants. The executor state
    // is shared across the whole test binary, so these pin *runtime
    // independence* and a *stable result pair* (which arm is observed
    // depends on test order and cannot be set by a `when`), not a fixed
    // value. `any_spawner::ExecutorError` derives no `PartialEq`, so the
    // pair is matched with `match_pattern!`, not `equal`. Manual-Red is weak
    // for both (the code structurally avoids the failure modes) — these lock
    // in the current invariants rather than acting as rich behavioural specs.

    // A trivial app whose route walk must not require a ntex arbiter.
    #[component]
    fn Empty() -> impl IntoView {
        provide_meta_context();
        view! { <h1>"empty"</h1> }
    }

    // Invariant: `ensure_executor_initialized()` must not depend on a
    // running ntex arbiter — SSG and library-mode callers invoke public
    // entry points (like `generate_route_list`) without booting a ntex
    // runtime. The `lets_expect!`-generated test is a plain `#[test]` (no
    // `#[ntex::test]`), so it runs off any ntex system and enforces this.
    fn generate_routes_without_a_ntex_runtime() {
        let _routes = crate::generate_route_list(Empty);
        let _routes2 = crate::generate_route_list(Empty);
    }

    lets_expect! {
        expect(generate_routes_without_a_ntex_runtime()) as executor_initialization {
            to not_panic
        }
    }

    // Invariant: `try_init_executor()` is idempotent — two consecutive calls
    // return the same outcome (both `Ok`, or both `AlreadySet`); they can
    // never disagree. The subject is the ATOMIC pair, because the property
    // is a relationship between the two calls and `lets_expect` re-runs the
    // subject once per `to` block.
    lets_expect! {
        expect((crate::try_init_executor(), crate::try_init_executor())) as repeated_executor_init {
            to produces_a_stable_result_pair {
                match_pattern!(
                    (Ok(()), Ok(()))
                        | (
                            Err(any_spawner::ExecutorError::AlreadySet),
                            Err(any_spawner::ExecutorError::AlreadySet),
                        )
                ),
                not_match_pattern!((Ok(()), Err(_))),
                not_match_pattern!((Err(_), Ok(()))),
            }
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
        let app =
            test::init_service(NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

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
            test::init_service(NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

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
            "/{tail}*",
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

    /// The catch-all must reach the handler for *nested* paths (multi-segment),
    /// which the actix `/{tail:.*}` idiom did not in ntex — only `/{tail}*`
    /// does. Pins: a nested asset and an RFC 8615 `.well-known/*` file are
    /// served, while a nested dotfile stays hidden and a deep miss renders the
    /// 404 shell.
    #[ntex::test]
    async fn file_and_error_handler_serves_nested_paths_and_well_known() {
        use crate::file_and_error_handler;

        let site_root = temp_site_root("nested_paths");
        std::fs::create_dir_all(site_root.join("assets/css")).unwrap();
        std::fs::create_dir_all(site_root.join(".well-known/acme-challenge")).unwrap();
        std::fs::write(site_root.join("assets/css/app.css"), "body{color:red}").unwrap();
        std::fs::write(
            site_root.join(".well-known/acme-challenge/token"),
            "acme-proof",
        )
        .unwrap();
        std::fs::write(site_root.join(".env"), "API_KEY=secret").unwrap();

        let options = LeptosOptions::builder()
            .output_name("leptos_ntex_nested_paths")
            .site_root(site_root.to_string_lossy().to_string())
            .site_pkg_dir("pkg")
            .build();

        let app = test::init_service(NtexApp::new().state(options.clone()).route(
            "/{tail}*",
            file_and_error_handler(|_opts: LeptosOptions| view! { <h1>"Not Found Shell"</h1> }),
        ))
        .await;

        // Nested static asset (2 segments) is served.
        let resp = test::call_service(
            &app,
            test::TestRequest::with_uri("/assets/css/app.css").to_request(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = test::read_body(resp).await;
        assert_eq!(String::from_utf8(body.to_vec()).unwrap(), "body{color:red}");

        // RFC 8615 well-known asset (3 segments, leading dot) is served.
        let resp = test::call_service(
            &app,
            test::TestRequest::with_uri("/.well-known/acme-challenge/token").to_request(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = test::read_body(resp).await;
        assert_eq!(String::from_utf8(body.to_vec()).unwrap(), "acme-proof");

        // An ordinary dotfile is still hidden (renders the 404 shell).
        let resp =
            test::call_service(&app, test::TestRequest::with_uri("/.env").to_request()).await;
        assert_ne!(resp.status(), StatusCode::OK);
        let body = test::read_body(resp).await;
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(!html.contains("API_KEY"), "dotfile leaked: {html}");

        // A nested miss falls back to the shell, not a bare router 404.
        let resp = test::call_service(
            &app,
            test::TestRequest::with_uri("/deep/missing/page").to_request(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = test::read_body(resp).await;
        assert!(
            String::from_utf8(body.to_vec())
                .unwrap()
                .contains("Not Found Shell"),
            "nested miss must reach the handler and render the shell"
        );

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
            "/{tail}*",
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
            "/{tail}*",
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
                "/{tail}*",
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
        let (routes, _generator) = generate_route_list_with_ssg(StaticApp);
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
                .route("/api/{tail}*", handle_server_fns())
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
            test::server(async || NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

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
                .route("/api/{tail}*", handle_server_fns())
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
            test::server(async || NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

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
                .route("/api/{tail}*", handle_server_fns())
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
            test::server(async || NtexApp::new().route("/api/{tail}*", handle_server_fns())).await;

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
