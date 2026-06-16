//! Server-function request dispatch: the per-path handler factory and the
//! catch-all [`handle_server_fns`] / [`handle_server_fns_with_context`]
//! entry points.

use leptos::{
    context::provide_context,
    reactive::{computed::ScopedFuture, owner::Owner},
};
use leptos_integration_utils::ExtendResponse;
use ntex::http::{
    Payload, StatusCode, Uri,
    header::{self, HeaderValue},
};
use ntex::web::{self, ErrorRenderer, HttpRequest, HttpResponse, Route};
use server_fn::{
    ServerFnTraitObj,
    error::SERVER_FN_ERROR_HEADER,
    middleware::{BoxedService, Layer},
};
use std::sync::Arc;

use crate::config::{PayloadTooLarge, content_length_exceeds, server_fn_config};
use crate::request::Request;
use crate::response::{NtexResponse, ResponseOptions, accept_header_includes_html};
use crate::routes::ensure_executor_initialized;
use crate::server_fn::registry::{get_server_fn_service, server_fn_methods};
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
                    .map(accept_header_includes_html)
                    .unwrap_or(false);
                let raw_referrer = req.headers().get(header::REFERER).cloned();
                // The request is consumed by `service.run` below, so the
                // origin used for the post-run Location checks is captured
                // up front.
                let (conn_scheme, conn_host) = {
                    let connection = req.connection_info();
                    (
                        connection.scheme().to_string(),
                        connection.host().to_string(),
                    )
                };
                let referrer = raw_referrer
                    .as_ref()
                    .and_then(|value| same_origin_location(&conn_scheme, &conn_host, value));

                let mut res = service.run(NtexRequest::from((req, payload))).await;

                // Whether the post-run `Location` is an echo of the request's
                // Referer — the signature of server_fn's form-redirect fallback
                // for an HTML form. It may append a bare `?`, which
                // `location_matches_referrer` tolerates.
                let location_is_referrer = raw_referrer.as_ref().is_some_and(|raw| {
                    res.0
                        .headers()
                        .get(header::LOCATION)
                        .is_some_and(|location| location_matches_referrer(location, raw))
                });

                if accepts_html {
                    if location_is_referrer {
                        if let Some(referrer) = referrer.clone() {
                            *res.0.status_mut() = StatusCode::FOUND;
                            res.0.headers_mut().insert(header::LOCATION, referrer);
                        } else {
                            res.0.headers_mut().remove(header::LOCATION);
                            reset_status_after_redirect_strip(&mut res);
                        }
                    } else if res.0.status().is_success()
                        && res.0.headers().get(header::LOCATION).is_none()
                        && let Some(referrer) = referrer
                    {
                        *res.0.status_mut() = StatusCode::FOUND;
                        res.0.headers_mut().insert(header::LOCATION, referrer);
                    }
                }
                // A non-HTML client (including the `text/html;q=0` case) never
                // reaches a form redirect to strip: `NtexRequest::accepts`
                // hides a strict-refused `text/html` token from server_fn's
                // fallback at the source, so the fallback simply does not fire.
                // Any 3xx still present here is therefore a user middleware's own
                // short-circuit response, which must pass through untouched
                // (the same-origin guard below still applies to its `Location`).

                // Same-origin invariant for the server_fn-layer `Location`.
                // The block above only repairs the EXACT referer echo, and
                // it is gated on the strict `Accept` parser — but server_fn's
                // own form-redirect fallback gates on a loose
                // `contains("text/html")` check and rewrites the referer on
                // the error path (appending the error query), so a
                // referer-derived `Location` can reach this point in shapes
                // the block above never sees (`Accept: text/html;q=0`, error
                // URLs). Rather than chase every shape, enforce the
                // documented policy wholesale: at this layer a redirect may
                // only target the current origin. Application-level
                // redirects via `redirect()`/`ResponseOptions` are applied
                // in `extend_response` AFTER this check and stay untouched.
                if let Some(location) = res.0.headers().get(header::LOCATION).cloned()
                    && same_origin_location(&conn_scheme, &conn_host, &location).is_none()
                {
                    res.0.headers_mut().remove(header::LOCATION);
                    reset_status_after_redirect_strip(&mut res);
                }

                let mut wrapped = NtexResponse(res.take());
                wrapped.extend_response(&res_options);
                wrapped.take()
            })
        })
        .await
}

/// Resets the status of a response whose `Location` was just stripped — either
/// because it failed the same-origin invariant or because it was a referer echo
/// refused by the strict `Accept` parser.
///
/// A server-function **error** response carries [`SERVER_FN_ERROR_HEADER`]: it
/// started life as a `500` from `NtexServerResponse::error_response` that
/// server_fn's form-redirect fallback then overwrote with `302`. Dropping that
/// redirect must not silently promote the failure to `200 OK`, so an error
/// response is restored to `500` while an ordinary, successful form redirect
/// becomes `200`.
fn reset_status_after_redirect_strip(res: &mut NtexServerResponse) {
    if res.0.status().is_redirection() {
        let was_error = res.0.headers().get(SERVER_FN_ERROR_HEADER).is_some();
        *res.0.status_mut() = if was_error {
            StatusCode::INTERNAL_SERVER_ERROR
        } else {
            StatusCode::OK
        };
    }
}

fn location_matches_referrer(location: &HeaderValue, referrer: &HeaderValue) -> bool {
    let Some(location) = location.to_str().ok() else {
        return false;
    };
    let Some(referrer) = referrer.to_str().ok() else {
        return false;
    };
    location == referrer || location.strip_suffix('?') == Some(referrer)
}

/// Accepts a `Location`/`Referer` value only when it stays on the current
/// origin: either an origin-relative path (`/...`, but not the
/// protocol-relative `//...`) or an absolute URI whose scheme and authority
/// match the connection. Returns the same-origin path-and-query form, or
/// `None` for everything else (cross-origin, protocol-relative,
/// `javascript:`-style schemes, unparseable values).
fn same_origin_location(
    conn_scheme: &str,
    conn_host: &str,
    value: &HeaderValue,
) -> Option<HeaderValue> {
    let value = value.to_str().ok()?;
    if value.starts_with('/') && !value.starts_with("//") {
        return HeaderValue::from_str(value).ok();
    }

    let uri = value.parse::<Uri>().ok()?;
    let scheme = uri.scheme_str()?;
    let authority = uri.authority()?.as_str();
    if !scheme.eq_ignore_ascii_case(conn_scheme) || !authority.eq_ignore_ascii_case(conn_host) {
        return None;
    }

    let path_and_query = uri.path_and_query().map_or("/", |pq| pq.as_str());
    HeaderValue::from_str(path_and_query).ok()
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

    Route::<Err>::new()
        .method(method)
        .to(move |req: HttpRequest, payload: web::types::Payload| {
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
        })
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
///     NtexApp::new().route("/api/{tail}*", handle_server_fns())
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
                let allowed = server_fn_methods(req.path());
                if allowed.is_empty() {
                    HttpResponse::BadRequest().body(format!(
                        "Could not find a server function at the route {}. \
\n\nIt's likely that either\n1. The API prefix you specify in the `#[server]` macro doesn't match the prefix at which your server function handler is mounted, or\n2. You are on a platform that doesn't support automatic server function registration and you need to call register_explicit() on the server function type, somewhere in your `main` function.",
                        req.path()
                    ))
                } else {
                    let allow = allowed
                        .iter()
                        .map(ntex::http::Method::as_str)
                        .collect::<Vec<_>>()
                        .join(", ");
                    HttpResponse::MethodNotAllowed()
                        .header(header::ALLOW, allow)
                        .body(format!(
                            "Server function at route {} does not accept method {}.",
                            req.path(),
                            req.method()
                        ))
                }
            }
        }
    })
}
