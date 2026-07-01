//! SSR pipeline: request-to-HTML rendering helpers.
//!
//! Hosts [`handle_response_inner`], the `render_app_*` family of route
//! builders, and the internal helpers they share with the static-route
//! generator and the file-fallback handler.

use futures::{StreamExt, stream::once};
use leptos::{IntoView, context::provide_context, hydration::IslandsRouterNavigation};
use leptos_integration_utils::{BoxedFnOnce, ExtendResponse, PinnedFuture, PinnedStream};
use leptos_meta::ServerMetaContext;
use leptos_router::{Method, SsrMode, components::provide_server_redirect, location::RequestUrl};
use ntex::web::{ErrorRenderer, HttpRequest, HttpResponse, Route};
use send_wrapper::SendWrapper;

use crate::request::Request;
use crate::response::{NtexResponse, ResponseOptions, redirect};
use crate::routes::ensure_executor_initialized;

pub(crate) fn leptos_corrected_path(req: &HttpRequest) -> String {
    let path = req.path();
    let query = req.query_string();
    if query.is_empty() {
        format!("http://leptos{path}")
    } else {
        format!("http://leptos{path}?{query}")
    }
}

pub(crate) fn provide_contexts(
    req: Request,
    meta_context: &ServerMetaContext,
    res_options: &ResponseOptions,
) {
    provide_context(RequestUrl::new(&leptos_corrected_path(&req)));
    provide_context(meta_context.clone());
    provide_context(res_options.clone());
    provide_context(req);
    provide_server_redirect(redirect);
    leptos::nonce::provide_nonce();
}

pub(crate) fn async_stream_builder<IV>(
    app: IV,
    chunks: BoxedFnOnce<PinnedStream<String>>,
    _supports_ooo: bool,
) -> PinnedFuture<PinnedStream<String>>
where
    IV: IntoView + 'static,
{
    Box::pin(async move {
        let app = if cfg!(feature = "islands-router") {
            app.to_html_stream_in_order_branching()
        } else {
            app.to_html_stream_in_order()
        };
        let app = app.collect::<String>().await;
        let chunks = chunks();
        Box::pin(once(async move { app }).chain(chunks)) as PinnedStream<String>
    })
}

pub(crate) fn ntex_method(method: Method) -> ntex::http::Method {
    match method {
        Method::Get => ntex::http::Method::GET,
        Method::Post => ntex::http::Method::POST,
        Method::Put => ntex::http::Method::PUT,
        Method::Delete => ntex::http::Method::DELETE,
        Method::Patch => ntex::http::Method::PATCH,
    }
}

/// Builds an ntex [`Route`] that serves `500 Internal Server Error` for an
/// [`SsrMode`] this integration does not know how to render.
///
/// [`SsrMode`] is `#[non_exhaustive]`, so the rendering `match` in
/// [`LeptosRoutes`](crate::LeptosRoutes) needs a catch-all arm. Rather than
/// silently rendering an unknown future variant as out-of-order — which could
/// emit subtly wrong output — or panicking the worker with `unreachable!()`,
/// log and serve a typed 500. Mirrors `unsupported_ssr_mode_route` in
/// `leptos_actix` (leptos-rs/leptos#4755).
///
/// HEAD is bound alongside GET (as the render routes are, via the
/// AND-combining-`.method()` caveat documented in [`handle_response`]), so a
/// HEAD to such a route also 500s rather than 404ing — a small, intentional
/// divergence from the PR's GET-only registration that keeps HEAD handling
/// uniform across this integration.
pub(crate) fn unsupported_ssr_mode_route<Err>(method: Method, mode: &SsrMode) -> Route<Err>
where
    Err: ErrorRenderer,
{
    #[cfg(feature = "tracing")]
    tracing::error!("unsupported SSR mode {mode:?} for this route; serving 500");
    #[cfg(not(feature = "tracing"))]
    eprintln!("unsupported SSR mode {mode:?} for this route; serving 500");

    let route = Route::<Err>::new();
    let route = if matches!(method, Method::Get) {
        route.guard(ntex::web::guard::Any(ntex::web::guard::Get()).or(ntex::web::guard::Head()))
    } else {
        route.method(ntex_method(method))
    };
    route.to(|| async {
        HttpResponse::InternalServerError().body("This rendering mode is not supported.")
    })
}

#[allow(clippy::type_complexity)]
fn handle_response<IV, Err>(
    method: Method,
    additional_context: impl Fn() + 'static + Clone + Send,
    app_fn: impl Fn() -> IV + Clone + Send + 'static,
    stream_builder: fn(
        IV,
        BoxedFnOnce<PinnedStream<String>>,
        bool,
    ) -> PinnedFuture<PinnedStream<String>>,
) -> Route<Err>
where
    Err: ErrorRenderer,
    IV: IntoView + 'static,
{
    let handler = move |req: HttpRequest| {
        let add_context = additional_context.clone();
        let app_fn = app_fn.clone();
        handle_response_inner(add_context, app_fn, req, stream_builder)
    };
    let route = Route::<Err>::new();
    // RFC 9110 §9.3.2: HEAD == GET without body. ntex's h1 encoder
    // unconditionally strips the body when the request method is HEAD
    // (ntex-3.9.6/src/http/h1/encoder.rs forces
    // `TransferEncoding::empty()`), so binding the same handler to
    // both methods produces the correct status and headers with no
    // body on the wire. `Route::method()` pushes onto a Vec that
    // `take_guards()` turns into AND-combined guards on the owning
    // Resource (see ntex/web/route.rs:35-43), which makes it unusable
    // for multi-method matching — use `Any(..).or(..)` instead.
    let route = if matches!(method, Method::Get) {
        route.guard(ntex::web::guard::Any(ntex::web::guard::Get()).or(ntex::web::guard::Head()))
    } else {
        route.method(ntex_method(method))
    };
    route.to(handler)
}

/// Low-level building block: runs the SSR pipeline for a single request
/// and returns the resulting [`HttpResponse`] future.
///
/// Exposed so that advanced users can compose it inside their own ntex
/// route handlers — for example, to render a shell after a custom
/// static-file fallback, or to add middleware that needs to inspect the
/// response body before sending. The `render_app_*` family doesn't use
/// this directly (they wrap a similar pipeline in a `Route`), but this
/// function mirrors `leptos_axum::handle_response_inner` so existing
/// axum-style code can be adapted.
///
/// The `stream_builder` argument selects how the HTML body is produced:
/// out-of-order / in-order / async. See the [`render_app_to_stream`] family
/// for common choices.
///
/// # Panics
///
/// The returned future captures the non-[`Send`] ntex [`HttpRequest`] inside a
/// [`SendWrapper`]. Although the future's *type* is `Send` (so it composes
/// with `Send`-bounded combinators), it must be **polled and dropped on the
/// thread that created it** — the ntex worker handling the request. Polling or
/// dropping it on another thread panics. In particular, do **not**
/// `tokio::spawn` it or move it onto a foreign runtime's thread pool, despite
/// the `Send` type that the `leptos_axum::handle_response_inner` shape invites.
/// Unlike [`Request`], this future has no leak-instead-of-panic drop guard.
#[allow(clippy::type_complexity)]
pub fn handle_response_inner<IV>(
    additional_context: impl FnOnce() + 'static + Send,
    app_fn: impl FnOnce() -> IV + Send + 'static,
    req: HttpRequest,
    stream_builder: fn(
        IV,
        BoxedFnOnce<PinnedStream<String>>,
        bool,
    ) -> PinnedFuture<PinnedStream<String>>,
) -> PinnedFuture<HttpResponse>
where
    IV: IntoView + 'static,
{
    ensure_executor_initialized();
    Box::pin(SendWrapper::new(async move {
        let is_island_router_navigation =
            cfg!(feature = "islands-router") && req.headers().contains_key("Islands-Router");
        let res_options = ResponseOptions::default();
        let (meta_context, meta_output) = ServerMetaContext::new();

        let cx = {
            let meta_context = meta_context.clone();
            let res_options = res_options.clone();
            let req_ctx = Request::new(&req);
            move || {
                provide_contexts(req_ctx, &meta_context, &res_options);
                additional_context();
                if is_island_router_navigation {
                    provide_context(IslandsRouterNavigation);
                }
            }
        };

        let res = NtexResponse::from_app(
            app_fn,
            meta_output,
            cx,
            res_options,
            stream_builder,
            !is_island_router_navigation,
        )
        .await;

        res.take()
    }))
}

/// Returns an ntex [`Route`] that responds to a request for the given
/// [`Method`] by rendering your app as an out-of-order HTML stream.
///
/// The stream includes fallback content for any `<Suspense/>` nodes, is
/// immediately interactive, and requires some client-side JavaScript.
///
/// ## Provided Context Types
/// - [`ResponseOptions`]
/// - [`Request`]
/// - [`ServerMetaContext`]
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(level = "trace", fields(error), skip_all)
)]
pub fn render_app_to_stream<IV, Err>(
    app_fn: impl Fn() -> IV + Clone + Send + 'static,
    method: Method,
) -> Route<Err>
where
    Err: ErrorRenderer,
    IV: IntoView + 'static,
{
    render_app_to_stream_with_context(|| {}, app_fn, method)
}

/// Returns an ntex [`Route`] that responds by rendering your app as an
/// in-order HTML stream.
///
/// The stream pauses at each `<Suspense/>` node and waits for it to resolve
/// before sending down its HTML. The app becomes interactive only once it
/// has fully loaded.
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(level = "trace", fields(error), skip_all)
)]
pub fn render_app_to_stream_in_order<IV, Err>(
    app_fn: impl Fn() -> IV + Clone + Send + 'static,
    method: Method,
) -> Route<Err>
where
    Err: ErrorRenderer,
    IV: IntoView + 'static,
{
    render_app_to_stream_in_order_with_context(|| {}, app_fn, method)
}

/// Returns an ntex [`Route`] that renders the app asynchronously, emitting a
/// single HTML body once every `async` resource has loaded.
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(level = "trace", fields(error), skip_all)
)]
pub fn render_app_async<IV, Err>(
    app_fn: impl Fn() -> IV + Clone + Send + 'static,
    method: Method,
) -> Route<Err>
where
    Err: ErrorRenderer,
    IV: IntoView + 'static,
{
    render_app_async_with_context(|| {}, app_fn, method)
}

/// Variant of [`render_app_to_stream`] that lets you inject additional values
/// into the reactive context when handling a route.
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(level = "trace", fields(error), skip_all)
)]
pub fn render_app_to_stream_with_context<IV, Err>(
    additional_context: impl Fn() + 'static + Clone + Send,
    app_fn: impl Fn() -> IV + Clone + Send + 'static,
    method: Method,
) -> Route<Err>
where
    Err: ErrorRenderer,
    IV: IntoView + 'static,
{
    render_app_to_stream_with_context_and_replace_blocks(additional_context, app_fn, method, false)
}

/// Variant of [`render_app_to_stream_with_context`] that additionally
/// controls whether `<Suspense/>` fragments reading blocking resources are
/// retrojected into the initially served HTML instead of being inserted by
/// client-side JavaScript.
///
/// ⚠ **Currently a no-op:** Leptos's HTML streaming APIs do not yet expose
/// a `replace_blocks` toggle, so this argument is accepted for API parity
/// with `leptos_actix` / `leptos_axum` but has no effect. This means
/// [`SsrMode::PartiallyBlocked`](leptos_router::SsrMode) produces the same
/// HTML stream as [`SsrMode::OutOfOrder`](leptos_router::SsrMode) across
/// all three integrations until upstream Leptos wires this through.
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(level = "trace", fields(error), skip_all)
)]
pub fn render_app_to_stream_with_context_and_replace_blocks<IV, Err>(
    additional_context: impl Fn() + 'static + Clone + Send,
    app_fn: impl Fn() -> IV + Clone + Send + 'static,
    method: Method,
    replace_blocks: bool,
) -> Route<Err>
where
    Err: ErrorRenderer,
    IV: IntoView + 'static,
{
    // TODO(upstream): Leptos's HTML stream APIs (`to_html_stream_out_of_order`,
    // `to_html_stream_in_order`, etc.) do not currently expose a flag to
    // retroject blocking `<Suspense/>` fragments into the initial payload.
    // This argument is accepted for API parity with `leptos_actix` and
    // `leptos_axum` (both of which have the same `_ = replace_blocks;`
    // placeholder) but has no effect here or there until upstream wires it
    // through. Track: https://github.com/leptos-rs/leptos (no dedicated
    // issue — search for "replace_blocks" / "PartiallyBlocked").
    _ = replace_blocks;
    handle_response(
        method,
        additional_context,
        app_fn,
        |app, chunks, supports_ooo| {
            Box::pin(async move {
                let app = if cfg!(feature = "islands-router") {
                    if supports_ooo {
                        app.to_html_stream_out_of_order_branching()
                    } else {
                        app.to_html_stream_in_order_branching()
                    }
                } else if supports_ooo {
                    app.to_html_stream_out_of_order()
                } else {
                    app.to_html_stream_in_order()
                };
                Box::pin(app.chain(chunks())) as PinnedStream<String>
            })
        },
    )
}

/// Variant of [`render_app_to_stream_in_order`] that lets you inject
/// additional values into the reactive context when handling a route.
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(level = "trace", fields(error), skip_all)
)]
pub fn render_app_to_stream_in_order_with_context<IV, Err>(
    additional_context: impl Fn() + 'static + Clone + Send,
    app_fn: impl Fn() -> IV + Clone + Send + 'static,
    method: Method,
) -> Route<Err>
where
    Err: ErrorRenderer,
    IV: IntoView + 'static,
{
    handle_response(
        method,
        additional_context,
        app_fn,
        |app, chunks, _supports_ooo| {
            Box::pin(async move {
                let app = if cfg!(feature = "islands-router") {
                    app.to_html_stream_in_order_branching()
                } else {
                    app.to_html_stream_in_order()
                };
                Box::pin(app.chain(chunks())) as PinnedStream<String>
            })
        },
    )
}

/// Variant of [`render_app_async`] that lets you inject additional values
/// into the reactive context when handling a route.
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(level = "trace", fields(error), skip_all)
)]
pub fn render_app_async_with_context<IV, Err>(
    additional_context: impl Fn() + 'static + Clone + Send,
    app_fn: impl Fn() -> IV + Clone + Send + 'static,
    method: Method,
) -> Route<Err>
where
    Err: ErrorRenderer,
    IV: IntoView + 'static,
{
    handle_response(method, additional_context, app_fn, async_stream_builder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntex::http::StatusCode;
    use ntex::web::{App, test};

    // A future `SsrMode` this integration cannot render must serve a typed 500
    // — not panic the worker via `unreachable!()`, and not silently mis-render
    // as OutOfOrder. These tests call `unsupported_ssr_mode_route` DIRECTLY
    // (not through the dispatch `match`), so the `SsrMode` argument is just an
    // arbitrary value passed to the 500 route builder — `SsrMode::Async`
    // happens to be convenient and is NOT a claim that dispatch fails to handle
    // Async (`leptos_routes` handles it explicitly). The route this helper
    // builds is the catch-all a new `#[non_exhaustive]` variant falls through
    // to. Mirrors `leptos_actix` (leptos-rs/leptos#4755).
    #[ntex::test]
    async fn unsupported_ssr_mode_serves_500() {
        let app = test::init_service(App::new().route(
            "/",
            unsupported_ssr_mode_route(Method::Get, &SsrMode::Async),
        ))
        .await;

        let req = test::TestRequest::get().uri("/").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        // Pin the exact body, not just the status: any 500 (or an empty body)
        // would otherwise pass.
        let body = test::read_body(resp).await;
        assert_eq!(
            String::from_utf8(body.to_vec()).unwrap(),
            "This rendering mode is not supported."
        );
    }

    // HEAD must reach the same 500: the helper binds GET+HEAD for a `Method::Get`
    // route (an intentional divergence from the PR's GET-only registration), so
    // a HEAD must not fall through to 404/405. Pins that divergence.
    #[ntex::test]
    async fn unsupported_ssr_mode_serves_500_for_head() {
        let app = test::init_service(App::new().route(
            "/",
            unsupported_ssr_mode_route(Method::Get, &SsrMode::Async),
        ))
        .await;

        let req = test::TestRequest::default()
            .method(ntex::http::Method::HEAD)
            .uri("/")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    // A non-GET route exercises the other branch (`.method(ntex_method(method))`)
    // and must serve the 500 on its own method.
    #[ntex::test]
    async fn unsupported_ssr_mode_serves_500_for_non_get_method() {
        let app = test::init_service(App::new().route(
            "/",
            unsupported_ssr_mode_route(Method::Post, &SsrMode::Async),
        ))
        .await;

        let req = test::TestRequest::post().uri("/").to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        // Mirror the exact-body pin from `unsupported_ssr_mode_serves_500`:
        // both branches currently funnel into the same handler closure, so a
        // body regression specific to the non-GET branch must be caught here
        // too, not just on the GET branch's own test.
        let body = test::read_body(resp).await;
        assert_eq!(
            String::from_utf8(body.to_vec()).unwrap(),
            "This rendering mode is not supported."
        );
    }

    // The route must reject a MISMATCHED method (it is not an all-method
    // catch-all): a POST to a `Method::Get` route must NOT 500. `App::route`
    // (ntex-3.9.6 src/web/app.rs:227-233) hoists the route's guards — the
    // method guard included — onto the wrapping `Resource` via
    // `take_guards()`/`add_guards()`, so a mismatched method fails the
    // Resource-level guard match itself: the app's router never selects this
    // Resource at all, and it falls through to the app's own 404 default
    // handler. `Resource`'s inner `MethodNotAllowed` (resource.rs:520) is
    // reached only when the Resource IS selected but no route inside it
    // matches (e.g. several `.route()` calls sharing a `web::resource(..)`
    // without per-route method guards) — not this crate's single-route-per-
    // path registration. Verified by direct repro against ntex-3.9.6, not
    // assumed from source alone.
    #[ntex::test]
    async fn unsupported_ssr_mode_get_route_rejects_post() {
        let get_route = test::init_service(App::new().route(
            "/",
            unsupported_ssr_mode_route(Method::Get, &SsrMode::Async),
        ))
        .await;
        let resp =
            test::call_service(&get_route, test::TestRequest::post().uri("/").to_request()).await;
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "POST to a GET-only route must be rejected with exactly 404, got {}",
            resp.status()
        );
    }

    // The `Method::Post` sibling: a GET to a `Method::Post` route must NOT
    // 500 either — same 404 rejection (see comment above), its own pass/fail
    // signal.
    #[ntex::test]
    async fn unsupported_ssr_mode_post_route_rejects_get() {
        let post_route = test::init_service(App::new().route(
            "/",
            unsupported_ssr_mode_route(Method::Post, &SsrMode::Async),
        ))
        .await;
        let resp =
            test::call_service(&post_route, test::TestRequest::get().uri("/").to_request()).await;
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "GET to a POST-only route must be rejected with exactly 404, got {}",
            resp.status()
        );
    }

    // `is_island_router_navigation` (islands-router header detection) drives
    // the `supports_ooo` flag threaded into the stream builder. This crate is
    // built with the `islands-router` feature OFF by default, so
    // `cfg!(feature = "islands-router")` is `false` here and the whole
    // expression short-circuits to `false` regardless of the header — the
    // header must be a strict no-op in that configuration. Capture the
    // `supports_ooo` value the stream builder actually observes (rather than
    // inspecting the body, which carries no visible trace of the flag) so a
    // broken/inverted header-detection predicate is caught even though the
    // feature is off.
    // `stream_builder` must be a plain `fn` pointer (no captures), so the
    // observed `supports_ooo` value is smuggled out through a thread-local
    // instead of a closure capture. `#[ntex::test]` runs single-threaded, so
    // the thread-local is set and read on the same thread within each call.
    thread_local! {
        static SEEN_SUPPORTS_OOO: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    }

    #[ntex::test]
    async fn islands_router_header_toggles_supports_ooo_only_when_the_feature_is_on() {
        use leptos::prelude::*;

        async fn supports_ooo_seen_for(with_header: bool) -> bool {
            SEEN_SUPPORTS_OOO.with(|cell| cell.set(None));
            let app = test::init_service(App::new().route(
                "/",
                ntex::web::get().to(|req: HttpRequest| async move {
                    handle_response_inner(
                        || {},
                        || view! { <h1>"IslandsProbe"</h1> },
                        req,
                        |app, chunks, supports_ooo| {
                            SEEN_SUPPORTS_OOO.with(|cell| cell.set(Some(supports_ooo)));
                            Box::pin(async move {
                                let app = app.to_html_stream_in_order().collect::<String>().await;
                                Box::pin(once(async move { app }).chain(chunks()))
                                    as PinnedStream<String>
                            })
                        },
                    )
                    .await
                }),
            ))
            .await;

            let mut req = test::TestRequest::get().uri("/");
            if with_header {
                req = req.header("Islands-Router", "1");
            }
            let resp = test::call_service(&app, req.to_request()).await;
            assert_eq!(resp.status(), StatusCode::OK);
            SEEN_SUPPORTS_OOO
                .with(|cell| cell.get())
                .expect("stream_builder must have run and recorded supports_ooo")
        }

        let without_header = supports_ooo_seen_for(false).await;
        let with_header = supports_ooo_seen_for(true).await;
        assert!(
            without_header,
            "supports_ooo must be true without the header (no island-router navigation)"
        );
        // `is_island_router_navigation` is `cfg!(feature = "islands-router")
        // && <header present>`, so the header only has an effect when the
        // crate is actually built with the feature on — this test must hold
        // (and exercise the real branch) under both build configurations.
        if cfg!(feature = "islands-router") {
            assert!(
                !with_header,
                "with the islands-router feature on, the Islands-Router header must disable supports_ooo"
            );
        } else {
            assert_eq!(
                with_header, without_header,
                "with the islands-router feature off, the Islands-Router header must not change supports_ooo"
            );
        }
    }

    // `leptos_corrected_path` builds the `RequestUrl` leptos routes on. The
    // query branch (`?{query}`) is otherwise only reached through the full
    // render pipeline by query-less URIs, so pin both branches directly.
    #[ntex::test]
    async fn leptos_corrected_path_includes_the_query() {
        let req = test::TestRequest::default()
            .uri("/p?q=1&x=2")
            .to_http_request();
        assert_eq!(leptos_corrected_path(&req), "http://leptos/p?q=1&x=2");
    }

    #[ntex::test]
    async fn leptos_corrected_path_omits_an_absent_query() {
        let req = test::TestRequest::default().uri("/p").to_http_request();
        assert_eq!(leptos_corrected_path(&req), "http://leptos/p");
    }
}
