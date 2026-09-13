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
    let url = req
        .with(leptos_corrected_path)
        .expect("new request context belongs to the active request scope");
    provide_context(RequestUrl::new(&url));
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
    crate::request::check_registration_scope();
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
/// response body before sending. The `render_app_*` family also uses this
/// pipeline through its route handlers. The signature follows
/// `leptos_axum::handle_response_inner` for adapting existing code.
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
/// The managed [`Request`] context can be moved independently; that does not
/// make this future's captured native request transferable.
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
    Box::pin(crate::owner::OwnerContextFuture::new(SendWrapper::new(
        async move {
            let is_island_router_navigation =
                cfg!(feature = "islands-router") && req.headers().contains_key("Islands-Router");
            let res_options = ResponseOptions::default();
            let (meta_context, meta_output) = ServerMetaContext::new();

            let req_ctx = match crate::request::scoped_request(&req) {
                Ok(request) => request,
                Err(response) => return response,
            };
            let cx = {
                let meta_context = meta_context.clone();
                let res_options = res_options.clone();
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

            crate::stream::terminate_on_body_error(&req, res.take())
        },
    )))
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
    use crate::tests::run_ntex;
    use lets_expect::lets_expect;
    use ntex::http::{Method as HttpMethod, StatusCode};
    use ntex::web::{App, test};

    #[derive(Clone, Copy)]
    enum RequestedMethod {
        Matching,
        Head,
        Other,
    }

    fn unsupported_response(
        configured: Method,
        requested: RequestedMethod,
    ) -> (StatusCode, String) {
        run_ntex(async move {
            let requested = match requested {
                RequestedMethod::Matching => ntex_method(configured),
                RequestedMethod::Head => HttpMethod::HEAD,
                RequestedMethod::Other if configured == Method::Get => HttpMethod::POST,
                RequestedMethod::Other => HttpMethod::GET,
            };
            // Async is merely a diagnostic argument when invoking this private
            // fallback directly. Normal dispatch supports Async.
            let app = test::init_service(
                App::new().route("/", unsupported_ssr_mode_route(configured, &SsrMode::Async)),
            )
            .await;
            let response = test::call_service(
                &app,
                test::TestRequest::default()
                    .method(requested)
                    .uri("/")
                    .to_request(),
            )
            .await;
            let status = response.status();
            let body = String::from_utf8(test::read_body(response).await.to_vec()).unwrap();
            (status, body)
        })
    }
    lets_expect! {
        expect(unsupported_response(configured, requested)) as unsupported_mode_route {
            let configured = Method::Get;
            let requested = RequestedMethod::Matching;
            to returns_the_explicit_unsupported_result {
                equal((StatusCode::INTERNAL_SERVER_ERROR, "This rendering mode is not supported.".to_string()))
            }
            when requested_with_head {
                let requested = RequestedMethod::Head;
                to selects_the_get_handler { have(0) equal(StatusCode::INTERNAL_SERVER_ERROR) }
            }
            when requested_with_another_method {
                let requested = RequestedMethod::Other;
                to falls_through_to_the_app_default { have(0) equal(StatusCode::NOT_FOUND) }
            }
            when configured_for_post {
                let configured = Method::Post;
                to returns_the_explicit_unsupported_result {
                    equal((StatusCode::INTERNAL_SERVER_ERROR, "This rendering mode is not supported.".to_string()))
                }
                when requested_with_head {
                    let requested = RequestedMethod::Head;
                    to does_not_add_head_to_a_post_route { have(0) equal(StatusCode::NOT_FOUND) }
                }
                when requested_with_another_method {
                    let requested = RequestedMethod::Other;
                    to falls_through_to_the_app_default { have(0) equal(StatusCode::NOT_FOUND) }
                }
            }
        }
    }

    fn supports_out_of_order(with_header: bool) -> (StatusCode, Option<bool>) {
        use leptos::prelude::*;
        use std::sync::{Arc, Mutex};
        run_ntex(async move {
            let seen = Arc::new(Mutex::new(None));
            let context = seen.clone();
            let mut request = test::TestRequest::get().uri("/");
            if with_header {
                request = request.header("Islands-Router", "1");
            }
            let response = handle_response_inner(
                move || provide_context(context),
                || view! { <!DOCTYPE html><html><head></head><body><h1>"IslandsProbe"</h1></body></html> },
                request.to_http_request(),
                |app, chunks, supports_ooo| {
                    *expect_context::<Arc<Mutex<Option<bool>>>>().lock().unwrap() = Some(supports_ooo);
                    Box::pin(async move {
                        let app = app.to_html_stream_in_order().collect::<String>().await;
                        Box::pin(once(async move { app }).chain(chunks())) as PinnedStream<String>
                    })
                },
            ).await;
            let observed = *seen.lock().unwrap();
            (response.status(), observed)
        })
    }
    lets_expect! {
        expect(supports_out_of_order(with_header)) as islands_navigation_streaming {
            let with_header = false;
            to enables_out_of_order_for_document_navigation { equal((StatusCode::OK, Some(true))) }
            when the_islands_router_header_is_present {
                let with_header = true;
                to disables_out_of_order_only_with_the_feature_enabled {
                    equal((StatusCode::OK, Some(!cfg!(feature = "islands-router"))))
                }
            }
        }
        expect(leptos_corrected_path(&test::TestRequest::default().uri(uri).to_http_request())) as leptos_request_url {
            let uri = "/p";
            to preserves_the_path_without_a_query_separator { equal("http://leptos/p".to_string()) }
            when a_query_is_present {
                let uri = "/p?q=1&x=2";
                to includes_the_complete_query { equal("http://leptos/p?q=1&x=2".to_string()) }
            }
        }
    }
}
