use super::*;
use crate::{LeptosRoutes, register_leptos_routes};
use lets_expect::*;
use ntex::http::{Method as HttpMethod, StatusCode, header};
use ntex::web::{App as NtexApp, test};

#[derive(Clone, Copy)]
enum Registration {
    App,
    ServiceConfig,
}

#[derive(Debug)]
struct Page {
    status: StatusCode,
    content_type: Option<String>,
    html: String,
}

fn render_page(marker: &'static str) -> impl Fn(&Page) -> AssertionResult {
    move |page| {
        let mut errors = Vec::new();
        if page.status != StatusCode::OK {
            errors.push(format!("expected 200, got {}", page.status));
        }
        if page.content_type.as_deref() != Some("text/html; charset=utf-8") {
            errors.push(format!(
                "expected HTML content type, got {:?}",
                page.content_type
            ));
        }
        if !page.html.contains(marker) {
            errors.push(format!("missing {marker:?} in body {:?}", page.html));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(AssertionError::new(errors))
        }
    }
}

// The upstream global suppression race is covered by a separate controlled
// reproduction. Resource-bearing fixtures here serialize against generation;
// this guard does not establish production isolation (see ROUTE_GEN_VS_RENDER).
#[allow(clippy::await_holding_lock)]
async fn registered_page<A: IntoView + 'static, V: IntoView + 'static>(
    registration: Registration,
    app_fn: fn() -> A,
    shell: fn() -> V,
    uri: &'static str,
    method: HttpMethod,
    has_resource: bool,
) -> Page {
    let routes = gen_route_list(app_fn);
    let app = NtexApp::new();
    let app = match registration {
        Registration::App => app.leptos_routes(routes, shell),
        Registration::ServiceConfig => app.configure(move |cfg| {
            register_leptos_routes(cfg, routes, shell);
        }),
    };
    let app = test::init_service(app).await;
    let _resource_guard = has_resource.then(|| {
        ROUTE_GEN_VS_RENDER
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    });
    let response = test::call_service(
        &app,
        test::TestRequest::default()
            .method(method)
            .uri(uri)
            .to_request(),
    )
    .await;
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_string());
    let html = String::from_utf8(test::read_body(response).await.to_vec()).unwrap();
    Page {
        status,
        content_type,
        html,
    }
}

lets_expect! {
    expect(run_ntex(registered_page(registration, UnitApp, unit_shell, uri, HttpMethod::GET, false))) as registered_pages {
        let registration = Registration::App;
        let uri = "/";
        to renders_the_root { render_page("Leptos over ntex") }
        when requesting_about {
            let uri = "/about";
            to renders_the_about_page { render_page("This route is generated from the Leptos router") }
        }
        when requesting_an_unregistered_path {
            let uri = "/missing";
            to falls_through_to_the_app_default { have(status) equal(StatusCode::NOT_FOUND) }
        }
    }
    expect(run_ntex(registered_page(registration, UnitApp, unit_shell, uri, HttpMethod::GET, false))) as registered_pages_via_service_config {
        let registration = Registration::ServiceConfig;
        let uri = "/";

        to renders_the_root { render_page("Leptos over ntex") }
        when requesting_about {
            let uri = "/about";
            to renders_the_about_page { render_page("This route is generated from the Leptos router") }
        }
        when requesting_an_unregistered_path {
            let uri = "/missing";
            to falls_through_to_the_app_default { have(status) equal(StatusCode::NOT_FOUND) }
        }

    }
    expect(run_ntex(registered_page(registration, SplatApp, splat_shell, uri, HttpMethod::GET, false))) as parameter_route_matching {
        let registration = Registration::App;
        let uri = "/users/42";
        to matches_one_segment { render_page("User Param") }
        when the_parameter_is_absent {
            let uri = "/users/";
            to rejects_the_missing_parameter { have(status) equal(StatusCode::NOT_FOUND) }
        }
        when the_parameter_has_an_extra_segment {
            let uri = "/users/42/extra";
            to rejects_more_than_one_segment { have(status) equal(StatusCode::NOT_FOUND) }
        }
    }
    expect(run_ntex(registered_page(registration, SplatApp, splat_shell, uri, HttpMethod::GET, false))) as parameter_route_matching_via_service_config {
        let registration = Registration::ServiceConfig;
        let uri = "/users/42";

        to matches_one_segment { render_page("User Param") }
        when the_parameter_is_absent {
            let uri = "/users/";
            to rejects_the_missing_parameter { have(status) equal(StatusCode::NOT_FOUND) }
        }
        when the_parameter_has_an_extra_segment {
            let uri = "/users/42/extra";
            to rejects_more_than_one_segment { have(status) equal(StatusCode::NOT_FOUND) }
        }

    }
    expect(run_ntex(registered_page(registration, SplatApp, splat_shell, uri, HttpMethod::GET, false))) as splat_route_matching {
        let registration = Registration::App;
        let uri = "/files/report.txt";
        to matches_one_segment { render_page("Splat Files") }
        when the_tail_has_nested_segments {
            let uri = "/files/a/b/report.txt";
            to matches_the_complete_tail { render_page("Splat Files") }
        }
    }
    expect(run_ntex(registered_page(registration, SplatApp, splat_shell, uri, HttpMethod::GET, false))) as splat_route_matching_via_service_config {
        let registration = Registration::ServiceConfig;
        let uri = "/files/report.txt";

        to matches_one_segment { render_page("Splat Files") }
        when the_tail_has_nested_segments {
            let uri = "/files/a/b/report.txt";
            to matches_the_complete_tail { render_page("Splat Files") }
        }

    }
}

#[component]
fn PartiallyBlockedApp() -> impl IntoView {
    provide_meta_context();
    view! {
        <Router><main><Routes fallback=|| ()>
            <Route path=path!("/partial") ssr=SsrMode::PartiallyBlocked view=|| view! { <h1>"PartiallyBlocked"</h1> }/>
        </Routes></main></Router>
    }
}
fn partially_blocked_shell() -> impl IntoView {
    view! { <!DOCTYPE html><html><head><MetaTags/></head><body><PartiallyBlockedApp/></body></html> }
}

// A separate subject covers ready mode dispatch; the pending-resource subject
// below observes each mode's distinct Suspense result. PartiallyBlocked's
// replace_blocks argument is currently a documented no-op upstream.
lets_expect! {
    expect(run_ntex(registered_page(registration, MixedApp, mixed_shell, uri, HttpMethod::GET, false))) as ready_ssr_modes {
        let registration = Registration::App;
        let uri = "/out";
        to renders_out_of_order { render_page("OutOfOrder") }
        when configured_in_order {
            let uri = "/in";
            to renders_in_order { render_page("InOrder") }
        }
        when configured_async {
            let uri = "/async";
            to renders_async { render_page("Async") }
        }
    }
    expect(run_ntex(registered_page(registration, MixedApp, mixed_shell, uri, HttpMethod::GET, false))) as ready_ssr_modes_via_service_config {
        let registration = Registration::ServiceConfig;
        let uri = "/out";

        to renders_out_of_order { render_page("OutOfOrder") }
        when configured_in_order {
            let uri = "/in";
            to renders_in_order { render_page("InOrder") }
        }
        when configured_async {
            let uri = "/async";
            to renders_async { render_page("Async") }
        }

    }
    expect(run_ntex(registered_page(registration, PartiallyBlockedApp, partially_blocked_shell, "/partial", HttpMethod::GET, false))) as partially_blocked_dispatch {
        let registration = Registration::App;
        to reaches_the_renderer { render_page("PartiallyBlocked") }
    }
    expect(run_ntex(registered_page(registration, PartiallyBlockedApp, partially_blocked_shell, "/partial", HttpMethod::GET, false))) as partially_blocked_dispatch_via_service_config {
        let registration = Registration::ServiceConfig;

        to reaches_the_renderer { render_page("PartiallyBlocked") }

    }
}

fn render_suspense(fallback: bool) -> impl Fn(&Page) -> AssertionResult {
    move |page| {
        render_page("RESOLVED-CONTENT")(page)?;
        if page.html.contains("FALLBACK-MARKER") == fallback {
            Ok(())
        } else {
            Err(AssertionError::new(vec![format!(
                "expected fallback presence {fallback}, body {:?}",
                page.html
            )]))
        }
    }
}

lets_expect! {
    expect(run_ntex(registered_page(registration, SuspenseApp, suspense_shell, uri, HttpMethod::GET, true))) as pending_resource_ssr {
        let registration = Registration::App;
        let uri = "/out";
        to retains_fallback_and_resolved_content { render_suspense(true) }
        when configured_in_order {
            let uri = "/in";
            to resolves_the_value_without_the_fallback { render_suspense(false) }
        }
        when configured_async {
            let uri = "/async";
            to resolves_the_value_without_the_fallback { render_suspense(false) }
        }
    }
    expect(run_ntex(registered_page(registration, SuspenseApp, suspense_shell, uri, HttpMethod::GET, true))) as pending_resource_ssr_via_service_config {
        let registration = Registration::ServiceConfig;
        let uri = "/out";

        to retains_fallback_and_resolved_content { render_suspense(true) }
        when configured_in_order {
            let uri = "/in";
            to resolves_the_value_without_the_fallback { render_suspense(false) }
        }
        when configured_async {
            let uri = "/async";
            to resolves_the_value_without_the_fallback { render_suspense(false) }
        }

    }
}

// The in-memory service bypasses the wire encoder. Body suppression for HEAD
// is checked over a real connection in integration.rs.
fn head_parity(registration: Registration, uri: &'static str) -> (StatusCode, StatusCode, bool) {
    run_ntex(async move {
        let get = registered_page(
            registration,
            MixedApp,
            mixed_shell,
            uri,
            HttpMethod::GET,
            false,
        )
        .await;
        let head = registered_page(
            registration,
            MixedApp,
            mixed_shell,
            uri,
            HttpMethod::HEAD,
            false,
        )
        .await;
        (
            get.status,
            head.status,
            get.content_type == head.content_type,
        )
    })
}
lets_expect! {
    expect(head_parity(registration, uri)) as head_response_parity {
        let registration = Registration::App;
        let uri = "/out";
        to mirrors_get_status_and_content_type { equal((StatusCode::OK, StatusCode::OK, true)) }
        when the_route_is_missing {
            let uri = "/totally-bogus-path";
            to preserves_the_not_found_result { equal((StatusCode::NOT_FOUND, StatusCode::NOT_FOUND, true)) }
        }
    }
    expect(head_parity(registration, uri)) as head_response_parity_via_service_config {
        let registration = Registration::ServiceConfig;
        let uri = "/out";

        to mirrors_get_status_and_content_type { equal((StatusCode::OK, StatusCode::OK, true)) }
        when the_route_is_missing {
            let uri = "/totally-bogus-path";
            to preserves_the_not_found_result { equal((StatusCode::NOT_FOUND, StatusCode::NOT_FOUND, true)) }
        }

    }
}

#[derive(Clone, Copy)]
enum Helper {
    Stream,
    InOrder,
    Async,
    Inner,
}
async fn helper_page(helper: Helper) -> Page {
    let route = match helper {
        Helper::Stream => crate::render_app_to_stream(unit_shell, leptos_router::Method::Get),
        Helper::InOrder => {
            crate::render_app_to_stream_in_order(unit_shell, leptos_router::Method::Get)
        }
        Helper::Async => crate::render_app_async(unit_shell, leptos_router::Method::Get),
        Helper::Inner => ntex::web::get().to(|req: ntex::web::HttpRequest| {
            crate::handle_response_inner(
                || {},
                unit_shell,
                req,
                crate::render::async_stream_builder,
            )
        }),
    };
    let app = test::init_service(NtexApp::new().route("/", route)).await;
    let response = test::call_service(&app, test::TestRequest::get().uri("/").to_request()).await;
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_string());
    let html = String::from_utf8(test::read_body(response).await.to_vec()).unwrap();
    Page {
        status,
        content_type,
        html,
    }
}
lets_expect! {
    expect(run_ntex(helper_page(helper))) as public_render_helpers {
        let helper = Helper::Stream;
        to renders_the_app { render_page("Leptos over ntex") }
        when using_the_in_order_helper {
            let helper = Helper::InOrder;
            to renders_the_app { render_page("Leptos over ntex") }
        }
        when using_the_async_helper {
            let helper = Helper::Async;
            to renders_the_app { render_page("Leptos over ntex") }
        }
        when composing_the_low_level_handler {
            let helper = Helper::Inner;
            to renders_the_app { render_page("Leptos over ntex") }
        }
    }
}

fn cloned_request_heads() -> [(String, Option<String>, Option<String>); 2] {
    let req = test::TestRequest::default()
        .uri("/some/path?q=1")
        .header("x-custom", "yes")
        .to_http_request();
    let (server_fn_request, head) =
        crate::generate_request_and_parts(req, ntex::http::Payload::None);
    let (request, _) = server_fn_request.take();
    [head, request].map(|request| {
        (
            request.uri().path().to_string(),
            request.uri().query().map(str::to_string),
            request
                .headers()
                .get("x-custom")
                .map(|v| v.to_str().unwrap().to_string()),
        )
    })
}
lets_expect! {
    expect(cloned_request_heads()) as server_function_request_head_copies {
        to preserves_path_query_and_headers_in_both_outputs {
            equal([
                ("/some/path".to_string(), Some("q=1".to_string()), Some("yes".to_string())),
                ("/some/path".to_string(), Some("q=1".to_string()), Some("yes".to_string())),
            ])
        }
    }
}
