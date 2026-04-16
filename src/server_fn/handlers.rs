//! Server-function request dispatch: the per-path handler factory and the
//! catch-all [`handle_server_fns`] / [`handle_server_fns_with_context`]
//! entry points.

use leptos::{
    context::provide_context,
    reactive::{computed::ScopedFuture, owner::Owner},
};
use leptos_integration_utils::ExtendResponse;
use ntex::http::{
    Payload, StatusCode,
    header,
};
use ntex::web::{self, ErrorRenderer, HttpRequest, HttpResponse, Route};
use or_poisoned::OrPoisoned;
use server_fn::{
    ServerFnTraitObj,
    middleware::{BoxedService, Layer},
};
use std::sync::Arc;

use crate::config::{
    PayloadTooLarge, content_length_exceeds, server_fn_config,
};
use crate::request::Request;
use crate::response::{NtexResponse, ResponseOptions};
use crate::routes::ensure_executor_initialized;
use crate::server_fn::registry::get_server_fn_service;
use crate::server_fn::request::NtexRequest;
use crate::server_fn::response::NtexServerResponse;

/// Runs a prepared [`BoxedService`] for a server function, setting up the
/// reactive Owner, provided contexts, referrer-based redirect fixup, and
/// `ResponseOptions` extension. Shared between the catchall
/// [`handle_server_fns_with_context`] handler and the per-path
/// [`handle_specific_server_fn_with_context`] handler.
pub(crate) async fn dispatch_server_fn(
    mut service: BoxedService<NtexRequest, NtexServerResponse>,
    req: HttpRequest,
    payload: Payload,
    additional_context: impl FnOnce() + 'static + Send,
) -> HttpResponse {
    let owner = Owner::new();
    owner
        .with(|| {
            ScopedFuture::new(async move {
                provide_context(Request::new(&req));
                let res_options = ResponseOptions::default();
                provide_context(res_options.clone());
                additional_context();

                let accepts_html = req
                    .headers()
                    .get(header::ACCEPT)
                    .and_then(|v| v.to_str().ok())
                    .map(|v| v.contains("text/html"))
                    .unwrap_or(false);
                let referrer = req.headers().get(header::REFERER).cloned();

                let mut res = service.run(NtexRequest::from((req, payload))).await;

                if accepts_html
                    && res.0.headers().get(header::LOCATION).is_none()
                    && let Some(referrer) = referrer
                {
                    *res.0.status_mut() = StatusCode::FOUND;
                    res.0.headers_mut().insert(header::LOCATION, referrer);
                }

                {
                    let mut res_options = res_options.0.write().or_poisoned();
                    let headers = res.0.headers_mut();
                    // `HeaderMap::get` returns only the first value; use
                    // `get_all` so multi-valued `Location` (rare but
                    // legal via `append_header`) isn't silently dropped.
                    let mut locations =
                        res_options.headers.get_all(header::LOCATION).cloned();
                    if let Some(first) = locations.next() {
                        headers.insert(header::LOCATION, first);
                        for v in locations {
                            headers.append(header::LOCATION, v);
                        }
                        res_options.headers.remove(header::LOCATION);
                    }
                }

                let mut wrapped = NtexResponse(res.take());
                wrapped.extend_response(&res_options);
                wrapped.take()
            })
        })
        .await
}

/// Returns an ntex [`Route`] bound to a single, pre-resolved server
/// function — skipping the per-request `HashMap` lookup that
/// [`handle_server_fns_with_context`] performs. Used by
/// [`LeptosRoutes::leptos_routes_with_context`](crate::LeptosRoutes::leptos_routes_with_context)
/// at registration time so every server-fn endpoint closes over its own
/// [`ServerFnTraitObj`] and middleware list.
///
/// The middleware factory is invoked once at registration and the result
/// is cached behind an [`Arc`] — cheap to clone per request (one atomic
/// increment), no per-request `Vec` allocation for the layer list.
pub(crate) fn handle_specific_server_fn_with_context<Err>(
    server_fn: ServerFnTraitObj<NtexRequest, NtexServerResponse>,
    additional_context: impl Fn() + 'static + Clone + Send,
) -> Route<Err>
where
    Err: ErrorRenderer,
{
    ensure_executor_initialized();
    let method = server_fn.method();
    let middleware: Arc<[Arc<dyn Layer<NtexRequest, NtexServerResponse>>]> =
        server_fn.middleware().into();
    let server_fn = Arc::new(server_fn);

    Route::<Err>::new().method(method).to(
        move |req: HttpRequest, payload: web::types::Payload| {
            let server_fn = server_fn.clone();
            let middleware = middleware.clone();
            let additional_context = additional_context.clone();
            async move {
                let limit = server_fn_config(&req).payload_limit;
                // Preflight: reject oversize bodies declared up-front
                // via Content-Length without reading any of the body.
                if content_length_exceeds(&req, limit) {
                    return oversize_response(limit);
                }
                let mut service = (*server_fn).clone().boxed();
                for m in middleware.iter() {
                    service = m.layer(service);
                }
                let resp = dispatch_server_fn(
                    service,
                    req.clone(),
                    payload.into_inner(),
                    additional_context,
                )
                .await;
                // Promote streaming/chunked overflow (detected by
                // `collect_payload` or the `try_into_stream` adapter
                // through the request-scoped marker) into a real 413.
                if req.extensions().get::<PayloadTooLarge>().is_some() {
                    return oversize_response(limit);
                }
                resp
            }
        },
    )
}

/// Builds a canonical `413 Payload Too Large` response with a human-
/// readable body that states the configured limit.
fn oversize_response(limit: usize) -> HttpResponse {
    HttpResponse::PayloadTooLarge()
        .content_type("text/plain; charset=utf-8")
        .body(format!("payload exceeds limit of {limit} bytes"))
}

/// Returns an ntex [`Route`] that dispatches requests to the registered
/// server functions.
///
/// This can be mounted once at a wildcard path matching the API prefix:
///
/// ```no_run
/// use ntex::web::{self, App as NtexApp};
/// use leptos_ntex_unofficial::handle_server_fns;
///
/// # fn main() -> std::io::Result<()> {
/// web::server(|| async {
///     NtexApp::new().route("/api/{tail:.*}", handle_server_fns())
/// })
/// .bind(("127.0.0.1", 3000))?
/// .run();
/// # Ok(())
/// # }
/// ```
///
/// ## Provided Context Types
/// - [`ResponseOptions`](crate::ResponseOptions)
/// - [`Request`](crate::Request)
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(level = "trace", fields(error), skip_all)
)]
pub fn handle_server_fns<Err>() -> Route<Err>
where
    Err: ErrorRenderer,
{
    handle_server_fns_with_context(|| {})
}

/// Variant of [`handle_server_fns`] that injects additional values into the
/// reactive context before dispatching the server function.
///
/// If your server functions expect some piece of context, make sure to
/// provide it both here and in
/// [`LeptosRoutes::leptos_routes_with_context`](crate::LeptosRoutes::leptos_routes_with_context)
/// (or whichever rendering method you use). During SSR, server functions
/// are called by the rendering method, while subsequent calls from the
/// client are handled by this server function handler — both paths need
/// to see the same context.
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(level = "trace", fields(error), skip_all)
)]
pub fn handle_server_fns_with_context<Err>(
    additional_context: impl Fn() + 'static + Clone + Send,
) -> Route<Err>
where
    Err: ErrorRenderer,
{
    ensure_executor_initialized();
    Route::<Err>::new().to(move |req: HttpRequest, payload: web::types::Payload| {
        let additional_context = additional_context.clone();
        async move {
            let limit = server_fn_config(&req).payload_limit;
            if content_length_exceeds(&req, limit) {
                return oversize_response(limit);
            }
            if let Some(service) = get_server_fn_service(req.path(), req.method()) {
                let resp = dispatch_server_fn(
                    service,
                    req.clone(),
                    payload.into_inner(),
                    additional_context,
                )
                .await;
                if req.extensions().get::<PayloadTooLarge>().is_some() {
                    return oversize_response(limit);
                }
                resp
            } else {
                HttpResponse::BadRequest().body(format!(
                    "Could not find a server function at the route {}. \
\n\nIt's likely that either\n1. The API prefix you specify in the `#[server]` macro doesn't match the prefix at which your server function handler is mounted, or\n2. You are on a platform that doesn't support automatic server function registration and you need to call register_explicit() on the server function type, somewhere in your `main` function.",
                    req.path()
                ))
            }
        }
    })
}

